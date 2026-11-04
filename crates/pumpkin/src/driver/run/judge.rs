//! Judge calls, each on a thread of its own, in a fresh passive sandbox.
//!
//! [`run`] reports a call's end only after its sandbox is destroyed; a sandbox that can't be
//! destroyed fails the run. [`start`] puts the call's payload in its slot
//! ([`slot_payload`](pumpkin_schema::sandbox::slot_payload)), and [`outputs`] collects and
//! parses the judge's result under its [`ResultSpec`](pumpkin_schema::ir::ResultSpec) and caps.
//!
//! A stop the core asks for takes effect before the command starts and while it runs.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use pumpkin_docker::Container;
use pumpkin_gateway::{Process, Put, Session};
use pumpkin_schema::ir::Name;
use pumpkin_schema::sandbox::tree::Limits;
use pumpkin_schema::sandbox::{
    RESULT_MAX_BYTES, RelativePath, evidence_dir, log_dir, out_dir, pumpkin_dir, result_file,
    slot_dir, slot_payload,
};
use pumpkin_schema::workflow::Judge;
use stdx::Context;

use super::sandbox::{Clock, PUMPKIN, Sandboxes, collect, launch};
use super::{Report, record};
use crate::driver::core::{Event, JobId, JudgeResult};

/// What a judge's logs keep at most.
const LOGS: Limits = Limits {
    bytes: 64 << 20,
    entries: 1000,
};

/// How often a running command is checked for a stop.
const POLL: Duration = Duration::from_millis(100);

/// A judge call, as its thread sees it.
#[derive(Debug)]
pub(super) struct Call<'r> {
    pub(super) job: JobId,
    pub(super) judge: &'r Judge,
    pub(super) slot: &'r Name,
    /// Where the payload is stored.
    pub(super) payload: PathBuf,
    pub(super) timeout: Duration,
    /// The call's directory in the record, `jobs/<n>`, which exists and is empty.
    pub(super) dir: PathBuf,
    pub(super) sandboxes: &'r Sandboxes,
    pub(super) clock: Clock,
    /// Set when the core asks to stop the call.
    // Shared with the loop, which sets it.
    pub(super) stop: Arc<AtomicBool>,
    // A clone of the loop's, which takes the call's reports.
    pub(super) reports: Sender<Report>,
}

/// How a judge's command ended.
enum Ended {
    Exited,
    TimedOut,
    Stopped,
}

/// Runs a judge call to its end, reporting as it goes.
pub(super) fn run(call: &Call<'_>) {
    let job = call.job;
    let name = format!("job-{}", job.0);
    let mut container = match call.sandboxes.start(&name, &call.judge.sandbox, None) {
        Ok(container) => container,
        Err(error) => {
            let reason = format!("the judge's sandbox didn't start: {error}");
            return call.event(Event::JudgeFailed { job, reason });
        }
    };
    let last = judge(call, &mut container);
    match container.destroy() {
        Ok(()) => call.event(last),
        Err(error) => call.report(Report::Undestroyed(format!(
            "job {}'s sandbox could not be destroyed: {error}",
            job.0
        ))),
    }
}

/// Runs the call in its sandbox, and returns the report of its end.
fn judge(call: &Call<'_>, container: &mut Container) -> Event {
    let job = call.job;
    let failed = |reason: String| Event::JudgeFailed { job, reason };
    if call.stopped() {
        return failed("the call was stopped before its command started".to_owned());
    }
    let session = container.session();
    let (mut process, started) = match start(call, session) {
        Ok(started) => started,
        Err(reason) => return failed(reason),
    };
    let ended = match wait(call, &mut process, started.checked_add(call.timeout)) {
        Ok(ended) => ended,
        Err(error) => return failed(format!("the judge's session ended: {error}")),
    };
    if let Err(error) = session.freeze() {
        return failed(format!("freezing the judge's sandbox failed: {error}"));
    }
    let result = outputs(call, session);
    if let Err(error) = collect(session, &log_dir(), &call.dir, LOGS) {
        call.note(format!("job {}'s logs are incomplete: {error}", job.0));
    }
    match ended {
        Ended::Exited | Ended::TimedOut => Event::JudgeCollected { job, result },
        Ended::Stopped => failed("the call was stopped".to_owned()),
    }
}

