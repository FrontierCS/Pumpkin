//! Pumpkin's host side, the runtime: runs a workflow on a [`Provider`], as the [core](decision)
//! decides, and keeps the run's record. An agent reaches its model through [`route`], which the
//! runtime serves.
//!
//! [`Run::prepare`] begins the record of a placed workflow, and [`Run::execute`] runs it to
//! its outcome, one thread driving the core as [`Core`] says, while each judge call runs on a
//! thread of its own, the `judge` module's. Between the core's ticks, the driving thread waits
//! on one inbox, into which the agent's session hands what it reads, and each judge call's
//! thread its reports. Its sandboxes start on one path, the `sandbox` module's, and the agent's
//! is the `agent` module's. Each model request of the agent's is answered on a thread of its
//! own, as the `model` module serves the agent's route, until the core stops allowing them
//! ([`Core::model_requests_allowed`]), which ends the route's exchanges. The record's files and
//! the run log's records, and when each is written, are laid out in the `record` and `log`
//! modules.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread::{self, Scope};
use std::time::{Duration, Instant, SystemTime};
use std::{fs, io, mem};

use decision::{Action, Core, Event, JobId, OpId, Outcome, PayloadRef, Request, lasts};
use gateway::{Arrival, IncomingModelRequest, IncomingOp, Op, RefusedOp};
use provider::Provider;
use route::{Answer, Serving};
use schema::provider::{RunTag, SandboxId};
use schema::sandbox::op::{OpReply, SubmitReply, SubmitState};
use schema::workflow::{JudgeCall, Placed, Workflow};
use serde_json::json;
use stdx::{Context, Deadline};

mod agent;
mod judge;
mod log;
mod model;
mod record;
mod sandbox;

use agent::Agent;
use judge::{Deadlines, Message, Report};
use log::Log;
use model::Served;
pub use record::{Ending, Record};
use record::{Lock, Payloads};
use sandbox::Sandboxes;

/// A run, prepared: its workflow placed, and its record begun.
#[derive(Debug)]
pub struct Run {
    placed: Placed,
    /// The agent's model route, as the run serves it, if it has one.
    served: Option<Served>,
    id: String,
    dir: PathBuf,
    /// Held until `outcome.json` is written: see [`Lock`].
    lock: Lock,
}

impl Run {
    /// Makes the record of `placed`'s run in `runs`, which is created if need be, with its IR
    /// in `ir.json`. The run's id holds `now`, the time by the caller's clock. The agent's model
    /// route, if it has one, is served as `serving` says, which goes unused for a workflow
    /// without one. A workflow whose route is not served is refused before any record is made.
    ///
    /// # Errors
    ///
    /// Fails if the workflow is refused, if `now` is before 1970, or if the record can't be
    /// written.
    pub fn prepare(
        placed: Placed,
        runs: &Path,
        now: SystemTime,
        serving: Option<Serving>,
    ) -> Result<Self, String> {
        let served = Served::new(placed.workflow(), serving)?;
        let (id, dir, lock) = record::create(runs, now)?;
        record::write_json(&dir, "ir.json", &json!(placed.workflow().ir()))?;
        Ok(Self {
            placed,
            served,
            id,
            dir,
            lock,
        })
    }

    /// The run's id, which names its record, and is the run tag its sandboxes carry.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The run's record.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Runs the workflow on `provider`, with `sandboxd`, a static binary, in every sandbox, and
    /// calls `started` once what they run is published, before the run's clock starts. However
    /// it ends, it records how in `outcome.json`, and deletes what `provider` still lists of it.
    ///
    /// # Errors
    ///
    /// Fails if the run can't be set up, recorded or cleaned up, if `started` fails, if it was
    /// still going at its hard end ([`lasts`]), or if the model provider refused the route's key
    /// before a final submission was accepted, or failed the agent's last request without one.
    pub fn execute(
        self,
        provider: &dyn Provider,
        sandboxd: &Path,
        started: impl FnOnce() -> Result<(), String>,
    ) -> Result<Outcome, String> {
        let outcome = self.run_on(provider, sandboxd, started);
        let recorded = match &outcome {
            Ok(outcome) => {
                let mut recorded = log::outcome_json(outcome);
                recorded["state"] = "finished".into();
                recorded
            }
            Err(reason) => json!({ "state": "failed", "reason": reason }),
        };
        let written = record::write_json(&self.dir, "outcome.json", &recorded);
        drop(self.lock);
        failed_also(outcome, written)
    }

