//! A run's host, what the run reads of time and waits on, and what makes its record last: [`Host`]
//! starts the run's [`Clock`], which makes the [`Inbox`]es that the run's threads wait on, and
//! syncs each file of the record it writes ([`Host::sync`]). [`System`] is the machine's.
//!
//! What a run reads or waits on by the machine's time, whatever its host:
//! - the session's and the provider's calls, and the model route's, each take a [`Deadline`]
//!   that `deadline` makes from the clock as the time left then, so a later jump of the clock
//!   shortens only the waits made after it;
//! - the gateway's own deadlines and its looks at a socket;
//! - a start's looks between its lists of the provider's sandboxes;
//! - the simulated provider's sleeps (`SLOW`, `HUNG`, `LINGERING`);
//! - the model route's requests upstream, through `ureq`;
//! - [`Clock::unix`], the run's start by the machine's clock.

use std::fs::File;
use std::io;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::{Duration, Instant, SystemTime};

use stdx::Deadline;

/// The machine a run runs on, as far as the run reads time, waits, and makes its record last.
// Sync, as each judge call's thread keeps its sandbox's console through it.
pub trait Host: Sync {
    type Clock: Clock;

    /// The clock of a run that starts now.
    fn start(&self) -> Self::Clock;

    /// Makes what was written to `file`, a file or directory of a run's record, last through a
    /// crash of the machine, as far as `durable` says.
    ///
    /// # Errors
    ///
    /// Fails as the machine's sync does: what was written to `file` since its last sync that
    /// succeeded may then be lost.
    fn sync(&self, file: &File, durable: Durable) -> io::Result<()>;
}

/// What of a file [`Host::sync`] makes last.
#[derive(Clone, Copy, Debug)]
pub enum Durable {
    /// Its data, and what of its metadata reading the data back needs, such as its size, as
    /// `fdatasync` makes last.
    Data,
    /// Its data and all its metadata, and a directory's entries, as `fsync` makes last.
    All,
}

/// A run's clock: the time since the run started, which the core takes with each event, and when
/// that was by the machine's clock.
///
/// Each of the run's threads reads it through a clone of its own, and every clone reads the same
/// time.
// 'static, as the agent's session's reader, a thread that outlives the run's borrows, stamps each
// of the agent's arrivals with a clone.
pub trait Clock: Clone + Send + 'static {
    type Inbox<T: Send + 'static>: Inbox<T> + Send;

    /// The time since the run started.
    fn now(&self) -> Duration;

    /// When the run started, by the machine's clock.
    fn unix(&self) -> SystemTime;

    /// An empty inbox, whose waits this clock times.
    fn inbox<T: Send + 'static>(&self) -> Self::Inbox<T>;
}

/// What other threads send a thread, in the order they sent it, for it to wait on. It never
/// disconnects, whoever else holds a sender.
pub trait Inbox<T> {
    fn sender(&self) -> Sender<T>;

    /// What arrives first by `until`, a time since the run started, or once that has passed,
    /// what the inbox holds first, if anything.
    fn wait(&mut self, until: Duration) -> Option<T>;

    /// What the inbox holds first, if anything.
    fn take(&mut self) -> Option<T>;
}

/// The machine's host: its monotonic clock times the run, its inboxes are `std`'s channels, and
/// its syncs are the kernel's `fdatasync` and `fsync`.
#[derive(Clone, Copy, Debug)]
pub struct System;

impl Host for System {
    type Clock = SystemClock;

    fn start(&self) -> SystemClock {
        SystemClock {
            start: Instant::now(),
            unix: SystemTime::now(),
        }
    }

    fn sync(&self, file: &File, durable: Durable) -> io::Result<()> {
        match durable {
            Durable::Data => file.sync_data(),
            Durable::All => file.sync_all(),
        }
    }
}

/// [`System`]'s clock of a run.
#[derive(Clone, Copy, Debug)]
pub struct SystemClock {
    start: Instant,
    unix: SystemTime,
}

impl Clock for SystemClock {
    type Inbox<T: Send + 'static> = SystemInbox<T>;

    fn now(&self) -> Duration {
        self.start.elapsed()
    }

    fn unix(&self) -> SystemTime {
        self.unix
    }

    fn inbox<T: Send + 'static>(&self) -> SystemInbox<T> {
        let (sender, receiver) = mpsc::channel();
        SystemInbox {
            receiver,
            sender,
            clock: *self,
        }
    }
}

/// [`SystemClock`]'s inbox.
#[derive(Debug)]
pub struct SystemInbox<T> {
    receiver: Receiver<T>,
    // Kept, so that the inbox never disconnects while its owner waits on it, whoever else holds
    // a sender.
    sender: Sender<T>,
    clock: SystemClock,
}

impl<T> Inbox<T> for SystemInbox<T> {
    fn sender(&self) -> Sender<T> {
        self.sender.clone()
    }

    fn wait(&mut self, until: Duration) -> Option<T> {
        let left = until.saturating_sub(self.clock.now());
        match self.receiver.recv_timeout(left) {
            Ok(item) => Some(item),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => disconnected(),
        }
    }

    fn take(&mut self) -> Option<T> {
        match self.receiver.try_recv() {
            Ok(item) => Some(item),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => disconnected(),
        }
    }
}

// `std`'s channel reports a disconnection, which a channel with a sender never has, in the same
// errors as a timeout and an empty channel.
fn disconnected() -> ! {
    unreachable!(
        "an inbox disconnected, though it holds a sender of its own, so its channel can't \
         disconnect while the inbox is borrowed to wait on it"
    )
}

/// The deadline at `at`, a time since the run started by `clock`, for a call that takes one.
pub(super) fn deadline(clock: &impl Clock, at: Duration) -> Deadline {
    Deadline::after(at.saturating_sub(clock.now()))
}
