//! Moving trees between the gateway and the sandbox: writing the trees the gateway puts, and
//! sending the trees it collects.
//!
//! A tree being put is written as its parts arrive, so [`Puts`] holds each tree in progress,
//! by its stream. A tree being collected is listed whole before anything is sent, so a tree
//! that can't be sent, such as one holding a link, fails before its first entry. Files are
//! expected not to change while they are sent, as after the sandbox is frozen.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use pumpkin_schema::ir::{AbsolutePath, Mode};
use pumpkin_schema::sandbox::frame::MAX_BODY_BYTES;
use pumpkin_schema::sandbox::tree::{self, Limits};
use pumpkin_schema::sandbox::{Entry, FromSandbox, RelativePath};
use stdx::Context;

use crate::session::Sender;
use crate::sys;

/// The trees being put, each written as its parts arrive.
#[derive(Default)]
pub(crate) struct Puts {
    /// The tree being put on each stream: `None` once it has failed, until its end arrives.
    trees: BTreeMap<u32, Option<tree::Writer>>,
}

impl Puts {
    /// Begins putting a tree in `dir` on `stream`, creating `dir` if need be.
    pub(crate) fn begin(
        &mut self,
        sender: &Sender,
        stream: u32,
        dir: &AbsolutePath,
    ) -> io::Result<()> {
        if self.trees.contains_key(&stream) {
            return Err(protocol_error(stream, "is already putting a tree"));
        }
        match fs::create_dir_all(dir.as_str()).context(dir) {
            Ok(()) => {
                let tree = tree::Writer::new(dir.as_str().into(), Limits::NONE);
                self.trees.insert(stream, Some(tree));
                Ok(())
            }
            Err(message) => {
                self.trees.insert(stream, None);
                sender.reply(stream, Err(message))
            }
        }
    }

    /// Writes an entry of the tree being put on `stream`, with `mode` ([`tree::Writer::entry`]).
    pub(crate) fn entry(
        &mut self,
        sender: &Sender,
        stream: u32,
        entry: &Entry,
        mode: Mode,
    ) -> io::Result<()> {
        self.write(sender, stream, |tree| tree.entry(entry, Some(mode)))
    }

    /// Writes bytes of the current file of the tree being put on `stream`
    /// ([`tree::Writer::data`]).
    pub(crate) fn data(&mut self, sender: &Sender, stream: u32, bytes: &[u8]) -> io::Result<()> {
        self.write(sender, stream, |tree| tree.data(bytes))
    }

    /// Writes a part of the tree being put on `stream`, unless an earlier part failed.
    fn write(
        &mut self,
        sender: &Sender,
        stream: u32,
        part: impl FnOnce(&mut tree::Writer) -> io::Result<()>,
    ) -> io::Result<()> {
        let Some(slot) = self.trees.get_mut(&stream) else {
            return Err(protocol_error(stream, "is putting no tree"));
        };
        let Some(tree) = slot else {
            return Ok(());
        };
        match part(tree) {
            Ok(()) => Ok(()),
            Err(error) => {
                *slot = None;
                sender.reply(stream, Err(error.to_string()))
            }
        }
    }

    /// Finishes the tree being put on `stream` and replies with the outcome, unless an earlier
    /// part failed, which has been replied to already.
    pub(crate) fn end(&mut self, sender: &Sender, stream: u32) -> io::Result<()> {
        match self.trees.remove(&stream) {
            None => Err(protocol_error(stream, "is putting no tree")),
            Some(None) => Ok(()),
            Some(Some(tree)) => {
                let outcome = tree.finish().map_err(|error| error.to_string());
                sender.reply(stream, outcome)
            }
        }
    }
}

fn protocol_error(stream: u32, what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("the gateway broke the protocol: stream {stream} {what}"),
    )
}

/// Sends the tree in `dir` on `stream`, or a failure.
pub(crate) fn collect(sender: &Sender, stream: u32, dir: &Path) -> io::Result<()> {
    let mut entries = Vec::new();
    if let Err(message) = list(dir, None, &mut entries) {
        return sender.reply(stream, Err(message));
    }
    let mut buffer = vec![0; MAX_BODY_BYTES];
    for (path, entry) in entries {
        let size = match &entry {
            Entry::Dir { .. } => None,
            Entry::File { size, .. } => Some(*size),
        };
        sender.send(&FromSandbox::Entry { stream, entry }, &[])?;
        if let Some(size) = size
            && let Err(message) = send_file(sender, stream, &path, size, &mut buffer)?
        {
            return sender.reply(stream, Err(message));
        }
    }
    sender.send(&FromSandbox::End { stream }, &[])
}

/// Lists the entries below `root`, or below `root/dir`, depth first and sorted by name, each
/// with its path on this machine.
fn list(
    root: &Path,
    dir: Option<&RelativePath>,
    entries: &mut Vec<(PathBuf, Entry)>,
) -> Result<(), String> {
    let full_dir = dir.map_or_else(|| root.to_owned(), |dir| root.join(dir.as_str()));
    let mut names: Vec<OsString> = fs::read_dir(&full_dir)
        .and_then(|read| read.map(|entry| Ok(entry?.file_name())).collect())
        .context(full_dir.display())?;
    names.sort_unstable();
    for name in names {
        let name = name.into_string().map_err(|name| {
            format!(
                "`{}` in {} is not UTF-8: collect only UTF-8 names",
                name.display(),
                full_dir.display()
            )
        })?;
        let path = RelativePath::try_from(match dir {
            Some(dir) => format!("{dir}/{name}"),
            None => name,
        })?;
        let full_path = root.join(path.as_str());
        let metadata = fs::symlink_metadata(&full_path).context(full_path.display())?;
        if metadata.is_dir() {
            entries.push((full_path, Entry::Dir { path: path.clone() }));
            list(root, Some(&path), entries)?;
        } else if metadata.is_file() {
            let size = metadata.len();
            entries.push((full_path, Entry::File { path, size }));
        } else {
            return Err(format!(
                "{} is neither a directory nor a regular file: collect only those",
                full_path.display()
            ));
        }
    }
    Ok(())
}

/// Sends the `size` bytes of the file at `path`, reading them through `buffer`, which holds a
/// frame's body. The outer error is the session's, the inner one the file's.
fn send_file(
    sender: &Sender,
    stream: u32,
    path: &Path,
    size: u64,
    buffer: &mut [u8],
) -> io::Result<Result<(), String>> {
    let file = match open(path, size) {
        Ok(file) => file,
        Err(message) => return Ok(Err(message)),
    };
    let mut file = file.take(size.saturating_add(1));
    let mut sent = 0;
    loop {
        let read = match file.read(buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Ok(Err(error).context(path.display())),
        };
        sent += read as u64;
        if sent > size {
            break;
        }
        sender.send(&FromSandbox::Data { stream }, &buffer[..read])?;
    }
    if sent == size {
        Ok(Ok(()))
    } else {
        Ok(Err(changed(path)))
    }
}

/// Opens the file at `path` to send it, which must still be the regular file of `size` bytes
/// that was listed.
fn open(path: &Path, size: u64) -> Result<File, String> {
    let file = sys::open_regular(path, OpenOptions::new().read(true))?;
    if file.metadata().context(path.display())?.len() == size {
        Ok(file)
    } else {
        Err(changed(path))
    }
}

fn changed(path: &Path) -> String {
    format!(
        "{} changed while it was collected: freeze the sandbox first",
        path.display()
    )
}
