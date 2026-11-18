//! Small extensions to the standard library that Pumpkin's crates share.

use std::fmt::Display;
use std::sync::LockResult;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{io, panic, thread};

pub mod net;
pub mod process;
pub mod signal;
pub mod slots;

/// Prefixes an error with what failed, as `<context>: <error>`, rendering it as a `String`.
///
/// ```
/// use stdx::Context;
///
/// let error = "x".parse::<u16>().context("the port").unwrap_err();
/// assert_eq!(error, "the port: invalid digit found in string");
/// ```
pub trait Context<T> {
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

/// What `run` returns, run at the top of a stack of `stack_bytes` on a thread named `name`.
///
/// This waits for the thread, to which `run` may lend borrows: an evaluator's bound on nesting
/// holds only on a stack of the size it was made for, which the calling thread's may not be. A
/// panic of `run`'s passes on to the caller.
///
/// # Errors
///
/// Fails if the thread doesn't start.
pub fn on_stack<T: Send>(
    name: &str,
    stack_bytes: usize,
    run: impl FnOnce() -> T + Send,
) -> io::Result<T> {
    thread::scope(|scope| {
        let thread = (thread::Builder::new())
            .name(name.to_owned())
            .stack_size(stack_bytes);
        let running = thread.spawn_scoped(scope, run)?;
        Ok(running
            .join()
            .unwrap_or_else(|panic| panic::resume_unwind(panic)))
    })
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
