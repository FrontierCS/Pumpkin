//! The driver's decision core: what a run does next, decided from what has happened.
//!
//! [`Core`] holds a run's state, does no I/O and reads no clock, so the same [`Event`]s give the
//! same [`Action`]s; [`Core`] says how a runtime drives it.
//!
//! The core implements [`SoloThenJudge`]. Its [`Action`]s ask the runtime to start the agent,
//! answer ops, run judge calls, collect outputs and destroy sandboxes; the corresponding
//! [`Event`]s confirm their completion. [`WorkEnd`] names why the work phase ended, and
//! [`Action::Finish`] carries the run's [`Outcome`].
//!
//! The rules by which the core authorizes ops and fills in their replies are in its `ops`
//! module, and how it meters judge calls in its `jobs` module.

use std::time::Duration;

use pumpkin_schema::ir::{Name, Sha256Digest};
use pumpkin_schema::sandbox::ExitStatus;
use pumpkin_schema::sandbox::op::OpReply;
use pumpkin_schema::workflow::{JudgeCall, JudgeGrantId, Main, SoloThenJudge, Workflow};
use serde_json::Number;

mod jobs;
mod ops;
mod result;

pub use jobs::Failure;
pub use result::JudgeResult;

/// The runtime's number for an op it took from a session, which the op's reply names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OpId(pub u64);

/// A judge call's job. The core numbers jobs from 1, in the order it starts them, and callers
/// ask about their jobs by these numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobId(pub u64);

/// A payload the runtime holds, named by its digest.
///
/// The core decides on its size and passes the reference on, never the bytes; the runtime
/// stores the bytes only once the core accepts the call that carries them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PayloadRef {
    pub size: u64,
    pub digest: Sha256Digest,
}

/// What an op asks, as its caller sent it: the gateway's `Op`, with the payload named by its
/// digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// Asks for the caller's limits: the time left to work, its resources and the grants it
    /// holds.
    Ops,
    /// Calls the judge grant named `grant` with `payload`.
    Judge { grant: Name, payload: PayloadRef },
    /// Submits `payload` to the submit grant named `grant`. `finalize` marks it as the final
    /// submission.
    Submit {
        grant: Name,
        finalize: bool,
        payload: PayloadRef,
    },
    /// Asks how job `job` is doing.
    Status { job: u64 },
}

/// What the runtime observed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The agent's command has started in its sandbox.
    AgentStarted,
    /// The agent's command has exited.
    AgentExited { status: ExitStatus },
    /// The agent's sandbox failed: it could not start, or its session ended.
    AgentFailed { reason: String },
    /// The agent's sandbox is gone, as [`Action::TearDownAgent`] asked.
    AgentDestroyed,
    /// The agent's sandbox made an op. The core answers it with an [`Action::Reply`] naming
    /// `op`.
    OpArrived { op: OpId, request: Request },
    /// A job's command has started.
    JudgeStarted { job: JobId },
    /// A job's command has exited by itself.
    JudgeExited { job: JobId, status: ExitStatus },
    /// A job's command reached its timeout, and the runtime stopped it.
    JudgeTimedOut { job: JobId },
    /// A job's outputs have been collected and its sandbox destroyed. `result` is its
    /// `result.json` parsed per its judge's [`ResultSpec`](pumpkin_schema::ir::ResultSpec), or
    /// why it could not be.
    JudgeCollected {
        job: JobId,
        result: Result<JudgeResult, String>,
    },
    /// A job failed, or was stopped as [`Action::StopJudge`] asked, and its sandbox has been
    /// destroyed.
    JudgeFailed { job: JobId, reason: String },
    /// Time passed.
    Tick,
}

/// What the core asks the runtime to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Starts the agent's sandbox and its command, then reports [`Event::AgentStarted`].
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
    /// Freezes the agent's sandbox, collects its outputs, its trajectory among them, and
    /// destroys it, then reports [`Event::AgentDestroyed`].
    TearDownAgent,
    /// Ends the run with its outcome. The core takes no further action.
    Finish { outcome: Outcome },
}

/// Why the work phase ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkEnd {
    /// The pattern's deadline passed.
    Deadline,
    /// The agent finalized a submission, and was still running when the grace after it ran out
    /// ([`SoloThenJudge::finalize_grace_seconds`]). An agent that exits within it ends the work
    /// phase with [`AgentExited`](Self::AgentExited), and its submission is still the finalized
    /// one.
    Finalized,
    /// The agent's command exited.
    AgentExited(ExitStatus),
    /// The agent's sandbox failed, for this reason.
    AgentFailed(String),
}

/// A submission the agent made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Submission {
    pub payload: PayloadRef,
    /// Whether the agent marked it as its final one.
    pub finalized: bool,
}

/// How a run ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub work_end: WorkEnd,
    pub verdict: Verdict,
}

