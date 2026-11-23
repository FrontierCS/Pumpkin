//! A request's bound by its caller's clock, which every wait of a session reads as it goes.

use std::fmt;
use std::io::{self, Write};
use std::time::Duration;

use stdx::Deadline;

use crate::Connection;

// The time left of a request, by its caller's clock.
#[derive(Clone, Copy)]
pub(crate) struct Bound<'a> {
    pub(crate) left: &'a dyn Fn() -> Option<Duration>,
}

impl Bound<'_> {
    // The time left, none once it has passed.
    pub(crate) fn left(self) -> Option<Duration> {
        (self.left)()
    }

    // The deadline by the machine's clock at the time left now, for a socket's waits: a dial's,
    // and an answer's to a hello.
    pub(crate) fn deadline(self) -> Deadline {
        Deadline::after(self.left().unwrap_or_default())
    }
}

pub(crate) fn passed(error: &io::Error) -> bool {
    // A socket's timeout fails a call as it would fail a non-blocking one.
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

// Writes to a connection until `by`, after which every write fails as timed out.
pub(crate) struct Until<'a> {
    pub(crate) connection: &'a Connection,
    pub(crate) by: Bound<'a>,
}

impl Write for Until<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        loop {
            // Set before each write, since the socket's timeout bounds each call on its own. A
            // deadline that has passed fails here, not in the socket, whose `None` would mean no
            // timeout at all. A write that times out has written nothing, so it is made again,
            // once `left` says time is left.
            let Some(left) = self.by.left() else {
                return Err(io::ErrorKind::TimedOut.into());
            };
            self.connection.set_write_timeout(Some(left))?;
            match (&mut &*self.connection).write(buf) {
                Err(error) if passed(&error) => {}
                written => return written,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        (&mut &*self.connection).flush()
    }
}

// A response's time left, which its later frames read.
pub(crate) struct Left(pub(crate) Box<dyn Fn() -> Option<Duration> + Send>);

impl fmt::Debug for Left {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Left")
    }
}
