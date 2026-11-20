//! Socket I/O bounded by one deadline across all reads and writes of a frame.

use std::io::{self, Read, Write};
use std::os::fd::AsFd;

use rustix::net::sockopt::{Timeout, set_socket_timeout};
use stdx::Deadline;

pub(crate) struct Until<S> {
    stream: S,
    by: Deadline,
}

impl<S: AsFd> Until<S> {
    pub(crate) fn new(stream: S, by: Deadline) -> Self {
        Self { stream, by }
    }

    fn bound(&self, timeout: Timeout) -> io::Result<()> {
        let left = self.by.remaining().ok_or(io::ErrorKind::TimedOut)?;
        set_socket_timeout(&self.stream, timeout, Some(left))?;
        Ok(())
    }
}

fn timed_out(error: io::Error) -> io::Error {
    match error.kind() {
        io::ErrorKind::WouldBlock => io::ErrorKind::TimedOut.into(),
        _ => error,
    }
}

impl<S: AsFd + Read> Read for Until<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.bound(Timeout::Recv)?;
        self.stream.read(buf).map_err(timed_out)
    }
}

impl<S: AsFd + Write> Write for Until<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.bound(Timeout::Send)?;
        self.stream.write(buf).map_err(timed_out)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}
