use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{fmt, fs};

use provider::UnixTime;
use schema::ir::Gpu;
use schema::sandbox::ExitStatus;
use schema::sandbox::op::{CommandEnd, TransferEnd};
use serde::Deserialize;
use serde_json::Value;
use stdx::Context;

use super::Direction;
use super::logged::{self, Collected, Line};
use crate::record;
use crate::sandbox::SandboxName;

/// Where a role's sandboxes are: a provider of the run's, and a class it offers, as
/// `provider.json` names them.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Place {
    pub provider: String,
    pub class: String,
}

/// What a run's log, as far as this build can read it ([`Readout`]), says of the run so far,
/// with the bounds of its workflow and of its roles' classes.
///
/// Each time is since the run started, as the log gives times.
#[derive(Clone, Debug)]
pub struct Progress {
    /// When the run started, by the driving machine's clock.
    pub started: UnixTime,
    /// When the core abandons the run if it is still going.
    pub hard_end: Duration,
    /// The work phase's deadline, counted from the agent's start.
    pub deadline: Option<Duration>,
    /// When the log wrote its last record.
    pub last: Duration,
    pub agent: AgentSpan,
    /// Each judge call, in the order the core started them.
    pub calls: Vec<CallSpan>,
    /// Each lease, in the order the core opened them.
    pub leases: Vec<LeaseSpan>,
    /// The model requests of every role's route.
    pub model: ModelUse,
    /// When the work phase ended, as the core tore the agent down, if it has.
    pub work_end: Option<Duration>,
    /// When the run ended, by the core's finish or abandon, if it has.
    pub end: Option<Duration>,
    /// The price of each class of the run's providers that declared one, in dollars an hour, by
    /// its place, as `provider.json` records it.
    pub prices: BTreeMap<Place, f64>,
}

/// What [`Record::progress`](crate::Record::progress), or a [`LogFold`], reads of a run's log.
///
/// The run's progress, once the log records its start with its hard end, and each part of the
/// log that this build can't read, in the log's order, which the progress leaves out.
///
/// [`LogFold`]: super::LogFold
#[derive(Debug)]
pub struct Readout {
    pub progress: Option<Progress>,
    pub unread: Vec<Unread>,
}

/// A part of a run log's line that this build can't read: it isn't JSON, or it is of a kind
/// this build knows but lacks a field of that kind's, or holds one in another form.
#[derive(Clone, Debug)]
pub struct Unread {
    /// Its line's number in the log, counting from 1.
    pub line: u64,
    pub part: Part,
    /// Why it doesn't read.
    pub reason: String,
}

/// Which part of a line doesn't read: the line, so none of it reads; its event; or its action
/// at an index, from 0, among those it holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    Line,
    Event,
    Action(usize),
}

/// Where it is, and why it doesn't read: `line 4: <reason>`, `line 4, its event: <reason>` or,
/// for the action at index 1, `line 4, its action 2: <reason>`.
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

/// A sandbox of the run, whatever started it, from the core's start of it, at `asked`.
#[derive(Clone, Debug, Default)]
pub struct Sandbox {
    pub place: Option<Place>,
    /// How long its class may take to start it.
    pub startup: Option<Duration>,
    /// The GPUs its role needs, if it needs any.
    pub needs: Option<Gpu>,
    /// The GPUs its `sandboxd` saw as it started, each by its model as `nvidia-smi` names it, as
    /// the log's `sandbox_gpus` says, if it does.
    pub gpus: Option<Vec<String>>,
    pub asked: Duration,
    /// When its command started, or its runner was ready.
    pub started: Option<Duration>,
    /// When the run was done with it: once it had collected what the agent's work phase left, or
    /// the agent failed; once a judge call or a lease ended; else once it was deleted, as one
    /// is when the run stops short of the core's end.
    pub gone: Option<Duration>,
    /// Whether the run deleted it, as its `sandbox.json` in the record says, if it is there.
    pub deleted: Option<bool>,
}

