//! The score's judge call, a child of the run ([`child`](super::child)): the call puts its payload
//! in its slot, runs the judge's command until it exits, its timeout passes or the core stops the
//! call, relaying its session's ops and model requests to the loop meanwhile, and collects and
//! parses the judge's result under its [`ResultSpec`](schema::ir::ResultSpec).

use std::path::Path;
use std::time::Duration;
use std::{fs, io};

use decision::{Event, JobId, JudgeResult};
use gateway::{Arrival, IncomingOp, Put, Session};
use schema::sandbox::op::OpReply;
use schema::sandbox::{
    ExitStatus, OpPayloadLimit, RESULT_MAX_BYTES, dir_mode, evidence_dir, file_mode, log_dir,
    out_dir, pumpkin_dir, result_file, slot_dir, slot_payload,
};
use schema::workflow::{Holder, Judge, SlotId};
use stdx::{Context, Deadline};

use super::child::{Child, Message, Report};
use super::host::deadline;
use super::record::Stored;
use super::sandbox::{collect, launch};
use super::{Clock, Host, Inbox, request};

/// The score's judge call, as its thread drives it, in its child of the run, whose orders are the
/// core's replies to its ops.
pub(super) struct Call {
    pub(super) job: JobId,
    /// The slot the payload is put in, whose judge's sandbox and command the call runs, as the
    /// workflow of its child's sandboxes finds them.
    pub(super) slot: SlotId,
    pub(super) payload: Stored,
    /// By when the payload is put and the judge's command started, as a time since the run
    /// started.
    pub(super) start: Duration,
    /// The longest the judge's command may run.
    pub(super) timeout: Duration,
}

/// How waiting on a child's command ended.
pub(super) enum Waited {
    Exited(ExitStatus),
    /// Its timeout passed first.
    TimedOut,
    /// The core stopped the child first.
    Stopped,
}

pub(super) fn run<H: Host>(call: &Call, child: &mut Child<'_, H, OpReply>) {
    let workflow = child.sandboxes.workflow();
    let holder = Holder::Judge(call.slot.judge());
    // A judge's ops carry no payload: it holds sessions alone.
    let ops = (workflow.sessions())
        .any(|(_, grant)| grant.holder == holder)
        .then_some(OpPayloadLimit::NONE);
    super::child::run(child, ops, |child, session| judge(call, child, session));
}

fn judge<H: Host>(call: &Call, child: &mut Child<'_, H, OpReply>, session: &mut Session) -> Event {
    let job = call.job;
    let failed = |reason: String| Event::JudgeFailed { job, reason };
    if child.stopped() {
        return failed("the call was stopped before its command started".to_owned());
    }
    let timeout = match start(call, child, session) {
        Ok(timeout) => timeout,
        Err(reason) => return failed(reason),
    };
    let ran = match Relay::default().wait(child, job, timeout, session) {
        Ok(Waited::Exited(status)) => {
            child.event(Event::JudgeExited { job, status });
            true
        }
        Ok(Waited::TimedOut) => {
            child.event(Event::JudgeTimedOut { job });
            true
        }
        Ok(Waited::Stopped) => false,
        Err(reason) => return failed(format!("the judge's session ended: {reason}")),
    };
    let by = deadline(&child.clock, child.deadlines.collect);
    if let Err(error) = session.freeze(by) {
        return failed(format!("freezing the judge's sandbox failed: {error}"));
    }
    let result = outputs(child, judge_of(child), session);
    if let Err(error) = collect(session, &log_dir(), &child.dir, by) {
        child.note(format!("job {}'s log is incomplete: {error}", job.0));
    }
    if ran {
        Event::JudgeCollected { job, result }
    } else {
        failed("the call was stopped".to_owned())
    }
}

/// Puts the payload in the judge's slot, and the evidence directory, and starts its command,
/// then reports the start. Returns when the command's timeout ends, since the run started.
fn start<H: Host>(
    call: &Call,
    child: &Child<'_, H, OpReply>,
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
    let by = deadline(&child.clock, call.start);
    (session.put(&pumpkin_dir(), puts, by)).context("putting the payload failed")?;
    // The sandbox runs this one command, so its exit needs no `Process` to tell it apart.
    let launched = launch(&judge_of(child).command);
    (session.start(launched, by)).context("the judge's command didn't start")?;
    // The timeout runs from the start's reported time.
    let at = child.clock.now();
    child.report(Report::Event(at, Event::JudgeStarted { job: call.job }));
    Ok(at.saturating_add(call.timeout))
}

