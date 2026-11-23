//! The agent's sandbox, which runs the agent's IR command, once its directories, files and
//! standard input are in place, and whose session takes its ops: each step of its start and its
//! teardown is a step of the loop's, which the step before it completes into.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use decision::Event;
use gateway::{Ask, Process, Put, SessionId, Sessions, Takes};
use provider::SandboxId;
use schema::ir::{AbsolutePath, Dir, File};
use schema::sandbox::{
    ExitStatus, Launch, OpPayloadLimit, Root, command_stdin, file_mode, log_dir, out_dir,
    pumpkin_dir,
};
use schema::workflow::{self, AgentId, TargetId, Workflow};

use crate::child::Report;
use crate::log::Entry;
use crate::sandbox::{PUMPKIN, Which, launch};
use crate::starts::{DeleteWaiter, DisposeWaiter, StartWaiter};
use crate::{Host, Input, Io, Purpose, Runtime, interruption, record};

/// The agent's sandbox, from when its start begins until it is deleted.
pub(super) struct Agent {
    pub(super) session: Option<SessionId>,
    sandbox: Option<SandboxId>,
    /// The command, from its start until the driver has reported its exit or the end of its
    /// session.
    process: Option<Process>,
    // The start is recorded once, even when the command sends an op before its confirmation.
    launched: bool,
    // Whether the core asked for the teardown before the sandbox's start ended.
    torn: bool,
    step: Step,
    launch: Option<Launch>,
    // What each answer to an op in flight is, by its token, for what its failure says.
    pub(super) answering: BTreeMap<u64, String>,
}

/// The agent's step under way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Step {
    Starting,
    // Its files, then its standard input, by the order in which they are put.
    Putting(u8),
    Ready,
    Launching,
    Running,
    // The answer to an op, which the token's op names.
    Answer,
    Freezing,
    Collecting(u8),
    Deleting,
}

// When an agent's set-up ended, and how.
type SetUp = (Duration, Result<(), String>);

/// The start of the work, which waits for each agent's and shared role's set-up, or the startup.
pub(super) struct Gate {
    startup: Duration,
    agents: BTreeMap<AgentId, Option<SetUp>>,
    residents: BTreeSet<TargetId>,
    failures: Vec<Event>,
}

impl Agent {
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

    /// Whether the agent's teardown freezes or collects its sandbox, whose requests end by the
    /// teardown's end at the latest. Its delete is a delete like any other.
    pub(super) fn tearing_down(&self) -> bool {
        matches!(self.step, Step::Freezing | Step::Collecting(_))
    }
}

