//! Pumpkin's host side, the runtime: runs a workflow on the [`Provider`]s its roles are placed
//! on, as the [core](decision) decides, and keeps the run's record. A role with a model route,
//! the agent or a judge, reaches its model through [`route`], which the runtime serves.
//!
//! [`Run::prepare`] begins the record of a placed workflow, and [`Run::execute`] runs it to its
//! outcome, or until a signal interrupts it ([`Stopped::Interrupted`]), one thread driving the core
//! as [`Core`] says, while each child of the run, a judge call or a lease, runs on a thread of its
//! own, the `child` module's, which the `judge` or the `lease` module drives. Between the core's
//! ticks, the driving thread waits on one inbox of the run's [`Host`], into which the agent's
//! session hands what it reads, and each child's thread its reports, a judge's call's ops and model
//! requests among them, which the loop answers, an op through the call's thread. Its sandboxes
//! start on one path, the `sandbox` module's, which ends a start once its sandbox has ended, and
//! the agent's is the `agent` module's. Each model request is answered on a thread of its own, as
//! the `model` module serves its sandbox's route, until the core stops allowing its caller's
//! ([`Core::model_requests_allowed`]), which ends the route's exchanges. The record's files and the
//! run log's records, and when each is written, are laid out in the `record` and `log` modules.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::thread::{self, Scope};
use std::time::Duration;
use std::{fs, io, mem};

use decision::{
    Action, Caller, Core, Event, JobId, LeaseId, OpId, Outcome, PayloadRef, Request, lasts,
};
use gateway::{
    Arrival, Dialer, GrantListener, IncomingModelRequest, IncomingOp, Op, RefusedOp, Trees,
};
use provider::{Provider, Reservation, RunTag, UnixTime};
use route::Serving;
use schema::ir::Image;
use schema::sandbox::op::{OpReply, SubmitReply, SubmitState};
use schema::workflow::{ByProvider, ByRole, JudgeCall, LeaseGrantId, Placed, RoleId, Workflow};
use serde_json::json;
use stdx::{Context, Deadline};

mod agent;
mod child;
mod host;
mod interruption;
mod judge;
mod lease;
mod log;
mod model;
mod record;
mod sandbox;

use agent::Agent;
use child::{Child, Deadlines, Message, Report};
use host::deadline;
pub use host::{Clock, Durable, Host, Inbox, System, SystemClock, SystemInbox};
pub use interruption::{Signal, Stopped};
pub use log::{
    AgentEnd, AgentSpan, CallEnd, CallSpan, Closed, LeaseCommand, LeaseSpan, ModelUse, Place,
    PlaceUse, Progress, Sandbox, Tokens, Work,
};
use log::{Entry, Log};
use model::{Exchange, Served};
pub use record::{BUILD, Build, Ending, Reaped, Record, ir_json, minted, named, new_id};
use record::{Lock, Payloads};
use sandbox::{Published, Sandboxes, Which};

/// The longest the loop waits on its inbox before it looks for an interruption again.
// A stop that comes a twentieth of a second late goes unnoticed by the person who asked for it,
// and the loop wakes for it at most 20 times a second.
const LOOK: Duration = Duration::from_millis(50);

/// What a run's record keeps of how the run was made.
#[derive(Clone, Copy)]
pub struct Recorded<'a> {
    /// The arguments of the `pumpkin run` that started the run.
    pub args: &'a [String],
    /// The tag of each image's build, by the image's ID.
    pub tags: &'a BTreeMap<Image, String>,
}

/// A run, prepared on its host, `H`: its workflow placed, on the providers its roles are placed
/// on, and its record begun.
pub struct Run<'r, H> {
    host: &'r H,
    placed: Placed,
    /// Each provider that a role is placed on, by its name.
    providers: ByProvider<&'r dyn Provider>,
    /// Each role's model route, as [`model::routes`] gives it.
    routes: ByRole<Vec<Serving>>,
    id: RunTag,
    /// The tag of each image's build, by the image's ID, for `provider.json`.
    tags: &'r BTreeMap<Image, String>,
    dir: PathBuf,
    /// Held until `outcome.json` is written: see [`Lock`].
    lock: Lock,
    /// What each provider holds for the run, held until the run's end has deleted what each
    /// lists of it.
    reserved: ByProvider<Reservation>,
}

impl<'r, H: Host> Run<'r, H> {
    /// Makes the record of `placed`'s run in `runs`, made if need be, on `host`, named by `id`
    /// ([`new_id`]), once each of `providers`, those its roles are placed on, has reserved what
    /// they need until the run's end ([`Provider::reserve`]); keeps what was `recorded`, and
    /// serves each role's route as `servings` do.
    ///
    /// # Errors
    ///
    /// Fails, before any record is made, if a route has no serving or a provider can't reserve;
    /// or if `runs` isn't safe from other users, `id` is taken, or the record can't be written.
    pub fn prepare(
        host: &'r H,
        placed: Placed,
        providers: ByProvider<&'r dyn Provider>,
        runs: &Path,
        id: RunTag,
        servings: &ByRole<Vec<Serving>>,
        recorded: Recorded<'r>,
    ) -> Result<Self, String> {
        let Recorded { args, tags } = recorded;
        let routes = model::routes(placed.workflow(), servings)?;
        let reserved = placed.by_provider(|name| {
            let phases = sandbox::phases(&placed, name);
            (providers.get(name).reserve(&id, &phases))
                .context(format_args!("reserving on provider `{name}`"))
        })?;
        let (dir, lock) = record::create(host, runs, id.as_str(), args)?;
        record::write_ir(host, &dir, placed.workflow())?;
        Ok(Self {
            host,
            placed,
            providers,
            routes,
            id,
            tags,
            dir,
            lock,
            reserved,
        })
    }

    /// The run's id, which names its record, and is the run tag its sandboxes carry.
    pub fn id(&self) -> &str {
        self.id.as_str()
    }

