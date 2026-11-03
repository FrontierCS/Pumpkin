//! The jobs that judge calls become, and the meter they keep.
//!
//! A call counts from when it is accepted, even if its job fails before its command starts.
//! Its seconds run from its command's start to the command's exit, to its stop at the timeout,
//! to the job's failure, or to the core stopping it when the work phase ends, each as the
//! runtime observed it. A call's place under the concurrency stays taken until its job has
//! ended, once its outputs are collected and its sandbox destroyed.
//!
//! The cumulative seconds left are what the grant allows less what ended commands used, so
//! calls that run at once may overshoot the grant, by as much as
//! [`cumulative_seconds`](pumpkin_schema::ir::JudgeGrant::cumulative_seconds) says.
//!
//! The quota ledger is not kept apart: what each grant has used is counted from its jobs
//! ([`Jobs::usage`]), so the two can't disagree.

use pumpkin_schema::sandbox::op::{self, FailureCode};
use pumpkin_schema::workflow::{JudgeCall, JudgeGrantId};

use super::{JobId, JudgeResult};

/// Why a job gave no result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// Its command ran past its timeout. A timed-out call returns no result, whatever its
    /// judge wrote.
    Timeout,
    /// It failed before giving a result, for the runtime's reason.
    Error(String),
    /// Its `result.json` was missing or invalid, for this reason.
    InvalidResult(String),
    /// The core stopped it when the work phase ended.
    Stopped,
}

impl Failure {
    /// The failure as a status reply gives it. Its message holds no detail from the judge or
    /// the runtime, which the run's log keeps, so a caller learns of a call only what its
    /// judge's declared fields say.
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
            #[expect(
                clippy::unreachable,
                reason = "the core stops jobs only as the work phase ends, and answers a status \
                          only during it"
            )]
            Self::Stopped => unreachable!("a status asked about a job the core stopped"),
        };
        op::Failure {
            code,
            message: message.to_owned(),
        }
    }
}

/// A run's jobs, numbered from 1 in the order they were added.
#[derive(Debug, Default)]
pub(super) struct Jobs(Vec<Job>);

#[derive(Debug)]
pub(super) struct Job {
    /// The agent's grant the call was made under, or the pattern's score.
    pub(super) call: JudgeCall,
    pub(super) state: State,
}

/// Where a job is.
#[derive(Debug)]
pub(super) enum State {
    /// Accepted; its command has not started.
    Queued,
    /// Its command started at `started_ms`.
    Running { started_ms: u64 },
    /// Its command has ended, or the core stopped the job; its sandbox is being collected and
    /// destroyed. `ran` is when its command ran, if it started.
    Ending { ran: Option<Span>, end: End },
    /// Its sandbox is gone.
    Ended {
        ran: Option<Span>,
        outcome: Result<JudgeResult, Failure>,
    },
}

/// When a job's command ran, in milliseconds since the run started.
#[derive(Clone, Copy, Debug)]
pub(super) struct Span {
    started_ms: u64,
    ended_ms: u64,
}

impl Span {
    fn ms(self) -> u64 {
        self.ended_ms.saturating_sub(self.started_ms)
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
    /// The time the commands that have ended ran, in milliseconds.
    pub(super) used_ms: u64,
    /// The calls whose jobs haven't ended.
    pub(super) running: u64,
}

impl Jobs {
    /// Adds a job, queued.
    pub(super) fn add(&mut self, call: JudgeCall) -> JobId {
        self.0.push(Job {
            call,
            state: State::Queued,
        });
        JobId(self.0.len() as u64)
    }

    pub(super) fn get(&self, job: JobId) -> Option<&Job> {
        let index = usize::try_from(job.0.checked_sub(1)?).ok()?;
        self.0.get(index)
    }

    fn get_mut(&mut self, job: JobId) -> Option<&mut Job> {
        let index = usize::try_from(job.0.checked_sub(1)?).ok()?;
        self.0.get_mut(index)
    }

