//! A store query and resumable uploads. Small whole blobs may share one request, as in Bazel's
//! `BatchUpdateBlobs`
//! (<https://github.com/bazelbuild/remote-apis/blob/c2c07e3d718d3d490a4958344d9c3e47de0bfaf3/build/bazel/remote/execution/v2/remote_execution.proto#L365-L388>).
//! A group gets a `Stored` per blob in order, up to its first failure, which ends its answers.

use std::io::{self, Cursor, Read, Write};
use std::process::ChildStdin;

use rustix::event::{self, PollFd, PollFlags, Timespec};
use rustix::io::Errno;
use schema::frame::{self, MAX_BODY_BYTES};
use schema::ir::Sha256Digest;
use serde::{Deserialize, Serialize};

use super::{Ask, Head, Link, Part, Request, Unanswered};
use crate::manifest;
use crate::manifest::store::{Missing, PutAnswer};

#[derive(Serialize, Deserialize)]
pub(crate) struct Lacks {
    pub(crate) manifest: Sha256Digest,
    pub(crate) body_bytes: usize,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum LacksHead {
    Missing { body_bytes: u64 },
}

impl Request for Lacks {
    type Answer = Vec<Missing>;
    type Head = LacksHead;

    fn ask(self) -> Ask {
        Ask::Lacks(self)
    }

    fn sent(missing: Vec<Missing>) -> io::Result<(LacksHead, Vec<u8>)> {
        let body = serde_json::to_vec(&missing)?;
        Ok((
            LacksHead::Missing {
                body_bytes: body.len() as u64,
            },
            body,
        ))
    }

    fn received(head: LacksHead, link: &mut Link) -> Result<Vec<Missing>, Unanswered> {
        let LacksHead::Missing { body_bytes } = head;
        let blobs: Vec<Missing> =
            serde_json::from_slice(&link.body(body_bytes)?).map_err(|error| {
                Unanswered::Failed(format!(
                    "the listing of missing blobs doesn't read: {error}"
                ))
            })?;
        if blobs
            .iter()
            .any(|blob| blob.held_bytes > blob.size_bytes || blob.size_bytes > manifest::BLOB_BYTES)
        {
            return Err(Unanswered::Failed(
                "the controller reported an invalid blob size or offset".to_owned(),
            ));
        }
        Ok(blobs)
    }
}

const GROUP_BLOBS: usize = 8;
// A small write in OpenSSH's SFTP client:
// https://github.com/openssh/openssh-portable/blob/5dba72adacbd39736960543373737bc01e35f778/sftp-client.c#L60
const SMALL_BYTES: u64 = 32 * 1024;

fn small(from: u64, size_bytes: u64) -> bool {
    from == 0 && size_bytes <= SMALL_BYTES
}

pub(crate) fn groups(mut missing: &[Missing]) -> impl Iterator<Item = &[Missing]> {
    std::iter::from_fn(move || {
        missing.first()?;
        let count = missing
            .iter()
            .take(GROUP_BLOBS)
            .take_while(|blob| small(blob.held_bytes, blob.size_bytes))
            .count()
            .max(1);
        let (group, rest) = missing.split_at(count);
        missing = rest;
        Some(group)
    })
}

pub(crate) fn check_group(blobs: &[Put]) -> io::Result<()> {
    if !(2..=GROUP_BLOBS).contains(&blobs.len())
        || !blobs.iter().all(|blob| small(blob.from, blob.size_bytes))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "a group holds 2 to {GROUP_BLOBS} whole blobs of at most {SMALL_BYTES} bytes each"
            ),
        ));
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Put {
    pub(crate) digest: Sha256Digest,
    pub(crate) from: u64,
    pub(crate) size_bytes: u64,
}

impl Put {
    pub(crate) fn body_bytes(&self) -> io::Result<u64> {
        self.size_bytes
            .checked_sub(self.from)
            .filter(|_| self.size_bytes <= manifest::BLOB_BYTES)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "the upload's size or offset is outside the blob",
                )
            })
    }
}

/// The bytes of exactly one request, stopped at the next frame when its body is whole.
pub(crate) struct Parts<'a, R> {
    input: &'a mut R,
    part: Cursor<Vec<u8>>,
    left: u64,
    broken: Option<io::ErrorKind>,
}

