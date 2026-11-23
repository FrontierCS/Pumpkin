//! The driver's decision core: what a run does next, decided from what has happened.
//!
//! [`Core`] holds a run's state, does no I/O and reads no clock, so the same [`Event`]s give the
//! same [`Action`]s; [`Core`] says how a runtime drives it.
//!
//! The core runs a workflow's two phases ([`schema::ir::Workflow`]). Its [`Action`]s ask the
//! runtime to start the agents, answer ops, run calls, open, drive and close leases, collect
//! outputs and destroy sandboxes, and [`Event`]s report what the runtime observed.
//! [`Action::Finish`] carries the run's [`Outcome`], or [`Action::Abandon`] ends it without one.

// Without `std`, the core can name none of its files, network, processes, environment, threads
// or clock; `alloc` gives it what else it uses, such as `Vec` and `String`. It still reaches the
// I/O items that `schema` holds (`tree::Writer`, `frame` over `std::io`, `PathBuf` paths). No
// clock from `core::arch` is reachable either, as `_rdtsc` takes `unsafe`, which the workspace
// denies.
#![no_std]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::num::NonZeroU64;
use core::time::Duration;

use schema::ir::{AbsolutePath, ArgumentValue, Argv, Name, Sha256Digest};
use schema::sandbox::ExitStatus;
use schema::sandbox::op::{CommandEnd, OpReply, SessionOp, TransferEnd};
use schema::workflow::{AgentId, JobFunctionId, Placed, SessionId, TargetId, Workflow};

mod call;
mod jobs;
mod leases;
mod ops;
mod result;
mod work;

pub use jobs::Failure;
pub use result::CallResult;
use work::AgentWork;

/// The runtime's number for an op it took from a session, which the op's reply names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OpId(pub u64);

/// A call's job: a job function's, or the final call's. The core numbers jobs from 0, in the order
/// it starts them, and callers ask about their jobs by these numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobId(pub u64);

/// The core numbers leases from 0, in the order it opens them, whoever holds them, and callers
/// ask about their leases by these numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LeaseId(pub u64);

/// Who made an op, whose grants it may use, and who holds the leases it opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Caller {
    /// The agent whose sandbox issued the request.
    Agent(AgentId),
    /// A call, from its target's sandbox, while the call's body runs.
    Job(JobId),
}

/// A payload the runtime holds, named by its digest.
///
/// The core decides on its size and passes the reference on, never the bytes, so the bytes
/// need be stored only once the core accepts the call that carries them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PayloadRef {
    pub size: u64,
    pub digest: Sha256Digest,
}

/// What an op asks, as its caller sent it: the gateway's `Op`, with the payload named by its
/// digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// See [`OpCall::Ops`](schema::sandbox::op::OpCall::Ops).
    Ops,
    /// See [`OpCall::Call`](schema::sandbox::op::OpCall::Call): a call of any function but a now
    /// function.
    Call {
        function: Name,
        payload: PayloadRef,
        arguments: BTreeMap<Name, String>,
    },
    /// A call of the now function named `function`, as [`OpCall::Call`] makes it, with what its
    /// body's evaluation gave, or why it gave nothing.
    ///
    /// [`OpCall::Call`]: schema::sandbox::op::OpCall::Call
    Now {
        function: Name,
        payload: PayloadRef,
        evaluated: Result<Evaluated, Unevaluated>,
    },
    /// See [`OpCall::Status`](schema::sandbox::op::OpCall::Status).
    Status { job: u64 },
    /// See [`OpCall::Session`](schema::sandbox::op::OpCall::Session).
    Session { grant: Name, op: SessionOp },
}

/// What a now function's body gave: the effects it proposes, in order, and what it returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Evaluated {
    pub effects: Vec<Effect>,
    /// See [`CallResult::returned_by`].
    pub returned: CallResult,
}

/// Why a now function's call gave no evaluation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unevaluated {
    /// One of its caller's arguments didn't read, in the words its caller hears.
    Unread(String),
    /// Its body failed, for this reason, which the record keeps, and its caller hears a fixed
    /// sentence of.
    Failed(String),
}

