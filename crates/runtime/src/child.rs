//! The run's children: sandboxes that the core starts for a part of the run, each on a thread of
//! its own, in a fresh passive sandbox, which a driver of its kind drives: a judge call's, the
//! `judge` module's, or a lease's, the `lease` module's.
//!
//! [`run`] starts a child's sandbox on the run's one start path, hands it to its driver, and
//! reports the child's end only once its sandbox is deleted; a sandbox that can't be deleted fails
//! the run. A child's thread waits on an inbox of its own ([`Message`]), into which its session hands what its reader reads, and the loop the core's stop
//! and its driver's orders. A stop takes effect before the child's command starts, as the thread
//! takes what its inbox holds then, and while it waits.

use std::iter;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::Duration;

use decision::Event;
use gateway::{Arrival, Session};
use provider::UnixTime;

use super::host::deadline;
use super::log::Entry;
use super::sandbox::{Sandboxes, Which};
use super::{Clock, Host, Inbox, Input};

/// A child of the run, as its thread sees it, timed and synced by the host `H`, whose driver
/// takes orders of type `O`.
pub(super) struct Child<'r, H: Host, O: Send + 'static> {
    pub(super) which: Which,
    pub(super) deadlines: Deadlines,
    /// The run's hard end, as its sandbox's create takes it
    /// ([`Create::hard_end`](provider::Create::hard_end)).
    pub(super) hard_end: UnixTime,
    /// Its directory in the record, which exists and is empty.
    pub(super) dir: PathBuf,
    pub(super) sandboxes: &'r Sandboxes<'r, H>,
    pub(super) clock: H::Clock,
    /// What reaches its thread.
    pub(super) inbox: <H::Clock as Clock>::Inbox<Message<O>>,
    /// A sender into the loop's inbox, which takes its reports.
    pub(super) reports: Sender<Input>,
}

/// By when each of a child's steps is to be done, as times since the run started.
#[derive(Clone, Copy)]
pub(super) struct Deadlines {
    /// Its sandbox's start: created, accepted and set up.
    pub(super) startup: Duration,
    /// What it puts in its sandbox, and the start of each command.
    pub(super) start: Duration,
    /// Its sandbox's freeze, and the collecting of what it holds.
    pub(super) collect: Duration,
}

/// What reaches a child's thread through its inbox.
pub(super) enum Message<O> {
    /// What its session's reader handed over ([`Registration::accept`]): at most its commands'
    /// exits and its end, since a passive session hands out no op, and the session takes no
    /// model requests.
    ///
    /// [`Registration::accept`]: gateway::Registration::accept
    Session(Arrival),
    /// The core's stop.
    Stop,
    /// An order of the core's for its driver.
    Order(O),
}

/// What a child's thread reports to the loop.
pub(super) enum Report {
    /// An event, at the time the thread observed it: the core meters a child's time from its
    /// reports' times, which the loop's delays then don't lengthen.
    Event(Duration, Event),
    /// Why the child's sandbox couldn't be deleted, which fails the run.
    Undeleted(String),
}

/// Runs `child` to its end, reporting as it goes: starts its sandbox, then hands its session to
/// `drive`, whose event is the child's last report once the sandbox is deleted. A child whose
/// start is due by a time that has passed fails at once, with nothing created, as one whose
/// sandbox doesn't start does ([`Which::failed`]).
pub(super) fn run<H: Host, O: Send + 'static>(
    child: &mut Child<'_, H, O>,
    drive: impl FnOnce(&mut Child<'_, H, O>, &mut Session) -> Event,
) {
    let which = child.which;
    let noun = which.noun();
    // The loop can be held past a child's start, by the agent's teardown's delete or a sync of
    // the run log, and a sandbox created for a start already due would only be deleted, at or past
    // the hard end at worst.
    if child.clock.now() >= child.deadlines.startup {
        let reason =
            format!("{noun} sandbox wasn't created: its start was due by a time that had passed");
        return child.event(which.failed(reason));
    }
    let by = deadline(&child.clock, child.deadlines.startup);
    let inbox = child.inbox.sender();
    let started = (child.sandboxes).start(which, None, by, child.hard_end, inbox, Message::Session);
    let mut sandbox = match started {
        Ok(sandbox) => sandbox,
        Err(error) => {
            return child.event(which.failed(format!("{noun} sandbox didn't start: {error}")));
        }
    };
    let last = drive(child, &mut sandbox.session);
    let (logged, deleted) = child.sandboxes.delete(sandbox);
    for entry in logged {
        // The loop's inbox outlives every child's thread, which the run's end waits for.
        let _ = child.reports.send(Input::Logged(entry));
    }
    match deleted {
        Ok(()) => child.event(last),
        Err(error) => child.report(Report::Undeleted(error)),
    }
}

impl<H: Host, O: Send + 'static> Child<'_, H, O> {
    /// Whether the core has stopped the child, by what its inbox holds, which this takes. Before
    /// its command starts, the only other message can be its session's end, after which what the
    /// child asks of the session fails for that reason.
    pub(super) fn stopped(&mut self) -> bool {
        iter::from_fn(|| self.inbox.take()).any(|message| matches!(message, Message::Stop))
    }

    /// Reports an event, observed now.
    pub(super) fn event(&self, event: Event) {
        self.report(Report::Event(self.clock.now(), event));
    }

    /// Notes `note` in the run log.
    pub(super) fn note(&self, note: String) {
        // The loop's inbox outlives every child's thread, which the run's end waits for.
        let _ = (self.reports).send(Input::Logged(Entry::Note(note)));
    }

    pub(super) fn report(&self, report: Report) {
        // The loop's inbox outlives every child's thread, which the run's end waits for.
        let _ = self.reports.send(Input::Child(report));
    }
}
