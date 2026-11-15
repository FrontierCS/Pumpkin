//! The jobs that judge calls become, and the meter they keep.
//!
//! A call counts from when it is accepted, unless its job fails before its command starts, as
//! when its sandbox fails to start: that is the run's or its provider's failure, not the
//! caller's, so the call is given back, as a lease that fails to start costs nothing. Its
//! seconds run from its command's start to the command's exit, to its stop at the timeout,
//! to the job's failure, or to the core stopping it when the work phase ends, each as the
//! runtime observed it. A call keeps its grant, which
//! [runs one call at a time](schema::ir::JudgeGrant), busy until its job has ended,
//! once its outputs are collected and its sandbox destroyed, and every lease it held has ended,
//! which the core reads from its leases.
//!
//! The cumulative seconds left are what the grant allows less what ended commands used. A call
//! is accepted only once the grant's earlier calls have all ended, and its timeout is at most
//! the time left then, so the grant's calls stay within
//! [`cumulative_seconds`](schema::ir::JudgeGrant::cumulative_seconds) but for how late
//! the runtime observes a call's end.
//!
//! The quota ledger is not kept apart: what each grant has used is counted from its jobs
//! ([`Jobs::usage`]), so the two can't disagree.

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;
use core::time::Duration;

use schema::sandbox::op::{self, FailureCode};
use schema::workflow::{JudgeCall, JudgeGrantId};

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

/// The run's jobs, each numbered by its index, in the order they were added.
#[derive(Debug, Default)]
pub(super) struct Jobs(Vec<Job>);

#[derive(Debug)]
pub(super) struct Job {
    /// What the call was made under.
    pub(super) call: JudgeCall,
    pub(super) timeout: Duration,
    pub(super) state: State,
}

#[derive(Debug)]
pub(super) enum State {
    /// Accepted; its command has not started.
    Queued,
    /// Its command started at `started`.
    Running { started: Duration },
    /// Its command has ended, or the core stopped the job; its sandbox is being collected and
    /// destroyed. `used` is how long its command ran, none if it didn't start.
    Ending { used: Duration, end: End },
    /// Its sandbox is gone. It is `charged` against its grant's calls unless it failed before
    /// its command started.
    Ended {
        used: Duration,
        outcome: Result<JudgeResult, Failure>,
        charged: bool,
    },
}

#[derive(Clone, Copy, Debug)]
pub(super) enum End {
    /// By itself.
    Exited,
    /// At its timeout.
    TimedOut,
    /// When the core stopped the job.
    Stopped,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Usage {
    /// The calls charged: those accepted, less those that failed before their command started.
    pub(super) calls: u64,
    /// The time the commands that have ended ran.
    pub(super) used: Duration,
    /// Whether one of the grant's calls hasn't ended.
    pub(super) busy: bool,
}

impl State {
    pub(super) fn started(&mut self, at: Duration) {
        if let Self::Queued = self {
            *self = Self::Running { started: at };
        }
    }

    /// A job whose start wasn't reported ran for no time the core saw, but ends all the same, so
    /// that a timeout still leaves no result.
    pub(super) fn ended(&mut self, at: Duration, end: End) {
        let used = match *self {
            Self::Queued => Duration::ZERO,
            Self::Running { started } => at.saturating_sub(started),
            Self::Ending { .. } | Self::Ended { .. } => return,
        };
        *self = Self::Ending { used, end };
    }

    /// A report that skips a step, such as a failure before the command started, ends the job
    /// all the same, its command's time running until the report if it had started.
    pub(super) fn report(&mut self, at: Duration, outcome: Result<JudgeResult, Failure>) {
        let charged = !matches!((&*self, &outcome), (Self::Queued, Err(_)));
        let (used, outcome) = match *self {
            Self::Queued => (Duration::ZERO, outcome),
            Self::Running { started } => (at.saturating_sub(started), outcome),
            // A stopped job ends as its report says, as after an exit: the core stops jobs only
            // as the work phase ends, after which no status asks what came of them.
            Self::Ending { used, end } => match end {
                End::TimedOut => (used, Err(Failure::Timeout)),
                End::Exited | End::Stopped => (used, outcome),
            },
            Self::Ended { .. } => return,
        };
        *self = Self::Ended {
            used,
            outcome,
            charged,
        };
    }
}

impl Jobs {
    pub(super) fn add(&mut self, call: JudgeCall, timeout: Duration) -> JobId {
        let job = JobId(self.0.len() as u64);
        self.0.push(Job {
            call,
            timeout,
            state: State::Queued,
        });
        job
    }

    pub(super) fn of(&self, grant: JudgeGrantId) -> impl Iterator<Item = JobId> {
        let jobs = self.0.iter().enumerate();
        jobs.filter(move |(_, job)| job.call == JudgeCall::Grant(grant))
            .map(|(index, _)| JobId(index as u64))
    }

    pub(super) fn get(&self, job: JobId) -> Option<&Job> {
        self.0.get(usize::try_from(job.0).ok()?)
    }

    pub(super) fn state_mut(&mut self, job: JobId) -> Option<&mut State> {
        Some(&mut self.0.get_mut(usize::try_from(job.0).ok()?)?.state)
    }

    /// Stops every job that is queued or running at `now`, and returns them. A job whose
    /// command has already ended is left to finish collecting.
    pub(super) fn stop_unfinished(&mut self, now: Duration) -> Vec<JobId> {
        let mut stopped = Vec::new();
        for (index, job) in self.0.iter_mut().enumerate() {
            match job.state {
                State::Queued | State::Running { .. } => {
                    job.state.ended(now, End::Stopped);
                    stopped.push(JobId(index as u64));
                }
                State::Ending { .. } | State::Ended { .. } => {}
            }
        }
        stopped
    }

    pub(super) fn all_ended(&self) -> bool {
        self.0
            .iter()
            .all(|job| matches!(job.state, State::Ended { .. }))
    }

    pub(super) fn usage(&self, grant: JudgeGrantId) -> Usage {
        let mut usage = Usage::default();
        for job in (self.0.iter()).filter(|job| job.call == JudgeCall::Grant(grant)) {
            let charged = !matches!(job.state, State::Ended { charged: false, .. });
            usage.calls += u64::from(charged);
            match job.state {
                State::Queued | State::Running { .. } => usage.busy = true,
                State::Ending { used, .. } => {
                    usage.busy = true;
                    usage.used += used;
                }
                State::Ended { used, .. } => usage.used += used,
            }
        }
        usage
    }
}
