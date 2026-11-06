//! Judge calls, each on a thread of its own, in a fresh passive sandbox.
//!
//! [`run`] reports a call's end only once its sandbox is deleted; a sandbox that can't be
//! deleted fails the run. [`start`] puts the call's payload in its slot
//! ([`slot_payload`](pumpkin_schema::sandbox::slot_payload)), and [`outputs`] collects and
//! parses the judge's result under its [`ResultSpec`](pumpkin_schema::ir::ResultSpec).
//!
//! A call's thread waits on an inbox of its own, into which its session hands what its reader
//! reads.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::Duration;
use std::{fs, io};

use pumpkin_gateway::{Arrival, Put, Session};
use pumpkin_schema::ir::Name;
use pumpkin_schema::sandbox::{
    RESULT_MAX_BYTES, evidence_dir, log_dir, out_dir, pumpkin_dir, result_file, slot_dir,
    slot_payload,
};
use pumpkin_schema::workflow::Judge;
use stdx::{Context, Deadline};

use super::sandbox::{PUMPKIN, Sandboxes, collect, launch};
use super::{Clock, Inbox, Input};
use crate::driver::core::{Event, JobId, JudgeResult};

/// A judge call, as its thread sees it.
pub(super) struct Call<'r> {
    pub(super) job: JobId,
    pub(super) judge: &'r Judge,
    pub(super) slot: &'r Name,
    /// Where the payload is stored.
    pub(super) payload: PathBuf,
    /// The longest the judge's command may run.
    pub(super) timeout: Duration,
    pub(super) deadlines: Deadlines,
    /// The call's directory in the record, `jobs/<n>`, which exists and is empty.
    pub(super) dir: PathBuf,
    pub(super) sandboxes: &'r Sandboxes<'r>,
    pub(super) clock: Clock,
    /// What its session's reader hands over ([`Listener::accept`]): at most its command's exit,
    /// as it runs one command, and its end, since a passive session hands out no op.
    ///
    /// [`Listener::accept`]: pumpkin_gateway::Listener::accept
    pub(super) inbox: Inbox<Arrival>,
    /// A sender into the loop's inbox, which takes the call's reports.
    pub(super) reports: Sender<Input>,
}

/// By when each of a judge call's steps is to be done.
#[derive(Clone, Copy)]
pub(super) struct Deadlines {
    /// Its sandbox's start: created, accepted and set up.
    pub(super) startup: Deadline,
    /// Its payload's put and its command's start.
    pub(super) start: Deadline,
    /// Its sandbox's freeze, and the collecting of what it holds.
    pub(super) collect: Deadline,
}

/// What a judge call's thread reports to the loop.
pub(super) enum Report {
    /// An event, at the time the thread observed it: the core meters a call's time from its
    /// reports' times, which the loop's delays then don't lengthen.
    Event(Duration, Event),
    /// A note for the run log, at the time the thread observed it.
    Note(Duration, String),
    /// Why the call's sandbox couldn't be deleted, which fails the run.
    Undeleted(String),
}

/// Runs a judge call to its end, reporting as it goes.
pub(super) fn run(call: &Call<'_>) {
    let job = call.job;
    let name = format!("job-{}", job.0);
    let (sandbox, by) = (&call.judge.sandbox, call.deadlines.startup);
    let inbox = call.inbox.sender();
    let started = (call.sandboxes).start(&name, sandbox, None, by, inbox, |arrival| arrival);
    let mut sandbox = match started {
        Ok(sandbox) => sandbox,
        Err(error) => {
            let reason = format!("the judge's sandbox didn't start: {error}");
            return call.event(Event::JudgeFailed { job, reason });
        }
    };
    let last = judge(call, &mut sandbox.session);
    match call.sandboxes.delete(sandbox) {
        Ok(()) => call.event(last),
        Err(error) => call.report(Report::Undeleted(error)),
    }
}

