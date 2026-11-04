//! The agent's sandbox: it makes ops through `sandboxctl`, has no network, and runs the agent's
//! IR command.

use std::io;
use std::path::Path;
use std::time::Duration;

use pumpkin_docker::Container;
use pumpkin_gateway::{Arrival, IncomingOp, Process, RefusedOp};
use pumpkin_schema::sandbox::{OpPayloadLimit, log_dir, out_dir};
use pumpkin_schema::workflow::{Main, Workflow};
use stdx::Context;

use super::sandbox::{PUMPKIN, Sandboxes, collect, launch};
use crate::driver::core::Event;

/// The agent's sandbox, from when it starts until it is destroyed.
#[derive(Debug)]
pub(super) struct Agent {
    container: Container,
    /// The agent's command, until its end has been reported; `None` if it didn't start.
    process: Option<Process>,
    /// Whether ops may still arrive: until the session ends or the sandbox is frozen.
    taking_ops: bool,
}

/// What the agent did while it was watched.
#[derive(Debug)]
pub(super) enum Seen {
    Op(IncomingOp),
    /// An op whose payload was over the agent's limit, which the gateway refused and answered
    /// itself: the run log records it, and the core never sees it.
    Refused(RefusedOp),
    Event(Event),
    Nothing,
}

impl Agent {
    /// Starts the agent's sandbox and command. Returns the sandbox, if it started, and what
    /// came of it: [`Event::AgentStarted`] or [`Event::AgentFailed`].
    pub(super) fn start(sandboxes: &Sandboxes, workflow: &Workflow) -> (Option<Self>, Event) {
        let agent = workflow.agent();
        let ops = Some(op_payload_limit(workflow));
        let mut container = match sandboxes.start("agent", &agent.sandbox, ops) {
            Ok(container) => container,
            Err(error) => {
                let reason = format!("the agent's sandbox didn't start: {error}");
                return (None, Event::AgentFailed { reason });
            }
        };
        let session = container.session();
        let started =
            (session.start(launch(&agent.command))).context("the agent's command didn't start");
        let (process, event) = match started {
            Ok(process) => (Some(process), Event::AgentStarted),
            Err(reason) => (None, Event::AgentFailed { reason }),
        };
        let agent = Self {
            container,
            process,
            taking_ops: true,
        };
        (Some(agent), event)
    }

    /// Whether the agent may still make ops.
    pub(super) fn taking_ops(&self) -> bool {
        self.taking_ops
    }

    /// Takes an op that has arrived, then looks for the agent's exit, then waits up to
    /// `timeout` for its next op. Ops that arrived before the exit come first, so the core sees
    /// them in the work phase. A session that ends is the agent's failure, unless its end was
    /// reported already.
    pub(super) fn watch(&mut self, timeout: Duration) -> Seen {
        if let Some(seen) = self.next_op(Duration::ZERO) {
            return seen;
        }
        if let Some(process) = &mut self.process {
            match process.wait(Duration::ZERO) {
                Ok(None) => {}
                Ok(Some(status)) => {
                    self.process = None;
                    return Seen::Event(Event::AgentExited { status });
                }
                Err(error) => {
                    self.process = None;
                    self.taking_ops = false;
                    let reason = format!("the agent's session ended: {error}");
                    return Seen::Event(Event::AgentFailed { reason });
                }
            }
        }
        self.next_op(timeout).unwrap_or(Seen::Nothing)
    }

    /// Waits up to `timeout` for the next op: `None` if none arrived, or if the session ended
    /// after the agent's end was reported.
    fn next_op(&mut self, timeout: Duration) -> Option<Seen> {
        match self.container.session().next_op(timeout) {
            Ok(arrival) => arrival.map(|arrival| match arrival {
                Arrival::Op(op) => Seen::Op(op),
                Arrival::Refused(refused) => Seen::Refused(refused),
            }),
            Err(error) => {
                self.taking_ops = false;
                self.process.take().map(|_| {
                    let reason = format!("the agent's session ended: {error}");
                    Seen::Event(Event::AgentFailed { reason })
                })
            }
        }
    }

    /// Answers an op the agent made.
    pub(super) fn reply(
        &mut self,
        op: IncomingOp,
        reply: pumpkin_schema::sandbox::op::OpReply,
    ) -> io::Result<()> {
        self.container.session().reply(op, reply)
    }

    /// Freezes the sandbox, then collects its [`out_dir()`] and [`log_dir()`] into `dir`.
    /// Returns what failed, if anything.
    pub(super) fn freeze_and_collect(&mut self, dir: &Path) -> Vec<String> {
        self.taking_ops = false;
        let session = self.container.session();
        let mut failures = Vec::new();
        if let Err(error) = session.freeze() {
            failures.push(format!("freezing the agent's sandbox failed: {error}"));
        }
        for part in [out_dir(), log_dir()] {
            if let Err(error) = collect(session, &part, dir, PUMPKIN) {
                failures.push(format!("the agent's {part} is incomplete: {error}"));
            }
        }
        failures
    }

    /// Destroys the sandbox.
    pub(super) fn destroy(self) -> Result<(), String> {
        self.container.destroy()
    }
}

/// The largest payload the agent may send: the largest `max_bytes` of its judge grants'
/// slots and of the score's slot, which caps its submissions.
fn op_payload_limit(workflow: &Workflow) -> OpPayloadLimit {
    let Main::SoloThenJudge(pattern) = workflow.main();
    let slots = workflow.judge_grants().map(|(_, grant)| grant.slot);
    let max_bytes = |slot| workflow.slot(slot).max_bytes.get();
    let score = max_bytes(pattern.score.slot);
    let bytes = slots.map(max_bytes).fold(score, u64::max);
    #[expect(
        clippy::unreachable,
        reason = "`Workflow::parse` refuses any judge's slot whose `max_bytes` is over \
                  `MAX_OP_PAYLOAD_BYTES`, and these are judges' slots, so only a bug in the parse \
                  can reach this; a parsed slot type carrying the bound would reshape \
                  `Workflow::slot` and `workflow::Judge::slots`, both `pub`, and the decision \
                  core's reads of them"
    )]
    let Ok(limit) = OpPayloadLimit::try_from(bytes) else {
        unreachable!("the agent's largest slot holds {bytes} bytes, over the op payload cap")
    };
    limit
}
