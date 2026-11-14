//! Judge calls, each a child of the run ([`child`](super::child)): [`call`] puts the call's
//! payload in its slot ([`slot_payload`](schema::sandbox::slot_payload)), runs the judge's
//! command until it exits, its timeout passes or the core stops the call, and [`outputs`]
//! collects and parses the judge's result under its [`ResultSpec`](schema::ir::ResultSpec).

use std::convert::Infallible;
use std::path::Path;
use std::time::Duration;
use std::{fs, io};

use decision::{Event, JobId, JudgeResult};
use gateway::{Arrival, Put, Session};
use schema::sandbox::{
    RESULT_MAX_BYTES, dir_mode, evidence_dir, file_mode, log_dir, out_dir, pumpkin_dir,
    result_file, slot_dir, slot_payload,
};
use schema::workflow::{Judge, SlotId};
use stdx::Context;

use super::child::{Child, Message, Report};
use super::host::deadline;
use super::record::Stored;
use super::sandbox::{collect, launch};
use super::{Clock, Host, Inbox};

/// A judge call, as its thread drives it, in its child of the run, which takes no orders but the
/// core's stop.
pub(super) struct Call {
    pub(super) job: JobId,
    /// The slot the payload is put in, whose judge's sandbox and command the call runs, as the
    /// workflow of its child's sandboxes finds them.
    pub(super) slot: SlotId,
    pub(super) payload: Stored,
    /// The longest the judge's command may run.
    pub(super) timeout: Duration,
}

/// How a judge's command ended.
enum Ended {
    /// It exited, or ran until its timeout.
    Ran,
    /// The core stopped the call.
    Stopped,
}

/// Runs `call` to its end in `child`, as a child of the run runs ([`child::run`]).
///
/// [`child::run`]: super::child::run
pub(super) fn run<H: Host>(call: &Call, child: &mut Child<'_, H, Infallible>) {
    super::child::run(child, |child, session| judge(call, child, session));
}

/// Runs `call` in `child`'s sandbox, whose session is `session`, and returns the report of its
/// end.
fn judge<H: Host>(
    call: &Call,
    child: &mut Child<'_, H, Infallible>,
    session: &mut Session,
) -> Event {
    let job = call.job;
    let failed = |reason: String| Event::JudgeFailed { job, reason };
    if child.stopped() {
        return failed("the call was stopped before its command started".to_owned());
    }
    let timeout = match start(call, child, session) {
        Ok(timeout) => timeout,
        Err(reason) => return failed(reason),
    };
    let ended = match wait(child, job, timeout) {
        Ok(ended) => ended,
        Err(reason) => return failed(format!("the judge's session ended: {reason}")),
    };
    let by = deadline(&child.clock, child.deadlines.collect);
    if let Err(error) = session.freeze(by) {
        return failed(format!("freezing the judge's sandbox failed: {error}"));
    }
    let result = outputs(child, judge_of(child, call.slot), session);
    if let Err(error) = collect(session, &log_dir(), &child.dir, by) {
        child.note(format!("job {}'s log is incomplete: {error}", job.0));
    }
    match ended {
        Ended::Ran => Event::JudgeCollected { job, result },
        Ended::Stopped => failed("the call was stopped".to_owned()),
    }
}

/// Puts the payload in the judge's slot, and the evidence directory, and starts its command,
/// then reports the start. Returns when the command's timeout ends, since the run started.
fn start<H: Host>(
    call: &Call,
    child: &Child<'_, H, Infallible>,
    session: &mut Session,
) -> Result<Duration, String> {
    let payload = call.payload.read()?;
    let name = &child.sandboxes.workflow().slot(call.slot).name;
    let (slot_at, raw, evidence) = (slot_dir(name), slot_payload(name), evidence_dir());
    let puts = [
        Put::Dir(slot_at, dir_mode()),
        Put::File(raw, file_mode(), &payload),
        Put::Dir(evidence, dir_mode()),
    ];
    let by = deadline(&child.clock, child.deadlines.start);
    (session.put(&pumpkin_dir(), puts, by)).context("putting the payload failed")?;
    // The sandbox runs this one command, so its exit needs no `Process` to tell it apart.
    let launched = launch(&judge_of(child, call.slot).command);
    (session.start(launched, by)).context("the judge's command didn't start")?;
    // The timeout runs from the start's reported time.
    let at = child.clock.now();
    child.report(Report::Event(at, Event::JudgeStarted { job: call.job }));
    Ok(at.saturating_add(call.timeout))
}

/// Waits for the command of job `job` to exit, until `timeout`, a time since the run started, or
/// for a stop, and reports its exit or its timeout. Fails with the reason the session ended, if
/// it ended first.
fn wait<H: Host>(
    child: &mut Child<'_, H, Infallible>,
    job: JobId,
    timeout: Duration,
) -> Result<Ended, String> {
    match child.inbox.wait(timeout) {
        None => {
            child.event(Event::JudgeTimedOut { job });
            Ok(Ended::Ran)
        }
        Some(Message::Stop) => Ok(Ended::Stopped),
        Some(Message::Order(never)) => match never {},
        // The sandbox runs one command, so the exit is its.
        Some(Message::Session(Arrival::Exited { process: _, status })) => {
            child.event(Event::JudgeExited { job, status });
            Ok(Ended::Ran)
        }
        Some(Message::Session(Arrival::Ended { reason })) => Err(reason),
        // A type for a passive session's arrivals would split the gateway's `pub` `Arrival` by
        // the kind of session.
        Some(Message::Session(Arrival::Op(_) | Arrival::Refused(_))) => unreachable!(
            "job {}'s passive session handed out an op, though it was accepted without an op \
             payload limit, and such a session ends on an op instead (`Registration::accept`)",
            job.0
        ),
        // No type excludes a model request here, since one `Arrival` serves sessions with model
        // requests and without.
        Some(Message::Session(Arrival::ModelRequest(_))) => unreachable!(
            "job {}'s session handed out a model request, though it was accepted without model \
             requests, so one breaks the protocol, which ends the session, and arrives as its end",
            job.0
        ),
    }
}

/// Collects [`out_dir()`] into the record, and parses the result that `judge` wrote there.
fn outputs<H: Host>(
    child: &Child<'_, H, Infallible>,
    judge: &Judge,
    session: &mut Session,
) -> Result<JudgeResult, String> {
    let by = deadline(&child.clock, child.deadlines.collect);
    let collected = collect(session, &out_dir(), &child.dir, by);
    let result = read_result(&child.dir.join(result_file().as_str()));
    collected.context("the judge's outputs are incomplete")?;
    JudgeResult::parse(&judge.result, &result?)
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

/// The judge whose slot `slot` is, as the workflow of `child`'s sandboxes finds it.
fn judge_of<'r, H: Host>(child: &Child<'r, H, Infallible>, slot: SlotId) -> &'r Judge {
    child.sandboxes.workflow().judge(slot.judge())
}
