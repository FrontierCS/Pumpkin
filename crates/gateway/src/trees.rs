//! A put's frames, which a session sends one after another at the sandbox's pace: the tree's
//! entries, each file's bytes after its entry, and the tree's end.

use std::collections::VecDeque;
use std::fs;
use std::io::{self, Read as _};
use std::sync::mpsc::SyncSender;

use schema::frame;
use schema::sandbox::{Entry, RelativePath, ToSandbox};

use crate::remote::ToThread;
use crate::{Put, context};

// How many of a put's frames wait to be sent at most: one in flight, one behind it, so that a put
// streams a file at the sandbox's pace and holds two frames of it.
const PUT_FRAMES: usize = 2;

pub(crate) struct PutState {
    entries: VecDeque<Put>,
    // Where the put asks for its next entries while the thread that hands them holds more, and
    // whether it has asked.
    more: Option<(SyncSender<ToThread>, bool)>,
    // The file whose bytes go next, with how many are left.
    file: Option<(RelativePath, Source, u64)>,
    ended: bool,
    // Why a host file read short or failed, after which the put ends at once.
    failed: Option<io::Error>,
    // How many of its frames wait in the session's queue.
    queued: usize,
}

enum Source {
    // With how many of its bytes have gone.
    Bytes(Vec<u8>, usize),
    Host(io::Take<fs::File>),
}

/// Where a put is when its reply comes.
pub(crate) enum Step {
    /// Its end is still to send: only a failure may come now.
    Sending,
    /// Its end was sent, after a host file's failure, if it had one, which is the put's.
    Ended(Option<io::Error>),
}

impl PutState {
    /// A put of `entries`, then of those that `more` hands it, if given, whose request is queued
    /// as the first of its frames.
    pub(crate) fn new(entries: Vec<Put>, more: Option<SyncSender<ToThread>>) -> Self {
        Self {
            entries: entries.into(),
            more: more.map(|more| (more, false)),
            file: None,
            ended: false,
            failed: None,
            queued: 1,
        }
    }

    pub(crate) fn replied(&mut self) -> Step {
        if self.ended {
            Step::Ended(self.failed.take())
        } else {
            Step::Sending
        }
    }

    pub(crate) fn stop(&mut self) {
        self.entries.clear();
        self.more = None;
        self.file = None;
        self.ended = true;
    }

    /// Takes the next entries that it asked for, after which more follow if `more`.
    pub(crate) fn hand(&mut self, entries: Vec<Put>, more: bool) {
        let Some((_, asked)) = self.more.as_mut().filter(|(_, asked)| *asked) else {
            unreachable!(
                "a put was handed entries that it hadn't asked for, though its caller hands them \
                 only as asked"
            )
        };
        *asked = false;
        if !more {
            self.more = None;
        }
        self.entries.extend(entries);
    }

    /// Takes the send of one of its frames.
    pub(crate) fn sent(&mut self) {
        assert!(
            self.queued > 0,
            "a put's frame was sent that it never queued"
        );
        self.queued -= 1;
    }

    /// The put's next frame on `stream`, to queue: none while it holds [`PUT_FRAMES`] queued, or
    /// waits for the entries that it asked for, or once its end has gone.
    pub(crate) fn next(&mut self, stream: u32) -> Option<(ToSandbox, Vec<u8>)> {
        if self.ended || self.queued >= PUT_FRAMES {
            return None;
        }
        let frame = self.frame(stream)?;
        self.queued += 1;
        Some(frame)
    }

    fn frame(&mut self, stream: u32) -> Option<(ToSandbox, Vec<u8>)> {
        if let Some((path, source, left)) = &mut self.file {
            let chunk = frame::MAX_BODY_BYTES.min(usize::try_from(*left).unwrap_or(usize::MAX));
            let read = match source {
                Source::Bytes(bytes, sent) => {
                    let chunk = bytes[*sent..*sent + chunk].to_vec();
                    *sent += chunk.len();
                    Ok(chunk)
                }
                Source::Host(file) => read_chunk(file, chunk, *left),
            };
            match read {
                Ok(bytes) => {
                    *left -= bytes.len() as u64;
                    if *left == 0 {
                        self.file = None;
                    }
                    return Some((ToSandbox::Data { stream }, bytes));
                }
                // The tree ends with the file short, which `sandboxd` fails; the file's error is
                // the one to report.
                Err(error) => {
                    self.failed = Some(context(&*path, &error));
                    self.stop();
                    return Some((ToSandbox::End { stream }, Vec::new()));
                }
            }
        }
        let Some(put) = self.entries.pop_front() else {
            if let Some((thread, asked)) = &mut self.more {
                if !*asked {
                    *asked = true;
                    if let Err(error) = thread.try_send(ToThread::More(stream)) {
                        unreachable!(
                            "the put on stream {stream} can't ask for its next entries: {error}, \
                             though its caller waits until the put's answer, and holds room for \
                             one ask besides it"
                        )
                    }
                }
                return None;
            }
            self.ended = true;
            return Some((ToSandbox::End { stream }, Vec::new()));
        };
        let entry = match put {
            Put::Dir(path, mode) => Entry::Dir { path, mode },
            Put::File(path, mode, bytes) => {
                let size = bytes.len() as u64;
                if size > 0 {
                    self.file = Some((path.clone(), Source::Bytes(bytes, 0), size));
                }
                Entry::File { path, size, mode }
            }
            Put::Read(path, mode, file) => {
                let size = match file.metadata() {
                    Ok(metadata) => metadata.len(),
                    Err(error) => {
                        self.failed = Some(context(&path, &error));
                        self.stop();
                        return Some((ToSandbox::End { stream }, Vec::new()));
                    }
                };
                if size > 0 {
                    self.file = Some((path.clone(), Source::Host(file.take(size)), size));
                }
                Entry::File { path, size, mode }
            }
        };
        Some((ToSandbox::Entry { stream, entry }, Vec::new()))
    }
}

// Reads the next `chunk` bytes of a host file of which `left` are still to come.
// A put reads its host files as it sends them, by synchronous reads: an operation of the loop's
// waits for a need that a measurement shows.
fn read_chunk(file: &mut io::Take<fs::File>, chunk: usize, left: u64) -> io::Result<Vec<u8>> {
    let mut buffer = vec![0; chunk];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => return Err(io::Error::other(format!("{left} of its bytes are missing"))),
            Ok(read) => {
                buffer.truncate(read);
                return Ok(buffer);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}