/// An effect on the run's state that a now function's body proposes, which its call applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    /// The call's payload becomes the caller's submission, replacing the last.
    SetSubmission,
    /// Locks the caller's submission and ends its work once its grace has run out.
    EndWork,
}

/// What the runtime observed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The work commands have been released. Their common deadline counts from this report,
    /// capped at the longest agent startup plus the work duration.
    WorkStarted,
    AgentExited {
        agent: AgentId,
        status: ExitStatus,
    },
    /// The agent's sandbox or command could not start, or its session ended.
    AgentFailed {
        agent: AgentId,
        reason: String,
    },
    /// The agent's teardown has completed, including collection and destruction.
    AgentTornDown {
        agent: AgentId,
    },
    /// The work target lost its sandbox or could not stop a command. Refuse further calls.
    SharedFailed {
        target: TargetId,
        reason: String,
    },
    /// The worker finished and deleted any shared sandbox its provider identified.
    SharedTornDown {
        target: TargetId,
    },
    /// `caller`'s sandbox made an op. The core answers it with an [`Action::Reply`] naming `op`.
    OpArrived {
        op: OpId,
        caller: Caller,
        request: Request,
    },
    /// A call has started its body on its target.
    CallStarted {
        job: JobId,
    },
    /// A call reached its timeout, and the runtime stops it.
    CallTimedOut {
        job: JobId,
    },
    /// A call's body returned, as [`CallResult::returned_by`] takes its return, and what its target
    /// held has been collected. `model_requests_over_budget` is how many of the target's model
    /// requests the route barred for a spent model budget, if any.
    CallReturned {
        job: JobId,
        result: CallResult,
        model_requests_over_budget: Option<NonZeroU64>,
    },
    /// A call failed, or was stopped as [`Action::StopCall`] asked;
    /// `model_requests_over_budget` as for [`Event::CallReturned`].
    CallFailed {
        job: JobId,
        reason: String,
        model_requests_over_budget: Option<NonZeroU64>,
    },
    /// A call whose end has been reported holds no sandbox: its fresh target, if one was created,
    /// has been destroyed. It comes only after [`Event::CallReturned`] or [`Event::CallFailed`].
    CallTornDown {
        job: JobId,
    },
    /// A lease's runner has started, and runs the commands it is given from now.
    LeaseReady {
        lease: LeaseId,
    },
    /// A command of a lease's, its `command`th, has ended, and what it wrote has been collected:
    /// the ends of its standard output and error, of at most
    /// [`OUTPUT_TAIL_BYTES`](schema::sandbox::op::OUTPUT_TAIL_BYTES) each, as JSON writes them.
    CommandEnded {
        lease: LeaseId,
        command: u64,
        end: CommandEnd,
        stdout: String,
        stderr: String,
    },
    /// A transfer of a lease's, its `transfer`th command or transfer, has ended, as `end` says,
    /// and what the host held of it is gone.
    TransferEnded {
        lease: LeaseId,
        transfer: u64,
        end: TransferEnd,
    },
    /// A lease that [`Action::CloseLease`] closed has ended: its runner has been destroyed.
    LeaseEnded {
        lease: LeaseId,
    },
    /// A lease's runner failed to start, or failed as it ran, and has been destroyed.
    LeaseFailed {
        lease: LeaseId,
        reason: String,
    },
    /// Time passed.
    Tick,
}

