//! What a run's driver used, as its `outcome.json` keeps it, and what a live run's job uses. Each
//! is said in the words that `wait`'s summary, `ls` and the page share.

use std::fmt::Write as _;
use std::time::Duration;

use runtime::{DriverUse, JobUse, Record};
use serde::{Deserialize, Serialize};

use super::model::seconds;

/// A figure of what a run's driver used: its name, and its value or [`UNREAD`].
pub(crate) type Figure = (String, String);

/// The value of a figure that the driver couldn't read.
const UNREAD: &str = "unread";

/// Adds to `summary` the line of what `record`'s driver used, if its record keeps that, such as
/// `driver: CPU 0.85s, children's CPU unread, peak memory 12.3 MiB`.
pub(super) fn summarize(summary: &mut String, record: &Record) {
    let _ = match record.driver_use() {
        Ok(None) => Ok(()),
        Ok(Some(used)) => {
            let (driver, job) = figures(&used);
            let job = job.map(|job| format!("; job: {}", said(&job)));
            writeln!(
                summary,
                "  driver: {}{}",
                said(&driver),
                job.unwrap_or_default()
            )
        }
        Err(error) => {
            let error = crate::escaped(&error.to_string());
            writeln!(summary, "  what its driver used can't be read: {error}")
        }
    };
}

/// The driver's CPU time, as its figure says it.
pub(crate) fn cpu(used: &DriverUse) -> String {
    value(used.cpu, seconds)
}

/// The figures of `used`: the driver's, then its job's, if it ran as a controller's job.
pub(crate) fn figures(used: &DriverUse) -> (Vec<Figure>, Option<Vec<Figure>>) {
    let DriverUse {
        cpu: _,
        children_cpu,
        peak_memory_bytes,
        job,
    } = used;
    let driver = vec![
        ("CPU".to_owned(), cpu(used)),
        ("children's CPU".to_owned(), value(*children_cpu, seconds)),
        (
            "peak memory".to_owned(),
            value(*peak_memory_bytes, crate::size),
        ),
    ];
    let job = job.as_ref().map(|job| {
        let JobUse {
            unit: _,
            cpu,
            peak_memory_bytes,
            peak_tasks_count,
        } = job;
        vec![
            ("CPU".to_owned(), value(*cpu, seconds)),
            (
                "peak memory".to_owned(),
                value(*peak_memory_bytes, crate::size),
            ),
            (
                "peak tasks".to_owned(),
                value(*peak_tasks_count, |count| count.to_string()),
            ),
        ]
    });
    (driver, job)
}

/// What the unit of a job that runs uses: the CPU time its tasks have used so far, and their
/// working set. That is the memory they hold now, less the page cache that the kernel reclaims
/// first. Its tasks are the job's driver and the commands it runs. The engine runs sandboxes and
/// builds in cgroups of its own. A figure is `None` if it couldn't be read.
#[derive(Default, Serialize, Deserialize)]
pub(crate) struct UnitUse {
    pub(crate) cpu: Option<Duration>,
    pub(crate) memory_bytes: Option<u64>,
}

pub(crate) fn unit_figures(used: &UnitUse) -> [Figure; 2] {
    let UnitUse { cpu, memory_bytes } = used;
    [
        ("CPU".to_owned(), value(*cpu, seconds)),
        ("memory".to_owned(), value(*memory_bytes, crate::size)),
    ]
}

/// `used`, as `ls` says it under its run's row, such as `CPU 57.03s, memory 483.2 MiB`.
pub(crate) fn unit_said(used: &UnitUse) -> String {
    said(&unit_figures(used))
}

fn value<T>(figure: Option<T>, shown: impl FnOnce(T) -> String) -> String {
    figure.map_or_else(|| UNREAD.to_owned(), shown)
}

fn said(figures: &[Figure]) -> String {
    let said: Vec<String> = (figures.iter())
        .map(|(name, value)| format!("{name} {value}"))
        .collect();
    said.join(", ")
}
