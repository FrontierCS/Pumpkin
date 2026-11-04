//! A judge call's result, parsed from its `result.json`.

use pumpkin_schema::ir::ResultSpec;
use pumpkin_schema::sandbox::op::{Failed, JobState, Returned};
use serde_json::{Number, Value};
use stdx::Context;

/// A judge call's result, parsed from its `result.json` per its judge's [`ResultSpec`]: a
/// status, a score when it passed, and the fields its caller's reply returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JudgeResult {
    status: Status,
    returned: Returned,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Status {
    Passed { score: Number },
    Failed,
}

impl JudgeResult {
    /// Parses a `result.json` per `spec`.
    ///
    /// # Errors
    ///
    /// Fails, with the reason, if the file is not JSON, if its status is not `"passed"` or
    /// `"failed"`, or if its score breaks the rule of [`ResultSpec::score`].
    pub fn parse(spec: &ResultSpec, json: &[u8]) -> Result<Self, String> {
        let ResultSpec {
            status,
            score,
            returns,
            evidence_max_bytes: _,
        } = spec;
        let document: Value = serde_json::from_slice(json).context("result.json is not JSON")?;
        let passed = match status.resolve(&document).and_then(Value::as_str) {
            Some("passed") => true,
            Some("failed") => false,
            _ => {
                return Err(format!(
                    "the status at `{status}` is not \"passed\" or \"failed\": write one of them"
                ));
            }
        };
        let score_value = score.resolve(&document).filter(|value| !value.is_null());
        let status = match (passed, score_value) {
            (true, Some(Value::Number(number))) => Status::Passed {
                score: number.clone(),
            },
            (true, Some(_)) => {
                return Err(format!(
                    "the score at `{score}` is not a number: write a number there"
                ));
            }
            (true, None) => {
                return Err(format!(
                    "the result passed with no score at `{score}`: write one there"
                ));
            }
            (false, None) => Status::Failed,
            (false, Some(_)) => {
                return Err(format!(
                    "the result failed, yet has a score at `{score}`: leave it out, or write null"
                ));
            }
        };
        let returned = returns
            .iter()
            .filter_map(|(key, pointer)| {
                Some((key.to_owned(), pointer.resolve(&document)?.clone()))
            })
            .collect();
        Ok(Self { status, returned })
    }

    pub fn passed(&self) -> bool {
        match self.status {
            Status::Passed { .. } => true,
            Status::Failed => false,
        }
    }

    /// The score, which a passed result has, and a failed one doesn't.
    pub fn score(&self) -> Option<&Number> {
        match &self.status {
            Status::Passed { score } => Some(score),
            Status::Failed => None,
        }
    }

    /// The fields the caller's reply returns, by key: those the judge's
    /// [`ResultSpec::returns`] names and the result has.
    pub fn returned(&self) -> &Returned {
        &self.returned
    }

    /// The state of the job that gave this result, as a status reply gives it.
    pub(super) fn job_state(&self) -> JobState {
        let returned = self.returned.clone();
        match self.status {
            Status::Passed { .. } => JobState::Passed { returned },
            Status::Failed => JobState::Failed(Failed::Judged { returned }),
        }
    }
}
