//! The runtime: runs a workflow on a [`Provider`], as the [core](super::core) decides, and keeps
//! the run's record.
//!
//! [`Run::prepare`] parses a workflow and begins its record, and [`Run::execute`] runs it to
//! its outcome on one thread, driving the core as [`Core`] says. Between the core's ticks, the
//! thread waits on one inbox, into which the sessions of the run's sandboxes hand what they
//! read. Its sandboxes start on one path, the `sandbox` module's, and the agent's is the `agent`
//! module's. The record's files and the run log's records, and when each is written, are laid
//! out in the `record` and `log` modules.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::{Duration, Instant, SystemTime};
use std::{fs, io, mem};

use pumpkin_gateway::{Arrival, IncomingOp, Op, RefusedOp};
use pumpkin_provider::Provider;
use pumpkin_schema::ir;
use pumpkin_schema::provider::{RunTag, SandboxId};
use pumpkin_schema::sandbox::op::OpReply;
use pumpkin_schema::workflow::{Main, Workflow};
use serde_json::json;
use stdx::{Context, Deadline};

use crate::driver::core::{Action, Core, Event, OpId, Outcome, PayloadRef, Request, lasts};

mod agent;
mod log;
mod record;
mod sandbox;

use agent::Agent;
use log::Log;
use record::Payloads;
use sandbox::Sandboxes;

/// A run, prepared: its workflow parsed, and its record begun.
#[derive(Debug)]
pub struct Run {
    workflow: Workflow,
    id: String,
    dir: PathBuf,
}

impl Run {
    /// Parses `ir` and makes the run's record in `runs`, which is created if need be:
    /// `ir.json`. The run's id holds `now`, the time by the caller's clock. A workflow that has a
    /// model route is refused, with `outcome.json` saying why.
    ///
    /// # Errors
    ///
    /// Fails if the IR breaks a parsing rule, if `now` is before 1970, if the workflow is
    /// refused, or if the record can't be written.
    pub fn prepare(ir: &ir::Workflow, runs: &Path, now: SystemTime) -> Result<Self, String> {
        let workflow = Workflow::parse(ir).map_err(|errors| errors.to_string())?;
        let (id, dir) = record::create(runs, now)?;
        record::write_json(&dir, "ir.json", &json!(ir))?;
        if let Some(reason) = model_route(&workflow) {
            let outcome = json!({ "state": "refused", "reason": reason });
            record::write_json(&dir, "outcome.json", &outcome)?;
            return Err(reason);
        }
        Ok(Self { workflow, id, dir })
    }

    /// The run's id, which names its record, and is the run tag its sandboxes carry.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The run's record.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Runs the workflow to its outcome on `provider`, with `sandboxd`, a static `sandboxd`
    /// binary, in every sandbox. The outcome, or why the run stopped without one, is also in the
    /// record's `outcome.json`. However the run ends, it deletes the sandboxes that `provider`
    /// still lists of it before returning.
    ///
    /// # Errors
    ///
    /// Fails if what the run's sandboxes run can't be published, or the provider doesn't name
    /// each thing published; if the run's socket directory can't be made; if the run was still
    /// going at its hard end ([`lasts`]); if the machine's clock reads before 1970; if the run's
    /// sandboxes can't be listed at its end, or one of them can't be deleted; or if the record
    /// can't be written.
    pub fn execute(self, provider: &dyn Provider, sandboxd: &Path) -> Result<Outcome, String> {
        let outcome = self.run_on(provider, sandboxd);
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

    /// Records what `provider` declared and publishes what the run's sandboxes run, then starts
    /// the run's clock and drives the run, keeping its log, and deletes what the provider still
    /// lists of it.
    fn run_on(&self, provider: &dyn Provider, sandboxd: &Path) -> Result<Outcome, String> {
        let ready = provider.ready();
        record::write_json(&self.dir, "provider.json", &json!(ready))?;
        let startup = Duration::from_secs(ready.capabilities.startup_seconds.get());
        let sandboxes = Sandboxes::create(provider, self.tag(), &self.workflow, sandboxd)?;
        let clock = Clock::start();
        let unix = record::since_epoch(clock.unix)?;
        let mut runtime = Runtime {
            workflow: &self.workflow,
            startup,
            dir: &self.dir,
            sandboxes,
            clock,
            log: Log::create(&self.dir)?,
            payloads: Payloads::create(&self.dir)?,
            agent: None,
            unanswered: None,
            next_op: 0,
            pending: VecDeque::new(),
            inbox: Inbox::new(),
        };
        let outcome = runtime.drive(unix);
        let outcome = failed_also(outcome, runtime.sweep());
        let noted = match &outcome {
            Ok(_) => Ok(()),
            Err(reason) => (runtime.log).note(clock.now(), &format!("the run failed: {reason}")),
        };
        failed_also(outcome, noted)
    }

    /// The tag its sandboxes carry: its id.
    fn tag(&self) -> RunTag {
        match RunTag::try_from(self.id.clone()) {
            Ok(tag) => tag,
            #[expect(
                clippy::unreachable,
                reason = "a run's id is a UTC time's digits and `-`, then lowercase hexadecimal \
                          digits (`record::create`), all of which a run tag allows, and starts \
                          with a digit; a run tag built from a time and a number, which couldn't \
                          fail, would be a `pub` item of the schema for this one caller"
            )]
            Err(error) => unreachable!("run {}'s id is no run tag: {error}", self.id),
        }
    }
}

