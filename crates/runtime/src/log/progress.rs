//! What a run's log says of the run so far: a [`Progress`], and the parts of the log that this
//! build couldn't read ([`Unread`]).
//!
//! Each line of the log moves a `Reading` on, and a [`Readout`] completes it with what the log
//! leaves out: each role's place, startup time and prices, from `ir.json` and `provider.json`
//! (`Roles`), and what the record's directories hold when the readout is made, such as a sandbox's
//! `sandbox.json`. Every `Duration` is a time since the run started, by the
//! run's clock.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;
use std::{fmt, mem};

use provider::UnixTime;
use schema::ir::Gpu;
use schema::sandbox::ExitStatus;
use schema::sandbox::op::{CommandEnd, TransferEnd};
use serde_json::Value;

use super::Direction;
use super::call::{CallEnd, CallOf, CallSpan};
use super::logged::{self, CommandOf, Line};
use super::model_use::{Answered, ModelUse};
use super::roles::{Grant, Roles};
use crate::TokenPrices;
use crate::read_error::{ReadError, form, read_optional};
use crate::sandbox::SandboxName;

/// Where a sandbox runs: a class of a provider, each by the name that `provider.json` gives it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Place {
    pub provider: String,
    pub class: String,
}

/// A run as its log tells it, up to the last line read. A time is `None` until the log says that it
/// happened.
#[derive(Clone, Debug)]
pub struct Progress {
    /// When the run started, by the machine's clock.
    pub started: UnixTime,
    /// When the core abandons the run if it is still going.
    pub hard_end: Duration,
    /// The time all agents may work from their common start, if the IR gives it.
    pub deadline: Option<Duration>,
    /// When the latest line read was written.
    pub last: Duration,
    pub agents: BTreeMap<logged::CallerName, AgentSpan>,
    pub shared: BTreeMap<usize, SharedSpan>,
    // The provider listed an allocation the driver could not associate with a sandbox record.
    pub(super) unplaced_sandbox: bool,
    /// When the agents began work on their common clock.
    pub work_started: Option<Duration>,
    /// Each call, in the order the log started them.
    pub calls: Vec<CallSpan>,
    /// Each lease, in the order the log started them.
    pub leases: Vec<LeaseSpan>,
    /// The model requests of every role.
    pub model: ModelUse,
    /// When the last agent's work ended.
    pub work_end: Option<Duration>,
    /// When the core finished or abandoned the run.
    pub end: Option<Duration>,
    /// The price in US dollars an hour of each class of the run's providers that declared one, as
    /// `provider.json` records it.
    pub prices: BTreeMap<Place, f64>,
    /// The prices of the tokens of each model of the run's routes that has them, by the route's
    /// name for it, as `provider.json` records them.
    pub token_prices: BTreeMap<String, TokenPrices>,
}

/// What a run's log tells of it, and what of the log couldn't be read.
#[derive(Debug)]
pub struct Readout {
    /// `None` if the log holds no `run_started` that gives a hard end, as none does of a log not
    /// yet begun, or whose `run_started` has no `hard_end_ns`.
    pub progress: Option<Progress>,
    /// The parts of the log that were left out, in the order they were met.
    pub unread: Vec<Unread>,
}

/// A part of the log that this build couldn't read, and why: it is left out of the readout and
/// listed, so that no content of a log fails a read.
#[derive(Clone, Debug)]
pub struct Unread {
    /// Its line's number in the log, from 1.
    pub line: u64,
    /// Which part of the line.
    pub part: Part,
    /// What is wrong with it, and what a reader expects.
    pub reason: String,
}

/// The part of a line that couldn't be read: the whole line, which is no JSON object of a line's
/// shape, its event, or one of its actions, by its index from 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    Line,
    Event,
    Action(usize),
}

/// Where it is, and why it doesn't read: `line 4: <reason>`, `line 4, its event: <reason>` or, for
/// the action at index 1, `line 4, its action 2: <reason>`.
impl fmt::Display for Unread {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { line, part, reason } = self;
        match part {
            Part::Line => write!(f, "line {line}: {reason}"),
            Part::Event => write!(f, "line {line}, its event: {reason}"),
            Part::Action(index) => write!(f, "line {line}, its action {}: {reason}", index + 1),
        }
    }
}

