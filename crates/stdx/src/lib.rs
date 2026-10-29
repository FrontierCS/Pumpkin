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
/// use std::fs;
/// use std::path::Path;
///
/// use stdx::Context;
///
/// let path = Path::new("/nonexistent/pumpkin.toml");
/// let error = fs::read_to_string(path).context(path.display()).unwrap_err();
/// assert_eq!(error, "/nonexistent/pumpkin.toml: No such file or directory (os error 2)");
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