/// A run's clock: the time since the run started, which the core takes with each event, and
/// when that was by the machine's clock.
#[derive(Clone, Copy, Debug)]
struct Clock {
    start: Instant,
    unix: SystemTime,
}

impl Clock {
    /// The clock of a run that starts now.
    fn start() -> Self {
        Self {
            start: Instant::now(),
            unix: SystemTime::now(),
        }
    }

    fn now(self) -> Duration {
        self.start.elapsed()
    }

    /// The deadline at `at`, a time since the run started.
    fn deadline(self, at: Duration) -> Deadline {
        Deadline::after(at.saturating_sub(self.now()))
    }
}

/// `outcome`, failed also for `then`'s error, if it has one: both reasons are kept.
fn failed_also<T>(outcome: Result<T, String>, then: Result<(), String>) -> Result<T, String> {
    match (outcome, then) {
        (outcome, Ok(())) => outcome,
        (Ok(_), Err(error)) => Err(error),
        (Err(reason), Err(error)) => Err(format!("{reason}; {error}")),
    }
}

/// Whether `reply` accepts the call it answers.
fn accepts(reply: &OpReply) -> bool {
    match reply {
        OpReply::Judge(_) | OpReply::Submit(_) => true,
        OpReply::Ops(_) | OpReply::Status(_) | OpReply::Refused { .. } => false,
    }
}

/// An op of the agent's, awaiting the core's reply.
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

/// What `incoming`, an op of the agent's, asks the core, and the op, to answer once the core
/// replies.
fn request(mut incoming: IncomingOp) -> (Request, Unanswered) {
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
    (request, unanswered)
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

/// The state of a run as it is driven.
struct Runtime<'r> {
    workflow: &'r Workflow,
    /// The longest the provider takes to start a sandbox.
    startup: Duration,
    /// The run's record.
    dir: &'r Path,
    /// The run's sandboxes, and its only way to its provider.
    sandboxes: Sandboxes<'r>,
    clock: Clock,
    log: Log,
    payloads: Payloads,
    agent: Option<Agent>,
    /// The op the core is to answer, by the number the runtime gave it, if one is: the agent's
    /// session hands out an op only once the one before it has been answered.
    unanswered: Option<(OpId, Unanswered)>,
    /// The number the next op takes, counting from 0.
    next_op: u64,
    /// Events that performing actions produced, to feed the core next.
    pending: VecDeque<(Duration, Event)>,
    /// What reaches the loop from other threads, which it waits on until the core's next tick.
    inbox: Inbox<Input>,
}

/// What reaches the loop through its inbox.
enum Input {
    /// What the agent's session's reader handed over ([`Listener::accept`]): at most the op in
    /// flight, as the session holds one, its command's exit, as it runs one command, and its end.
    ///
    /// [`Listener::accept`]: pumpkin_gateway::Listener::accept
    Agent(Arrival),
}

/// What other threads send a thread, in the order they sent it, for it to wait on.
struct Inbox<T> {
    receiver: Receiver<T>,
    // Kept, so that the inbox never disconnects while its owner waits on it, whoever else holds
    // a sender.
    sender: Sender<T>,
}

