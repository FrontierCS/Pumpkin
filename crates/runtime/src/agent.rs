//! The agent's sandbox, which runs the agent's IR command, once its directories, files and
//! standard input are in place, and whose session takes its ops.

use std::io;
use std::path::Path;
use std::sync::mpsc::Sender;
use std::thread::{self, Scope};
use std::time::Duration;

use decision::Event;
use gateway::{Arrival, IncomingOp, Process, Put, RefusedOp, Session, Trees};
use provider::UnixTime;
use schema::ir::{AbsolutePath, Dir, File};
use schema::sandbox::op::OpReply;
use schema::sandbox::{
    ExitStatus, Launch, OpPayloadLimit, command_stdin, file_mode, log_dir, out_dir, pumpkin_dir,
};
use schema::workflow::{self, AgentId, Workflow};
use stdx::{Context, Deadline};

use super::log::Entry;
use super::sandbox::{Sandboxes, Started, Which, collect, launch};
use super::{Clock, Host, Inbox, Input, Report, Runtime, deadline, failed_also, record};

/// The agent's sandbox, from when it starts until it is deleted.
pub(super) struct Agent {
    sandbox: Started,
    /// The agent's command, until its end, or its session's, has been reported; `None` if it
    /// didn't start.
    process: Option<Process>,
}

impl Agent {
    /// Creates the sandbox and installs the files its command needs. Keeps the session even on
    /// a file error so its output and deletion are accounted for by the usual teardown.
    fn prepare(
        sandboxes: &Sandboxes<'_, impl Host>,
        agent: &workflow::Agent,
        by: Deadline,
        hard_end: UnixTime,
        inbox: Sender<Input>,
        clock: impl Clock,
    ) -> Result<(Self, Result<Launch, String>), String> {
        let id = agent.id;
        let ops = Some(op_payload_limit(sandboxes.workflow(), id));
        let wrap = move |arrival| Input::Agent(id, clock.now(), arrival);
        let mut sandbox = sandboxes
            .start(Which::Agent(id), ops, by, hard_end, inbox, wrap)
            .context("the agent's sandbox didn't start")?;
        let launch = put(&mut sandbox.session, agent, by).map(|stdin| Launch {
            stdin,
            ..launch(&agent.command)
        });
        Ok((
            Self {
                sandbox,
                process: None,
            },
            launch,
        ))
    }

    /// The event that the exit of `process`, with `status`, brings the core, if the core still
    /// waits on it as the agent's command: [`Event::AgentExited`]. The agent's end, its command's
    /// exit or its session's end ([`Agent::ended`]), is reported once.
    pub(super) fn exited(
        &mut self,
        agent: AgentId,
        process: Process,
        status: ExitStatus,
    ) -> Option<Event> {
        // What follows a report, or a command that didn't start: the core waits on neither. The
        // loop tears the agent down in the batch that hears of either, so it drops what follows
        // with no agent; this keeps the agent's end reported once whatever the loop does.
        if self.process != Some(process) {
            return None;
        }
        self.process = None;
        Some(Event::AgentExited { agent, status })
    }

    /// The event that its session's end, for `reason`, brings the core, if the core still waits
    /// on the agent's command: [`Event::AgentFailed`], reported once, as [`Agent::exited`] says.
    pub(super) fn ended(&mut self, agent: AgentId, reason: &str) -> Option<Event> {
        self.process.take()?;
        let reason = format!("the agent's session ended: {reason}");
        Some(Event::AgentFailed { agent, reason })
    }

    pub(super) fn reply(&mut self, op: IncomingOp, reply: OpReply, by: Deadline) -> io::Result<()> {
        self.sandbox.session.reply(op, reply, by)
    }

    pub(super) fn trees(&self) -> Trees {
        self.sandbox.session.trees()
    }

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

    pub(super) fn delete(
        self,
        sandboxes: &Sandboxes<'_, impl Host>,
    ) -> (Vec<Entry>, Result<(), String>) {
        sandboxes.delete(self.sandbox)
    }
}

/// Arrivals can reach the driver before its launch thread returns the session. A deadline can
/// also ask for teardown then; the session is collected as soon as the thread returns it.
#[derive(Default)]
pub(super) struct Starting {
    pub(super) arrivals: Vec<(Duration, Arrival)>,
    pub(super) tear_down: bool,
}

