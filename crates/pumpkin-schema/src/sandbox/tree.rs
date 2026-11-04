//! Writing a tree that arrives over a session into a directory.
//!
//! Both ends use it: `sandboxd` for what the gateway puts, the gateway for what it collects,
//! which a sandbox running untrusted code produced. Parsing has already confined every path
//! to the directory, so the writer checks how the parts fit: each entry's parent exists, file
//! sizes match their data, and the tree stays within its [`Limits`]. Since no entry creates a
//! missing parent, each creates at most one directory or file.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::PathBuf;

use super::{Entry, RelativePath};

/// The most a tree may hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// File bytes, in all.
    pub bytes: u64,
    /// Entries, directories included.
    pub entries: u64,
}

impl Limits {
    /// No limit, for trees from a trusted sender.
    pub const NONE: Self = Self {
        bytes: u64::MAX,
        entries: u64::MAX,
    };
}

/// Writes the parts of one tree, in the order they arrive, below a directory.
#[derive(Debug)]
pub struct Writer {
    root: PathBuf,
    limits: Limits,
    bytes: u64,
    entries: u64,
    /// The file being written, its path, and the bytes still due for it.
    file: Option<(File, RelativePath, u64)>,
}

impl Writer {
    /// A writer into `root`, which exists.
    pub fn new(root: PathBuf, limits: Limits) -> Self {
        Self {
            root,
            limits,
            bytes: 0,
            entries: 0,
            file: None,
        }
    }

    /// Creates an entry below its parent, which must exist: a directory, unless it exists,
    /// or a new file, whose bytes [`Writer::data`] then writes.
    ///
    /// # Errors
    ///
    /// Fails if the previous file is incomplete, if a limit is passed, if the parent is
    /// missing, if a file already exists, or if creating the entry fails.
    pub fn entry(&mut self, entry: &Entry) -> io::Result<()> {
        self.check_file_complete()?;
        self.entries += 1;
        if self.entries > self.limits.entries {
            return Err(over_limit(format!(
                "the tree has more than {} entries",
                self.limits.entries
            )));
        }
        match entry {
            Entry::Dir { path } => {
                create_dir(&self.root.join(path.as_str())).map_err(|error| at(path, &error))
            }
            Entry::File { path, size } => {
                self.bytes = self.bytes.saturating_add(*size);
                if self.bytes > self.limits.bytes {
                    return Err(over_limit(format!(
                        "the tree's files hold more than {} bytes",
                        self.limits.bytes
                    )));
                }
                let target = self.root.join(path.as_str());
                let file = File::create_new(&target).map_err(|error| at(path, &error))?;
                if *size > 0 {
                    self.file = Some((file, path.clone(), *size));
                }
                Ok(())
            }
        }
    }

    /// Writes bytes of the current file.
    ///
    /// # Errors
    ///
    /// Fails if no file is due bytes, if there are none, if they run past the file's size, or
    /// if writing fails.
    pub fn data(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Err(invalid(
                "file data is empty: send at least one byte in each frame",
            ));
        }
        let Some((file, path, due)) = &mut self.file else {
            return Err(invalid(
                "file data arrived outside a file: send it after a file entry",
            ));
        };
        let len = bytes.len() as u64;
        if len > *due {
            return Err(invalid(
                "file data runs past the file's size: send exactly the size given",
            ));
        }
        file.write_all(bytes).map_err(|error| at(path, &error))?;
        *due -= len;
        if *due == 0 {
            self.file = None;
        }
        Ok(())
    }

    /// Ends the tree.
    ///
    /// # Errors
    ///
    /// Fails if the last file is incomplete.
    pub fn finish(self) -> io::Result<()> {
        self.check_file_complete()
    }

    fn check_file_complete(&self) -> io::Result<()> {
        match &self.file {
            None => Ok(()),
            Some((_, _, due)) => Err(invalid(format!(
                "a file ended {due} bytes short: send all of its bytes before the next part"
            ))),
        }
    }
}

/// Creates the directory at `path`, whose parent exists, unless a directory is already there.
fn create_dir(path: &std::path::Path) -> io::Result<()> {
    match fs::create_dir(path) {
        Err(error)
            if error.kind() == io::ErrorKind::AlreadyExists
                && fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir()) =>
        {
            Ok(())
        }
        created => created,
    }
}

/// An error about the entry at `path`, keeping its kind.
fn at(path: &RelativePath, error: &io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{path}: {error}"))
}

fn over_limit(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::FileTooLarge, message)
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
