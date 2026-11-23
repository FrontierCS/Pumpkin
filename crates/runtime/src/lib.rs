//! A run's driver: it turns what happens in a run into events for the decision core, and the
//! core's actions into acts on the run's providers. A role's model requests are answered on
//! threads of their own, until the core stops allowing their caller's.
//!
//! The driver runs on one loop ([`stdx::io`]), which holds every sandbox's session, the record's
//! syncs, and the agent's start and teardown. What the run's threads observe reaches the loop as
//! an `Input`, which the loop times as it takes it; what the agent's session says comes in the
//! loop's own batches. The loop gives each to the core as an [`Event`], records the event with
//! the actions the core returned, and only then performs them. The core reads no clock, so each
//! event carries the time since the run started, and a [`Event::Tick`] comes once the core's next
//! act is due. The record's layout belongs to `record` and `log`, and the start of every sandbox
//! to `starts`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::thread::{self, Scope};
use std::time::Duration;

use decision::{Action, Caller, Core, Event, JobId, LeaseId, OpId, Outcome, Request, lasts};
use gateway::{Answer, Arrival, Ask, Dialer, Listener, RefusedOp, SessionId, Sessions, Trees};
use provider::{Provider, Reservation, RunTag, UnixTime};
use route::Serving;
use schema::ir::{Label, ModelName};
use schema::sandbox::op::OpReply;
use schema::workflow::{AgentId, ByProvider, ByRole, Placed, RoleId, TargetId, Workflow};
use stdx::Context;
use stdx::io::Events;
use stdx::time::Clock as _;

mod agent;
mod body;
mod budget;
mod call;
mod child;
mod disk;
mod host;
mod interruption;
mod lease;
mod log;
mod model;
mod op;
mod price;
mod read_error;
mod record;
mod relay;
mod removal;
mod routes;
mod sandbox;
mod scoped;
mod shared;
mod sources;
mod start;
mod starts;
mod turn;

use agent::Agent;
pub use body::{Bodies, RunState, Target};
use budget::Budget;
pub use budget::{Budgets, budgets};
use child::{Message, Report};
use disk::Disk;
pub use host::{Clock, Host, Inbox, System};
pub use interruption::{Signal, Stopped};
pub use log::{
    AgentEnd, AgentSpan, CallEnd, CallOf, CallSpan, CallStart, CallerName, Closed, Cost, Direction,
    LeaseSpan, LogFold, ModelUse, Part, Place, PlaceUse, Priced, Progress, Readout, Sandbox,
    SharedSpan, StartedOver, TakenLine, Tokens, TokensCost, Unread, Used, Work, returned_values,
};
use log::{Entry, Log, Targeted};
use model::Served;
pub use model::{KeptPart, KeptRequest};
use op::{Unanswered, accepts, request};
pub use price::TokenPrices;
pub use read_error::ReadError;
pub use record::{
    BUILD, Build, Ending, Reaped, Record, RouteEntry, Writer, ir_json, minted, named, new_id,
    writer,
};
use record::{Lock, Payloads};
use sandbox::{Capacity, Inventory, Published, Sandboxes};
pub use stdx::io::Durable;