    /// Records what `provider` declared and publishes what the run's sandboxes run, calls
    /// `started`, then starts the run's clock and drives the run, keeping its log, and deletes
    /// what the provider still lists of it.
    fn run_on(
        &self,
        provider: &dyn Provider,
        sandboxd: &Path,
        started: impl FnOnce() -> Result<(), String>,
    ) -> Result<Outcome, String> {
        record::write_json(&self.dir, "provider.json", &json!(provider.ready()))?;
        let workflow = self.placed.workflow();
        let sandboxes = Sandboxes::create(provider, self.tag(), workflow, sandboxd)?;
        started()?;
        let clock = Clock::start();
        let unix = record::since_epoch(clock.unix)?;
        let mut runtime = Runtime {
            placed: &self.placed,
            served: self.served.as_ref(),
            dir: &self.dir,
            sandboxes: &sandboxes,
            clock,
            log: Log::create(&self.dir)?,
            payloads: Payloads::create(&self.dir)?,
            agent: None,
            unanswered: None,
            next_op: 0,
            next_model_request: 0,
            finalized_first: false,
            stops: BTreeMap::new(),
            pending: VecDeque::new(),
            inbox: Inbox::new(),
        };
        let outcome = runtime.drive(unix);
        // What the agent did with a route whose key the provider refused before a finalize
        // locked the selection, or whose provider failed its last request as it exited, is the
        // deployment's failure, not a result of the agent's to score.
        let keyed = (self.served.as_ref()).map_or(Ok(()), |served| {
            served.upstream_held(runtime.finalized_first, &outcome)
        });
        let outcome = failed_also(failed_also(outcome, keyed), runtime.sweep());
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
            // A run tag built from a time and a number, which couldn't fail, would be a `pub`
            // item of the schema for this one caller.
            Err(error) => unreachable!(
                "run {}'s id is no run tag, though it is a UTC time's digits and `-`, then \
                 lowercase hexadecimal digits (`record::create`), all of which a run tag allows, \
                 and starts with a digit: {error}",
                self.id
            ),
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

/// Whether `actions` accept the agent's final submission, which locks the selection.
fn finalizes(actions: &[Action]) -> bool {
    actions.iter().any(|action| {
        matches!(
            action,
            Action::Reply {
                reply: OpReply::Submit(SubmitReply {
                    state: SubmitState::Accepted,
                    ..
                }),
                ..
            }
        )
    })
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

/// The state of a run as it is driven.
struct Runtime<'r> {
    /// The run's workflow, placed, which says how long each role's sandboxes take to start.
    placed: &'r Placed,
    /// The agent's model route, which model requests' threads share, if it has one.
    served: Option<&'r Served>,
    /// The run's record.
    dir: &'r Path,
    /// The run's sandboxes, and its only way to its provider, which judge calls' threads share.
    sandboxes: &'r Sandboxes<'r>,
    clock: Clock,
    log: Log,
    payloads: Payloads,
    agent: Option<Agent>,
    /// The op the core is to answer, by the number the runtime gave it, if one is: the agent's
    /// session hands out an op only once the one before it has been answered.
    unanswered: Option<(OpId, Unanswered)>,
    /// The number the next op takes, counting from 0.
    next_op: u64,
    /// The number the next model request takes, counting from 0.
    next_model_request: u64,
    /// Whether the core accepted the agent's final submission while the model provider had not
    /// refused the route's key ([`Served::upstream_held`]).
    finalized_first: bool,
    /// A sender into each judge call's inbox, by job, through which the core's stop reaches it.
    stops: BTreeMap<JobId, Sender<Message>>,
    /// Events that performing actions produced, to feed the core next.
    pending: VecDeque<(Duration, Event)>,
    /// What reaches the loop from other threads, which it waits on until the core's next tick.
    inbox: Inbox<Input>,
}

/// What reaches the loop through its inbox.
enum Input {
    /// What the agent's session's reader handed over ([`Listener::accept`]): at most the op in
    /// flight, as the session holds one, the model requests in flight, its command's exit, as it
    /// runs one command, and its end.
    ///
    /// [`Listener::accept`]: gateway::Listener::accept
    Agent(Arrival),
    /// A note on a model request of the agent's, from the thread that answers it, at the time
    /// the loop took the request.
    Model(Duration, String),
    /// How the route answered a model request of the agent's, by its number, from the thread
    /// that answered it, at the time the thread observed it.
    ModelAnswered(Duration, u64, Answer),
    /// A judge call's report, from its thread: at most its command's start and end, a note, and
    /// its last report.
    Judge(Report),
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

    // `std`'s channel reports a disconnection, which a channel with a sender never has, in the
    // same errors as a timeout and an empty channel.
    fn disconnected() -> ! {
        unreachable!(
            "an inbox disconnected, though it holds a sender of its own, so its channel can't \
             disconnect while the inbox is borrowed to wait on it"
        )
    }
}

impl<'r> Runtime<'r> {
    fn workflow(&self) -> &'r Workflow {
        self.placed.workflow()
    }

    /// Drives the run from its start, `unix` after the Unix epoch, until the core finishes or
    /// abandons it, and waits for its judge calls' threads, stopping those still running; then
    /// gives the core what is left in the inbox ([`Runtime::drain`]).
    fn drive(&mut self, unix: Duration) -> Result<Outcome, String> {
        let (mut core, actions) = Core::start(self.placed);
        self.log.start(self.workflow(), unix, &actions)?;
        if let Some(served) = self.served {
            self.log.note(self.clock.now(), served.started())?;
        }
        // Judge calls' threads borrow the run's workflow and sandboxes, and the scope ends once
        // each has, so that no sandbox starts after the run's end, and the drain takes all they
        // reported.
        let end = thread::scope(|scope| {
            let end = self.turns(scope, &mut core, actions);
            self.stop_jobs();
            // The route ended with the work phase, at the agent's teardown, unless the run ended
            // first: this ends it then. The scope's end still waits on its exchanges, within the
            // upstream's bounds and the responses' deadlines.
            self.end_route();
            end
        })?;
        // Only the core's end drains the inbox: after a failure of the runtime's own, such as a
        // record it couldn't write, the core may still be taking ops, and would decide on ones
        // that nothing acts on.
        failed_also(end, self.drain(&mut core))
    }

    /// Performs `actions`, the core's, then feeds the core each event and performs what it
    /// decides, until it ends the run: returns how, with its outcome or why it has none. Each
    /// judge call runs on a thread of `scope`.
    fn turns<'s>(
        &mut self,
        scope: &'s Scope<'s, 'r>,
        core: &mut Core<'_>,
        mut actions: Vec<Action>,
    ) -> Result<Result<Outcome, String>, String> {
        loop {
            if let Some(end) = self.perform(scope, core, actions)? {
                return Ok(end);
            }
            let (at, event) = self.next_event(scope, core)?;
            actions = core.handle(at, event.clone());
            // The key's refusal is read as the core locks the selection, before the record of it
            // is written, so that a refusal heard after the lock leaves the run to be scored.
            if let Some(served) = self.served
                && finalizes(&actions)
            {
                self.finalized_first = !served.key_was_refused();
            }
            self.log.event(self.workflow(), at, &event, &actions)?;
        }
    }