    /// The run's record.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Runs the workflow on its roles' providers, with the static `sandboxd` in each sandbox, calling `started` once that is published; a private provider's
    /// sandboxes meet the gateway at `private`, a listening one's through `dialer`. Records its
    /// end in `outcome.json`, and deletes what each provider lists of it.
    ///
    /// # Errors
    ///
    /// Stops once a signal comes ([`Stopped::Interrupted`]); fails if it can't be set up, recorded
    /// or cleaned up, if `started` fails, past its hard end ([`lasts`]), or as `routes_held` says.
    pub fn execute(
        self,
        sandboxd: &Path,
        private: Option<&GrantListener>,
        dialer: &Dialer,
        started: impl FnOnce() -> Result<(), String>,
    ) -> Result<Outcome, Stopped> {
        let outcome = self.run_on(sandboxd, (private, dialer), started);
        // What the run used, as its log, synced, says; none if the run never began its log.
        let (outcome, usage) = match log::progress(&self.dir) {
            Ok(progress) => (outcome, progress.as_ref().map(log::usage_of)),
            Err(error) => (failed_also(outcome, Err(error)), None),
        };
        let mut recorded = match &outcome {
            Ok(outcome) => {
                let mut recorded = log::outcome_json(outcome);
                recorded["state"] = "finished".into();
                recorded
            }
            Err(Stopped::Failed(reason)) => json!({ "state": "failed", "reason": reason }),
            Err(Stopped::Interrupted(signal)) => {
                json!({ "state": "interrupted", "signal": signal.name() })
            }
        };
        if let Some(usage) = usage {
            recorded["usage"] = usage;
        }
        let written = record::write_json(self.host, &self.dir, "outcome.json", &recorded);
        // Before the lock, which `wait` follows, so that a run that `wait` saw end holds nothing.
        drop(self.reserved);
        drop(self.lock);
        failed_also(outcome, written)
    }

    /// Records what each provider declared, and where each role is placed, and publishes what
    /// the run's sandboxes run, calls `started` unless a signal has come first, then starts the
    /// run's clock on its host and drives the run, keeping its log, deletes what each provider
    /// still lists of it, and syncs the log.
    fn run_on(
        &self,
        sandboxd: &Path,
        (private, dialer): (Option<&GrantListener>, &Dialer),
        started: impl FnOnce() -> Result<(), String>,
    ) -> Result<Outcome, Stopped> {
        let host = self.host;
        let provided = record::provided(&self.placed, &self.providers, self.tags);
        record::write_json(host, &self.dir, "provider.json", &provided)?;
        let published = Published::create(
            &self.providers,
            &self.placed,
            sandboxd,
            private,
            dialer,
            &self.dir,
        )?;
        // A run interrupted as it publishes never starts, so nothing tells of a start that the
        // signal then undoes.
        interruption::check()?;
        started()?;
        let clock = host.start();
        let unix = UnixTime::of(clock.unix())?;
        let inbox = clock.inbox();
        let sandboxes = Sandboxes::new(
            published,
            self.id.clone(),
            &self.placed,
            &self.dir,
            host,
            inbox.sender(),
        );
        let log = Log::create(host, clock.clone(), &self.dir)?;
        let mut runtime = Runtime {
            placed: &self.placed,
            routes: &self.routes,
            served: BTreeMap::new(),
            dir: &self.dir,
            sandboxes: &sandboxes,
            clock,
            unix_hard_end: unix.saturating_add(lasts(&self.placed)),
            log,
            payloads: Payloads::create(host, &self.dir)?,
            agent: None,
            unanswered: BTreeMap::new(),
            next_op: 0,
            next_model_request: 0,
            held_at_lock: BTreeSet::new(),
            jobs: BTreeMap::new(),
            leases: BTreeMap::new(),
            pending: VecDeque::new(),
            inbox,
            held: None,
        };
        let outcome = runtime.drive(unix);
        // What a sandbox did with a route whose key the provider refused before a finalize
        // locked the selection, or whose provider failed the agent's last request as it exited,
        // is the deployment's failure, not a result of the agent's to score.
        let keyed = runtime.routes_held(&outcome);
        let outcome = failed_also(failed_also(outcome, keyed), runtime.sweep());
        let outcome = failed_also(outcome, runtime.settle());
        let noted = match &outcome {
            // An interruption is noted as the loop heeds it (`Runtime::drive`).
            Ok(_) | Err(Stopped::Interrupted(_)) => Ok(()),
            Err(failed @ Stopped::Failed(_)) => runtime.log.note(&format!("the run {failed}")),
        };
        // The log's last records may have led to no act, and so be unsynced: they last before
        // `outcome.json` says the run has ended, or the run fails.
        let synced = runtime.log.sync();
        failed_also(failed_also(outcome, noted), synced)
    }
}

/// `outcome`, failed also for `then`'s error, if it has one: both reasons are kept.
fn failed_also<T, E: Failure>(outcome: Result<T, E>, then: Result<(), String>) -> Result<T, E> {
    match (outcome, then) {
        (outcome, Ok(())) => outcome,
        (Ok(_), Err(error)) => Err(E::from(error)),
        (Err(stopped), Err(error)) => Err(stopped.and(&error)),
    }
}

/// Why something stopped, which a later failure joins ([`failed_also`]).
trait Failure: From<String> {
    /// Why it stopped, failed also for `error`.
    fn and(self, error: &str) -> Self;
}

impl Failure for String {
    fn and(self, error: &str) -> Self {
        format!("{self}; {error}")
    }
}

impl Failure for Stopped {
    fn and(self, error: &str) -> Self {
        Self::Failed(match self {
            Self::Failed(reason) => format!("{reason}; {error}"),
            Self::Interrupted(signal) => format!("{} interrupted the run; {error}", signal.name()),
        })
    }
}