impl<'r, 's, H: Host + 'r> Runtime<'r, 's, H> {
    pub(super) fn start_agents(&mut self, io: &mut Io<'r, 's, H>) -> Result<(), String> {
        let workflow = self.workflow();
        self.start_residents()?;
        let mut startup = self
            .residents
            .keys()
            .map(|target| self.placed.startup(target.role()))
            .max()
            .unwrap_or_default();
        let mut gate = Gate {
            startup: Duration::ZERO,
            agents: BTreeMap::new(),
            residents: self.residents.keys().copied().collect(),
            failures: Vec::new(),
        };
        for role in workflow.agents() {
            let id = role.id;
            self.serve(decision::Caller::Agent(id));
            let bound = self.placed.startup(id.role());
            startup = startup.max(bound);
            gate.agents.insert(id, None);
            self.agents.insert(
                id,
                Agent {
                    session: None,
                    sandbox: None,
                    process: None,
                    launched: false,
                    torn: false,
                    step: Step::Starting,
                    launch: None,
                    answering: BTreeMap::new(),
                },
            );
            let takes = Takes {
                ops: Some(op_payload_limit(workflow, id)),
                models: workflow.model(id.role()).is_some(),
            };
            self.begin_start(io, Which::Agent(id), (bound, takes), StartWaiter::Agent(id));
        }
        gate.startup = startup;
        self.gate = Some(gate);
        self.release_agents(io);
        Ok(())
    }

    // Waits for each agent's and shared role's set-up, until the startup, then starts the work.
    fn release_agents(&mut self, io: &mut Io<'r, 's, H>) {
        loop {
            if interruption::signal().is_some() {
                return;
            }
            let Some(gate) = &self.gate else {
                unreachable!("the agents are released once")
            };
            let waits = gate.agents.values().any(Option::is_none) || !gate.residents.is_empty();
            if !waits || self.at >= gate.startup {
                break;
            }
            self.run(io, gate.startup);
        }
        let Some(gate) = self.gate.take() else {
            unreachable!("the agents are released once")
        };
        let Gate {
            startup,
            agents,
            residents,
            mut failures,
        } = gate;
        let at = self.at;
        self.pending.push_back((at, Event::WorkStarted));
        let mut releases = Vec::new();
        for (agent, ready) in agents {
            let reason = match ready {
                Some((ready_at, Ok(()))) if ready_at <= self.placed.startup(agent.role()) => {
                    releases.push(agent);
                    continue;
                }
                Some((_, Ok(()))) | None => SETUP_EXPIRED.to_owned(),
                Some((_, Err(reason))) => reason,
            };
            failures.push(Event::AgentFailed { agent, reason });
        }
        for target in residents {
            if let Some(resident) = self.residents.get_mut(&target) {
                failures.extend(resident.fail(target, SHARED_EXPIRED.to_owned()));
            }
        }
        self.pending
            .extend(failures.into_iter().map(|event| (at, event)));
        let end = at
            .min(startup)
            .saturating_add(self.workflow().work_deadline());
        for id in releases {
            let by = end.min(at.saturating_add(self.placed.startup(id.role())));
            self.launch_agent(io, id, by);
        }
    }

    pub(super) fn shared_ready(
        &mut self,
        target: TargetId,
        at: Duration,
        ready: Result<(), String>,
    ) {
        let Some(gate) = &mut self.gate else {
            return;
        };
        gate.residents.remove(&target);
        if let Some(resident) = self.residents.get_mut(&target) {
            match ready {
                Ok(()) if at <= self.placed.startup(target.role()) => {}
                Ok(()) => gate
                    .failures
                    .extend(resident.fail(target, SHARED_EXPIRED.to_owned())),
                Err(reason) => gate.failures.extend(resident.fail(target, reason)),
            }
        }
    }

    fn agent_set_up(&mut self, id: AgentId, ready: Result<(), String>) {
        let at = self.at;
        if let Some(gate) = &mut self.gate
            && let Some(slot) = gate.agents.get_mut(&id)
        {
            *slot = Some((at, ready));
        }
    }

    fn launch_agent(&mut self, io: &mut Io<'r, 's, H>, id: AgentId, by: Duration) {
        if interruption::signal().is_some() {
            return;
        }
        let Some(agent) = self.agents.get_mut(&id) else {
            return;
        };
        let (Some(session), Some(launch)) = (agent.session, agent.launch.take()) else {
            return;
        };
        agent.step = Step::Launching;
        let token = self.token(Purpose::Agent(id, Step::Launching));
        let process = Sessions::ask(self, io, session, Ask::Start(launch), (by, token));
        if let Some(agent) = self.agents.get_mut(&id) {
            agent.process = process;
        }
    }

    /// Takes the agent's sandbox, started and set up, whose files go in next.
    pub(super) fn agent_ready(
        &mut self,
        io: &mut Io<'r, 's, H>,
        id: AgentId,
        (sandbox, session): (SandboxId, SessionId),
    ) {
        let Some(agent) = self.agents.get_mut(&id) else {
            unreachable!("an agent's start ended for no agent")
        };
        agent.session = Some(session);
        agent.sandbox = Some(sandbox);
        agent.step = Step::Putting(0);
        self.put_agent(io, id);
    }

    // Puts the agent's next part: its directories and files, then its standard input's text.
    fn put_agent(&mut self, io: &mut Io<'r, 's, H>, id: AgentId) {
        let workflow = self.workflow();
        let Some(role) = workflow.agents().find(|agent| agent.id == id) else {
            unreachable!("agent {id:?} is no agent of the workflow's")
        };
        let Some(agent) = self.agents.get_mut(&id) else {
            return;
        };
        let (Step::Putting(part), Some(session)) = (agent.step, agent.session) else {
            return;
        };
        let by = self.placed.startup(id.role());
        let (dir, entries) = match (part, &role.stdin) {
            (0, _) => (AbsolutePath::root(), files(role)),
            (1, Some(text)) => {
                let put = Put::File(command_stdin(), file_mode(), text.as_bytes().to_vec());
                (pumpkin_dir(), vec![put])
            }
            _ => return self.agent_put(io, id),
        };
        let token = self.token(Purpose::Agent(id, Step::Putting(part)));
        Sessions::ask(
            self,
            io,
            session,
            Ask::Put(dir, Root::Kept, entries),
            (by, token),
        );
    }

    // Ends the agent's set-up, its files put, and hands the gate its launch.
    fn agent_put(&mut self, io: &mut Io<'r, 's, H>, id: AgentId) {
        let Some(role) = self.workflow().agents().find(|agent| agent.id == id) else {
            unreachable!("agent {id:?} is no agent of the workflow's")
        };
        let Some(agent) = self.agents.get_mut(&id) else {
            return;
        };
        let stdin = role
            .stdin
            .as_ref()
            .map(|_| pumpkin_dir().join(&command_stdin()));
        agent.launch = Some(Launch {
            stdin,
            ..launch(&role.command)
        });
        agent.step = Step::Ready;
        let torn = agent.torn;
        self.agent_set_up(id, Ok(()));
        if torn {
            self.tear_down_agent(io, id);
        }
    }

    /// Takes why the agent's sandbox didn't start, which its disposal follows.
    pub(super) fn agent_unstarted(
        &mut self,
        io: &mut Io<'r, 's, H>,
        id: AgentId,
        unstarted: crate::sandbox::Unstarted,
    ) {
        let reason = format!("the agent's sandbox didn't start: {}", unstarted.reason());
        self.agent_set_up(id, Err(reason));
        match unstarted {
            crate::sandbox::Unstarted::Uncreated(reason) => self.agent_disposed(id, Ok(reason)),
            crate::sandbox::Unstarted::Created {
                which,
                id: made,
                reason,
            } => {
                self.dispose(io, (which, made), &reason, DisposeWaiter::Agent(id));
            }
        }
    }

    /// Takes how the disposal of the agent's sandbox that didn't start went.
    pub(super) fn agent_disposed(&mut self, id: AgentId, disposed: Result<String, String>) {
        let torn = self.agents.remove(&id).is_some_and(|agent| agent.torn);
        match disposed {
            Ok(detail) => self.input(Input::Unstarted(id, detail, torn)),
            Err(reason) => self.input(Input::Child(Report::Undeleted(reason))),
        }
    }

    pub(super) fn tear_down_agent(&mut self, io: &mut Io<'r, 's, H>, id: AgentId) {
        let Some(agent) = self.agents.get_mut(&id) else {
            let at = self.at;
            let torn_down = Event::AgentTornDown { agent: id };
            self.input(Input::Child(Report::Event(at, torn_down)));
            return self.note("the agent's sandbox never started: nothing was collected");
        };
        match agent.step {
            Step::Starting | Step::Putting(_) => {
                agent.torn = true;
                return;
            }
            Step::Freezing | Step::Collecting(_) | Step::Deleting => return,
            Step::Ready | Step::Launching | Step::Running | Step::Answer => {}
        }
        let Some(session) = agent.session else {
            unreachable!("an agent past its start holds its session until its delete")
        };
        // The core waits on neither its command's exit nor its session's end from now on: the
        // freeze kills the one, and the delete ends the other.
        agent.process = None;
        agent.step = Step::Freezing;
        let token = self.token(Purpose::Agent(id, Step::Freezing));
        let by = self.teardown_end();
        Sessions::ask(self, io, session, Ask::Freeze, (by, token));
    }

    /// Takes the answer to one of the agent's steps.
    pub(super) fn agent_answered(
        &mut self,
        io: &mut Io<'r, 's, H>,
        (id, step, token): (AgentId, Step, u64),
        answer: gateway::Answer,
    ) {
        use gateway::Answer::{Collected, Done, Started};
        match (step, answer) {
            (Step::Putting(part), Done(done)) => match done {
                Ok(()) => {
                    if let Some(agent) = self.agents.get_mut(&id) {
                        agent.step = Step::Putting(part + 1);
                    }
                    self.put_agent(io, id);
                }
                Err(error) => {
                    let reason = match part {
                        0 => format!("the agent's files can't be put below `/`: {error}"),
                        _ => format!("the agent's standard input can't be put: {error}"),
                    };
                    let torn = self.agents.get_mut(&id).is_some_and(|agent| {
                        agent.step = Step::Ready;
                        agent.torn
                    });
                    self.agent_set_up(id, Err(reason));
                    if torn {
                        self.tear_down_agent(io, id);
                    }
                }
            },
            (Step::Launching, Started(started)) => match started {
                Ok(_) => {
                    if let Some(agent) = self.agents.get_mut(&id) {
                        agent.step = Step::Running;
                    }
                    if let Err(error) = self.launched(id, self.at) {
                        self.fail(error);
                    }
                }
                Err(error) => {
                    if let Some(agent) = self.agents.get_mut(&id) {
                        agent.step = Step::Running;
                    }
                    let reason = format!("the agent's command didn't start: {error}");
                    self.input(Input::Core(Event::AgentFailed { agent: id, reason }));
                }
            },
            (Step::Answer, Done(done)) => {
                let what = (self.agents.get_mut(&id))
                    .and_then(|agent| agent.answering.remove(&token))
                    .unwrap_or_default();
                if let Err(error) = done {
                    self.note(&format!("{what} failed: {error}"));
                }
            }
            (Step::Freezing, Done(done)) => {
                if let Err(error) = done {
                    self.note(&format!("freezing the agent's sandbox failed: {error}"));
                }
                self.collect_agent(io, id, 0);
            }
            (Step::Collecting(part), Collected(collected)) => {
                if let Err(error) = collected {
                    let name = [out_dir(), log_dir()][usize::from(part)].clone();
                    let from = pumpkin_dir().join(&name);
                    self.note(&format!(
                        "the agent's {name} is incomplete: collecting {from} failed: {error}"
                    ));
                }
                self.collect_agent(io, id, part + 1);
            }
            (step, answer) => unreachable!("{answer:?} answered the agent's {step:?}"),
        }
    }

    // Collects the agent's `out`, then its `log`, then deletes its sandbox.
    fn collect_agent(&mut self, io: &mut Io<'r, 's, H>, id: AgentId, part: u8) {
        let which = Which::Agent(id);
        let dir = self.dir.join(which.record_dir(self.workflow()));
        let Some(agent) = self.agents.get_mut(&id) else {
            return;
        };
        let Some(session) = agent.session else {
            return;
        };
        if let Some(name) = [out_dir(), log_dir()].get(usize::from(part)) {
            agent.step = Step::Collecting(part);
            let from = pumpkin_dir().join(name);
            let into = dir.join(name.as_str());
            let token = self.token(Purpose::Agent(id, Step::Collecting(part)));
            let by = self.teardown_end();
            let collect = Ask::Collect(from, into, PUMPKIN);
            Sessions::ask(self, io, session, collect, (by, token));
            return;
        }
        let collected = record::collected(&dir);
        let role = &self.workflow().sandbox(id.role()).name;
        self.note(&format!(
            "collected from agent `{role}`'s sandbox: {collected}"
        ));
        self.logged(Entry::AgentCollected(id));
        let Some(agent) = self.agents.get_mut(&id) else {
            return;
        };
        agent.step = Step::Deleting;
        agent.session = None;
        let Some(sandbox) = agent.sandbox.clone() else {
            unreachable!("an agent that holds its session holds its sandbox")
        };
        self.begin_delete(io, which, sandbox, DeleteWaiter::Agent(id, Some(session)));
    }

    /// Takes how the delete of the agent's sandbox went.
    pub(super) fn agent_deleted(&mut self, id: AgentId, deleted: Result<(), String>) {
        self.agents.remove(&id);
        let report = match deleted {
            Ok(()) => Report::Event(self.at, Event::AgentTornDown { agent: id }),
            Err(error) => Report::Undeleted(error),
        };
        self.input(Input::Child(report));
    }

    /// Queues `input`, which reached the loop now.
    pub(super) fn input(&mut self, input: Input) {
        self.inputs.push_back((self.at, input));
    }

    pub(super) fn note(&mut self, note: &str) {
        if let Err(error) = self.log.note(note) {
            self.fail(error);
        }
    }

    /// Takes a thread's ask for a sandbox's start.
    pub(super) fn asked(&mut self, io: &mut Io<'r, 's, H>, asked: crate::starts::Asked) {
        let crate::starts::Asked {
            which,
            ops,
            by,
            arrivals,
            reply,
        } = asked;
        let takes = Takes {
            ops,
            models: self.workflow().model(which.role()).is_some(),
        };
        self.begin_start(io, which, (by, takes), StartWaiter::Thread(arrivals, reply));
    }
}

const SETUP_EXPIRED: &str = "the agent's setup missed the startup deadline";

const SHARED_EXPIRED: &str = "the shared role's setup missed the startup deadline";

// The agent's directories and files, to put below `/`.
fn files(agent: &workflow::Agent) -> Vec<Put> {
    let dirs = (agent.dirs.iter()).map(|Dir { path, mode }| Put::Dir(path.relative(), *mode));
    let files = (agent.files.iter()).map(|file| {
        let File {
            path,
            mode,
            content,
        } = file;
        Put::File(path.relative(), *mode, content.as_bytes().to_vec())
    });
    dirs.chain(files).collect()
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