/// The agent's sandbox, from the core's start of it, at time 0.
#[derive(Clone, Debug, Default)]
pub struct AgentSpan {
    pub sandbox: Sandbox,
    /// Whether its role has a model route.
    pub routed: bool,
    /// When it exited or failed, and how.
    pub end: Option<(Duration, AgentEnd)>,
}

/// How the agent ended: its command's exit, with its status, or its failure.
#[derive(Clone, Debug)]
pub enum AgentEnd {
    Exited(ExitStatus),
    Failed(String),
}

/// A judge call, job `job`.
#[derive(Clone, Debug)]
pub struct CallSpan {
    pub job: u64,
    /// The grant of the agent's that it was made under, or `None` for the workflow's score.
    pub grant: Option<String>,
    /// The judge role it called, if the record names it.
    pub role: Option<String>,
    pub sandbox: Sandbox,
    /// How long its command may run.
    pub timeout: Duration,
    /// Whether the core stopped it, or its command was killed at its timeout.
    pub stopped: bool,
    pub timed_out: bool,
    /// How the call ended, collected or failed, at its sandbox's `gone`.
    pub end: Option<CallEnd>,
    /// Its command's standard error in the record, if it holds any bytes.
    pub stderr: Option<PathBuf>,
    /// Its evidence directory in the record, if it collected one.
    pub evidence: Option<PathBuf>,
    /// Its own model requests, which the run's ([`Progress::model`]) count too.
    pub model: ModelUse,
}

/// A lease, `lease`, opened under the grant `grant`.
#[derive(Clone, Debug)]
pub struct LeaseSpan {
    pub lease: u64,
    pub grant: String,
    /// The job whose judge call holds it, or `None` for the agent.
    pub holder: Option<u64>,
    /// Its runner's sandbox.
    pub sandbox: Sandbox,
    /// How long its runner may be ready, as its grant says, or its holder asked at its open.
    pub ready_for: Option<Duration>,
    /// When the core closed it, and why.
    pub closed: Option<(Duration, Closed)>,
    /// How many commands and transfers it ran, and the last.
    pub commands: u64,
    pub last: Option<Work>,
    pub failed: Option<String>,
    /// How long it was busy, until the command or transfer that runs, if one does, began.
    busy: Duration,
    /// When the command or transfer that runs began, until it ended.
    working: Option<Duration>,
}

/// Why the core closed a lease: its holder asked it to, the agent's work ended, the judge call
/// that held it ended, or the lease's time, or its grant's, ran out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Closed {
    Asked,
    WorkEnded,
    CallEnded,
    Expired,
}

/// What a lease ran, and how it ended, once it has: a command, its `argv`, or a transfer of a
/// tree, from a path to a path.
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

    /// How much of the time it was charged for until `now` ([`LeaseSpan::charged`]) a command or
    /// transfer of its ran: from when the core accepted it until it ended, or the core closed
    /// the lease, or its sandbox was gone, whichever came first.
    pub fn busy(&self, now: Duration) -> Duration {
        let working = (self.working).map_or(Duration::ZERO, |since| {
            self.until(now).saturating_sub(since)
        });
        self.busy + working
    }

    /// When its runner's charge ends, as of `now`: when the core closed the lease, or else its
    /// sandbox was gone, or else `now`.
    fn until(&self, now: Duration) -> Duration {
        let closed = self.closed.map(|(at, _)| at);
        closed.or(self.sandbox.gone).unwrap_or(now)
    }

    /// Notes that its last command, if it ran one, ended at `at`, as `end` says.
    fn command_ended(&mut self, at: Duration, end: CommandEnd) {
        if let Some(Work::Command { end: last, .. }) = &mut self.last {
            *last = Some(end);
            self.rest(at);
        }
    }

    /// Notes that its last transfer, if it ran one, ended at `at`, as `end` says.
    fn transfer_ended(&mut self, at: Duration, end: TransferEnd) {
        if let Some(Work::Transfer { end: last, .. }) = &mut self.last {
            *last = Some(end);
            self.rest(at);
        }
    }

    /// Ends its busy time at `at`, if it is busy, or at its charge's end, if that came first.
    fn rest(&mut self, at: Duration) {
        if let Some(since) = self.working.take() {
            self.busy += self.until(at).saturating_sub(since);
        }
    }
}