impl Outcome {
    /// The score of the selected submission, if the final judge passed it.
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
    /// The final judge's result.
    Judged {
        submission: Submission,
        result: JudgeResult,
    },
    /// The final judge gave no result.
    NoResult {
        submission: Submission,
        failure: Failure,
    },
}

/// A run's decision core.
///
/// A runtime drives it: [`Core::start`] begins the run, at time 0, and returns the first
/// actions; the runtime then feeds each [`Event`] it observes to [`Core::handle`], with the
/// milliseconds since the run started, and performs the actions it returns, in order. Replies
/// to ops are actions too ([`Action::Reply`]), and every op gets exactly one.
///
/// Time passes only as events say, so a runtime with nothing else to report sends
/// [`Event::Tick`] when [`Core::tick_due_ms`] says. An event that doesn't apply to the run's
/// state, such as a second report on a job that has ended, changes nothing, and one observed
/// before an earlier one, as a report from a job's thread can be, doesn't move the core's
/// clock back. [`Event::AgentDestroyed`] is the exception: the runtime reports it only as
/// [`Action::TearDownAgent`] asks, once the work phase has ended.
#[derive(Debug)]
pub struct Core<'w> {
    workflow: &'w Workflow,
    pattern: &'w SoloThenJudge,
    /// The latest time an event was observed, in milliseconds since the run started.
    now_ms: u64,
    phase: Phase,
    jobs: jobs::Jobs,
}

/// Where a run is.
#[derive(Debug)]
enum Phase {
    /// The agent works, from when the core asks for it. Whether its command has started yet
    /// changes no decision.
    Work {
        /// The latest submission.
        submission: Option<Submission>,
    },
    /// The agent finalized `submission`, which is locked as the selection, and the grace after
    /// it ([`SoloThenJudge::finalize_grace_seconds`]) ends at `grace_end_ms`.
    Finalized {
        submission: Submission,
        grace_end_ms: u64,
    },
    /// The work phase has ended. The agent's jobs are being stopped, and its sandbox frozen,
    /// collected and destroyed.
    Teardown {
        work_end: WorkEnd,
        agent_destroyed: bool,
        submission: Option<Submission>,
    },
    /// The final judge judges the selected submission in job `job`.
    Final {
        work_end: WorkEnd,
        submission: Submission,
        job: JobId,
    },
    /// The run has finished.
    Finished { work_end: WorkEnd },
}

impl<'w> Core<'w> {
    /// Starts a run of `workflow`: its work phase begins at time 0. Returns the core and its
    /// first actions.
    pub fn start(workflow: &'w Workflow) -> (Self, Vec<Action>) {
        let Main::SoloThenJudge(pattern) = workflow.main();
        let core = Self {
            workflow,
            pattern,
            now_ms: 0,
            phase: Phase::Work { submission: None },
            jobs: jobs::Jobs::default(),
        };
        (core, vec![Action::StartAgent])
    }

