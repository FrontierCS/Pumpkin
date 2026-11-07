//! The agent's sandbox, which runs the agent's IR command, once its directories, files and
//! standard input are in place, and whose session takes its ops.

use std::io;
use std::path::Path;
use std::sync::mpsc::Sender;

use pumpkin_gateway::{IncomingOp, Process, Put, RefusedOp, Session};
use pumpkin_schema::ir::{AbsolutePath, Dir, File, Mode};
use pumpkin_schema::sandbox::op::OpReply;
use pumpkin_schema::sandbox::{
    ExitStatus, Launch, OpPayloadLimit, RelativePath, command_stdin, file_mode, log_dir, out_dir,
    pumpkin_dir,
};
use pumpkin_schema::workflow::{self, Main, Workflow};
use stdx::{Context, Deadline};

use super::Input;
use super::sandbox::{PUMPKIN, Sandboxes, Started, collect, launch};
use crate::driver::core::Event;

/// The agent's sandbox, from when it starts until it is deleted.
pub(super) struct Agent {
    sandbox: Started,
    /// The agent's command, until its end, or its session's, has been reported; `None` if it
    /// didn't start.
    process: Option<Process>,
}

impl Agent {
    /// Starts the agent's sandbox, whose session takes its ops, puts its directories, files and
    /// standard input there ([`put`]), then starts its command, all by `by`, its session's
    /// arrivals sent on `inbox`. Returns the sandbox, if it started, and what came of it:
    /// [`Event::AgentStarted`] or [`Event::AgentFailed`].
    pub(super) fn start(
        sandboxes: &Sandboxes<'_>,
        workflow: &Workflow,
        by: Deadline,
        inbox: Sender<Input>,
    ) -> (Option<Self>, Event) {
        let agent = workflow.agent();
        let ops = op_payload_limit(workflow);
        let started = sandboxes.start("agent", &agent.sandbox, Some(ops), by, inbox, Input::Agent);
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
            if let Err(error) = collect(session, &part, dir, PUMPKIN, by) {
                failures.push(format!("the agent's {part} is incomplete: {error}"));
            }
        }
        failures
    }

    /// Ends the agent's sandbox, with whatever still runs in it, by [`Sandboxes::delete`].
    pub(super) fn delete(self, sandboxes: &Sandboxes<'_>) -> Result<(), String> {
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
    let dirs: Vec<(RelativePath, Mode)> = (agent.dirs.iter())
        .map(|Dir { path, mode }| (path.relative(), *mode))
        .collect();
    let files: Vec<(RelativePath, Mode, &[u8])> = (agent.files.iter())
        .map(|file| {
            let File {
                path,
                mode,
                content,
            } = file;
            (path.relative(), *mode, content.as_bytes())
        })
        .collect();
    let dirs = dirs.iter().map(|(path, mode)| Put::Dir(path, *mode));
    let files = (files.iter()).map(|(path, mode, content)| Put::File(path, *mode, content));
    let puts: Vec<Put<'_>> = dirs.chain(files).collect();
    (session.put(&AbsolutePath::root(), &puts, by))
        .context("the agent's files can't be put below `/`")?;
    let Some(text) = &agent.stdin else {
        return Ok(None);
    };
    let stdin = command_stdin();
    let put = [Put::File(&stdin, file_mode(), text.as_bytes())];
    (session.put(&pumpkin_dir(), &put, by)).context("the agent's standard input can't be put")?;
    Ok(Some(pumpkin_dir().join(&stdin)))
}

/// The largest payload the agent may send: the largest `max_bytes` of its judge grants' slots
/// and of the score's slot, which caps its submissions.
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
