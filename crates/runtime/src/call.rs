//! Calls of the agent's functions, each kind's in a module of its own below this one: a job's,
//! each a child of the run ([`child`](super::child)), and a now function's, which the loop awaits.

pub(super) mod job;
pub(super) mod now;
