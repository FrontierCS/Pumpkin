//! The files of the assets that the provider's sandboxes hold, in a cache on the machine
//! ([`AssetCache`]).

use std::collections::BTreeSet;
use std::fs::{self, DirBuilder, File, OpenOptions, Permissions};
use std::io::{self, Read as _, Write};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use provider::{AssetDigest, AssetRef, Assets, PublishedAsset, RunTag};
use schema::ir::{AssetFile, AssetFiles, Sha256Digest};
use sha2::{Digest as _, Sha256};
use stdx::{Context as _, Deadline};
use ureq::tls::{Certificate, RootCerts, TlsConfig};

/// How long a fetch may take to resolve a server's name, and then to connect to it, its TLS
/// handshake included.
// As long as cargo gives a download's connect:
// https://github.com/rust-lang/cargo/blob/ecb9851afd3095e988daaa35a48bc7f3cb748e04/src/cargo/util/network/http.rs#L202-L226
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a fetch may wait on any one read of its response.
// As long as cargo lets a download stall, below 10 bytes a second, at the lines above.
const STALL_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a fetch of an asset's files may take in all.
// Sized only to turn a server that trickles into a failure, as the engine's push timeout is. The
// vLLM task's 57 GiB take 8 minutes at 128 MiB a second, and 97 at 10.
const FETCH_TIMEOUT: Duration = Duration::from_hours(2);

/// The most redirects that a fetch follows.
// As many as Bazel's downloader follows:
// https://github.com/bazelbuild/bazel/blob/06deebfb5b73f848de5a0ea0e00fcfaa26788d1f/src/main/java/com/google/devtools/build/lib/bazel/repository/downloader/HttpConnector.java#L56
const MAX_REDIRECTS: u32 = 40;

/// The cache of the files of the assets that the provider's sandboxes hold, on the machine, and
/// the servers that it fetches them from.
///
/// It fetches each digest's files once, into an entry named by the digest, which each sandbox that
/// holds them mounts read-only. A fetch holds the digest's lock, which a fetch in any process waits
/// for. It fills the digest's staging directory, then makes it read-only. It renames that to the
/// entry once each file has the size and SHA-256 that its asset pins. So no fetch finds an entry
/// half made. A fetch that fails removes its directory. A later fetch of the digest removes one
/// that a killed fetch left.
// Bazel's repository cache moves a download into place alike:
// https://github.com/bazelbuild/bazel/blob/06deebfb5b73f848de5a0ea0e00fcfaa26788d1f/src/main/java/com/google/devtools/build/lib/bazel/repository/cache/RepositoryCache.java#L231-L236
#[derive(Debug)]
pub struct AssetCache {
    dir: PathBuf,
    agent: ureq::Agent,
}

impl AssetCache {
    /// The cache in `dir`, made once a publish needs it. It fetches each file from a server that
    /// Web PKI's roots vouch for, as a browser trusts one.
    pub fn new(dir: &Path) -> Self {
        Self::of(dir, RootCerts::WebPki)
    }

    /// The cache in `dir`, as [`new`](Self::new) makes it, but trusting only `root`, a
    /// certificate in DER, such as a stand-in's.
    pub fn trusting(dir: &Path, root: &[u8]) -> Self {
        let root = Certificate::from_der(root).to_owned();
        Self::of(dir, RootCerts::new_with_certs(&[root]))
    }

    fn of(dir: &Path, roots: RootCerts) -> Self {
        let agent = ureq::Agent::config_builder()
            .tls_config(TlsConfig::builder().root_certs(roots).build())
            // A redirect may lead to another server, but never off `https`.
            .https_only(true)
            .max_redirects(MAX_REDIRECTS)
            // Runs are hermetic: otherwise a proxy that the environment names would carry each
            // fetch.
            .proxy(None)
            .timeout_resolve(Some(CONNECT_TIMEOUT))
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_per_read(Some(STALL_TIMEOUT))
            .build()
            .into();
        Self {
            dir: dir.to_owned(),
            agent,
        }
    }

    /// Fetches `file` into `staging`, at its path, by `by`, and fails unless its URL gives exactly
    /// its size and its SHA-256.
    fn fetch(&self, staging: &Path, file: &AssetFile, by: Deadline) -> Result<(), String> {
        let AssetFile {
            path,
            url,
            size,
            sha256,
        } = file;
        let to = staging.join(path.as_str());
        let mut dir = staging.to_owned();
        let parent = Path::new(path.as_str()).parent();
        for component in parent.into_iter().flat_map(Path::components) {
            dir.push(component);
            if !dir.is_dir() {
                fs::create_dir(&dir).context(dir.display())?;
                // Whatever the umask, so that a sandbox's user reads below it.
                let readable = fs::set_permissions(&dir, Permissions::from_mode(0o755));
                readable.context(dir.display())?;
            }
        }
        let failed =
            |error: &dyn std::fmt::Display| format!("fetching `{path}` from {url} failed: {error}");
        let Some(left) = by.remaining() else {
            return Err(failed(&format_args!(
                "the fetch took over {FETCH_TIMEOUT:?}"
            )));
        };
        let call = self.agent.get(url.as_str()).config();
        let call = call.timeout_global(Some(left)).build();
        let response = call.call().map_err(|error| failed(&error))?;
        let mut written = File::create_new(&to).context(to.display())?;
        let mut hashed = Hashed {
            to: &mut written,
            sha256: Sha256::new(),
            size: 0,
        };
        // One byte past its size, to tell a file that is too long.
        let mut body = (response.into_body().into_reader()).take(size.saturating_add(1));
        io::copy(&mut body, &mut hashed).map_err(|error| failed(&error))?;
        let Hashed {
            to: _,
            sha256: found,
            size: fetched,
        } = hashed;
        if fetched != *size {
            let held = if fetched > *size {
                format!("holds more than its {size} bytes")
            } else {
                format!("ends after {fetched} of its {size} bytes")
            };
            return Err(format!(
                "`{path}` from {url} {held}: give the size of the file that the URL serves"
            ));
        }
        let found = Sha256Digest::from(<[u8; 32]>::from(found.finalize()));
        if found != *sha256 {
            return Err(format!(
                "`{path}` from {url} has the SHA-256 {found}, not the pinned {sha256}: give the \
                 SHA-256 of the file that the URL serves"
            ));
        }
        // So that a crash once the entry is named leaves none of its files short.
        written.sync_all().context(to.display())?;
        // Read-only, as the mount is: a second guard, should the mount's flag ever fail, against a
        // sandbox's user that isn't the file's owner.
        let read_only = fs::set_permissions(&to, Permissions::from_mode(0o444));
        read_only.context(to.display())
    }
}

