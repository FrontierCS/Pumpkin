//! The runs' records.
//!
//! A run's record is evidence, and the controller's may be its only copy, so no shortage of room
//! removes it. It lasts until the run's member has copied it whole. After that it lasts
//! `KEPT_DAYS` days from the last copy. The controller keeps the run's tree as long, to run it
//! again.

use std::time::SystemTime;

use crate::KEPT;

/// A run's job, `jobs/<id>/`, and the run's record, as a prune observed them.
#[derive(Clone, Debug)]
pub struct Record {
    pub id: String,
    /// When the run's member last copied its record whole, if they have.
    pub copied: Option<SystemTime>,
    /// Whether the run's job runs, which its directory in tmpfs shows.
    pub running: bool,
    /// Whether the run's record is there.
    pub recorded: bool,
}

/// The records in `records` that a prune removes at `now`, in their order.
///
/// Each comes as its run's id, with when its member last copied it. Each record is there. Its job
/// no longer runs. Its member last copied it more than `KEPT_DAYS` days before `now`. It takes no
/// account of what holds a record, or of what was written to it since that copy.
pub fn removed(records: &[Record], now: SystemTime) -> Vec<(&str, SystemTime)> {
    (records.iter())
        .filter(|record| record.recorded && !record.running)
        .filter_map(|record| Some((record.id.as_str(), record.copied?)))
        .filter(|&(_, copied)| crate::past(copied, KEPT, now))
        .collect()
}

/// The records in `records` that their members haven't copied, by their runs' ids.
///
/// Each is there, its job no longer runs, and its member hasn't copied it, so no prune removes it.
/// They come in the order of `records`.
pub fn uncopied(records: &[Record]) -> Vec<&str> {
    (records.iter())
        .filter(|record| record.recorded && !record.running && record.copied.is_none())
        .map(|record| record.id.as_str())
        .collect()
}
