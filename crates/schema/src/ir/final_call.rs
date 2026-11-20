//! The final call judges the submissions once the work phase has ended.

use std::num::NonZeroU64;

use serde::{Deserialize, Serialize};

use super::{Label, Name};

/// The body receives a fresh sandbox of `target` and every agent's submission, by role, in
/// declaration order. A submission is a payload of zero or more bytes, or `None` if absent.
///
/// The host starts the target, calls the body, and ends
/// the target once the body returns. What it returns is the run's result: each of its entries is
/// a value the run shows, in the order the body gives them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Final {
    /// The target that the call starts, which no job function names.
    pub target: Name,
    pub body: FinalBody,
    /// The keyword arguments that the call passes the body, by the keyword-only parameter each
    /// names; absent, none.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub keywords: serde_json::Map<String, serde_json::Value>,
    /// The most bytes a submission may hold, at most
    /// [`MAX_OP_PAYLOAD_BYTES`](crate::sandbox::MAX_OP_PAYLOAD_BYTES), since each comes in an op:
    /// every submission fits the call.
    pub payload_max_bytes: NonZeroU64,
    /// The call's timeout, counted from its target's start.
    pub timeout_seconds: NonZeroU64,
}

/// The final call's body: a `def` of a file of the workflow's, as a [`Body`](super::Body) is,
/// with no caller to give it arguments.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalBody {
    pub file: Label,
    pub def: Name,
}