impl<'r, H: Host + 'r> Runtime<'r, H> {
    pub(super) fn tear_down_agent<'s>(
        &mut self,
        scope: &'s Scope<'s, 'r>,
        id: AgentId,
    ) -> Result<(), String> {
        let team = self.workflow().final_call().selections();
        if let Some(starting) = self.starting.get_mut(&id) {
            starting.tear_down = true;
            return Ok(());
        }
        let Some(mut agent) = self.agents.remove(&id) else {
            if team {
                self.pending
                    .push_back((self.clock.now(), Event::AgentTornDown { agent: id }));
            }
            return self
                .log
                .note("the agent's sandbox never started: nothing was collected");
        };
        let dir = self.dir.join(Which::Agent(id).record_dir(self.workflow()));
        let by = deadline(&self.clock, self.teardown_end());
        let sandboxes = self.sandboxes;
        let collect = move |agent: &mut Agent| {
            let mut logged = agent
                .freeze_and_collect(&dir, by)
                .into_iter()
                .map(Entry::Note)
                .collect::<Vec<_>>();
            let collected = record::collected(&dir);
            logged.push(Entry::Note(format!(
                "collected from the agent's sandbox: {collected}"
            )));
            logged.push(Entry::AgentCollected(id));
            logged
        };
        if team {
            let sender = self.inbox.sender();
            let clock = self.clock.clone();
            thread::Builder::new()
                .spawn_scoped(scope, move || {
                    for entry in collect(&mut agent) {
                        let _ = sender.send(Input::Logged(entry));
                    }
                    let (logged, result) = agent.delete(sandboxes);
                    for entry in logged {
                        let _ = sender.send(Input::Logged(entry));
                    }
                    let report = match result {
                        Ok(()) => Report::Event(clock.now(), Event::AgentTornDown { agent: id }),
                        Err(error) => Report::Undeleted(error),
                    };
                    let _ = sender.send(Input::Child(report));
                })
                .context("starting the agent's teardown thread")?;
            Ok(())
        } else {
            let collected = collect(&mut agent)
                .into_iter()
                .try_for_each(|entry| self.log.entry(entry));
            let (logged, deleted) = agent.delete(sandboxes);
            let noted = logged
                .into_iter()
                .try_for_each(|entry| self.log.entry(entry));
            failed_also(failed_also(collected, noted), deleted)
        }
    }

    pub(super) fn start_agents<'s>(&mut self, scope: &'s Scope<'s, 'r>) -> Result<(), String> {
        let workflow = self.workflow();
        for agent in workflow.agents() {
            self.serve(decision::Caller::Agent(agent.id));
        }
        let sandboxes = self.sandboxes;
        let hard_end = self.unix_hard_end;
        let placed = self.placed;
        let prepare = |agent: &workflow::Agent, clock: H::Clock, inbox| {
            Agent::prepare(
                sandboxes,
                agent,
                deadline(&clock, placed.startup(agent.id.role())),
                hard_end,
                inbox,
                clock,
            )
        };
        let prepared = together(workflow.agents().map(|agent| {
            let clock = self.clock.clone();
            let inbox = self.inbox.sender();
            (agent.id, move || prepare(agent, clock, inbox))
        }));
        let released = self.clock.now();
        let startup = workflow
            .agents()
            .map(|agent| placed.startup(agent.id.role()))
            .max()
            .unwrap_or_default();
        let team = workflow.final_call().selections();
        let launch_end = if team {
            self.pending.push_back((released, Event::WorkStarted));
            // A late or failed setup can't take another prepared member's chance to launch, or
            // extend its work clock. The driver serves the rest while a launch awaits its reply.
            released
                .min(startup)
                .saturating_add(workflow.work_deadline())
        } else {
            startup
        };
        for (id, prepared) in prepared {
            let (agent, launch) = match prepared
                .context("starting the agent's setup thread")
                .and_then(|r| r)
            {
                Ok((agent, Ok(launch))) => (agent, launch),
                Ok((agent, Err(reason))) => {
                    self.agents.insert(id, agent);
                    self.pending
                        .push_back((released, Event::AgentFailed { agent: id, reason }));
                    continue;
                }
                Err(reason) => {
                    self.pending
                        .push_back((released, Event::AgentFailed { agent: id, reason }));
                    continue;
                }
            };
            let by = launch_end.min(released.saturating_add(placed.startup(id.role())));
            self.launch_agent(scope, id, agent, launch, by)?;
        }
        Ok(())
    }

    fn launch_agent<'s>(
        &mut self,
        scope: &'s Scope<'s, 'r>,
        id: AgentId,
        mut agent: Agent,
        launch: Launch,
        launch_end: Duration,
    ) -> Result<(), String> {
        let clock = self.clock.clone();
        let launch = move || {
            let result = agent
                .sandbox
                .session
                .start(launch, deadline(&clock, launch_end))
                .context("the agent's command didn't start")
                .map(|process| agent.process = Some(process));
            (agent, clock.now(), result)
        };
        if self.workflow().final_call().selections() {
            let inbox = self.inbox.sender();
            self.starting.insert(id, Starting::default());
            thread::Builder::new()
                .spawn_scoped(scope, move || {
                    let (agent, at, result) = launch();
                    let _ = inbox.send(Input::Launched(id, at, Box::new(agent), result));
                })
                .context("starting the agent's launch thread")?;
        } else {
            let (agent, at, result) = launch();
            self.agents.insert(id, agent);
            let event = match result {
                Ok(()) => Event::WorkStarted,
                Err(reason) => Event::AgentFailed { agent: id, reason },
            };
            self.pending.push_back((at, event));
        }
        Ok(())
    }
}

// Each startup step finishes for all members before the next. A singleton runs directly on the
// driver thread and a team gets one result for every member, even if a thread couldn't start.
fn together<T: Send, F: FnOnce() -> T + Send>(
    mut work: impl ExactSizeIterator<Item = (AgentId, F)>,
) -> Vec<(AgentId, io::Result<T>)> {
    if work.len() == 1 {
        return work
            .next()
            .map(|(id, run)| (id, Ok(run())))
            .into_iter()
            .collect();
    }
    thread::scope(|scope| {
        let workers: Vec<_> = work
            .map(|(id, run)| (id, thread::Builder::new().spawn_scoped(scope, run)))
            .collect();
        workers
            .into_iter()
            .map(|(id, worker)| {
                let result = worker.map(|worker| {
                    worker
                        .join()
                        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
                });
                (id, result)
            })
            .collect()
    })
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

/// The largest payload the agent may send: the largest `payload_max_bytes` of its job and now
/// functions.
fn op_payload_limit(workflow: &Workflow, agent: AgentId) -> OpPayloadLimit {
    workflow
        .job_functions_of(agent.role())
        .map(|(_, function)| function.payload_limit())
        .chain(
            (workflow.now_functions_of(agent.role())).map(|(_, function)| function.payload_limit()),
        )
        .fold(OpPayloadLimit::NONE, OpPayloadLimit::max)
}
