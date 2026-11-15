//! The driver's decision core: what a run does next, decided from what has happened.
//!
//! [`Core`] holds a run's state, does no I/O and reads no clock, so the same [`Event`]s give the
//! same [`Action`]s; [`Core`] says how a runtime drives it.
//!
//! The core runs a workflow's two phases ([`schema::ir::Workflow`]). Its [`Action`]s ask the
//! runtime to start the agent, answer ops, run judge calls, open, drive and close leases, collect
//! outputs and destroy sandboxes, and [`Event`]s report what the runtime observed.
//! [`Action::Finish`] carries the run's [`Outcome`], or [`Action::Abandon`] ends it without one.

// Without `std`, the core can name none of its files, network, processes, environment, threads
// or clock; `alloc` gives it what else it uses, such as `Vec` and `String`. It still reaches the
// I/O items that `schema` holds (`tree::Writer`, `frame` over `std::io`, `PathBuf` paths).
#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::num::NonZeroU64;
use core::time::Duration;

use schema::ir::{AbsolutePath, Argv, Name, Sha256Digest};
use schema::sandbox::ExitStatus;
use schema::sandbox::op::{CommandEnd, OpReply, TransferEnd};
use schema::workflow::{JudgeCall, LeaseGrantId, Placed, Workflow};
use serde_json::Number;

mod jobs;
mod leases;
mod ops;
mod result;

pub use jobs::Failure;
pub use result::JudgeResult;

/// The runtime's number for an op it took from a session, which the op's reply names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OpId(pub u64);

/// A judge call's job. The core numbers jobs from 0, in the order it starts them, and callers
/// ask about their jobs by these numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobId(pub u64);

/// The core numbers leases from 0, in the order it opens them, whoever holds them, and callers
/// ask about their leases by these numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LeaseId(pub u64);

/// Who made an op, whose grants it may use, and who holds the leases it opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Caller {
    /// The agent, from its sandbox.
    Agent,
    /// A judge's call, from the sandbox of its job, while the judge's command runs.
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
    /// See [`OpCall::Judge`](schema::sandbox::op::OpCall::Judge).
    Judge { grant: Name, payload: PayloadRef },
    /// See [`OpCall::Submit`](schema::sandbox::op::OpCall::Submit).
    Submit {
        grant: Name,
        finalize: bool,
        payload: PayloadRef,
    },
    /// See [`OpCall::Status`](schema::sandbox::op::OpCall::Status).
    Status { job: u64 },
    /// See [`OpCall::LeaseOpen`](schema::sandbox::op::OpCall::LeaseOpen).
    LeaseOpen {
        grant: Name,
        instance_seconds: Option<NonZeroU64>,
    },
    /// See [`OpCall::LeaseRun`](schema::sandbox::op::OpCall::LeaseRun).
    LeaseRun {
        lease: u64,
        argv: Argv,
        timeout_seconds: Option<NonZeroU64>,
    },
    /// See [`OpCall::LeasePut`](schema::sandbox::op::OpCall::LeasePut).
    LeasePut {
        lease: u64,
        tree: AbsolutePath,
        to: AbsolutePath,
        replace: bool,
    },
    /// See [`OpCall::LeaseGet`](schema::sandbox::op::OpCall::LeaseGet).
    LeaseGet {
        lease: u64,
        from: AbsolutePath,
        to: AbsolutePath,
    },
    /// See [`OpCall::LeaseStatus`](schema::sandbox::op::OpCall::LeaseStatus).
    LeaseStatus { lease: u64 },
    /// See [`OpCall::LeaseClose`](schema::sandbox::op::OpCall::LeaseClose).
    LeaseClose { lease: u64 },
}

