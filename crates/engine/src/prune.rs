//! Which of an engine's images a prune removes, so that they stay within its disk, and when the
//! disk is too full for a build to start.
//!
//! Once more than [`HIGH_PERCENT`] of the disk is used, the least recently used images that live
//! runs and builds don't hold ([`Held`]), until their sizes bring the use down to
//! [`LOW_PERCENT`] ([`evicted`]). The build cache is then pruned to the same mark
//! ([`Engine::prune_build_cache`](crate::Engine::prune_build_cache)), which also frees the
//! layers that it shared with the images removed. Past [`REFUSED_PERCENT`], as once a prune has
//! left the disk there, a build is refused ([`Disk::refused`]).

use std::collections::BTreeSet;

use schema::ir::Image;

/// The share of the disk used, in percent, past which a prune removes images.
// Past it, a task's build may not fit what is left: its images take 20 to 70 GB, and a fifth of
// the controller's default 1000 GiB volume is 200 GiB.
pub const HIGH_PERCENT: u64 = 80;

/// The share of the disk used, in percent, down to which a prune removes images.
// Low enough to leave room for the builds of several tasks, as of jobs that build at once.
pub const LOW_PERCENT: u64 = 60;

/// The share of the disk used, in percent, past which a build is refused.
// The kubelet's default hard eviction threshold for the disk that holds its images,
// `imagefs.available<15%`, past which it admits no pod.
pub const REFUSED_PERCENT: u64 = 85;

const _: () = assert!(LOW_PERCENT < HIGH_PERCENT && HIGH_PERCENT < REFUSED_PERCENT);
const _: () = assert!(REFUSED_PERCENT <= 100);

/// The disk that an engine keeps its images and build cache on.
#[derive(Clone, Copy, Debug)]
pub struct Disk {
    pub size_bytes: u64,
    /// What an ordinary user may still write there, as `df` counts it.
    pub free_bytes: u64,
}

impl Disk {
    /// Whether more than [`HIGH_PERCENT`] of the disk is used, so that a prune removes images.
    pub fn full(self) -> bool {
        self.used_bytes() > self.size_bytes / 100 * HIGH_PERCENT
    }

    /// Whether more than [`REFUSED_PERCENT`] of the disk is used, so that a build is refused.
    pub fn refused(self) -> bool {
        self.used_bytes() > self.size_bytes / 100 * REFUSED_PERCENT
    }

    /// What is free of the disk once [`LOW_PERCENT`] of it is used, in bytes.
    pub fn low_free_bytes(self) -> u64 {
        self.size_bytes - self.size_bytes / 100 * LOW_PERCENT
    }

    fn used_bytes(self) -> u64 {
        self.size_bytes.saturating_sub(self.free_bytes)
    }
}

/// An image that the engine has, with its tags and its size, which counts its layers shared
/// with other images ([`Engine::images`](crate::Engine::images)).
#[derive(Debug)]
pub struct Cached {
    pub image: Image,
    /// Each as `<repository>:<tag>`, such as a build's ([`Built::tag`](crate::Built::tag)).
    pub tags: Vec<String>,
    pub size_bytes: u64,
}

/// What no prune removes: the images that live runs and builds use, by their IDs and tags.
#[derive(Debug, Default)]
pub struct Held {
    /// Each image held by its ID, such as one that a live run names, or that a live build found
    /// or built.
    pub images: BTreeSet<Image>,
    /// Each tag held, such as the one that a build looks its image up by, before it looks.
    pub tags: BTreeSet<String>,
    /// Whether a build may be under way, whose image is untagged until the build tags it: then
    /// no untagged image is removed.
    pub building: bool,
}

impl Held {
    fn holds(&self, cached: &Cached) -> bool {
        self.images.contains(&cached.image)
            || cached.tags.iter().any(|tag| self.tags.contains(tag))
            || (self.building && cached.tags.is_empty())
    }
}

/// The images of `cached` that a prune removes from an engine whose images are on `disk`.
///
/// Each was last used as `last_used` says, by what orders the uses, such as the start of the
/// newest job that named it, or never. None unless the disk is [full](Disk::full); else, of those
/// that `held` doesn't hold, least recently used first, those never used, then the rest by their
/// last use, each tie by its ID, until their sizes add up to what brings the disk's use down to
/// [`LOW_PERCENT`], or all.
pub fn evicted<U: Ord>(
    disk: Disk,
    cached: Vec<Cached>,
    last_used: impl Fn(&Image) -> Option<U>,
    held: &Held,
) -> Vec<Image> {
    if !disk.full() {
        return Vec::new();
    }
    // It counts each image's whole size, so where images share layers it frees less than it
    // reckons, and a later prune frees the rest.
    let excess_bytes = disk.low_free_bytes().saturating_sub(disk.free_bytes);
    let mut candidates: Vec<Cached> = (cached.into_iter())
        .filter(|cached| !held.holds(cached))
        .collect();
    candidates.sort_by_cached_key(|cached| (last_used(&cached.image), cached.image.clone()));
    let mut freed_bytes = 0u64;
    let mut evicted = Vec::new();
    for Cached {
        image,
        tags: _,
        size_bytes,
    } in candidates
    {
        if freed_bytes >= excess_bytes {
            break;
        }
        freed_bytes = freed_bytes.saturating_add(size_bytes);
        evicted.push(image);
    }
    evicted
}
