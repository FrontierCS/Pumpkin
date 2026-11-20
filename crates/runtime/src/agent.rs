//! The agent's sandbox, which runs the agent's IR command, once its directories, files and
//! standard input are in place, and whose session takes its ops.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
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
use schema::workflow::{self, AgentId, TargetId, Workflow};
use stdx::{Context, Deadline};

use super::log::Entry;
use super::sandbox::{Sandboxes, Started, Unstarted, Which, collect, launch};
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
    ) -> Result<(Self, Result<Launch, String>), Unstarted> {
        let id = agent.id;
        let ops = Some(op_payload_limit(sandboxes.workflow(), id));
        let wrap = move |arrival| Input::Agent(id, clock.now(), arrival);
        let mut sandbox = sandboxes.attempt(Which::Agent(id), ops, by, hard_end, inbox, wrap)?;
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
        let team = self.workflow().final_call().selections() || !self.services.is_empty();
        if let Some(starting) = self.starting.get_mut(&id) {
            starting.tear_down = true;
            return Ok(());
        }
        let Some(mut agent) = self.agents.remove(&id) else {
            if team {
                let _ = self.inbox.sender().send(Input::Child(Report::Event(
                    self.clock.now(),
                    Event::AgentTornDown { agent: id },
                )));
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
            let subject = if team {
                format!("agent `{}`", sandboxes.workflow().sandbox(id.role()).name)
            } else {
                "the agent".to_owned()
            };
            logged.push(Entry::Note(format!(
                "collected from {subject}'s sandbox: {collected}"
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

    pub(super) fn launched<'s>(
        &mut self,
        scope: Option<&'s Scope<'s, 'r>>,
        id: AgentId,
        at: Duration,
        agent: Option<Agent>,
        result: Result<(), String>,
    ) -> Result<Option<(Duration, Event)>, String> {
        self.agents.extend(agent.map(|agent| (id, agent)));
        if result.is_ok() {
            self.log.entry(Entry::AgentLaunched(id, at))?;
        }
        if let Some(starting) = self.starting.remove(&id) {
            self.arrivals.extend(
                starting
                    .arrivals
                    .into_iter()
                    .map(|(at, arrival)| Input::Agent(id, at, arrival)),
            );
            if starting.tear_down {
                if let Err(reason) = result {
                    self.log.note(&format!(
                        "agent `{}`: {reason}",
                        self.workflow().sandbox(id.role()).name
                    ))?;
                }
                if let Some(scope) = scope {
                    self.tear_down_agent(scope, id)?;
                }
                return Ok(None);
            }
        }
        Ok(result
            .err()
            .map(|reason| (at, Event::AgentFailed { agent: id, reason })))
    }

    pub(super) fn start_agents<'s>(&mut self, scope: &'s Scope<'s, 'r>) -> Result<(), String> {
        let workflow = self.workflow();
        let sandboxes = self.sandboxes;
        let hard_end = self.unix_hard_end;
        let (notices, ready) = mpsc::channel();
        let mut gates = BTreeMap::new();
        self.start_services(scope, &notices)?;
        let mut startup = self
            .services
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
            let by = deadline(&clock, bound);
            let prepare = move || Agent::prepare(sandboxes, role, by, hard_end, inbox, clock);
            if !workflow.final_call().selections() && self.services.is_empty() {
                let (agent, result) = match prepare() {
                    Ok((agent, launch)) => agent.launch(launch, by),
                    Err(failure) => {
                        let reason = failure.dispose(sandboxes).unwrap_or_else(|reason| reason);
                        (
                            None,
                            Err(format!("the agent's sandbox didn't start: {reason}")),
                        )
                    }
                };
                self.agents.extend(agent.map(|agent| (id, agent)));
                let event = result.map_or_else(
                    |reason| Event::AgentFailed { agent: id, reason },
                    |()| Event::WorkStarted,
                );
                self.pending.push_back((self.clock.now(), event));
                return Ok(());
            }
            startup = startup.max(bound);
            self.starting.insert(id, Starting::default());
            let (release, gate) = mpsc::channel();
            let worker = Worker {
                id,
                clock: self.clock.clone(),
                notices: notices.clone(),
                inbox: self.inbox.sender(),
                gate,
            };
            thread::Builder::new()
                .spawn_scoped(scope, move || worker.start(prepare(), sandboxes))
                .context("starting the agent's setup thread")?;
            gates.insert(id, release);
        }
        drop(notices);
        self.release_agents(startup, gates, &ready);
        Ok(())
    }

    fn release_agents(
        &mut self,
        startup: Duration,
        mut gates: BTreeMap<AgentId, Sender<Duration>>,
        ready: &Receiver<Ready>,
    ) {
        let by = deadline(&self.clock, startup);
        let mut releases = Vec::new();
        let mut waiting: BTreeSet<_> = self.services.keys().copied().collect();
        let mut failures = Vec::new();
        while !gates.is_empty() || !waiting.is_empty() {
            let Some(left) = by.remaining() else { break };
            match ready.recv_timeout(left) {
                Ok(Ready::Agent(id, status)) => {
                    if let Some(release) = gates.remove(&id) {
                        match status {
                            Ok(()) => releases.push((id, release)),
                            Err(reason) => failures.push(Event::AgentFailed { agent: id, reason }),
                        }
                    }
                }
                Ok(Ready::Shared(target, status)) => {
                    waiting.remove(&target);
                    if let Some(service) = self.services.get_mut(&target) {
                        match status {
                            Ok(process) => service.process = Some(process),
                            Err(reason) => failures.extend(service.fail(target, reason)),
                        }
                    }
                }
                Err(_) => break,
            }
        }
        let at = self.clock.now();
        self.pending.push_back((at, Event::WorkStarted));
        failures.extend(gates.into_keys().map(|agent| Event::AgentFailed {
            agent,
            reason: "the agent's setup missed the startup deadline".to_owned(),
        }));
        for target in waiting {
            if let Some(service) = self.services.get_mut(&target) {
                failures.extend(service.fail(
                    target,
                    "the shared role's setup missed the startup deadline".to_owned(),
                ));
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

impl Agent {
    fn launch(
        mut self,
        launch: Result<Launch, String>,
        by: Deadline,
    ) -> (Option<Self>, Result<(), String>) {
        let result = launch
            .and_then(|launch| {
                self.sandbox
                    .session
                    .start(launch, by)
                    .context("the agent's command didn't start")
            })
            .map(|process| self.process = Some(process));
        (Some(self), result)
    }
}

pub(super) enum Ready {
    Agent(AgentId, Result<(), String>),
    Shared(TargetId, Result<Process, String>),
}

struct Worker<C> {
    id: AgentId,
    clock: C,
    notices: Sender<Ready>,
    inbox: Sender<Input>,
    gate: Receiver<Duration>,
}

impl<C: Clock> Worker<C> {
    fn start(
        self,
        prepared: Result<(Agent, Result<Launch, String>), Unstarted>,
        sandboxes: &Sandboxes<'_, impl Host>,
    ) {
        let id = self.id;
        let (agent, result) = match prepared {
            Ok((agent, Ok(launch))) => {
                let _ = self.notices.send(Ready::Agent(id, Ok(())));
                match self.gate.recv() {
                    Ok(end) => agent.launch(Ok(launch), deadline(&self.clock, end)),
                    Err(_) => (
                        Some(agent),
                        Err("the agent's setup missed the startup deadline".to_owned()),
                    ),
                }
            }
            Ok((agent, Err(reason))) => {
                let _ = self.notices.send(Ready::Agent(id, Err(reason.clone())));
                (Some(agent), Err(reason))
            }
            Err(failure) => {
                let reason = format!("the agent's sandbox didn't start: {}", failure.reason());
                let _ = self.notices.send(Ready::Agent(id, Err(reason)));
                match failure.dispose(sandboxes) {
                    Ok(detail) => (
                        None,
                        Err(format!("the agent's sandbox didn't start: {detail}")),
                    ),
                    Err(reason) => {
                        let _ = self.inbox.send(Input::Child(Report::Undeleted(reason)));
                        return;
                    }
                }
            }
        };
        let _ = self.inbox.send(Input::Launched(
            id,
            self.clock.now(),
            agent.map(Box::new),
            result,
        ));
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
