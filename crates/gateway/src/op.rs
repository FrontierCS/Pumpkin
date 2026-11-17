//! The ops a session's owner is handed.

use std::collections::BTreeMap;

use schema::ir::Name;
use schema::sandbox::op::{OpCall, SessionOp};

/// What an op asks of the session's owner: an [`OpCall`], with the payload that a function's call
/// takes, of at most the session's limit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// See [`OpCall::Ops`].
    Ops,
    /// See [`OpCall::Call`], whose payload is `payload`.
    Call {
        function: Name,
        arguments: BTreeMap<Name, String>,
        payload: Vec<u8>,
    },
    /// See [`OpCall::Status`].
    Status { job: u64 },
    /// See [`OpCall::Session`].
    Session { grant: Name, op: SessionOp },
}

pub(crate) fn whole(call: OpCall, payload: Vec<u8>) -> Op {
    match call {
        OpCall::Ops => Op::Ops,
        OpCall::Call { grant, arguments } => Op::Call {
            function: grant,
            arguments,
            payload,
        },
        OpCall::Status { job } => Op::Status { job },
        OpCall::Session { grant, op } => Op::Session { grant, op },
    }
}
