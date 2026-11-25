//! The agent's sandbox, which runs the agent's IR command, once its directories, files and
//! standard input are in place, and whose session takes its ops.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, Scope};
use std::time::Duration;

use decision::Event;
use gateway::{IncomingOp, Process, Put, RefusedOp, Session, Trees};
use provider::UnixTime;
use schema::ir::{AbsolutePath, Dir, File};
use schema::sandbox::op::OpReply;
use schema::sandbox::{
    ExitStatus, Launch, OpPayloadLimit, command_stdin, file_mode, log_dir, out_dir, pumpkin_dir,
};
use schema::workflow::{self, AgentId, TargetId, Workflow};
use stdx::Context;
use stdx::time::Clock as _;

use super::log::Entry;
use super::sandbox::{Sandboxes, Started, Unstarted, Which, collect, launch};
use super::{Clock, Host, Inbox, Input, Report, Runtime, record};

/// The agent's sandbox, from when it starts until it is deleted.
pub(super) struct Agent<C> {
    sandbox: Started<C>,
    /// The reserved command, until the driver has reported its exit or the end of its session.
    process: Option<Process>,
    // The start is recorded once, even when the command sends an op before its confirmation.
    launched: bool,
}

impl<C: Clock> Agent<C> {
    /// Creates the sandbox and installs the files its command needs, by `by`, a time on `clock`.
    /// Keeps the session even on a file error so its output and deletion are accounted for by the
    /// usual teardown.
    fn prepare<H: Host<Clock = C>>(
        sandboxes: &Sandboxes<'_, H>,
        agent: &workflow::Agent,
        by: Duration,
        hard_end: UnixTime,
        inbox: Sender<Input<C>>,
        clock: &C,
    ) -> Result<(Self, Result<Launch, String>), Unstarted> {
        let id = agent.id;
        let ops = Some(op_payload_limit(sandboxes.workflow(), id));
        let stamp = clock.clone();
        let wrap = move |arrival| Input::Agent(id, stamp.now(), arrival);
        let which = Which::Agent(id);
        let mut sandbox = sandboxes.start(which, ops, (clock, by), hard_end, inbox, wrap)?;
        let launch = put(&mut sandbox.session, agent, by).map(|stdin| Launch {
            stdin,
            ..launch(&agent.command)
        });
        Ok((
            Self {
                sandbox,
                process: None,
                launched: false,
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

    pub(super) fn record_start(&mut self) -> bool {
        self.process.is_some() && !std::mem::replace(&mut self.launched, true)
    }

    pub(super) fn reply(&mut self, op: IncomingOp, reply: OpReply, by: Duration) -> io::Result<()> {
        self.sandbox.session.reply(op, reply, by)
    }

    pub(super) fn trees(&self) -> Trees {
        self.sandbox.session.trees()
    }

    pub(super) fn refuse(&mut self, refused: RefusedOp, by: Duration) -> io::Result<()> {
        self.sandbox.session.refuse(refused, by)
    }

    /// Freezes the sandbox, then collects its [`out_dir()`] and [`log_dir()`] into `dir`, all by
    /// `by`. Returns what failed, if anything.
    fn freeze_and_collect(&mut self, dir: &Path, by: Duration) -> Vec<String> {
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
}

impl<'r, H: Host + 'r> Runtime<'r, H> {
    pub(super) fn prepared<'s>(
        &mut self,
        scope: Option<&'s Scope<'s, 'r>>,
        id: AgentId,
        agent: Result<Box<Agent<H::Clock>>, String>,
    ) -> Result<(), String> {
        match agent {
            Ok(agent) => {
                self.agents.insert(id, *agent);
            }
            Err(detail) => {
                let role = &self.workflow().sandbox(id.role()).name;
                self.log.note(&format!(
                    "agent `{role}`: the agent's sandbox didn't start: {detail}"
                ))?;
            }
        }
        if self.starting.remove(&id) == Some(true)
            && let Some(scope) = scope
        {
            self.tear_down_agent(scope, id)?;
        }
        Ok(())
    }

    pub(super) fn tear_down_agent<'s>(
        &mut self,
        scope: &'s Scope<'s, 'r>,
        id: AgentId,
    ) -> Result<(), String> {
        if let Some(starting) = self.starting.get_mut(&id) {
            *starting = true;
            return Ok(());
        }
        let Some(mut agent) = self.agents.remove(&id) else {
            let _ = self.inbox.sender().send(Input::Child(Report::Event(
                self.clock.now(),
                Event::AgentTornDown { agent: id },
            )));
            return self
                .log
                .note("the agent's sandbox never started: nothing was collected");
        };
        let dir = self.dir.join(Which::Agent(id).record_dir(self.workflow()));
        let by = self.teardown_end();
        let sandboxes = self.sandboxes;
        let sender = self.inbox.sender();
        let clock = self.clock.clone();
        thread::Builder::new()
            .spawn_scoped(scope, move || {
                let log = |entry| {
                    let _ = sender.send(Input::Logged(entry));
                };
                for reason in agent.freeze_and_collect(&dir, by) {
                    log(Entry::Note(reason));
                }
                let collected = record::collected(&dir);
                let role = &sandboxes.workflow().sandbox(id.role()).name;
                log(Entry::Note(format!(
                    "collected from agent `{role}`'s sandbox: {collected}"
                )));
                log(Entry::AgentCollected(id));
                let (logged, result) = sandboxes.delete(agent.sandbox);
                for entry in logged {
                    log(entry);
                }
                let report = match result {
                    Ok(()) => Report::Event(clock.now(), Event::AgentTornDown { agent: id }),
                    Err(error) => Report::Undeleted(error),
                };
                let _ = sender.send(Input::Child(report));
            })
            .context("starting the agent's teardown thread")?;
        Ok(())
    }

    pub(super) fn start_agents<'s>(&mut self, scope: &'s Scope<'s, 'r>) -> Result<(), String> {
        let workflow = self.workflow();
        let sandboxes = self.sandboxes;
        // An agent's sandbox is torn down, and what it holds collected, by the teardown's end.
        let hard_end = self.unix_at(self.teardown_end());
        let mut ready = self.clock.inbox();
        let mut gates = BTreeMap::new();
        self.start_residents(scope, &ready.sender())?;
        let mut startup = self
            .residents
            .keys()
            .map(|target| self.placed.startup(target.role()))
            .max()
            .unwrap_or_default();
        for role in workflow.agents() {
            let id = role.id;
            self.serve(decision::Caller::Agent(id));
            let clock = self.clock.clone();
            let inbox = self.inbox.sender();
            let bound = self.placed.startup(id.role());
            let prepare = move || Agent::prepare(sandboxes, role, bound, hard_end, inbox, &clock);
            startup = startup.max(bound);
            self.starting.insert(id, false);
            let (release, gate) = mpsc::channel();
            let worker = Worker {
                id,
                clock: self.clock.clone(),
                notices: ready.sender(),
                inbox: self.inbox.sender(),
                gate,
            };
            thread::Builder::new()
                .spawn_scoped(scope, move || worker.start(prepare(), sandboxes, bound))
                .context("starting the agent's setup thread")?;
            gates.insert(id, release);
        }
        self.release_agents(startup, gates, &mut ready);
        Ok(())
    }

    fn release_agents(
        &mut self,
        startup: Duration,
        mut gates: BTreeMap<AgentId, Sender<Duration>>,
        ready: &mut impl Inbox<(Duration, Ready)>,
    ) {
        let mut releases = Vec::new();
        let mut failures = Vec::new();
        let mut waiting: BTreeSet<_> = self.residents.keys().copied().collect();
        while !gates.is_empty() || !waiting.is_empty() {
            if super::interruption::signal().is_some() {
                return;
            }
            // Even after the driver wakes past the bound, take notices already queued.
            let now = self.clock.now();
            let until = startup.min(now.saturating_add(super::LOOK));
            let Some((at, notice)) = ready.wait(until) else {
                if now >= startup {
                    break;
                }
                continue;
            };
            match notice {
                Ready::Agent(id, status) => {
                    if let Some(release) = gates.remove(&id) {
                        let reason = match status {
                            Ok(()) if at <= self.placed.startup(id.role()) => {
                                releases.push((id, release));
                                continue;
                            }
                            Ok(()) => SETUP_EXPIRED.to_owned(),
                            Err(reason) => reason,
                        };
                        failures.push(Event::AgentFailed { agent: id, reason });
                    }
                }
                Ready::Shared(target, status) => {
                    waiting.remove(&target);
                    if let Some(resident) = self.residents.get_mut(&target) {
                        match status {
                            Ok(()) if at <= self.placed.startup(target.role()) => {}
                            Ok(()) => {
                                let expired = SHARED_EXPIRED.to_owned();
                                failures.extend(resident.fail(target, at, expired));
                            }
                            Err(reason) => failures.extend(resident.fail(target, at, reason)),
                        }
                    }
                }
            }
        }
        if super::interruption::signal().is_some() {
            return;
        }
        let at = self.clock.now();
        self.pending.push_back((at, Event::WorkStarted));
        failures.extend(gates.into_keys().map(|agent| Event::AgentFailed {
            agent,
            reason: SETUP_EXPIRED.to_owned(),
        }));
        for target in waiting {
            if let Some(resident) = self.residents.get_mut(&target) {
                failures.extend(resident.fail(target, at, SHARED_EXPIRED.to_owned()));
            }
        }
        self.pending
            .extend(failures.into_iter().map(|event| (at, event)));
        let end = at
            .min(startup)
            .saturating_add(self.workflow().work_deadline());
        for (id, release) in releases {
            let _ = release.send(end.min(at.saturating_add(self.placed.startup(id.role()))));
        }
    }
}

const SETUP_EXPIRED: &str = "the agent's setup missed the startup deadline";

const SHARED_EXPIRED: &str = "the shared role's setup missed the startup deadline";

pub(super) enum Ready {
    Agent(AgentId, Result<(), String>),
    Shared(TargetId, Result<(), String>),
}

struct Worker<C> {
    id: AgentId,
    clock: C,
    notices: Sender<(Duration, Ready)>,
    inbox: Sender<Input<C>>,
    gate: Receiver<Duration>,
}

impl<C: Clock> Worker<C> {
    fn start(
        self,
        prepared: Result<(Agent<C>, Result<Launch, String>), Unstarted>,
        sandboxes: &Sandboxes<'_, impl Host<Clock = C>>,
        by: Duration,
    ) {
        let id = self.id;
        let (mut agent, launch) = match prepared {
            Ok(prepared) => prepared,
            Err(failure) => {
                let reason = format!("the agent's sandbox didn't start: {}", failure.reason());
                let _ = self
                    .notices
                    .send((self.clock.now(), Ready::Agent(id, Err(reason))));
                match failure.dispose(sandboxes) {
                    Ok(detail) => {
                        let _ = self.inbox.send(Input::Prepared(id, Err(detail)));
                    }
                    Err(reason) => {
                        let _ = self.inbox.send(Input::Child(Report::Undeleted(reason)));
                    }
                }
                return;
            }
        };
        let start = launch.and_then(|launch| {
            let (process, send) = agent
                .sandbox
                .session
                .prepare_start(by)
                .context("reserving the agent's command")?;
            agent.process = Some(process);
            Ok(move |by| send(launch, by))
        });
        // The driver owns the session before any command may send an op or exit.
        let _ = self.inbox.send(Input::Prepared(id, Ok(Box::new(agent))));
        let status = start.as_ref().map(|_| ()).map_err(Clone::clone);
        let _ = self
            .notices
            .send((self.clock.now(), Ready::Agent(id, status)));
        if let Ok(start) = start
            && let Ok(end) = self.gate.recv()
            && super::interruption::signal().is_none()
        {
            let result = start(end).context("the agent's command didn't start");
            let _ = self
                .inbox
                .send(Input::Launched(id, self.clock.now(), result));
        }
    }
}

/// Puts `agent`'s directories and files in its sandbox, at `/`, then its standard input's text,
/// if it has one, at [`command_stdin()`], by `by`. Returns where the standard input is.
fn put(
    session: &mut Session<impl Clock>,
    agent: &workflow::Agent,
    by: Duration,
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
