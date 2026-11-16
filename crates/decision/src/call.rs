//! Calls of a function that the caller's role holds, by the function's kind, each kind's in a
//! module of its own below this one.

mod job;
mod now;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use schema::ir::Name;
use schema::sandbox::op::OpReply;

use crate::ops::{Held, Working};
use crate::{Action, Core, PayloadRef};

impl Core<'_> {
    /// The reply to the call of `function` with `payload` and its caller's `arguments`, as text, by
    /// `working`, and the actions it takes.
    pub(super) fn call(
        &mut self,
        working: &Working,
        function: &Name,
        payload: PayloadRef,
        arguments: &BTreeMap<Name, String>,
        then: &mut Vec<Action>,
    ) -> Result<OpReply, String> {
        match self.held(working, function)? {
            Held::Job(id, deadline) => self.job(id, payload, arguments, deadline, then),
            held @ Held::Session(_) => Err(held.misnamed(function, "a function")),
            Held::Now(..) => unreachable!(
                "`{function}`'s call came as a call of no now function, though the runtime makes \
                 every call of the agent's that names one a now function's, and only the agent \
                 holds one"
            ),
        }
    }
}