/// How a judge call ended: with a result, which has a score, the JSON number, or not, when it
/// is what the judge returned, as JSON; with a result that was invalid; or with a failure.
#[derive(Clone, Debug)]
pub enum CallEnd {
    Scored(String),
    Unscored(String),
    Invalid(String),
    Failed(String),
}

/// The run's model requests, of every role with a route, as its routes answered them.
#[derive(Clone, Debug, Default)]
pub struct ModelUse {
    pub requests: u64,
    /// How many were answered each way: the upstream's status for a response forwarded whole,
    /// `<status> cut` for one cut short, and `refused` or `barred` for the route's refusal.
    pub answers: BTreeMap<String, u64>,
    /// How many of them stated that they failed ([`route::Failure`]), by the code each gave, or
    /// `None` for one that gave none.
    pub failed: BTreeMap<Option<String>, u64>,
    /// How many were not forwarded whole with a 2xx status: cut short, refused or barred by the
    /// route, or forwarded with another status.
    pub not_2xx: u64,
    /// When the good one that ended last was answered, a good one being forwarded whole, with a
    /// 2xx status, and its stream stating no failure.
    pub last_good: Option<Duration>,
    /// Each that went upstream and has no answer yet, by its number, with when it went.
    pub in_flight: BTreeMap<u64, Duration>,
    /// How long each good one whose start the log records took, from going upstream to its
    /// answer's end, in the order they were answered.
    pub took: Vec<Duration>,
    /// How long each of those whose stream's output began took to begin it.
    pub first_tokens: Vec<Duration>,
    /// The tokens of those whose responses reported their usage, summed.
    pub tokens: Tokens,
    /// The names of the request headers that the route left behind, of any of them: the first
    /// [`route::MAX_DROPPED_HEADERS`] by name.
    pub dropped_headers: BTreeSet<String>,
    /// Whether the route left behind more names than `dropped_headers` holds.
    pub more_dropped_headers: bool,
}

/// Tokens summed over the responses that reported them: `reported` is how many did, and each
/// `Option` count is `None` if none of those gave it.
#[derive(Clone, Debug, Default)]
pub struct Tokens {
    pub reported: u64,
    pub input: u64,
    pub output: u64,
    pub cached: Option<u64>,
    /// The input tokens written to the provider's cache.
    pub cache_write: Option<u64>,
    pub reasoning: Option<u64>,
}

/// What `reading`, of the log of the record in `dir`, with `roles` and `unread`, says of the
/// run ([`LogFold::readout`]).
///
/// [`LogFold::readout`]: super::LogFold::readout
pub(super) fn readout(
    dir: &Path,
    roles: Option<&Roles>,
    reading: Reading,
    unread: Vec<Unread>,
) -> Result<Readout, String> {
    let (Some(roles), Some((started, hard_end))) = (roles, reading.started) else {
        return Ok(Readout {
            progress: None,
            unread,
        });
    };
    let Reading {
        started: _,
        last,
        mut agent,
        calls,
        leases,
        model,
        work_end,
        end,
    } = reading;
    agent.sandbox = Sandbox {
        started: agent.sandbox.started,
        gone: agent.sandbox.gone,
        deleted: deleted(&dir.join("agent"))?,
        gpus: agent.sandbox.gpus.take(),
        ..roles.sandbox(Some(&roles.agent_role), Duration::ZERO)
    };
    agent.routed = roles.routed;
    let mut progress = Progress {
        started,
        hard_end,
        deadline: roles.deadline,
        last,
        agent,
        calls,
        leases,
        model,
        work_end,
        end,
        prices: roles.prices.clone(),
    };
    // Each sandbox's `sandbox.json` is rewritten as the run deletes it, and a judge call's
    // standard error and evidence are collected as it ends, so they are read now.
    for lease in &mut progress.leases {
        lease.sandbox.deleted = deleted(&dir.join(format!("leases/{}", lease.lease)))?;
    }
    for call in &mut progress.calls {
        let job = dir.join(format!("jobs/{}", call.job));
        call.sandbox.deleted = deleted(&job)?;
        let stderr = job.join(schema::sandbox::command_stderr().as_str());
        let held = fs::metadata(&stderr).is_ok_and(|stderr| stderr.len() > 0);
        call.stderr = held.then_some(stderr);
        let evidence = job.join(schema::sandbox::evidence_dir().as_str());
        call.evidence = evidence.is_dir().then_some(evidence);
    }
    Ok(Readout {
        progress: Some(progress),
        unread,
    })
}

