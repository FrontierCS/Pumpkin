//! A run's driver: it turns what happens in a run into events for the decision core, and the
//! core's actions into acts on the run's providers. A role's model requests are answered on
//! threads of their own, until the core stops allowing their caller's.
//!
//! One thread takes every input through a single inbox: the agent's session's arrivals, the
//! reports of each call's and lease's own thread, and the log entries of the model
//! threads. It gives each to the core as an [`Event`], records the event with the actions the
//! core returned, and only then performs them. The core reads no clock, so each event carries
//! the time since the run started, and a [`Event::Tick`] comes once the core's next act is due.
//! The record's layout belongs to `record` and `log`, and the start of every sandbox to `sandbox`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::thread::{self, Scope};
use std::time::Duration;
use std::{fs, io};

use decision::{Action, Caller, Core, Event, JobId, LeaseId, OpId, Outcome, Request, lasts};
use gateway::{Arrival, Dialer, GrantListener, RefusedOp, Trees};
use provider::{Provider, Reservation, RunTag, UnixTime};
use route::Serving;
use schema::ir::{Label, ModelName};
use schema::sandbox::op::OpReply;
use schema::workflow::{ByProvider, ByRole, Placed, RoleId, Workflow};
use serde_json::json;
use stdx::{Context, Deadline};

mod agent;
mod body;
mod budget;
mod call;
mod child;
mod host;
mod interruption;
mod lease;
mod log;
mod model;
mod op;
mod price;
mod record;
mod relay;
mod routes;
mod sandbox;
mod sources;
mod start;

use agent::Agent;
pub use body::{Bodies, RunState, Target};
use budget::Budget;
pub use budget::{Budgets, budgets};
use child::{Message, Report};
use host::deadline;
pub use host::{Clock, Durable, Host, Inbox, System, SystemClock, SystemInbox};
pub use interruption::{Signal, Stopped};
pub use log::{
    AgentEnd, AgentSpan, CallEnd, CallOf, CallSpan, CallStart, Closed, Cost, Direction, LeaseSpan,
    LogFold, ModelUse, Part, Place, PlaceUse, Priced, Progress, Readout, Sandbox, StartedOver,
    TakenLine, Tokens, TokensCost, Unread, Used, Work, returned_values,
};
use log::{Entry, Log, Targeted};
use model::Served;
pub use model::{KeptPart, KeptRequest};
use op::{Unanswered, accepts, request};
pub use price::TokenPrices;
pub use record::{
    BUILD, Build, Ending, Reaped, Record, RouteEntry, ir_json, minted, named, new_id,
};
use record::{Lock, Payloads};
use sandbox::{Published, Sandboxes};

// The longest the loop waits on its inbox before it looks for a signal: a stop that comes this
// late goes unnoticed by the person who asked for it, and the loop wakes for it at most 20 times a
// second.
const LOOK: Duration = Duration::from_millis(50);

/// What a run's record holds beyond its workflow: the arguments that started the run, what each
/// model's tokens cost, and the text of each file that the workflow's evaluation read, by its
/// label.
#[derive(Clone, Copy)]
pub struct Recorded<'a> {
    pub args: &'a [String],
    pub prices: &'a BTreeMap<ModelName, TokenPrices>,
    pub sources: &'a BTreeMap<Label, String>,
}

/// The upstreams that serve each role's model route, one for each entry in the route's order, and
/// the budgets that bound what the roles spend through them ([`budgets`]).
pub struct ModelRoutes<'s> {
    pub servings: &'s ByRole<Vec<Serving>>,
    pub budgets: Budgets,
}

/// A run whose providers have reserved room for it and whose record exists, ready to
/// [`execute`](Run::execute).
pub struct Run<'r, H> {
    host: &'r H,
    placed: Placed,
    providers: ByProvider<&'r dyn Provider>,
    routes: ByRole<Vec<Serving>>,
    budgets: Budgets,
    prices: &'r BTreeMap<ModelName, TokenPrices>,
    id: RunTag,
    dir: PathBuf,
    lock: Lock,
    reserved: ByProvider<Reservation>,
}