/// Runs the call in its sandbox, whose session is `session`, and returns the report of its end.
fn judge(call: &Call<'_>, session: &mut Session) -> Event {
    let job = call.job;
    let failed = |reason: String| Event::JudgeFailed { job, reason };
    let timeout = match start(call, session) {
        Ok(timeout) => timeout,
        Err(reason) => return failed(reason),
    };
    if let Err(reason) = wait(call, timeout) {
        return failed(format!("the judge's session ended: {reason}"));
    }
    let by = call.deadlines.collect;
    if let Err(error) = session.freeze(by) {
        return failed(format!("freezing the judge's sandbox failed: {error}"));
    }
    let result = outputs(call, session);
    if let Err(error) = collect(session, &log_dir(), &call.dir, PUMPKIN, by) {
        call.note(format!("job {}'s log is incomplete: {error}", job.0));
    }
    Event::JudgeCollected { job, result }
}

/// Puts the payload in the judge's slot, and the evidence directory, and starts its command,
/// then reports the start. Returns when the command's timeout ends.
fn start(call: &Call<'_>, session: &mut Session) -> Result<Deadline, String> {
    let payload = fs::read(&call.payload).context(call.payload.display())?;
    let (slot, raw, evidence) = (slot_dir(call.slot), slot_payload(call.slot), evidence_dir());
    let puts = [
        Put::Dir(&slot),
        Put::File(&raw, &payload),
        Put::Dir(&evidence),
    ];
    let by = call.deadlines.start;
    (session.put(&pumpkin_dir(), &puts, by)).context("putting the payload failed")?;
    // The sandbox runs this one command, so its exit needs no `Process` to tell it apart.
    (session.start(launch(&call.judge.command), by)).context("the judge's command didn't start")?;
    // The start's time comes first, so that the time the core counts for the command is at
    // least its timeout once it runs out.
    let at = call.clock.now();
    let timeout = Deadline::after(call.timeout);
    call.report(Report::Event(at, Event::JudgeStarted { job: call.job }));
    Ok(timeout)
}

/// Waits for the command to exit, until `timeout`, and reports its exit or its timeout. Fails
/// with the reason the session ended, if it ended first.
fn wait(call: &Call<'_>, timeout: Deadline) -> Result<(), String> {
    let job = call.job;
    match call.inbox.wait(timeout) {
        None => {
            call.event(Event::JudgeTimedOut { job });
            Ok(())
        }
        // The sandbox runs one command, so the exit is its.
        Some(Arrival::Exited { process: _, status }) => {
            call.event(Event::JudgeExited { job, status });
            Ok(())
        }
        Some(Arrival::Ended { reason }) => Err(reason),
        #[expect(
            clippy::unreachable,
            reason = "the judge's session was accepted without an op payload limit, and such a \
                      session ends on an op instead of handing it out (`Listener::accept`); a \
                      type for a passive session's arrivals would split the gateway's `pub` \
                      `Arrival` by the kind of session"
        )]
        Some(Arrival::Op(_) | Arrival::Refused(_)) => {
            unreachable!("job {}'s passive session handed out an op", job.0)
        }
    }
}

/// Collects [`out_dir()`] into the record, and parses the result the judge wrote there.
fn outputs(call: &Call<'_>, session: &mut Session) -> Result<JudgeResult, String> {
    let collected = collect(
        session,
        &out_dir(),
        &call.dir,
        PUMPKIN,
        call.deadlines.collect,
    );
    let result = read_result(&call.dir.join(result_file().as_str()));
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

impl Call<'_> {
    /// Reports an event, observed now.
    fn event(&self, event: Event) {
        self.report(Report::Event(self.clock.now(), event));
    }

    fn note(&self, note: String) {
        self.report(Report::Note(self.clock.now(), note));
    }

    fn report(&self, report: Report) {
        // The loop's inbox outlives every call's thread, which the run's end waits for.
        let _ = self.reports.send(Input::Judge(report));
    }
}