/// The JSON in `path`, or `null` if there is no such file.
pub(super) fn json_file(path: &Path) -> Result<Value, String> {
    match record::read_optional(path)? {
        Some(bytes) => serde_json::from_slice(&bytes).context(path.display()),
        None => Ok(Value::Null),
    }
}

/// Each role's place, by its name, as `provided`, a record's `provider.json`, names it.
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

/// Whether the sandbox whose record is in `dir` was deleted, as its `sandbox.json` says, if it
/// is there.
fn deleted(dir: &Path) -> Result<Option<bool>, String> {
    let sandbox = json_file(&dir.join("sandbox.json"))?;
    Ok((!sandbox.is_null()).then(|| sandbox["delete"]["state"] == "deleted"))
}

/// What the record's `ir.json` and `provider.json` say of where the run's roles are, what GPUs
/// they need, and their bounds.
#[derive(Default)]
pub(super) struct Roles {
    /// The agent's role's name.
    agent_role: String,
    /// Whether the agent's role has a model route.
    routed: bool,
    /// Each judge or lease grant, by the name of the role that holds it and its own name.
    grants: BTreeMap<(String, String), Grant>,
    /// The score's judge role's name.
    score_role: Option<String>,
    /// Each role's place, by its name.
    places: BTreeMap<String, Place>,
    /// The GPUs of each role that needs any, by its name.
    gpus: BTreeMap<String, Gpu>,
    deadline: Option<Duration>,
    /// Each class's startup, by its place.
    startups: BTreeMap<Place, Duration>,
    /// Each class's price, in dollars an hour, by its place, if its provider declared one.
    prices: BTreeMap<Place, f64>,
}

impl Roles {
    pub(super) fn of(ir: &Value, provided: &Value) -> Self {
        let roles = ir["roles"].as_array().into_iter().flatten();
        let gpus = roles.clone().filter_map(|role| {
            let gpu = Gpu::deserialize(&role["needs"]["gpu"]).ok()?;
            Some((role["name"].as_str()?.to_owned(), gpu))
        });
        let agent = (roles.clone()).find(|role| !role["runtime"]["agent"].is_null());
        let agent = agent.unwrap_or(&Value::Null);
        let agent_role = agent["name"].as_str().unwrap_or_default();
        let held = roles.map(|role| (role["name"].as_str().unwrap_or_default(), &role["grants"]));
        let grants = held.flat_map(|(holder, grants)| {
            let grants = grants.as_array().into_iter().flatten();
            grants.filter_map(move |grant| {
                // `{"judge": g}`, `{"lease": g}` or `{"submit": g}`.
                let (_, grant) = grant.as_object()?.iter().next()?;
                let role = grant["role"].as_str().map(str::to_owned);
                let granted = Grant {
                    role,
                    ready_for: grant["instance_seconds"].as_u64().map(Duration::from_secs),
                };
                Some((
                    (holder.to_owned(), grant["name"].as_str()?.to_owned()),
                    granted,
                ))
            })
        });
        let score = ir["score"]["role"].as_str();
        let (mut startups, mut prices) = (BTreeMap::new(), BTreeMap::new());
        let providers = provided["providers"].as_object().into_iter().flatten();
        for (provider, ready) in providers {
            let classes = ready["classes"].as_object().into_iter().flatten();
            for (class, offered) in classes {
                let place = Place {
                    provider: provider.clone(),
                    class: class.clone(),
                };
                if let Some(price) = offered["usd_per_hour"].as_f64() {
                    prices.insert(place.clone(), price);
                }
                if let Some(seconds) = offered["startup_seconds"].as_u64() {
                    startups.insert(place, Duration::from_secs(seconds));
                }
            }
        }
        Self {
            agent_role: agent_role.to_owned(),
            routed: !agent["egress"]["model"].is_null(),
            grants: grants.collect(),
            score_role: score.map(str::to_owned),
            places: places(provided),
            gpus: gpus.collect(),
            deadline: (agent["runtime"]["agent"]["deadline_seconds"].as_u64())
                .map(Duration::from_secs),
            startups,
            prices,
        }
    }

