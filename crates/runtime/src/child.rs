//! The run's children: a sandbox the core starts for a part of the run, a call's target or a
//! lease's runner, which the loop starts on the run's one start path, drives, and deletes.
//!
//! A child that fails to start, or whose start is due by a time that has passed, ends at once
//! ([`End::failed`]). A lease's end is reported once its sandbox is deleted, and a call's as it
//! comes, before the delete, whose end the call's teardown reports ([`Event::CallTornDown`]); a
//! sandbox that can't be deleted fails the run.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use decision::{CallResult, Event, JobId};
use gateway::{Ask, SessionId};
use provider::SandboxId;
use schema::sandbox::{RelativePath, pumpkin_dir};
use stdx::Context;

use crate::sandbox::{PUMPKIN, Which};
use crate::starts::DeleteWaiter;
use crate::{Host, Input, Io, Runtime};

/// A child's sandbox, as the loop drives it.
pub(super) struct Child {
    pub(super) which: Which,
    pub(super) deadlines: Deadlines,
    /// Its directory in the record, which exists.
    pub(super) dir: PathBuf,
    /// Its sandbox and session, once started.
    pub(super) started: Option<(SandboxId, SessionId)>,
}

/// By when each of a child's steps is to be done, as times since the run started.
#[derive(Clone, Copy)]
pub(super) struct Deadlines {
    /// Its sandbox's start: created, accepted and set up.
    pub(super) startup: Duration,
    /// Its sandbox's freeze, and the collecting of what it holds.
    pub(super) collect: Duration,
}

/// How a child ended.
pub(super) enum End {
    /// The event of its end, reported once its sandbox is gone: a lease's.
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
}

/// Collects `dir` of the sandbox's `.pumpkin` directory into `into`, a new directory, within the
/// `PUMPKIN` limits; what fails of it says what was collected.
pub(super) fn collect(dir: &RelativePath, into: &Path) -> Ask {
    Ask::Collect(pumpkin_dir().join(dir), into.to_owned(), PUMPKIN)
}

/// What a failed [`collect`] of `dir` says, for `error`.
pub(super) fn uncollected(dir: &RelativePath, error: &std::io::Error) -> String {
    format!("collecting {} failed: {error}", pumpkin_dir().join(dir))
}

impl<'r, 's, H: Host + 'r> Runtime<'r, 's, H> {
    /// A child for `which`, by `deadlines`, its directory in the record made.
    pub(super) fn child(&self, which: Which, deadlines: Deadlines) -> Result<Child, String> {
        let dir = self.dir.join(which.record_dir(self.workflow()));
        let parent = dir.parent().unwrap_or(self.dir);
        fs::create_dir_all(parent).context(parent.display())?;
        fs::create_dir(&dir).context(dir.display())?;
        Ok(Child {
            which,
            deadlines,
            dir,
            started: None,
        })
    }

    /// Reports `end` around the delete of `which`'s sandbox, if it `started`: a call's end before
    /// it, so that the call's caller hears it as the delete runs, and the call's teardown after
    /// it; any other end after it. A failed delete is reported in place of what follows it.
    pub(super) fn close_child(
        &mut self,
        io: &mut Io<'r, 's, H>,
        (which, started): (Which, Option<(SandboxId, SessionId)>),
        end: End,
    ) {
        let after = match end {
            End::Event(event) => event,
            End::Call(job, end) => {
                self.input(Input::CallEnded(job, end));
                Event::CallTornDown { job }
            }
        };
        match started {
            Some((sandbox, session)) => {
                let waiter = DeleteWaiter::Sandbox {
                    after,
                    session: Some(session),
                };
                self.begin_delete(io, which, sandbox, waiter);
            }
            None => self.input(Input::Core(after)),
        }
    }
}
