//! A run's host: what the run reads of time and waits on, and what makes its record last.
//!
//! Every deadline of a run is a time on the run's clock. A session's requests, a model response's
//! frames and a start's wait for its sandbox take their deadlines as times on it. What a run waits
//! on by the machine's time, whatever its host:
//! - within a start's wait for its sandbox, each dial, its TLS and its hello, and the private
//!   listener's wait for a check, which take the time left as they begin;
//! - a provider's own calls, which wait on its platform;
//! - the model route's exchange upstream, through `ureq`, which takes the time left as it begins,
//!   and its resolve, connect and reads, by bounds of their own.

use std::fs::File;
use std::io;
use std::time::{Duration, Instant, SystemTime};

use stdx::io::{Backend, Durable};

/// The machine a run runs on, as far as the run reads time, waits, and makes its record last.
pub trait Host {
    type Clock: Clock;

    /// The clock of a run that starts now.
    fn start(&self) -> Self::Clock;

    /// What the loop that the run's driver runs on performs its operations on, whose clock is the
    /// run's.
    fn backend(&self) -> io::Result<Box<dyn Backend>>;

    /// Makes what was written to `file`, a file or directory of a run's record, last through a
    /// crash of the machine, as far as `durable` says. A failure may lose what was written to
    /// `file` since its last sync that succeeded.
    fn sync(&self, file: &File, durable: Durable) -> io::Result<()>;
}

/// A run's clock, whose origin is the run's start: the core takes its time with each event.
///
/// Each of the run's threads that reads it reads a clone of its own.
pub trait Clock: stdx::time::Clock + Send {}

/// The machine's host: its monotonic clock times the run, and its syncs are the kernel's
/// `fdatasync` and `fsync`.
#[derive(Clone, Copy, Debug)]
pub struct System;

impl Host for System {
    type Clock = SystemClock;

    fn start(&self) -> SystemClock {
        SystemClock {
            start: Instant::now(),
        }
    }

    fn backend(&self) -> io::Result<Box<dyn Backend>> {
        stdx::io::machine()
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
}

impl stdx::time::Clock for SystemClock {
    fn now(&self) -> Duration {
        self.start.elapsed()
    }

    fn realtime(&self) -> SystemTime {
        SystemTime::now()
    }

    fn look(&self) -> Option<Duration> {
        None
    }
}

impl Clock for SystemClock {}
