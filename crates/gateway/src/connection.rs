//! The connection that a session runs over, to a socket, over TCP or over TLS, which the session's
//! reader reads beside the threads that write it.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::unix::net::UnixStream;
use std::time::Duration;

#[derive(Debug)]
pub(crate) enum Connection {
    Unix(UnixStream),
    Tcp(TcpStream),
    Tls(tls::Stream),
}

impl Connection {
    pub(crate) fn shutdown(&self) -> io::Result<()> {
        match self {
            Self::Unix(stream) => stream.shutdown(Shutdown::Both),
            Self::Tcp(stream) => stream.shutdown(Shutdown::Both),
            Self::Tls(stream) => stream.shutdown(),
        }
    }

    pub(crate) fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        match self {
            Self::Unix(stream) => stream.set_nonblocking(nonblocking),
            Self::Tcp(stream) => stream.set_nonblocking(nonblocking),
            Self::Tls(stream) => stream.set_nonblocking(nonblocking),
        }
    }

    pub(crate) fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Self::Unix(stream) => stream.set_write_timeout(timeout),
            Self::Tcp(stream) => stream.set_write_timeout(timeout),
            Self::Tls(stream) => stream.set_write_timeout(timeout),
        }
    }
}

// Through a shared reference, as `UnixStream` and `TcpStream` read and write, since the session's
// reader thread reads beside the owner that writes and may end the session.
impl Read for &Connection {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Connection::Unix(stream) => (&mut &*stream).read(buf),
            Connection::Tcp(stream) => (&mut &*stream).read(buf),
            Connection::Tls(stream) => (&mut &*stream).read(buf),
        }
    }
}

impl Write for &Connection {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Connection::Unix(stream) => (&mut &*stream).write(buf),
            Connection::Tcp(stream) => (&mut &*stream).write(buf),
            Connection::Tls(stream) => (&mut &*stream).write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Connection::Unix(stream) => (&mut &*stream).flush(),
            Connection::Tcp(stream) => (&mut &*stream).flush(),
            Connection::Tls(stream) => (&mut &*stream).flush(),
        }
    }
}
