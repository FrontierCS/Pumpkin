//! The run's children: a sandbox the core starts for a part of the run, on a thread of its own,
//! which a driver of its kind drives.
//!
//! A child's thread waits on an inbox of its own ([`Message`]), into which its session hands what
//! its reader reads, and the loop the core's stop and its driver's orders. A stop takes effect
//! before the child's command starts, as the thread takes what its inbox holds then, and while it
//! waits.

use std::iter;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::thread::Scope;
use std::time::Duration;

use decision::{CallResult, Caller, Core, Event, JobId, Request};
use gateway::{Arrival, IncomingModelRequest, Session, Trees};
use provider::UnixTime;
use schema::sandbox::OpPayloadLimit;
use stdx::time::Clock as _;

use super::log::Entry;
use super::sandbox::{Sandboxes, Which};
use super::{Asked, Clock, Host, Inbox, Input, Runtime};

/// A child of the run, as its thread sees it, timed and synced by the host `H`, whose driver
/// takes orders of type `O`.
pub(super) struct Child<'r, H: Host, O: Send + 'static> {
    pub(super) which: Which,
    pub(super) deadlines: Deadlines,
    /// Its sandbox's hard end, when what it holds is to be collected, as its create takes it
    /// ([`Create::hard_end`](provider::Create::hard_end)).
    pub(super) hard_end: UnixTime,
    /// Its directory in the record, which exists and is empty.
    pub(super) dir: PathBuf,
    pub(super) sandboxes: &'r Sandboxes<'r, H>,
    pub(super) clock: H::Clock,
    pub(super) inbox: <H::Clock as Clock>::Inbox<Message<O>>,
    /// A sender into the loop's inbox, which outlives every child's thread, so a send can't fail.
    pub(super) reports: Sender<Input<H::Clock>>,
}

/// By when each of a child's steps is to be done, as times since the run started.
#[derive(Clone, Copy)]
pub(super) struct Deadlines {
    /// Its sandbox's start: created, accepted and set up.
    pub(super) startup: Duration,
    /// Its sandbox's freeze, and the collecting of what it holds.
    pub(super) collect: Duration,
}

pub(super) enum Message<O> {
    /// What its session's reader handed over ([`Registration::accept`]): its commands' exits and
    /// its end, and the ops and model requests of a session that takes them.
    ///
    /// [`Registration::accept`]: gateway::Registration::accept
    Session(Arrival),
    Stop,
    /// An order of the core's for its driver, or the core's reply to its op.
    Order(O),
}

pub(super) enum Report {
    /// An event, at the time the thread observed it: the core meters a child's time from its
    /// reports' times, which the loop's delays then don't lengthen.
    Event(Duration, Event),
    /// Job `job`'s call has ended, at the time `at` the thread observed it: its body returned, or
    /// the call failed for the reason given. Its teardown follows.
    CallEnded {
        at: Duration,
        job: JobId,
        end: Result<CallResult, String>,
    },
    /// An op that job `job`'s sandbox made, at the time `at` the thread took it, which the loop
    /// numbers, and whose reply it sends back as an order, with the sandbox's `trees`, which a
    /// transfer the op asks for moves.
    Op {
        at: Duration,
        job: JobId,
        request: Request,
        body: Option<crate::scoped::Waiting>,
        trees: Trees,
    },
    /// A model request that job `job`'s sandbox made, at the time `at` the thread took it, which
    /// the loop numbers and answers.
    Model {
        at: Duration,
        job: JobId,
        incoming: IncomingModelRequest,
    },
    /// Why the child's sandbox couldn't be deleted, which fails the run.
    Undeleted(String),
}

/// How a child ended, which its reports say.
pub(super) enum End {
    /// The event of its end, reported once its sandbox is gone: a lease's, or the agent's failure.
    Event(Event),
    /// A call's end, reported before its target's delete: what its body returned, or why the call
    /// failed.
    Call(JobId, Result<CallResult, String>),
}

impl End {
    /// The end of `which`'s child, failed for `reason`.
    pub(super) fn failed(which: Which, reason: String) -> Self {
        match which {
            Which::Agent(agent) => Self::Event(Event::AgentFailed { agent, reason }),
            Which::Shared(target) => Self::Event(Event::SharedFailed { target, reason }),
            Which::Call(job, ..) => Self::Call(job, Err(reason)),
            Which::Lease(lease, ..) => Self::Event(Event::LeaseFailed { lease, reason }),
        }
    }

    /// What is reported of the end at `at`, a time since the run started: before its sandbox's
    /// delete, if anything, and the event after it.
    pub(super) fn reports(self, at: Duration) -> (Option<Report>, Event) {
        match self {
            Self::Event(event) => (None, event),
            Self::Call(job, end) => (
                Some(Report::CallEnded { at, job, end }),
                Event::CallTornDown { job },
            ),
        }
    }
}

