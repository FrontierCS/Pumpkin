//! What the controller's prune removes of what its jobs keep on its volume, decided on values.
//!
//! The decisions cover the runs' records, the installs, the jobs' trees and directories, and the
//! store's files. Each takes what the prune observed in the controller's directories, as plain
//! values, and the time now. It returns what to remove. The caller reads the directories, holds the
//! locks while it reads them and removes the files. `engine::prune` decides which images go.

use std::time::{Duration, SystemTime};

pub mod boot;
pub mod installs;
pub mod jobs;
pub mod records;
pub mod store;

/// How long a prune keeps what it removes by age, in days.
// One horizon for the trees, the jobs' directories, the installs and the copied records. A run's
// record names its entry in its tree, to run it again. A tree is links into the store, so it holds
// no bytes of its own.
pub const KEPT_DAYS: u64 = 14;

const KEPT: Duration = Duration::from_hours(KEPT_DAYS * 24);

/// Whether `at` is more than `kept` before `now`. False if `at` is after `now`.
fn past(at: SystemTime, kept: Duration, now: SystemTime) -> bool {
    now.duration_since(at).is_ok_and(|age| age > kept)
}
