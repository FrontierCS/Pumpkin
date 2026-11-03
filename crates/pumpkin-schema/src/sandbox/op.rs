//! Ops: the calls a caller in a sandbox makes to the session's owner, and the owner's replies.
//!
//! A caller makes an op with `sandboxctl`, which has a command for each call and prints the
//! reply's JSON form on one line. This module is the reference for that JSON, which is what an
//! agent reads; `sandboxctl --help` gives the commands' arguments and exit codes.
//!
//! | Command | Call | Reply |
//! | --- | --- | --- |
//! | `sandboxctl ops` | [`OpCall::Ops`] | [`OpsReply`] |
//! | `sandboxctl judge <grant> <file>` | [`OpCall::Judge`] | [`JudgeReply`] |
//! | `sandboxctl submit <grant> <file> [--finalize]` | [`OpCall::Submit`] | [`SubmitReply`] |
//! | `sandboxctl status <job>` | [`OpCall::Status`] | [`StatusReply`] |
//!
//! The owner may refuse any op instead, with [`OpReply::Refused`], whose message says what is
//! wrong and what to do instead.
//!
//! A sandbox's callers take turns ([`super`] says why). A call's payload is read from its file
//! only in its turn, so leave the file as it is until the call returns; a call whose
//! `sandboxctl` has gone by its turn is dropped unsent, and any other is carried out even if its
//! `sandboxctl` is killed meanwhile.
//!
//! # JSON
//!
//! A reply is an object whose `kind` names it: `ops`, `judge`, `submit`, `status` or
//! `refused`. Its other fields are its type's, under the same names; a limit or a quota that
//! doesn't exist is `null`. A status has `returned` once its job's judge has given a result,
//! whether the job passed or failed, and `failure` only if the job failed without one. The
//! crate's tests pin an example of each form.

use std::num::{NonZeroU32, NonZeroU64};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::ir::{Name, Resources};

/// What an op asks of the session's owner, which decides whether the sandbox may ask it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OpCall {
    /// Asks for the caller's limits: the time left to work, its resources and the grants it
    /// holds.
    Ops,
    /// Calls the judge grant named `grant` with the op's payload, which a new job judges.
    Judge { grant: Name },
    /// Submits the op's payload to the submit grant named `grant`. `finalize` marks it as the
    /// final submission.
    Submit { grant: Name, finalize: bool },
    /// Asks how job `job`, which an earlier judge call started, is doing.
    Status { job: u64 },
}

/// The owner's answer to an op: the reply for the op's kind, or a refusal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OpReply {
    Ops(OpsReply),
    Judge(JudgeReply),
    Submit(SubmitReply),
    Status(StatusReply),
    /// The owner refused the op, and did nothing, for the reason given.
    Refused {
        message: String,
    },
}

/// The reply to [`OpCall::Ops`]: the time left to work, the caller's resources and the grants
/// it holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpsReply {
    /// The time left to work, in whole seconds rounded down: until the deadline.
    pub remaining_seconds: u64,
    /// The caller's CPU and memory limits: its role's.
    pub resources: Resources,
    pub grants: Vec<Grant>,
}

/// A grant that a caller holds, and its limits. Its `kind` says which call takes it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Grant {
    Judge(JudgeGrant),
    Submit(SubmitGrant),
}

/// A grant that [`OpCall::Judge`] calls; see [`ir::JudgeGrant`](crate::ir::JudgeGrant) for
/// its limits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgeGrant {
    pub name: Name,
    /// The most bytes a payload may hold: its judge slot's.
    pub max_bytes: NonZeroU64,
    pub calls: Option<NonZeroU64>,
    pub cumulative_seconds: Option<NonZeroU64>,
    pub concurrency: NonZeroU32,
    pub call_timeout_seconds: NonZeroU64,
}

/// A grant that [`OpCall::Submit`] submits to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitGrant {
    pub name: Name,
    /// The most bytes a payload may hold: the slot's of the judge that scores the selection.
    pub max_bytes: NonZeroU64,
}

/// The reply to [`OpCall::Judge`]: the job that judges the payload, queued, and what the grant
/// has left now that the call counts against it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgeReply {
    pub job: u64,
    pub remaining: Remaining,
}