/// A sandbox of the run, which one of the agent, a call or a lease holds. Its times are those the
/// log gave it, and `None` while it hasn't.
#[derive(Clone, Debug, Default)]
pub struct Sandbox {
    /// Where its role is placed; `None` if `provider.json` doesn't place it.
    pub place: Option<Place>,
    /// How long its class may take to start it.
    pub startup: Option<Duration>,
    /// The GPUs its role asks for.
    pub needs: Option<Gpu>,
    /// The GPUs its `sandboxd` saw, once the log says so.
    pub gpus: Option<Vec<String>>,
    /// When the core started it: 0 for the agent's.
    pub asked: Duration,
    /// When its command started, or its runner was ready.
    pub started: Option<Duration>,
    /// Its start as the runtime timed it, once the log says that its provider's create returned.
    /// `None` before then, for a create that failed, and in a log of a build that didn't time it.
    pub start: Option<SandboxStart>,
    /// When the agent's sandbox was deleted or its teardown finished without a sandbox, a call's
    /// sandbox was deleted, a call with no sandbox of its own ended, or a lease ended. The deletion
    /// also ends a sandbox when its run stops early.
    pub gone: Option<Duration>,
    /// Whether its `sandbox.json` records the provider's delete as done; `None` if it has no
    /// `sandbox.json`, so was never created.
    pub deleted: Option<bool>,
}

/// A sandbox's start, between when the core asked for it ([`Sandbox::asked`]) and when it started
/// ([`Sandbox::started`]).
#[derive(Clone, Debug)]
pub struct SandboxStart {
    /// When the runtime began its provider's create.
    pub began: Duration,
    /// When its provider's create returned.
    pub created: Duration,
    /// When its session met the gateway, once the log says so.
    pub met: Option<Duration>,
    /// The steps of the create that its provider timed, in the order it gave them. They may
    /// overlap or leave gaps.
    pub phases: Vec<StartPhase>,
}

/// A step of a sandbox's create, as its provider named and timed it.
#[derive(Clone, Debug)]
pub struct StartPhase {
    /// What the step did, in its provider's words, such as `lookup`.
    pub name: String,
    pub began: Duration,
    /// How long it took: a duration, not a time since the run started.
    pub took: Duration,
}

impl From<&logged::Phase> for StartPhase {
    fn from(phase: &logged::Phase) -> Self {
        let logged::Phase {
            name,
            began_ns,
            took_ns,
        } = phase;
        Self {
            name: name.clone(),
            began: *began_ns,
            took: *took_ns,
        }
    }
}

/// The agent: its sandbox, its command's end, and its model requests.
#[derive(Clone, Debug, Default)]
pub struct AgentSpan {
    pub role: String,
    /// When the driver began tearing down this member.
    pub work_end: Option<Duration>,
    pub sandbox: Sandbox,
    /// When, and how, its command ended.
    pub end: Option<(Duration, AgentEnd)>,
    /// Its own model requests, which the run's (`Progress::model`) count too; `None` if its role
    /// has no model route.
    pub model: Option<ModelUse>,
}

/// One work-lifetime sandbox for all calls that name this target.
#[derive(Clone, Debug, Default)]
pub struct SharedSpan {
    pub role: String,
    pub sandbox: Sandbox,
    pub failure: Option<String>,
}

/// How the agent's command ended.
#[derive(Clone, Debug)]
pub enum AgentEnd {
    /// It exited, or a signal killed it.
    Exited(ExitStatus),
    /// The run couldn't start it or keep its sandbox, for this reason.
    Failed(String),
}

/// A lease: a runner that the agent or a call holds, to run commands and move trees on.
#[derive(Clone, Debug)]
pub struct LeaseSpan {
    /// Its number in the run.
    pub lease: u64,
    /// The grant that gave it, by name.
    pub grant: String,
    /// The agent or job that owns the lease.
    pub holder: logged::CallerName,
    pub sandbox: Sandbox,
    /// How long it may last: its grant's `instance_seconds`, or less if its open asked for less.
    pub ready_for: Option<Duration>,
    /// When the core ended it, and why.
    pub closed: Option<(Duration, Closed)>,
    /// How many commands and transfers it ran.
    pub commands: u64,
    /// The latest of them.
    pub last: Option<Work>,
    /// Why its runner failed, if it did.
    pub failed: Option<String>,
    busy: Duration,
    working: Option<Duration>,
}

