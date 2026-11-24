//! The ops and model requests that the sandbox sends, each assembled from its frames and handed
//! to the session's sink once whole.

use schema::sandbox::op::OpCall;
use schema::sandbox::{
    MAX_MODEL_REQUEST_BYTES, MAX_MODEL_REQUESTS_IN_FLIGHT, MAX_OP_PAYLOAD_BYTES, OpPayloadLimit,
};

use super::Session;
use crate::{Arrival, IncomingModelRequest, IncomingOp, ModelRequest, RefusedOp, Told, broke, op};

pub(super) struct Assembly {
    call: OpCall,
    // The payload's bytes still to arrive: the op is whole once none are.
    due: u64,
    payload: Payload,
}

enum Payload {
    Kept(Vec<u8>),
    // The limit is the session's, not the protocol's, so a payload past it is no break: its bytes
    // arrive and are dropped, and the op is refused only once all have, so that the sandbox's next
    // op follows it whole.
    Dropped { size: u64, limit: OpPayloadLimit },
}

impl Session {
    pub(super) fn open_op(
        &mut self,
        number: u32,
        call: OpCall,
        size: u64,
        told: &mut Told<'_>,
    ) -> Result<(), String> {
        let Some(limit) = self.limit else {
            return Err(broke(&format!(
                "it opened op {number}, but the session takes no ops"
            )));
        };
        if let Some(in_flight) = self.flight.op {
            return Err(broke(&format!(
                "it opened op {number} while op {in_flight} is in flight"
            )));
        }
        let payload = payload(number, &call, size, limit)?;
        self.flight.op = Some(number);
        let assembly = Assembly {
            call,
            due: size,
            payload,
        };
        if size == 0 {
            self.pass_on(number, assembly, told);
        } else {
            self.assembly = Some((number, assembly));
        }
        Ok(())
    }

    pub(super) fn op_data(
        &mut self,
        number: u32,
        bytes: &[u8],
        told: &mut Told<'_>,
    ) -> Result<(), String> {
        let Some((_, mut assembly)) = self.assembly.take().filter(|(open, _)| *open == number)
        else {
            return Err(broke(&format!(
                "it sent data for op {number}, which is not open"
            )));
        };
        let len = bytes.len() as u64;
        let Some(due) = assembly.due.checked_sub(len) else {
            return Err(broke(&format!(
                "it sent {len} bytes of op {number}'s payload, past its size: {} were left",
                assembly.due
            )));
        };
        assembly.due = due;
        match &mut assembly.payload {
            Payload::Kept(payload) => payload.extend_from_slice(bytes),
            Payload::Dropped { .. } => {}
        }
        if due == 0 {
            self.pass_on(number, assembly, told);
        } else {
            self.assembly = Some((number, assembly));
        }
        Ok(())
    }

    fn pass_on(&self, number: u32, assembly: Assembly, told: &mut Told<'_>) {
        let Assembly {
            call,
            due: _,
            payload,
        } = assembly;
        let session = self.id;
        let arrival = match payload {
            Payload::Kept(payload) => Arrival::Op(IncomingOp {
                session,
                number,
                op: op::whole(call, payload),
            }),
            Payload::Dropped { size, limit } => Arrival::Refused(RefusedOp {
                session,
                number,
                call,
                size,
                payload_limit: limit,
            }),
        };
        self.arrived(arrival, told);
    }

