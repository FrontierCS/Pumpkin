//! Small extensions to the standard library that Pumpkin's crates share.

use std::fmt::Display;
use std::panic;
use std::sync::LockResult;
use std::time::{Duration, Instant};

pub mod process;

/// Prefixes an error with what failed, as `<context>: <error>`, rendering it as a `String`.
///
/// `context` is formatted only on failure, so a borrowed one such as `path.display()` costs
/// nothing on success.
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
    #[expect(
        clippy::unreachable,
        reason = "only a thread that panics while holding a lock poisons it, and a panic is a \
                  bug; a lock that can't be poisoned would take a dependency, as std's \
                  (`sync::nonpoison`) is unstable"
    )]
    let Ok(guard) = locked else {
        unreachable!("a thread panicked while holding a lock")
    };
    guard
}

/// The instant by which an operation is to end, made from the time it has left, so that each
/// step an operation hands it to ends by the same instant.
///
/// ```
/// use std::time::Duration;
/// use stdx::Deadline;
///
/// assert_eq!(Deadline::after(Duration::MAX).remaining(), Some(Duration::MAX));
/// let soon = Deadline::after(Duration::from_millis(1));
/// while let Some(left) = soon.remaining() {
///     assert!(!left.is_zero());
/// }
/// assert_eq!(Deadline::after(Duration::ZERO).remaining(), None);
/// ```
#[derive(Clone, Copy, Debug)]
pub struct Deadline(Option<Instant>);

impl Deadline {
    /// The deadline `remaining` from now; one past what the clock can hold is never reached.
    pub fn after(remaining: Duration) -> Self {
        Self(Instant::now().checked_add(remaining))
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
