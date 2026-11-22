//! The store of the files that clients sent.
//!
//! A manifest or a partial blob of an upload lasts `STORE_KEPT` after it was last written. A blob
//! lasts while a tree links it or a manifest that is left names it.
// A blob lasts by reference, as Nix collects its store from its roots. A fresh manifest is such a
// root, as Nix's temporary roots are for a build under way:
// https://github.com/NixOS/nix/blob/d1ecb78bf7a1c5604726bd72651bd2169c14163d/src/libstore/store-api.cc#L1028

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

// How long a manifest or a partial blob lasts in the store. It is also how long the files that a
// manifest names last, if no tree links them. A day is long enough for a send that was cut off, or
// a start that failed, to carry on the next day from what had arrived. It is short enough that
// files no tree links, as of an edit made and undone, are gone soon. A tree, which a run's record
// names, keeps its files for `KEPT` by its links.
const STORE_KEPT: Duration = Duration::from_hours(24);

/// Whether a manifest or a partial blob, last written at `written`, is older than `STORE_KEPT` at
/// `now`.
///
/// A prune removes such a file. This takes no account of an upload that holds a partial blob.
pub fn expired(written: SystemTime, now: SystemTime) -> bool {
    crate::past(written, STORE_KEPT, now)
}

/// A blob of the store, `<sha256>`, or its copy at 0555, `<sha256>.x`.
#[derive(Clone, Debug)]
pub struct Blob<D> {
    pub path: PathBuf,
    pub digest: D,
    /// How many hard links it has, the store's own one of them.
    pub links_count: u64,
}

/// The digests that the manifests name, from the result of reading each one.
///
/// Fails with the error of the last manifest that couldn't be read. Such a manifest may name any
/// blob, so no set of digests is complete.
pub fn named<D: Ord, E>(
    manifests: impl IntoIterator<Item = Result<impl IntoIterator<Item = D>, E>>,
) -> Result<BTreeSet<D>, E> {
    let mut named = BTreeSet::new();
    let mut unread = None;
    for manifest in manifests {
        match manifest {
            Ok(digests) => named.extend(digests),
            Err(error) => unread = Some(error),
        }
    }
    match unread {
        Some(error) => Err(error),
        None => Ok(named),
    }
}

/// The blobs in `blobs` that a prune removes, in their order.
///
/// Each has only the store's own link, so no tree links it, and `named` lacks its digest.
pub fn unlinked<'b, D: Ord>(blobs: &'b [Blob<D>], named: &BTreeSet<D>) -> Vec<&'b Blob<D>> {
    (blobs.iter())
        .filter(|blob| blob.links_count == 1 && !named.contains(&blob.digest))
        .collect()
}
