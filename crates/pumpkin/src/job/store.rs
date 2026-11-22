//! The store of the files that clients sent, as `crate::manifest` lays it out, and its prune.
//!
//! `retention::store` decides which of its files go. This module observes the store and removes
//! them.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use stdx::Context as _;

use crate::manifest::{self, INCOMING, MANIFESTS};
use crate::say;

/// Removes the files of `store` that `retention::store` picks, under the store's lock. They are
/// the manifests and the partial blobs in `incoming/` that expired, and the blobs that no tree
/// links and no manifest names.
///
/// It leaves a partial blob that an upload holds. It says why it keeps every blob when a manifest
/// can't be read.
pub(super) fn collect(store: &Path) -> Result<(), String> {
    let now = SystemTime::now();
    let expired = |path: &Path, metadata: &fs::Metadata| {
        let modified = metadata.modified().context(path.display())?;
        Ok::<_, String>(retention::store::expired(modified, now))
    };
    let path = store.join(manifest::LOCK);
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path);
    let lock = match lock {
        Ok(lock) => lock,
        // No store yet, as no client has sent this controller anything.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context(path.display()),
    };
    lock.lock().context(path.display())?;
    let mut manifests = Vec::new();
    for (path, metadata) in files(&store.join(MANIFESTS))? {
        if expired(&path, &metadata)? {
            removed(&path)?;
            continue;
        }
        let referenced = manifest::referenced(&fs::read(&path).context(path.display())?);
        manifests.push(referenced.map_err(|error| format!("{}: {error}", path.display())));
    }
    for (path, metadata) in files(&store.join(INCOMING))? {
        if expired(&path, &metadata)? {
            let file = match File::open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error).context(path.display()),
            };
            match file.try_lock() {
                // A live upload may have waited for a frame since before the cutoff.
                Err(fs::TryLockError::WouldBlock) => {}
                Err(fs::TryLockError::Error(error)) => return Err(error).context(path.display()),
                Ok(()) => removed(&path)?,
            }
        }
    }
    let named = match retention::store::named(manifests) {
        Ok(named) => named,
        Err(unread) => {
            say(format_args!(
                "{unread}; so the prune keeps every blob of the store, which it may name"
            ));
            return Ok(());
        }
    };
    let mut blobs = Vec::new();
    for (path, metadata) in files(store)? {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        // A blob, or its copy at 0555; not the store's lock.
        let Some(digest) = manifest::digest(name.strip_suffix(".x").unwrap_or(name)) else {
            continue;
        };
        let links_count = metadata.nlink();
        blobs.push(retention::store::Blob {
            path,
            digest,
            links_count,
        });
    }
    for blob in retention::store::unlinked(&blobs, &named) {
        removed(&blob.path)?;
    }
    Ok(())
}

fn files(dir: &Path) -> Result<Vec<(PathBuf, fs::Metadata)>, String> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("{}: {error}", dir.display())),
    };
    let mut files = Vec::new();
    for entry in entries {
        let path = entry.context(dir.display())?.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            // Moved since it was listed, as a start moves a copy to run out of `incoming/`.
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context(path.display()),
        };
        if metadata.is_file() {
            files.push((path, metadata));
        }
    }
    Ok(files)
}

/// Removes the file `path`, unless it is gone, as a start may move it at once.
fn removed(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error).context(path.display()),
        _ => Ok(()),
    }
}