/// Whether `reply` accepts the call it answers.
fn accepts(reply: &OpReply) -> bool {
    match reply {
        OpReply::Judge(_) | OpReply::Submit(_) => true,
        OpReply::Ops(_)
        | OpReply::Status(_)
        | OpReply::Lease(_)
        | OpReply::LeaseStatus(_)
        | OpReply::Refused { .. } => false,
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

/// An op, awaiting the core's reply.
enum Unanswered {
    /// An op that carries no payload: `ops`, `status`, or a lease op.
    Query(IncomingOp),
    /// A judge call or a submission, with its payload, taken out of the op, and the payload's
    /// reference.
    Call {
        incoming: IncomingOp,
        payload: Vec<u8>,
        reference: PayloadRef,
    },
}

impl Unanswered {
    /// The op, its payload, if it carried one, dropped.
    fn into_incoming(self) -> IncomingOp {
        match self {
            Self::Query(incoming) | Self::Call { incoming, .. } => incoming,
        }
    }
}

/// A judge call, as the loop reaches it.
struct Called {
    /// A sender into its inbox, through which the core's stop, and its replies to the call's
    /// ops, reach it.
    orders: Sender<Message<OpReply>>,
    /// When what it left is to be collected by, since the run started, which its leases' are too.
    collect: Duration,
    /// The trees of its sandbox, which its last op came with, until a transfer it asked for takes
    /// them.
    trees: Option<Trees>,
    /// Its judge's role, whose model route, if it has one, the call is served once its command
    /// starts.
    judge: RoleId,
}

/// Who answers an op the core is to reply to.
enum Asked {
    /// The loop, on the agent's session.
    Agent(Unanswered),
    /// The thread of the job's call, which holds the op, once the reply reaches it.
    Job(JobId),
}

/// What `incoming`, an op, asks the core, and the op, to answer once the core replies.
#[expect(clippy::too_many_lines, reason = "one arm per op")]
fn request(mut incoming: IncomingOp) -> (Request, Unanswered) {
    // The request takes the op's fields, and what stays of the op only answers it.
    let (request, call) = match mem::replace(&mut incoming.op, Op::Ops) {
        Op::Judge { grant, payload } => {
            let reference = record::reference(&payload);
            let request = Request::Judge {
                grant,
                payload: reference.clone(),
            };
            (request, Some((payload, reference)))
        }
        Op::Submit {
            grant,
            finalize,
            payload,
        } => {
            let reference = record::reference(&payload);
            let request = Request::Submit {
                grant,
                finalize,
                payload: reference.clone(),
            };
            (request, Some((payload, reference)))
        }
        Op::Ops => (Request::Ops, None),
        Op::Status { job } => (Request::Status { job }, None),
        Op::LeaseOpen {
            grant,
            instance_seconds,
        } => (
            Request::LeaseOpen {
                grant,
                instance_seconds,
            },
            None,
        ),
        Op::LeaseRun {
            lease,
            argv,
            timeout_seconds,
        } => (
            Request::LeaseRun {
                lease,
                argv,
                timeout_seconds,
            },
            None,
        ),
        Op::LeasePut {
            lease,
            tree,
            to,
            replace,
        } => (
            Request::LeasePut {
                lease,
                tree,
                to,
                replace,
            },
            None,
        ),
        Op::LeaseGet { lease, from, to } => (Request::LeaseGet { lease, from, to }, None),
        Op::LeaseStatus { lease } => (Request::LeaseStatus { lease }, None),
        Op::LeaseClose { lease } => (Request::LeaseClose { lease }, None),
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

/// The state of a run as it is driven on its host, `H`.
struct Runtime<'r, H: Host> {
    /// The run's workflow, placed, which says how long each role's sandboxes take to start.
    placed: &'r Placed,
    /// Each role's model route, as [`model::routes`] gives it.
    routes: &'r ByRole<Vec<Serving>>,
    /// The model route of each sandbox that the run serves one, by its caller: the agent's, and
    /// each call's of a judge with a route, each shared with its model requests' threads.
    served: BTreeMap<Caller, Arc<Served>>,
    /// The run's record.
    dir: &'r Path,
    /// The run's sandboxes, and its only way to its provider, which judge calls' threads share.
    sandboxes: &'r Sandboxes<'r, H>,
    clock: H::Clock,
    /// The run's hard end, as each of its sandboxes' creates takes it
    /// ([`Create::hard_end`](provider::Create::hard_end)).
    unix_hard_end: UnixTime,
    log: Log<'r, H>,
    payloads: Payloads<'r, H>,
    agent: Option<Agent>,
    /// Each op the core is to answer, by the number the runtime gave it: at most one of each
    /// sandbox's, whose session hands out an op only once the one before it has been answered.
    unanswered: BTreeMap<OpId, Asked>,
    /// The number the next op takes, counting from 0.
    next_op: u64,
    /// The number the next model request takes, counting from 0.
    next_model_request: u64,
    /// The callers whose routes' keys no model provider had refused when the core accepted the
    /// agent's final submission, which locks the selection, if it did ([`Runtime::routes_held`]).
    held_at_lock: BTreeSet<Caller>,
    /// Each judge call, by job.
    jobs: BTreeMap<JobId, Called>,
    /// A sender into each lease's inbox, by lease, through which the core's orders reach it.
    leases: BTreeMap<LeaseId, Sender<Message<lease::Order>>>,
    /// Events that performing actions produced, to feed the core next.
    pending: VecDeque<(Duration, Event)>,
    /// What reaches the loop ([`Input`]), which it waits on until the core's next tick.
    inbox: <H::Clock as Clock>::Inbox<Input>,
    /// What the loop took from the inbox once the core's tick was due, which reached the host at
    /// or after the tick's time, and so goes to the core after the tick.
    held: Option<Input>,
}

/// What reaches the loop through its inbox: what the core decides on, each at the time it reached
/// the host ([`Input::reached`]), however long the loop takes to get to it, and what the run log
/// alone takes, which the loop writes as it gets to it.
enum Input {
    /// What the agent's session's reader handed over ([`Registration::accept`]), when it did: at
    /// most the op in flight, as the session holds one, the model requests in flight, its
    /// command's exit, as it runs one command, and its end.
    ///
    /// [`Registration::accept`]: gateway::Registration::accept
    Agent(Duration, Arrival),
    /// A child's report, from its thread, or from the loop for a child whose thread didn't
    /// start: a judge call's command's start and end, a lease's readiness and its commands'
    /// ends, and its last report.
    Child(Report),
    /// A record for the run log alone, from the thread that observed it: a note, such as a
    /// child's or one on a model request from the thread that answers it; how a route answered a
    /// model request ([`Exchange::kept`]); the GPUs a sandbox saw; or a sandbox's delete.
    Logged(Entry),
}

impl Input {
    /// When it reached the host, as a time since the run started, if the core decides on it by
    /// that time: what the agent's session handed over, and a child's event, op or model request;
    /// not what is for the run log alone, or a child's sandbox that couldn't be deleted, which
    /// fails the run.
    fn reached(&self) -> Option<Duration> {
        match self {
            Self::Agent(at, _)
            | Self::Child(
                Report::Event(at, _) | Report::Op { at, .. } | Report::Model { at, .. },
            ) => Some(*at),
            Self::Child(Report::Undeleted(_)) | Self::Logged(_) => None,
        }
    }
}

impl<'r, H: Host + 'r> Runtime<'r, H> {
    fn workflow(&self) -> &'r Workflow {
        self.placed.workflow()
    }

    /// Drives the run from its start, `unix` after the Unix epoch, until the core finishes or
    /// abandons it, or a signal interrupts it, and waits for its judge calls' threads, stopping
    /// those still running; then, once the core has ended, gives it what is left in the inbox
    /// ([`Runtime::drain`]), and else logs what the run log alone takes of it
    /// ([`Runtime::settle`]).
    fn drive(&mut self, unix: UnixTime) -> Result<Outcome, Stopped> {
        let (mut core, actions) = Core::start(self.placed);
        self.log
            .start(self.workflow(), unix, self.hard_end(), &actions)?;
        self.serve(Caller::Agent, self.workflow().agent().role);
        for (role, sandbox) in self.workflow().roles() {
            if let Some(note) = model::started(sandbox, self.routes.get(role)) {
                self.log.note(&note)?;
            }
        }
        // Judge calls' threads borrow the run's workflow and sandboxes, and the scope ends once
        // each has, so that no sandbox starts after the run's end, and the drain takes all they
        // reported. A stop waits so for a child whose provider's create is under way, which
        // nothing cuts short but its own deadline: a sandbox that a create made after the sweep's
        // list would outlive the run, until its hard end, so the create's return, and the start's
        // delete of what it made, come before the sweep.
        let end = thread::scope(|scope| {
            let end = match self.turns(scope, &mut core, actions) {
                // As the loop heeds it, so that the record of each sandbox's end after it tells
                // how long the stop took.
                Err(interrupted @ Stopped::Interrupted(_)) => {
                    let noted = self.log.note(&format!("the run {interrupted}"));
                    failed_also(Err(interrupted), noted)
                }
                end => end,
            };
            self.stop_children();
            // Each route ended once the core stopped allowing its model requests, unless the run
            // ended first: this ends them then. The scope's end still waits on their exchanges,
            // within the upstreams' bounds and the responses' deadlines.
            self.end_routes(None);
            end
        });
        // Only the core's end drains the inbox into the core: after a failure of the runtime's
        // own, such as a record it couldn't write, or an interruption, the core may still be
        // taking ops, and would decide on ones that nothing acts on; the run log still takes
        // what the run's children noted and deleted as they stopped.
        match end {
            Ok(end) => failed_also(end.map_err(Stopped::Failed), self.drain(&mut core)),
            Err(stopped) => failed_also(Err(stopped), self.settle()),
        }
    }

    /// Performs `actions`, the core's, then feeds the core each event and performs what it
    /// decides, until it ends the run: returns how, with its outcome or why it has none. Each
    /// judge call runs on a thread of `scope`.
    fn turns<'s>(
        &mut self,
        scope: &'s Scope<'s, 'r>,
        core: &mut Core<'_>,
        mut actions: Vec<Action>,
    ) -> Result<Result<Outcome, String>, Stopped> {
        loop {
            if let Some(end) = self.perform(scope, core, actions)? {
                return Ok(end);
            }
            let (at, event) = self.next_event(scope, core)?;
            actions = core.handle(at, event.clone());
            // A call's route is open from its command's start, as the agent's is from the run's.
            if let Event::JudgeStarted { job } = event {
                let judge = self.call(job).judge;
                self.serve(Caller::Job(job), judge);
            }
            // The keys' refusals are read as the core locks the selection, before the record of
            // it is written, so that a refusal heard after the lock leaves the run to be scored.
            if finalizes(&actions) {
                let held = self
                    .served
                    .iter()
                    .filter(|(_, served)| served.key_refused_by().is_none());
                self.held_at_lock = held.map(|(caller, _)| *caller).collect();
            }
            self.log.event(self.workflow(), at, &event, &actions)?;
            // A route ends as the core stops allowing its model requests, the agent's at the work
            // phase's end, a call's at its command's, before the core's actions are performed:
            // its exchanges in flight then end while the agent's teardown, if the actions hold
            // it, still needs the agent's session. The event that first stops the agent's ends
            // the work phase or the run, so its record lists the agent's teardown or the run's
            // abandon, and is synced before the route ends too.
            self.end_routes(Some((core, at)));
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

    /// Waits for the next event: one that performing an action produced; else what arrives in the
    /// inbox before the core's tick is due, and else the tick then. Once the tick is due, what
    /// reached the host before its time comes before it, and the tick before what reached the host
    /// at or after it. The wait ends at the tick's time, which arrivals the core doesn't wait on,
    /// handled as they come, don't move. Model requests are answered on threads of `scope`. Stops,
    /// with nothing more taken, once a signal has come.
    fn next_event<'s>(
        &mut self,
        scope: &'s Scope<'s, 'r>,
        core: &Core<'_>,
    ) -> Result<(Duration, Event), Stopped> {
        interruption::check()?;
        if let Some(event) = self.pending.pop_front() {
            return Ok(event);
        }
        let due = core.tick_due();
        loop {
            let input = self.held.take().or_else(|| {
                // Waited in slices, so that a signal ends the wait within `LOOK`.
                let slice = due.min(self.clock.now().saturating_add(LOOK));
                self.inbox.wait(slice)
            });
            // Before what arrived is taken, so that the core takes nothing once a signal has come.
            interruption::check()?;
            let Some(input) = input else {
                if self.clock.now() < due {
                    continue;
                }
                return Ok((self.clock.now(), Event::Tick));
            };
            // Once the tick's time has come, what reaches the host is stamped at or after it, so
            // taking what came first ends, however much the agent sends that the core doesn't
            // wait on. What reached the host before that time is in the inbox by then, but for an
            // input whose sender was preempted between stamping and sending it, which may come
            // after the tick, an event then counting at the tick's time, as the core's clock
            // doesn't go back. Each due tick ends a phase or the run, so the input held goes on
            // after it.
            if input.reached().is_some_and(|reached| reached >= due) {
                self.held = Some(input);
                return Ok((self.clock.now(), Event::Tick));
            }
            if let Some(event) = self.arrived(Some(scope), core, input)? {
                return Ok(event);
            }
        }
    }

    /// The event that `input` brings the core, if the core waits on it, at the time it reached
    /// the host ([`Input::reached`]). The core learns of none of these: an op whose payload the
    /// gateway refused, which is refused by the core's next act ([`Core::phase_due`]) and
    /// goes to the run log; a model request, answered on a thread of `scope`
    /// ([`Runtime::model_request`]); and what is for the run log alone ([`Input::Logged`]).
    fn arrived<'s>(
        &mut self,
        scope: Option<&'s Scope<'s, 'r>>,
        core: &Core<'_>,
        input: Input,
    ) -> Result<Option<(Duration, Event)>, String> {
        let (at, arrival) = match input {
            Input::Agent(at, arrival) => (at, arrival),
            Input::Child(report) => return self.report(scope, core, report),
            Input::Logged(entry) => return self.log.entry(entry).map(|()| None),
        };
        // Ops are taken as usual even once the agent's session has closed: the core has ended
        // the work phase, or the run, by then, so it refuses each, and the run log notes that
        // the answer wasn't sent.
        let event = match arrival {
            Arrival::Op(incoming) => {
                let (request, unanswered) = request(incoming);
                Some(self.op(Asked::Agent(unanswered), request))
            }
            Arrival::Refused(refused) => {
                self.refuse(refused, core.phase_due())?;
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
                self.model_request(scope, core, (Caller::Agent, at), incoming)?;
                None
            }
        };
        Ok(event.map(|event| (at, event)))
    }

    /// Takes `incoming`, a model request of `caller`'s that reached the host at `at`, under the
    /// next number, and answers it on a thread of `scope` as the caller's route serves it
    /// ([`Served::serve`]), refused unless the core allows the caller's model requests then. One
    /// taken once the run's threads have ended, with no `scope`, is dropped unanswered, since the
    /// caller's session has ended by then ([`Runtime::drain`]).
    fn model_request<'s>(
        &mut self,
        scope: Option<&'s Scope<'s, 'r>>,
        core: &Core<'_>,
        (caller, at): (Caller, Duration),
        incoming: IncomingModelRequest,
    ) -> Result<(), String> {
        let number = self.next_model_request;
        self.next_model_request += 1;
        // No type excludes a model request here, since one `Arrival` serves sessions with model
        // requests and without.
        let Some(served) = self.served.get(&caller).map(Arc::clone) else {
            unreachable!(
                "a model request arrived from {caller:?}, though a session takes them only if its \
                 role has a route (`Sandboxes::start`), whose sandbox's start serves it, and one \
                 breaks the protocol otherwise, which ends the session"
            )
        };
        let Some(scope) = scope else {
            drop(incoming);
            let note = format!("model request {number} wasn't answered: the run has ended");
            return self.log.note(&note);
        };
        let allowed = core.model_requests_allowed(caller, at);
        // A response still open at its deadline ends the caller's session, which its collect
        // needs, so it is due by then; the core's stop of its requests ends it before that
        // (`end_routes`).
        let by = match caller {
            Caller::Agent => self.teardown_end(),
            Caller::Job(job) => self.call(job).collect,
        };
        let by = deadline(&self.clock, by);
        let reports = self.inbox.sender();
        let exchange = Exchange::new(self.dir, number);
        // A request stays in flight until its thread writes its response's end, the thread's
        // last step, so the session's bound on the requests in flight,
        // `MAX_MODEL_REQUESTS_IN_FLIGHT`, bounds these threads too, but for those writing that
        // end.
        let spawned = thread::Builder::new()
            .name(format!("model-{number}"))
            .spawn_scoped(scope, move || {
                // The loop's inbox outlives every request's thread, which the run's end waits
                // for. Both reports come before the answer's end, and so before what the caller
                // does next.
                let noted = |note| {
                    let _ = reports.send(Input::Logged(Entry::Note(note)));
                };
                let answered = |answer| {
                    let kept = exchange.kept();
                    let answered = Entry::ModelAnswered((caller, number), answer, kept);
                    let _ = reports.send(Input::Logged(answered));
                };
                served.serve((number, &exchange), incoming, allowed, by, noted, answered);
                if let Some(failed) = exchange.finish() {
                    let note = format!("model request {number} wasn't kept whole: {failed}");
                    let _ = reports.send(Input::Logged(Entry::Note(note)));
                }
            });
        match spawned {
            Ok(_) => Ok(()),
            Err(error) => {
                let note = format!(
                    "model request {number} wasn't answered: its thread didn't start: {error}"
                );
                self.log.note(&note)
            }
        }
    }

    /// The event that `report`, a child's, brings the core, if it has one, at the time the
    /// child's thread observed it; a model request is answered as [`Runtime::model_request`]
    /// says. A sandbox that couldn't be deleted fails the run.
    fn report<'s>(
        &mut self,
        scope: Option<&'s Scope<'s, 'r>>,
        core: &Core<'_>,
        report: Report,
    ) -> Result<Option<(Duration, Event)>, String> {
        match report {
            Report::Event(at, event) => Ok(Some((at, event))),
            Report::Op {
                at,
                job,
                request,
                trees,
            } => {
                self.call(job).trees = Some(trees);
                Ok(Some((at, self.op(Asked::Job(job), request))))
            }
            Report::Model { at, job, incoming } => {
                let caller = Caller::Job(job);
                self.model_request(scope, core, (caller, at), incoming)
                    .map(|()| None)
            }
            Report::Undeleted(reason) => Err(reason),
        }
    }

    /// Gives the core `request`, an op that `asked` answers: the event of its arrival, under the
    /// next number. An agent's payload is stored only once the core accepts it
    /// ([`Runtime::reply`]).
    fn op(&mut self, asked: Asked, request: Request) -> Event {
        let op = OpId(self.next_op);
        self.next_op += 1;
        let caller = match asked {
            Asked::Agent(_) => Caller::Agent,
            Asked::Job(job) => Caller::Job(job),
        };
        self.unanswered.insert(op, asked);
        Event::OpArrived {
            op,
            caller,
            request,
        }
    }

    /// Refuses `refused`, an op of the agent's whose payload the gateway refused, by `by`, a time
    /// since the run started, and records it in the run log.
    fn refuse(&mut self, refused: RefusedOp, by: Duration) -> Result<(), String> {
        self.log.refused(&refused)?;
        let what = "the refusal of an op whose payload was over the agent's limit";
        self.answer(what, by, |agent, by| agent.refuse(refused, by))
    }

    /// Takes the input held behind the last tick, if there is one, and what is left in the inbox,
    /// once the core has finished or abandoned the run, and every judge call's thread has ended,
    /// as the loop does ([`Runtime::arrived`]): the core refuses each op, as it does every op
    /// after its end, and the run log notes that the refusal wasn't sent.
    fn drain(&mut self, core: &mut Core<'_>) -> Result<(), String> {
        // Its teardown has dropped the agent's session, unless the run was abandoned first, and
        // this drops it then, its sandbox left to the sweep: either way, its reader has handed
        // over all it will by now.
        self.agent = None;
        while let Some(input) = self.held.take().or_else(|| self.inbox.take()) {
            let Some((at, event)) = self.arrived(None, core, input)? else {
                continue;
            };
            let actions = core.handle(at, event.clone());
            self.log.event(self.workflow(), at, &event, &actions)?;
            for action in actions {
                match action {
                    Action::Reply { op, reply } => self.reply(op, reply, core.phase_due())?,
                    // A type excluding these actions here would split the core's actions by the
                    // run's phase.
                    other @ (Action::StartAgent
                    | Action::StartJudge { .. }
                    | Action::StopJudge { .. }
                    | Action::StartLease { .. }
                    | Action::RunCommand { .. }
                    | Action::Transfer { .. }
                    | Action::CloseLease { .. }
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
                Action::StartAgent => self.start_agent(),
                Action::Reply { op, reply } => self.reply(op, reply, core.phase_due())?,
                Action::StartJudge {
                    job,
                    call,
                    payload,
                    timeout,
                } => self.start_judge(scope, core, job, call, &payload, timeout)?,
                Action::StopJudge { job } => self.stop_job(job),
                Action::StartLease {
                    lease,
                    holder,
                    grant,
                    instance,
                    start_by,
                } => {
                    let start = (lease, instance, start_by);
                    self.start_lease(scope, core, start, (holder, grant))?;
                }
                Action::RunCommand {
                    lease,
                    command,
                    argv,
                    timeout,
                } => {
                    let run = lease::Run {
                        command,
                        argv,
                        timeout,
                    };
                    self.send_lease(lease, Message::Order(lease::Order::Run(run)));
                }
                Action::Transfer {
                    lease,
                    holder,
                    transfer: number,
                    direction,
                    max_bytes,
                } => {
                    let holder = self.holder_trees(lease, holder);
                    let transfer = lease::Transfer {
                        number,
                        direction,
                        max_bytes,
                        holder,
                    };
                    let order = lease::Order::Transfer(transfer);
                    self.send_lease(lease, Message::Order(order));
                }
                Action::CloseLease { lease } => self.send_lease(lease, Message::Stop),
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

    /// Starts the agent's sandbox and its command, whose report is the next event.
    fn start_agent(&mut self) {
        // From the run's start, where the core counts the agent's startup from: a start past it
        // would silently shorten the agent's deadline, so it fails.
        let agent = self.workflow().agent().role;
        let by = deadline(&self.clock, self.placed.startup(agent));
        let sender = self.inbox.sender();
        let hard_end = self.unix_hard_end;
        let clock = self.clock.clone();
        let (agent, event) =
            Agent::start(self.sandboxes, self.workflow(), by, hard_end, sender, clock);
        self.agent = agent;
        self.pending.push_back((self.clock.now(), event));
    }

    /// Answers op `op` with `reply`, the agent's by `by`, a time since the run started, and a
    /// call's through its thread. An op the core accepted has its payload stored first, since a
    /// judge call reads it, now or in the final phase; a refused op's bytes are never written.
    fn reply(&mut self, op: OpId, reply: OpReply, by: Duration) -> Result<(), String> {
        // A type tying the reply to its op would carry the gateway's `IncomingOp` through the
        // core, which does no I/O.
        let Some(asked) = self.unanswered.remove(&op) else {
            unreachable!(
                "op {} was answered twice, or never given to the core, though the core answers \
                 every op it is given exactly once (`Core`), and the runtime gives it each op \
                 once, under a number that only this takes away",
                op.0
            )
        };
        let unanswered = match asked {
            Asked::Agent(unanswered) => unanswered,
            Asked::Job(job) => {
                // A call whose thread has ended has no op left to answer.
                let _ = self.call(job).orders.send(Message::Order(reply));
                return Ok(());
            }
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

    /// Sends an answer to an op of the agent's, `what`, by `by`, a time since the run started,
    /// with `write`, unless `by` has passed or the agent's sandbox is gone. The run log notes an
    /// answer that isn't sent, or fails.
    fn answer(
        &mut self,
        what: &str,
        by: Duration,
        write: impl FnOnce(&mut Agent, Deadline) -> io::Result<()>,
    ) -> Result<(), String> {
        let sent = match &mut self.agent {
            // A write past `by` would end the session, which the agent's teardown still needs,
            // for a delay of the host's own, such as hashing the payload, the log's sync or
            // storing the payload. The op stays in flight, which wedges the agent's ops, but
            // only until the core's next act: while the agent works, `by` is the end of the work
            // phase (`Core::tick_due`), so that act tears the agent down; after `Abandon`, the
            // run's end drops it.
            Some(_) if self.clock.now() >= by => Err(format!(
                "{what} wasn't sent: it was due by the core's next act, which has come"
            )),
            Some(agent) => {
                let by = deadline(&self.clock, by);
                write(agent, by).context(format_args!("{what} failed"))
            }
            None => Err(format!("{what} wasn't sent: the agent's sandbox is gone")),
        };
        match sent {
            Ok(()) => Ok(()),
            Err(note) => self.log.note(&note),
        }
    }

    /// Starts job `job`, a judge call of `call` on `payload` whose command runs for at most
    /// `timeout`, on a thread of `scope`'s, which reports to the loop ([`judge::run`]). The job
    /// fails, with nothing started, if the thread doesn't start.
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
        let slot = self.workflow().call_slot(call);
        let call = judge::Call {
            job,
            slot,
            payload: self.payloads.stored(payload),
            timeout,
        };
        let which = Which::Job(job, slot.judge());
        let run = move |mut child: Child<'r, H, OpReply>| judge::run(&call, &mut child);
        let orders = self.start_child(scope, which, deadlines, run)?;
        let collect = deadlines.collect;
        let called = Called {
            orders,
            collect,
            trees: None,
            judge: slot.judge().role(),
        };
        self.jobs.insert(job, called);
        Ok(())
    }

    /// Opens lease `lease` of grant `grant` for `holder`, as instance `instance` of its runner,
    /// whose start is given up at `start_by` at the latest, its holder's end, on a thread of
    /// `scope`'s, which reports to the loop ([`lease::run`]). The lease fails, with nothing
    /// started, if the thread doesn't start.
    fn start_lease<'s>(
        &mut self,
        scope: &'s Scope<'s, 'r>,
        core: &Core<'_>,
        (lease, instance, start_by): (LeaseId, u32, Duration),
        (holder, grant): (Caller, LeaseGrantId),
    ) -> Result<(), String> {
        let runner_id = self.workflow().lease_grant(grant).runner;
        let runner = self.workflow().runner(runner_id);
        let (start, collect) = match holder {
            // A lease of the agent's ends with the work phase, which ends by the core's next end
            // of a phase, and what it left is collected by the agent's teardown's end, as a judge
            // call's is.
            Caller::Agent => (core.phase_due(), self.teardown_end()),
            // A call's ends with the call's command, and what it left is collected by when the
            // call's is.
            Caller::Job(job) => {
                let collect = self.call(job).collect;
                (collect, collect)
            }
        };
        let startup = (self.clock.now())
            .saturating_add(self.placed.startup(runner_id.role()))
            .min(start)
            .min(start_by);
        let deadlines = Deadlines {
            startup,
            start,
            collect,
        };
        let which = Which::Lease(lease, runner_id, instance);
        let run =
            move |mut child: Child<'r, H, lease::Order>| lease::run(lease, runner, &mut child);
        let orders = self.start_child(scope, which, deadlines, run)?;
        self.leases.insert(lease, orders);
        Ok(())
    }

    /// Stops job `job`'s call.
    fn stop_job(&mut self, job: JobId) {
        // A call whose thread has ended has nothing left to stop.
        let _ = self.call(job).orders.send(Message::Stop);
    }

    /// Job `job`'s call.
    fn call(&mut self, job: JobId) -> &mut Called {
        // A type tying the job to its start would carry the runtime's sender through the core,
        // which holds no runtime state.
        let Some(call) = self.jobs.get_mut(&job) else {
            unreachable!(
                "job {} was stopped, answered or leased for, but never started, though the core \
                 acts only on a job it has started, and the `StartJudge` that started it, \
                 performed before, kept a sender into its call's inbox, even if the call then \
                 failed as it started",
                job.0
            )
        };
        call
    }

    /// The trees of `holder`'s sandbox, for a transfer of lease `lease`'s, which an op of the
    /// holder's asked for.
    fn holder_trees(&mut self, lease: LeaseId, holder: Caller) -> Trees {
        let trees = match holder {
            // The core takes the agent's ops only while the agent works, its sandbox started.
            Caller::Agent => self.agent.as_ref().map(Agent::trees),
            // The op came with them.
            Caller::Job(job) => self.call(job).trees.take(),
        };
        let Some(trees) = trees else {
            unreachable!(
                "lease {} was ordered a transfer with no trees of its holder's, though the core \
                 orders one only for an op of the holder's, taken while its sandbox runs, and a \
                 call relays each op with its sandbox's trees",
                lease.0
            )
        };
        trees
    }

    /// Sends `message` into lease `lease`'s inbox.
    fn send_lease(&self, lease: LeaseId, message: Message<lease::Order>) {
        // A type tying the order to the open would carry the runtime's sender through the core,
        // which holds no runtime state.
        let Some(orders) = self.leases.get(&lease) else {
            unreachable!(
                "lease {} was given an order, but never opened, though the core orders only a \
                 lease it has opened, and the `StartLease` that opened it, performed before, kept \
                 a sender into its inbox, even if the lease then failed as it started",
                lease.0
            )
        };
        // A lease whose thread has ended has run its last command, and the core hears of its end.
        let _ = orders.send(message);
    }

    /// Starts `which`, a child of the run whose steps are due by `deadlines`, on a thread of
    /// `scope`'s that runs `run` on it, in a directory of its own in the record, and returns the
    /// sender into its inbox. The child fails ([`Which::failed`]), with nothing started, if the
    /// thread doesn't start.
    ///
    /// # Errors
    ///
    /// Fails if the child's directory can't be made.
    fn start_child<'s, O: Send + 'static>(
        &mut self,
        scope: &'s Scope<'s, 'r>,
        which: Which,
        deadlines: Deadlines,
        run: impl FnOnce(Child<'r, H, O>) + Send + 's,
    ) -> Result<Sender<Message<O>>, String> {
        let dir = self.dir.join(which.record_dir());
        let parent = dir.parent().unwrap_or(self.dir);
        fs::create_dir_all(parent).context(parent.display())?;
        fs::create_dir(&dir).context(dir.display())?;
        let inbox = self.clock.inbox();
        let sender = inbox.sender();
        let child = Child {
            which,
            deadlines,
            hard_end: self.unix_hard_end,
            dir,
            sandboxes: self.sandboxes,
            clock: self.clock.clone(),
            inbox,
            reports: self.inbox.sender(),
        };
        let spawned = thread::Builder::new()
            .name(which.name().to_string())
            .spawn_scoped(scope, move || run(child));
        if let Err(error) = spawned {
            // Reported as the child's thread would have, behind what reached the host before.
            let reason = format!("{} thread didn't start: {error}", which.noun());
            let failed = Report::Event(self.clock.now(), which.failed(reason));
            // The inbox holds its receiver, so the send can't fail.
            let _ = self.inbox.sender().send(Input::Child(failed));
        }
        Ok(sender)
    }

    /// By when the steps of a judge call of `call`, whose command runs for at most `timeout`,
    /// starting now, are to be done.
    fn judge_deadlines(&self, core: &Core<'_>, call: JudgeCall, timeout: Duration) -> Deadlines {
        let judge = self.workflow().call_slot(call).judge().role();
        let startup = self.clock.now().saturating_add(self.placed.startup(judge));
        match call {
            // A call of the agent's ends with the work phase, which ends by the core's next act
            // of its own, and what it left is collected by the agent's teardown's end, which
            // leaves the final judge its share of the hard end.
            JudgeCall::Grant(_) => {
                let start = startup.min(core.phase_due());
                Deadlines {
                    startup: start,
                    start,
                    collect: self.teardown_end(),
                }
            }
            // The final judge's command starts in time to run its whole timeout before the hard
            // end, by which what it left is collected.
            JudgeCall::Score => {
                let start = self.hard_end().saturating_sub(timeout);
                Deadlines {
                    startup: startup.min(start),
                    start,
                    collect: self.hard_end(),
                }
            }
        }
    }

    /// Serves `caller`'s sandbox, of role `role`, a route of its own, if the role has one, with
    /// upstreams of its own ([`Served::new`]).
    fn serve(&mut self, caller: Caller, role: RoleId) {
        if let Some(served) = Served::new(self.routes.get(role)) {
            self.served.insert(caller, Arc::new(served));
        }
    }

    /// Ends each model route whose model requests `core` no longer allows at `at`, or every
    /// route for `None`, with its every exchange in flight and every later one
    /// ([`Served::end`]). Ending one again does nothing.
    fn end_routes(&self, allowing: Option<(&Core<'_>, Duration)>) {
        for (caller, served) in &self.served {
            if allowing.is_none_or(|(core, at)| !core.model_requests_allowed(*caller, at)) {
                served.end();
            }
        }
    }

    /// Checks that no model provider failed a route of the run's as the deployment's fault, not a
    /// result of the agent's to score: that none refused a route's key, but one heard after the
    /// selection was locked on a route that served before it ([`Runtime::held_at_lock`]), and
    /// that none failed the agent's last request, if it exited with no submission finalized in a
    /// run that ended with `outcome` ([`Served::failed_last`]).
    ///
    /// # Errors
    ///
    /// Fails, naming the upstream and the route, if one did.
    fn routes_held(&self, outcome: &Result<Outcome, Stopped>) -> Result<(), String> {
        // A refusal heard after the lock can't change the score: the agent's or a call's whose
        // answer the agent can no longer act on. The score's call starts after the lock, so a
        // refusal on its route always fails the run.
        for (caller, served) in &self.served {
            if let Some(name) = served.key_refused_by()
                && !self.held_at_lock.contains(caller)
            {
                let route = match caller {
                    Caller::Agent => "the agent's".to_owned(),
                    Caller::Job(job) => format!("job {}'s", job.0),
                };
                return Err(format!(
                    "the upstream `{name}` refused its key, with a 401, which ended {route} \
                     model route: give it a key that it takes, and run again"
                ));
            }
        }
        match self.served.get(&Caller::Agent) {
            Some(agent) => agent.failed_last(outcome),
            None => Ok(()),
        }
    }

    /// Stops every child's thread that still runs, a judge call's or a lease's, as the run's end
    /// does.
    fn stop_children(&self) {
        // A child whose thread has ended has nothing left to stop.
        for call in self.jobs.values() {
            let _ = call.orders.send(Message::Stop);
        }
        for stop in self.leases.values() {
            let _ = stop.send(Message::Stop);
        }
    }

    /// Collects what the agent's sandbox holds, then deletes it, even if collecting failed,
    /// keeping its console, or noting why it wasn't kept, and logs the delete. A failure
    /// to write the record, or to delete the sandbox, is returned once each has been tried, joined
    /// if more than one fails.
    fn tear_down_agent(&mut self) -> Result<(), String> {
        let Some(mut agent) = self.agent.take() else {
            let note = "the agent's sandbox never started: nothing was collected";
            return self.log.note(note);
        };
        let collected = self.collect_agent(&mut agent);
        let (logged, deleted) = agent.delete(self.sandboxes);
        let noted = (logged.into_iter()).try_for_each(|entry| self.log.entry(entry));
        failed_also(failed_also(collected, noted), deleted)
    }

    /// Freezes the agent's sandbox and collects what it holds into the record's `agent`, by the
    /// teardown's end, and notes in the run log what was collected and what failed.
    fn collect_agent(&mut self, agent: &mut Agent) -> Result<(), String> {
        // Its `sandbox.json` is there already, as the agent's create wrote it.
        let dir = self.dir.join("agent");
        fs::create_dir_all(&dir).context(dir.display())?;
        let by = deadline(&self.clock, self.teardown_end());
        let mut notes = agent.freeze_and_collect(&dir, by);
        let collected = record::collected(&dir);
        notes.push(format!("collected from the agent's sandbox: {collected}"));
        notes.iter().try_for_each(|note| self.log.note(note))
    }

    /// Deletes every sandbox that each of the run's providers still lists of it, and notes each
    /// console the sweep didn't keep.
    fn sweep(&mut self) -> Result<(), String> {
        let (unkept, swept) = self.sandboxes.sweep();
        let noted = unkept.iter().try_for_each(|unkept| self.log.note(unkept));
        failed_also(swept, noted)
    }

    /// Logs what is in the inbox for the run log alone ([`Input::Logged`]), once the core has
    /// taken all it will: after a stop short of the core's end, what the run's children noted and
    /// deleted as they stopped, whose events and ops the core no longer takes; after the sweep,
    /// what it deleted. A child's sandbox that couldn't be deleted fails the run, once the rest
    /// is logged.
    fn settle(&mut self) -> Result<(), String> {
        let mut failures = Vec::new();
        while let Some(input) = self.held.take().or_else(|| self.inbox.take()) {
            let logged = match input {
                Input::Logged(entry) => self.log.entry(entry),
                Input::Child(Report::Undeleted(reason)) => Err(reason),
                Input::Agent(..)
                | Input::Child(Report::Event(..) | Report::Op { .. } | Report::Model { .. }) => {
                    Ok(())
                }
            };
            failures.extend(logged.err());
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }
}
