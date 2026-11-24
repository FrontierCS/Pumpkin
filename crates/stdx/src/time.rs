//! A clock that a component reads time by, which its owner injects, so that a test can script it.
//!
//! A time on a clock is the time since the clock's origin, and a deadline is such a time. The
//! machine's clock is [`Machine`]. A test may give one that moves only as the test says, and a wait
//! on the machine's timers then reads it again every [`Clock::look`].

use std::time::{Duration, Instant, SystemTime};

use rustix::time::ClockId;

/// A clock: its monotonic time, its wall-clock time, and how often a wait reads it again.
///
/// Every clone reads the same time.
// The monotonic and the wall-clock time behind one interface, which a simulator implements, as
// TigerBeetle's `Time`:
// https://github.com/tigerbeetle/tigerbeetle/blob/68958eb757747d0fc96a79d1f9c2fa6766b67b48/src/stdx/time.zig#L15-L38
pub trait Clock: Clone {
    /// The time since the clock's origin, which never goes back.
    fn now(&self) -> Duration;

    /// The wall-clock time now, which may jump.
    fn realtime(&self) -> SystemTime;

    /// The longest that one wait on the machine's timers may block before it reads this clock
    /// again. None where the clock moves with those timers, so that the time left bounds the wait.
    fn look(&self) -> Option<Duration>;
}

/// The machine's monotonic clock, whose origin the kernel sets, at its boot.
#[derive(Clone, Copy, Debug)]
pub struct Machine;

impl Clock for Machine {
    fn now(&self) -> Duration {
        // `Instant`'s clock on Linux, which `Instant` doesn't give as a time since its origin.
        let now = rustix::time::clock_gettime(ClockId::Monotonic);
        let Ok(now) = Duration::try_from(now) else {
            unreachable!("the monotonic clock read before its origin: {now:?}")
        };
        now
    }

    fn realtime(&self) -> SystemTime {
        SystemTime::now()
    }

    fn look(&self) -> Option<Duration> {
        None
    }
}

/// The time left until `at`, a time on `clock`.
///
/// It is never zero: `None` once `at` has passed, and [`Duration::MAX`] if `at` is past what the
/// machine's clock can hold, and so never reached, as [`crate::Deadline::remaining`] gives.
pub fn left(clock: &impl Clock, at: Duration) -> Option<Duration> {
    let left = (at.checked_sub(clock.now())).filter(|left| !left.is_zero())?;
    Some(match Instant::now().checked_add(left) {
        Some(_) => left,
        None => Duration::MAX,
    })
}
