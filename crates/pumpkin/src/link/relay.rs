//! The output of a link's `ssh`, which has a standard error of its own: this process passes on
//! what `ssh` writes there to its own standard error.

use std::io::{self, Read};
use std::process::{ChildStderr, ChildStdout};

use rustix::event::{self, PollFd, PollFlags, Timespec};
use rustix::io::Errno;

// `PIPE_BUF` on Linux: a write of at most this much to a pipe arrives whole, so that a chunk
// passed on doesn't interleave with what another process writes to the same pipe.
// https://man7.org/linux/man-pages/man7/pipe.7.html
const CHUNK_BYTES: usize = 4096;

// What a pipe holds on Linux unless its size is set.
// https://man7.org/linux/man-pages/man7/pipe.7.html
const PIPE_BYTES: usize = 65536;

/// The standard output of `ssh`, each read of which first passes on what has arrived on `ssh`'s
/// standard error.
pub(super) struct Relaying {
    stdout: ChildStdout,
    /// `None` once `ssh` has closed it.
    stderr: Option<ChildStderr>,
}

impl Relaying {
    pub(super) fn new(stdout: ChildStdout, stderr: ChildStderr) -> Self {
        Self {
            stdout,
            stderr: Some(stderr),
        }
    }

    /// Passes on what `ssh` wrote to its standard error and this process hasn't, once `ssh` has
    /// ended.
    pub(super) fn rest(&mut self) {
        // It takes only what is in the pipe, as a process that `ssh` started may hold it open.
        for _ in 0..PIPE_BYTES / CHUNK_BYTES {
            let Some(stderr) = &self.stderr else {
                return;
            };
            let mut polled = [PollFd::new(stderr, PollFlags::IN)];
            let ready = event::poll(&mut polled, Some(&Timespec::default()));
            if !matches!(ready, Ok(1..)) || self.relayed().is_err() {
                return;
            }
        }
    }

    /// Passes on one chunk of what `ssh` wrote to its standard error, waiting for it if none has
    /// arrived.
    fn relayed(&mut self) -> io::Result<()> {
        let Some(stderr) = &mut self.stderr else {
            return Ok(());
        };
        let mut chunk = [0; CHUNK_BYTES];
        match stderr.read(&mut chunk) {
            Ok(0) => self.stderr = None,
            // As `say` does, it ignores a failure to write, and reads on, so that `ssh` never
            // waits on a full pipe.
            Ok(count) => crate::to_stderr(&chunk[..count]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                self.stderr = None;
                let error = format!("reading its standard error failed: {error}");
                return Err(io::Error::other(error));
            }
        }
        Ok(())
    }
}

impl Read for Relaying {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // On this thread, while it waits for the output, as Mercurial's `doublepipe` passes on
        // what its `ssh` writes to its standard error.
        // https://foss.heptapod.net/mercurial/mercurial-devel/-/blob/53221078e0de65d1a821ce5311dec45a7a978301/mercurial/sshpeer.py#L52-L149
        while let Some(stderr) = &self.stderr {
            let mut polled = [
                PollFd::new(&self.stdout, PollFlags::IN),
                PollFd::new(stderr, PollFlags::IN),
            ];
            match event::poll(&mut polled, None) {
                Ok(_) => {}
                Err(Errno::INTR) => continue,
                Err(error) => return Err(error.into()),
            }
            let [output, error] = polled.map(|polled| !polled.revents().is_empty());
            if error {
                self.relayed()?;
            }
            if output {
                break;
            }
        }
        self.stdout.read(buf)
    }
}
