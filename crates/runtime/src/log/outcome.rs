//! What `outcome.json` says of a finished run, in the one form that the run writes and a reader
//! reads back.

use decision::{CallResult, Outcome, PayloadRef};
use schema::ir::{Name, Sha256Digest};
use schema::sandbox::ExitStatus;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::logged::Shown;

/// How a finished run ended: how its work phase did, and the verdict on its selection.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Finished {
    pub(crate) work_end: WorkEnd,
    pub(crate) verdict: Verdict,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum WorkEnd {
    Deadline,
    Finalized,
    AgentExited { status: ExitStatus },
    AgentFailed { reason: String },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Verdict {
    NoSubmission,
    /// The final call returned `result`.
    Returned {
        submission: Submission,
        result: Returned,
    },
    NoResult {
        submission: Submission,
        failure: Failure,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Submission {
    payload: Payload,
    finalized: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct Payload {
    size: u64,
    digest: Sha256Digest,
}

/// What the final call returned: each name with its value, in the body's order.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Returned {
    values: Vec<Shown>,
}

impl Returned {
    pub(crate) fn values(self) -> Vec<(Name, Value)> {
        let values = self.values.into_iter();
        values.map(|shown| (shown.name, shown.value)).collect()
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Failure {
    Timeout,
    Error { reason: String },
}

impl From<&Outcome> for Finished {
    fn from(outcome: &Outcome) -> Self {
        let Outcome { work_end, verdict } = outcome;
        let work_end = match work_end {
            decision::WorkEnd::Deadline => WorkEnd::Deadline,
            decision::WorkEnd::Finalized => WorkEnd::Finalized,
            decision::WorkEnd::AgentExited(status) => WorkEnd::AgentExited { status: *status },
            decision::WorkEnd::AgentFailed(reason) => WorkEnd::AgentFailed {
                reason: reason.clone(),
            },
        };
        let verdict = match verdict {
            decision::Verdict::NoSubmission => Verdict::NoSubmission,
            decision::Verdict::Returned { submission, result } => Verdict::Returned {
                submission: submission.into(),
                result: result.into(),
            },
            decision::Verdict::NoResult {
                submission,
                failure,
            } => Verdict::NoResult {
                submission: submission.into(),
                failure: match failure {
                    decision::Failure::Timeout => Failure::Timeout,
                    decision::Failure::Error(reason) => Failure::Error {
                        reason: reason.clone(),
                    },
                },
            },
        };
        Self { work_end, verdict }
    }
}

impl From<&decision::Submission> for Submission {
    fn from(submission: &decision::Submission) -> Self {
        let decision::Submission { payload, finalized } = submission;
        let PayloadRef { size, digest } = payload;
        Self {
            payload: Payload {
                size: *size,
                digest: digest.clone(),
            },
            finalized: *finalized,
        }
    }
}

impl From<&CallResult> for Returned {
    fn from(result: &CallResult) -> Self {
        let values = result.values().iter();
        let values = values.map(|(name, value)| Shown {
            name: name.clone(),
            value: value.clone(),
        });
        Self {
            values: values.collect(),
        }
    }
}
