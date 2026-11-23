//! A run's host: what the run reads of time and waits on, and what makes its record last.
//!
//! Every wait of a run ends by the run's clock. A session's requests, a model response's frames
//! and a start's wait for its sandbox take the time left as a function that reads the clock
//! (`until`, `whole_until`, `left`), which the gateway reads again as it waits. What a run still
//! waits on by the machine's time, whatever its host:
//! - within a start's wait for its sandbox, each dial, its TLS and its hello, the answer to its
//!   hello, and the private listener's wait for a check, which take the time left as they begin;
//! - a provider's own calls, which wait on its platform;
//! - the model route's exchange upstream, through `ureq`, which takes the time left as it begins,
//!   and its resolve, connect and reads, by bounds of their own;
//! - [`Clock::unix`], the run's start by the machine's clock.

use std::fs::File;
use std::io;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::{Duration, Instant, SystemTime};

/// The machine a run runs on, as far as the run reads time, waits, and makes its record last.
// Sync, as each child's thread syncs the record of its sandbox's start and delete through it.
pub trait Host: Sync {
    type Clock: Clock;

    /// The clock of a run that starts now.
    fn start(&self) -> Self::Clock;

    /// Makes what was written to `file`, a file or directory of a run's record, last through a
    /// crash of the machine, as far as `durable` says. A failure may lose what was written to
    /// `file` since its last sync that succeeded.
    fn sync(&self, file: &File, durable: Durable) -> io::Result<()>;
}

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

    fn now(&self) -> Duration;

    /// When the run started, by the machine's clock.
    fn unix(&self) -> SystemTime;

    /// An empty inbox, whose waits this clock times.
    fn inbox<T: Send + 'static>(&self) -> Self::Inbox<T>;

    /// The longest that a wait on the machine's timers may block before it reads this clock
    /// again, where the clock can move while the wait blocks; none where only the wait's own time
    /// left bounds it.
    fn look(&self) -> Option<Duration> {
        None
    }
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

/// The time left until `at`, a time since the run started by `clock`: never zero, and none once
/// it has passed, as [`stdx::Deadline::remaining`] gives.
pub(super) fn left(clock: &impl Clock, at: Duration) -> Option<Duration> {
    (at.checked_sub(clock.now())).filter(|left| !left.is_zero())
}

/// How long a wait for `at` may block on the machine's timers before it calls the function again,
/// on any thread: the time left, as [`left`] gives it, at most `clock`'s [`Clock::look`].
pub(super) fn until<C: Clock>(
    clock: &C,
    at: Duration,
) -> impl Fn() -> Option<Duration> + Send + 'static + use<C> {
    let clock = clock.clone();
    move || {
        let left = left(&clock, at)?;
        Some(clock.look().map_or(left, |look| left.min(look)))
    }
}

/// The time left until `at`, as [`left`] gives it, whole, for a bound that a wait takes once as it
/// begins, on any thread.
pub(super) fn whole_until<C: Clock>(
    clock: &C,
    at: Duration,
) -> impl Fn() -> Option<Duration> + Send + 'static + use<C> {
    let clock = clock.clone();
    move || left(&clock, at)
}