/// What the runtime observed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The agent's command has started in its sandbox.
    ///
    /// The work phase's deadline counts from the first such report while the agent works, so the
    /// time its sandbox takes to start doesn't count against it, but from the agent's startup
    /// ([`Placed::startup`]) at the latest: until a report comes, and after one that comes later,
    /// the deadline is the agent's deadline after the startup.
    AgentStarted,
    /// The agent's command has exited.
    AgentExited { status: ExitStatus },
    /// The agent's sandbox or its command could not start, or its session ended.
    AgentFailed { reason: String },
    /// `caller`'s sandbox made an op. The core answers it with an [`Action::Reply`] naming `op`.
    OpArrived {
        op: OpId,
        caller: Caller,
        request: Request,
    },
    /// A job's command has started.
    JudgeStarted { job: JobId },
    /// A job's command has exited by itself.
    JudgeExited { job: JobId, status: ExitStatus },
    /// A job's command reached its timeout, and the runtime stops it.
    JudgeTimedOut { job: JobId },
    /// A job's outputs have been collected and its sandbox destroyed. `result` is its
    /// `result.json` parsed per its judge's [`ResultSpec`](schema::ir::ResultSpec), or
    /// why it could not be.
    JudgeCollected {
        job: JobId,
        result: Result<JudgeResult, String>,
    },
    /// A job failed, or was stopped as [`Action::StopJudge`] asked, and its sandbox has been
    /// destroyed.
    JudgeFailed { job: JobId, reason: String },
    /// A lease's runner has started, and runs the commands it is given from now.
    LeaseReady { lease: LeaseId },
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
    LeaseEnded { lease: LeaseId },
    /// A lease's runner failed to start, or failed as it ran, and has been destroyed.
    LeaseFailed { lease: LeaseId, reason: String },
    /// Time passed.
    Tick,
}

/// What the core asks the runtime to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Starts the agent's sandbox and its command. The runtime reports [`Event::AgentStarted`]
    /// once both have started, then the command's end as [`Event::AgentExited`] or
    /// [`Event::AgentFailed`]; or [`Event::AgentFailed`] alone, if either didn't start.
    StartAgent,
    /// Answers op `op`.
    Reply { op: OpId, reply: OpReply },
    /// Runs a judge call: a fresh sandbox of the call's judge, with `payload` in the call's
    /// slot ([`Workflow::call_slot`]), and the judge's command, stopped once it has run for
    /// `timeout`. The runtime reports [`Event::JudgeStarted`] when the command starts, then
    /// [`Event::JudgeExited`] or [`Event::JudgeTimedOut`], and last
    /// [`Event::JudgeCollected`], or [`Event::JudgeFailed`] at any point, once the job's
    /// sandbox is gone.
    StartJudge {
        job: JobId,
        call: JudgeCall,
        payload: PayloadRef,
        timeout: Duration,
    },
    /// Stops a job wherever it is, and destroys its sandbox. Its last report follows as usual.
    StopJudge { job: JobId },
    /// Opens lease `lease` of grant `grant` for `holder`: starts a fresh sandbox of the grant's
    /// runner, as instance `instance` of its role (`provider::Create::instance`). The runtime
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
        grant: LeaseGrantId,
        instance: u32,
        start_by: Duration,
    },
    /// Runs `argv` as lease `lease`'s `command`th command, killing it once it has run for
    /// `timeout`, from its start, if given; without one, it runs until the lease is closed.
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
    /// Freezes the agent's sandbox, collects its outputs and destroys it, before the runtime
    /// performs the next action: the core takes the agent as gone once it asks.
    TearDownAgent,
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

/// Why the work phase ended.
///
/// Of the deadline and the end of the grace after a finalize, the first to pass names the end,
/// however late the runtime observed it, and the deadline if they pass together; the agent's
/// exit or failure names it only if observed before then ([`Core::handle`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkEnd {
    /// The work phase's deadline passed, which counts from the agent's start
    /// ([`Event::AgentStarted`]).
    Deadline,
    /// The agent finalized a submission, and was still running when the grace after it ran out
    /// ([`finalize_grace`](schema::workflow::Agent::finalize_grace)). An agent that exits within
    /// it ends the work phase with [`AgentExited`](Self::AgentExited), and its submission is
    /// still the finalized one.
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
pub struct Outcome {
    pub work_end: WorkEnd,
    pub verdict: Verdict,
}

impl Outcome {
    /// The score of the selected submission, if the final judge scored it.
    pub fn score(&self) -> Option<&Number> {
        match &self.verdict {
            Verdict::Judged { result, .. } => result.score(),
            Verdict::NoSubmission | Verdict::NoResult { .. } => None,
        }
    }
}

/// What the final judge made of the selected submission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The agent made no submission, so nothing was judged.
    NoSubmission,
    Judged {
        submission: Submission,
        result: JudgeResult,
    },
    NoResult {
        submission: Submission,
        failure: Failure,
    },
}

