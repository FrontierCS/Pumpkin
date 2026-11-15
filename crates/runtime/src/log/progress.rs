//! What a run's log says of the run so far: a [`Progress`], and the parts of the log that this
//! build couldn't read ([`Unread`]).
//!
//! Each line of the log moves a `Reading` on, and a [`Readout`] completes it with what the log
//! leaves out: each role's place, startup time and prices, from `ir.json` and `provider.json`
//! (`Roles`), and what the record's directories hold when the readout is made, such as a sandbox's
//! `sandbox.json` and a judge's output. Every `Duration` is a time since the run started, by the
//! run's clock.

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
use crate::sandbox::SandboxName;
use crate::{TokenPrices, record};

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
    /// The work phase's deadline, counted from the agent's start, as the IR gives it; `None` if the
    /// IR has no agent.
    pub deadline: Option<Duration>,
    /// When the latest line read was written.
    pub last: Duration,
    pub agent: AgentSpan,
    /// Each judge call, in the order the log started them.
    pub calls: Vec<CallSpan>,
    /// Each lease, in the order the log started them.
    pub leases: Vec<LeaseSpan>,
    /// The model requests of every role.
    pub model: ModelUse,
    /// When the work phase ended: when the agent's teardown began.
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
    /// yet begun, or of a build that predates it.
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

/// A sandbox of the run, which one of the agent, a judge call or a lease holds. Its times are those
/// the log gave it, and `None` while it hasn't.
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
    /// When the run was done with it: once it had collected what the agent's work phase left, or
    /// the agent failed; once a judge call or a lease ended; else once it was deleted, as one is
    /// when the run stops short of the core's end.
    pub gone: Option<Duration>,
    /// Whether its `sandbox.json` records the provider's delete as done; `None` if it has no
    /// `sandbox.json`, so was never created.
    pub deleted: Option<bool>,
}

/// The agent: its sandbox, and its command's end.
#[derive(Clone, Debug, Default)]
pub struct AgentSpan {
    pub sandbox: Sandbox,
    /// Whether its role has a model route.
    pub routed: bool,
    /// When, and how, its command ended.
    pub end: Option<(Duration, AgentEnd)>,
}

/// How the agent's command ended.
#[derive(Clone, Debug)]
pub enum AgentEnd {
    /// It exited, or a signal killed it.
    Exited(ExitStatus),
    /// The run couldn't start it or keep its sandbox, for this reason.
    Failed(String),
}

/// A judge call: the final judge's, which scores the selected submission, or a call that the agent
/// made.
#[derive(Clone, Debug)]
pub struct CallSpan {
    /// Its number in the run.
    pub job: u64,
    /// The agent's grant it answers, by name; `None` for the final judge.
    pub grant: Option<String>,
    /// The role of its judge.
    pub role: Option<String>,
    pub sandbox: Sandbox,
    /// How long the core gave it.
    pub timeout: Duration,
    /// Whether the core ordered it stopped.
    pub stopped: bool,
    /// Whether it ran past its timeout.
    pub timed_out: bool,
    /// How the call ended, collected or failed, at its sandbox's `gone`.
    pub end: Option<CallEnd>,
    /// Its judge's standard error in the record, if the judge wrote any when the readout was made.
    pub stderr: Option<PathBuf>,
    /// The directory in the record that holds its judge's evidence, if there is one.
    pub evidence: Option<PathBuf>,
    /// Its own model requests, which the run's (`Progress::model`) count too.
    pub model: ModelUse,
}

