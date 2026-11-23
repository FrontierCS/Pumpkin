//! A request's deadline as a time on its clock, which every wait of a session reads as it goes.

use std::io::{self, Write};
use std::time::Duration;

use stdx::Deadline;
use stdx::time::{self, Clock};

use crate::{Connection, ModelResponse, Session};

// The deadline `by`, a time on `clock`.
pub(crate) struct Bound<'a, C> {
    pub(crate) clock: &'a C,
    pub(crate) by: Duration,
}

impl<C> Clone for Bound<'_, C> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<C> Copy for Bound<'_, C> {}

impl<C: Clock> Bound<'_, C> {
    // The time left, none once it has passed.
    pub(crate) fn left(self) -> Option<Duration> {
        time::left(self.clock, self.by)
    }

    // How long one wait may block before it reads the clock again: the time left, at most the
    // clock's look, none once the deadline has passed.
    pub(crate) fn wait(self) -> Option<Duration> {
        let left = self.left()?;
        Some(self.clock.look().map_or(left, |look| left.min(look)))
    }

    // The deadline by the machine's clock at the time left now, for a socket's waits: a dial's,
    // and its TLS's.
    pub(crate) fn deadline(self) -> Deadline {
        Deadline::after(self.left().unwrap_or_default())
    }
}

impl<C> Session<C> {
    // The deadline `by`, a time on the session's clock.
    pub(crate) fn by(&self, by: Duration) -> Bound<'_, C> {
        Bound {
            clock: &self.clock,
            by,
        }
    }
}

impl<C: Clock> ModelResponse<C> {
    pub(crate) fn by(&self) -> Bound<'_, C> {
        Bound {
            clock: &self.clock,
            by: self.by,
        }
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
pub(crate) struct Until<'a, C> {
    pub(crate) connection: &'a Connection,
    pub(crate) by: Bound<'a, C>,
}

impl<C: Clock> Write for Until<'_, C> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        loop {
            // Set before each write, since the socket's timeout bounds each call on its own. A
            // deadline that has passed fails here, not in the socket, whose `None` would mean no
            // timeout at all. A write that times out has written nothing, so it is made again,
            // once the clock says time is left.
            let Some(wait) = self.by.wait() else {
                return Err(io::ErrorKind::TimedOut.into());
            };
            self.connection.set_write_timeout(Some(wait))?;
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