/// Who closed a lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Closed {
    /// Its holder asked.
    Asked,
    /// The work phase ended, which tears down the agent that held it.
    WorkEnded,
    /// The call that held it ended.
    CallEnded,
    /// The lease's time, or its grant's, ran out.
    Expired,
}

/// What a lease last ran, and how that ended, once it has.
#[derive(Clone, Debug)]
pub enum Work {
    Command {
        argv: Vec<String>,
        end: Option<CommandEnd>,
    },
    Transfer {
        direction: Direction,
        from: String,
        to: String,
        end: Option<TransferEnd>,
    },
}

impl Work {
    fn asked(request: &logged::Request) -> Option<Self> {
        match request {
            logged::Request::LeaseRun { argv } => Some(Self::Command {
                argv: argv.clone(),
                end: None,
            }),
            logged::Request::LeasePut { tree, to, replace } => Some(Self::Transfer {
                direction: if *replace {
                    Direction::Replace
                } else {
                    Direction::Put
                },
                from: tree.clone(),
                to: to.clone(),
                end: None,
            }),
            logged::Request::LeaseGet { from, to } => Some(Self::Transfer {
                direction: Direction::Get,
                from: from.clone(),
                to: to.clone(),
                end: None,
            }),
            logged::Request::LeaseOpen { .. }
            | logged::Request::LeaseClose
            | logged::Request::Other => None,
        }
    }
}

impl LeaseSpan {
    /// How long its runner was charged for until `now`: from when it was ready until the core
    /// closed the lease, or its sandbox was gone.
    pub fn charged(&self, now: Duration) -> Duration {
        let Some(ready) = self.sandbox.started else {
            return Duration::ZERO;
        };
        self.until(now).saturating_sub(ready)
    }

    /// How much of the time it was charged for until `now` a command or transfer of its ran: from
    /// when the core accepted it until it ended, or the core closed the lease, or its sandbox was
    /// gone, whichever came first.
    pub fn busy(&self, now: Duration) -> Duration {
        let working = (self.working).map_or(Duration::ZERO, |since| {
            self.until(now).saturating_sub(since)
        });
        self.busy + working
    }

    fn until(&self, now: Duration) -> Duration {
        let closed = self.closed.map(|(at, _)| at);
        closed.or(self.sandbox.gone).unwrap_or(now)
    }

    fn command_ended(&mut self, at: Duration, end: CommandEnd) {
        if let Some(Work::Command { end: last, .. }) = &mut self.last {
            *last = Some(end);
            self.rest(at);
        }
    }

    fn transfer_ended(&mut self, at: Duration, end: TransferEnd) {
        if let Some(Work::Transfer { end: last, .. }) = &mut self.last {
            *last = Some(end);
            self.rest(at);
        }
    }

    fn rest(&mut self, at: Duration) {
        if let Some(since) = self.working.take() {
            self.busy += self.until(at).saturating_sub(since);
        }
    }
}

pub(super) fn readout(
    dir: &Path,
    roles: Option<&Roles>,
    mut reading: Reading,
    unread: Vec<Unread>,
) -> Result<Readout, ReadError> {
    let (Some(roles), Some((started, hard_end))) = (roles, reading.started) else {
        return Ok(Readout {
            progress: None,
            unread,
        });
    };
    reading.place_roles(dir, roles)?;
    let Reading {
        started: _,
        last,
        agents,
        shared,
        unplaced_sandbox,
        work_started,
        calls,
        leases,
        model,
        work_end,
        end,
    } = reading;
    let mut progress = Progress {
        started,
        hard_end,
        deadline: roles.deadline,
        last,
        agents,
        shared,
        unplaced_sandbox,
        work_started,
        calls,
        leases,
        model,
        work_end,
        end,
        prices: roles.prices.clone(),
        token_prices: roles.token_prices.clone(),
    };
    // Each sandbox's `sandbox.json` is rewritten as the run deletes it, so it is read now.
    for lease in &mut progress.leases {
        lease.sandbox.deleted = deleted(&dir.join(format!("leases/{}", lease.lease)))?;
    }
    for call in &mut progress.calls {
        if call.shared.is_none() {
            let job = dir.join(format!("jobs/{}", call.job));
            call.sandbox.deleted = deleted(&job)?;
        }
        // A call with no sandbox of its own, none created or a shared one, holds none once it ends.
        if call.sandbox.deleted.is_none() {
            let ended = call.end.as_ref().map(|(at, ..)| *at);
            call.sandbox.gone = call.sandbox.gone.or(ended);
        }
    }
    Ok(Readout {
        progress: Some(progress),
        unread,
    })
}