/// What the core asks the runtime to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Starts the work-lifetime targets and agents. Releasing the agents' commands produces
    /// [`Event::WorkStarted`]. Each agent reports its own exit or failure.
    StartAgents,
    /// Answers op `op`.
    Reply { op: OpId, reply: OpReply },
    /// Runs the final body on a fresh sandbox of its target with the agents' submissions.
    /// Its timeout and the runtime's reports are as for [`Action::StartCall`].
    StartFinalCall {
        job: JobId,
        /// One entry for each agent in declaration order, including those that submitted nothing.
        selections: Vec<(AgentId, Option<PayloadRef>)>,
        timeout: Duration,
    },
    /// Runs a call of job function `function`: a fresh sandbox of the function's target, and the
    /// function's body on it with `payload` and its caller's `arguments`, whose every step on the
    /// target is to be done within `timeout` of the body's start: the call is cut short at the
    /// body's first step past it, or as it passes while a command runs, and when the core stops
    /// it. The runtime reports [`Event::CallStarted`] when the body starts,
    /// [`Event::CallTimedOut`] if its timeout passes, [`Event::CallReturned`], or
    /// [`Event::CallFailed`] at any point, and last [`Event::CallTornDown`], once the target is
    /// gone.
    StartCall {
        job: JobId,
        function: JobFunctionId,
        payload: PayloadRef,
        arguments: BTreeMap<Name, ArgumentValue>,
        timeout: Duration,
    },
    /// Stops a call wherever it is, and destroys its fresh target; a shared target outlives it. Its
    /// reports follow as usual.
    StopCall { job: JobId },
    /// Opens lease `lease` of grant `grant` for `holder`: starts a fresh sandbox of the grant's
    /// runner. The instance number is local to this holder and grant. The runtime
    /// reports [`Event::LeaseReady`] once it has started, then each command's end as
    /// [`Event::CommandEnded`], and last [`Event::LeaseEnded`] once [`Action::CloseLease`] closes
    /// it, or [`Event::LeaseFailed`] at any point, once the lease's sandbox is gone.
    ///
    /// Its runner is to have started by `start_by`, a time since the run started: its holder's
    /// work end, past which nothing could use it. Else the start is given up, and the lease
    /// fails.
    StartLease {
        lease: LeaseId,
        holder: Caller,
        grant: SessionId,
        instance: u32,
        start_by: Duration,
    },
    /// Runs `argv` as lease `lease`'s `command`th command, killing it once `timeout` has passed
    /// since this action, its start included, if given; without one, it runs until the lease is
    /// closed.
    RunCommand {
        lease: LeaseId,
        command: u64,
        argv: Argv,
        timeout: Option<Duration>,
    },
    /// Copies a tree between the sandbox of the lease's holder, `holder`, and lease `lease`'s
    /// runner, as `direction` says, as the lease's `transfer`th command or transfer, failing once
    /// its files hold more than `max_bytes`. The runtime reports [`Event::TransferEnded`] once it
    /// has ended, which [`Action::CloseLease`] makes come early, cut short.
    Transfer {
        lease: LeaseId,
        holder: Caller,
        transfer: u64,
        direction: Direction,
        max_bytes: NonZeroU64,
    },
    /// Closes lease `lease` wherever it is, and destroys its sandbox with whatever runs in it.
    CloseLease { lease: LeaseId },
    /// Freezes this agent's sandbox, collects its outputs and destroys it. The runtime
    /// reports [`Event::AgentTornDown`] once teardown completes, including a failed start.
    TearDownAgent { agent: AgentId },
    /// Stop and delete the shared sandbox after all work calls have drained.
    TearDownShared { target: TargetId },
    /// Ends the run with its outcome. After it, the core only refuses the ops that still
    /// arrive.
    Finish { outcome: Outcome },
    /// Ends the run without an outcome, since it was still going at its hard end ([`lasts`]),
    /// and stops whatever of it still runs. As after [`Action::Finish`], the core then only
    /// refuses the ops that still arrive.
    Abandon,
}

/// Which way a transfer copies a tree, and between which paths: the tree's root becomes `to`,
/// which must not exist, unless a put is to `replace` it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Direction {
    /// From the holder's `tree` to the runner's `to`, whatever is there removed first if
    /// `replace`.
    Put {
        tree: AbsolutePath,
        to: AbsolutePath,
        replace: bool,
    },
    /// From the runner's `from` to the holder's `to`.
    Get {
        from: AbsolutePath,
        to: AbsolutePath,
    },
}

