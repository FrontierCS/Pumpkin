//! What a run's driver used, as its `outcome.json` keeps it, in the words that `wait`'s summary and
//! the page share.

use std::fmt::Write as _;

use runtime::{DriverUse, JobUse, Record};

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

fn value<T>(figure: Option<T>, shown: impl FnOnce(T) -> String) -> String {
    figure.map_or_else(|| UNREAD.to_owned(), shown)
}

fn said(figures: &[Figure]) -> String {
    let said: Vec<String> = (figures.iter())
        .map(|(name, value)| format!("{name} {value}"))
        .collect();
    said.join(", ")
}
