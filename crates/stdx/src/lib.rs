//! Small extensions to the standard library that Pumpkin's crates share.

use std::fmt::Display;
use std::panic;
use std::sync::LockResult;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub mod process;
pub mod signal;

/// Prefixes an error with what failed, as `<context>: <error>`, rendering it as a `String`.
///
/// ```
/// use stdx::Context;
///
/// let error = "x".parse::<u16>().context("the port").unwrap_err();
/// assert_eq!(error, "the port: invalid digit found in string");
/// ```
pub trait Context<T> {
    /// # Errors
    ///
    /// Fails with `<context>: <error>` if `self` holds an error.
    fn context(self, context: impl Display) -> Result<T, String>;
}

impl<T, E: Display> Context<T> for Result<T, E> {
    fn context(self, context: impl Display) -> Result<T, String> {
        self.map_err(|error| format!("{context}: {error}"))
    }
}

/// Makes a panic in any thread abort the whole process once the default hook has reported it,
/// as `panic = "abort"` does.
///
/// A binary that runs threads calls it first in `main`, so that it aborts on a panic in dev
/// builds too, which unwind: the workspace's `Cargo.toml` says why a panic ends the process, and
/// why dev builds unwind. A single-threaded binary needs none, since a panic there already ends
/// it.
pub fn abort_on_panic() {
    let report = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        report(info);
        std::process::abort();
    }));
}

/// The guard from locking a mutex, or from waiting on a condition variable with one, in a
/// process where a panic ends it before it unwinds, as [`abort_on_panic`] and
/// `panic = "abort"` make it.
///
/// # Panics
///
/// Panics if a thread panicked while holding the lock, which a process can see only where a
/// panic unwinds, as in tests.
pub fn unpoisoned<T>(locked: LockResult<T>) -> T {
    // A lock that can't be poisoned would take a dependency, as std's (`sync::nonpoison`) is
    // unstable.
    let Ok(guard) = locked else {
        unreachable!("a thread panicked while holding a lock, which only a bug does")
    };
    guard
}

/// An operation's end, made from its time left and shared by every step.
///
/// ```
/// use stdx::Deadline;
///
/// let max = std::time::Duration::MAX;
/// assert_eq!(Deadline::after(max).remaining(), Some(max));
/// assert_eq!(Deadline::after(std::time::Duration::ZERO).remaining(), None);
/// ```
#[derive(Clone, Copy, Debug)]
pub struct Deadline(Option<Instant>);

impl Deadline {
    /// The deadline `remaining` from now; one past what the clock can hold is never reached.
    pub fn after(remaining: Duration) -> Self {
        Self(Instant::now().checked_add(remaining))
    }

    /// The deadline `since_epoch` after the Unix epoch by this machine's clock, as it reads now,
    /// so that a later change of the clock doesn't move it: one already past has passed, and one
    /// past what the clock can hold is never reached.
    pub fn at_unix(since_epoch: Duration) -> Self {
        let Some(at) = UNIX_EPOCH.checked_add(since_epoch) else {
            return Self(None);
        };
        let remaining = at.duration_since(SystemTime::now());
        Self::after(remaining.unwrap_or(Duration::ZERO))
    }

    /// The time left, which is never zero: `None` once the deadline has passed, and
    /// [`Duration::MAX`] if it is never reached.
    pub fn remaining(self) -> Option<Duration> {
        let Some(at) = self.0 else {
            return Some(Duration::MAX);
        };
        let now = Instant::now();
        (now < at).then(|| at.duration_since(now))
    }
}

/// A time in UTC, to the second, in the proleptic Gregorian calendar, after the Unix epoch.
///
/// ```
/// let shown = |seconds| stdx::Utc::from_unix(seconds).to_string();
/// assert_eq!(shown(951_827_696), "2000-02-29T12:34:56");
/// assert_eq!(shown(4_133_980_799), "2100-12-31T23:59:59");
/// assert_eq!(stdx::Utc::from_unix(951_827_696).to_unix(), 951_827_696);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Utc {
    pub year: u64,
    pub month: u64,
    pub day: u64,
    pub hour: u64,
    pub minute: u64,
    pub second: u64,
}

impl Utc {
    /// The time `seconds` after the Unix epoch.
    pub fn from_unix(seconds: u64) -> Self {
        let (days, time) = (seconds / 86_400, seconds % 86_400);
        // Days to a civil date, after Howard Hinnant's `civil_from_days`, for days from 1970 on.
        let shifted = days + 719_468;
        let (era, day_of_era) = (shifted / 146_097, shifted % 146_097);
        let year_of_era =
            (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
        let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
        let month_from_march = (5 * day_of_year + 2) / 153;
        let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
        let month = if month_from_march < 10 {
            month_from_march + 3
        } else {
            month_from_march - 9
        };
        Self {
            year: year_of_era + era * 400 + u64::from(month <= 2),
            month,
            day,
            hour: time / 3600,
            minute: time % 3600 / 60,
            second: time % 60,
        }
    }

    /// The seconds after the Unix epoch at this time, which is not before it.
    pub fn to_unix(&self) -> u64 {
        let Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
        } = *self;
        // A civil date to days, after Howard Hinnant's `days_from_civil`, the inverse of the
        // above.
        let year = year - u64::from(month <= 2);
        let (era, year_of_era) = (year / 400, year % 400);
        let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
        let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
        let days = era * 146_097 + day_of_era - 719_468;
        days * 86_400 + hour * 3600 + minute * 60 + second
    }
}

/// As ISO 8601 writes it without its zone: `YYYY-MM-DDThh:mm:ss`.
impl Display for Utc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
        } = self;
        write!(
            f,
            "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}"
        )
    }
}
