//! Sending the tree in a directory to the gateway.
//!
//! The whole directory is listed before anything is sent, so a tree that can't be sent, such
//! as one holding a link, fails before its first entry. Files are expected not to change
//! while they are sent, as after the sandbox is frozen.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use pumpkin_schema::sandbox::frame::MAX_BODY_BYTES;
use pumpkin_schema::sandbox::{Entry, FromSandbox, RelativePath};
use stdx::Context;

use crate::session::Sender;

/// Sends the tree in `dir` on `stream`, or a failure.
pub(crate) fn send(sender: &Sender, stream: u32, dir: &Path) -> io::Result<()> {
    let mut entries = Vec::new();
    if let Err(message) = list(dir, None, &mut entries) {
        return sender.reply(stream, Err(message));
    }
    for (path, entry) in entries {
        let size = match &entry {
            Entry::Dir { .. } => None,
            Entry::File { size, .. } => Some(*size),
        };
        sender.send(&FromSandbox::Entry { stream, entry }, &[])?;
        if let Some(size) = size
            && let Err(message) = send_file(sender, stream, &path, size)?
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

/// Sends the `size` bytes of the file at `path`. The outer error is the session's, the inner
/// one the file's.
fn send_file(
    sender: &Sender,
    stream: u32,
    path: &Path,
    size: u64,
) -> io::Result<Result<(), String>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) => return Ok(Err(format!("{}: {error}", path.display()))),
    };
    let mut file = file.take(size.saturating_add(1));
    let mut buffer = vec![0; MAX_BODY_BYTES];
    let mut sent = 0;
    loop {
        let read = match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Ok(Err(format!("{}: {error}", path.display()))),
        };
        sent += u64::try_from(read).map_err(io::Error::other)?;
        if sent > size {
            break;
        }
        sender.send(&FromSandbox::Data { stream }, &buffer[..read])?;
    }
    if sent == size {
        Ok(Ok(()))
    } else {
        Ok(Err(format!(
            "{} changed while it was collected: freeze the sandbox first",
            path.display()
        )))
    }
}
