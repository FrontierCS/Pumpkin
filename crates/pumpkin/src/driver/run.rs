//! The runtime: runs a workflow on the [Docker provider](pumpkin_docker), as the
//! [core](super::core) decides, and keeps the run's record.
//!
//! [`Run::prepare`] parses a workflow and begins its record, and [`Run::execute`] runs it to
//! its outcome. One thread drives the run. In a loop, it takes the agent's ops from its
//! session, the reports of judge calls, and a tick when the core wants one
//! ([`Core::tick_due`]); it turns each into an [`Event`], appends that to the run log with the
//! actions the core returns, and then performs them. Each judge call runs on a thread of its
//! own, in a fresh sandbox (the `judge` module); the record's files and the run log's records
//! are laid out in the `record` and `log` modules.
//!
//! What a role's code finds in its sandbox is the in-sandbox contract, which
//! [`pumpkin_schema::sandbox`] gives.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, Scope};
use std::time::Duration;
use std::{fs, mem};

use pumpkin_docker::Engine;
use pumpkin_gateway::{IncomingOp, Op};
use pumpkin_schema::ir;
use pumpkin_schema::sandbox::op::OpReply;
use pumpkin_schema::workflow::{JudgeCall, Workflow};
use serde_json::json;
use stdx::Context;

use crate::driver::core::{Action, Core, Event, JobId, OpId, Outcome, PayloadRef, Request};

mod agent;
mod judge;
mod log;
mod record;
mod sandbox;

use agent::{Agent, Seen};
use log::Log;
use record::Payloads;
use sandbox::{Clock, Sandboxes};

/// How long the loop waits for the agent's next op, or a judge's report, before it looks at
/// the rest again.
const POLL: Duration = Duration::from_millis(50);

/// A run, prepared: its workflow parsed, and its record begun.
#[derive(Debug)]
pub struct Run {
    workflow: Workflow,
    id: String,
    dir: PathBuf,
}

impl Run {
    /// Parses `ir` and makes the run's record in `runs`, which is created if need be:
    /// `ir.json`. A workflow that has a model route is refused, with `outcome.json` saying why.
    ///
    /// # Errors
    ///
    /// Fails if the IR breaks a parsing rule, if the workflow is refused, or if the record
    /// can't be written.
    pub fn prepare(ir: &ir::Workflow, runs: &Path) -> Result<Self, String> {
        let workflow = Workflow::parse(ir).map_err(|errors| errors.to_string())?;
        let (id, dir) = record::create(runs)?;
        record::write_json(&dir, "ir.json", &json!(ir))?;
        if let Some(reason) = model_route(&workflow) {
            let outcome = json!({ "state": "refused", "reason": reason });
            record::write_json(&dir, "outcome.json", &outcome)?;
            return Err(reason);
        }
        Ok(Self { workflow, id, dir })
    }

    /// The run's id, which names its record, and its containers'
    /// [`RunTag`](pumpkin_docker::RunTag), by which [`Engine::destroy_all`] finds them. Its
    /// sandboxes' socket directories are in `pumpkin-<id>` in the system's temporary directory
    /// while it executes.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The run's record.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Runs the workflow to its outcome on `engine`, with `sandboxd`, a static `sandboxd`
    /// binary, in every sandbox. The outcome, or why the run stopped without one, is also in
    /// the record's `outcome.json`.
    ///
    /// # Errors
    ///
    /// Fails if `sandboxd` can't be found, if the run's socket directory can't be made, if
    /// Docker can't be reached, if a sandbox can't be destroyed, or if the record can't be
    /// written. Removes the run's containers and socket directories before returning, however
    /// it ends, and fails if it can't.
    pub fn execute(self, engine: &Engine, sandboxd: &Path) -> Result<Outcome, String> {
        let outcome = self.drive(engine, sandboxd);
        let recorded = match &outcome {
            Ok(outcome) => {
                let mut recorded = log::outcome_json(outcome);
                recorded["state"] = "finished".into();
                recorded
            }
            Err(reason) => json!({ "state": "failed", "reason": reason }),
        };
        let written = record::write_json(&self.dir, "outcome.json", &recorded);
        failed_also(outcome, written)
    }

