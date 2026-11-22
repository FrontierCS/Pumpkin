//! When a job's tree and directory expire, by the time in the job's id.

use std::time::SystemTime;

use crate::KEPT;

/// Whether a job started more than `KEPT_DAYS` days before `now`. `started` is the time that the
/// job's id says, if it says one.
///
/// A job whose id says no time never expires. A prune removes an expired job's tree.
pub fn expired(started: Option<SystemTime>, now: SystemTime) -> bool {
    started.is_some_and(|started| crate::past(started, KEPT, now))
}

/// A run's job's directory, `jobs/<id>/`, as a prune observed it.
#[derive(Clone, Debug)]
pub struct Job {
    pub id: String,
    /// When the job started, if its id says.
    pub started: Option<SystemTime>,
    /// Whether the run's record is there. The record names the job's configuration, to run it
    /// again.
    pub recorded: bool,
}

/// The jobs in `jobs` whose directories a prune removes at `now`, in their order.
///
/// Each job has [`expired`], and its run's record has gone.
pub fn removed(jobs: &[Job], now: SystemTime) -> Vec<&Job> {
    (jobs.iter())
        .filter(|job| !job.recorded && expired(job.started, now))
        .collect()
}