    /// The run's hard end, since it started, as the core has it.
    fn hard_end(&self) -> Duration {
        lasts(self.placed)
    }

    /// When the agent's teardown is to end, since the run started: the hard end, less the final
    /// judge's startup and timeout, so that the final judge still has both.
    fn teardown_end(&self) -> Duration {
        let score = self.workflow().score();
        let startup = self.placed.startup(score.slot.judge().role());
        let final_judge = startup.saturating_add(score.timeout);
        self.hard_end().saturating_sub(final_judge)
    }

    /// When the answer to an op is due: by the next time the core acts of itself
    /// ([`Core::tick_due`]).
    fn answers_due(&self, core: &Core<'_>) -> Deadline {
        // An answer the agent doesn't read then holds the loop no later than the time it would
        // act on next anyway.
        self.clock.deadline(core.tick_due())
    }

    /// Waits for the next event: one that performing an action produced; else what arrives in
    /// the inbox before the core's tick is due, and else the tick then. A due tick comes before
    /// what the inbox holds while the agent is held, and after it once the agent is gone. The
    /// wait ends at the tick's time, which arrivals the core doesn't wait on, handled as they
    /// come, don't move. The agent's model requests are answered on threads of `scope`.
    fn next_event<'s>(
        &mut self,
        scope: &'s Scope<'s, 'r>,
        core: &Core<'_>,
    ) -> Result<(Duration, Event), String> {
        if let Some(event) = self.pending.pop_front() {
            return Ok(event);
        }
        let until = self.clock.deadline(core.tick_due());
        loop {
            // The agent's arrivals are timed as the loop takes them, after a due tick, so the
            // tick comes first while the agent is held. Once it is gone, what the inbox holds is
            // a judge call's report, timed as its thread observed it, or what is left of the
            // agent's session, so it comes first: a call that ended before the tick, the final
            // judge's at the hard end included, counts before it, and one that ended after it
            // meets the core's checks at its own time, as the tick would. An op left of the
            // session is timed as taken, as every op is, so one taken past the hard end abandons
            // the run there, ahead of the reports from before it behind the op. Only a run that
            // would finish without a submission loses by it: with one, the loop is that late
            // only once the final judge can no longer meet its deadlines.
            let input = if self.agent.is_some() && until.remaining().is_none() {
                None
            } else {
                self.inbox.wait(until)
            };
            let Some(input) = input else {
                return Ok((self.clock.now(), Event::Tick));
            };
            if let Some(event) = self.arrived(Some(scope), core, input)? {
                return Ok(event);
            }
        }
    }

    /// The event that `input` brings the core, if the core waits on it: a judge call's report
    /// at its own time ([`Runtime::report`]), and else at the time the loop took it. The core
    /// learns of none of these: an op whose payload the gateway refused, which is refused by the
    /// core's next act ([`Runtime::answers_due`]) and goes to the run log; a model request,
    /// answered on a thread of `scope` ([`Runtime::model_request`]); and a note on one, or how
    /// it was answered, which go to the run log.
    fn arrived<'s>(
        &mut self,
        scope: Option<&'s Scope<'s, 'r>>,
        core: &Core<'_>,
        input: Input,
    ) -> Result<Option<(Duration, Event)>, String> {
        let at = self.clock.now();
        let arrival = match input {
            Input::Agent(arrival) => arrival,
            Input::Model(at, note) => return self.log.note(at, &note).map(|()| None),
            Input::ModelAnswered(at, number, answer) => {
                let answered = self
                    .log
                    .model_answered(self.workflow(), at, number, &answer);
                return answered.map(|()| None);
            }
            Input::Judge(report) => return self.report(report),
        };
        // Ops are taken as usual even once the agent's session has closed: the core has ended
        // the work phase, or the run, by then, so it refuses each, and the run log notes that
        // the answer wasn't sent.
        let event = match arrival {
            Arrival::Op(incoming) => Some(self.op(incoming)),
            Arrival::Refused(refused) => {
                let by = self.answers_due(core);
                self.refuse(at, refused, by)?;
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
            Arrival::ModelRequest(incoming) => {
                self.model_request(scope, core, at, incoming)?;
                None
            }
        };
        Ok(event.map(|event| (at, event)))
    }

    /// Takes `incoming`, a model request of the agent's, at `at`, under the next number, and
    /// answers it on a thread of `scope` as the route serves it ([`Served::serve`]), refused
    /// unless the core allows the agent's model requests then. One taken once the run's threads
    /// have ended, with no `scope`, is dropped unanswered, since the agent's session has ended
    /// by then ([`Runtime::drain`]).
    fn model_request<'s>(
        &mut self,
        scope: Option<&'s Scope<'s, 'r>>,
        core: &Core<'_>,
        at: Duration,
        incoming: IncomingModelRequest,
    ) -> Result<(), String> {
        let number = self.next_model_request;
        self.next_model_request += 1;
        // No type excludes a model request here, since one `Arrival` serves sessions with model
        // requests and without.
        let Some(served) = self.served else {
            unreachable!(
                "a model request arrived on the agent's session, though it takes them only if \
                 the run serves a route (`Agent::start`), and one breaks the protocol otherwise, \
                 which ends the session"
            )
        };
        let Some(scope) = scope else {
            drop(incoming);
            let note = format!("model request {number} wasn't answered: the run has ended");
            return self.log.note(at, &note);
        };
        let allowed = core.model_requests_allowed(at);
        // A response still open at its deadline ends the agent's session, which the teardown
        // needs, so it is due by the teardown's end; the work phase's end ends it before that
        // (`end_route`).
        let by = self.clock.deadline(self.teardown_end());
        let (reports, clock) = (self.inbox.sender(), self.clock);
        // A request stays in flight until its thread writes its response's end, the thread's
        // last step, so the session's bound on the requests in flight,
        // `MAX_MODEL_REQUESTS_IN_FLIGHT`, bounds these threads too, but for those writing that
        // end.
        let spawned = thread::Builder::new()
            .name(format!("model-{number}"))
            .spawn_scoped(scope, move || {
                // The loop's inbox outlives every request's thread, which the run's end waits
                // for. Both reports come before the answer's end, and so before what the agent
                // does next.
                let noted = |note| {
                    // At the take time, at which it was allowed, not when the thread gets to it.
                    let _ = reports.send(Input::Model(at, note));
                };
                let answered = |answer| {
                    let _ = reports.send(Input::ModelAnswered(clock.now(), number, answer));
                };
                served.serve(number, incoming, allowed, by, noted, answered);
            });
        match spawned {
            Ok(_) => Ok(()),
            Err(error) => {
                let note = format!(
                    "model request {number} wasn't answered: its thread didn't start: {error}"
                );
                self.log.note(at, &note)
            }
        }
    }

    /// The event that `report`, a judge call's, brings the core, if it has one, at the time the
    /// call's thread observed it; a note goes to the run log at that time. A sandbox that
    /// couldn't be deleted fails the run.
    fn report(&mut self, report: Report) -> Result<Option<(Duration, Event)>, String> {
        match report {
            Report::Event(at, event) => Ok(Some((at, event))),
            Report::Note(at, note) => self.log.note(at, &note).map(|()| None),
            Report::Undeleted(reason) => Err(reason),
        }
    }

    /// Gives the core `incoming`, an op of the agent's: the event of its arrival, under the next
    /// number. Its payload is stored only once the core accepts it ([`Runtime::reply`]).
    fn op(&mut self, incoming: IncomingOp) -> Event {
        let (request, unanswered) = request(incoming);
        let op = OpId(self.next_op);
        self.next_op += 1;
        // The session's one op in flight is a rule of the protocol behind its `pub` API, which
        // a type showing it would reshape.
        if let Some((earlier, _)) = self.unanswered.replace((op, unanswered)) {
            unreachable!(
                "op {} arrived while op {} was unanswered, though the agent's session hands out \
                 an op only once the one before it has been answered, by `Session::reply`, which \
                 only `Runtime::reply` reaches, taking that op from here, or by \
                 `Session::refuse`, for an op that never comes here",
                op.0, earlier.0
            )
        }
        Event::OpArrived { op, request }
    }

    /// Refuses `refused`, an op of the agent's whose payload the gateway refused, taken at `at`,
    /// by `by`, and records it in the run log.
    fn refuse(&mut self, at: Duration, refused: RefusedOp, by: Deadline) -> Result<(), String> {
        self.log.refused(self.workflow(), at, &refused)?;
        let what = "the refusal of an op whose payload was over the agent's limit";
        self.answer(what, by, |agent, by| agent.refuse(refused, by))
    }

    /// Takes what is left in the inbox once the core has finished or abandoned the run, and every
    /// judge call's thread has ended, as the loop does ([`Runtime::arrived`]): the core refuses
    /// each op, as it does every op after its end, and the run log notes that the refusal wasn't
    /// sent.
    fn drain(&mut self, core: &mut Core<'_>) -> Result<(), String> {
        // Its teardown has dropped the agent's session, unless the run was abandoned first, and
        // this drops it then, its sandbox left to the sweep: either way, its reader has handed
        // over all it will by now.
        self.agent = None;
        while let Some(input) = self.inbox.take() {
            let Some((at, event)) = self.arrived(None, core, input)? else {
                continue;
            };
            let actions = core.handle(at, event.clone());
            self.log.event(self.workflow(), at, &event, &actions)?;
            for action in actions {
                match action {
                    Action::Reply { op, reply } => {
                        let by = self.answers_due(core);
                        self.reply(op, reply, by)?;
                    }
                    // A type excluding these actions here would split the core's actions by the
                    // run's phase.
                    other @ (Action::StartAgent
                    | Action::StartJudge { .. }
                    | Action::StopJudge { .. }
                    | Action::TearDownAgent
                    | Action::Finish { .. }
                    | Action::Abandon) => {
                        unreachable!(
                            "the core asked for {other:?} once it had ended the run, though \
                             after `Finish` or `Abandon` it only refuses the ops that still \
                             arrive, as `Action::Finish` and `Action::Abandon` say"
                        )
                    }
                }
            }
        }
        Ok(())
    }

    /// Performs the core's actions, in order, starting judge calls on threads of `scope`, and
    /// returns how the run ended, if one of them ended it: with its outcome, or why it has none.
    fn perform<'s>(
        &mut self,
        scope: &'s Scope<'s, 'r>,
        core: &Core<'_>,
        actions: Vec<Action>,
    ) -> Result<Option<Result<Outcome, String>>, String> {
        let mut end = None;
        for action in actions {
            match action {
                Action::StartAgent => {
                    // The agent starts at the run's start, so its startup ends long before the
                    // hard end, which counts it.
                    let by = Deadline::after(self.placed.startup(self.workflow().agent().role));
                    let (sender, model) = (self.inbox.sender(), self.served.is_some());
                    let (agent, event) =
                        Agent::start(self.sandboxes, self.workflow(), model, by, sender);
                    self.agent = agent;
                    self.pending.push_back((self.clock.now(), event));
                }
                Action::Reply { op, reply } => {
                    let by = self.answers_due(core);
                    self.reply(op, reply, by)?;
                }
                Action::StartJudge {
                    job,
                    call,
                    payload,
                    timeout,
                } => self.start_judge(scope, core, job, call, &payload, timeout)?,
                Action::StopJudge { job } => {
                    // A type tying the stop to the start would carry the runtime's sender through
                    // the core, which holds no runtime state.
                    let Some(stop) = self.stops.get(&job) else {
                        unreachable!(
                            "job {} was stopped, but never started, though the core stops only \
                             a job it has started, and the `StartJudge` that started it, \
                             performed before, kept a sender into its call's inbox, even if the \
                             call then failed as it started",
                            job.0
                        )
                    };
                    // A call whose thread has ended has nothing left to stop.
                    let _ = stop.send(Message::Stop);
                }
                Action::TearDownAgent => {
                    // The route ends with the work phase, first, so that its exchanges end while
                    // the teardown still has the agent's session.
                    self.end_route();
                    self.tear_down_agent()?;
                }
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
        // A type tying the reply to its op would carry the gateway's `IncomingOp` through the
        // core, which does no I/O.
        let Some((_, unanswered)) = self.unanswered.take().filter(|(id, _)| *id == op) else {
            unreachable!(
                "op {} was answered twice, or never given to the core, though the core answers \
                 every op it is given exactly once (`Core`), and the runtime gives it each op \
                 once, under a number that only this takes away",
                op.0
            )
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

    /// Starts job `job`, a judge call of `call` on `payload` whose command runs for at most
    /// `timeout`, on a thread of `scope`'s, which reports to the loop. The job fails, with
    /// nothing started, if its start is due by a time that has passed, or if the thread doesn't
    /// start.
    fn start_judge<'s>(
        &mut self,
        scope: &'s Scope<'s, 'r>,
        core: &Core<'_>,
        job: JobId,
        call: JudgeCall,
        payload: &PayloadRef,
        timeout: Duration,
    ) -> Result<(), String> {
        let deadlines = self.judge_deadlines(core, call, timeout);
        // The core may stop a job before it takes the job's failure to start, so the job's
        // sender is kept first.
        let inbox = Inbox::new();
        self.stops.insert(job, inbox.sender());
        // The loop can be held past a call's start, by the agent's teardown's delete or a sync of
        // the run log, and a sandbox created for a start already due would only be deleted, at
        // or past the hard end at worst.
        if deadlines.startup.remaining().is_none() {
            let reason = "the judge's sandbox wasn't created: the call's start was due by a time \
                          that had passed"
                .to_owned();
            let event = Event::JudgeFailed { job, reason };
            self.pending.push_back((self.clock.now(), event));
            return Ok(());
        }
        let slot = self.workflow().call_slot(call);
        let jobs = self.dir.join("jobs");
        fs::create_dir_all(&jobs).context(jobs.display())?;
        let dir = jobs.join(job.0.to_string());
        fs::create_dir(&dir).context(dir.display())?;
        let call = judge::Call {
            job,
            judge: self.workflow().judge(slot.judge()),
            sandbox: self.workflow().sandbox(slot.judge().role()),
            slot: &self.workflow().slot(slot).name,
            payload: self.payloads.path(&payload.digest),
            timeout,
            deadlines,
            dir,
            sandboxes: self.sandboxes,
            clock: self.clock,
            inbox,
            reports: self.inbox.sender(),
        };
        let spawned = thread::Builder::new()
            .name(format!("job-{}", job.0))
            .spawn_scoped(scope, move || judge::run(&call));
        if let Err(error) = spawned {
            let reason = format!("the call's thread didn't start: {error}");
            let event = Event::JudgeFailed { job, reason };
            self.pending.push_back((self.clock.now(), event));
        }
        Ok(())
    }

    /// By when the steps of a judge call of `call`, whose command runs for at most `timeout`,
    /// starting now, are to be done.
    fn judge_deadlines(&self, core: &Core<'_>, call: JudgeCall, timeout: Duration) -> Deadlines {
        let judge = self.workflow().call_slot(call).judge().role();
        let startup = self.clock.now().saturating_add(self.placed.startup(judge));
        let at = |time| self.clock.deadline(time);
        match call {
            // A call of the agent's ends with the work phase, which ends by the core's next act
            // of its own, and what it left is collected by the agent's teardown's end, which
            // leaves the final judge its share of the hard end.
            JudgeCall::Grant(_) => {
                let start = at(startup.min(core.tick_due()));
                Deadlines {
                    startup: start,
                    start,
                    collect: at(self.teardown_end()),
                }
            }
            // The final judge's command starts in time to run its whole timeout before the hard
            // end, by which what it left is collected.
            JudgeCall::Score => {
                let start = self.hard_end().saturating_sub(timeout);
                Deadlines {
                    startup: at(startup.min(start)),
                    start: at(start),
                    collect: at(self.hard_end()),
                }
            }
        }
    }

    /// Ends the agent's model route, if it has one, with its every exchange in flight and every
    /// later one ([`Served::end`]). Ending it again does nothing.
    fn end_route(&self) {
        if let Some(served) = self.served {
            served.end();
        }
    }

    /// Stops every judge call's thread that still runs, as the run's end does.
    fn stop_jobs(&self) {
        for stop in self.stops.values() {
            // A call whose thread has ended has nothing left to stop.
            let _ = stop.send(Message::Stop);
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
        let deleted = agent.delete(self.sandboxes);
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