/// Why an agent stopped working.
///
/// Of the deadline and the end of the grace after a finalize, the first to pass names the end,
/// however late the runtime observed it, and the deadline if they pass together; the agent's
/// exit or failure names it only if observed before then ([`Core::handle`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkEnd {
    /// The common work deadline passed, counted from the release of the commands
    /// ([`Event::WorkStarted`]).
    Deadline,
    /// A call ended the agent's work, and the agent was still running when the grace after it ran
    /// out ([`finalize_grace`](schema::workflow::Agent::finalize_grace)). An agent that exits
    /// within it ends its work with [`AgentExited`](Self::AgentExited), and its submission
    /// is still the locked one.
    Finalized,
    AgentExited(ExitStatus),
    AgentFailed(String),
}

/// A submission the agent made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Submission {
    pub payload: PayloadRef,
    /// Whether the agent marked it as its final one.
    pub finalized: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentOutcome {
    pub agent: AgentId,
    pub work_end: WorkEnd,
    pub submission: Option<Submission>,
    /// The runtime records a model route failure here after its in-flight requests have ended.
    pub route_failure: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// One entry for every agent, in declaration order.
    pub agents: Vec<AgentOutcome>,
    pub result: Result<CallResult, Failure>,
}

/// How long a run of `placed` may last from its start: its hard end, which [`Core::start`]
/// sets, after the deadline.
///
/// It includes the longest agent startup, the work duration, the longest finalize grace and
/// one more agent startup for collecting, plus the final target's startup and timeout.
pub fn lasts(placed: &Placed) -> Duration {
    let workflow = placed.workflow();
    let final_call = workflow.final_call();
    let startup = agent_startup(placed);
    let grace = workflow
        .agents()
        .map(|agent| agent.finalize_grace)
        .max()
        .unwrap_or_default();
    startup
        .saturating_add(workflow.work_deadline())
        .saturating_add(grace)
        .saturating_add(startup)
        .saturating_add(final_call.timeout)
        .saturating_add(placed.startup(final_call.target.role()))
}

fn agent_startup(placed: &Placed) -> Duration {
    placed
        .workflow()
        .agents()
        .map(|agent| placed.startup(agent.id.role()))
        .chain(
            placed
                .workflow()
                .work_targets()
                .map(|id| placed.startup(id.role())),
        )
        .max()
        .unwrap_or_default()
}

/// A run's decision core.
///
/// A runtime drives it: [`Core::start`] begins the run, at time 0, and returns the first
/// actions; the runtime then feeds each [`Event`] it observes to [`Core::handle`], with the time
/// since the run started, and performs the actions it returns, in order; every op gets exactly
/// one reply ([`Action::Reply`]). Time passes only as events say, so a runtime with nothing else
/// to report sends [`Event::Tick`] when [`Core::tick_due`] says. An event that doesn't apply to
/// the run's state changes nothing, and one older than an earlier event doesn't move the clock
/// back.
#[derive(Debug)]
pub struct Core<'w> {
    workflow: &'w Workflow,
    /// The latest time an event was observed, since the run started.
    now: Duration,
    /// The run's hard end, since it started.
    hard_end: Duration,
    phase: Phase,
    agents: BTreeMap<AgentId, AgentWork>,
    shared: BTreeMap<TargetId, Availability>,
    jobs: jobs::Jobs,
    leases: leases::Leases,
}

#[derive(Debug, PartialEq, Eq)]
enum Availability {
    Available,
    Failed,
    Stopping,
}

#[derive(Debug)]
enum Phase {
    Work { deadline: Duration },
    Final { job: JobId },
    Finished,
    Abandoned,
}