/// How long a run of `placed` may last from its start: its hard end, which [`Core::start`]
/// sets, after the deadline.
///
/// It is the sum of six shares: the agent's startup, its deadline, its finalize grace, the
/// agent's startup again, the score's timeout, and the final judge's startup. A role's
/// startup ([`Placed::startup`]) is how long its class may take to start one of its sandboxes,
/// from when the runtime asks for one to when its session is ready.
pub fn lasts(placed: &Placed) -> Duration {
    let workflow = placed.workflow();
    let score = workflow.score();
    let agent = placed.startup(workflow.agent().role);
    let final_judge = placed.startup(score.slot.judge().role());
    // Each start is attempted once (a failed start isn't retried), which is why the hard
    // end counts one startup per start. The core ends the work phase by the first two shares
    // (`latest_deadline`), and the agent's teardown begins by then. The final judge's startup
    // and the score's timeout are the final judge's, the run's last sandbox: its start and its
    // command. The grace, which the deadline cuts short, and the agent's startup again are slack
    // for ending the work phase and collecting what it left, which nothing else bounds: a margin
    // chosen, not derived.
    latest_deadline(placed)
        .saturating_add(workflow.agent().finalize_grace)
        .saturating_add(agent)
        .saturating_add(score.timeout)
        .saturating_add(final_judge)
}

/// The latest that the work phase's deadline of a run of `placed` may be, from the run's start:
/// the agent's startup, then its deadline, which counts from the agent's start, at the latest
/// from its startup ([`Event::AgentStarted`]).
fn latest_deadline(placed: &Placed) -> Duration {
    let agent = placed.workflow().agent();
    placed.startup(agent.role).saturating_add(agent.deadline)
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
    jobs: jobs::Jobs,
    leases: leases::Leases,
}

#[derive(Debug)]
enum Phase {
    /// The agent works, from when the core asks for it. Its start's report sets the deadline,
    /// and changes no other decision.
    Work {
        latest: Option<PayloadRef>,
        /// The work phase's deadline, since the run started, as [`Event::AgentStarted`] says.
        deadline: Duration,
    },
    /// The agent finalized a submission of `payload`, which is locked as the selection; unless
    /// the agent ends it first, the work phase ends at `end`, as `work_end` ([`WorkEnd`]).
    Finalized {
        payload: PayloadRef,
        end: Duration,
        work_end: WorkEnd,
    },
    /// The work phase has ended, and the agent has been torn down. The jobs it left, stopped or
    /// collecting, and the leases it and they held, closed, are ending.
    Teardown {
        work_end: WorkEnd,
        submission: Option<Submission>,
    },
    /// The final judge judges the selected submission in job `job`, and the run ends once the
    /// job, and every lease it held, has ended.
    Final {
        work_end: WorkEnd,
        submission: Submission,
        job: JobId,
    },
    Finished {
        work_end: WorkEnd,
    },
    Abandoned,
}

impl<'w> Core<'w> {
    /// Starts a run of `placed`: its work phase begins at time 0, though its deadline counts
    /// from the agent's start ([`Event::AgentStarted`]), and its hard end is [`lasts`] after.
    /// Returns the core and its first actions.
    pub fn start(placed: &'w Placed) -> (Self, Vec<Action>) {
        let workflow = placed.workflow();
        let core = Self {
            workflow,
            now: Duration::ZERO,
            hard_end: lasts(placed),
            phase: Phase::Work {
                latest: None,
                deadline: latest_deadline(placed),
            },
            jobs: jobs::Jobs::default(),
            leases: leases::Leases::default(),
        };
        (core, vec![Action::StartAgent])
    }

