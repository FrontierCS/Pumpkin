//! What a call's thread relays between its target's session and the loop, while it waits for one
//! of the target's commands to exit.

use std::time::Duration;

use decision::JobId;
use gateway::{Arrival, IncomingOp, Process, Session};
use schema::ir::Name;
use schema::sandbox::ExitStatus;
use schema::sandbox::op::{LeaseStatusReply, OpReply, SessionOp};
use stdx::time::Clock as _;

use crate::child::{Child, Message, Report};
use crate::{Host, Inbox, request};

/// Separate destinations for the sandbox's outstanding op and the body's session operation.
pub(super) enum Order {
    Sandbox(OpReply),
    Body(Result<LeaseStatusReply, String>),
}

pub(super) enum Waited<T = ExitStatus> {
    Finished(T),
    /// Its timeout passed first.
    TimedOut,
    /// The core stopped the child first.
    Stopped,
}

/// The end of a job's session, at the time the run observed it. That is when the session's
/// reader handed it over, or when the loop ended a shared target.
#[derive(Clone)]
pub(super) struct SessionEnd {
    pub(super) at: Duration,
    pub(super) reason: String,
}

/// What a job's thread relays between its sandbox's session and the loop: each op its sandbox
/// makes, which the loop answers with the reply it sends back, and each model request, which the
/// loop answers. The session hands out an op only once the one before it has been answered, so
/// at most one is unanswered, which a later wait answers if the command that made it has exited.
#[derive(Default)]
pub(super) struct Relay {
    asked: Option<IncomingOp>,
    closed: Option<SessionEnd>,
}

/// What came of a message into a job's inbox, if it ends a wait.
enum Arrived {
    Body(Result<LeaseStatusReply, String>),
    Exited(Process, ExitStatus),
    Stopped,
}

impl Relay {
    /// Waits for the command that job `job` runs to exit, until `timeout`, a time since the run
    /// started, or for a stop, relaying meanwhile. Fails with the session's end, if it came first.
    pub(super) fn wait<H: Host>(
        &mut self,
        child: &mut Child<'_, H, Order>,
        job: JobId,
        timeout: Duration,
        process: Process,
        session: &mut Session<H::Clock>,
    ) -> Result<Waited, SessionEnd> {
        self.until(child, job, timeout, session, |arrival| match arrival {
            Arrived::Exited(exited, status) if exited == process => Some(status),
            _ => None,
        })
    }

    pub(super) fn runner<H: Host>(
        &mut self,
        child: &mut Child<'_, H, Order>,
        job: JobId,
        timeout: Duration,
        session: &mut Session<H::Clock>,
        grant: &Name,
        (waiting, op): (crate::scoped::Waiting, SessionOp),
    ) -> Result<Waited<Result<LeaseStatusReply, String>>, SessionEnd> {
        child.report(Report::Op {
            at: child.clock.now(),
            job,
            request: decision::Request::Session {
                grant: grant.clone(),
                op,
            },
            body: Some(waiting),
            trees: session.trees(),
        });
        self.until(child, job, timeout, session, |arrival| match arrival {
            Arrived::Body(result) => Some(result),
            _ => None,
        })
    }

    fn until<H: Host, T>(
        &mut self,
        child: &mut Child<'_, H, Order>,
        job: JobId,
        timeout: Duration,
        session: &mut Session<H::Clock>,
        finished: impl Fn(Arrived) -> Option<T>,
    ) -> Result<Waited<T>, SessionEnd> {
        if let Some(ended) = &self.closed {
            return Err(ended.clone());
        }
        loop {
            let Some(message) = child.inbox.wait(timeout) else {
                return Ok(Waited::TimedOut);
            };
            match self.relay(child, job, (session, timeout), message)? {
                Some(Arrived::Stopped) => return Ok(Waited::Stopped),
                Some(arrival) => {
                    if let Some(finished) = finished(arrival) {
                        return Ok(Waited::Finished(finished));
                    }
                }
                None => {}
            }
        }
    }

    /// Relays what job `job`'s inbox holds, while none of its commands runs, without waiting for
    /// more, and returns whether the core stopped the job. Fails with the session's end, if it
    /// has come.
    pub(super) fn idle<H: Host>(
        &mut self,
        child: &mut Child<'_, H, Order>,
        job: JobId,
        (session, by): (&mut Session<H::Clock>, Duration),
    ) -> Result<bool, SessionEnd> {
        if let Some(ended) = &self.closed {
            return Err(ended.clone());
        }
        while let Some(message) = child.inbox.take() {
            match self.relay(child, job, (&mut *session, by), message)? {
                Some(Arrived::Stopped) => return Ok(true),
                // Only a waiter for a particular process consumes its exit status.
                None | Some(Arrived::Exited(_, _) | Arrived::Body(_)) => {}
            }
        }
        Ok(false)
    }

    fn relay<H: Host>(
        &mut self,
        child: &mut Child<'_, H, Order>,
        job: JobId,
        (session, by): (&mut Session<H::Clock>, Duration),
        message: Message<Order>,
    ) -> Result<Option<Arrived>, SessionEnd> {
        match message {
            Message::Stop => return Ok(Some(Arrived::Stopped)),
            Message::Order(Order::Body(result)) => return Ok(Some(Arrived::Body(result))),
            Message::Order(Order::Sandbox(reply)) => {
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
            Message::Session(_, Arrival::Op(incoming)) => {
                // A call's target holds no now function.
                let (request, unanswered) = request(incoming, |_, _| None);
                self.asked = Some(unanswered.into_incoming());
                let (at, trees) = (child.clock.now(), session.trees());
                child.report(Report::Op {
                    at,
                    job,
                    request,
                    body: None,
                    trees,
                });
            }
            Message::Session(_, Arrival::Refused(refused)) => {
                let message = refused.message();
                if let Err(error) = session.refuse(refused, by) {
                    child.note(format!("job {}'s op's refusal failed: {error}", job.0));
                }
                child.note(format!("job {}'s op was refused: {message}", job.0));
            }
            Message::Session(_, Arrival::Exited { process, status }) => {
                return Ok(Some(Arrived::Exited(process, status)));
            }
            Message::Session(at, Arrival::Ended { reason }) => {
                let ended = SessionEnd { at, reason };
                self.closed = Some(ended.clone());
                return Err(ended);
            }
            Message::Session(_, Arrival::ModelRequest(incoming)) => {
                let at = child.clock.now();
                child.report(Report::Model { at, job, incoming });
            }
        }
        Ok(None)
    }
}