impl Assets for AssetCache {
    // It makes no sandbox, so it has nothing to tag with the run.
    fn publish_asset(
        &self,
        _run: &RunTag,
        digest: &AssetDigest,
        files: &AssetFiles,
    ) -> Result<PublishedAsset, String> {
        let entry = self.dir.join(digest.hex());
        let Some(reference) = entry.to_str().map(|entry| AssetRef::from(entry.to_owned())) else {
            return Err(format!(
                "{} isn't UTF-8, as the engine's API takes a path: keep the workspace at a path \
                 of UTF-8",
                entry.display()
            ));
        };
        let made = DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(&self.dir);
        made.context(self.dir.display())?;
        let lock = self.dir.join(format!(".{}.lock", digest.hex()));
        let lock = (OpenOptions::new().create(true).truncate(false).write(true))
            .mode(0o600)
            .open(&lock)
            .and_then(|file| file.lock().map(|()| file))
            .context(lock.display())?;
        // Only a fetch that checked each file names an entry.
        if fs::exists(&entry).context(entry.display())? {
            return Ok(PublishedAsset {
                reference,
                fetched: false,
            });
        }
        // The wait for the lock is unbounded, as Go's module cache waits for its lockfile. Its
        // holder's fetch is bounded by `FETCH_TIMEOUT`, and the kernel releases the lock of a
        // killed holder. So a staging directory that the holder finds is a dead fetch's.
        // https://github.com/golang/go/blob/46b62495e1bf4d56e0313320772ae588d9119aed/src/cmd/go/internal/modfetch/fetch.go#L218-L234
        let staging = Staging {
            dir: self.dir.join(format!(".{}.part", digest.hex())),
            _lock: lock,
        };
        match remove(&staging.dir) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            removed => removed.context(format_args!("removing {}", staging.dir.display()))?,
        }
        fs::create_dir(&staging.dir).context(staging.dir.display())?;
        let by = Deadline::after(FETCH_TIMEOUT);
        let mut dirs = BTreeSet::new();
        for file in files.as_slice() {
            self.fetch(&staging.dir, file, by)?;
            let below = Path::new(file.path.as_str()).ancestors().skip(1);
            dirs.extend(below.map(|dir| staging.dir.join(dir)));
        }
        // Deepest first, so that a crash once the entry is named leaves none of its files out.
        for dir in dirs.iter().rev() {
            let read_only = fs::set_permissions(dir, Permissions::from_mode(0o555));
            read_only.context(dir.display())?;
            sync(dir)?;
        }
        let renamed = fs::rename(&staging.dir, &entry);
        renamed.context(format_args!(
            "renaming {} to {}",
            staging.dir.display(),
            entry.display()
        ))?;
        sync(&self.dir)?;
        Ok(PublishedAsset {
            reference,
            fetched: true,
        })
    }
}

/// A fetch's directory, and the digest's lock. It removes the directory as it drops, unless the
/// fetch renamed it, and only then releases the lock.
struct Staging {
    dir: PathBuf,
    _lock: File,
}

impl Drop for Staging {
    fn drop(&mut self) {
        // Gone once renamed. A directory left by a failed removal goes at the next fetch.
        let _ = remove(&self.dir);
    }
}

/// Removes the tree at `root`, though its directories may be read-only, as a fetch leaves them.
fn remove(root: &Path) -> io::Result<()> {
    let mut dirs = Vec::from([root.to_owned()]);
    while let Some(dir) = dirs.pop() {
        fs::set_permissions(&dir, Permissions::from_mode(0o755))?;
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                dirs.push(entry.path());
            }
        }
    }
    fs::remove_dir_all(root)
}

/// Syncs the directory at `dir`, so that the entries made or renamed in it last.
fn sync(dir: &Path) -> Result<(), String> {
    let synced = File::open(dir).and_then(|dir| dir.sync_all());
    synced.context(format_args!("syncing {}", dir.display()))
}

/// A writer that hashes what it writes `to`, and counts its bytes.
struct Hashed<'f> {
    to: &'f mut File,
    sha256: Sha256,
    size: u64,
}

impl Write for Hashed<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.to.write(bytes)?;
        self.sha256.update(&bytes[..written]);
        self.size += u64::try_from(written).unwrap_or(u64::MAX);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.to.flush()
    }
}