impl<'w> Core<'w> {
    /// Starts the run at time 0 and returns its first actions. The workflow's deadline counts
    /// from the release of the commands ([`Event::WorkStarted`]). Its hard end is [`lasts`].
    pub fn start(placed: &'w Placed) -> (Self, Vec<Action>) {
        let workflow = placed.workflow();
        let core = Self {
            workflow,
            now: Duration::ZERO,
            hard_end: lasts(placed),
            phase: Phase::Work {
                deadline: agent_startup(placed).saturating_add(workflow.work_deadline()),
            },
            agents: workflow
                .agents()
                .map(|agent| (agent.id, AgentWork::default()))
                .collect(),
            shared: workflow
                .work_targets()
                .map(|id| (id, Availability::Available))
                .collect(),
            jobs: jobs::Jobs::default(),
            leases: leases::Leases::default(),
        };
        (core, vec![Action::StartAgents])
    }

    /// Takes in `event`, observed `at` after the run started, and returns the actions it calls
    /// for, in the order to perform them.
    ///
    /// The time acts before the event: an `at` that reaches the hard end, an agent's work end,
    /// or a lease's end abandons the run, ends that agent's work or closes the lease first.
    /// A report that a job's command has ended, or that the job has, closes the leases it holds.
    #[expect(clippy::too_many_lines, reason = "one arm per event")]
    pub fn handle(&mut self, at: Duration, event: Event) -> Vec<Action> {
        self.now = self.now.max(at);
        let mut actions = Vec::new();
        if self.now >= self.hard_end {
            self.abandon(&mut actions);
        }
        self.end_due_work(&mut actions);
        self.expire_leases(&mut actions);
        match event {
            Event::WorkStarted => {
                if let Phase::Work { deadline } = &mut self.phase {
                    *deadline =
                        (*deadline).min(self.now.saturating_add(self.workflow.work_deadline()));
                }
            }
            Event::SharedFailed { target, .. } => {
                if let Some(state @ Availability::Available) = self.shared.get_mut(&target) {
                    *state = Availability::Failed;
                    if let Some(job) = self.jobs.active_on(self.workflow, target) {
                        actions.push(Action::StopCall { job });
                    }
                }
            }
            Event::SharedTornDown { target } => {
                if self.shared.get(&target) == Some(&Availability::Stopping) {
                    self.shared.remove(&target);
                }
            }
            Event::AgentTornDown { agent } => {
                if let Some(end) = self
                    .agents
                    .get_mut(&agent)
                    .and_then(|work| work.ended.as_mut())
                {
                    end.torn_down = true;
                }
            }
            // A tick only moves the clock, on which the checks above have acted.
            Event::Tick => {}
            Event::AgentExited { agent, status } => {
                self.end_work(agent, WorkEnd::AgentExited(status), &mut actions);
            }
            Event::AgentFailed { agent, reason } => {
                self.end_work(agent, WorkEnd::AgentFailed(reason), &mut actions);
            }
            Event::OpArrived {
                op,
                caller,
                request,
            } => self.op(op, caller, request, &mut actions),
            Event::CallStarted { job } => {
                self.on_job(job, |state| state.started(at), &mut actions);
            }
            Event::CallTimedOut { job } => {
                let ended = |state: &mut jobs::State| state.ended(at, jobs::End::TimedOut);
                self.on_job(job, ended, &mut actions);
            }
            Event::CallFailed {
                job,
                reason,
                model_requests_over_budget,
            } => {
                let outcome = Err(Failure::Error(reason));
                let report = |state: &mut jobs::State| {
                    state.report(at, outcome, model_requests_over_budget);
                };
                self.on_job(job, report, &mut actions);
            }
            Event::CallReturned {
                job,
                result,
                model_requests_over_budget,
            } => {
                let report = |state: &mut jobs::State| {
                    state.report(at, Ok(result), model_requests_over_budget);
                };
                self.on_job(job, report, &mut actions);
            }
            Event::CallTornDown { job } => self.on_job(job, jobs::State::torn_down, &mut actions),
            Event::LeaseReady { lease } => self.on_lease(lease, |lease| lease.state.ready(at)),
            Event::CommandEnded {
                lease,
                command,
                end,
                stdout,
                stderr,
            } => self.on_lease(lease, |lease| {
                lease.command_ended(command, (end, stdout, stderr), at);
            }),
            Event::TransferEnded {
                lease,
                transfer,
                end,
            } => self.on_lease(lease, |lease| lease.transfer_ended(transfer, end, at)),
            Event::LeaseEnded { lease } => {
                self.on_lease(lease, |lease| lease.report(at, leases::End::Closed));
            }
            Event::LeaseFailed { lease, reason } => {
                self.on_lease(lease, |lease| {
                    lease.report(at, leases::End::Failed(reason));
                });
            }
        }
        self.advance(&mut actions);
        actions
    }

