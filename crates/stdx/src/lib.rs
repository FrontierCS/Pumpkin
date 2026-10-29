//! Small extensions to the standard library that Pumpkin's crates share.

use std::fmt::Display;

/// Says what failed, by prefixing an error's message with `context`, as `<context>: <error>`.
///
/// The error becomes a `String`. An error that no caller branches on is rendered as soon as
/// it happens, because where it goes is already known (standard error, a protocol reply, a
/// log), so text is all it needs to be. No source error converts to a `String` by itself, so
/// on a path that returns one, `?` compiles only once the call says what it was doing. An
/// error that a caller does branch on keeps its type, and is checked by kind at the call.
///
/// `context` can be any [`Display`], formatted only on failure, so it costs nothing on
/// success: `path.display()` is a borrowed wrapper, as is
/// `format_args!("reading {}", path.display())`, which adds words.
///
/// ```
/// use stdx::Context;
///
/// let error = "x".parse::<u16>().context("the port").unwrap_err();
/// assert_eq!(error, "the port: invalid digit found in string");
/// ```
pub trait Context<T> {
    /// Prefixes the error's message with `context`.
    ///
    /// # Errors
    ///
    /// Fails if `self` holds an error, with `<context>: <error>`.
    fn context(self, context: impl Display) -> Result<T, String>;
}

impl<T, E: Display> Context<T> for Result<T, E> {
    fn context(self, context: impl Display) -> Result<T, String> {
        self.map_err(|error| format!("{context}: {error}"))
    }
}
