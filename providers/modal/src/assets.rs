//! The assets that the provider's sandboxes hold: for each digest, an image of Modal's that a fill
//! made. Each sandbox of a role that holds the asset mounts it.
//!
//! The machine records the image's id by the digest and by how the fill made it ([`Fills`]). A
//! later publish mounts that image while Modal holds it. Nothing on Modal names the image, as any
//! holder of the workspace's token can point a name at another image. A publish holds its record's
//! lock while it finds or fills the image. So a machine fills a digest once, however many runs
//! publish it at once.

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

/// How long a fill's sandbox lasts at most, at which Modal ends it.
// Its fetch and its snapshot, and a call's bound for its create, its mount and Python's start.
const FILL_TIMEOUT: Duration = FETCH_TIMEOUT
    .saturating_add(SNAPSHOT_TIMEOUT)
    .saturating_add(ASK_TIMEOUT);

/// How long the helper's `fill` may run before it is killed: its sandbox's bound, and a call's
/// bound for the calls before and after it.
const CALL_TIMEOUT: Duration = FILL_TIMEOUT.saturating_add(ASK_TIMEOUT);

/// The ids of the images that this machine's fills made, each in a file named by its digest and by
/// how the fill made it. Only the machine's user reaches their directory.
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

    /// The record of the fill of `digest` that `how` makes, once no other process of the machine
    /// holds it.
    // Named by how too, so that a changed fill fills again. Bazel refetches a repository whose rule
    // changed, while its cache of downloads stays keyed by their content:
    // https://github.com/bazelbuild/bazel/blob/06deebfb5b73f848de5a0ea0e00fcfaa26788d1f/src/main/java/com/google/devtools/build/lib/rules/repository/RepositoryDelegatorFunction.java#L622-L627
    // The wait for the lock is unbounded, as Go's module cache waits for its lockfile. Its holder's
    // publish is bounded by its fill's, and the kernel releases the lock of a killed holder.
    // https://github.com/golang/go/blob/46b62495e1bf4d56e0313320772ae588d9119aed/src/cmd/go/internal/modfetch/fetch.go#L218-L234
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
    // Held by owning it, and let go as it drops.
    _lock: File,
}

impl Record<'_> {
    /// The id that the fill recorded, if any. A record that holds no id counts as none, as only a
    /// fill writes one.
    fn image(&self) -> Result<Option<ImageId>, String> {
        let path = self.dir.join(&self.name);
        match fs::read_to_string(&path) {
            Ok(text) => Ok(ImageId::try_from(text).ok()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("reading {} failed: {error}", path.display())),
        }
    }

    /// Records `image` as the fill's, in place of any earlier one. A crash leaves the one or the
    /// other whole.
    fn record(&self, image: &ImageId) -> Result<(), String> {
        let part = self.dir.join(format!(".{}.part", self.name));
        fs::write(&part, image.as_str()).context(part.display())?;
        let path = self.dir.join(&self.name);
        fs::rename(&part, &path).context(format_args!("recording {}", path.display()))
    }
}

impl Assets for Modal<'_> {
    // Modal mounts an image anywhere else, though one where its role's image has a file fails.
    fn holds(&self, path: &AssetPath) -> Result<(), String> {
        create::asset_held(path)
    }

    /// Publishes `files` as [`Assets::publish_asset`] asks. The reference is the id of the image
    /// that each create mounts. It first waits for any other publish of `digest` on the machine,
    /// which ends within its fill's bound. A fill that fails keeps nothing, so a later publish
    /// fills the image anew.
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