/// The run's loop, which its driver owns.
pub(crate) type Io<'r, 's, H> = stdx::io::Io<Runtime<'r, 's, H>>;

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
    capacity: Capacity,
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
        let capacity = Capacity::of(placed.workflow())?;
        let reserved = placed.by_provider(|name| {
            let phases = capacity.phases(&placed, name);
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
            capacity,
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
    /// Stops on a signal. Fails if a private provider lacks a listener, `started` fails, the run is
    /// live at its hard end, or the final target's model provider refuses its key.
    pub fn execute(
        self,
        sandboxd: &Path,
        private: Option<&Listener>,
        dialer: &Dialer,
        bodies: &dyn Bodies,
        started: impl FnOnce() -> Result<(), String>,
    ) -> Result<Outcome, Stopped> {
        let outcome = self.run_on(sandboxd, (private, dialer), bodies, started);
        // None if the run never began its log.
        let (outcome, usage) = match log::progress(&self.dir) {
            Ok(readout) => (outcome, readout.progress.as_ref().map(log::usage_of)),
            Err(error) => (failed_also(outcome, Err(error.into())), None),
        };
        let mut recorded = log::outcome_json(self.placed.workflow(), &outcome);
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

    #[expect(
        clippy::too_many_lines,
        reason = "most of it sets each of the runtime's fields"
    )]
    fn run_on(
        &self,
        sandboxd: &Path,
        (private, dialer): (Option<&Listener>, &Dialer),
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
        let unix = UnixTime::of(clock.realtime())?;
        let mut backend = host.backend().context("the run's loop can't be made")?;
        let start = backend.now();
        let (events, inbox) = stdx::io::Inbox::new(&*backend);
        let sandboxes = Sandboxes::new(
            published,
            (self.id.clone(), &self.placed, &self.capacity),
            events,
        );
        let log = Log::create(host, &self.dir)?;
        let payloads = Payloads::create(host, &self.dir)?;
        // The threads of the run's children borrow the run's workflow and sandboxes, and the scope
        // ends once each has, so that no sandbox starts after the run's end. The loop runs until
        // every one of them has ended, as each of them reaches its session through it.
        thread::scope(|scope| {
            let mut io: Io<'_, '_, H> = Io::new(backend, self.capacity.operations(), inbox);
            let events = io.events();
            let remote = io.events();
            let sessions = Sessions::new(
                start,
                Arc::new(move |request| remote.send(Input::Session(request))),
            );
            let mut runtime = Runtime {
                placed: &self.placed,
                routes: &self.routes,
                served: BTreeMap::new(),
                budgets: self.budgets.unspent(),
                dir: &self.dir,
                sandboxes: &sandboxes,
                bodies,
                clock,
                start,
                at: Duration::ZERO,
                unix_hard_end: unix.saturating_add(lasts(&self.placed)),
                log,
                payloads,
                scope,
                sessions,
                disk: Disk::default(),
                events,
                threads: 0,
                inputs: VecDeque::new(),
                next_token: 0,
                purposes: BTreeMap::new(),
                failure: None,
                inventory: Inventory::default(),
                starts: BTreeMap::new(),
                deletes: BTreeMap::new(),
                agents: BTreeMap::new(),
                gate: None,
                residents: BTreeMap::new(),
                unanswered: BTreeMap::new(),
                waiting: BTreeMap::new(),
                next_op: 0,
                next_model_request: 0,
                held_at_lock: BTreeSet::new(),
                jobs: BTreeMap::new(),
                leases: BTreeMap::new(),
                pending: VecDeque::new(),
            };
            let outcome = runtime.drive(&mut io, unix);
            runtime.finish(&mut io, outcome)
        })
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
    orders: Sender<Message<crate::relay::Order>>,
    // By when what the call left is collected, as a time since the run started; its leases' too.
    collect: Duration,
    // The trees of the call's sandbox that its last op came with; a transfer takes them.
    trees: Option<Trees>,
    // The role that runs the call, whose model route serves once the call's command or body
    // starts.
    target: TargetId,
    instance: u32,
    owner: Option<AgentId>,
    // How many of the call's model requests its route has barred for a spent model budget.
    model_requests_over_budget: u64,
}

enum Asked {
    Agent(AgentId, Unanswered),
    Job(JobId),
    Body(JobId, scoped::Waiting),
}

/// What a token in `purposes` names: the step whose end it is. A start's token is in `starts`, a
/// delete's in `deletes`, and a sweep's in none.
#[derive(Clone, Copy, Debug)]
enum Purpose {
    Agent(AgentId, agent::Step),
    // The sync of the run log's lines so far, which the loop waits for.
    Logged,
}

struct Runtime<'r, 's, H: Host> {
    placed: &'r Placed,
    routes: &'r ByRole<Vec<Serving>>,
    // The model routes of the callers whose roles have one.
    served: BTreeMap<Caller, Arc<Served>>,
    budgets: BTreeMap<RoleId, Budget<'r>>,
    dir: &'r Path,
    sandboxes: &'r Sandboxes<'r>,
    bodies: &'r dyn Bodies,
    // The run's clock, which its threads read.
    clock: H::Clock,
    // When the run started, on the loop's clock.
    start: Duration,
    // The time of the loop's batch, since the run started.
    at: Duration,
    unix_hard_end: UnixTime,
    log: Log,
    payloads: Payloads,
    scope: &'s Scope<'s, 'r>,
    sessions: Sessions,
    disk: Disk,
    events: Events<Input>,
    // How many of the run's threads still run, each of which ends with an `Input::Exited`.
    threads: usize,
    // What reached the loop, by when it did, for the core to take in order.
    inputs: VecDeque<(Duration, Input)>,
    next_token: u64,
    purposes: BTreeMap<u64, Purpose>,
    // Why a write of the record failed, which fails the run.
    failure: Option<String>,
    inventory: Inventory<'r>,
    starts: BTreeMap<u64, starts::Starting>,
    deletes: BTreeMap<u64, starts::Deleting>,
    agents: BTreeMap<AgentId, Agent>,
    // The start of the work, which waits for every agent and shared role to set up.
    gate: Option<agent::Gate>,
    residents: BTreeMap<TargetId, shared::Resident<'r, H>>,
    // The ops given to the core that it hasn't replied to: at most one of each sandbox's, whose
    // session hands out an op only once the one before it has been answered.
    unanswered: BTreeMap<OpId, Asked>,
    waiting: BTreeMap<LeaseId, scoped::Waiter>,
    next_op: u64,
    next_model_request: u64,
    // The callers whose routes had no key refused when a submission was accepted for good.
    held_at_lock: BTreeSet<Caller>,
    jobs: BTreeMap<JobId, Called>,
    leases: BTreeMap<LeaseId, Sender<Message<lease::Order>>>,
    // Events known already, which the loop gives the core before it takes another input.
    pending: VecDeque<(Duration, Event)>,
}

/// What the run's threads send the loop.
enum Input {
    Agent(AgentId, Arrival),
    Shared(TargetId, Arrival),
    Child(Report),
    Logged(Entry),
    Session(gateway::Request),
    Start(starts::Asked),
    Delete(starts::Delete),
    Dispose(starts::Dispose),
    Edge(starts::Edge),
    // One of the run's threads has ended.
    Exited,
    // An event that the loop itself observed.
    Core(Event),
    // Why an agent's sandbox didn't start, once what it made is disposed of, which the core's
    // turns note, and whether its teardown was asked for meanwhile.
    Unstarted(AgentId, String, bool),
}

impl<'r, 's, H: Host + 'r> stdx::io::Loop for Runtime<'r, 's, H> {
    type Event = Input;

    fn event(&mut self, io: &mut Io<'r, 's, H>, input: Input) {
        self.read_clock(io);
        match input {
            Input::Session(request) => Sessions::serve(self, io, request),
            Input::Start(asked) => self.asked(io, asked),
            Input::Delete(starts::Delete { which, id, reply }) => {
                self.begin_delete(io, which, id, starts::DeleteWaiter::Thread(reply));
            }
            Input::Dispose(starts::Dispose {
                which,
                id,
                reason,
                reply,
            }) => self.dispose(
                io,
                (which, id),
                &reason,
                starts::DisposeWaiter::Thread(reply),
            ),
            Input::Edge(edge) => self.edge(io, edge),
            Input::Exited => self.threads -= 1,
            input @ (Input::Agent(..)
            | Input::Shared(..)
            | Input::Child(_)
            | Input::Core(_)
            | Input::Unstarted(..)
            | Input::Logged(_)) => self.inputs.push_back((self.at, input)),
        }
    }
}

