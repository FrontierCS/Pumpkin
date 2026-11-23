//! The output of a link's `ssh`, which has a standard error of its own: this process passes on
//! what `ssh` writes there to its own standard error.

use std::io::{self, Read};
use std::process::{ChildStderr, ChildStdout};

use rustix::event::{self, PollFd, PollFlags, Timespec};
use rustix::io::Errno;

// `PIPE_BUF` on Linux, the most that a pipe takes whole in one write.
// https://man7.org/linux/man-pages/man7/pipe.7.html
const CHUNK_BYTES: usize = 4096;

// What a pipe holds on Linux unless its size is set.
// https://man7.org/linux/man-pages/man7/pipe.7.html
const PIPE_BYTES: usize = 65536;

/// The standard output of `ssh`. Each read passes on what has arrived on `ssh`'s standard error,
/// before and after it reads the output.
pub(super) struct Relaying {
    stdout: ChildStdout,
    /// `None` once `ssh` has closed it, or once reading it has failed.
    stderr: Option<ChildStderr>,
}

impl Relaying {
    pub(super) fn new(stdout: ChildStdout, stderr: ChildStderr) -> Self {
        Self {
            stdout,
            stderr: Some(stderr),
        }
    }

    /// Passes on what has arrived on `ssh`'s standard error, without waiting for more.
    pub(super) fn passed_on(&mut self) {
        // At most what the pipe holds, so that a writer that keeps up can't hold this here.
        for _ in 0..PIPE_BYTES / CHUNK_BYTES {
            let Some(stderr) = &self.stderr else {
                return;
            };
            let mut polled = [PollFd::new(stderr, PollFlags::IN)];
            match event::poll(&mut polled, Some(&Timespec::default())) {
                Ok(0) => return,
                Ok(_) => self.relayed(),
                Err(Errno::INTR) => {}
                Err(error) => return self.unread(&error.into()),
            }
        }
    }

    /// Passes on one chunk of what has arrived on `ssh`'s standard error, which is ready to read.
    fn relayed(&mut self) {
        let Some(stderr) = &mut self.stderr else {
            return;
        };
        let mut chunk = [0; CHUNK_BYTES];
        match stderr.read(&mut chunk) {
            Ok(0) => self.stderr = None,
            // As `say` does, it ignores a failure to write, and reads on, so that `ssh` never
            // waits on a full pipe.
            Ok(count) => crate::to_stderr(&chunk[..count]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => self.unread(&error),
        }
    }

    /// Stops reading `ssh`'s standard error, which failed with `error`, and says so.
    fn unread(&mut self, error: &io::Error) {
        self.stderr = None;
        crate::say(format_args!(
            "reading the standard error of `ssh` failed: {error}"
        ));
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
                Err(error) => {
                    self.unread(&error.into());
                    break;
                }
            }
            let [output, error] = polled.map(|polled| !polled.revents().is_empty());
            if error {
                self.passed_on();
            }
            if output {
                break;
            }
        }
        let read = self.stdout.read(buf);
        // `sshd` sends what the remote command wrote to its standard error after what it wrote to
        // its output at the same time, so a line said just before an answer can arrive after it.
        // https://github.com/openssh/openssh-portable/blob/5dba72adacbd39736960543373737bc01e35f778/channels.c#L3269-L3275
        self.passed_on();
        read
    }
}
