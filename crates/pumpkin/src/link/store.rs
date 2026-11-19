//! A store query and one blob's resumable upload. Only a frame at a time crosses in memory.

use std::io::{self, Cursor, Read, Write};
use std::process::ChildStdin;
use std::time::Instant;

use rustix::event::{self, PollFd, PollFlags, Timespec};
use rustix::io::Errno;
use schema::frame::{self, MAX_BODY_BYTES};
use schema::ir::Sha256Digest;
use serde::{Deserialize, Serialize};
use stdx::Context as _;

use super::{Ask, Head, Link, Part, Request, Unanswered};
use crate::manifest;
use crate::manifest::store::{Missing, PutAnswer};

#[derive(Serialize, Deserialize)]
pub(crate) struct Lacks {
    pub(crate) manifest: Sha256Digest,
    pub(crate) body_bytes: usize,
    pub(crate) install: bool,
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
    pub(crate) fn put(
        &mut self,
        request: Put,
        source: impl Read,
        progress: impl FnMut(u64),
    ) -> Result<Result<PutAnswer, String>, Unanswered> {
        let nonblocking = |enabled| {
            rustix::io::retry_on_intr(|| rustix::io::ioctl_fionbio(&self.stdin, enabled))
                .context("setting the blocking mode of the pipe to `ssh` failed")
                .map_err(Unanswered::Failed)
        };
        nonblocking(true)?;
        let sent = Pipe(&self.stdin).send(request, source, progress);
        // The next request writes to the same pipe in its ordinary blocking mode.
        let restored = nonblocking(false);
        sent?;
        restored?;
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

struct Pipe<'a>(&'a ChildStdin);

impl Pipe<'_> {
    fn send(
        &mut self,
        request: Put,
        mut source: impl Read,
        mut progress: impl FnMut(u64),
    ) -> Result<(), Unanswered> {
        let mut left = request
            .body_bytes()
            .map_err(|error| Unanswered::Failed(error.to_string()))?;
        let failed = |error: io::Error| match error.kind() {
            io::ErrorKind::BrokenPipe => {
                Unanswered::Unreached("ended during the upload".to_owned())
            }
            _ => Unanswered::Failed(format!("writing to `ssh` failed: {error}")),
        };
        frame::write(self, &Ask::Put(request), &[]).map_err(failed)?;
        let capacity = usize::try_from(left)
            .unwrap_or(usize::MAX)
            .min(MAX_BODY_BYTES);
        let mut buffer = vec![0; capacity];
        while left > 0 {
            let size = usize::try_from(left.min(MAX_BODY_BYTES as u64)).unwrap_or(MAX_BODY_BYTES);
            let part = &mut buffer[..size];
            source.read_exact(part).map_err(|error| Unanswered::Failed(format!(
                "the file changed or could not be read while it was sent: {error}: send the task again"
            )))?;
            frame::write(self, &Part, part).map_err(failed)?;
            left -= size as u64;
            progress(size as u64);
        }
        Ok(())
    }
}

impl Write for Pipe<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        // A nonblocking write larger than PIPE_BUF can fill the available space in one call:
        // https://man7.org/linux/man-pages/man7/pipe.7.html
        let started = Instant::now();
        loop {
            match self.0.write(bytes) {
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                written => return written,
            }
            let elapsed = Timespec::try_from(started.elapsed()).unwrap_or(STALL_TIMEOUT);
            if elapsed >= STALL_TIMEOUT {
                break;
            }
            let timeout = STALL_TIMEOUT - elapsed;
            let mut polled = [PollFd::new(self.0, PollFlags::OUT)];
            match event::poll(&mut polled, Some(&timeout)) {
                Ok(0) => break,
                Ok(_) | Err(Errno::INTR) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("it took no byte for {} s", STALL_TIMEOUT.tv_sec),
        ))
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}
