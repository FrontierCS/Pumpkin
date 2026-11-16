//! The ops a session's owner is handed.

use std::num::NonZeroU64;

use schema::ir::{AbsolutePath, Argv, Name};
use schema::sandbox::op::OpCall;

/// What an op asks of the session's owner: an [`OpCall`], with the payload that a judge call or a
/// submission takes, of at most the session's limit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// See [`OpCall::Ops`].
    Ops,
    /// See [`OpCall::Judge`], whose payload is `payload`.
    Judge { grant: Name, payload: Vec<u8> },
    /// See [`OpCall::Submit`], whose payload is `payload`.
    Submit {
        grant: Name,
        finalize: bool,
        payload: Vec<u8>,
    },
    /// See [`OpCall::Status`].
    Status { job: u64 },
    /// See [`OpCall::LeaseOpen`].
    LeaseOpen {
        grant: Name,
        instance_seconds: Option<NonZeroU64>,
    },
    /// See [`OpCall::LeaseRun`].
    LeaseRun {
        lease: u64,
        argv: Argv,
        timeout_seconds: Option<NonZeroU64>,
    },
    /// See [`OpCall::LeasePut`].
    LeasePut {
        lease: u64,
        tree: AbsolutePath,
        to: AbsolutePath,
        replace: bool,
    },
    /// See [`OpCall::LeaseGet`].
    LeaseGet {
        lease: u64,
        from: AbsolutePath,
        to: AbsolutePath,
    },
    /// See [`OpCall::LeaseStatus`].
    LeaseStatus { lease: u64 },
    /// See [`OpCall::LeaseClose`].
    LeaseClose { lease: u64 },
}

pub(crate) fn whole(call: OpCall, payload: Vec<u8>) -> Op {
    match call {
        OpCall::Ops => Op::Ops,
        OpCall::Judge { grant } => Op::Judge { grant, payload },
        OpCall::Submit { grant, finalize } => Op::Submit {
            grant,
            finalize,
            payload,
        },
        OpCall::Status { job } => Op::Status { job },
        OpCall::LeaseOpen {
            grant,
            instance_seconds,
        } => Op::LeaseOpen {
            grant,
            instance_seconds,
        },
        OpCall::LeaseRun {
            lease,
            argv,
            timeout_seconds,
        } => Op::LeaseRun {
            lease,
            argv,
            timeout_seconds,
        },
        OpCall::LeasePut {
            lease,
            tree,
            to,
            replace,
        } => Op::LeasePut {
            lease,
            tree,
            to,
            replace,
        },
        OpCall::LeaseGet { lease, from, to } => Op::LeaseGet { lease, from, to },
        OpCall::LeaseStatus { lease } => Op::LeaseStatus { lease },
        OpCall::LeaseClose { lease } => Op::LeaseClose { lease },
    }
}