impl<'r, H: Host> Run<'r, H> {
    /// Reserves room on each provider, then begins the record under `runs`, so that a run that
    /// doesn't fit leaves no record.
    ///
    /// # Errors
    ///
    /// Fails, before any record is made, if a role's route has no serving, a provider can't
    /// reserve, `runs` isn't safe from other users, or `id` is taken.
    pub fn prepare(
        host: &'r H,
        placed: Placed,
        providers: ByProvider<&'r dyn Provider>,
        runs: &Path,
        id: RunTag,
        models: ModelRoutes<'_>,
        recorded: Recorded<'r>,
    ) -> Result<Self, String> {
        let Recorded {
            args,
            prices,
            sources,
        } = recorded;
        let ModelRoutes { servings, budgets } = models;
        let routes = model::routes(placed.workflow(), servings)?;
        let reserved = placed.by_provider(|name| {
            let phases = sandbox::phases(&placed, name);
            (providers.get(name).reserve(&id, &phases))
                .context(format_args!("reserving on provider `{name}`"))
        })?;
        let (dir, lock) = record::create(host, runs, id.as_str(), args)?;
        record::write_ir(host, &dir, placed.workflow())?;
        sources::write(host, &dir, sources)?;
        Ok(Self {
            host,
            placed,
            providers,
            routes,
            budgets,
            prices,
            id,
            dir,
            lock,
            reserved,
        })
    }

    pub fn id(&self) -> &str {
        self.id.as_str()
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Runs the workflow to its outcome and writes `outcome.json`, whichever way it ends, before it
    /// releases each provider's reservation and the record's lock. The run ends with none of its
    /// sandboxes left, or fails. `started` is called once, when the run can start its first
    /// sandbox and before its clock starts.
    ///
    /// # Errors
    ///
    /// Stops once a signal comes. Fails if `started` fails, if the run is still going at its hard
    /// end, or if a model provider's refusal or failure could have changed the result.
    pub fn execute(
        self,
        sandboxd: &Path,
        private: Option<&GrantListener>,
        dialer: &Dialer,
        bodies: &dyn Bodies,
        started: impl FnOnce() -> Result<(), String>,
    ) -> Result<Outcome, Stopped> {
        let outcome = self.run_on(sandboxd, (private, dialer), bodies, started);
        // None if the run never began its log.
        let (outcome, usage) = match log::progress(&self.dir) {
            Ok(readout) => (outcome, readout.progress.as_ref().map(log::usage_of)),
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
        // Released before the lock, which `pumpkin wait` follows, so that a run it saw end holds
        // nothing.
        drop(self.reserved);
        drop(self.lock);
        failed_also(outcome, written)
    }

    fn run_on(
        &self,
        sandboxd: &Path,
        (private, dialer): (Option<&GrantListener>, &Dialer),
        bodies: &dyn Bodies,
        started: impl FnOnce() -> Result<(), String>,
    ) -> Result<Outcome, Stopped> {
        let host = self.host;
        let provided = record::provided(&self.placed, &self.providers, &self.routes, self.prices);
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
            budgets: self.budgets.unspent(),
            dir: &self.dir,
            sandboxes: &sandboxes,
            bodies,
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
        // A refused key, or a model provider's failure of the agent's last request, is the
        // deployment's failure, not a result of the agent's to judge.
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

fn failed_also<T, E: Failure>(outcome: Result<T, E>, then: Result<(), String>) -> Result<T, E> {
    match (outcome, then) {
        (outcome, Ok(())) => outcome,
        (Ok(_), Err(error)) => Err(E::from(error)),
        (Err(stopped), Err(error)) => Err(stopped.and(&error)),
    }
}

trait Failure: From<String> {
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

// A call: its thread's orders, and what the run knows of the role that runs it.
struct Called {
    orders: Sender<Message<OpReply>>,
    // By when what the call left is collected, as a time since the run started; its leases' too.
    collect: Duration,
    // The trees of the call's sandbox that its last op came with; a transfer takes them.
    trees: Option<Trees>,
    // The role that runs the call, whose model route serves once the call's command or body
    // starts.
    role: RoleId,
}

enum Asked {
    Agent(Unanswered),
    Job(JobId),
}

struct Runtime<'r, H: Host> {
    placed: &'r Placed,
    routes: &'r ByRole<Vec<Serving>>,
    // The model routes of the callers whose roles have one.
    served: BTreeMap<Caller, Arc<Served>>,
    budgets: BTreeMap<RoleId, Budget<'r>>,
    dir: &'r Path,
    sandboxes: &'r Sandboxes<'r, H>,
    bodies: &'r dyn Bodies,
    clock: H::Clock,
    unix_hard_end: UnixTime,
    log: Log<'r, H>,
    payloads: Payloads<'r, H>,
    agent: Option<Agent>,
    // The ops given to the core that it hasn't replied to: at most one of each sandbox's, whose
    // session hands out an op only once the one before it has been answered.
    unanswered: BTreeMap<OpId, Asked>,
    next_op: u64,
    next_model_request: u64,
    // The callers whose routes had no key refused when a submission was accepted for good.
    held_at_lock: BTreeSet<Caller>,
    jobs: BTreeMap<JobId, Called>,
    leases: BTreeMap<LeaseId, Sender<Message<lease::Order>>>,
    // Events known already, which the loop gives the core before it waits on its inbox.
    pending: VecDeque<(Duration, Event)>,
    inbox: <H::Clock as Clock>::Inbox<Input>,
    // What the loop took from the inbox once the core's tick was due, which reached the host at or
    // after the tick's time, and so goes to the core after the tick.
    held: Option<Input>,
}

enum Input {
    Agent(Duration, Arrival),
    Child(Report),
    Logged(Entry),
}

impl Input {
    // None for what the core doesn't decide on by time: log entries, and a child's sandbox that
    // couldn't be deleted, which fails the run.
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

    fn drive(&mut self, unix: UnixTime) -> Result<Outcome, Stopped> {
        let (mut core, actions) = Core::start(self.placed);
        self.log
            .start(self.workflow(), unix, self.hard_end(), &actions)?;
        self.serve(Caller::Agent, self.workflow().agent().role);
        // The threads of calls borrow the run's workflow and sandboxes, and the scope ends
        // once each has, so that no sandbox starts after the run's end, and the drain takes all
        // they reported. A stop waits so for a child whose provider's create is under way, which
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
            // Each route is ended once the core stops allowing its model requests, unless the run
            // ended first: this ends them then. The scope's end still waits on their exchanges,
            // within the upstreams' bounds and the responses' deadlines.
            self.end_routes(None);
            end
        });
        // Only the core's end drains the inbox into the core: after a failure of the runtime's
        // own, such as a record it couldn't write, or an interruption, the core may still be
        // taking ops, and would decide on ones that nothing acts on; the run log still takes what
        // the run's children noted and deleted as they stopped.
        match end {
            Ok(end) => failed_also(end.map_err(Stopped::Failed), self.drain(&mut core)),
            Err(stopped) => failed_also(Err(stopped), self.settle()),
        }
    }

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
            // The decision is recorded below, before it is carried out at the top of the next turn.
            let locked = core.selection_locked();
            actions = core.handle(at, event.clone());
            // A call's route is open from its command's or its body's start, as the agent's is
            // from the run's.
            if let Event::CallStarted { job } = event {
                let role = self.call(job).role;
                self.serve(Caller::Job(job), role);
            }
            // The keys' refusals are read as the core locks the selection, before the record of it
            // is written, so that a refusal heard after the lock leaves the run to be judged.
            if !locked && core.selection_locked() {
                let held = self
                    .served
                    .iter()
                    .filter(|(_, served)| served.key_refused_by().is_none());
                self.held_at_lock = held.map(|(caller, _)| *caller).collect();
            }
            self.log.event(self.workflow(), at, &event, &actions)?;
            // A route ends as the core stops allowing its model requests, the agent's at the work
            // phase's end, a call's at its timeout or its report, before the core's actions are
            // performed: its exchanges in flight then end while the agent's teardown, if the
            // actions hold it, still needs the agent's session. The event that first stops the
            // agent's ends the work phase or the run, so its record lists the agent's teardown or
            // the run's abandon, and is synced before the route ends too.
            self.end_routes(Some((core, at)));
        }
    }

