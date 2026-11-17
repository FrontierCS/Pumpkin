//! Calls of a now function: its body's effects on the run's state, checked and applied in the
//! call's own step, as [`NowFunction`](schema::ir::NowFunction) says.

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::time::Duration;

use schema::ir::Name;
use schema::sandbox::op::{CallReply, NowReply, OpReply};
use schema::workflow::NowFunctionId;

use crate::jobs::BODY_FAILED;
use crate::ops::{Held, Working, fits};
use crate::{Action, Core, Effect, Evaluated, PayloadRef, Phase, Unevaluated, WorkEnd};

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
        let (id, deadline, latest) = self.now_function(working, function)?;
        fits(
            function,
            payload,
            self.workflow.now_function(id).payload_max_bytes,
        )?;
        let Evaluated { effects, returned } =
            evaluated.map_err(|unevaluated| match unevaluated {
                Unevaluated::Unread(words) => words,
                Unevaluated::Failed(_) => BODY_FAILED.to_owned(),
            })?;
        let end = self.apply(&effects, payload, latest, deadline, then)?;
        Ok(OpReply::Call(CallReply::Now(NowReply {
            returned: returned.returned().clone(),
            remaining_seconds: self.seconds_left(end),
        })))
    }

    /// The now function named `function` that `working` holds, its work's deadline and the
    /// selection so far.
    fn now_function(
        &self,
        working: &Working,
        function: &Name,
    ) -> Result<(NowFunctionId, Duration, Option<PayloadRef>), String> {
        match self.held(working, function)? {
            Held::Now(id, deadline, latest) => Ok((id, deadline, latest)),
            Held::Job(..) | Held::Session(_) => unreachable!(
                "`{function}`'s call came as a now function's, though the runtime makes only a \
                 call that names a now function so, and names are unique among a role's grants"
            ),
        }
    }

    /// Applies `effects`, a call's with `payload` in the work phase whose selection is `latest`,
    /// each checked against the state that those before it leave, and gives when the caller's work
    /// ends: its deadline, or, once an effect ends the work, the end of its grace, if that comes
    /// first.
    ///
    /// # Errors
    ///
    /// The first effect refused, with every effect unapplied.
    fn apply(
        &mut self,
        effects: &[Effect],
        payload: &PayloadRef,
        mut latest: Option<PayloadRef>,
        deadline: Duration,
        then: &mut Vec<Action>,
    ) -> Result<Duration, String> {
        let mut locked = None;
        for effect in effects {
            if locked.is_some() {
                return Err(
                    "`run.end_work()` ended the work, and the call has more effects after \
                            it: end the work last"
                        .to_owned(),
                );
            }
            match effect {
                Effect::Select => {
                    self.selectable(payload)?;
                    latest = Some(payload.clone());
                }
                Effect::EndWork => {
                    let selected = latest.clone().ok_or_else(|| {
                        "`run.end_work()` needs a selection, and nothing is selected: call \
                         `run.select(payload)` first"
                            .to_owned()
                    })?;
                    locked = Some(selected);
                }
            }
        }
        // Ops are taken only while the agent works (`working`): the phase goes on with the
        // selection, or ending the work locks it.
        let Some(selected) = locked else {
            self.phase = Phase::Work { latest, deadline };
            return Ok(deadline);
        };
        let grace = self.workflow.agent().finalize_grace;
        let grace_end = self.now.saturating_add(grace);
        // A tie goes to the deadline, which names the end.
        let (end, work_end) = if grace_end < deadline {
            (grace_end, WorkEnd::Finalized)
        } else {
            (deadline, WorkEnd::Deadline)
        };
        self.phase = Phase::Finalized {
            payload: selected,
            end,
            work_end,
        };
        if grace.is_zero() {
            self.end_work(WorkEnd::Finalized, then);
        }
        Ok(end)
    }

    // Refuses a payload that the final call can't take, so that whatever is selected fits it.
    fn selectable(&self, payload: &PayloadRef) -> Result<(), String> {
        let max_bytes = self.workflow.slot(self.workflow.score().slot).max_bytes();
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
