//! Now functions: a body on the run's state, evaluated as its call arrives, whose return is the
//! call's reply.

use std::num::NonZeroU64;

use serde::{Deserialize, Serialize};

use super::Body;

/// A body that a call evaluates at once, with a handle on the run's state, the payload and the
/// caller's arguments, and whose return the call replies with.
///
/// The body only proposes effects on the run's state, `run.select()` and `run.end_work()`; the
/// call applies them all, or, if one is refused, none.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NowFunction {
    pub body: Body,
    /// The most bytes a call's payload may hold, at most
    /// [`MAX_OP_PAYLOAD_BYTES`](crate::sandbox::MAX_OP_PAYLOAD_BYTES), since it comes in an op.
    pub payload_max_bytes: NonZeroU64,
}