    fn hard_end(&self) -> Duration {
        lasts(self.placed)
    }

    // When the agent's teardown must have ended, as a time since the run started: the hard end
    // less what the final call's target, the run's last sandbox, may take to start and to run.
    fn teardown_end(&self) -> Duration {
        let final_call = self.workflow().final_call();
        let startup = self.placed.startup(final_call.target.role());
        let final_share = startup.saturating_add(final_call.timeout);
        self.hard_end().saturating_sub(final_share)
    }

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
            // wait on. What reached the host before that time is in the inbox by then, but for
            // an input whose sender was preempted between stamping and sending it, which may
            // come after the tick, an event then counting at the tick's time, as the core's
            // clock doesn't go back. Each due tick ends a phase or the run, so the input held
            // goes on after it.
            if input.reached().is_some_and(|reached| reached >= due) {
                self.held = Some(input);
                return Ok((self.clock.now(), Event::Tick));
            }
            if let Some(event) = self.arrived(Some(scope), core, input)? {
                return Ok(event);
            }
        }
    }

    fn arrived<'s>(
        &mut self,
        scope: Option<&'s Scope<'s, 'r>>,
        core: &Core<'_>,
        input: Input,
    ) -> Result<Option<(Duration, Event)>, String> {
        let (at, arrival) = match input {
            Input::Agent(at, arrival) => (at, arrival),
            Input::Child(report) => return self.report(scope, core, report),
            Input::Logged(entry) => {
                self.count(&entry);
                return self.log.entry(entry).map(|()| None);
            }
        };
        // Ops are taken as usual even once the agent's session has closed: the core has ended the
        // work phase, or the run, by then, so it refuses each, and the run log notes that the
        // answer wasn't sent.
        let event = match arrival {
            Arrival::Op(incoming) => {
                let (workflow, bodies) = (self.workflow(), self.bodies);
                // The agent's now function's body is evaluated as its call arrives.
                let (request, unanswered) = request(incoming, |function, arguments| {
                    let mut now = workflow.now_functions();
                    let (_, now) = now.find(|(_, now)| now.name == *function)?;
                    Some(call::now::evaluate(now, arguments, bodies))
                });
                Some(self.op(Asked::Agent(unanswered), request))
            }
            Arrival::Refused(refused) => {
                self.refuse(refused, core.phase_due())?;
                None
            }
            // With no agent, the core waits on neither. The loop tears the agent down in the batch
            // that hears of its end, or of its command's failed start, so whatever of the
            // session's it takes after that batch comes here, such as its command's exit as the
            // teardown's freeze kills it, its end as the teardown drops it, or the exit of a
            // command whose start failed at its deadline as the sandbox said it started. So
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

    fn refuse(&mut self, refused: RefusedOp, by: Duration) -> Result<(), String> {
        self.log.refused(&refused)?;
        let what = "the refusal of an op whose payload was over the agent's limit";
        self.answer(what, by, |agent, by| agent.refuse(refused, by))
    }

    // Takes what the threads sent before they ended, so that each op that still arrived is
    // refused by the core and recorded.
    fn drain(&mut self, core: &mut Core<'_>) -> Result<(), String> {
        // Its teardown has dropped the agent's session, unless the run was abandoned first, and
        // this drops it then, its sandbox left to the sweep: either way, its reader has handed
        // over all it will.
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
                    | Action::StartFinalCall { .. }
                    | Action::StartCall { .. }
                    | Action::StopCall { .. }
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

    #[expect(clippy::too_many_lines, reason = "one arm per action")]
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
                Action::StartFinalCall {
                    job,
                    payload,
                    timeout,
                } => self.start_final_call(scope, job, &payload, timeout)?,
                Action::StartCall {
                    job,
                    function,
                    payload,
                    arguments,
                    timeout,
                } => {
                    let call = (&payload, arguments);
                    self.start_call(scope, core, job, function, call, timeout)?;
                }
                Action::StopCall { job } => self.stop_job(job),
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
                // Only a reply can follow `Finish` or `Abandon` in its batch: the refusal of an op
                // taken at the hard end, by which its answer was due, so `answer` notes it unsent,
                // and the op ends with the agent's sandbox, which the run's end deletes.
                Action::Finish { outcome } => end = Some(Ok(outcome)),
                Action::Abandon => {
                    end = Some(Err(format!(
                        "the run was still going at its hard end, {} s after it started: \
                         expected it to end within its deadline, its finalize grace, its final \
                         call's timeout and its sandboxes' startups",
                        self.hard_end().as_secs()
                    )));
                }
            }
        }
        Ok(end)
    }

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
                // Stored before the reply goes, since a call reads it, in the work phase or the
                // final one; a refused op's bytes are never written.
                if accepts(&reply) {
                    self.payloads.store(&payload, &reference)?;
                }
                incoming
            }
        };
        let what = format!("op {}'s reply", op.0);
        self.answer(&what, by, |agent, by| agent.reply(incoming, reply, by))
    }

    fn answer(
        &mut self,
        what: &str,
        by: Duration,
        write: impl FnOnce(&mut Agent, Deadline) -> io::Result<()>,
    ) -> Result<(), String> {
        let sent = match &mut self.agent {
            // A write past `by` would end the session, which the agent's teardown still needs, for
            // a delay of the host's own, such as hashing the payload, the log's sync or storing the
            // payload. The op stays in flight, which wedges the agent's ops, but only until the
            // core's next act: while the agent works, `by` is the end of the work phase
            // (`Core::tick_due`), so that act tears the agent down; after `Abandon`, the run's end
            // drops it.
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

    fn stop_job(&mut self, job: JobId) {
        // A call whose thread has ended has nothing left to stop.
        let _ = self.call(job).orders.send(Message::Stop);
    }

    fn call(&mut self, job: JobId) -> &mut Called {
        // A type tying the job to its start would carry the runtime's sender through the core,
        // which holds no runtime state.
        let Some(call) = self.jobs.get_mut(&job) else {
            unreachable!(
                "job {} was stopped, answered or leased for, but never started, though the core \
                 acts only on a job it has started, and the `StartFinalCall` or `StartCall` that started it, \
                 performed before, kept a sender into its call's inbox, even if the call then \
                 failed as it started",
                job.0
            )
        };
        call
    }

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

    fn stop_children(&self) {
        for call in self.jobs.values() {
            let _ = call.orders.send(Message::Stop);
        }
        for stop in self.leases.values() {
            let _ = stop.send(Message::Stop);
        }
    }

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

    fn sweep(&self) -> Result<(), String> {
        self.sandboxes.sweep()
    }

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