fn set_place(
    sandbox: &mut Sandbox,
    roles: &Roles,
    role: &str,
    dir: &Path,
) -> Result<(), ReadError> {
    let deleted = deleted(dir)?;
    // What the log told of the sandbox stays, and its role gives the rest.
    let Sandbox {
        place: _,
        startup: _,
        needs: _,
        gpus,
        asked: _,
        started,
        start,
        gone,
        deleted: _,
    } = mem::take(sandbox);
    *sandbox = Sandbox {
        gpus,
        started,
        start,
        gone,
        deleted,
        ..roles.sandbox(Some(role), Duration::ZERO)
    };
    Ok(())
}

/// The JSON in `path`, or `null` if there is no such file.
pub(super) fn json_file(path: &Path) -> Result<Value, ReadError> {
    match read_optional(path)? {
        Some(bytes) => serde_json::from_slice(&bytes).map_err(form(path.display())),
        None => Ok(Value::Null),
    }
}

/// Each role's place, by the role's name, from the `roles` of `provider.json`'s content `provided`;
/// a role without a complete place is left out.
pub(crate) fn places(provided: &Value) -> BTreeMap<String, Place> {
    let roles_placed = provided["roles"].as_object().into_iter().flatten();
    let places = roles_placed.filter_map(|(role, placed)| {
        let place = Place {
            provider: placed["provider"].as_str()?.to_owned(),
            class: placed["class"].as_str()?.to_owned(),
        };
        Some((role.clone(), place))
    });
    places.collect()
}

fn deleted(dir: &Path) -> Result<Option<bool>, ReadError> {
    let sandbox = json_file(&dir.join("sandbox.json"))?;
    Ok((!sandbox.is_null()).then(|| sandbox["delete"]["state"] == "deleted"))
}

#[derive(Clone, Default)]
pub(super) struct Reading {
    started: Option<(UnixTime, Duration)>,
    last: Duration,
    agents: BTreeMap<logged::CallerName, AgentSpan>,
    shared: BTreeMap<usize, SharedSpan>,
    unplaced_sandbox: bool,
    work_started: Option<Duration>,
    calls: Vec<CallSpan>,
    leases: Vec<LeaseSpan>,
    model: ModelUse,
    work_end: Option<Duration>,
    end: Option<Duration>,
}

impl Reading {
    fn place_roles(&mut self, dir: &Path, roles: &Roles) -> Result<(), ReadError> {
        for (caller, role) in &roles.agents {
            let agent = self.agents.entry(*caller).or_default();
            let path = dir.join("agents").join(&role.name);
            agent.role.clone_from(&role.name);
            set_place(&mut agent.sandbox, roles, &role.name, &path)?;
            agent.model = agent
                .model
                .take()
                .or_else(|| role.routed.then(ModelUse::default));
        }
        for (name, &id) in &roles.shared {
            let service = self.shared.entry(id).or_default();
            service.role.clone_from(name);
            set_place(
                &mut service.sandbox,
                roles,
                name,
                &dir.join("shared").join(name),
            )?;
        }
        Ok(())
    }
}