    /// When the core next acts without another event, as a time since the run started: at the
    /// end of a lease ([`Core::handle`]) or an agent's work ([`Core::agent_due`]), whichever comes
    /// first. The runtime sends [`Event::Tick`] once that time has come.
    pub fn tick_due(&self) -> Duration {
        let phase = self
            .agents
            .keys()
            .map(|id| self.agent_due(*id))
            .min()
            .unwrap_or(self.hard_end);
        self.leases_due().map_or(phase, |lease| phase.min(lease))
    }

    /// Whether this agent locked its submission with `run.end_work()`.
    pub fn submission_locked(&self, agent: AgentId) -> bool {
        self.agents
            .get(&agent)
            .is_some_and(|work| work.grace_end.is_some())
    }

    /// The common work deadline, or the hard end after the work phase.
    pub fn phase_due(&self) -> Duration {
        match self.phase {
            Phase::Work { deadline } => deadline,
            Phase::Final { .. } | Phase::Finished | Phase::Abandoned => self.hard_end,
        }
    }

    /// The earlier of this agent's grace and the common work deadline, or the hard end
    /// once its work has ended.
    pub fn agent_due(&self, agent: AgentId) -> Duration {
        self.work_due(agent).map_or(self.hard_end, |(end, _)| end)
    }

    /// Whether `caller`'s model requests are allowed at `at`, a time since the run started: the
    /// agent's until that agent's work ends, the grace after a finalize included
    /// ([`finalize_grace_seconds`](schema::ir::AgentRuntime::finalize_grace_seconds)); a call's
    /// from its body's start until it ends, is stopped, or its timeout passes, in whichever phase.
    /// Once they aren't, the runtime refuses each that it takes, and ends those in flight.
    pub fn model_requests_allowed(&self, caller: Caller, at: Duration) -> bool {
        match caller {
            // A harness asks its model after each command, the finalize's too, and fails its turn
            // if refused; the locked submission keeps any answer off the result.
            Caller::Agent(agent) => self.work_due(agent).is_some_and(|(end, _)| at < end),
            // A call's route is open while it may make ops, which its timeout bounds.
            Caller::Job(_) => self.working(caller).is_ok_and(|working| at < working.end()),
        }
    }

    fn abandon(&mut self, actions: &mut Vec<Action>) {
        match self.phase {
            Phase::Work { .. } | Phase::Final { .. } => {}
            Phase::Finished | Phase::Abandoned => return,
        }
        self.phase = Phase::Abandoned;
        // Abandon stops the workers. A late service failure has no call left to stop.
        self.shared.clear();
        // Abandon stops every runner. A later expiry or job report has no lease left to close.
        drop(self.leases.close_where(self.now, |_| true));
        actions.push(Action::Abandon);
    }

    fn on_job(
        &mut self,
        job: JobId,
        apply: impl FnOnce(&mut jobs::State),
        actions: &mut Vec<Action>,
    ) {
        let Some(state) = self.jobs.state_mut(job) else {
            return;
        };
        apply(state);
        match state {
            jobs::State::Queued | jobs::State::Running { .. } => {}
            jobs::State::Ending { .. } | jobs::State::Ended { .. } => {
                let closed =
                    (self.leases).close_where(self.now, |lease| lease.holder == Caller::Job(job));
                actions.extend(closed.into_iter().map(|lease| Action::CloseLease { lease }));
            }
        }
    }
}
