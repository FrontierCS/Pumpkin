//! The agent's sandbox, which runs the agent's IR command and takes no ops.

use std::sync::mpsc::Sender;

use pumpkin_gateway::{Arrival, Process};
use pumpkin_schema::workflow::Workflow;
use stdx::{Context, Deadline};

use super::Input;
use super::sandbox::{Sandboxes, Started, launch};
use crate::driver::core::Event;

/// The agent's sandbox, from when it starts until it is deleted.
pub(super) struct Agent {
    sandbox: Started,
    /// The agent's command, until its end, or its session's, has been reported; `None` if it
    /// didn't start.
    process: Option<Process>,
}

impl Agent {
    /// Starts the agent's sandbox, then its command, both by `by`, its session's arrivals sent
    /// on `inbox`. Returns the sandbox, if it started, and what came of it:
    /// [`Event::AgentStarted`] or [`Event::AgentFailed`].
    pub(super) fn start(
        sandboxes: &Sandboxes<'_>,
        workflow: &Workflow,
        by: Deadline,
        inbox: Sender<Input>,
    ) -> (Option<Self>, Event) {
        let agent = workflow.agent();
        let started = sandboxes.start("agent", &agent.sandbox, by, inbox, Input::Agent);
        let mut sandbox = match started {
            Ok(sandbox) => sandbox,
            Err(error) => {
                let reason = format!("the agent's sandbox didn't start: {error}");
                return (None, Event::AgentFailed { reason });
            }
        };
        let started = (sandbox.session.start(launch(&agent.command), by))
            .context("the agent's command didn't start");
        let (process, event) = match started {
            Ok(process) => (Some(process), Event::AgentStarted),
            Err(reason) => (None, Event::AgentFailed { reason }),
        };
        (Some(Self { sandbox, process }), event)
    }

    /// The event that `arrival`, from the agent's session, brings the core, if the core still
    /// waits on the agent's command: its exit is [`Event::AgentExited`], and its session's end
    /// before that [`Event::AgentFailed`]. Either is reported once.
    pub(super) fn arrived(&mut self, arrival: Arrival) -> Option<Event> {
        let event = match arrival {
            Arrival::Exited { process, status } if self.process == Some(process) => {
                Event::AgentExited { status }
            }
            Arrival::Ended { reason } if self.process.is_some() => Event::AgentFailed {
                reason: format!("the agent's session ended: {reason}"),
            },
            // What follows a report, or a command that didn't start: the core waits on neither.
            // The loop tears the agent down in the batch that hears of either, so it drops what
            // follows in `Runtime::arrived`, with no agent; this keeps the agent's end reported
            // once whatever the loop does.
            Arrival::Exited { .. } | Arrival::Ended { .. } => return None,
            #[expect(
                clippy::unreachable,
                reason = "the agent's session is accepted without ops, so an op breaks the \
                          protocol, which ends the session, and arrives as its end; no type \
                          excludes it, since one `Arrival` serves sessions with ops and without"
            )]
            Arrival::Op(_) | Arrival::Refused(_) => {
                unreachable!("an op arrived on the agent's session, which takes none")
            }
        };
        self.process = None;
        Some(event)
    }

    /// Ends the agent's sandbox, with whatever still runs in it, by [`Sandboxes::delete`].
    pub(super) fn delete(self, sandboxes: &Sandboxes<'_>) -> Result<(), String> {
        sandboxes.delete(self.sandbox)
    }
}
