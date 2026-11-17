//! The ops that the agent and the calls send, as the core takes them: the request that an op
//! makes, what stays of the op to answer it, and what a reply says of the op.

use std::mem;

use decision::{Action, PayloadRef, Request};
use gateway::{IncomingOp, Op};
use schema::sandbox::op::{OpReply, SubmitReply, SubmitState};

use crate::record;

// Whether the core took the op that this answers: the record keeps the payload of such an op
// alone.
pub(crate) fn accepts(reply: &OpReply) -> bool {
    match reply {
        OpReply::Call(_) | OpReply::Submit(_) => true,
        OpReply::Ops(_)
        | OpReply::Status(_)
        | OpReply::Lease(_)
        | OpReply::LeaseStatus(_)
        | OpReply::Refused { .. } => false,
    }
}

// Whether the actions answer a submission as accepted for good.
pub(crate) fn finalizes(actions: &[Action]) -> bool {
    actions.iter().any(|action| {
        matches!(
            action,
            Action::Reply {
                reply: OpReply::Submit(SubmitReply {
                    state: SubmitState::Accepted,
                    ..
                }),
                ..
            }
        )
    })
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

pub(crate) fn request(mut incoming: IncomingOp) -> (Request, Unanswered) {
    // The request takes the op's fields, and what stays of the op only answers it.
    let (request, call) = match mem::replace(&mut incoming.op, Op::Ops) {
        Op::Call { function, payload } => {
            let reference = record::reference(&payload);
            let request = Request::Call {
                function,
                payload: reference.clone(),
            };
            (request, Some((payload, reference)))
        }
        Op::Submit {
            grant,
            finalize,
            payload,
        } => {
            let reference = record::reference(&payload);
            let request = Request::Submit {
                grant,
                finalize,
                payload: reference.clone(),
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
