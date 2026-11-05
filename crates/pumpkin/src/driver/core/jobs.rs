//! The jobs that judge calls become, and the meter they keep.
//!
//! A call counts from when it is accepted, even if its job fails before its command starts.
//! Its seconds run from its command's start to the command's exit, to its stop at the timeout,
//! to the job's failure, or to the core stopping it when the work phase ends, each as the
//! runtime observed it. A call keeps its grant, which
//! [runs one call at a time](pumpkin_schema::ir::JudgeGrant), busy until its job has ended,
//! once its outputs are collected and its sandbox destroyed.
//!
//! The cumulative seconds left are what the grant allows less what ended commands used. A call
//! is accepted only once the grant's earlier calls have all ended, and its timeout is at most
//! the time left then, so the grant's calls stay within
//! [`cumulative_seconds`](pumpkin_schema::ir::JudgeGrant::cumulative_seconds) but for how late
//! the runtime observes a call's end.
//!
//! The quota ledger is not kept apart: what each grant has used is counted from its jobs
//! ([`Jobs::usage`]), so the two can't disagree.

use std::time::Duration;

use pumpkin_schema::sandbox::op::{self, FailureCode};
use pumpkin_schema::workflow::JudgeGrantId;

use super::{JobId, JudgeResult};

/// Why a job gave no result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// Its command ran past its timeout. A timed-out call returns no result, whatever its
    /// judge wrote.
    Timeout,
    /// It failed before giving a result, for the runtime's reason.
    Error(String),
    /// Its `result.json` was missing or invalid, or its evidence over its limit, for this
    /// reason.
    InvalidResult(String),
}

impl Failure {
    /// The failure as a status reply gives it. Its message holds no detail from the judge or
    /// the runtime, so a caller learns of a call only what its judge's declared fields say.
    pub(super) fn reply(&self) -> op::Failure {
        let (code, message) = match self {
            Self::Timeout => (FailureCode::JudgeTimeout, "the judge ran past its timeout"),
            Self::Error(_) => (
                FailureCode::JudgeError,
                "the judge failed before giving a result",
            ),
            Self::InvalidResult(_) => (
                FailureCode::InvalidResult,
                "the judge wrote no valid result",
            ),
        };
        op::Failure {
            code,
            message: message.to_owned(),
        }
    }
}

/// The agent's jobs, numbered from 1 in the order they were added. The score's job takes the
/// number after theirs, and the final phase holds it, so a number the agent asks about never
/// names it.
#[derive(Debug, Default)]
pub(super) struct Jobs(Vec<Job>);

#[derive(Debug)]
pub(super) struct Job {
    /// The agent's grant the call was made under.
    pub(super) grant: JudgeGrantId,
    pub(super) state: State,
}

/// Where a job is.
#[derive(Debug)]
pub(super) enum State {
    /// Accepted; its command has not started.
    Queued,
    /// Its command started at `started`.
    Running { started: Duration },
    /// Its command has ended, or the core stopped the job; its sandbox is being collected and
    /// destroyed. `ran` is when its command ran, if it started.
    Ending { ran: Option<Span>, end: End },
    /// Its sandbox is gone.
    Ended {
        ran: Option<Span>,
        outcome: Result<JudgeResult, Failure>,
    },
}

/// When a job's command ran, as times since the run started.
#[derive(Clone, Copy, Debug)]
pub(super) struct Span {
    started: Duration,
    ended: Duration,
}

impl Span {
    fn duration(self) -> Duration {
        self.ended.saturating_sub(self.started)
    }
}

/// How a job's command ended.
#[derive(Clone, Copy, Debug)]
pub(super) enum End {
    /// By itself.
    Exited,
    /// At its timeout.
    TimedOut,
    /// When the core stopped the job.
    Stopped,
}

/// A job's last report, once its sandbox is gone.
#[derive(Debug)]
pub(super) enum Report {
    Collected(Result<JudgeResult, String>),
    Failed(String),
}

/// What a grant's calls have used.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Usage {
    /// The calls accepted.
    pub(super) calls: u64,
    /// The time the commands that have ended ran.
    pub(super) used: Duration,
    /// Whether one of the grant's calls hasn't ended.
    pub(super) busy: bool,
}

impl State {
    /// Notes that a queued job's command started.
    pub(super) fn started(&mut self, at: Duration) {
        if let Self::Queued = self {
            *self = Self::Running { started: at };
        }
    }

