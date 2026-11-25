//! The start of a run's children, each on a thread of its own: a call of a body, and a lease of a
//! runner.

use std::collections::BTreeMap;
use std::fs;
use std::sync::mpsc::Sender;
use std::thread::{self, Scope};
use std::time::Duration;

use decision::{Caller, Core, JobId, LeaseId, PayloadRef};
use schema::ir::{ArgumentValue, Name};
use schema::workflow::{AgentId, JobFunctionId, SessionId};
use stdx::Context;
use stdx::time::Clock as _;

use crate::child::{Child, Deadlines, End, Message, Report};
use crate::sandbox::Which;
use crate::{Called, Clock, Host, Inbox, Input, Runtime, body, call, lease};

impl<'r, H: Host + 'r> Runtime<'r, H> {
    pub(crate) fn start_call<'s>(
        &mut self,
        scope: &'s Scope<'s, 'r>,
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
        let startup = (self.clock.now())
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
        self.start_body(scope, call, deadlines, instance)
    }

    pub(crate) fn start_final_call<'s>(
        &mut self,
        scope: &'s Scope<'s, 'r>,
        job: JobId,
        selections: Vec<(AgentId, Option<PayloadRef>)>,
        timeout: Duration,
    ) -> Result<(), crate::Stopped> {
        let swept = self.sandboxes.finish_work();
        crate::interruption::check()?;
        swept.context("cleaning up work sandboxes before the final call")?;
        let final_call = self.workflow().final_call();
        let role = final_call.target.role();
        // The final call's body starts in time to run its whole timeout before the hard end, by
        // which what it left is collected.
        let start = self.hard_end().saturating_sub(timeout);
        let startup = (self.clock.now())
            .saturating_add(self.placed.startup(role))
            .min(start);
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
        Ok(self.start_body(scope, call, deadlines, Some(0))?)
    }

    /// Runs a call on its reserved fresh instance or the work phase's shared instance.
    fn start_body<'s>(
        &mut self,
        scope: &'s Scope<'s, 'r>,
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
        let orders = if instance.is_some() {
            let bodies = self.bodies;
            self.start_child(
                scope,
                which,
                deadlines,
                Some(bodies.stack_bytes()),
                move |mut child| {
                    call::job::run(call, bodies, &mut child);
                },
            )?
        } else {
            let child = self.child(which, deadlines)?;
            let orders = child.inbox.sender();
            self.residents
                .get_mut(&target)
                .ok_or_else(|| "the shared role has no worker".to_owned())?
                .call(call, child)?;
            orders
        };
        let called = Called {
            orders,
            collect: deadlines.collect,
            trees: None,
            target,
            instance: instance.unwrap_or(0),
            owner,
            model_requests_over_budget: 0,
        };
        self.jobs.insert(job, called);
        Ok(())
    }

    pub(crate) fn start_lease<'s>(
        &mut self,
        scope: &'s Scope<'s, 'r>,
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
                let collect = self.call(job).collect;
                (collect, collect)
            }
        };
        // A sandbox's start ends by when the lease must start and by the core's `start_by`.
        let startup = (self.clock.now())
            .saturating_add(self.placed.startup(runner_id.role()))
            .min(start)
            .min(start_by);
        let deadlines = Deadlines { startup, collect };
        let which = Which::Lease(lease, runner_id, instance);
        let leased = lease::Leased {
            lease,
            runner,
            start,
        };
        let run = move |mut child: Child<'r, H, lease::Order>| lease::run(&leased, &mut child);
        let orders = self.start_child(scope, which, deadlines, None, run)?;
        self.leases.insert(lease, orders);
        Ok(())
    }

    fn start_child<'s, O: Send + 'static>(
        &mut self,
        scope: &'s Scope<'s, 'r>,
        which: Which,
        deadlines: Deadlines,
        stack_bytes: Option<usize>,
        run: impl FnOnce(Child<'r, H, O>) + Send + 's,
    ) -> Result<Sender<Message<O>>, String> {
        let child = self.child(which, deadlines)?;
        let sender = child.inbox.sender();
        let mut builder = thread::Builder::new().name(which.name().to_string());
        if let Some(bytes) = stack_bytes {
            builder = builder.stack_size(bytes);
        }
        let spawned = builder.spawn_scoped(scope, move || run(child));
        if let Err(error) = spawned {
            // Reported as the child's thread would have, behind what reached the host before.
            let reason = format!("{} thread didn't start: {error}", which.noun());
            let at = self.clock.now();
            let (before, after) = End::failed(which, reason).reports(at);
            for report in before.into_iter().chain([Report::Event(at, after)]) {
                // The inbox holds its receiver, so the send can't fail.
                let _ = self.inbox.sender().send(Input::Child(report));
            }
        }
        Ok(sender)
    }

    fn child<O: Send + 'static>(
        &self,
        which: Which,
        deadlines: Deadlines,
    ) -> Result<Child<'r, H, O>, String> {
        let dir = self.dir.join(which.record_dir(self.workflow()));
        let parent = dir.parent().unwrap_or(self.dir);
        fs::create_dir_all(parent).context(parent.display())?;
        fs::create_dir(&dir).context(dir.display())?;
        let inbox = self.clock.inbox();
        Ok(Child {
            which,
            deadlines,
            hard_end: self.unix_at(deadlines.collect),
            dir,
            sandboxes: self.sandboxes,
            clock: self.clock.clone(),
            inbox,
            reports: self.inbox.sender(),
        })
    }
}
