//! Calls of a function that the caller's role holds, by the function's kind, each kind's in a
//! module of its own below this one.

mod job;

use alloc::string::String;
use alloc::vec::Vec;

use schema::ir::Name;
use schema::sandbox::op::OpReply;

use crate::ops::{Held, Working};
use crate::{Action, Core, PayloadRef};

impl Core<'_> {
    /// The reply to the call of `function` with `payload`, by `working`, and the actions it takes.
    pub(super) fn call(
        &mut self,
        working: Working,
        function: &Name,
        payload: PayloadRef,
        then: &mut Vec<Action>,
    ) -> Result<OpReply, String> {
        match self.held(working, function)? {
            Held::Job(id, deadline) => self.job(id, payload, deadline, then),
            held @ (Held::Session(_) | Held::Submit(_)) => {
                Err(held.misnamed(function, "a function"))
            }
        }
    }
}