impl Reading {
    pub(super) fn take(&mut self, line: &Line, roles: &Roles) {
        self.last = self.last.max(line.at_ns);
        let at = line.when();
        let event = line.event.as_ref();
        if let Some(event) = event {
            self.event(at, event);
        }
        let request = event.and_then(logged::Event::request);
        let closed = if matches!(request, Some(logged::Request::LeaseClose)) {
            Closed::Asked
        } else if (line.actions.iter())
            .any(|action| matches!(action, logged::Action::TearDownAgent { .. }))
        {
            Closed::WorkEnded
        } else {
            Closed::Expired
        };
        // A job's end closes the leases it holds.
        let ended = event.and_then(logged::Event::job);
        // A holder may ask, at open, for a shorter life than its grant's.
        let asked = match request {
            Some(logged::Request::LeaseOpen { instance_seconds }) => {
                instance_seconds.map(Duration::from_secs)
            }
            Some(
                logged::Request::LeaseRun { .. }
                | logged::Request::LeasePut { .. }
                | logged::Request::LeaseGet { .. }
                | logged::Request::LeaseClose
                | logged::Request::Other,
            )
            | None => None,
        };
        let mut work = request.and_then(Work::asked);
        for action in &line.actions {
            if let logged::Action::RunCommand { lease } | logged::Action::Transfer { lease } =
                action
                && let Some(work) = work.take()
            {
                self.lease_ran(at, *lease, work);
            }
            self.action(at, action, (closed, ended), roles);
            if let (Some(asked), logged::Action::StartLease { lease, .. }) = (asked, action)
                && let Some(lease) = self.lease(*lease)
            {
                lease.ready_for = Some(lease.ready_for.map_or(asked, |most| most.min(asked)));
            }
        }
    }