impl<'r, 's, H: Host + 'r> gateway::Owner for Runtime<'r, 's, H> {
    fn sessions(&mut self) -> &mut Sessions {
        &mut self.sessions
    }

    fn arrived(&mut self, io: &mut Io<'r, 's, H>, session: SessionId, arrival: Arrival) {
        self.read_clock(io);
        let agent = (self.agents.iter()).find(|(_, agent)| agent.session == Some(session));
        if let Some((&id, _)) = agent {
            self.inputs.push_back((self.at, Input::Agent(id, arrival)));
        }
    }

    fn answered(&mut self, io: &mut Io<'r, 's, H>, token: u64, answer: Answer) {
        self.read_clock(io);
        if let Some(&Purpose::Agent(agent, step)) = self.purposes.get(&token) {
            self.purposes.remove(&token);
            return self.agent_answered(io, (agent, step, token), answer);
        }
        self.start_answered(io, token, answer);
    }
}

impl<'r, 's, H: Host + 'r> disk::Owner for Runtime<'r, 's, H> {
    fn disk(&mut self) -> &mut Disk {
        &mut self.disk
    }

    fn written(&mut self, io: &mut Io<'r, 's, H>, token: u64, written: Result<(), String>) {
        self.read_clock(io);
        if self.starts.contains_key(&token) {
            return self.start_written(io, token, written);
        }
        if let Some(delete) = self.deletes.remove(&token) {
            return self.delete_written(io, (token, delete), written);
        }
        // The run log's sync, which the loop waits for, or the record of a sweep or of a start that
        // has failed, which nothing waits for.
        if let Some(Purpose::Logged) = self.purposes.get(&token) {
            self.purposes.remove(&token);
        }
        if let Err(error) = written {
            self.fail(error);
        }
    }
}

