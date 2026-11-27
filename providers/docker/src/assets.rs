//! The files of the assets that the provider's sandboxes hold, in a cache on the machine
//! ([`AssetCache`]).

use std::collections::BTreeSet;
use std::fs::{self, DirBuilder, File, OpenOptions, Permissions};
use std::io::{self, Read as _, Write};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use engine::READ_ONLY_TMP;
use provider::{AssetDigest, AssetRef, Assets, PublishedAsset, RunTag};
use schema::ir::{AssetFile, AssetFiles, AssetPath, Sha256Digest};
use sha2::{Digest as _, Sha256};
use stdx::{Context as _, Deadline};
use ureq::tls::{Certificate, RootCerts, TlsConfig};

// Name resolution and the connect, its TLS handshake included, each get this long. So each
// connection may take 60 s to set up. A redirect's connection gets fresh budgets:
// https://github.com/algesten/ureq/blob/33ca9d35e97b6a10e767fb0fa3139f01d315e89c/src/lib.rs#L101-L104
// Cargo gives a download's connect 30 s, which in libcurl covers name resolution too:
// https://github.com/rust-lang/cargo/blob/ecb9851afd3095e988daaa35a48bc7f3cb748e04/src/cargo/util/network/http.rs#L202-L226
// https://github.com/curl/curl/blob/b1ef0e1a01c0bb6ee5367bd9c186a603bde3615a/docs/libcurl/opts/CURLOPT_CONNECTTIMEOUT.md?plain=1#L30-L34
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

// The longest that one read of a response may wait. Cargo waits as long on a download below 10
// bytes a second, at the lines above. A server that sends a byte now and then passes this bound,
// and `FETCH_TIMEOUT` ends its fetch.
const STALL_TIMEOUT: Duration = Duration::from_secs(30);

// It bounds the fetch of all of an asset's files together. It is sized only to end the fetch of a
// server that trickles, as the engine's `PUSH_TIMEOUT` ends a push that hangs. At 10 MiB a
// second, it admits 70 GiB.
const FETCH_TIMEOUT: Duration = Duration::from_hours(2);

// As many as Bazel's downloader follows:
// https://github.com/bazelbuild/bazel/blob/06deebfb5b73f848de5a0ea0e00fcfaa26788d1f/src/main/java/com/google/devtools/build/lib/bazel/repository/downloader/HttpConnector.java#L56
const MAX_REDIRECTS: u32 = 40;

/// The cache, on the machine, of the files of the assets that the provider's sandboxes hold, and
/// the HTTPS client that fetches them.
///
/// Processes may share the cache's directory. A digest's files are fetched once, however many
/// publishes of it run, at once or later. A publish that succeeds returns a directory that holds
/// each of the digest's files, read-only and as pinned.
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
    /// certificate in DER.
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
            // The fetch ignores the proxy that ureq takes by default from `ALL_PROXY`,
            // `HTTPS_PROXY` or `HTTP_PROXY`. So the driver's environment doesn't decide where a
            // run's fetches go.
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
        let dir = to.parent().unwrap_or(staging);
        fs::create_dir_all(dir).context(dir.display())?;
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
        };
        // One byte past its size, to tell a file that is too long.
        let mut body = (response.into_body().into_reader()).take(size.saturating_add(1));
        let fetched = io::copy(&mut body, &mut hashed).map_err(|error| failed(&error))?;
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
        let found = Sha256Digest::from(<[u8; 32]>::from(hashed.sha256.finalize()));
        if found != *sha256 {
            return Err(format!(
                "`{path}` from {url} has the SHA-256 {found}, not the pinned {sha256}: give the \
                 SHA-256 of the file that the URL serves"
            ));
        }
        // So that a crash after the rename to the entry leaves no file short.
        written.sync_all().context(to.display())?;
        // Read-only, as the mount is: a second guard, should the mount's flag ever fail, against a
        // sandbox's user that isn't the file's owner.
        let read_only = fs::set_permissions(&to, Permissions::from_mode(0o444));
        read_only.context(to.display())
    }
}

