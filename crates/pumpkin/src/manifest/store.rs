//! Admission to the controller's shared store. A sender owns the partial file's lock while it
//! appends. Admission and collection take the store lock, so a digest names only checked bytes.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions, Permissions, TryLockError};
use std::io::{self, Read, Seek, Write as _};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use schema::ir::Sha256Digest;
use serde::{Deserialize, Serialize};
use stdx::Context as _;

use super::{Entry, INCOMING, Kind, LOCK, MANIFESTS, Manifest, file_sha256};

/// A `ByteStream` upload UUID, reused across digests and across reconnections of one send.
// https://github.com/bazelbuild/remote-apis/blob/c2c07e3d718d3d490a4958344d9c3e47de0bfaf3/build/bazel/remote/execution/v2/remote_execution.proto#L232-L234
#[derive(Clone, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub(crate) struct UploadId(String);

impl UploadId {
    pub(crate) fn new() -> Result<Self, String> {
        let path = "/proc/sys/kernel/random/uuid";
        fs::read_to_string(path)
            .context(path)?
            .trim_end()
            .to_owned()
            .try_into()
    }
}

impl TryFrom<String> for UploadId {
    type Error = String;

    fn try_from(mut id: String) -> Result<Self, String> {
        id.make_ascii_lowercase();
        if id.len() == 36
            && id.bytes().enumerate().all(|(i, b)| match i {
                8 | 13 | 18 | 23 => b == b'-',
                14 => b == b'4',
                19 => matches!(b, b'8' | b'9' | b'a' | b'b'),
                _ => b.is_ascii_hexdigit(),
            })
        {
            Ok(Self(id))
        } else {
            Err("the upload id is not a version 4 UUID".to_owned())
        }
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Missing {
    pub(crate) digest: Sha256Digest,
    pub(crate) size_bytes: u64,
    pub(crate) held_bytes: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum PutAnswer {
    Stored {
        committed_size: u64,
    },
    /// The reply ends the link while part of the declared body may still be in flight.
    StoredEarly {
        committed_size: u64,
    },
    NotItsDigest,
    /// The client must ask again which prefix is held before resuming.
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

fn partial(store: &Path, upload: &UploadId, digest: &Sha256Digest, size: u64) -> PathBuf {
    store
        .join(INCOMING)
        .join(format!("{}.{size}.{}.part", digest.hex(), upload.0))
}

/// Refreshes the manifest before listing its missing blobs, under the lock that a prune takes.
/// The running install can supply a blob of its own manifest without a second upload.
pub(crate) fn lacks(
    store: &Path,
    manifest: &Manifest,
    upload: &UploadId,
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
        let part = partial(store, upload, &entry.digest, entry.size_bytes);
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
    if stored_size(&blob, entry.size_bytes).context(blob.display())? {
        return Ok(None);
    }
    // The REAPI empty blob is always available. A job still needs an inode to link into its tree.
    if entry.size_bytes == 0 && entry.digest == super::sha256(&[]) {
        let incoming = store.join(INCOMING);
        let empty = tempfile::NamedTempFile::new_in(&incoming).context(incoming.display())?;
        empty
            .as_file()
            .set_permissions(Permissions::from_mode(0o444))
            .context(blob.display())?;
        empty.persist(&blob).context(blob.display())?;
        return Ok(None);
    }
    if entry.kind == Kind::Executable {
        let copy = store.join(format!("{}.x", entry.digest.hex()));
        return Ok(
            (!stored_size(&copy, entry.size_bytes).context(copy.display())?).then_some(copy),
        );
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
/// A cut leaves each complete frame for the next write of this upload.
pub(crate) fn put(
    store: &Path,
    upload: &UploadId,
    digest: &Sha256Digest,
    from: u64,
    size: u64,
    body: &mut impl Read,
) -> Result<PutAnswer, String> {
    let path = partial(store, upload, digest, size);
    let blob = store.join(digest.hex());
    let mut file = {
        let _held = lock(store)?;
        if stored_size(&blob, size).context(blob.display())? {
            return Ok(PutAnswer::Stored {
                committed_size: size,
            });
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
        match file.try_lock() {
            Ok(()) => file,
            Err(TryLockError::Error(error)) => return Err(error).context(path.display()),
            Err(TryLockError::WouldBlock) => return Ok(PutAnswer::OffsetChanged),
        }
    };
    if file.seek(io::SeekFrom::End(0)).context(path.display())? != from {
        return Ok(PutAnswer::OffsetChanged);
    }
    let written = io::copy(body, &mut file)
        .and_then(|_| file.rewind())
        .and_then(|()| hash_upload(&file, &blob, size));
    let _held = lock(store)?;
    if stored_size(&blob, size).context(blob.display())? {
        if still_named(&file, &path)? {
            fs::remove_file(&path).context(path.display())?;
        }
        return Ok(PutAnswer::Stored {
            committed_size: size,
        });
    }
    let actual = written.context(path.display())?;
    // Older collectors remove stale partials without checking their file lock.
    if !still_named(&file, &path)? {
        return Ok(PutAnswer::OffsetChanged);
    }
    // The peer's admitted blob was collected before we took the store lock.
    if actual.1 < size {
        return Ok(PutAnswer::OffsetChanged);
    }
    if actual != (digest.clone(), size) {
        fs::remove_file(&path).context(path.display())?;
        return Ok(PutAnswer::NotItsDigest);
    }
    file.set_permissions(Permissions::from_mode(0o444))
        .context(path.display())?;
    fs::rename(&path, &blob).context(blob.display())?;
    Ok(PutAnswer::Stored {
        committed_size: size,
    })
}

fn hash_upload(file: &File, blob: &Path, size: u64) -> io::Result<(Sha256Digest, u64)> {
    super::digesting(|hash| {
        // A peer may publish while this sender checks even a fully resumed partial.
        let block = schema::frame::MAX_BODY_BYTES as u64;
        let mut total = 0;
        loop {
            let read = io::copy(&mut file.take(block), hash)?;
            total += read;
            if read < block || stored_size(blob, size)? {
                return Ok(total);
            }
        }
    })
}

/// Whether the published blob exists with the size its digest names.
pub(crate) fn stored_size(blob: &Path, size: u64) -> io::Result<bool> {
    match fs::metadata(blob) {
        Ok(metadata) if metadata.len() == size => Ok(true),
        Ok(metadata) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "the blob holds {} bytes: expected {size} for this digest",
                metadata.len()
            ),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}
