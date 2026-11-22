//! Each case is what a prune observed. Its snapshot shows what the prune removes and what it
//! keeps.

mod boot;
mod installs;
mod jobs;
mod records;
mod store;

use std::time::{Duration, SystemTime};

// A fixed now, so that each time below is a span before it.
fn now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_hours(20_000 * 24)
}

fn ago(span: Duration) -> SystemTime {
    now() - span
}

fn days(count: u64) -> Duration {
    Duration::from_hours(count * 24)
}
