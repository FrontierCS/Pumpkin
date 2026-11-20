//! The final call: the body that judges the selection once the work phase has ended.

use std::num::NonZeroU64;

use serde::{Deserialize, Serialize};

use super::{Label, Name};

/// What the run calls with the selection once the work phase has ended: a body that runs in a
/// fresh sandbox of `target`, as a job function's does, on the selected submission.
///
/// The host starts the target, calls the body with a handle on it and the selection, and ends
/// the target once the body returns. What it returns is the run's result: each of its entries is
/// a value the run shows, in the order the body gives them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Final {
    /// The target that the call starts, which no job function names.
    pub target: Name,
    pub body: FinalBody,
    /// Absent, the selected payload of the sole agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<FinalInput>,
    /// The keyword arguments that the call passes the body, by the keyword-only parameter each
    /// names; absent, none.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub keywords: serde_json::Map<String, serde_json::Value>,
    /// The most bytes a submission may hold, at most
    /// [`MAX_OP_PAYLOAD_BYTES`](crate::sandbox::MAX_OP_PAYLOAD_BYTES), since each comes in an op:
    /// what is selected fits the call.
    pub payload_max_bytes: NonZeroU64,
    /// The call's timeout, counted from its target's start.
    pub timeout_seconds: NonZeroU64,
}

/// The input that the driver supplies to the final body.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinalInput {
    /// A JSON manifest naming every agent and the file staged for each present selection.
    Selections,
}

/// The final call's body: a `def` of a file of the workflow's, as a [`Body`](super::Body) is,
/// with no caller to give it arguments.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalBody {
    pub file: Label,
    pub def: Name,
}