    /// Takes in `event`, observed `at` after the run started, and returns the actions it calls
    /// for, in the order to perform them.
    ///
    /// The time acts before the event: an `at` that reaches the hard end, the work phase's end,
    /// or a lease's, abandons the run, ends the work phase or closes the lease first, and the
    /// event then finds it so. A report that a job's command has ended, or that the job has,
    /// closes the leases it holds.
    pub fn handle(&mut self, at: Duration, event: Event) -> Vec<Action> {
        self.now = self.now.max(at);
        let mut actions = Vec::new();
        if self.now >= self.hard_end {
            self.abandon(&mut actions);
        }
        self.end_due_work(&mut actions);
        self.expire_leases(&mut actions);
        match event {
            Event::AgentStarted => self.agent_started(),
            // A tick only moves the clock, on which the checks above have acted.
            Event::Tick => {}
            Event::AgentExited { status } => {
                self.end_work(WorkEnd::AgentExited(status), &mut actions);
            }
            Event::AgentFailed { reason } => {
                self.end_work(WorkEnd::AgentFailed(reason), &mut actions);
            }
            Event::OpArrived {
                op,
                caller,
                request,
            } => self.op(op, caller, request, &mut actions),
            Event::JudgeStarted { job } => {
                self.on_job(job, |state| state.started(at), &mut actions);
            }
            // The run log keeps the status; whether a call scored is its result's to say.
            Event::JudgeExited { job, status: _ } => {
                let ended = |state: &mut jobs::State| state.ended(at, jobs::End::Exited);
                self.on_job(job, ended, &mut actions);
            }
            Event::JudgeTimedOut { job } => {
                let ended = |state: &mut jobs::State| state.ended(at, jobs::End::TimedOut);
                self.on_job(job, ended, &mut actions);
            }
            Event::JudgeCollected { job, result } => {
                let outcome = result.map_err(Failure::InvalidResult);
                self.on_job(job, |state| state.report(at, outcome), &mut actions);
            }
            Event::JudgeFailed { job, reason } => {
                let outcome = Err(Failure::Error(reason));
                self.on_job(job, |state| state.report(at, outcome), &mut actions);
            }
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
    /// end of a lease ([`Core::handle`]) or of the phase ([`Core::phase_due`]), whichever comes
    /// first. The runtime sends [`Event::Tick`] once that time has come.
    pub fn tick_due(&self) -> Duration {
        // A run that has ended has no lease ready: the work phase's end closes every lease, and
        // the run ends only once the final job's have ended.
        let phase = self.phase_due();
        self.leases_due().map_or(phase, |lease| phase.min(lease))
    }

    /// When the core next ends a phase, or the run, without another event, as a time since the
    /// run started: while the agent works, when the work phase ends; then at the hard end, also
    /// once the run has ended.
    pub fn phase_due(&self) -> Duration {
        // The work phase ends by the deadline, which comes before the hard end.
        self.work_end_due().map_or(self.hard_end, |(end, _)| end)
    }

    /// Whether `caller`'s model requests are allowed at `at`, a time since the run started: the
    /// agent's until the work phase ends, the grace after a finalize included
    /// ([`finalize_grace_seconds`](schema::ir::AgentRuntime::finalize_grace_seconds)); a call's
    /// from its command's start until the command exits, is stopped, or its timeout passes, in
    /// whichever phase. Once they aren't, the runtime refuses each that it takes, and ends those
    /// in flight.
    pub fn model_requests_allowed(&self, caller: Caller, at: Duration) -> bool {
        match caller {
            // A harness asks its model after each command, the finalize's too, and fails its turn
            // if refused; the locked selection keeps any answer off the score.
            Caller::Agent => self.work_end_due().is_some_and(|(end, _)| at < end),
            // A call's route is open while it may make ops, which its timeout bounds.
            Caller::Job(_) => self.working(caller).is_ok_and(|working| at < working.end()),
        }
    }

    fn agent_started(&mut self) {
        let counted = self.now.saturating_add(self.workflow.agent().deadline);
        match &mut self.phase {
            // The deadline after the startup is the latest it may be, and a later report gives
            // no earlier one, so the least is the first report's, and a report past the startup
            // changes nothing. The time has not passed the deadline, which would have ended the
            // work phase, so the least is still to come.
            Phase::Work { deadline, .. } => *deadline = (*deadline).min(counted),
            // A finalize has settled the work phase's end, or the work phase has ended.
            Phase::Finalized { .. }
            | Phase::Teardown { .. }
            | Phase::Final { .. }
            | Phase::Finished { .. }
            | Phase::Abandoned => {}
        }
    }

    fn abandon(&mut self, actions: &mut Vec<Action>) {
        match self.phase {
            Phase::Work { .. }
            | Phase::Finalized { .. }
            | Phase::Teardown { .. }
            | Phase::Final { .. } => {}
            Phase::Finished { .. } | Phase::Abandoned => return,
        }
        self.phase = Phase::Abandoned;
        actions.push(Action::Abandon);
    }

    /// When the work phase ends unless the agent ends it first, and as which [`WorkEnd`], while
    /// it lasts.
    fn work_end_due(&self) -> Option<(Duration, WorkEnd)> {
        match &self.phase {
            Phase::Work { deadline, .. } => Some((*deadline, WorkEnd::Deadline)),
            Phase::Finalized { end, work_end, .. } => Some((*end, work_end.clone())),
            Phase::Teardown { .. }
            | Phase::Final { .. }
            | Phase::Finished { .. }
            | Phase::Abandoned => None,
        }
    }

    fn end_due_work(&mut self, actions: &mut Vec<Action>) {
        if let Some((end, work_end)) = self.work_end_due()
            && self.now >= end
        {
            self.end_work(work_end, actions);
        }
    }

    fn end_work(&mut self, work_end: WorkEnd, actions: &mut Vec<Action>) {
        let submission = match &mut self.phase {
            Phase::Work { latest, .. } => latest.take().map(|payload| Submission {
                payload,
                finalized: false,
            }),
            Phase::Finalized { payload, .. } => Some(Submission {
                payload: payload.clone(),
                finalized: true,
            }),
            Phase::Teardown { .. }
            | Phase::Final { .. }
            | Phase::Finished { .. }
            | Phase::Abandoned => return,
        };
        self.phase = Phase::Teardown {
            work_end,
            submission,
        };
        let stopped = self.jobs.stop_unfinished(self.now);
        actions.extend(stopped.into_iter().map(|job| Action::StopJudge { job }));
        let closed = self.leases.close_where(self.now, |_| true);
        actions.extend(closed.into_iter().map(|lease| Action::CloseLease { lease }));
        actions.push(Action::TearDownAgent);
    }

    /// Moves the run on once what it waits for is done: the final phase once nothing the agent
    /// started runs on, and the run's end once the final job, and every lease, has ended.
    fn advance(&mut self, actions: &mut Vec<Action>) {
        match &self.phase {
            Phase::Teardown {
                work_end,
                submission,
            } if self.jobs.all_ended() && self.leases.all_ended() => {
                let (work_end, submission) = (work_end.clone(), submission.clone());
                self.enter_final(work_end, submission, actions);
            }
            Phase::Final {
                work_end,
                submission,
                job,
            } => {
                let Some(jobs::Job {
                    state: jobs::State::Ended { outcome, .. },
                    ..
                }) = self.jobs.get(*job)
                else {
                    return;
                };
                if !self.leases.all_ended() {
                    return;
                }
                let submission = submission.clone();
                let verdict = match outcome.clone() {
                    Ok(result) => Verdict::Judged { submission, result },
                    Err(failure) => Verdict::NoResult {
                        submission,
                        failure,
                    },
                };
                let work_end = work_end.clone();
                self.finish(Outcome { work_end, verdict }, actions);
            }
            Phase::Work { .. }
            | Phase::Finalized { .. }
            | Phase::Teardown { .. }
            | Phase::Finished { .. }
            | Phase::Abandoned => {}
        }
    }

    fn enter_final(
        &mut self,
        work_end: WorkEnd,
        submission: Option<Submission>,
        actions: &mut Vec<Action>,
    ) {
        let Some(submission) = submission else {
            let verdict = Verdict::NoSubmission;
            return self.finish(Outcome { work_end, verdict }, actions);
        };
        // The final phase comes after the deadline, so the time left before it doesn't count.
        let job = (self.jobs).add(JudgeCall::Score, self.workflow.score().timeout);
        actions.push(Action::StartJudge {
            job,
            call: JudgeCall::Score,
            payload: submission.payload.clone(),
            timeout: self.workflow.score().timeout,
        });
        self.phase = Phase::Final {
            work_end,
            submission,
            job,
        };
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

    fn finish(&mut self, outcome: Outcome, actions: &mut Vec<Action>) {
        self.phase = Phase::Finished {
            work_end: outcome.work_end.clone(),
        };
        actions.push(Action::Finish { outcome });
    }
}
