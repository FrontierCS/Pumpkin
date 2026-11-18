//! Why a reader of a record failed, told apart by whether the record is of a form this build
//! reads.

use std::fmt::{self, Display};
use std::path::Path;
use std::{fs, io};

/// Why reading a record failed.
///
/// A message says what failed and where, and a caller never branches on its text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadError {
    /// The record is in a form this build doesn't read: another `record_version`, JSON that
    /// doesn't parse or doesn't have the form expected, or a file of that form missing.
    Form(String),
    /// Reading it failed, as the kernel reported, or the path isn't a run's record.
    Failed(String),
}

impl Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (Self::Form(message) | Self::Failed(message)) = self;
        f.write_str(message)
    }
}

impl From<ReadError> for String {
    fn from(error: ReadError) -> Self {
        error.to_string()
    }
}

/// The bytes of `path`, or `None` if there is no such file.
pub(crate) fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, ReadError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(failed(path.display())(error)),
    }
}

/// The file at `path`, which a record's form has: one that isn't there is a form this build
/// doesn't read, and one that is there but can't be opened is a failure.
pub(crate) fn open_required(path: &Path) -> Result<fs::File, ReadError> {
    fs::File::open(path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => ReadError::Form(format!(
            "{} isn't there: expected the record of a run to have it",
            path.display()
        )),
        _ => failed(path.display())(error),
    })
}

/// The [`ReadError::Failed`] that an I/O error on `path` makes.
pub(crate) fn failed(path: impl Display) -> impl FnOnce(io::Error) -> ReadError {
    move |error| ReadError::Failed(format!("{path}: {error}"))
}

/// The [`ReadError::Form`] that `error`, from parsing the content of `path`, makes.
pub(crate) fn form<E: Display>(path: impl Display) -> impl FnOnce(E) -> ReadError {
    move |error| ReadError::Form(format!("{path}: {error}"))
}