    #[expect(clippy::too_many_lines, reason = "one arm per kind of event")]
    fn event(&mut self, at: Duration, event: &logged::Event) {
        match event {
            logged::Event::RunStarted {
                unix_ns,
                hard_end_ns,
            } => self.started = hard_end_ns.map(|hard_end| (*unix_ns, hard_end)),
            logged::Event::WorkStarted => self.work_started = Some(at),
            logged::Event::SharedStarted { shared } => {
                self.shared.entry(*shared).or_default().sandbox.started = Some(at);
            }
            logged::Event::SharedFailed { shared, reason } => {
                self.shared.entry(*shared).or_default().failure = Some(reason.clone());
            }
            logged::Event::SharedTornDown { shared } => {
                self.shared
                    .entry(*shared)
                    .or_default()
                    .sandbox
                    .gone
                    .get_or_insert(at);
            }
            logged::Event::AgentStarted { caller } => {
                self.agents.entry(*caller).or_default().sandbox.started = Some(at);
            }
            logged::Event::AgentExited { caller, status } => {
                self.agents.entry(*caller).or_default().end = Some((at, AgentEnd::Exited(*status)));
            }
            logged::Event::AgentFailed { caller, reason } => {
                let agent = self.agents.entry(*caller).or_default();
                agent.end = Some((at, AgentEnd::Failed(reason.clone())));
            }
            logged::Event::AgentTornDown { caller } => {
                self.agents
                    .entry(*caller)
                    .or_default()
                    .sandbox
                    .gone
                    .get_or_insert(at);
            }
            logged::Event::CallStarted { job } => {
                if let Some(call) = self.call(*job) {
                    call.sandbox.started = Some(at);
                }
            }
            logged::Event::CallTimedOut { job } => {
                if let Some(call) = self.call(*job) {
                    call.timed_out = true;
                }
            }
            logged::Event::CallReturned {
                job,
                returned,
                model_requests_over_budget,
            } => {
                let returned = returned.iter();
                let end = CallEnd::Result(
                    returned
                        .map(|shown| (shown.name.clone(), shown.value.clone()))
                        .collect(),
                );
                if let Some(call) = self.call(*job) {
                    call.end(at, end, *model_requests_over_budget);
                }
            }
            logged::Event::CallFailed {
                job,
                reason,
                model_requests_over_budget,
            } => {
                if let Some(call) = self.call(*job) {
                    let end = CallEnd::Failed(reason.clone());
                    call.end(at, end, *model_requests_over_budget);
                }
            }
            logged::Event::LeaseReady { lease } => {
                if let Some(lease) = self.lease(*lease) {
                    lease.sandbox.started = Some(at);
                }
            }
            logged::Event::CommandEnded {
                of: CommandOf::Lease { lease },
                end,
            } => {
                if let Some(lease) = self.lease(*lease) {
                    lease.command_ended(at, *end);
                }
            }
            logged::Event::TransferEnded { lease, end } => {
                if let Some(lease) = self.lease(*lease) {
                    lease.transfer_ended(at, end.clone());
                }
            }
            logged::Event::LeaseEnded { lease } => {
                if let Some(lease) = self.lease(*lease) {
                    lease.sandbox.gone = Some(at);
                    lease.failed = None;
                }
            }
            logged::Event::LeaseFailed { lease, reason } => {
                if let Some(lease) = self.lease(*lease) {
                    lease.sandbox.gone = Some(at);
                    lease.failed = Some(reason.clone());
                }
            }
            logged::Event::ModelRequestStarted { caller, request } => {
                self.model.in_flight.insert(*request, at);
                if let Some(model) = self.model_of(*caller) {
                    model.in_flight.insert(*request, at);
                }
            }
            logged::Event::ModelAnswered {
                caller,
                request,
                api,
                model,
                first_token_ns,
                upstream_status,
                answer,
                failed,
                usage,
                kept,
            } => {
                let answered = Answered {
                    request: *request,
                    model: api.zip(model.as_deref()),
                    dropped_headers: &kept.request.dropped_headers,
                    first_token: *first_token_ns,
                    upstream_status: *upstream_status,
                    answer,
                    failed: failed.as_ref(),
                    usage: usage.as_ref().map(route::Usage::from),
                };
                self.model.take(at, &answered);
                // The caller's own, which the run's count too.
                if let Some(model) = caller.and_then(|caller| self.model_of(caller)) {
                    model.take(at, &answered);
                }
            }
            logged::Event::SandboxCreated {
                sandbox,
                began_ns,
                phases,
            } => {
                // The runtime creates each sandbox once. A second line for one replaces the first.
                if let Some(sandbox) = self.named(*sandbox) {
                    sandbox.start = Some(SandboxStart {
                        began: *began_ns,
                        created: at,
                        met: None,
                        phases: phases.iter().map(StartPhase::from).collect(),
                    });
                }
            }
            logged::Event::SandboxMet { sandbox } => {
                if let Some(start) = self
                    .named(*sandbox)
                    .and_then(|sandbox| sandbox.start.as_mut())
                {
                    start.met = Some(at);
                }
            }
            logged::Event::SandboxGpus { sandbox, gpus } => {
                if let Some(sandbox) = self.named(*sandbox) {
                    sandbox.gpus = Some(gpus.clone());
                }
            }
            logged::Event::SandboxDeleted { sandbox } => {
                if let Some(name) = sandbox {
                    if let Some(sandbox) = self.named(*name) {
                        sandbox.gone.get_or_insert(at);
                    }
                } else {
                    self.unplaced_sandbox = true;
                }
            }
            // A call's commands show in no line of its own.
            logged::Event::CommandEnded {
                of: CommandOf::Job { .. },
                ..
            }
            | logged::Event::OpArrived { .. }
            | logged::Event::OpRefused
            | logged::Event::Tick
            | logged::Event::Unknown => {}
        }
    }
}

impl Reading {
    fn named(&mut self, name: SandboxName) -> Option<&mut Sandbox> {
        match name {
            SandboxName::Agent(role) => Some(
                &mut self
                    .agents
                    .entry(logged::CallerName::Agent(role))
                    .or_default()
                    .sandbox,
            ),
            SandboxName::Shared(index) => Some(&mut self.shared.entry(index).or_default().sandbox),
            SandboxName::Job(job) => self.call(job).map(|call| &mut call.sandbox),
            SandboxName::Lease(lease) => self.lease(lease).map(|lease| &mut lease.sandbox),
        }
    }

    fn model_of(&mut self, caller: logged::CallerName) -> Option<&mut ModelUse> {
        match caller {
            logged::CallerName::Job(job) => self.call(job).map(|call| &mut call.model),
            agent @ logged::CallerName::Agent(_) => Some(
                self.agents
                    .entry(agent)
                    .or_default()
                    .model
                    .get_or_insert_default(),
            ),
        }
    }