    /// Runs the workflow, then removes whatever the run left behind.
    fn drive(&self, engine: &Engine, sandboxd: &Path) -> Result<Outcome, String> {
        record::write_json(&self.dir, "engine.json", &json!(engine.report()))?;
        let mut log = Log::create(&self.dir)?;
        let payloads = Payloads::create(&self.dir)?;
        let sandboxes = Sandboxes::create(&self.id, engine.clone(), sandboxd)?;
        let clock = Clock::start();
        // The loop runs beside each judge call's thread, which borrows the workflow and the
        // record; the scope ends once every call's thread has.
        let outcome = thread::scope(|scope| {
            let (reporter, reports) = mpsc::channel();
            let mut runtime = Runtime {
                scope,
                workflow: &self.workflow,
                dir: &self.dir,
                sandboxes: &sandboxes,
                clock,
                log: &mut log,
                payloads: &payloads,
                agent: None,
                ops: HashMap::new(),
                next_op: 1,
                stops: HashMap::new(),
                reporter,
                reports,
                pending: VecDeque::new(),
            };
            let outcome = runtime.drive();
            if outcome.is_err() {
                runtime.abort();
            }
            outcome
        });
        // Every judge call's thread has ended, so no container starts after this.
        let swept = sandboxes.sweep();
        let at = clock.now();
        let swept = swept.and_then(|left| match left.as_slice() {
            [] => Ok(()),
            left => log.note(
                at,
                &format!("removed containers the run left: {}", left.join(", ")),
            ),
        });
        let outcome = failed_also(outcome, swept);
        let noted = match &outcome {
            Ok(_) => Ok(()),
            Err(reason) => log.note(at, &format!("the run failed: {reason}")),
        };
        failed_also(outcome, noted)
    }
}

/// `outcome`, failed also for `then`'s error, if it has one: both reasons are kept.
fn failed_also(
    outcome: Result<Outcome, String>,
    then: Result<(), String>,
) -> Result<Outcome, String> {
    match (outcome, then) {
        (outcome, Ok(())) => outcome,
        (Ok(_), Err(error)) => Err(error),
        (Err(reason), Err(error)) => Err(format!("{reason}; {error}")),
    }
}

/// Why a workflow with a model route is refused. Parsing has refused a route on a judge.
fn model_route(workflow: &Workflow) -> Option<String> {
    let agent = &workflow.agent().sandbox;
    agent.model_route.as_ref()?;
    Some(format!(
        "`{}` has a model route, which the driver doesn't serve: remove the role's \
         `egress.model` to run without one",
        agent.name
    ))
}

/// Whether `reply` accepts the call it answers.
fn accepts(reply: &OpReply) -> bool {
    match reply {
        OpReply::Judge(_) | OpReply::Submit(_) => true,
        OpReply::Ops(_) | OpReply::Status(_) | OpReply::Refused { .. } => false,
    }
}

/// What a judge call's thread reports.
#[derive(Debug)]
enum Report {
    /// An event, observed at a time.
    Event(Duration, Event),
    /// A note for the log, at a time.
    Note(Duration, String),
    /// A sandbox that couldn't be destroyed, which fails the run.
    Undestroyed(String),
}

/// An op of the agent's, awaiting the core's reply.
#[derive(Debug)]
enum Unanswered {
    /// An op that carries no payload: `ops` or `status`.
    Query(IncomingOp),
    /// A judge call or a submission, with its payload, taken out of the op, and the payload's
    /// reference.
    Call {
        incoming: IncomingOp,
        payload: Vec<u8>,
        reference: PayloadRef,
    },
}

/// The state of a run as it is driven.
struct Runtime<'scope, 'env> {
    scope: &'scope Scope<'scope, 'env>,
    workflow: &'env Workflow,
    dir: &'env Path,
    sandboxes: &'env Sandboxes,
    clock: Clock,
    log: &'env mut Log,
    payloads: &'env Payloads,
    agent: Option<Agent>,
    /// The ops awaiting the core's reply, by the number the runtime gave them.
    ops: HashMap<OpId, Unanswered>,
    /// The number the next op takes.
    next_op: u64,
    /// How to stop each job's thread.
    // Each flag is shared with its job's thread, which checks it between its steps and while its
    // command runs.
    stops: HashMap<JobId, Arc<AtomicBool>>,
    // Each job's thread holds a clone, through which it reports to the loop.
    reporter: Sender<Report>,
    reports: Receiver<Report>,
    /// Events that performing actions produced, to feed the core next.
    pending: VecDeque<(Duration, Event)>,
}