    /// A sandbox of `role`, which the core started at `asked`, in its role's place, with its
    /// class's startup and the GPUs its role needs.
    fn sandbox(&self, role: Option<&str>, asked: Duration) -> Sandbox {
        let place = role.and_then(|role| self.places.get(role)).cloned();
        Sandbox {
            startup: (place.as_ref()).and_then(|place| self.startups.get(place).copied()),
            place,
            needs: role.and_then(|role| self.gpus.get(role)).cloned(),
            asked,
            ..Sandbox::default()
        }
    }
}

/// A judge or lease grant: the role it names, and how long a lease's runner may be ready.
#[derive(Clone, Default)]
struct Grant {
    role: Option<String>,
    ready_for: Option<Duration>,
}

/// A log as far as it has been read: what its lines say, each as [`Progress`]'s field of its
/// name holds it.
#[derive(Clone, Default)]
pub(super) struct Reading {
    /// When the run started, and its hard end, once its start is read.
    started: Option<(UnixTime, Duration)>,
    last: Duration,
    agent: AgentSpan,
    calls: Vec<CallSpan>,
    leases: Vec<LeaseSpan>,
    model: ModelUse,
    work_end: Option<Duration>,
    end: Option<Duration>,
}

impl Reading {
    /// Takes in `line`, a line of the log of a run whose roles are `roles`.
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
            .any(|action| matches!(action, logged::Action::TearDownAgent))
        {
            Closed::WorkEnded
        } else {
            Closed::Expired
        };
        // A judge call's end closes the leases it holds.
        let ended = event.and_then(logged::Event::judge_job);
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
        if let Some(note) = &line.note
            && note.starts_with("collected from the agent's sandbox")
        {
            self.agent.sandbox.gone = Some(at);
        }
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

    /// Takes in `event`, observed at `at`.
    #[expect(clippy::too_many_lines, reason = "one arm per kind of event")]
    fn event(&mut self, at: Duration, event: &logged::Event) {
        match event {
            logged::Event::RunStarted {
                unix_ns,
                hard_end_ns,
            } => self.started = hard_end_ns.map(|hard_end| (*unix_ns, hard_end)),
            logged::Event::AgentStarted => self.agent.sandbox.started = Some(at),
            logged::Event::AgentExited { status } => {
                self.agent.end = Some((at, AgentEnd::Exited(*status)));
            }
            logged::Event::AgentFailed { reason } => {
                self.agent.end = Some((at, AgentEnd::Failed(reason.clone())));
                self.agent.sandbox.gone.get_or_insert(at);
            }
            logged::Event::JudgeStarted { job } => {
                if let Some(call) = self.call(*job) {
                    call.sandbox.started = Some(at);
                }
            }
            logged::Event::JudgeTimedOut { job } => {
                if let Some(call) = self.call(*job) {
                    call.timed_out = true;
                }
            }
            logged::Event::JudgeCollected { job, collected } => {
                let end = match collected {
                    Collected::Result(result) => match &result.score {
                        Some(score) => CallEnd::Scored(score.to_string()),
                        None => CallEnd::Unscored(result.returned.to_string()),
                    },
                    Collected::Invalid(invalid) => CallEnd::Invalid(invalid.clone()),
                };
                if let Some(call) = self.call(*job) {
                    call.end(at, end);
                }
            }
            logged::Event::JudgeFailed { job, reason } => {
                if let Some(call) = self.call(*job) {
                    call.end(at, CallEnd::Failed(reason.clone()));
                }
            }
            logged::Event::LeaseReady { lease } => {
                if let Some(lease) = self.lease(*lease) {
                    lease.sandbox.started = Some(at);
                }
            }
            logged::Event::CommandEnded { lease, end } => {
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
                if let logged::Caller::Job(job) = caller
                    && let Some(call) = self.call(*job)
                {
                    call.model.in_flight.insert(*request, at);
                }
            }
            logged::Event::ModelAnswered {
                caller,
                request,
                first_token_ns,
                upstream_status,
                answer,
                failed,
                usage,
                kept,
            } => {
                let answered = Answered {
                    request: *request,
                    dropped_headers: &kept.request.dropped_headers,
                    first_token: *first_token_ns,
                    upstream_status: *upstream_status,
                    answer,
                    failed: failed.as_ref(),
                    usage: usage.as_ref(),
                };
                self.model.take(at, &answered);
                // A judge call's own, which the run's count too.
                if let Some(logged::Caller::Job(job)) = caller
                    && let Some(call) = self.call(*job)
                {
                    call.model.take(at, &answered);
                }
            }
            logged::Event::SandboxGpus { sandbox, gpus } => {
                if let Some(sandbox) = self.named(*sandbox) {
                    sandbox.gpus = Some(gpus.clone());
                }
            }
            logged::Event::SandboxDeleted { sandbox } => {
                if let Some(sandbox) = sandbox.and_then(|sandbox| self.named(sandbox)) {
                    sandbox.gone.get_or_insert(at);
                }
            }
            logged::Event::JudgeExited { .. }
            | logged::Event::OpArrived { .. }
            | logged::Event::OpRefused
            | logged::Event::Tick
            | logged::Event::Unknown => {}
        }
    }
}

