//! What `outcome.json` says of how a run ended, in the one form that the run writes and a reader
//! reads back.

use decision::{CallResult, Outcome, PayloadRef};
use schema::ir::{Name, Sha256Digest};
use schema::sandbox::ExitStatus;
use schema::workflow::Workflow;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::logged::Shown;
use crate::{Signal, Stopped};

/// How a run ended: it finished, or it stopped without an outcome, as [`Stopped`] says.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum Ended {
    Finished(Finished),
    Failed { reason: String },
    Interrupted { signal: Signal },
}

/// How a finished run ended: how its work phase did, and the verdict on its selection.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum Finished {
    Single(Single),
    Team(Team),
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Team {
    pub(crate) members: Vec<MemberEnding>,
    pub(crate) verdict: TeamVerdict,
}

impl<'de> Deserialize<'de> for Finished {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        // Choose the recorded shape before parsing, so malformed verdicts retain their diagnostics.
        if value.get("members").is_some() {
            serde_json::from_value(value)
                .map(Self::Team)
                .map_err(serde::de::Error::custom)
        } else {
            serde_json::from_value(value)
                .map(Self::Single)
                .map_err(serde::de::Error::custom)
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MemberEnding {
    pub role: Name,
    work_end: WorkEnd,
    submission: Option<Submission>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route_failure: Option<String>,
}

impl MemberEnding {
    pub fn has_selection(&self) -> bool {
        self.submission.is_some()
    }

    pub fn reason(&self) -> String {
        match &self.work_end {
            WorkEnd::Deadline => "deadline".to_owned(),
            WorkEnd::Finalized => "finalized".to_owned(),
            WorkEnd::AgentExited {
                status: ExitStatus::Code(code),
            } => format!("exited with status {code}"),
            WorkEnd::AgentExited {
                status: ExitStatus::Signal(signal),
            } => format!("killed by signal {signal}"),
            WorkEnd::AgentFailed { reason } => format!("failed: {reason}"),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum TeamVerdict {
    Returned { result: Returned },
    NoResult { failure: Failure },
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Single {
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

impl Ended {
    pub(super) fn of(workflow: &Workflow, ended: &Result<Outcome, Stopped>) -> Self {
        match ended {
            Ok(Outcome::Single { work_end, verdict }) => Self::Finished(Finished::Single(Single {
                work_end: work_end.into(),
                verdict: verdict.into(),
            })),
            Ok(Outcome::Team { members, result }) => {
                let members = members
                    .iter()
                    .map(|member| MemberEnding {
                        role: workflow.sandbox(member.agent.role()).name.clone(),
                        work_end: (&member.work_end).into(),
                        submission: member.submission.as_ref().map(Into::into),
                        route_failure: member.route_failure.clone(),
                    })
                    .collect();
                let verdict = match result {
                    Ok(result) => TeamVerdict::Returned {
                        result: result.into(),
                    },
                    Err(failure) => TeamVerdict::NoResult {
                        failure: failure.into(),
                    },
                };
                Self::Finished(Finished::Team(Team { members, verdict }))
            }
            Err(Stopped::Failed(reason)) => Self::Failed {
                reason: reason.clone(),
            },
            Err(Stopped::Interrupted(signal)) => Self::Interrupted { signal: *signal },
        }
    }
}

impl From<&decision::WorkEnd> for WorkEnd {
    fn from(end: &decision::WorkEnd) -> Self {
        match end {
            decision::WorkEnd::Deadline => Self::Deadline,
            decision::WorkEnd::Finalized => Self::Finalized,
            decision::WorkEnd::AgentExited(status) => Self::AgentExited { status: *status },
            decision::WorkEnd::AgentFailed(reason) => Self::AgentFailed {
                reason: reason.clone(),
            },
        }
    }
}

impl From<&decision::Failure> for Failure {
    fn from(failure: &decision::Failure) -> Self {
        match failure {
            decision::Failure::Timeout => Self::Timeout,
            decision::Failure::Error(reason) => Self::Error {
                reason: reason.clone(),
            },
        }
    }
}

impl From<&decision::Verdict> for Verdict {
    fn from(verdict: &decision::Verdict) -> Self {
        match verdict {
            decision::Verdict::NoSubmission => Self::NoSubmission,
            decision::Verdict::Returned { submission, result } => Self::Returned {
                submission: submission.into(),
                result: result.into(),
            },
            decision::Verdict::NoResult {
                submission,
                failure,
            } => Self::NoResult {
                submission: submission.into(),
                failure: failure.into(),
            },
        }
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
