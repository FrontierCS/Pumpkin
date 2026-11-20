//! What a call's thread relays between its target's session and the loop, while it waits for one
//! of the target's commands to exit.

use std::time::Duration;

use decision::JobId;
use gateway::{Arrival, IncomingOp, Process, Session};
use schema::sandbox::ExitStatus;
use schema::sandbox::op::OpReply;
use stdx::Deadline;

use crate::child::{Child, Message, Report};
use crate::host::deadline;
use crate::{Clock, Host, Inbox, request};

/// How waiting on a child's command ended.
pub(super) enum Waited {
    Exited(ExitStatus),
    /// Its timeout passed first.
    TimedOut,
    /// The core stopped the child first.
    Stopped,
}

/// What a job's thread relays between its sandbox's session and the loop: each op its sandbox
/// makes, which the loop answers with the reply it sends back, and each model request, which the
/// loop answers. The session hands out an op only once the one before it has been answered, so
/// at most one is unanswered, which a later wait answers if the command that made it has exited.
#[derive(Default)]
pub(super) struct Relay {
    asked: Option<IncomingOp>,
    closed: Option<String>,
}

/// What came of a message into a job's inbox, if it ends a wait.
enum Arrived {
    Exited(Process, ExitStatus),
    Stopped,
}

impl Relay {
    /// Waits for the command that job `job` runs to exit, until `timeout`, a time since the run
    /// started, or for a stop, relaying meanwhile. Fails with the reason the session ended, if it
    /// ended first.
    pub(super) fn wait<H: Host>(
        &mut self,
        child: &mut Child<'_, H, OpReply>,
        job: JobId,
        timeout: Duration,
        process: Process,
        session: &mut Session,
    ) -> Result<Waited, String> {
        if let Some(reason) = &self.closed {
            return Err(reason.clone());
        }
        let by = deadline(&child.clock, timeout);
        loop {
            let Some(message) = child.inbox.wait(timeout) else {
                return Ok(Waited::TimedOut);
            };
            match self.relay(child, job, (session, by), message)? {
                Some(Arrived::Exited(exited, status)) if exited == process => {
                    return Ok(Waited::Exited(status));
                }
                None | Some(Arrived::Exited(_, _)) => {}
                Some(Arrived::Stopped) => return Ok(Waited::Stopped),
            }
        }
    }

    /// Relays what job `job`'s inbox holds, while none of its commands runs, without waiting for
    /// more, and returns whether the core stopped the job. Fails with the reason the session
    /// ended, if it has.
    pub(super) fn idle<H: Host>(
        &mut self,
        child: &mut Child<'_, H, OpReply>,
        job: JobId,
        (session, by): (&mut Session, Deadline),
    ) -> Result<bool, String> {
        if let Some(reason) = &self.closed {
            return Err(reason.clone());
        }
        while let Some(message) = child.inbox.take() {
            match self.relay(child, job, (&mut *session, by), message)? {
                None => {}
                Some(Arrived::Stopped) => return Ok(true),
                Some(Arrived::Exited(_, _)) => unreachable!(
                    "job {} received a client exit while idle, though it waits for each client's \
                     exit and the driver handles the shared server's exit separately",
                    job.0
                ),
            }
        }
        Ok(false)
    }

    fn relay<H: Host>(
        &mut self,
        child: &mut Child<'_, H, OpReply>,
        job: JobId,
        (session, by): (&mut Session, Deadline),
        message: Message<OpReply>,
    ) -> Result<Option<Arrived>, String> {
        match message {
            Message::Stop => return Ok(Some(Arrived::Stopped)),
            Message::Order(reply) => {
                let Some(op) = self.asked.take() else {
                    unreachable!(
                        "job {}'s call was sent a reply to no op, though the loop replies only \
                         to the op the call relayed, once",
                        job.0
                    )
                };
                if let Err(error) = session.reply(op, reply, by) {
                    child.note(format!("job {}'s op's reply failed: {error}", job.0));
                }
            }
            Message::Session(Arrival::Op(incoming)) => {
                // A call's target holds no now function.
                let (request, unanswered) = request(incoming, |_, _| None);
                self.asked = Some(unanswered.into_incoming());
                let (at, trees) = (child.clock.now(), session.trees());
                child.report(Report::Op {
                    at,
                    job,
                    request,
                    trees,
                });
            }
            Message::Session(Arrival::Refused(refused)) => {
                let message = refused.message();
                if let Err(error) = session.refuse(refused, by) {
                    child.note(format!("job {}'s op's refusal failed: {error}", job.0));
                }
                child.note(format!("job {}'s op was refused: {message}", job.0));
            }
            // The sandbox runs one command at a time, so the exit is its.
            Message::Session(Arrival::Exited { process, status }) => {
                return Ok(Some(Arrived::Exited(process, status)));
            }
            Message::Session(Arrival::Ended { reason }) => {
                self.closed = Some(reason.clone());
                return Err(reason);
            }
            Message::Session(Arrival::ModelRequest(incoming)) => {
                let at = child.clock.now();
                child.report(Report::Model { at, job, incoming });
            }
        }
        Ok(None)
    }
}
