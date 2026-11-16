use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::String;

use schema::ir::ResultSpec;
use schema::sandbox::RESULT_MAX_DEPTH;
use schema::sandbox::op::{JobState, Returned};
use serde_json::{Number, Value};

/// A judge call's result, parsed from its `result.json` per its judge's [`ResultSpec`]: a score,
/// if it has one, and the fields its caller's reply returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JudgeResult {
    score: Option<Number>,
    returned: Returned,
}

impl JudgeResult {
    /// Parses a `result.json` per `spec`.
    ///
    /// # Errors
    ///
    /// Fails, with the reason, if the file is not JSON, nests deeper than [`RESULT_MAX_DEPTH`],
    /// or if its score breaks the rule of [`ResultSpec::score`].
    pub fn parse(spec: &ResultSpec, json: &[u8]) -> Result<Self, String> {
        let ResultSpec { score, returns } = spec;
        let document: Value = serde_json::from_slice(json)
            .map_err(|error| format!("result.json is not JSON: {error}"))?;
        if depth(&document) > RESULT_MAX_DEPTH {
            return Err(format!(
                "result.json nests arrays and objects more than {RESULT_MAX_DEPTH} deep: nest \
                 them at most that deep"
            ));
        }
        let score = match score.resolve(&document) {
            None | Some(Value::Null) => None,
            Some(Value::Number(number)) => Some(number.clone()),
            Some(_) => {
                return Err(format!(
                    "the score at `{score}` is not a number: write a number there, or null"
                ));
            }
        };
        let returned = returns
            .iter()
            .filter_map(|(key, pointer)| {
                Some((key.to_owned(), pointer.resolve(&document)?.clone()))
            })
            .collect();
        Ok(Self { score, returned })
    }

    pub fn score(&self) -> Option<&Number> {
        self.score.as_ref()
    }

    /// The fields the caller's reply returns, by key: those the judge's
    /// [`ResultSpec::returns`] names and the result has.
    pub fn returned(&self) -> &Returned {
        &self.returned
    }

    pub(super) fn job_state(&self) -> JobState {
        let Self { score: _, returned } = self;
        JobState::Completed {
            returned: returned.clone(),
        }
    }
}

/// How deep `value` nests arrays and objects, itself included: 0 for a scalar.
fn depth(value: &Value) -> usize {
    let deepest = |values: &mut dyn Iterator<Item = &Value>| values.map(depth).max().unwrap_or(0);
    match value {
        Value::Array(items) => 1 + deepest(&mut items.iter()),
        Value::Object(fields) => 1 + deepest(&mut fields.values()),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => 0,
    }
}