    /// Notes that a queued job's command started.
    pub(super) fn started(&mut self, job: JobId, at_ms: u64) {
        if let Some(job) = self.get_mut(job)
            && let State::Queued = job.state
        {
            job.state = State::Running { started_ms: at_ms };
        }
    }

    /// Notes that a job's command ended. A job whose start wasn't reported ran for no time
    /// the core saw, but ends all the same, so that a timeout still leaves no result.
    pub(super) fn ended(&mut self, job: JobId, at_ms: u64, end: End) {
        let Some(job) = self.get_mut(job) else {
            return;
        };
        let ran = match job.state {
            State::Queued => None,
            State::Running { started_ms } => Some(Span {
                started_ms,
                ended_ms: at_ms,
            }),
            State::Ending { .. } | State::Ended { .. } => return,
        };
        job.state = State::Ending { ran, end };
    }

    /// Ends a job with its last report. A report that skips a step, such as a failure before
    /// the command started, ends the job all the same, its command's time running until the
    /// report if it had started.
    pub(super) fn report(&mut self, job: JobId, at_ms: u64, report: Report) {
        let Some(job) = self.get_mut(job) else {
            return;
        };
        let (ran, end) = match job.state {
            State::Queued => (None, None),
            State::Running { started_ms } => {
                let ended_ms = at_ms;
                (
                    Some(Span {
                        started_ms,
                        ended_ms,
                    }),
                    None,
                )
            }
            State::Ending { ran, end } => (ran, Some(end)),
            State::Ended { .. } => return,
        };
        let outcome = match (end, report) {
            (Some(End::Stopped), _) => Err(Failure::Stopped),
            (Some(End::TimedOut), _) => Err(Failure::Timeout),
            (Some(End::Exited) | None, Report::Collected(Ok(result))) => Ok(result),
            (Some(End::Exited) | None, Report::Collected(Err(reason))) => {
                Err(Failure::InvalidResult(reason))
            }
            (Some(End::Exited) | None, Report::Failed(reason)) => Err(Failure::Error(reason)),
        };
        job.state = State::Ended { ran, outcome };
    }

    /// Stops every job that is queued or running at `now_ms`, and returns them. A job whose
    /// command has already ended is left to finish collecting.
    pub(super) fn stop_unfinished(&mut self, now_ms: u64) -> Vec<JobId> {
        let mut stopped = Vec::new();
        for (index, job) in self.0.iter_mut().enumerate() {
            let ran = match job.state {
                State::Queued => None,
                State::Running { started_ms } => Some(Span {
                    started_ms,
                    ended_ms: now_ms,
                }),
                State::Ending { .. } | State::Ended { .. } => continue,
            };
            job.state = State::Ending {
                ran,
                end: End::Stopped,
            };
            stopped.push(JobId(index as u64 + 1));
        }
        stopped
    }

    pub(super) fn all_ended(&self) -> bool {
        self.0
            .iter()
            .all(|job| matches!(job.state, State::Ended { .. }))
    }

    /// What came of a job, once it has ended.
    pub(super) fn outcome(&self, job: JobId) -> Option<&Result<JudgeResult, Failure>> {
        match &self.get(job)?.state {
            State::Ended { outcome, .. } => Some(outcome),
            State::Queued | State::Running { .. } | State::Ending { .. } => None,
        }
    }

    /// What `grant`'s calls have used so far.
    pub(super) fn usage(&self, grant: JudgeGrantId) -> Usage {
        let mut usage = Usage::default();
        for job in (self.0.iter()).filter(|job| job.call == JudgeCall::Grant(grant)) {
            usage.calls += 1;
            match job.state {
                State::Queued | State::Running { .. } => usage.running += 1,
                State::Ending { ran, .. } => {
                    usage.running += 1;
                    usage.used_ms += ran.map_or(0, Span::ms);
                }
                State::Ended { ran, .. } => usage.used_ms += ran.map_or(0, Span::ms),
            }
        }
        usage
    }
}
