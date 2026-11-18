//! Admission to the controller's shared store. A sender owns the partial file's lock while it
//! appends. Admission and collection take the store lock, so a digest names only checked bytes.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Read, Seek, SeekFrom, Write as _};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use schema::ir::Sha256Digest;
use serde::{Deserialize, Serialize};
use stdx::Context as _;

use super::{Entry, INCOMING, Kind, LOCK, MANIFESTS, Manifest, file_sha256};

#[derive(Serialize, Deserialize)]
pub(crate) struct Missing {
    pub(crate) digest: Sha256Digest,
    pub(crate) size_bytes: u64,
    pub(crate) held_bytes: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum PutAnswer {
    Stored,
    NotItsDigest,
    /// A partial was collected or discarded after the client asked what was held.
    OffsetChanged,
}

fn lock(store: &Path) -> Result<File, String> {
    let path = store.join(LOCK);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .context(path.display())?;
    file.lock().context(path.display())?;
    Ok(file)
}

fn partial(store: &Path, digest: &Sha256Digest) -> PathBuf {
    store.join(INCOMING).join(format!("{}.part", digest.hex()))
}

/// Refreshes the manifest before listing its missing blobs, under the lock that a prune takes.
/// The running install can supply a blob of its own manifest without a second upload.
pub(crate) fn lacks(
    store: &Path,
    manifest: &Manifest,
    install: Option<&Path>,
) -> Result<Vec<Missing>, String> {
    let directory = store.join(MANIFESTS);
    let incoming = store.join(INCOMING);
    for dir in [&directory, &incoming] {
        fs::create_dir_all(dir).context(dir.display())?;
    }
    let text = manifest.text();
    let name = super::sha256(&text);
    if let Some(install) = install
        && install.file_name() != Some(name.hex().as_ref())
    {
        return Err(
            "the install manifest differs from this serve's build: link to its own build"
                .to_owned(),
        );
    }
    let path = directory.join(name.hex());
    let mut staged = tempfile::NamedTempFile::new_in(&incoming).context(incoming.display())?;
    staged.write_all(&text).context(path.display())?;
    staged
        .as_file()
        .set_permissions(Permissions::from_mode(0o444))
        .context(path.display())?;
    let _lock = lock(store)?;
    staged.persist(&path).context(path.display())?;
    let mut lacking = BTreeMap::new();
    for entry in manifest.entries() {
        let Some(target) = missing_path(store, entry)? else {
            continue;
        };
        if let Some(install) = install
            && seed(&incoming, install, entry, &target)?
        {
            continue;
        }
        let part = partial(store, &entry.digest);
        let held_bytes = match fs::metadata(&part) {
            Ok(metadata) => metadata.len().min(entry.size_bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error).context(part.display()),
        };
        lacking.insert(
            entry.digest.clone(),
            Missing {
                digest: entry.digest.clone(),
                size_bytes: entry.size_bytes,
                held_bytes,
            },
        );
    }
    Ok(lacking.into_values().collect())
}

fn missing_path(store: &Path, entry: &Entry) -> Result<Option<PathBuf>, String> {
    let blob = store.join(entry.digest.hex());
    if blob.try_exists().context(blob.display())? {
        return Ok(None);
    }
    if entry.kind == Kind::Executable {
        let copy = store.join(format!("{}.x", entry.digest.hex()));
        return Ok((!copy.try_exists().context(copy.display())?).then_some(copy));
    }
    Ok(Some(blob))
}

/// A job may start once its entire tree is available. Renew its manifest under the prune's lock
/// so its blobs stay available until the job makes its hard links.
pub(crate) fn required(store: &Path, tree: &Sha256Digest) -> Result<(), String> {
    let _held = lock(store)?;
    let path = store.join(MANIFESTS).join(tree.hex());
    let file = File::open(&path).context(path.display())?;
    let mut text = Vec::new();
    (&file).read_to_end(&mut text).context(path.display())?;
    for entry in Manifest::parse(&text)?.entries() {
        if missing_path(store, entry)?.is_some() {
            return Err(format!(
                "the controller lacks the blob for `{}`: send the task again",
                entry.path
            ));
        }
    }
    file.set_modified(std::time::SystemTime::now())
        .context(path.display())
}

fn seed(incoming: &Path, install: &Path, entry: &Entry, target: &Path) -> Result<bool, String> {
    let path = install.join(&entry.path);
    let mut file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context(path.display()),
    };
    let mut copy = tempfile::NamedTempFile::new_in(incoming).context(incoming.display())?;
    io::copy(&mut file, copy.as_file_mut()).context(path.display())?;
    if file_sha256(copy.path())? != (entry.digest.clone(), entry.size_bytes) {
        return Ok(false);
    }
    let mode = if entry.kind == Kind::Executable {
        0o555
    } else {
        0o444
    };
    copy.as_file()
        .set_permissions(Permissions::from_mode(mode))
        .context(target.display())?;
    copy.persist(target).context(target.display())?;
    Ok(true)
}

fn still_named(file: &File, path: &Path) -> Result<bool, String> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.ino() == file.metadata().context(path.display())?.ino()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context(path.display()),
    }
}

/// Appends the body starting at `from` and admits the whole blob once its digest agrees.
/// A cut leaves each complete frame for the next sender.
pub(crate) fn put(
    store: &Path,
    digest: &Sha256Digest,
    from: u64,
    size: u64,
    body: &mut impl Read,
) -> Result<PutAnswer, String> {
    let path = partial(store, digest);
    let blob = store.join(digest.hex());
    let mut file = loop {
        let held = lock(store)?;
        if blob.try_exists().context(blob.display())? {
            return Ok(PutAnswer::Stored);
        }
        // A sender killed between chmod and rename leaves a verified, read-only partial.
        if fs::metadata(&path).is_ok_and(|meta| meta.permissions().readonly()) {
            fs::set_permissions(&path, Permissions::from_mode(0o600)).context(path.display())?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .context(path.display())?;
        // An active writer needs the store lock again to admit its file.
        drop(held);
        file.lock().context(path.display())?;
        let _held = lock(store)?;
        // A previous writer may have moved or discarded this inode while we waited.
        if still_named(&file, &path)? {
            break file;
        }
    };
    let held = file.seek(SeekFrom::End(0)).context(path.display())?;
    if held < from {
        return Ok(PutAnswer::OffsetChanged);
    }
    // Another sender may have extended the partial since Lacks. Keep its prefix even if this
    // sender is cut while it repeats bytes.
    let overlap = held.min(size) - from;
    io::copy(&mut body.by_ref().take(overlap), &mut io::sink()).context(path.display())?;
    io::copy(body, &mut file).context(path.display())?;
    file.rewind().context(path.display())?;
    let matches = super::hash_reader(&file).context(path.display())? == (digest.clone(), size);
    let _held = lock(store)?;
    // Older collectors remove stale partials without checking their file lock.
    if !still_named(&file, &path)? {
        return Ok(PutAnswer::OffsetChanged);
    }
    if !matches {
        fs::remove_file(&path).context(path.display())?;
        return Ok(PutAnswer::NotItsDigest);
    }
    file.set_permissions(Permissions::from_mode(0o444))
        .context(path.display())?;
    fs::rename(&path, &blob).context(blob.display())?;
    Ok(PutAnswer::Stored)
}