    /// Notes that a job's command ended. A job whose start wasn't reported ran for no time
    /// the core saw, but ends all the same, so that a timeout still leaves no result.
    pub(super) fn ended(&mut self, at: Duration, end: End) {
        let ran = match *self {
            Self::Queued => None,
            Self::Running { started } => Some(Span { started, ended: at }),
            Self::Ending { .. } | Self::Ended { .. } => return,
        };
        *self = Self::Ending { ran, end };
    }

    /// Ends a job with its last report. A report that skips a step, such as a failure before
    /// the command started, ends the job all the same, its command's time running until the
    /// report if it had started.
    pub(super) fn report(&mut self, at: Duration, report: Report) {
        let (ran, end) = match *self {
            Self::Queued => (None, None),
            Self::Running { started } => (Some(Span { started, ended: at }), None),
            Self::Ending { ran, end } => (ran, Some(end)),
            Self::Ended { .. } => return,
        };
        // A stopped job ends as its report says, as after an exit: the core stops jobs only as
        // the work phase ends, after which no status asks what came of them.
        let outcome = match (end, report) {
            (Some(End::TimedOut), _) => Err(Failure::Timeout),
            (Some(End::Exited | End::Stopped) | None, Report::Collected(Ok(result))) => Ok(result),
            (Some(End::Exited | End::Stopped) | None, Report::Collected(Err(reason))) => {
                Err(Failure::InvalidResult(reason))
            }
            (Some(End::Exited | End::Stopped) | None, Report::Failed(reason)) => {
                Err(Failure::Error(reason))
            }
        };
        *self = Self::Ended { ran, outcome };
    }

    /// What came of the job, once it has ended.
    pub(super) fn outcome(&self) -> Option<&Result<JudgeResult, Failure>> {
        match self {
            Self::Ended { outcome, .. } => Some(outcome),
            Self::Queued | Self::Running { .. } | Self::Ending { .. } => None,
        }
    }
}

impl Jobs {
    /// Adds a job, queued, made under `grant`.
    pub(super) fn add(&mut self, grant: JudgeGrantId) -> JobId {
        let job = job_id(self.0.len());
        self.0.push(Job {
            grant,
            state: State::Queued,
        });
        job
    }

    /// The number the score's job takes: the one after the agent's jobs'.
    pub(super) fn score_job(&self) -> JobId {
        job_id(self.0.len())
    }

    pub(super) fn get(&self, job: JobId) -> Option<&Job> {
        self.0.get(job_index(job)?)
    }

    pub(super) fn state_mut(&mut self, job: JobId) -> Option<&mut State> {
        Some(&mut self.0.get_mut(job_index(job)?)?.state)
    }

    /// Stops every job that is queued or running at `now`, and returns them. A job whose
    /// command has already ended is left to finish collecting.
    pub(super) fn stop_unfinished(&mut self, now: Duration) -> Vec<JobId> {
        let mut stopped = Vec::new();
        for (index, job) in self.0.iter_mut().enumerate() {
            let ran = match job.state {
                State::Queued => None,
                State::Running { started } => Some(Span {
                    started,
                    ended: now,
                }),
                State::Ending { .. } | State::Ended { .. } => continue,
            };
            job.state = State::Ending {
                ran,
                end: End::Stopped,
            };
            stopped.push(job_id(index));
        }
        stopped
    }

    pub(super) fn all_ended(&self) -> bool {
        self.0
            .iter()
            .all(|job| matches!(job.state, State::Ended { .. }))
    }

    /// What `grant`'s calls have used so far.
    pub(super) fn usage(&self, grant: JudgeGrantId) -> Usage {
        let mut usage = Usage::default();
        for job in (self.0.iter()).filter(|job| job.grant == grant) {
            usage.calls += 1;
            match job.state {
                State::Queued | State::Running { .. } => usage.busy = true,
                State::Ending { ran, .. } => {
                    usage.busy = true;
                    usage.used += ran.map_or(Duration::ZERO, Span::duration);
                }
                State::Ended { ran, .. } => {
                    usage.used += ran.map_or(Duration::ZERO, Span::duration);
                }
            }
        }
        usage
    }
}

/// The number of the job at `index` in [`Jobs`], which numbers its jobs from 1.
fn job_id(index: usize) -> JobId {
    JobId(index as u64 + 1)
}

/// Where job `job` is in [`Jobs`], if its number is one a job can have.
fn job_index(job: JobId) -> Option<usize> {
    usize::try_from(job.0.checked_sub(1)?).ok()
}