impl CallSpan {
    fn end(&mut self, at: Duration, end: CallEnd) {
        self.end = Some(end);
        self.sandbox.gone = Some(at);
    }
}

impl Reading {
    /// The sandbox that `name` names within the run, if the core started it.
    fn named(&mut self, name: SandboxName) -> Option<&mut Sandbox> {
        match name {
            SandboxName::Agent => Some(&mut self.agent.sandbox),
            SandboxName::Job(job) => self.call(job).map(|call| &mut call.sandbox),
            SandboxName::Lease(lease) => self.lease(lease).map(|lease| &mut lease.sandbox),
        }
    }

    /// Judge call `job`, if the core started it.
    fn call(&mut self, job: u64) -> Option<&mut CallSpan> {
        self.calls.iter_mut().find(|call| call.job == job)
    }

    /// Lease `lease`, if the core opened it.
    fn lease(&mut self, lease: u64) -> Option<&mut LeaseSpan> {
        self.leases.iter_mut().find(|span| span.lease == lease)
    }
}

impl Reading {
    /// Takes in `action`, which the core decided on at `at`, in a run whose roles are `roles`;
    /// a lease it closes is `closed`, but one that the judge call of job `ended`, which ended,
    /// holds.
    fn action(
        &mut self,
        at: Duration,
        action: &logged::Action,
        (closed, ended): (Closed, Option<u64>),
        roles: &Roles,
    ) {
        match action {
            logged::Action::StartJudge {
                job,
                grant,
                timeout_ns,
            } => {
                let role = match grant {
                    Some(grant) => {
                        let key = (roles.agent_role.clone(), grant.clone());
                        roles.grants.get(&key).cloned().unwrap_or_default().role
                    }
                    None => roles.score_role.clone(),
                };
                self.calls.push(CallSpan {
                    job: *job,
                    grant: grant.clone(),
                    sandbox: roles.sandbox(role.as_deref(), at),
                    role,
                    timeout: *timeout_ns,
                    stopped: false,
                    timed_out: false,
                    end: None,
                    stderr: None,
                    evidence: None,
                    model: ModelUse::default(),
                });
            }
            logged::Action::StopJudge { job } => {
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
                        Some(holder) if ended == Some(holder) => Closed::CallEnded,
                        _ => closed,
                    };
                    lease.closed.get_or_insert((at, closed));
                }
            }
            logged::Action::TearDownAgent => {
                self.work_end.get_or_insert(at);
            }
            logged::Action::Finish | logged::Action::Abandon => self.end = Some(at),
            // Its work is its op's request's, which `take` reads.
            logged::Action::RunCommand { .. }
            | logged::Action::Transfer { .. }
            | logged::Action::StartAgent
            | logged::Action::Reply
            | logged::Action::Unknown => {}
        }
    }

    /// Takes in the open of lease `lease`, under `grant`, held by `holder`, which the core
    /// decided on at `at`, in a run whose roles are `roles`.
    fn open_lease(
        &mut self,
        at: Duration,
        lease: u64,
        holder: Option<logged::Caller>,
        grant: &str,
        roles: &Roles,
    ) {
        let holder = match holder {
            Some(logged::Caller::Job(job)) => Some(job),
            Some(logged::Caller::Agent) | None => None,
        };
        let call = holder.and_then(|holder| self.calls.iter().find(|call| call.job == holder));
        let role = match call {
            Some(call) => call.role.clone().unwrap_or_default(),
            None => roles.agent_role.clone(),
        };
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

    /// Notes that lease `lease` ran `work`, which is its last, from `at`, if the core opened it.
    fn lease_ran(&mut self, at: Duration, lease: u64, work: Work) {
        if let Some(lease) = self.lease(lease) {
            lease.commands += 1;
            lease.last = Some(work);
            lease.working = Some(at);
        }
    }
}

