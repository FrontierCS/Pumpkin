//! Writing a tree that arrives over a session into a directory.
//!
//! Both ends use it: `sandboxd` for what the gateway puts, the gateway for what it collects,
//! which may come from a buggy sandbox. Parsing has already confined every path to the
//! directory, so the writer checks how the parts fit: each entry's parent exists, file sizes
//! match their data, and the tree stays within its [`Limits`]. Since no entry creates a missing
//! parent, each creates at most one directory or file.

use std::fs::{self, DirBuilder, File, OpenOptions, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

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
    /// No limit, for trees from the host, which decides what it sends.
    pub const NONE: Self = Self {
        bytes: u64::MAX,
        entries: u64::MAX,
    };
}

/// How a [`Writer`] gives each entry its mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Modes {
    /// Exactly, whatever the umask: in a sandbox, where the session's owner decides each mode.
    Exact,
    /// Within the umask, as the kernel applies it to what a process creates: on the host, whose
    /// umask keeps what it writes from other users.
    Umasked,
}

/// Writes the parts of one tree, in the order they arrive, below a directory.
#[derive(Debug)]
pub struct Writer {
    root: PathBuf,
    limits: Limits,
    modes: Modes,
    bytes: u64,
    entries: u64,
    /// The file being written, its path, and the bytes still due for it.
    file: Option<(File, RelativePath, u64)>,
}

impl Writer {
    /// A writer into `root`, which exists.
    pub fn new(root: PathBuf, limits: Limits, modes: Modes) -> Self {
        Self {
            root,
            limits,
            modes,
            bytes: 0,
            entries: 0,
            file: None,
        }
    }

    /// Creates an entry below its parent, which must exist: a directory, unless it exists, or a
    /// new file, whose bytes [`Writer::data`] then writes. It gets the entry's mode as [`Modes`]
    /// says, but that a directory's owner, the receiving side's user, may always read, write and
    /// search it, so that it can fill it and remove it. A directory that exists is kept as it is.
    ///
    /// # Errors
    ///
    /// Fails if the previous file is incomplete, if a limit is passed, if the parent is
    /// missing, if a file already exists, or if creating the entry, or giving it its mode, fails.
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
            Entry::Dir { path, mode } => {
                let target = self.root.join(path.as_str());
                let mode = mode.bits() | 0o700;
                let created = create_dir(&target, mode).map_err(|error| at(path, &error))?;
                if created && self.modes == Modes::Exact {
                    let mode = Permissions::from_mode(mode);
                    fs::set_permissions(&target, mode).map_err(|error| at(path, &error))?;
                }
                Ok(())
            }
            Entry::File { path, size, mode } => {
                self.bytes = self.bytes.saturating_add(*size);
                if self.bytes > self.limits.bytes {
                    return Err(over_limit(format!(
                        "the tree's files hold more than {} bytes",
                        self.limits.bytes
                    )));
                }
                let target = self.root.join(path.as_str());
                // The file stays open for writing, whatever its mode.
                let file = (OpenOptions::new().write(true).create_new(true))
                    .mode(mode.bits())
                    .open(&target)
                    .map_err(|error| at(path, &error))?;
                if self.modes == Modes::Exact {
                    (file.set_permissions(Permissions::from_mode(mode.bits())))
                        .map_err(|error| at(path, &error))?;
                }
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

/// Creates the directory at `path`, whose parent exists, with `mode` within the umask, unless a
/// directory is already there. Returns whether it created one.
fn create_dir(path: &Path, mode: u32) -> io::Result<bool> {
    match DirBuilder::new().mode(mode).create(path) {
        Err(error)
            if error.kind() == io::ErrorKind::AlreadyExists
                && fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir()) =>
        {
            Ok(false)
        }
        created => created.map(|()| true),
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