/// What a job's thread relays between its sandbox's session and the loop: each op its sandbox
/// makes, which the loop answers with the reply it sends back, and each model request, which the
/// loop answers. The session hands out an op only once the one before it has been answered, so
/// at most one is unanswered, which a later wait answers if the command that made it has exited.
#[derive(Default)]
pub(super) struct Relay {
    asked: Option<IncomingOp>,
}

/// What came of a message into a job's inbox, if it ends a wait.
enum Arrived {
    Exited(ExitStatus),
    Stopped,
}

impl Relay {
    /// Waits for the command that job `job` runs to exit, until `timeout`, a time since the run
    /// started, or for a stop, relaying meanwhile. Fails with the reason the session ended, if it
    /// ended first.
    pub(super) fn wait<H: Host>(
        &mut self,
        child: &mut Child<'_, H, OpReply>,
        job: JobId,
        timeout: Duration,
        session: &mut Session,
    ) -> Result<Waited, String> {
        let by = deadline(&child.clock, timeout);
        loop {
            let Some(message) = child.inbox.wait(timeout) else {
                return Ok(Waited::TimedOut);
            };
            match self.relay(child, job, (session, by), message)? {
                None => {}
                Some(Arrived::Exited(status)) => return Ok(Waited::Exited(status)),
                Some(Arrived::Stopped) => return Ok(Waited::Stopped),
            }
        }
    }

    /// Relays what job `job`'s inbox holds, while none of its commands runs, without waiting for
    /// more, and returns whether the core stopped the job. Fails with the reason the session
    /// ended, if it has.
    pub(super) fn idle<H: Host>(
        &mut self,
        child: &mut Child<'_, H, OpReply>,
        job: JobId,
        (session, by): (&mut Session, Deadline),
    ) -> Result<bool, String> {
        while let Some(message) = child.inbox.take() {
            match self.relay(child, job, (&mut *session, by), message)? {
                None => {}
                Some(Arrived::Stopped) => return Ok(true),
                Some(Arrived::Exited(_)) => unreachable!(
                    "job {}'s sandbox reported a command's exit while none ran, though the job \
                     starts one command at a time and waits for each to exit before it starts \
                     the next",
                    job.0
                ),
            }
        }
        Ok(false)
    }

    fn relay<H: Host>(
        &mut self,
        child: &mut Child<'_, H, OpReply>,
        job: JobId,
        (session, by): (&mut Session, Deadline),
        message: Message<OpReply>,
    ) -> Result<Option<Arrived>, String> {
        match message {
            Message::Stop => return Ok(Some(Arrived::Stopped)),
            Message::Order(reply) => {
                let Some(op) = self.asked.take() else {
                    unreachable!(
                        "job {}'s call was sent a reply to no op, though the loop replies only \
                         to the op the call relayed, once",
                        job.0
                    )
                };
                if let Err(error) = session.reply(op, reply, by) {
                    child.note(format!("job {}'s op's reply failed: {error}", job.0));
                }
            }
            Message::Session(Arrival::Op(incoming)) => {
                let (request, unanswered) = request(incoming);
                self.asked = Some(unanswered.into_incoming());
                let (at, trees) = (child.clock.now(), session.trees());
                child.report(Report::Op {
                    at,
                    job,
                    request,
                    trees,
                });
            }
            Message::Session(Arrival::Refused(refused)) => {
                let message = refused.message();
                if let Err(error) = session.refuse(refused, by) {
                    child.note(format!("job {}'s op's refusal failed: {error}", job.0));
                }
                child.note(format!("job {}'s op was refused: {message}", job.0));
            }
            // The sandbox runs one command at a time, so the exit is its.
            Message::Session(Arrival::Exited { process: _, status }) => {
                return Ok(Some(Arrived::Exited(status)));
            }
            Message::Session(Arrival::Ended { reason }) => return Err(reason),
            Message::Session(Arrival::ModelRequest(incoming)) => {
                let at = child.clock.now();
                child.report(Report::Model { at, job, incoming });
            }
        }
        Ok(None)
    }
}

/// Collects [`out_dir()`] into the record, and parses the result that `judge` wrote there.
fn outputs<H: Host>(
    child: &Child<'_, H, OpReply>,
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

fn judge_of<'r, H: Host>(child: &Child<'r, H, OpReply>) -> &'r Judge {
    child.sandboxes.workflow().judge()
}
