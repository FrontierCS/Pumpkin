//! The assets that the provider's sandboxes hold. Each is an image of Modal's that a fill made. A
//! fill's sandbox fetches an asset's files, checks each one's size and SHA-256, and snapshots them.
//!
//! Each machine fills its own images, rather than sharing one by a name on Modal. Anyone with the
//! workspace's token could point such a name at an image of other files, and no create checks the
//! files it mounts. The machine records each image's id ([`Fills`]), and reuses it while Modal
//! holds the image. It fills a digest once, however many runs publish it at once.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use provider::{AssetDigest, AssetRef, Assets, PublishedAsset, RunTag};
use schema::ir::{AssetFiles, AssetPath, Sha256Digest};
use sha2::{Digest as _, Sha256};
use stdx::{Context as _, Deadline};

use crate::Modal;
use crate::create::{self, FETCH_TIMEOUT, Filled, Filling, SNAPSHOT_TIMEOUT};
use crate::helper::{ASK_TIMEOUT, Request};
use crate::images::ImageId;

/// How long Modal lets a fill's sandbox run. One `ASK_TIMEOUT` covers its steps besides the fetch
/// and the snapshot.
const FILL_TIMEOUT: Duration = FETCH_TIMEOUT
    .saturating_add(SNAPSHOT_TIMEOUT)
    .saturating_add(ASK_TIMEOUT);

/// How long the helper's `fill` may run before it is killed. One `ASK_TIMEOUT` covers its steps
/// outside the sandbox's run.
const CALL_TIMEOUT: Duration = FILL_TIMEOUT.saturating_add(ASK_TIMEOUT);

/// The ids of the images that this machine's fills made. Only the machine's user reaches them.
#[derive(Debug)]
pub(crate) struct Fills {
    dir: PathBuf,
}

impl Fills {
    pub(crate) fn in_dir(dir: &Path) -> Self {
        Self {
            dir: dir.to_owned(),
        }
    }

    /// The record of the fill of `digest` that `how` makes, once no other publish on the machine
    /// holds it.
    // The record's name holds `how` too, so a publish fills again once `how` changes. Bazel
    // likewise counts a fetched repository out of date once its rule's fingerprint differs from the
    // one in its marker file:
    // https://github.com/bazelbuild/bazel/blob/06deebfb5b73f848de5a0ea0e00fcfaa26788d1f/src/main/java/com/google/devtools/build/lib/rules/repository/RepositoryDelegatorFunction.java#L563-L588
    // https://github.com/bazelbuild/bazel/blob/06deebfb5b73f848de5a0ea0e00fcfaa26788d1f/src/main/java/com/google/devtools/build/lib/rules/repository/RepositoryDelegatorFunction.java#L622-L627
    // The wait for the lock has no bound, as Go's module cache waits for its lock:
    // https://github.com/golang/go/blob/46b62495e1bf4d56e0313320772ae588d9119aed/src/cmd/go/internal/modfetch/fetch.go#L183
    // https://github.com/golang/go/blob/46b62495e1bf4d56e0313320772ae588d9119aed/src/cmd/go/internal/modfetch/cache.go#L149-L158
    // A holder lets go within a `held` call and a fill. The kernel releases its `flock` once its
    // process ends, however it ends.
    fn lock(&self, digest: &AssetDigest, how: &Filling) -> Result<Record<'_>, String> {
        let how = serde_json::to_vec(how)
            .map_err(|error| format!("writing a fill's request as JSON failed: {error}"))?;
        let how = Sha256Digest::from(<[u8; 32]>::from(Sha256::digest(&how)));
        let name = format!("{}-{}", digest.hex(), how.hex());
        let made = DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(&self.dir);
        made.context(self.dir.display())?;
        let lock = self.dir.join(format!(".{name}.lock"));
        let lock = (OpenOptions::new().create(true).truncate(false).write(true))
            .mode(0o600)
            .open(&lock)
            .and_then(|file| file.lock().map(|()| file))
            .context(lock.display())?;
        Ok(Record {
            dir: &self.dir,
            name,
            _lock: lock,
        })
    }
}

/// The record of a fill, which its holder alone reads and writes until it drops.
struct Record<'f> {
    dir: &'f Path,
    name: String,
    _lock: File,
}

impl Record<'_> {
    /// The id that the fill recorded, if any. A record whose text is no image id counts as none.
    fn image(&self) -> Result<Option<ImageId>, String> {
        let path = self.dir.join(&self.name);
        match fs::read_to_string(&path) {
            Ok(text) => Ok(ImageId::try_from(text).ok()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("reading {} failed: {error}", path.display())),
        }
    }

    // If the process dies, the rename leaves the earlier id or this one whole. Nothing syncs the
    // record, so a machine's crash can leave it empty or missing, which `image` counts as none.
    fn record(&self, image: &ImageId) -> Result<(), String> {
        let part = self.dir.join(format!(".{}.part", self.name));
        fs::write(&part, image.as_str()).context(part.display())?;
        let path = self.dir.join(&self.name);
        fs::rename(&part, &path).context(format_args!("recording {}", path.display()))
    }
}

impl Assets for Modal<'_> {
    // A path where the role's image has a file passes, as this isn't given the image. The create's
    // mount refuses it.
    fn holds(&self, path: &AssetPath) -> Result<(), String> {
        create::asset_held(path)
    }

    /// Publishes `files` as [`Assets::publish_asset`] asks, once any other publish of `digest` on
    /// the machine has ended. A fill that fails keeps nothing.
    fn publish_asset(
        &self,
        run: &RunTag,
        digest: &AssetDigest,
        files: &AssetFiles,
    ) -> Result<PublishedAsset, String> {
        let helper = &self.reached.helper;
        let fill = create::fill(run, files, FILL_TIMEOUT);
        let record = self.fills.lock(digest, &fill.how)?;
        if let Some(image) = record.image()? {
            let request = Request::Held {
                image: image.as_str(),
            };
            if helper.call::<bool>(&request, Deadline::after(ASK_TIMEOUT))? {
                return Ok(PublishedAsset {
                    reference: AssetRef::from(image),
                    fetched: false,
                });
            }
        }
        let expected = format!(
            "expected the fill to end within {} s: a server that serves slowly can't finish its \
             fetch within {} s, so run it again later",
            CALL_TIMEOUT.as_secs(),
            FETCH_TIMEOUT.as_secs()
        );
        let request = Request::Fill(fill);
        let filled = helper.call_expecting(&request, Deadline::after(CALL_TIMEOUT), &expected);
        let Filled { id } = filled.map_err(|error| {
            format!("{error}; a fill that fails keeps nothing, so a run again fills it anew")
        })?;
        record.record(&id)?;
        Ok(PublishedAsset {
            reference: AssetRef::from(id),
            fetched: true,
        })
    }
}
