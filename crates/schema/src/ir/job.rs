//! Job functions: a body that runs in a fresh sandbox of a target role, on the caller's payload.

use std::num::NonZeroU64;

use serde::{Deserialize, Serialize};

use super::{Body, Name};

/// A body that a call runs in a fresh sandbox of `target`, as a job that the caller follows.
///
/// The host starts the target, calls the body with a handle on it and the payload, and ends the
/// target once the body returns; what it returns is what the job returned.
///
/// The function runs one call at a time: a call made while one of its calls is queued or running
/// is refused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobFunction {
    /// The target role that each call starts afresh, which no other job function names.
    pub target: Name,
    pub body: Body,
    /// The keyword arguments that each call passes the body, by the keyword-only parameter each
    /// names; absent, none.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub keywords: serde_json::Map<String, serde_json::Value>,
    /// The most bytes a call's payload may hold, at most
    /// [`MAX_OP_PAYLOAD_BYTES`](crate::sandbox::MAX_OP_PAYLOAD_BYTES), since it comes in an op.
    pub payload_max_bytes: NonZeroU64,
    /// The most calls the holder may make.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calls: Option<NonZeroU64>,
    /// The most time the holder's calls may take in all, each counting once it has ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cumulative_seconds: Option<NonZeroU64>,
    /// The timeout of each call, counted from its target's start. A call also ends with the
    /// function's cumulative time, or with the work phase, if either comes first.
    pub call_timeout_seconds: NonZeroU64,
}
