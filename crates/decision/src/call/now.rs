//! Calls of a now function: its body's effects on the run's state, checked and applied in the
//! call's own step, as [`NowFunction`](schema::ir::NowFunction) says.

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::time::Duration;

use schema::ir::Name;
use schema::sandbox::op::{CallReply, NowReply, OpReply};
use schema::workflow::AgentId;

use crate::jobs::BODY_FAILED;
use crate::ops::{Held, Working, fits};
use crate::{Action, Core, Effect, Evaluated, PayloadRef, Unevaluated, WorkEnd};

impl Core<'_> {
    /// The reply to the call of the now function `function` with `payload`, by `working`, whose
    /// body gave `evaluated`, and the actions it takes.
    pub(crate) fn now(
        &mut self,
        working: &Working,
        function: &Name,
        payload: &PayloadRef,
        evaluated: Result<Evaluated, Unevaluated>,
        then: &mut Vec<Action>,
    ) -> Result<OpReply, String> {
        let Held::Now(id) = self.held(working, function)? else {
            unreachable!(
                "the runtime sends a now call only for a now function of the caller, and grant \
                 names are unique for each role"
            )
        };
        let grant = self.workflow.now_function(id);
        fits(function, payload, grant.payload_max_bytes)?;
        let Evaluated { effects, returned } =
            evaluated.map_err(|unevaluated| match unevaluated {
                Unevaluated::Unread(words) => words,
                Unevaluated::Failed(_) => BODY_FAILED.to_owned(),
            })?;
        let end = self.apply(grant.holder, &effects, payload, working.end(), then)?;
        Ok(OpReply::Call(CallReply::Now(NowReply {
            returned: returned.returned(),
            remaining_seconds: self.seconds_left(end),
        })))
    }

    /// Applies the call's effects together and gives the earlier of its deadline and grace end.
    ///
    /// # Errors
    ///
    /// The first effect refused, with every effect unapplied.
    fn apply(
        &mut self,
        agent: AgentId,
        effects: &[Effect],
        payload: &PayloadRef,
        deadline: Duration,
        then: &mut Vec<Action>,
    ) -> Result<Duration, String> {
        let mut select = false;
        let mut end_work = false;
        for effect in effects {
            if end_work {
                return Err(
                    "`run.end_work()` ended the work, and the call has more effects after \
                     it: end the work last"
                        .to_owned(),
                );
            }
            match effect {
                Effect::Select => {
                    self.selectable(payload)?;
                    select = true;
                }
                Effect::EndWork => end_work = true,
            }
        }
        let Some(work) = self.agents.get_mut(&agent) else {
            unreachable!("a now function's holder is one of the workflow's agent roles")
        };
        if select {
            work.selection = Some(payload.clone());
        }
        if !end_work {
            return Ok(deadline);
        }
        let grace = self.workflow.agent_by_id(agent).finalize_grace;
        let end = self.now.saturating_add(grace).min(deadline);
        work.grace_end = Some(end);
        if grace.is_zero() {
            self.end_work(agent, WorkEnd::Finalized, then);
        }
        Ok(end)
    }

    // Refuses a payload that the final call can't take, so that whatever is selected fits it.
    fn selectable(&self, payload: &PayloadRef) -> Result<(), String> {
        let max_bytes = self.workflow.final_call().payload_max_bytes();
        if payload.size <= max_bytes.get() {
            Ok(())
        } else {
            Err(format!(
                "the payload is {} bytes, over the final call's limit of {max_bytes}: send a \
                 smaller one",
                payload.size
            ))
        }
    }
}