impl<T> Inbox<T> {
    fn new() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self { receiver, sender }
    }

    /// A sender into the inbox.
    fn sender(&self) -> Sender<T> {
        self.sender.clone()
    }

    /// What arrives first by `until`, or once it has passed, what the inbox holds, if anything.
    fn wait(&self, until: Deadline) -> Option<T> {
        let Some(left) = until.remaining() else {
            return self.take();
        };
        match self.receiver.recv_timeout(left) {
            Ok(item) => Some(item),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => Self::disconnected(),
        }
    }

    /// What the inbox holds first, if anything.
    fn take(&self) -> Option<T> {
        match self.receiver.try_recv() {
            Ok(item) => Some(item),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Self::disconnected(),
        }
    }

    #[expect(
        clippy::unreachable,
        reason = "an inbox holds a sender of its own, so its channel can't disconnect while the \
                  inbox is borrowed to wait on it; `std`'s channel reports a disconnection, which \
                  a channel with a sender never has, in the same errors as a timeout and an empty \
                  channel"
    )]
    fn disconnected() -> ! {
        unreachable!("an inbox disconnected, though it holds a sender")
    }
}

impl Runtime<'_> {
    /// Drives the run from its start, `unix` after the Unix epoch, until the core finishes or
    /// abandons it, then gives the core what is left in the inbox ([`Runtime::drain`]).
    fn drive(&mut self, unix: Duration) -> Result<Outcome, String> {
        let (mut core, mut actions) = Core::start(self.workflow, self.startup);
        self.log.start(self.workflow, unix, &actions)?;
        // Only the core's end drains the inbox: after a failure of the runtime's own, such as a
        // record it couldn't write, the core may still be taking ops, and would decide on ones
        // that nothing acts on.
        loop {
            if let Some(end) = self.perform(&core, actions)? {
                return failed_also(end, self.drain(&mut core));
            }
            let (at, event) = self.next_event(&core)?;
            actions = core.handle(at, event.clone());
            self.log.event(self.workflow, at, &event, &actions)?;
        }
    }

    /// The run's hard end, since it started, as the core has it.
    fn hard_end(&self) -> Duration {
        lasts(self.workflow, self.startup)
    }

    /// When the agent's teardown is to end, since the run started: the hard end, less the final
    /// judge's startup and timeout, so that the final judge still has both.
    fn teardown_end(&self) -> Duration {
        let Main::SoloThenJudge(pattern) = self.workflow.main();
        let final_judge = self.startup.saturating_add(pattern.score.timeout);
        self.hard_end().saturating_sub(final_judge)
    }

    /// When the answer to an op is due, since the run started: the next time the core acts of
    /// itself ([`Core::tick_due`]), or the hard end once it won't.
    fn answers_due(&self, core: &Core<'_>) -> Duration {
        // An answer the agent doesn't read then holds the loop no later than the time it would
        // act on next anyway.
        core.tick_due().unwrap_or_else(|| self.hard_end())
    }

    /// Waits for the next event: one that performing an action produced; else the core's tick, if
    /// it is due; else what arrives in the inbox before the tick is due, and else the tick then.
    /// The wait ends at the tick's time, which arrivals the core doesn't wait on, handled as they
    /// come, don't move.
    fn next_event(&mut self, core: &Core<'_>) -> Result<(Duration, Event), String> {
        if let Some(event) = self.pending.pop_front() {
            return Ok(event);
        }
        #[expect(
            clippy::unreachable,
            reason = "`drive` calls this only until `perform` returns the run's end, on \
                      `Finish` or `Abandon`, and `tick_due` is `Some` in every phase where the \
                      run is still going, so `None` here is a bug"
        )]
        let Some(due) = core.tick_due() else {
            unreachable!("the run is still going, yet the core has no tick due")
        };
        let until = self.clock.deadline(due);
        loop {
            let input = if until.remaining().is_none() {
                None
            } else {
                self.inbox.wait(until)
            };
            let Some(input) = input else {
                return Ok((self.clock.now(), Event::Tick));
            };
            if let Some(event) = self.arrived(input, due)? {
                return Ok(event);
            }
        }
    }

    /// The event that `input` brings the core, if the core waits on it, at the time the loop
    /// took it. An op whose payload the gateway refused is refused by `due`, the core's next
    /// tick, and goes to the run log, since the core neither decides on it nor learns of it.
    fn arrived(
        &mut self,
        input: Input,
        due: Duration,
    ) -> Result<Option<(Duration, Event)>, String> {
        let at = self.clock.now();
        let Input::Agent(arrival) = input;
        // Ops are taken as usual even once the agent's session has closed: the core has ended
        // the work phase, or the run, by then, so it refuses each, and the run log notes that
        // the answer wasn't sent.
        let event = match arrival {
            Arrival::Op(incoming) => Some(self.op(incoming)),
            Arrival::Refused(refused) => {
                self.refuse(at, refused, self.clock.deadline(due))?;
                None
            }
            // With no agent, the core waits on neither. The loop tears the agent down in the
            // batch that hears of its end, or of its command's failed start, so whatever of the
            // session's it takes after that batch comes here, such as its command's exit as the
            // teardown's freeze kills it, its end as the teardown drops it, or the exit of a
            // command whose start failed at its deadline just as the sandbox said it started. So
            // does the end of a session that the start path dropped after its set-up failed.
            Arrival::Exited { process, status } => {
                (self.agent.as_mut()).and_then(|agent| agent.exited(process, status))
            }
            Arrival::Ended { reason } => {
                (self.agent.as_mut()).and_then(|agent| agent.ended(&reason))
            }
        };
        Ok(event.map(|event| (at, event)))
    }

    /// Gives the core `incoming`, an op of the agent's: the event of its arrival, under the next
    /// number. Its payload is stored only once the core accepts it ([`Runtime::reply`]).
    fn op(&mut self, incoming: IncomingOp) -> Event {
        let (request, unanswered) = request(incoming);
        let op = OpId(self.next_op);
        self.next_op += 1;
        #[expect(
            clippy::unreachable,
            reason = "the agent's session hands out an op only once the one before it has been \
                      answered, by `Session::reply`, which only `Runtime::reply` reaches, taking \
                      that op from here, or by `Session::refuse`, for an op that never comes \
                      here; the session's one op in flight is a rule of the protocol behind its \
                      `pub` API, which a type showing it would reshape"
        )]
        if let Some((earlier, _)) = self.unanswered.replace((op, unanswered)) {
            unreachable!("op {} arrived while op {} was unanswered", op.0, earlier.0)
        }
        Event::OpArrived { op, request }
    }

    /// Refuses `refused`, an op of the agent's whose payload the gateway refused, taken at `at`,
    /// by `by`, and records it in the run log.
    fn refuse(&mut self, at: Duration, refused: RefusedOp, by: Deadline) -> Result<(), String> {
        self.log.refused(self.workflow, at, &refused)?;
        let what = "the refusal of an op whose payload was over the agent's limit";
        self.answer(what, by, |agent, by| agent.refuse(refused, by))
    }

    /// Takes what is left in the inbox once the core has finished or abandoned the run, as the
    /// loop does ([`Runtime::arrived`]): the core refuses each op, as it does every op after its
    /// end, and the run log notes that the refusal wasn't sent.
    fn drain(&mut self, core: &mut Core<'_>) -> Result<(), String> {
        // Its teardown has dropped the agent's session, unless the run was abandoned first, and
        // this drops it then, its sandbox left to the sweep: either way, its reader has handed
        // over all it will by now.
        self.agent = None;
        while let Some(input) = self.inbox.take() {
            let Some((at, event)) = self.arrived(input, self.answers_due(core))? else {
                continue;
            };
            let actions = core.handle(at, event.clone());
            self.log.event(self.workflow, at, &event, &actions)?;
            #[expect(
                clippy::unreachable,
                reason = "after `Finish` or `Abandon`, the core only refuses the ops that still \
                          arrive, as `Action::Finish` and `Action::Abandon` say, so it ends the \
                          run no more; a type showing it would split the core's actions by the \
                          run's phase"
            )]
            if self.perform(core, actions)?.is_some() {
                unreachable!("the core ended the run it had ended")
            }
        }
        Ok(())
    }

    /// Performs the core's actions, in order, and returns how the run ended, if one of them
    /// ended it: with its outcome, or why it has none.
    fn perform(
        &mut self,
        core: &Core<'_>,
        actions: Vec<Action>,
    ) -> Result<Option<Result<Outcome, String>>, String> {
        let mut end = None;
        for action in actions {
            match action {
                Action::StartAgent => {
                    // The agent starts at the run's start, so its startup ends long before the
                    // hard end, which counts it.
                    let by = Deadline::after(self.startup);
                    let sender = self.inbox.sender();
                    let (agent, event) = Agent::start(&self.sandboxes, self.workflow, by, sender);
                    self.agent = agent;
                    self.pending.push_back((self.clock.now(), event));
                }
                Action::Reply { op, reply } => {
                    let by = self.clock.deadline(self.answers_due(core));
                    self.reply(op, reply, by)?;
                }
                Action::StartJudge {
                    job,
                    call: _,
                    payload: _,
                    timeout: _,
                } => {
                    // This runtime runs no judge call: each fails as it starts.
                    let reason = "this runtime runs no judge call yet".to_owned();
                    let event = Event::JudgeFailed { job, reason };
                    self.pending.push_back((self.clock.now(), event));
                }
                // This runtime fails each job as it starts it, a failure the core takes as its
                // next event, so the core stops a job only as it takes the job's failure, at or
                // past the end of the work phase, which it checks before it applies an event.
                // Nothing runs to stop, and the failure is the job's last report.
                Action::StopJudge { job: _ } => {}
                Action::TearDownAgent => self.tear_down_agent()?,
                Action::Finish { outcome } => end = Some(Ok(outcome)),
                // Only a reply can follow it in its batch: the refusal of an op taken at the hard
                // end, by which its answer was due, so `answer` notes it unsent, and the op ends
                // with the agent's sandbox, which the run's end deletes.
                Action::Abandon => {
                    end = Some(Err(format!(
                        "the run was still going at its hard end, {} s after it started: \
                         expected it to end within its deadline, its finalize grace, its final \
                         judge's timeout and its sandboxes' startups",
                        self.hard_end().as_secs()
                    )));
                }
            }
        }
        Ok(end)
    }

    /// Answers op `op` with `reply`, by `by`. An op the core accepted has its payload stored
    /// first, since a judge call reads it, now or in the final phase; a refused op's bytes are
    /// never written.
    fn reply(&mut self, op: OpId, reply: OpReply, by: Deadline) -> Result<(), String> {
        #[expect(
            clippy::unreachable,
            reason = "the core answers every op it is given exactly once (`Core`), and the \
                      runtime gives it each op once, under a number that only this takes away; \
                      a type tying the reply to its op would carry the gateway's `IncomingOp` \
                      through the core, which does no I/O"
        )]
        let Some((_, unanswered)) = self.unanswered.take().filter(|(id, _)| *id == op) else {
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
        let what = format!("op {}'s reply", op.0);
        self.answer(&what, by, |agent, by| agent.reply(incoming, reply, by))
    }

    /// Sends an answer to an op of the agent's, `what`, by `by`, with `write`, unless `by` has
    /// passed or the agent's sandbox is gone. The run log notes an answer that isn't sent, or
    /// fails.
    fn answer(
        &mut self,
        what: &str,
        by: Deadline,
        write: impl FnOnce(&mut Agent, Deadline) -> io::Result<()>,
    ) -> Result<(), String> {
        let sent = match &mut self.agent {
            // A write past `by` would end the session, which the agent's teardown still needs,
            // for a delay of the host's own, such as hashing the payload, the log's sync or
            // storing the payload. The op stays in flight, which wedges the agent's ops, but
            // only until the core's next act: while the agent works, `by` is the end of the work
            // phase (`Core::tick_due`), so that act tears the agent down; after `Abandon`, the
            // run's end drops it.
            Some(_) if by.remaining().is_none() => Err(format!(
                "{what} wasn't sent: it was due by the core's next act, which has come"
            )),
            Some(agent) => write(agent, by).context(format_args!("{what} failed")),
            None => Err(format!("{what} wasn't sent: the agent's sandbox is gone")),
        };
        match sent {
            Ok(()) => Ok(()),
            Err(note) => self.log.note(self.clock.now(), &note),
        }
    }

    /// Collects what the agent's sandbox holds, then deletes it, even if collecting failed. A
    /// failure to write the record, or to delete the sandbox, is returned once both have been
    /// tried, joined if both fail.
    fn tear_down_agent(&mut self) -> Result<(), String> {
        let Some(mut agent) = self.agent.take() else {
            let note = "the agent's sandbox never started: nothing was collected";
            return self.log.note(self.clock.now(), note);
        };
        let collected = self.collect_agent(&mut agent);
        let deleted = agent.delete(&self.sandboxes);
        failed_also(collected, deleted)
    }

    /// Freezes the agent's sandbox and collects what it holds into the record's `agent`, by the
    /// teardown's end, and notes in the run log what was collected and what failed.
    fn collect_agent(&mut self, agent: &mut Agent) -> Result<(), String> {
        let dir = self.dir.join("agent");
        fs::create_dir(&dir).context(dir.display())?;
        let mut notes = agent.freeze_and_collect(&dir, self.clock.deadline(self.teardown_end()));
        let collected = record::collected(&dir);
        notes.push(format!("collected from the agent's sandbox: {collected}"));
        let at = self.clock.now();
        notes.iter().try_for_each(|note| self.log.note(at, note))
    }

    /// Deletes every sandbox the run's provider still lists of it, and notes which.
    fn sweep(&mut self) -> Result<(), String> {
        let deleted = self.sandboxes.sweep()?;
        if deleted.is_empty() {
            return Ok(());
        }
        let ids: Vec<&str> = deleted.iter().map(SandboxId::as_str).collect();
        let note = format!("deleted the sandboxes the run left: {}", ids.join(", "));
        self.log.note(self.clock.now(), &note)
    }
}
