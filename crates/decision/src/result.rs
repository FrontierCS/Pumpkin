use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::{String, ToString as _};
use alloc::vec::Vec;
use core::num::NonZeroU64;

use schema::ir::Name;
use schema::sandbox::op::{JobState, Returned};
use schema::sandbox::{RESULT_MAX_BYTES, RESULT_MAX_DEPTH};
use serde_json::Value;

/// What a call's body returned: each name with its value, in the body's order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallResult {
    values: Vec<(Name, Value)>,
}

impl CallResult {
    /// A body's return: each name with its value, in the body's order.
    ///
    /// # Errors
    ///
    /// Fails, with the reason, if the return, its dict included, nests deeper than
    /// [`RESULT_MAX_DEPTH`], or takes more than [`RESULT_MAX_BYTES`] as JSON.
    pub fn returned_by(values: Vec<(Name, Value)>) -> Result<Self, String> {
        let deepest = values.iter().map(|(_, value)| depth(value)).max();
        if 1 + deepest.unwrap_or(0) > RESULT_MAX_DEPTH {
            return Err(format!(
                "the body's return nests lists, tuples and dicts more than {RESULT_MAX_DEPTH} \
                 deep, its dict included: nest them at most that deep"
            ));
        }
        // The dict as JSON, as a caller's reply holds it.
        let dict = (values.iter()).map(|(name, value)| (name.as_str().to_owned(), value.clone()));
        let json_bytes = Value::Object(dict.collect()).to_string().len() as u64;
        if json_bytes > RESULT_MAX_BYTES {
            return Err(format!(
                "the body's return takes {json_bytes} bytes as JSON, over the limit of \
                 {RESULT_MAX_BYTES}: return at most that"
            ));
        }
        Ok(Self { values })
    }

    /// Each name with its value, in the order the body returned them.
    pub fn values(&self) -> &[(Name, Value)] {
        &self.values
    }

    /// The fields the caller's reply returns, by name.
    pub fn returned(&self) -> Returned {
        let fields = self.values.iter();
        let fields = fields.map(|(name, value)| (name.as_str().to_owned(), value.clone()));
        fields.collect()
    }

    pub(super) fn job_state(&self, model_requests_over_budget: Option<NonZeroU64>) -> JobState {
        JobState::Completed {
            returned: self.returned(),
            model_requests_over_budget,
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