impl<'a, R: Read> Parts<'a, R> {
    pub(crate) fn new(input: &'a mut R, bytes: u64) -> Self {
        Self {
            input,
            part: Cursor::new(Vec::new()),
            left: bytes,
            broken: None,
        }
    }

    /// Consumes a body left by a store error or a racing upload that finished first.
    pub(crate) fn finish(mut self) -> io::Result<()> {
        if let Some(kind) = self.broken {
            return Err(kind.into());
        }
        io::copy(&mut self, &mut io::sink()).map(drop)
    }

    fn next_part(&mut self) -> io::Result<Vec<u8>> {
        let Some((Part, part)) = frame::read(self.input)? else {
            return Err(io::ErrorKind::UnexpectedEof.into());
        };
        if part.is_empty() || part.len() as u64 > self.left {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "a part of the request's body holds {} bytes: expected 1 to {}",
                    part.len(),
                    self.left
                ),
            ));
        }
        Ok(part)
    }
}

impl<R: Read> Read for Parts<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() || self.left == 0 {
            return Ok(0);
        }
        if self.part.position() == self.part.get_ref().len() as u64 {
            let part = self
                .next_part()
                .inspect_err(|error| self.broken = Some(error.kind()))?;
            self.part = Cursor::new(part);
        }
        let read = self.part.read(buffer)?;
        self.left -= read as u64;
        Ok(read)
    }
}

impl Link {
    /// Sends the group. Read one `put_answer` per input until its first failure.
    pub(crate) fn send_puts(
        &mut self,
        inputs: Vec<(&Missing, Box<dyn Read + '_>)>,
        mut progress: impl FnMut(u64),
    ) -> Result<(), Unanswered> {
        let put = |blob: &Missing| Put {
            digest: blob.digest.clone(),
            from: blob.held_bytes,
            size_bytes: blob.size_bytes,
        };
        let ask = match inputs.as_slice() {
            [] => return Ok(()),
            [(one, _)] => Ask::Put(put(one)),
            group => Ask::Puts {
                blobs: group.iter().map(|(blob, _)| put(blob)).collect(),
            },
        };
        let mut pipe = Pipe(&mut self.stdin);
        let failed = |error: io::Error| match error.kind() {
            io::ErrorKind::BrokenPipe => {
                Unanswered::Unreached("ended during the upload".to_owned())
            }
            _ => Unanswered::Failed(format!("writing to `ssh` failed: {error}")),
        };
        frame::write(&mut pipe, &ask, &[]).map_err(failed)?;
        for (blob, mut source) in inputs {
            let mut left = blob.size_bytes - blob.held_bytes;
            let capacity = usize::try_from(left)
                .unwrap_or(usize::MAX)
                .min(MAX_BODY_BYTES);
            let mut buffer = vec![0; capacity];
            while left > 0 {
                let size =
                    usize::try_from(left.min(MAX_BODY_BYTES as u64)).unwrap_or(MAX_BODY_BYTES);
                let part = &mut buffer[..size];
                source.read_exact(part).map_err(|error| Unanswered::Failed(format!(
                    "the file changed or could not be read while it was sent: {error}: send the task again"
                )))?;
                frame::write(&mut pipe, &Part, part).map_err(failed)?;
                left -= size as u64;
                progress(size as u64);
            }
        }
        Ok(())
    }

    pub(crate) fn put_answer(&mut self) -> Result<Result<PutAnswer, String>, Unanswered> {
        match self.received("the upload's answer")? {
            Head::Answered(answer) => Ok(Ok(answer)),
            Head::Refused { message } => Ok(Err(message)),
        }
    }
}

// A send has no deadline on its whole time. It stops once the connection takes no more bytes.
const STALL_TIMEOUT: Timespec = Timespec {
    tv_sec: 2 * 60,
    tv_nsec: 0,
};

struct Pipe<'a>(&'a mut ChildStdin);

impl Write for Pipe<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        loop {
            let mut polled = [PollFd::new(&*self.0, PollFlags::OUT)];
            match event::poll(&mut polled, Some(&STALL_TIMEOUT)) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("it took no byte for {} s", STALL_TIMEOUT.tv_sec),
                    ));
                }
                Ok(_) => break,
                Err(Errno::INTR) => {}
                Err(error) => return Err(error.into()),
            }
        }
        // A writable pipe takes at least PIPE_BUF without waiting.
        self.0.write(&bytes[..bytes.len().min(libc::PIPE_BUF)])
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}
