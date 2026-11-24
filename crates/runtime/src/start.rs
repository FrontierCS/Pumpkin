//! The start of a run's children: a call of a body, and a lease of a runner.

use std::collections::BTreeMap;
use std::time::Duration;

use decision::{Caller, Core, JobId, LeaseId, PayloadRef};
use schema::ir::{ArgumentValue, Name};
use schema::sandbox::OpPayloadLimit;
use schema::workflow::{AgentId, Holder, JobFunctionId, SessionId};
use stdx::Context;

use crate::call::job::Called;
use crate::child::Deadlines;
use crate::sandbox::Which;
use crate::{Host, Io, Runtime, body, call};

impl<'r, 's, H: Host + 'r> Runtime<'r, 's, H> {
    pub(crate) fn start_call(
        &mut self,
        io: &mut Io<'r, 's, H>,
        core: &Core<'_>,
        job: JobId,
        function: JobFunctionId,
        (payload, arguments): (&PayloadRef, BTreeMap<Name, ArgumentValue>),
        timeout: Duration,
    ) -> Result<(), String> {
        let instance = self.sandboxes.capacity.functions.get(&function).copied();
        let function = self.workflow().job_function(function);
        let role = function.target.role();
        // A call ends with the work phase, which ends by the core's next act of its own, and what
        // it left is collected by the agent's teardown's end, which leaves the final call its
        // share of the hard end.
        let startup = self
            .at
            .saturating_add(self.placed.startup(role))
            .min(core.phase_due());
        let deadlines = Deadlines {
            startup,
            collect: self.teardown_end(),
        };
        let call = call::job::Call {
            job,
            of: body::Of::Function {
                function,
                arguments,
            },
            inputs: vec![self.payloads.stored(payload)],
            timeout,
        };
        self.start_body(io, call, deadlines, instance)
    }

    pub(crate) fn start_final_call(
        &mut self,
        io: &mut crate::Io<'r, 's, H>,
        job: JobId,
        selections: Vec<(AgentId, Option<PayloadRef>)>,
        timeout: Duration,
    ) -> Result<(), crate::Stopped> {
        let swept = self.finish_work(io);
        crate::interruption::check()?;
        swept.context("cleaning up work sandboxes before the final call")?;
        let final_call = self.workflow().final_call();
        let role = final_call.target.role();
        // The final call's body starts in time to run its whole timeout before the hard end, by
        // which what it left is collected.
        let start = self.hard_end().saturating_sub(timeout);
        let startup = self.at.saturating_add(self.placed.startup(role)).min(start);
        let deadlines = Deadlines {
            startup,
            collect: self.hard_end(),
        };
        let mut inputs = Vec::new();
        let submissions = selections
            .into_iter()
            .map(|(agent, selected)| {
                let role = self.workflow().sandbox(agent.role()).name.clone();
                let slot = selected.map(|payload| {
                    let slot = inputs.len();
                    inputs.push(self.payloads.stored(&payload));
                    slot
                });
                (role, slot)
            })
            .collect();
        let call = call::job::Call {
            job,
            of: body::Of::Final {
                final_call,
                submissions,
            },
            inputs,
            timeout,
        };
        Ok(self.start_body(io, call, deadlines, Some(0))?)
    }

    /// Runs a call on its reserved fresh instance or the work phase's shared instance.
    fn start_body(
        &mut self,
        io: &mut Io<'r, 's, H>,
        call: call::job::Call<'r>,
        deadlines: Deadlines,
        instance: Option<u32>,
    ) -> Result<(), String> {
        let (job, target) = (call.job, call.of.target());
        let owner = match &call.of {
            body::Of::Function { function, .. } => Some(function.holder),
            body::Of::Final { .. } => None,
        };
        let which = Which::Call(job, target, instance.unwrap_or(0));
        let child = self.child(which, deadlines)?;
        let called = Called::new(child, call, (instance.unwrap_or(0), owner));
        self.jobs.insert(job, called);
        if instance.is_none() {
            self.resident_call(io, target, job);
            return Ok(());
        }
        // A target's ops carry no payload: it holds sessions alone.
        let holder = Holder::Target(target);
        let ops = (self.workflow().sessions())
            .any(|(_, grant)| grant.holder == holder)
            .then_some(OpPayloadLimit::NONE);
        self.start_child(io, which, (deadlines.startup, ops));
        Ok(())
    }

    pub(crate) fn start_lease(
        &mut self,
        io: &mut Io<'r, 's, H>,
        core: &Core<'_>,
        (lease, instance, start_by): (LeaseId, u32, Duration),
        (holder, grant): (Caller, SessionId),
    ) -> Result<(), String> {
        let session = self.workflow().session(grant);
        let runner_id = session.runner;
        let holder_instance = match holder {
            Caller::Agent(_) => 0,
            Caller::Job(job) => self.call(job).instance,
        };
        // Capacity reserved this complete range, including every slot of every holder instance.
        let instance = self.sandboxes.capacity.sessions[&grant]
            + holder_instance * session.instances.get()
            + instance;
        let runner = self.workflow().runner(runner_id);
        let (start, collect) = match holder {
            // A lease of the agent's ends with the work phase, which ends by the core's next end of
            // a phase, and what it left is collected by the agent's teardown's end, as a call's
            // is.
            Caller::Agent(_) => (core.phase_due(), self.teardown_end()),
            // A call's ends with the call's body, and what it left is collected by when the
            // call's is.
            Caller::Job(job) => {
                let collect = self.call(job).child.deadlines.collect;
                (collect, collect)
            }
        };
        // A sandbox's start ends by when the lease must start and by the core's `start_by`.
        let startup = self
            .at
            .saturating_add(self.placed.startup(runner_id.role()))
            .min(start)
            .min(start_by);
        let deadlines = Deadlines { startup, collect };
        let which = Which::Lease(lease, runner_id, instance);
        let child = self.child(which, deadlines)?;
        self.open_lease(io, lease, (child, runner, start));
        Ok(())
    }
}
