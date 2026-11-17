//! The ops that the agent and the calls send, as the core takes them: the request that an op
//! makes, what stays of the op to answer it, and what a reply says of the op.

use std::collections::BTreeMap;
use std::mem;

use decision::{Evaluated, PayloadRef, Request, Unevaluated};
use gateway::{IncomingOp, Op};
use schema::ir::Name;
use schema::sandbox::op::OpReply;

use crate::record;

// Whether the core took the op that this answers: the record keeps the payload of such an op
// alone.
pub(crate) fn accepts(reply: &OpReply) -> bool {
    match reply {
        OpReply::Call(_) => true,
        OpReply::Ops(_)
        | OpReply::Status(_)
        | OpReply::Lease(_)
        | OpReply::LeaseStatus(_)
        | OpReply::Refused { .. } => false,
    }
}

pub(crate) enum Unanswered {
    Query(IncomingOp),
    Call {
        incoming: IncomingOp,
        payload: Vec<u8>,
        reference: PayloadRef,
    },
}

impl Unanswered {
    pub(crate) fn into_incoming(self) -> IncomingOp {
        match self {
            Self::Query(incoming) | Self::Call { incoming, .. } => incoming,
        }
    }
}

// The request that `incoming` makes, and what answers it: the call of a function that `now`
// evaluates, given its name and its caller's arguments, as a now function's, with what that gave.
pub(crate) fn request(
    mut incoming: IncomingOp,
    now: impl FnOnce(&Name, &BTreeMap<Name, String>) -> Option<Result<Evaluated, Unevaluated>>,
) -> (Request, Unanswered) {
    // The request takes the op's fields, and what stays of the op only answers it.
    let (request, call) = match mem::replace(&mut incoming.op, Op::Ops) {
        Op::Call {
            function,
            arguments,
            payload,
        } => {
            let reference = record::reference(&payload);
            let request = match now(&function, &arguments) {
                Some(evaluated) => Request::Now {
                    function,
                    payload: reference.clone(),
                    evaluated,
                },
                None => Request::Call {
                    function,
                    payload: reference.clone(),
                    arguments,
                },
            };
            (request, Some((payload, reference)))
        }
        Op::Ops => (Request::Ops, None),
        Op::Status { job } => (Request::Status { job }, None),
        Op::Session { grant, op } => (Request::Session { grant, op }, None),
    };
    let unanswered = match call {
        Some((payload, reference)) => Unanswered::Call {
            incoming,
            payload,
            reference,
        },
        None => Unanswered::Query(incoming),
    };
    (request, unanswered)
}
