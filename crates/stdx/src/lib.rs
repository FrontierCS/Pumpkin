//! Small extensions to the standard library that Pumpkin's crates share.

use std::fmt::Display;
use std::{panic, process};

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
/// A binary calls it first in `main`, so that it aborts on a panic in dev builds too, which
/// unwind: the workspace's `Cargo.toml` says why a panic ends the process, and why dev builds
/// unwind.
pub fn abort_on_panic() {
    let report = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        report(info);
        process::abort();
    }));
}