impl Assets for AssetCache {
    fn holds(&self, path: &AssetPath) -> Result<(), String> {
        if path.as_str() == READ_ONLY_TMP {
            return Err(format!(
                "each sandbox has an empty read-only tmpfs at `{READ_ONLY_TMP}`: hold the asset at \
                 another path, such as one below `{READ_ONLY_TMP}`"
            ));
        }
        Ok(())
    }

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
        // The wait for the lock has no bound, as Go's module cache waits on its own lock:
        // https://github.com/golang/go/blob/46b62495e1bf4d56e0313320772ae588d9119aed/src/cmd/go/internal/modfetch/fetch.go#L183
        // https://github.com/golang/go/blob/46b62495e1bf4d56e0313320772ae588d9119aed/src/cmd/go/internal/modfetch/cache.go#L149-L158
        // The holder's downloads end within `FETCH_TIMEOUT`. `File::lock` is `flock(2)`, whose lock
        // ends once its descriptor and every copy of it are closed:
        // https://github.com/rust-lang/rust/blob/58cf8f9c35b355199727627d07ae34a4e2846d67/library/std/src/fs.rs#L825-L830
        // The kernel closes a process's descriptors as it ends, however it ends. Rust opens each
        // file close-on-exec, so no child process holds a copy:
        // https://github.com/rust-lang/rust/blob/58cf8f9c35b355199727627d07ae34a4e2846d67/library/std/src/sys/fs/unix.rs#L1233
        let lock = self.dir.join(format!(".{}.lock", digest.hex()));
        let lock = (OpenOptions::new().create(true).truncate(false).write(true))
            .mode(0o600)
            .open(&lock)
            .and_then(|file| file.lock().map(|()| file))
            .context(lock.display())?;
        // A publish that held the lock first may have made the entry while this one waited. Only
        // a fetch that checked each file renames its staging directory to the entry. Go checks
        // again after its wait too:
        // https://github.com/golang/go/blob/46b62495e1bf4d56e0313320772ae588d9119aed/src/cmd/go/internal/modfetch/fetch.go#L192-L197
        if fs::exists(&entry).context(entry.display())? {
            return Ok(PublishedAsset {
                reference,
                fetched: false,
            });
        }
        let staging = Staging {
            dir: self.dir.join(format!(".{}.part", digest.hex())),
            _lock: lock,
        };
        // Only the lock's holder writes a staging directory. So one that the holder finds was left
        // by a fetch that died, or whose removal of the directory failed. Go removes a dead
        // writer's partial directories alike:
        // https://github.com/golang/go/blob/46b62495e1bf4d56e0313320772ae588d9119aed/src/cmd/go/internal/modfetch/fetch.go#L218-L234
        match remove(&staging.dir) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            removed => removed.context(format_args!("removing {}", staging.dir.display()))?,
        }
        let by = Deadline::after(FETCH_TIMEOUT);
        let mut dirs = BTreeSet::new();
        for file in files.as_slice() {
            self.fetch(&staging.dir, file, by)?;
            let below = Path::new(file.path.as_str()).ancestors().skip(1);
            dirs.extend(below.map(|dir| staging.dir.join(dir)));
        }
        // Each directory turns read-only, as each file does, and is synced before the rename. So a
        // crash after the rename leaves no file out of the entry. The loop's order doesn't matter.
        for dir in &dirs {
            let read_only = fs::set_permissions(dir, Permissions::from_mode(0o555));
            read_only.context(dir.display())?;
            sync(dir)?;
        }
        // Bazel's repository cache moves a download into place alike:
        // https://github.com/bazelbuild/bazel/blob/06deebfb5b73f848de5a0ea0e00fcfaa26788d1f/src/main/java/com/google/devtools/build/lib/bazel/repository/cache/RepositoryCache.java#L231-L236
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

/// Syncs the directory at `dir`, so that the entries made or renamed in it survive a crash.
fn sync(dir: &Path) -> Result<(), String> {
    let synced = File::open(dir).and_then(|dir| dir.sync_all());
    synced.context(format_args!("syncing {}", dir.display()))
}

/// A writer that hashes what it writes `to`.
struct Hashed<'f> {
    to: &'f mut File,
    sha256: Sha256,
}

impl Write for Hashed<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.to.write(bytes)?;
        self.sha256.update(&bytes[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.to.flush()
    }
}