/// A model request's answer, as its `model_answered` says: its number, when its stream's output
/// began, if it did, its upstream's status, if a response came, how the route answered it, the
/// code of the failure it stated, if it stated one, and the tokens it reported, if it did.
struct Answered<'e> {
    request: u64,
    /// The names of its request's headers that the route left behind.
    dropped_headers: &'e [String],
    first_token: Option<Duration>,
    upstream_status: Option<u16>,
    answer: &'e logged::Answer,
    failed: Option<&'e logged::Failed>,
    usage: Option<&'e logged::Usage>,
}

impl ModelUse {
    /// Takes in `answered`, a model request answered at `at`.
    fn take(&mut self, at: Duration, answered: &Answered<'_>) {
        let Answered {
            request,
            dropped_headers,
            first_token,
            upstream_status,
            answer,
            failed,
            usage,
        } = answered;
        self.requests += 1;
        (self.dropped_headers).extend(dropped_headers.iter().cloned());
        while self.dropped_headers.len() > route::MAX_DROPPED_HEADERS {
            self.dropped_headers.pop_last();
            self.more_dropped_headers = true;
        }
        let started = self.in_flight.remove(request);
        if let Some(logged::Failed { code }) = failed {
            *self.failed.entry(code.clone()).or_default() += 1;
        }
        let whole_2xx = matches!(answer, logged::Answer::Forwarded)
            && upstream_status.is_some_and(|status| (200..300).contains(&status));
        if !whole_2xx {
            self.not_2xx += 1;
        } else if failed.is_none() {
            // Threads write their answers as they end, which needn't be in the order they ended.
            self.last_good = self.last_good.max(Some(at));
            if let Some(started) = started {
                self.took.push(at.saturating_sub(started));
                if let Some(first_token) = first_token {
                    self.first_tokens.push(first_token.saturating_sub(started));
                }
            }
        }
        // As the log gives the status: `null` for none.
        let status = upstream_status.map_or_else(|| "null".to_owned(), |status| status.to_string());
        let answer = match answer {
            logged::Answer::Forwarded => status,
            logged::Answer::Cut => format!("{status} cut"),
            logged::Answer::Barred => "barred".to_owned(),
            logged::Answer::Refused => "refused".to_owned(),
        };
        *self.answers.entry(answer).or_default() += 1;
        if let Some(usage) = usage {
            let tokens = &mut self.tokens;
            tokens.reported += 1;
            tokens.input += usage.input_tokens;
            tokens.output += usage.output_tokens;
            if let Some(cached) = usage.cached_tokens {
                *tokens.cached.get_or_insert(0) += cached;
            }
            if let Some(written) = usage.cache_write_tokens {
                *tokens.cache_write.get_or_insert(0) += written;
            }
            if let Some(reasoning) = usage.reasoning_tokens {
                *tokens.reasoning.get_or_insert(0) += reasoning;
            }
        }
    }
}
