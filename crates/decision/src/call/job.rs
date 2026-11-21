//! A job function's call: a job, which runs the function's body on its target, and which the
//! caller follows with `status`.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::time::Duration;

use schema::ir::{Name, argument};
use schema::sandbox::op::{CallReply, JobReply, OpReply, Remaining};
use schema::workflow::{Holder, JobFunctionId, JobOf, TargetId};

use crate::ops::fits;
use crate::{Action, Caller, Core, PayloadRef};

impl Core<'_> {
    /// The reply to a call of job function `id` with `payload` and its caller's `arguments`, as
    /// text, by the agent, whose work ends at `deadline`: a job, if the function's limits let it
    /// start.
    pub(super) fn job(
        &mut self,
        id: JobFunctionId,
        payload: PayloadRef,
        arguments: &BTreeMap<Name, String>,
        deadline: Duration,
        then: &mut Vec<Action>,
    ) -> Result<OpReply, String> {
        let function = self.workflow.job_function(id);
        let name = &function.name;
        fits(name, &payload, function.payload_max_bytes())?;
        let arguments = argument::read(name, &function.body.arguments, arguments)?;
        let usage = self.jobs.usage(JobOf::Function(id));
        if let Some(calls) = function.calls
            && usage.calls >= calls.get()
        {
            return Err(format!(
                "`{name}` has no calls left of the {calls} it allows: make no more calls to it"
            ));
        }
        let cumulative = function.cumulative();
        if let Some(total) = cumulative
            && usage.used >= total
        {
            return Err(format!(
                "`{name}` has no call time left of the {} s it allows: make no more calls to it",
                total.as_secs()
            ));
        }
        if let Some(service) = self.shared.get(&function.target) {
            if *service != crate::Availability::Available {
                return Err(format!(
                    "`{name}` cannot run because its shared sandbox is unavailable"
                ));
            }
            if self.work_target_busy(function.target) {
                return Ok(OpReply::Refused {
                    busy: true,
                    message: format!(
                        "`{name}`'s shared sandbox is busy: retry after its current call ends; this call is not charged"
                    ),
                });
            }
        } else if usage.busy
            || self
                .jobs
                .of(JobOf::Function(id))
                .any(|job| self.leases.holds(Caller::Job(job)))
        {
            return Ok(OpReply::Refused {
                busy: true,
                message: format!(
                    "`{name}` runs one call at a time, and already has one queued, running or \
                 releasing its sandbox: call again once it has finished; this call is not charged"
                ),
            });
        }
        // Ending an agent's work stops its calls (`end_work`), so a call is given at most the time
        // its caller has left when it is accepted; after a wait in the queue, its timeout can
        // reach past the deadline, where that stop ends it all the same. Each term is positive: the
        // work phase ends at the deadline, and every limit is left above zero.
        let work_left = deadline.saturating_sub(self.now);
        let call_left = cumulative.map_or(Duration::MAX, |total| total.saturating_sub(usage.used));
        let timeout = function.call_timeout().min(call_left).min(work_left);
        let job = self.jobs.add(JobOf::Function(id), timeout);
        then.push(Action::StartCall {
            job,
            function: id,
            payload,
            arguments,
            timeout,
        });
        let remaining = self.remaining(id);
        Ok(OpReply::Call(CallReply::Job(JobReply {
            job: job.0,
            remaining,
        })))
    }

    fn work_target_busy(&self, target: TargetId) -> bool {
        // A job can report its end while its runner still occupies this target's slot.
        self.jobs.active_on(self.workflow, target).is_some()
            || self
                .leases
                .active()
                .any(|lease| self.workflow.session(lease.grant).holder == Holder::Target(target))
    }

    /// What job function `id` has left of its quotas.
    pub(crate) fn remaining(&self, id: JobFunctionId) -> Remaining {
        let function = self.workflow.job_function(id);
        let usage = self.jobs.usage(JobOf::Function(id));
        Remaining {
            calls: (function.calls).map(|calls| calls.get().saturating_sub(usage.calls)),
            seconds: (function.cumulative())
                .map(|total| total.saturating_sub(usage.used).as_secs()),
        }
    }
}
