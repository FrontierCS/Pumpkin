//! The start of a run's children, each on a thread of its own: a judge call, and a lease of a
//! runner.

use std::fs;
use std::sync::mpsc::Sender;
use std::thread::{self, Scope};
use std::time::Duration;

use decision::{Caller, Core, JobId, LeaseId, PayloadRef};
use schema::sandbox::op::OpReply;
use schema::workflow::{JudgeCall, LeaseGrantId};
use stdx::Context;

use crate::child::{Child, Deadlines, Message, Report};
use crate::sandbox::Which;
use crate::{Called, Clock, Host, Inbox, Input, Runtime, judge, lease};

impl<'r, H: Host + 'r> Runtime<'r, H> {
    pub(crate) fn start_judge<'s>(
        &mut self,
        scope: &'s Scope<'s, 'r>,
        core: &Core<'_>,
        job: JobId,
        call: JudgeCall,
        payload: &PayloadRef,
        timeout: Duration,
    ) -> Result<(), String> {
        let deadlines = self.judge_deadlines(core, call, timeout);
        let slot = self.workflow().call_slot(call);
        let call = judge::Call {
            job,
            slot,
            payload: self.payloads.stored(payload),
            timeout,
        };
        let which = Which::Job(job, slot.judge());
        let run = move |mut child: Child<'r, H, OpReply>| judge::run(&call, &mut child);
        let orders = self.start_child(scope, which, deadlines, run)?;
        let collect = deadlines.collect;
        let called = Called {
            orders,
            collect,
            trees: None,
            judge: slot.judge().role(),
        };
        self.jobs.insert(job, called);
        Ok(())
    }

    pub(crate) fn start_lease<'s>(
        &mut self,
        scope: &'s Scope<'s, 'r>,
        core: &Core<'_>,
        (lease, instance, start_by): (LeaseId, u32, Duration),
        (holder, grant): (Caller, LeaseGrantId),
    ) -> Result<(), String> {
        let runner_id = self.workflow().lease_grant(grant).runner;
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
        let deadlines = Deadlines {
            startup,
            start,
            collect,
        };
        let which = Which::Lease(lease, runner_id, instance);
        let run =
            move |mut child: Child<'r, H, lease::Order>| lease::run(lease, runner, &mut child);
        let orders = self.start_child(scope, which, deadlines, run)?;
        self.leases.insert(lease, orders);
        Ok(())
    }

    fn start_child<'s, O: Send + 'static>(
        &mut self,
        scope: &'s Scope<'s, 'r>,
        which: Which,
        deadlines: Deadlines,
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
        let spawned = thread::Builder::new()
            .name(which.name().to_string())
            .spawn_scoped(scope, move || run(child));
        if let Err(error) = spawned {
            // Reported as the child's thread would have, behind what reached the host before.
            let reason = format!("{} thread didn't start: {error}", which.noun());
            let failed = Report::Event(self.clock.now(), which.failed(reason));
            // The inbox holds its receiver, so the send can't fail.
            let _ = self.inbox.sender().send(Input::Child(failed));
        }
        Ok(sender)
    }

    fn judge_deadlines(&self, core: &Core<'_>, call: JudgeCall, timeout: Duration) -> Deadlines {
        let judge = self.workflow().call_slot(call).judge().role();
        let startup = self.clock.now().saturating_add(self.placed.startup(judge));
        match call {
            // A call of the agent's ends with the work phase, which ends by the core's next act of
            // its own, and what it left is collected by the agent's teardown's end, which leaves
            // the final judge its share of the hard end.
            JudgeCall::Grant(_) => {
                let start = startup.min(core.phase_due());
                Deadlines {
                    startup: start,
                    start,
                    collect: self.teardown_end(),
                }
            }
            // The final judge's command starts in time to run its whole timeout before the hard
            // end, by which what it left is collected.
            JudgeCall::Score => {
                let start = self.hard_end().saturating_sub(timeout);
                Deadlines {
                    startup: startup.min(start),
                    start,
                    collect: self.hard_end(),
                }
            }
        }
    }
}