impl Runtime<'_, '_> {
    /// Drives the run from its start to its outcome.
    fn drive(&mut self) -> Result<Outcome, String> {
        let (mut core, mut actions) = Core::start(self.workflow);
        self.log.start(self.workflow, self.clock.unix(), &actions)?;
        loop {
            if let Some(outcome) = self.perform(actions)? {
                return Ok(outcome);
            }
            let (at, event) = self.next_event(&core)?;
            actions = core.handle(at, event.clone());
            self.log.event(self.workflow, at, &event, &actions)?;
        }
    }

    /// Waits for the next event: one that performing an action produced, a judge's report,
    /// a tick that is due, or what the agent did. An op the gateway refused itself goes
    /// straight to the run log, since the core neither decides on it nor learns of it.
    fn next_event(&mut self, core: &Core<'_>) -> Result<(Duration, Event), String> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(event);
            }
            while let Ok(report) = self.reports.try_recv() {
                if let Some(event) = self.report(report)? {
                    return Ok(event);
                }
            }
            let now = self.clock.now();
            if core.tick_due().is_some_and(|due| now >= due) {
                return Ok((now, Event::Tick));
            }
            match self.agent.as_mut().filter(|agent| agent.taking_ops()) {
                Some(agent) => match agent.watch(POLL) {
                    Seen::Op(op) => return Ok(self.op(op)),
                    Seen::Refused(refused) => {
                        let at = self.clock.now();
                        self.log.refused(self.workflow, at, &refused)?;
                    }
                    Seen::Event(event) => return Ok((self.clock.now(), event)),
                    Seen::Nothing => {}
                },
                None => {
                    if let Ok(report) = self.reports.recv_timeout(POLL)
                        && let Some(event) = self.report(report)?
                    {
                        return Ok(event);
                    }
                }
            }
        }
    }

    /// Takes in a judge call's report: its event, if it has one.
    fn report(&mut self, report: Report) -> Result<Option<(Duration, Event)>, String> {
        match report {
            Report::Event(at, event) => Ok(Some((at, event))),
            Report::Note(at, note) => {
                self.log.note(at, &note)?;
                Ok(None)
            }
            Report::Undestroyed(reason) => Err(reason),
        }
    }

    /// Takes in an op from the agent: the event of its arrival. A payload is stored only once
    /// the core accepts its op ([`Runtime::reply`]).
    fn op(&mut self, mut incoming: IncomingOp) -> (Duration, Event) {
        let (request, call) = match &mut incoming.op {
            Op::Ops => (Request::Ops, None),
            Op::Judge { grant, payload } => {
                let payload = mem::take(payload);
                let reference = record::reference(&payload);
                let request = Request::Judge {
                    grant: grant.clone(),
                    payload: reference.clone(),
                };
                (request, Some((payload, reference)))
            }
            Op::Submit {
                grant,
                finalize,
                payload,
            } => {
                let payload = mem::take(payload);
                let reference = record::reference(&payload);
                let request = Request::Submit {
                    grant: grant.clone(),
                    finalize: *finalize,
                    payload: reference.clone(),
                };
                (request, Some((payload, reference)))
            }
            Op::Status { job } => (Request::Status { job: *job }, None),
        };
        let unanswered = match call {
            Some((payload, reference)) => Unanswered::Call {
                incoming,
                payload,
                reference,
            },
            None => Unanswered::Query(incoming),
        };
        let op = OpId(self.next_op);
        self.next_op += 1;
        self.ops.insert(op, unanswered);
        let event = Event::OpArrived { op, request };
        (self.clock.now(), event)
    }

    /// Performs the core's actions, in order: the outcome once one finishes the run.
    fn perform(&mut self, actions: Vec<Action>) -> Result<Option<Outcome>, String> {
        for action in actions {
            match action {
                Action::StartAgent => {
                    let (agent, event) = Agent::start(self.sandboxes, self.workflow);
                    self.agent = agent;
                    self.pending.push_back((self.clock.now(), event));
                }
                Action::Reply { op, reply } => self.reply(op, reply)?,
                Action::StartJudge {
                    job,
                    call,
                    payload,
                    timeout,
                } => self.start_judge(job, call, &payload, timeout)?,
                Action::StopJudge { job } => {
                    #[expect(
                        clippy::unreachable,
                        reason = "the core stops only jobs it has started, and the StartJudge \
                                  that starts one inserts its flag first, even when its thread \
                                  then fails to start; a type tying the stop to the start \
                                  would carry the runtime's flag through the core, which \
                                  holds no runtime state"
                    )]
                    let Some(stop) = self.stops.get(&job) else {
                        unreachable!("job {} was stopped, but never started", job.0)
                    };
                    stop.store(true, Ordering::Relaxed);
                }
                Action::TearDownAgent => self.tear_down_agent()?,
                Action::Finish { outcome } => return Ok(Some(outcome)),
            }
        }
        Ok(None)
    }

    /// Answers an op. An op the core accepted has its payload stored first, since a judge call
    /// reads it, now or in the final phase; a refused op's bytes are never written.
    fn reply(&mut self, op: OpId, reply: OpReply) -> Result<(), String> {
        #[expect(
            clippy::unreachable,
            reason = "the core answers every op it is given exactly once (`Core`), and the \
                      runtime gives it each op once, under a number that only this removes; a \
                      type tying the reply to its op would carry the gateway's `IncomingOp` \
                      through the core, which does no I/O"
        )]
        let Some(unanswered) = self.ops.remove(&op) else {
            unreachable!("op {} was answered twice, or never given to the core", op.0)
        };
        let incoming = match unanswered {
            Unanswered::Query(incoming) => incoming,
            Unanswered::Call {
                incoming,
                payload,
                reference,
            } => {
                if accepts(&reply) {
                    self.payloads.store(&payload, &reference)?;
                }
                incoming
            }
        };
        let sent = match &mut self.agent {
            Some(agent) => agent
                .reply(incoming, reply)
                .context(format_args!("op {}'s reply failed", op.0)),
            None => Err(format!(
                "op {}'s reply wasn't sent: the agent's sandbox is gone",
                op.0
            )),
        };
        match sent {
            Ok(()) => Ok(()),
            Err(note) => self.log.note(self.clock.now(), &note),
        }
    }

    /// Starts a judge call on a thread of its own.
    fn start_judge(
        &mut self,
        job: JobId,
        call: JudgeCall,
        payload: &PayloadRef,
        timeout: Duration,
    ) -> Result<(), String> {
        let slot = self.workflow.call_slot(call);
        let dir = self.dir.join("jobs").join(job.0.to_string());
        fs::create_dir_all(&dir).context(dir.display())?;
        let stop = Arc::new(AtomicBool::new(false));
        self.stops.insert(job, Arc::clone(&stop));
        let call = judge::Call {
            job,
            judge: self.workflow.judge(slot.judge()),
            slot: &self.workflow.slot(slot).name,
            payload: self.payloads.path(&payload.digest),
            timeout,
            dir,
            sandboxes: self.sandboxes,
            clock: self.clock,
            stop,
            reports: self.reporter.clone(),
        };
        let spawned = thread::Builder::new()
            .name(format!("job-{}", job.0))
            .spawn_scoped(self.scope, move || judge::run(&call));
        if let Err(error) = spawned {
            let reason = format!("the call's thread didn't start: {error}");
            let event = Event::JudgeFailed { job, reason };
            self.pending.push_back((self.clock.now(), event));
        }
        Ok(())
    }

    /// Freezes and collects the agent's sandbox, then destroys it, even if collecting failed.
    /// What couldn't be collected is noted in the run log. A failure to write the record, or to
    /// destroy the sandbox, is returned once both have been tried, joined if both fail.
    fn tear_down_agent(&mut self) -> Result<(), String> {
        let collected = self.freeze_and_collect_agent();
        let destroyed = match self.agent.take() {
            Some(agent) => (agent.destroy()).context("the agent's sandbox could not be destroyed"),
            None => Ok(()),
        };
        match (collected, destroyed) {
            (collected, Ok(())) => collected,
            (Ok(()), Err(error)) => Err(error),
            (Err(collected), Err(error)) => Err(format!("{collected}; {error}")),
        }
    }

    fn freeze_and_collect_agent(&mut self) -> Result<(), String> {
        let dir = self.dir.join("agent");
        fs::create_dir(&dir).context(dir.display())?;
        let notes = match &mut self.agent {
            Some(agent) => {
                let mut notes = agent.freeze_and_collect(&dir);
                let collected = record::collected(&dir);
                notes.push(format!("collected from the agent's sandbox: {collected}"));
                notes
            }
            None => vec!["the agent's sandbox never started: nothing was collected".to_owned()],
        };
        let at = self.clock.now();
        notes.iter().try_for_each(|note| self.log.note(at, note))
    }

    /// Stops what is left of a failed run: every job, and the agent's sandbox, as best it can.
    fn abort(&mut self) {
        for stop in self.stops.values() {
            stop.store(true, Ordering::Relaxed);
        }
        // Dropping the agent's container ends its session and removes it.
        self.agent = None;
    }
}
