//! The start of a run's children, each on a thread of its own: a call, and a lease of a runner.

use std::collections::BTreeMap;
use std::fs;
use std::sync::mpsc::Sender;
use std::thread::{self, Scope};
use std::time::Duration;

use decision::{Caller, Core, JobId, LeaseId, PayloadRef};
use schema::ir::{ArgumentValue, Name};
use schema::sandbox::op::OpReply;
use schema::workflow::{JobFunctionId, RoleId, SessionId};
use stdx::Context;

use crate::child::{Child, Deadlines, Message, Report};
use crate::sandbox::Which;
use crate::{Called, Clock, Host, Inbox, Input, Runtime, call, judge, lease};

impl<'r, H: Host + 'r> Runtime<'r, H> {
    pub(crate) fn start_judge<'s>(
        &mut self,
        scope: &'s Scope<'s, 'r>,
        job: JobId,
        payload: &PayloadRef,
        timeout: Duration,
    ) -> Result<(), String> {
        let slot = self.workflow().score().slot;
        let role = slot.judge().role();
        // The final judge's command starts in time to run its whole timeout before the hard end,
        // by which what it left is collected.
        let start = self.hard_end().saturating_sub(timeout);
        let startup = (self.clock.now())
            .saturating_add(self.placed.startup(role))
            .min(start);
        let deadlines = Deadlines {
            startup,
            collect: self.hard_end(),
        };
        let call = judge::Call {
            job,
            slot,
            payload: self.payloads.stored(payload),
            start,
            timeout,
        };
        let run = move |mut child: Child<'r, H, OpReply>| judge::run(&call, &mut child);
        let which = Which::Job(job, slot.judge());
        let orders = self.start_child(scope, which, deadlines, None, run)?;
        self.called(job, orders, deadlines, role);
        Ok(())
    }

    pub(crate) fn start_call<'s>(
        &mut self,
        scope: &'s Scope<'s, 'r>,
        core: &Core<'_>,
        job: JobId,
        function: JobFunctionId,
        (payload, arguments): (&PayloadRef, BTreeMap<Name, ArgumentValue>),
        timeout: Duration,
    ) -> Result<(), String> {
        let function = self.workflow().job_function(function);
        let role = function.target.role();
        // A call ends with the work phase, which ends by the core's next act of its own, and what
        // it left is collected by the agent's teardown's end, which leaves the final judge its
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
            function,
            payload: self.payloads.stored(payload),
            arguments,
            timeout,
        };
        let bodies = self.bodies;
        let run = move |mut child: Child<'r, H, OpReply>| {
            call::job::run(call, bodies, &mut child);
        };
        let stack = Some(bodies.stack_bytes());
        let which = Which::Call(job, function.target);
        let orders = self.start_child(scope, which, deadlines, stack, run)?;
        self.called(job, orders, deadlines, role);
        Ok(())
    }

    /// Keeps job `job`'s call, started on `role`'s sandbox with `orders` into its inbox.
    fn called(
        &mut self,
        job: JobId,
        orders: Sender<Message<OpReply>>,
        deadlines: Deadlines,
        role: RoleId,
    ) {
        let called = Called {
            orders,
            collect: deadlines.collect,
            trees: None,
            role,
        };
        self.jobs.insert(job, called);
    }

    pub(crate) fn start_lease<'s>(
        &mut self,
        scope: &'s Scope<'s, 'r>,
        core: &Core<'_>,
        (lease, instance, start_by): (LeaseId, u32, Duration),
        (holder, grant): (Caller, SessionId),
    ) -> Result<(), String> {
        let runner_id = self.workflow().session(grant).runner;
        let runner = self.workflow().runner(runner_id);
        let (start, collect) = match holder {
            // A lease of the agent's ends with the work phase, which ends by the core's next end of
            // a phase, and what it left is collected by the agent's teardown's end, as a judge
            // call's is.
            Caller::Agent => (core.phase_due(), self.teardown_end()),
            // A call's ends with the call's command, and what it left is collected by when the
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
        let dir = self.dir.join(which.record_dir());
        let parent = dir.parent().unwrap_or(self.dir);
        fs::create_dir_all(parent).context(parent.display())?;
        fs::create_dir(&dir).context(dir.display())?;
        let inbox = self.clock.inbox();
        let sender = inbox.sender();
        let child = Child {
            which,
            deadlines,
            hard_end: self.unix_hard_end,
            dir,
            sandboxes: self.sandboxes,
            clock: self.clock.clone(),
            inbox,
            reports: self.inbox.sender(),
        };
        let mut builder = thread::Builder::new().name(which.name().to_string());
        if let Some(bytes) = stack_bytes {
            builder = builder.stack_size(bytes);
        }
        let spawned = builder.spawn_scoped(scope, move || run(child));
        if let Err(error) = spawned {
            // Reported as the child's thread would have, behind what reached the host before.
            let reason = format!("{} thread didn't start: {error}", which.noun());
            let failed = Report::Event(self.clock.now(), which.failed(reason));
            // The inbox holds its receiver, so the send can't fail.
            let _ = self.inbox.sender().send(Input::Child(failed));
        }
        Ok(sender)
    }
}