    /// Takes in `event`, observed `at_ms` milliseconds after the run started, and returns the
    /// actions it calls for, in the order to perform them.
    pub fn handle(&mut self, at_ms: u64, event: Event) -> Vec<Action> {
        self.now_ms = self.now_ms.max(at_ms);
        let mut actions = Vec::new();
        if self.now_ms >= self.deadline_ms() {
            self.end_work(WorkEnd::Deadline, &mut actions);
        }
        if let Phase::Finalized { grace_end_ms, .. } = self.phase
            && self.now_ms >= grace_end_ms
        {
            self.end_work(WorkEnd::Finalized, &mut actions);
        }
        match event {
            // Recorded for the run's log; nothing waits for it.
            Event::AgentStarted | Event::Tick => {}
            Event::AgentExited { status } => {
                self.end_work(WorkEnd::AgentExited(status), &mut actions);
            }
            Event::AgentFailed { reason } => {
                self.end_work(WorkEnd::AgentFailed(reason), &mut actions);
            }
            Event::AgentDestroyed => match &mut self.phase {
                Phase::Teardown {
                    agent_destroyed, ..
                } => *agent_destroyed = true,
                #[expect(
                    clippy::unreachable,
                    reason = "the runtime reports the agent destroyed only as \
                              `Action::TearDownAgent` asks, which the core asks once, as it \
                              enters the teardown, and the teardown lasts until then"
                )]
                phase @ (Phase::Work { .. }
                | Phase::Finalized { .. }
                | Phase::Final { .. }
                | Phase::Finished { .. }) => {
                    unreachable!(
                        "the agent was reported destroyed outside the teardown, in {phase:?}"
                    )
                }
            },
            Event::OpArrived { op, request } => self.op(op, request, &mut actions),
            Event::JudgeStarted { job } => self.jobs.started(job, at_ms),
            Event::JudgeExited { job, status: _ } => {
                self.jobs.ended(job, at_ms, jobs::End::Exited);
            }
            Event::JudgeTimedOut { job } => self.jobs.ended(job, at_ms, jobs::End::TimedOut),
            Event::JudgeCollected { job, result } => {
                self.jobs
                    .report(job, at_ms, jobs::Report::Collected(result));
            }
            Event::JudgeFailed { job, reason } => {
                self.jobs.report(job, at_ms, jobs::Report::Failed(reason));
            }
        }
        self.advance(&mut actions);
        actions
    }

    /// When the core next acts without another event, in milliseconds since the run started:
    /// at the deadline, or at the end of the grace after a finalize. The runtime sends
    /// [`Event::Tick`] once that time has come. `None` once no time is due, after the work
    /// phase.
    pub fn tick_due_ms(&self) -> Option<u64> {
        match self.phase {
            Phase::Work { .. } => Some(self.deadline_ms()),
            Phase::Finalized { grace_end_ms, .. } => Some(grace_end_ms.min(self.deadline_ms())),
            Phase::Teardown { .. } | Phase::Final { .. } | Phase::Finished { .. } => None,
        }
    }

    /// The time the work phase ends at the latest, in milliseconds since the run started.
    fn deadline_ms(&self) -> u64 {
        self.pattern.deadline_seconds.get().saturating_mul(1000)
    }

    /// How long the agent may keep running after its finalize is accepted, in milliseconds.
    fn finalize_grace_ms(&self) -> u64 {
        self.pattern.finalize_grace_seconds.saturating_mul(1000)
    }

    /// Locks `submission`, finalized, as the selection, and starts the grace after it. A grace
    /// of 0 ends the work phase at once.
    fn finalize(&mut self, submission: Submission, actions: &mut Vec<Action>) {
        let grace_ms = self.finalize_grace_ms();
        self.phase = Phase::Finalized {
            submission,
            grace_end_ms: self.now_ms.saturating_add(grace_ms),
        };
        if grace_ms == 0 {
            self.end_work(WorkEnd::Finalized, actions);
        }
    }

    /// Ends the work phase, if it hasn't ended: stops the queued and running jobs, then
    /// freezes, collects and destroys the agent.
    fn end_work(&mut self, work_end: WorkEnd, actions: &mut Vec<Action>) {
        let submission = match &mut self.phase {
            Phase::Work { submission } => submission.take(),
            Phase::Finalized { submission, .. } => Some(submission.clone()),
            Phase::Teardown { .. } | Phase::Final { .. } | Phase::Finished { .. } => return,
        };
        self.phase = Phase::Teardown {
            work_end,
            agent_destroyed: false,
            submission,
        };
        let stopped = self.jobs.stop_unfinished(self.now_ms);
        actions.extend(stopped.into_iter().map(|job| Action::StopJudge { job }));
        actions.push(Action::TearDownAgent);
    }

    /// Moves the run on once what it waits for is done: the final phase once the teardown is
    /// confirmed, and the run's end once the final job has ended.
    fn advance(&mut self, actions: &mut Vec<Action>) {
        match &self.phase {
            Phase::Teardown {
                work_end,
                agent_destroyed: true,
                submission,
            } if self.jobs.all_ended() => {
                let (work_end, submission) = (work_end.clone(), submission.clone());
                self.enter_final(work_end, submission, actions);
            }
            Phase::Final {
                work_end,
                submission,
                job,
            } => {
                let Some(outcome) = self.jobs.outcome(*job) else {
                    return;
                };
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
            | Phase::Finished { .. } => {}
        }
    }

    /// Enters the final phase: judges the selected submission with the score, or finishes
    /// without a score if there is none.
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
        let timeout_seconds = self.pattern.score.timeout_seconds.get();
        let job = self.jobs.add(JudgeCall::Score);
        actions.push(Action::StartJudge {
            job,
            call: JudgeCall::Score,
            payload: submission.payload.clone(),
            timeout: Duration::from_secs(timeout_seconds),
        });
        self.phase = Phase::Final {
            work_end,
            submission,
            job,
        };
    }

    fn finish(&mut self, outcome: Outcome, actions: &mut Vec<Action>) {
        self.phase = Phase::Finished {
            work_end: outcome.work_end.clone(),
        };
        actions.push(Action::Finish { outcome });
    }

    /// The longest a new call of `grant` may run, in milliseconds: the smaller of the grant's
    /// call timeout and its cumulative time left.
    fn call_limit_ms(&self, grant: JudgeGrantId) -> u64 {
        let call_timeout = self.workflow.judge_grant(grant).call_timeout_seconds;
        let call_ms = call_timeout.get().saturating_mul(1000);
        self.cumulative_left_ms(grant)
            .map_or(call_ms, |left_ms| left_ms.min(call_ms))
    }

    /// The judge time `grant` has left, in milliseconds: `None` without a cumulative limit.
    fn cumulative_left_ms(&self, grant: JudgeGrantId) -> Option<u64> {
        let total = self.workflow.judge_grant(grant).cumulative_seconds?;
        let used_ms = self.jobs.usage(grant).used_ms;
        Some(total.get().saturating_mul(1000).saturating_sub(used_ms))
    }
}