    pub(super) fn open_model(
        &mut self,
        number: u32,
        head: ModelRequest,
        size: u64,
        told: &mut Told<'_>,
    ) -> Result<(), String> {
        if !self.takes_models {
            return Err(broke(&format!(
                "it opened model request {number}, but the session takes no model requests: send \
                 one only on a session accepted to take them"
            )));
        }
        if size > MAX_MODEL_REQUEST_BYTES {
            return Err(broke(&format!(
                "it opened model request {number} with a body of {size} bytes, more than the \
                 protocol's {MAX_MODEL_REQUEST_BYTES}"
            )));
        }
        // `ModelRequest`'s readers rely on lowercase names, as the protocol has them. The message
        // leaves the name unquoted, as it may be anything the sandbox chose.
        if (head.headers.iter()).any(|(name, _)| name.bytes().any(|b| b.is_ascii_uppercase())) {
            return Err(broke(&format!(
                "it opened model request {number} with a header name that is not in lowercase"
            )));
        }
        if self.flight.models.contains(&number) {
            return Err(broke(&format!(
                "it opened model request {number}, which is in flight"
            )));
        }
        // The session's model requests that its owner holds are all in flight, so this bounds how
        // many it holds.
        if self.flight.models.len() >= MAX_MODEL_REQUESTS_IN_FLIGHT {
            return Err(broke(&format!(
                "it opened model request {number} while {MAX_MODEL_REQUESTS_IN_FLIGHT} are in \
                 flight, the most the protocol allows"
            )));
        }
        self.flight.models.insert(number);
        let Ok(capacity) = usize::try_from(size) else {
            unreachable!(
                "a model request's body of {size} bytes overflows a usize, though it is at most \
                 `MAX_MODEL_REQUEST_BYTES`, 64 MiB, which fits the usize of every Linux target, \
                 of 32 bits or more"
            )
        };
        let request = ModelRequest {
            body: Vec::with_capacity(capacity),
            ..head
        };
        if size == 0 {
            self.pass_on_model(number, request, told);
        } else {
            self.flight.bodies.insert(number, (request, size));
        }
        Ok(())
    }

    pub(super) fn model_data(
        &mut self,
        number: u32,
        bytes: &[u8],
        told: &mut Told<'_>,
    ) -> Result<(), String> {
        // Bytes for a request never opened, or for one whose body has all arrived, break the
        // protocol. Unlike a reply on a stream that its request has closed, they can't be the late
        // end of an answer the gateway gave up on: a body ends at the size the sandbox gave.
        let Some((mut request, due)) = self.flight.bodies.remove(&number) else {
            return Err(broke(&format!(
                "it sent data for model request {number}, which is not open"
            )));
        };
        let len = bytes.len() as u64;
        let Some(due) = due.checked_sub(len) else {
            return Err(broke(&format!(
                "it sent {len} bytes of model request {number}'s body, past its size: {due} were \
                 left"
            )));
        };
        request.body.extend_from_slice(bytes);
        if due == 0 {
            self.pass_on_model(number, request, told);
        } else {
            self.flight.bodies.insert(number, (request, due));
        }
        Ok(())
    }

    fn pass_on_model(&self, number: u32, request: ModelRequest, told: &mut Told<'_>) {
        let session = self.id;
        let incoming = IncomingModelRequest {
            request,
            session,
            number,
        };
        self.arrived(Arrival::ModelRequest(incoming), told);
    }
}

fn payload(
    number: u32,
    call: &OpCall,
    size: u64,
    limit: OpPayloadLimit,
) -> Result<Payload, String> {
    if size > MAX_OP_PAYLOAD_BYTES {
        return Err(broke(&format!(
            "it opened op {number} with a payload of {size} bytes, more than the protocol's \
             {MAX_OP_PAYLOAD_BYTES}"
        )));
    }
    if size > 0 && !call.takes_payload() {
        return Err(broke(&format!(
            "it opened op {number} with a payload of {size} bytes, for a call that takes none"
        )));
    }
    if size > limit.bytes() {
        return Ok(Payload::Dropped { size, limit });
    }
    let Ok(capacity) = usize::try_from(size) else {
        unreachable!(
            "a payload of {size} bytes overflows a usize, though it is at most the limit, which \
             is at most `MAX_OP_PAYLOAD_BYTES`, 64 MiB (an `OpPayloadLimit` holds no more), which \
             fits the usize of every Linux target, of 32 bits or more"
        )
    };
    Ok(Payload::Kept(Vec::with_capacity(capacity)))
}