impl<'r, 's, H: Host + 'r> Runtime<'r, 's, H> {
    fn workflow(&self) -> &'r Workflow {
        self.placed.workflow()
    }

    /// The time of `io`'s batch, since the run started.
    fn now(&self, io: &Io<'r, 's, H>) -> Duration {
        io.now().saturating_sub(self.start)
    }

    // Takes the time of `io`'s batch as the run's, which what the loop logs then carries.
    fn read_clock(&mut self, io: &Io<'r, 's, H>) {
        self.at = self.now(io);
        self.log.at(self.at);
    }

    /// A token that no other step of the loop's has.
    fn new_token(&mut self) -> u64 {
        let token = self.next_token;
        self.next_token += 1;
        token
    }

    fn token(&mut self, purpose: Purpose) -> u64 {
        let token = self.new_token();
        self.purposes.insert(token, purpose);
        token
    }

    /// Notes that the record couldn't be written, which fails the run.
    fn fail(&mut self, error: String) {
        self.failure = Some(match self.failure.take() {
            Some(before) => format!("{before}; {error}"),
            None => error,
        });
    }

    fn logged(&mut self, entry: Entry) {
        self.count(&entry);
        if let Err(error) = self.log.entry(entry) {
            self.fail(error);
        }
    }

    /// Runs `run` on a thread of the run's, named `name`, with a stack of `stack_bytes` if given,
    /// which the loop waits for at the run's end. Fails if the kernel gives no thread.
    fn spawn(
        &mut self,
        (name, stack_bytes): (&str, Option<usize>),
        run: impl FnOnce() + Send + 's,
    ) -> Result<(), String> {
        let events = self.events.clone();
        let mut builder = thread::Builder::new().name(name.to_owned());
        if let Some(bytes) = stack_bytes {
            builder = builder.stack_size(bytes);
        }
        let spawned = builder.spawn_scoped(self.scope, move || {
            run();
            events.send(Input::Exited);
        });
        spawned
            .map(|_| self.threads += 1)
            .map_err(|error| format!("starting the run's thread `{name}` failed: {error}"))
    }

    fn drive(&mut self, io: &mut Io<'r, 's, H>, unix: UnixTime) -> Result<Outcome, Stopped> {
        let (mut core, actions) = Core::start(self.placed);
        self.log
            .start(self.workflow(), unix, self.hard_end(), &actions)?;
        self.synced(io)?;
        let end = match self.turns(io, &mut core, actions) {
            // As the loop heeds it, so that the record of each sandbox's end after it tells how
            // long the stop took.
            Err(interrupted @ Stopped::Interrupted(_)) => {
                let noted = self.log.note(&format!("the run {interrupted}"));
                failed_also(Err(interrupted), noted)
            }
            end => end,
        };
        self.stop_children();
        // Each route is ended once the core stops allowing its model requests, unless the run
        // ended first: this ends them then. The run's end still waits on their exchanges, within
        // the upstreams' bounds and the responses' deadlines.
        self.end_routes(None);
        // A stop waits for a child whose provider's create is under way, which nothing cuts short
        // but its own deadline: a sandbox that a create made after the sweep's list would outlive
        // the run, until its hard end, so the create's return, and the start's delete of what it
        // made, come before the sweep.
        self.settle_threads(io);
        // Only the core's end drains the inputs into the core: after a failure of the runtime's
        // own, such as a record it couldn't write, or an interruption, the core may still be
        // taking ops, and would decide on ones that nothing acts on; the run log still takes what
        // the run's children noted and deleted as they stopped.
        match end {
            Ok(end) => failed_also(end.map_err(Stopped::Failed), self.drain(io, &mut core)),
            Err(stopped) => failed_also(Err(stopped), self.settle()),
        }
    }

    // Runs the loop until every thread of the run has ended, and nothing of the agent's is under
    // way: its start, and its teardown, which collects what the record keeps of it.
    fn settle_threads(&mut self, io: &mut Io<'r, 's, H>) {
        while self.threads > 0
            || !self.starts.is_empty()
            || !self.deletes.is_empty()
            || self.agents.values().any(Agent::tearing_down)
        {
            self.run(io, Duration::MAX);
        }
    }

    /// Runs the loop's next batch, by `until`, a time since the run started, at the latest: by the
    /// earliest deadline of a session's or a start's look, if one comes first.
    fn run(&mut self, io: &mut Io<'r, 's, H>, until: Duration) {
        let until = match self.sessions.due() {
            Some(due) => until.min(due.saturating_sub(self.start)),
            None => until,
        };
        let until = self.looks_due().map_or(until, |due| until.min(due));
        io.run_until(self, self.start.saturating_add(until));
        self.read_clock(io);
        Sessions::tick(self, io);
        self.look(io);
        if interruption::signal().is_some() {
            self.stop_meetings(io);
        }
    }

    /// Waits for the run log's lines so far to last.
    fn synced(&mut self, io: &mut Io<'r, 's, H>) -> Result<(), String> {
        let (file, path) = self.log.handle()?;
        self.written_whole(io, disk::Job::Sync(file, path, Durable::Data))
    }

    /// Waits for `job` to make what it writes last.
    fn written_whole(&mut self, io: &mut Io<'r, 's, H>, job: disk::Job) -> Result<(), String> {
        let token = self.token(Purpose::Logged);
        Disk::push(self, io, job, token);
        while self.purposes.contains_key(&token) {
            self.run(io, Duration::MAX);
        }
        self.failure.take().map_or(Ok(()), Err)
    }

    // Ends what is left of the run: its routes' failures, the sweep, the last records, and the
    // loop's sessions and syncs.
    fn finish(
        &mut self,
        io: &mut Io<'r, 's, H>,
        mut outcome: Result<Outcome, Stopped>,
    ) -> Result<Outcome, Stopped> {
        // In-flight exchanges have ended, so each route has its final status.
        let keyed = self.routes_held(&mut outcome);
        let outcome = failed_also(failed_also(outcome, keyed), self.sweep(io));
        let outcome = failed_also(outcome, self.settle());
        let noted = match &outcome {
            // An interruption is noted as the loop heeds it (`Runtime::drive`).
            Ok(_) | Err(Stopped::Interrupted(_)) => Ok(()),
            Err(failed @ Stopped::Failed(_)) => self.log.note(&format!("the run {failed}")),
        };
        // The log's last records may have led to no act, and so be unsynced: they last before
        // `outcome.json` says the run has ended, or the run fails.
        let synced = self.synced(io);
        // Every session ends, and the loop waits for what is in flight on each.
        for agent in std::mem::take(&mut self.agents).into_values() {
            if let Some(session) = agent.session {
                Sessions::drop_session(self, io, session);
            }
        }
        while self.sessions.busy() || self.disk.busy() {
            self.run(io, Duration::MAX);
        }
        failed_also(failed_also(outcome, noted), synced)
    }

    fn launched(&mut self, id: AgentId, at: Duration) -> Result<(), String> {
        if self.agents.get_mut(&id).is_some_and(Agent::record_start) {
            self.log.entry(Entry::AgentLaunched(id, at))?;
        }
        Ok(())
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

    fn op(&mut self, asked: Asked, request: Request) -> Event {
        let op = OpId(self.next_op);
        self.next_op += 1;
        let caller = match asked {
            Asked::Agent(id, _) => Caller::Agent(id),
            Asked::Job(job) | Asked::Body(job, _) => Caller::Job(job),
        };
        self.unanswered.insert(op, asked);
        Event::OpArrived {
            op,
            caller,
            request,
        }
    }

    fn refuse(
        &mut self,
        io: &mut Io<'r, 's, H>,
        id: AgentId,
        refused: RefusedOp,
        by: Duration,
    ) -> Result<(), String> {
        self.log.refused(Caller::Agent(id), &refused)?;
        self.synced(io)?;
        let what = "the refusal of an op whose payload was over the agent's limit";
        self.answer(io, id, what, by, Ask::Refuse(refused))
    }

    #[expect(clippy::too_many_lines, reason = "one arm per action")]
    fn perform(
        &mut self,
        io: &mut Io<'r, 's, H>,
        core: &Core<'_>,
        actions: Vec<Action>,
    ) -> Result<Option<Result<Outcome, String>>, Stopped> {
        let mut end = None;
        for action in actions {
            match action {
                Action::StartAgents => self.start_agents(io)?,
                Action::Reply { op, reply } => self.reply(io, op, reply, core)?,
                Action::StartFinalCall {
                    job,
                    selections,
                    timeout,
                } => self.start_final_call(io, job, selections, timeout)?,
                Action::StartCall {
                    job,
                    function,
                    payload,
                    arguments,
                    timeout,
                } => {
                    let call = (&payload, arguments);
                    self.start_call(core, job, function, call, timeout)?;
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
                    self.start_lease(core, start, (holder, grant))?;
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
                        required_output: self.waiting.contains_key(&lease),
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
                    let holder = self.holder_trees(lease, holder).in_group(lease.0);
                    let transfer = lease::Transfer {
                        number,
                        direction,
                        max_bytes,
                        holder,
                    };
                    let order = lease::Order::Transfer(transfer);
                    self.send_lease(lease, Message::Order(order));
                }
                Action::CloseLease { lease } => {
                    self.send_lease(lease, Message::Stop);
                    // The lease's transfer ends with it, as the stop reaches its thread.
                    Sessions::cancel(self, io, lease.0);
                }
                Action::TearDownAgent { agent } => self.tear_down_agent(io, agent),
                Action::TearDownShared { target } => {
                    if let Some(resident) = self.residents.get_mut(&target) {
                        resident.close();
                    }
                }
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

    fn reply(
        &mut self,
        io: &mut Io<'r, 's, H>,
        op: OpId,
        reply: OpReply,
        core: &Core<'_>,
    ) -> Result<(), String> {
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
        let (id, unanswered) = match asked {
            Asked::Agent(id, unanswered) => (id, unanswered),
            Asked::Body(job, waiting) => return self.watch(core, job, waiting, reply),
            Asked::Job(job) => {
                // A call whose thread has ended has no op left to answer.
                let _ = self
                    .call(job)
                    .orders
                    .send(Message::Order(relay::Order::Sandbox(reply)));
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
                if accepts(&reply)
                    && let Some(job) = self.payloads.store(&payload, &reference)?
                {
                    self.written_whole(io, job)?;
                }
                incoming
            }
        };
        let what = format!("op {}'s reply", op.0);
        let by = core.agent_due(id);
        self.answer(io, id, &what, by, Ask::Reply(incoming, reply))
    }

    fn answer(
        &mut self,
        io: &mut Io<'r, 's, H>,
        id: AgentId,
        what: &str,
        by: Duration,
        answer: Ask,
    ) -> Result<(), String> {
        let session = self.agents.get(&id).and_then(|agent| agent.session);
        match session {
            // A write past `by` would end the session, which the agent's teardown still needs, for
            // a delay of the host's own, such as hashing the payload, the log's sync or storing the
            // payload. The op stays in flight, which wedges the agent's ops, but only until the
            // core's next act: while the agent works, `by` is the end of the work phase
            // (`Core::tick_due`), so that act tears the agent down; after `Abandon`, the run's end
            // drops it.
            Some(_) if self.at >= by => self.log.note(&format!(
                "{what} wasn't sent: it was due by the core's next act, which has come"
            )),
            Some(session) => {
                let token = self.token(Purpose::Agent(id, agent::Step::Answer));
                if let Some(agent) = self.agents.get_mut(&id) {
                    agent.answering.insert(token, what.to_owned());
                }
                Sessions::ask(self, io, session, answer, (by, token));
                Ok(())
            }
            None => self
                .log
                .note(&format!("{what} wasn't sent: the agent's sandbox is gone")),
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
            Caller::Agent(id) => (self.agents.get(&id))
                .and_then(|agent| agent.session)
                .map(|session| self.sessions.trees(session)),
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

    fn stop_children(&mut self) {
        for resident in self.residents.values_mut() {
            resident.close();
        }
        for call in self.jobs.values() {
            let _ = call.orders.send(Message::Stop);
        }
        for stop in self.leases.values() {
            let _ = stop.send(Message::Stop);
        }
    }
}
