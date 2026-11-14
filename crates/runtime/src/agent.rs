//! The agent's sandbox, which runs the agent's IR command, once its directories, files and
//! standard input are in place, and whose session takes its ops.

use std::io;
use std::path::Path;
use std::sync::mpsc::Sender;

use decision::Event;
use gateway::{IncomingOp, Process, Put, RefusedOp, Session, Trees};
use provider::UnixTime;
use schema::ir::{AbsolutePath, Dir, File};
use schema::sandbox::op::OpReply;
use schema::sandbox::{
    ExitStatus, Launch, OpPayloadLimit, command_stdin, file_mode, log_dir, out_dir, pumpkin_dir,
};
use schema::workflow::{self, Workflow};
use stdx::{Context, Deadline};

use super::log::Entry;
use super::sandbox::{Sandboxes, Started, Which, collect, launch};
use super::{Clock, Host, Input};

/// The agent's sandbox, from when it starts until it is deleted.
pub(super) struct Agent {
    sandbox: Started,
    /// The agent's command, until its end, or its session's, has been reported; `None` if it
    /// didn't start.
    process: Option<Process>,
}

impl Agent {
    /// Starts the agent's sandbox, whose session takes its ops, and its model requests if it has
    /// a model route, puts its directories, files and standard input there ([`put`]), then starts
    /// its command, all by `by`, its session's arrivals sent on `inbox`, each at the time `clock`
    /// reads as the session's reader hands it over, its create given `hard_end`
    /// ([`Sandboxes::start`]). Returns the sandbox, if it started, and what came of it:
    /// [`Event::AgentStarted`] or [`Event::AgentFailed`].
    pub(super) fn start(
        sandboxes: &Sandboxes<'_, impl Host>,
        workflow: &Workflow,
        by: Deadline,
        hard_end: UnixTime,
        inbox: Sender<Input>,
        clock: impl Clock,
    ) -> (Option<Self>, Event) {
        let agent = workflow.agent();
        let ops = Some(op_payload_limit(workflow));
        // Stamped as the reader hands it over, which is when it reached the host.
        let wrap = move |arrival| Input::Agent(clock.now(), arrival);
        let started = sandboxes.start(Which::Agent, ops, by, hard_end, inbox, wrap);
        let mut sandbox = match started {
            Ok(sandbox) => sandbox,
            Err(error) => {
                let reason = format!("the agent's sandbox didn't start: {error}");
                return (None, Event::AgentFailed { reason });
            }
        };
        let started = put(&mut sandbox.session, agent, by).and_then(|stdin| {
            let launch = Launch {
                stdin,
                ..launch(&agent.command)
            };
            (sandbox.session.start(launch, by)).context("the agent's command didn't start")
        });
        let (process, event) = match started {
            Ok(process) => (Some(process), Event::AgentStarted),
            Err(reason) => (None, Event::AgentFailed { reason }),
        };
        (Some(Self { sandbox, process }), event)
    }

    /// The event that the exit of `process`, with `status`, brings the core, if the core still
    /// waits on it as the agent's command: [`Event::AgentExited`]. The agent's end, its command's
    /// exit or its session's end ([`Agent::ended`]), is reported once.
    pub(super) fn exited(&mut self, process: Process, status: ExitStatus) -> Option<Event> {
        // What follows a report, or a command that didn't start: the core waits on neither. The
        // loop tears the agent down in the batch that hears of either, so it drops what follows
        // with no agent; this keeps the agent's end reported once whatever the loop does.
        if self.process != Some(process) {
            return None;
        }
        self.process = None;
        Some(Event::AgentExited { status })
    }

    /// The event that its session's end, for `reason`, brings the core, if the core still waits
    /// on the agent's command: [`Event::AgentFailed`], reported once, as [`Agent::exited`] says.
    pub(super) fn ended(&mut self, reason: &str) -> Option<Event> {
        self.process.take()?;
        let reason = format!("the agent's session ended: {reason}");
        Some(Event::AgentFailed { reason })
    }

    /// Answers `op`, which the agent's session handed out, with `reply`, by `by`.
    pub(super) fn reply(&mut self, op: IncomingOp, reply: OpReply, by: Deadline) -> io::Result<()> {
        self.sandbox.session.reply(op, reply, by)
    }

    /// The trees of the agent's sandbox, which a lease's thread moves to and from its runner.
    pub(super) fn trees(&self) -> Trees {
        self.sandbox.session.trees()
    }

    /// Refuses `refused`, which the agent's session handed out, by `by`.
    pub(super) fn refuse(&mut self, refused: RefusedOp, by: Deadline) -> io::Result<()> {
        self.sandbox.session.refuse(refused, by)
    }

    /// Freezes the sandbox, then collects its [`out_dir()`] and [`log_dir()`] into `dir`, all by
    /// `by`. Returns what failed, if anything.
    pub(super) fn freeze_and_collect(&mut self, dir: &Path, by: Deadline) -> Vec<String> {
        let session = &mut self.sandbox.session;
        let mut failures = Vec::new();
        if let Err(error) = session.freeze(by) {
            failures.push(format!("freezing the agent's sandbox failed: {error}"));
        }
        for part in [out_dir(), log_dir()] {
            if let Err(error) = collect(session, &part, dir, by) {
                failures.push(format!("the agent's {part} is incomplete: {error}"));
            }
        }
        failures
    }

    /// Ends the agent's sandbox, with whatever still runs in it, by [`Sandboxes::delete`].
    pub(super) fn delete(
        self,
        sandboxes: &Sandboxes<'_, impl Host>,
    ) -> (Vec<Entry>, Result<(), String>) {
        sandboxes.delete(self.sandbox)
    }
}

/// Puts `agent`'s directories and files in its sandbox, at `/`, then its standard input's text,
/// if it has one, at [`command_stdin()`], by `by`. Returns where the standard input is.
fn put(
    session: &mut Session,
    agent: &workflow::Agent,
    by: Deadline,
) -> Result<Option<AbsolutePath>, String> {
    let dirs = (agent.dirs.iter()).map(|Dir { path, mode }| Put::Dir(path.relative(), *mode));
    let files = (agent.files.iter()).map(|file| {
        let File {
            path,
            mode,
            content,
        } = file;
        Put::File(path.relative(), *mode, content.as_bytes())
    });
    (session.put(&AbsolutePath::root(), dirs.chain(files), by))
        .context("the agent's files can't be put below `/`")?;
    let Some(text) = &agent.stdin else {
        return Ok(None);
    };
    let stdin = command_stdin();
    let put = [Put::File(stdin.clone(), file_mode(), text.as_bytes())];
    (session.put(&pumpkin_dir(), put, by)).context("the agent's standard input can't be put")?;
    Ok(Some(pumpkin_dir().join(&stdin)))
}

/// The largest payload the agent may send: the largest `max_bytes` of its judge grants' slots
/// and of the score's slot, which caps its submissions.
fn op_payload_limit(workflow: &Workflow) -> OpPayloadLimit {
    let slots = workflow.judge_grants().map(|(_, grant)| grant.slot);
    let limit = |slot| workflow.slot(slot).payload_limit();
    let score = limit(workflow.score().slot);
    slots.map(limit).fold(score, OpPayloadLimit::max)
}