/// Runs `child` to its end, reporting as it goes: starts its sandbox, whose session takes ops
/// with `ops` as their payload limit, if given, then hands its session to `drive`, whose end it
/// reports around the sandbox's delete ([`Child::close`]). A child whose start is due by a time
/// that has passed fails at once, with nothing created, as one whose sandbox doesn't start does
/// ([`End::failed`]).
pub(super) fn run<H: Host, O: Send + 'static>(
    child: &mut Child<'_, H, O>,
    ops: Option<OpPayloadLimit>,
    drive: impl FnOnce(&mut Child<'_, H, O>, &mut Session<H::Clock>) -> End,
) {
    let which = child.which;
    let noun = which.noun();
    // The loop can be held past a child's start, by the agent's teardown's delete or a sync of
    // the run log, and a sandbox created for a start already due would only be deleted, at or past
    // the hard end at worst.
    if child.clock.now() >= child.deadlines.startup {
        let reason =
            format!("{noun} sandbox wasn't created: its start was due by a time that had passed");
        return child.close(End::failed(which, reason), || Ok(()));
    }
    let by = (&child.clock, child.deadlines.startup);
    let inbox = child.inbox.sender();
    let started = (child.sandboxes).start(which, ops, by, child.hard_end, inbox, Message::Session);
    let mut sandbox = match started {
        Ok(sandbox) => sandbox,
        Err(failure) => match failure.dispose(child.sandboxes) {
            Ok(reason) => {
                let reason = format!("{noun} sandbox didn't start: {reason}");
                return child.close(End::failed(which, reason), || Ok(()));
            }
            Err(reason) => return child.report(Report::Undeleted(reason)),
        },
    };
    let last = drive(child, &mut sandbox.session);
    child.close(last, || {
        let (logged, deleted) = child.sandboxes.delete(sandbox);
        for entry in logged {
            child.log(entry);
        }
        deleted
    });
}

impl<H: Host, O: Send + 'static> Child<'_, H, O> {
    /// Whether the core has stopped the child, by what its inbox holds, which this takes. Before
    /// its command starts, the only other message can be its session's end, after which what the
    /// child asks of the session fails for that reason.
    pub(super) fn stopped(&mut self) -> bool {
        iter::from_fn(|| self.inbox.take()).any(|message| matches!(message, Message::Stop))
    }

    pub(super) fn event(&self, event: Event) {
        self.report(Report::Event(self.clock.now(), event));
    }

    /// Reports `end` around `delete`, which deletes what the child's sandbox left: a call's end
    /// before it, so that the call's caller hears it as the delete runs, and the call's teardown
    /// after it; any other end after it. A failed delete is reported in place of what follows it.
    pub(super) fn close(&self, end: End, delete: impl FnOnce() -> Result<(), String>) {
        let (before, after) = end.reports(self.clock.now());
        if let Some(report) = before {
            self.report(report);
        }
        match delete() {
            Ok(()) => self.event(after),
            Err(reason) => self.report(Report::Undeleted(reason)),
        }
    }

    pub(super) fn note(&self, note: String) {
        self.log(Entry::Note(note));
    }

    pub(super) fn log(&self, entry: Entry) {
        let _ = (self.reports).send(Input::Logged(entry));
    }

    pub(super) fn report(&self, report: Report) {
        let _ = self.reports.send(Input::Child(report));
    }
}

impl<'r, H: Host + 'r> Runtime<'r, H> {
    pub(super) fn report<'s>(
        &mut self,
        scope: Option<&'s Scope<'s, 'r>>,
        core: &Core<'_>,
        report: Report,
    ) -> Result<Option<(Duration, Event)>, String> {
        match report {
            Report::Event(at, Event::SharedFailed { target, reason }) => {
                let event = self
                    .residents
                    .get_mut(&target)
                    .and_then(|resident| resident.fail(target, reason));
                Ok(event.map(|event| (at, event)))
            }
            Report::Event(at, event) => Ok(Some((at, event))),
            Report::CallEnded { at, job, mut end } => {
                let target = self.call(job).target;
                if let Some(resident) = self.residents.get_mut(&target) {
                    resident.finish(job, &mut end);
                }
                // Whole for every bar the target heard: the route reports a bar before its client
                // hears it, so before the call can end on it, into this inbox, which keeps order.
                let count = self.call(job).model_requests_over_budget;
                let model_requests_over_budget = NonZeroU64::new(count);
                let event = match end {
                    Ok(result) => Event::CallReturned {
                        job,
                        result,
                        model_requests_over_budget,
                    },
                    Err(reason) => Event::CallFailed {
                        job,
                        reason,
                        model_requests_over_budget,
                    },
                };
                Ok(Some((at, event)))
            }
            Report::Op {
                at,
                job,
                request,
                body,
                trees,
            } => {
                self.call(job).trees = Some(trees);
                let asked = body.map_or(Asked::Job(job), |wait| Asked::Body(job, wait));
                Ok(Some((at, self.op(asked, request))))
            }
            Report::Model { at, job, incoming } => {
                let caller = Caller::Job(job);
                self.model_request(scope, core, (caller, at), incoming)
                    .map(|()| None)
            }
            Report::Undeleted(reason) => Err(reason),
        }
    }
}