/// Puts the payload in the judge's slot and starts its command, then reports the start.
/// Returns the command, and when it started.
fn start(call: &Call<'_>, session: &mut Session) -> Result<(Process, Instant), String> {
    let payload = fs::read(&call.payload).context(call.payload.display())?;
    let (slot, raw, evidence) = (slot_dir(call.slot), slot_payload(call.slot), evidence_dir());
    let puts = [
        Put::Dir(&slot),
        Put::File(&raw, &payload),
        Put::Dir(&evidence),
    ];
    session
        .put(&pumpkin_dir(), &puts)
        .context("putting the payload failed")?;
    let process = session
        .start(launch(&call.judge.command))
        .context("the judge's command didn't start")?;
    let started = Instant::now();
    call.report(Report::Event(
        call.clock.at(started),
        Event::JudgeStarted { job: call.job },
    ));
    Ok((process, started))
}

/// Waits for the command to exit, up to `deadline`, or until a stop. Reports its exit or its
/// timeout. A `deadline` of `None`, past what the clock can hold, is never reached.
fn wait(call: &Call<'_>, process: &mut Process, deadline: Option<Instant>) -> io::Result<Ended> {
    let job = call.job;
    loop {
        let left = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
        if left.is_some_and(|left| left.is_zero()) {
            call.event(Event::JudgeTimedOut { job });
            return Ok(Ended::TimedOut);
        }
        if call.stopped() {
            return Ok(Ended::Stopped);
        }
        if let Some(status) = process.wait(left.map_or(POLL, |left| left.min(POLL)))? {
            call.event(Event::JudgeExited { job, status });
            return Ok(Ended::Exited);
        }
    }
}

/// Collects [`out_dir()`] into the record, keeps what the result contract allows, and parses
/// the result.
fn outputs(call: &Call<'_>, session: &mut Session) -> Result<JudgeResult, String> {
    let collected = collect(session, &out_dir(), &call.dir, PUMPKIN);
    let result = read_result(&call.dir.join(result_file().as_str()));
    if let Err(error) = prune_outputs(call) {
        let job = call.job.0;
        call.note(format!(
            "removing what the record doesn't keep of job {job}'s outputs failed: {error}"
        ));
    }
    collected.context("the judge's outputs are incomplete")?;
    JudgeResult::parse(&call.judge.result, &result?)
}

/// Reads the collected result, at `path`, or removes it if it is too large.
fn read_result(path: &Path) -> Result<Vec<u8>, String> {
    let size = match fs::metadata(path) {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let expected = pumpkin_dir().join(&result_file());
            return Err(format!("the judge wrote no result: write it at {expected}"));
        }
        Err(error) => return Err(error).context(path.display()),
    };
    if size > RESULT_MAX_BYTES {
        let too_large = format!(
            "result.json is {size} bytes, over the limit of {RESULT_MAX_BYTES}: write a smaller \
             one"
        );
        return Err(match fs::remove_file(path) {
            Ok(()) => too_large,
            Err(error) => format!("{too_large}; removing it from the record failed: {error}"),
        });
    }
    fs::read(path).context(path.display())
}

/// Removes from the collected [`out_dir()`] what the record doesn't keep: anything but the
/// [`result_file()`] and the [`evidence_dir()`], and the evidence if it holds more than the
/// judge's `evidence_max_bytes`.
fn prune_outputs(call: &Call<'_>) -> io::Result<()> {
    let max = call.judge.result.evidence_max_bytes;
    let (result, evidence) = (result_file(), evidence_dir());
    let kept = |path: &RelativePath| call.dir.join(path.as_str());
    let entries = match fs::read_dir(call.dir.join(out_dir().as_str())) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let is_dir = entry.file_type()?.is_dir();
        if !is_dir && path == kept(&result) {
            continue;
        }
        if is_dir && path == kept(&evidence) {
            let bytes = record::tree_size(&path)?.bytes;
            if bytes <= max {
                continue;
            }
            call.note(format!(
                "job {}'s evidence holds {bytes} bytes, over its limit of {max}: it is not kept",
                call.job.0
            ));
        } else {
            call.note(format!(
                "job {} wrote {}, which is neither its {result} file nor its {evidence} \
                 directory: it is not kept",
                call.job.0,
                path.display()
            ));
        }
        if is_dir {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}

impl Call<'_> {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Reports an event, observed now.
    fn event(&self, event: Event) {
        self.report(Report::Event(self.clock.now(), event));
    }

    fn note(&self, note: String) {
        self.report(Report::Note(self.clock.now(), note));
    }

    fn report(&self, report: Report) {
        // The runtime has stopped listening only once the run is over.
        let _ = self.reports.send(report);
    }
}
