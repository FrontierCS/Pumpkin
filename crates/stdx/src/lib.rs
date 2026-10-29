//! Small extensions to the standard library that Pumpkin's crates share.

use std::fmt::Display;

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
