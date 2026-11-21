//! Following a run's child to its end on this thread alone: feeding its input and reading its
//! outputs as each pipe is ready, until each has closed and the child has exited, or the bound has
//! passed; then killing what is left of the run, and reaping the child.
//!
//! No pipe is left to a thread of its own, so a pipe that a process the command started holds open
//! keeps nothing waiting past the bound.

// Each pipe is read as it is ready, as TigerBeetle's `exec_inner` polls both outputs within its
// timeout and caps each:
// https://github.com/tigerbeetle/tigerbeetle/blob/3cd7cf4799f65c81ffbc36eda788b8b26fd7da72/src/shell.zig#L516-L531

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::process::{Child, Output};
use std::time::Duration;

use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::io::Errno;
use rustix::process::{Pid, WaitId, WaitIdOptions};

use super::children::kill;
use super::{FIRST_PAUSE, MAX_PAUSE};
use crate::Deadline;

// The most of each stream that a run captures, as TigerBeetle's `exec` caps each:
// https://github.com/tigerbeetle/tigerbeetle/blob/3cd7cf4799f65c81ffbc36eda788b8b26fd7da72/src/shell.zig#L468
const MAX_CAPTURED_BYTES: usize = 128 * 1024 * 1024;

// Each wait is given to `poll` in nanoseconds alone.
const _: () = assert!(MAX_PAUSE.as_secs() == 0);

/// Whose process group a run's child is in, which says what its end kills.
#[derive(Clone, Copy)]
pub(super) enum Group {
    /// One that the child leads, killed whole.
    Own,
    /// This process's, in which the child alone is killed, and only at the bound.
    Callers,
}

/// Follows `child`, in `group`, to its end, feeding it `input`, and gives how it exited and what
/// it wrote to each output that is piped: `None` if it hadn't ended by `by`. What is left of the
/// run is killed then, or as `group` says once the child has exited, and the child is reaped.
pub(super) fn watched(
    child: &mut Child,
    input: Option<&[u8]>,
    by: Deadline,
    group: Group,
) -> io::Result<Option<Output>> {
    let followed = follow(child, input, by);
    let status = match group {
        // Its group is killed while `child`, unreaped, leads it, so its ID names no other.
        Group::Own => kill(child)?,
        Group::Callers => {
            if !matches!(followed, Ok(Some(_))) {
                child.kill()?;
            }
            child.wait()?
        }
    };
    Ok(followed?.map(|(stdout, stderr)| Output {
        status,
        stdout,
        stderr,
    }))
}

/// What `child` wrote to each output that is piped, once each has closed, its input has, and it
/// has exited, unreaped: `None` if that hasn't happened by `by`.
fn follow(
    child: &mut Child,
    input: Option<&[u8]>,
    by: Deadline,
) -> io::Result<Option<(Vec<u8>, Vec<u8>)>> {
    let mut stdout = Reading::new(child.stdout.take().map(OwnedFd::from), "output")?;
    let mut stderr = Reading::new(child.stderr.take().map(OwnedFd::from), "error")?;
    let fd = child.stdin.take().map(OwnedFd::from);
    let mut stdin = Feeding::new(fd, input.unwrap_or_default())?;
    let mut pause = FIRST_PAUSE;
    loop {
        stdout.read()?;
        stderr.read()?;
        stdin.write()?;
        let closed = [&stdout.pipe, &stderr.pipe, &stdin.pipe]
            .iter()
            .all(|pipe| pipe.is_none());
        if closed && exited(child)? {
            return Ok(Some((stdout.read, stderr.read)));
        }
        let Some(left) = by.remaining() else {
            return Ok(None);
        };
        let mut ready: Vec<PollFd<'_>> = [
            stdout
                .pipe
                .as_ref()
                .map(|pipe| PollFd::new(pipe, PollFlags::IN)),
            stderr
                .pipe
                .as_ref()
                .map(|pipe| PollFd::new(pipe, PollFlags::IN)),
            stdin
                .pipe
                .as_ref()
                .map(|pipe| PollFd::new(pipe, PollFlags::OUT)),
        ]
        .into_iter()
        .flatten()
        .collect();
        wait(&mut ready, pause.min(left))?;
        pause = (pause * 2).min(MAX_PAUSE);
    }
}

/// Waits until one of `ready` is, or `pause`, which is under a second, has passed.
fn wait(ready: &mut [PollFd<'_>], pause: Duration) -> io::Result<()> {
    let timeout = Timespec {
        tv_sec: 0,
        tv_nsec: pause.subsec_nanos().into(),
    };
    match rustix::event::poll(ready, Some(&timeout)) {
        // A signal that this process catches ends the wait early, which the next one makes up.
        Ok(_) | Err(Errno::INTR) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Whether `child` has exited, leaving it unreaped.
fn exited(child: &Child) -> io::Result<bool> {
    let options = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
    let waited = rustix::process::waitid(WaitId::Pid(Pid::from_child(child)), options)?;
    Ok(waited.is_some())
}

/// An output of the child's, the standard `stream`, read until its pipe closes.
struct Reading {
    pipe: Option<File>,
    read: Vec<u8>,
    stream: &'static str,
}

impl Reading {
    fn new(fd: Option<OwnedFd>, stream: &'static str) -> io::Result<Self> {
        let pipe = fd.map(File::from);
        if let Some(pipe) = &pipe {
            rustix::io::ioctl_fionbio(pipe, true)?;
        }
        Ok(Self {
            pipe,
            read: Vec::new(),
            stream,
        })
    }

    /// Reads what the pipe holds, and closes it at its end; an error once it passes
    /// [`MAX_CAPTURED_BYTES`].
    fn read(&mut self) -> io::Result<()> {
        let Some(pipe) = &mut self.pipe else {
            return Ok(());
        };
        // A byte past the cap tells a stream that passes it from one that ends at it.
        let left = MAX_CAPTURED_BYTES + 1 - self.read.len();
        let read = Read::by_ref(pipe)
            .take(left as u64)
            .read_to_end(&mut self.read);
        match read {
            Ok(_) if self.read.len() > MAX_CAPTURED_BYTES => {
                let (stream, mib) = (self.stream, MAX_CAPTURED_BYTES >> 20);
                Err(io::Error::other(format!(
                    "its standard {stream} passed the {mib} MiB that a run captures, so it was \
                     killed: expected less"
                )))
            }
            Ok(_) => {
                self.pipe = None;
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(()),
            Err(error) => Err(error),
        }
    }
}

/// The child's standard input, fed `left` until that is all written, then closed.
struct Feeding<'a> {
    pipe: Option<File>,
    left: &'a [u8],
}

impl<'a> Feeding<'a> {
    fn new(fd: Option<OwnedFd>, input: &'a [u8]) -> io::Result<Self> {
        let pipe = fd.map(File::from);
        if let Some(pipe) = &pipe {
            rustix::io::ioctl_fionbio(pipe, true)?;
        }
        Ok(Self { pipe, left: input })
    }

    /// Writes what the pipe takes, and closes it once it has taken all.
    fn write(&mut self) -> io::Result<()> {
        let Some(pipe) = &mut self.pipe else {
            return Ok(());
        };
        while !self.left.is_empty() {
            match pipe.write(self.left) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(written) => self.left = &self.left[written..],
                Err(error) => match error.kind() {
                    io::ErrorKind::WouldBlock => return Ok(()),
                    io::ErrorKind::Interrupted => {}
                    // A command that closes its end first takes no more, which is no failure.
                    io::ErrorKind::BrokenPipe => break,
                    _ => return Err(error),
                },
            }
        }
        self.pipe = None;
        Ok(())
    }
}