/// A lease: a runner that the agent or a judge call holds, to run commands and move trees on.
#[derive(Clone, Debug)]
pub struct LeaseSpan {
    /// Its number in the run.
    pub lease: u64,
    /// The grant that gave it, by name.
    pub grant: String,
    /// The judge call that holds it, by `job`; `None` if the agent does.
    pub holder: Option<u64>,
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
    /// The judge call that held it ended.
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

/// How a judge call ended.
#[derive(Clone, Debug)]
pub enum CallEnd {
    /// The judge returned a score, as JSON.
    Scored(String),
    /// The judge returned a result without one, which is this, as JSON.
    Unscored(String),
    /// The judge's output was no result its spec reads, for this reason.
    Invalid(String),
    /// The run couldn't start the call or keep its sandbox, for this reason.
    Failed(String),
}

/// The model requests of a role, or of the whole run: each has been answered, or is in flight.
#[derive(Clone, Debug, Default)]
pub struct ModelUse {
    /// How many were answered.
    pub requests: u64,
    /// How many answers ended each way: with the upstream's status, such as `200`, or `null` if it
    /// gave none; with `<status> cut`; or with `barred` or `refused`.
    pub answers: BTreeMap<String, u64>,
    /// How many responses stated a failure, by the code they gave, or `None` if they gave none.
    pub failed: BTreeMap<Option<String>, u64>,
    /// How many answers were not a whole response with a 2xx status.
    pub not_2xx: u64,
    /// When the good answer that ended last was answered: a whole response with a 2xx status, whose
    /// stream stated no failure.
    pub last_good: Option<Duration>,
    /// The requests begun and not yet answered, by number, with when each began.
    pub in_flight: BTreeMap<u64, Duration>,
    /// How long each good answer whose start the log records took, from going upstream to its end,
    /// in the order they were answered.
    pub took: Vec<Duration>,
    /// How long each good answer took to its first token of output, if it had one.
    pub first_tokens: Vec<Duration>,
    /// What the responses reported.
    pub tokens: Tokens,
    /// What the answers that name their model reported, by the route's name for it.
    pub by_model: BTreeMap<String, Tokens>,
    /// The names of request headers that the route left behind, at most
    /// `route::MAX_DROPPED_HEADERS`.
    pub dropped_headers: BTreeSet<String>,
    /// Whether more names were left behind than `dropped_headers` holds.
    pub more_dropped_headers: bool,
}

/// Tokens summed over the responses that reported them.
///
/// `reported` is how many did, and each `Option` count is `None` if none of those gave it.
/// `cached` and `cache_write` count input tokens read from and written to the provider's cache,
/// which `input` includes.
#[derive(Clone, Debug, Default)]
pub struct Tokens {
    /// How many responses reported their usage.
    pub reported: u64,
    pub input: u64,
    pub output: u64,
    pub cached: Option<u64>,
    pub cache_write: Option<u64>,
    pub reasoning: Option<u64>,
}

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
        token_prices: roles.token_prices.clone(),
    };
    // Each sandbox's `sandbox.json` is rewritten as the run deletes it, and a judge call's standard
    // error and evidence are collected as it ends, so they are read now.
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

fn deleted(dir: &Path) -> Result<Option<bool>, String> {
    let sandbox = json_file(&dir.join("sandbox.json"))?;
    Ok((!sandbox.is_null()).then(|| sandbox["delete"]["state"] == "deleted"))
}

/// What the IR and `provider.json` say of a run's roles: what the log's lines refer to by name.
#[derive(Default)]
pub(super) struct Roles {
    agent_role: String,
    routed: bool,
    grants: BTreeMap<(String, String), Grant>,
    score_role: Option<String>,
    places: BTreeMap<String, Place>,
    gpus: BTreeMap<String, Gpu>,
    deadline: Option<Duration>,
    startups: BTreeMap<Place, Duration>,
    prices: BTreeMap<Place, f64>,
    token_prices: BTreeMap<String, TokenPrices>,
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
        // `{"routes": {role: [{"models": {m: {"usd_per_mtok": t}}}]}}`.
        let entries = provided["routes"].as_object().into_iter().flatten();
        let entries = entries.flat_map(|(_, entries)| entries.as_array().into_iter().flatten());
        let models = entries.flat_map(|entry| entry["models"].as_object().into_iter().flatten());
        let token_prices = models.filter_map(|(model, served)| {
            let prices = TokenPrices::deserialize(&served["usd_per_mtok"]).ok()?;
            Some((model.clone(), prices))
        });
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
            token_prices: token_prices.collect(),
        }
    }

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

#[derive(Clone, Default)]
struct Grant {
    role: Option<String>,
    ready_for: Option<Duration>,
}

#[derive(Clone, Default)]
pub(super) struct Reading {
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
                    model: model.as_deref(),
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
    fn named(&mut self, name: SandboxName) -> Option<&mut Sandbox> {
        match name {
            SandboxName::Agent => Some(&mut self.agent.sandbox),
            SandboxName::Job(job) => self.call(job).map(|call| &mut call.sandbox),
            SandboxName::Lease(lease) => self.lease(lease).map(|lease| &mut lease.sandbox),
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

    fn lease_ran(&mut self, at: Duration, lease: u64, work: Work) {
        if let Some(lease) = self.lease(lease) {
            lease.commands += 1;
            lease.last = Some(work);
            lease.working = Some(at);
        }
    }
}

struct Answered<'e> {
    request: u64,
    model: Option<&'e str>,
    dropped_headers: &'e [String],
    first_token: Option<Duration>,
    upstream_status: Option<u16>,
    answer: &'e logged::Answer,
    failed: Option<&'e logged::Failed>,
    usage: Option<&'e logged::Usage>,
}

impl ModelUse {
    fn take(&mut self, at: Duration, answered: &Answered<'_>) {
        let Answered {
            request,
            model,
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
            self.tokens.add(usage);
            if let Some(model) = model {
                let tokens = self.by_model.entry((*model).to_owned()).or_default();
                tokens.add(usage);
            }
        }
    }
}

impl Tokens {
    fn add(&mut self, usage: &logged::Usage) {
        self.reported += 1;
        self.input += usage.input_tokens;
        self.output += usage.output_tokens;
        if let Some(cached) = usage.cached_tokens {
            *self.cached.get_or_insert(0) += cached;
        }
        if let Some(written) = usage.cache_write_tokens {
            *self.cache_write.get_or_insert(0) += written;
        }
        if let Some(reasoning) = usage.reasoning_tokens {
            *self.reasoning.get_or_insert(0) += reasoning;
        }
    }
}