    fn call(&mut self, job: u64) -> Option<&mut CallSpan> {
        self.calls.iter_mut().find(|call| call.job == job)
    }

    fn lease(&mut self, lease: u64) -> Option<&mut LeaseSpan> {
        self.leases.iter_mut().find(|span| span.lease == lease)
    }
}

impl Reading {
    fn action(
        &mut self,
        at: Duration,
        action: &logged::Action,
        (closed, ended): (Closed, Option<u64>),
        roles: &Roles,
    ) {
        match action {
            logged::Action::StartCall {
                job,
                caller,
                grant,
                payload: _,
                timeout_ns,
            } => {
                let granted = roles
                    .agent_name(*caller)
                    .and_then(|role| roles.grants.get(&(role.to_owned(), grant.clone())))
                    .cloned()
                    .unwrap_or_default();
                let (of, role) = (CallOf::Function(grant.clone()), granted.role);
                let sandbox = roles.sandbox(role.as_deref(), at);
                let shared = role
                    .as_ref()
                    .and_then(|role| roles.shared.get(role))
                    .copied();
                let mut call = CallSpan::new(*job, Some(*caller), of, role, sandbox, *timeout_ns);
                call.shared = shared;
                self.calls.push(call);
            }
            logged::Action::StartFinalCall {
                job,
                selections: _,
                timeout_ns,
            } => {
                let role = roles.final_target.clone();
                let sandbox = roles.sandbox(role.as_deref(), at);
                let of = CallOf::Final;
                (self.calls).push(CallSpan::new(*job, None, of, role, sandbox, *timeout_ns));
            }
            logged::Action::StopCall { job } => {
                if let Some(call) = self.call(*job) {
                    call.stopped = true;
                }
            }
            logged::Action::StartLease {
                lease,
                holder,
                grant,
            } => self.open_lease(at, *lease, *holder, grant, roles),
            logged::Action::CloseLease { lease } => {
                if let Some(lease) = self.lease(*lease) {
                    let closed = match lease.holder {
                        logged::CallerName::Job(holder) if ended == Some(holder) => {
                            Closed::CallEnded
                        }
                        _ => closed,
                    };
                    lease.closed.get_or_insert((at, closed));
                }
            }
            logged::Action::TearDownAgent { caller } => self.end_member_work(*caller, at, roles),
            logged::Action::Finish | logged::Action::Abandon => self.end = Some(at),
            // Its work is its op's request's, which `take` reads.
            logged::Action::RunCommand { .. }
            | logged::Action::Transfer { .. }
            | logged::Action::StartAgents
            | logged::Action::Reply
            | logged::Action::Unknown => {}
        }
    }

    fn end_member_work(&mut self, caller: logged::CallerName, at: Duration, roles: &Roles) {
        self.agents
            .entry(caller)
            .or_default()
            .work_end
            .get_or_insert(at);
        if roles.agents.keys().all(|id| {
            self.agents
                .get(id)
                .is_some_and(|agent| agent.work_end.is_some())
        }) {
            self.work_end.get_or_insert(at);
        }
    }

    fn open_lease(
        &mut self,
        at: Duration,
        lease: u64,
        holder: logged::CallerName,
        grant: &str,
        roles: &Roles,
    ) {
        let role = match holder {
            logged::CallerName::Job(job) => self.call(job).and_then(|call| call.role.clone()),
            caller @ logged::CallerName::Agent(_) => {
                roles.agents.get(&caller).map(|role| role.name.clone())
            }
        }
        .unwrap_or_default();
        let granted = roles.grants.get(&(role, grant.to_owned())).cloned();
        let Grant { role, ready_for } = granted.unwrap_or_default();
        self.leases.push(LeaseSpan {
            lease,
            grant: grant.to_owned(),
            holder,
            sandbox: roles.sandbox(role.as_deref(), at),
            ready_for,
            closed: None,
            commands: 0,
            last: None,
            failed: None,
            busy: Duration::ZERO,
            working: None,
        });
    }

    fn lease_ran(&mut self, at: Duration, lease: u64, work: Work) {
        if let Some(lease) = self.lease(lease) {
            lease.commands += 1;
            lease.last = Some(work);
            lease.working = Some(at);
        }
    }
}