/// What a judge grant has left of its quotas.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Remaining {
    /// The calls left, or `None` if the grant doesn't limit them.
    pub calls: Option<u64>,
    /// The judge time left, in whole seconds rounded down, or `None` if the grant doesn't
    /// limit it.
    pub seconds: Option<u64>,
}

/// The reply to [`OpCall::Submit`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitReply {
    pub state: SubmitState,
    /// The time left before the deadline, in whole seconds rounded down.
    pub remaining_seconds: u64,
}

/// What became of a submission.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmitState {
    /// Stored, as the latest submission.
    Stored,
    /// Accepted as the final submission.
    Accepted,
}

/// The reply to [`OpCall::Status`]: how a job is doing, and what its grant has left.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "StatusJson", into = "StatusJson")]
pub struct StatusReply {
    pub job: u64,
    pub state: JobState,
    pub remaining: Remaining,
}

/// Where a job is, with what came of it once it has finished. Its JSON form is the status's
/// `state`, with `returned` or `failure` beside it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobState {
    /// Accepted; its judge hasn't started.
    Queued,
    /// Its judge is running, or has ended and is being cleaned up.
    Running,
    /// The judge's result passed the payload.
    Passed { returned: Returned },
    /// The judge's result failed the payload, or the judge gave no result.
    Failed(Failed),
}

/// How a job failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failed {
    /// The judge's result failed the payload.
    Judged { returned: Returned },
    /// The judge gave no result.
    NoResult { failure: Failure },
}

/// The fields of a judge's result that its caller sees, by name, each with the result's value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Returned(Map<String, Value>);

impl FromIterator<(String, Value)> for Returned {
    fn from_iter<I: IntoIterator<Item = (String, Value)>>(fields: I) -> Self {
        Self(fields.into_iter().collect())
    }
}

/// Why a job gave no result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Failure {
    pub code: FailureCode,
    /// What happened, in a sentence.
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCode {
    /// The judge ran past its timeout.
    JudgeTimeout,
    /// The judge failed, or was stopped, before giving a result.
    JudgeError,
    /// The judge wrote no valid result.
    InvalidResult,
}

/// The JSON form of a [`StatusReply`], which a status's state checks the rest of.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusJson {
    job: u64,
    state: StateName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    returned: Option<Returned>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    failure: Option<Failure>,
    remaining: Remaining,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StateName {
    Queued,
    Running,
    Passed,
    Failed,
}

impl TryFrom<StatusJson> for StatusReply {
    type Error = &'static str;

    fn try_from(json: StatusJson) -> Result<Self, Self::Error> {
        let StatusJson {
            job,
            state,
            returned,
            failure,
            remaining,
        } = json;
        let state = match (state, returned, failure) {
            (StateName::Queued, None, None) => JobState::Queued,
            (StateName::Running, None, None) => JobState::Running,
            (StateName::Passed, Some(returned), None) => JobState::Passed { returned },
            (StateName::Failed, Some(returned), None) => {
                JobState::Failed(Failed::Judged { returned })
            }
            (StateName::Failed, None, Some(failure)) => {
                JobState::Failed(Failed::NoResult { failure })
            }
            (StateName::Queued | StateName::Running, _, _) => {
                return Err("a job that hasn't finished has no result: give it neither \
                            `returned` nor `failure`");
            }
            (StateName::Passed, _, _) => {
                return Err("a passed job has a result: give it `returned`, and no `failure`");
            }
            (StateName::Failed, _, _) => {
                return Err(
                    "a failed job has a result or a failure: give it exactly one of \
                            `returned` and `failure`",
                );
            }
        };
        Ok(Self {
            job,
            state,
            remaining,
        })
    }
}

impl From<StatusReply> for StatusJson {
    fn from(reply: StatusReply) -> Self {
        let StatusReply {
            job,
            state,
            remaining,
        } = reply;
        let (state, returned, failure) = match state {
            JobState::Queued => (StateName::Queued, None, None),
            JobState::Running => (StateName::Running, None, None),
            JobState::Passed { returned } => (StateName::Passed, Some(returned), None),
            JobState::Failed(Failed::Judged { returned }) => {
                (StateName::Failed, Some(returned), None)
            }
            JobState::Failed(Failed::NoResult { failure }) => {
                (StateName::Failed, None, Some(failure))
            }
        };
        Self {
            job,
            state,
            returned,
            failure,
            remaining,
        }
    }
}
