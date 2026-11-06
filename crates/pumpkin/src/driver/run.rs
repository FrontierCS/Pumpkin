//! The runtime: runs a workflow on a [`Provider`], as the [core](super::core) decides, and keeps
//! the run's record.
//!
//! [`Run::prepare`] parses a workflow and begins its record, and [`Run::execute`] runs it to
//! its outcome on one thread, driving the core as [`Core`] says. Between the core's ticks, the
//! thread waits on one inbox, into which the sessions of the run's sandboxes hand what they
//! read. Its sandboxes start on one path, the `sandbox` module's, and the agent's is the `agent`
//! module's. The record's files and the run log's records, and when each is written, are laid
//! out in the `record` and `log` modules.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant, SystemTime};

use pumpkin_gateway::Arrival;
use pumpkin_provider::Provider;
use pumpkin_schema::ir;
use pumpkin_schema::provider::{RunTag, SandboxId};
use pumpkin_schema::workflow::Workflow;
use serde_json::json;
use stdx::Deadline;

use crate::driver::core::{Action, Core, Event, Outcome, lasts};

mod agent;
mod log;
mod record;
mod sandbox;

use agent::Agent;
use log::Log;
use sandbox::Sandboxes;

/// A run, prepared: its workflow parsed, and its record begun.
#[derive(Debug)]
pub struct Run {
    workflow: Workflow,
    id: String,
    dir: PathBuf,
}

impl Run {
    /// Parses `ir` and makes the run's record in `runs`, which is created if need be:
    /// `ir.json`. The run's id holds `now`, the time by the caller's clock. A workflow that has a
    /// model route is refused, with `outcome.json` saying why.
    ///
    /// # Errors
    ///
    /// Fails if the IR breaks a parsing rule, if `now` is before 1970, if the workflow is
    /// refused, or if the record can't be written.
    pub fn prepare(ir: &ir::Workflow, runs: &Path, now: SystemTime) -> Result<Self, String> {
        let workflow = Workflow::parse(ir).map_err(|errors| errors.to_string())?;
        let (id, dir) = record::create(runs, now)?;
        record::write_json(&dir, "ir.json", &json!(ir))?;
        if let Some(reason) = model_route(&workflow) {
            let outcome = json!({ "state": "refused", "reason": reason });
            record::write_json(&dir, "outcome.json", &outcome)?;
            return Err(reason);
        }
        Ok(Self { workflow, id, dir })
    }

    /// The run's id, which names its record, and is the run tag its sandboxes carry.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The run's record.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Runs the workflow to its outcome on `provider`, with `sandboxd`, a static `sandboxd`
    /// binary, in every sandbox. The outcome, or why the run stopped without one, is also in the
    /// record's `outcome.json`. However the run ends, it deletes the sandboxes that `provider`
    /// still lists of it before returning.
    ///
    /// # Errors
    ///
    /// Fails if what the run's sandboxes run can't be published, or the provider doesn't name
    /// each thing published; if the run's socket directory can't be made; if the run was still
    /// going at its hard end ([`lasts`]); if the machine's clock reads before 1970; if the run's
    /// sandboxes can't be listed at its end, or one of them can't be deleted; or if the record
    /// can't be written.
    pub fn execute(self, provider: &dyn Provider, sandboxd: &Path) -> Result<Outcome, String> {
        let outcome = self.run_on(provider, sandboxd);
        let recorded = match &outcome {
            Ok(outcome) => {
                let mut recorded = log::outcome_json(outcome);
                recorded["state"] = "finished".into();
                recorded
            }
            Err(reason) => json!({ "state": "failed", "reason": reason }),
        };
        let written = record::write_json(&self.dir, "outcome.json", &recorded);
        failed_also(outcome, written)
    }

    /// Records what `provider` declared and publishes what the run's sandboxes run, then starts
    /// the run's clock and drives the run, keeping its log, and deletes what the provider still
    /// lists of it.
    fn run_on(&self, provider: &dyn Provider, sandboxd: &Path) -> Result<Outcome, String> {
        let ready = provider.ready();
        record::write_json(&self.dir, "provider.json", &json!(ready))?;
        let startup = Duration::from_secs(ready.capabilities.startup_seconds.get());
        let sandboxes = Sandboxes::create(provider, self.tag(), &self.workflow, sandboxd)?;
        let clock = Clock::start();
        let unix = record::since_epoch(clock.unix)?;
        let (sender, inbox) = mpsc::channel();
        let mut runtime = Runtime {
            workflow: &self.workflow,
            startup,
            sandboxes,
            clock,
            log: Log::create(&self.dir)?,
            agent: None,
            pending: VecDeque::new(),
            inbox,
            sender,
        };
        let outcome = runtime.drive(unix);
        let outcome = failed_also(outcome, runtime.sweep());
        let noted = match &outcome {
            Ok(_) => Ok(()),
            Err(reason) => (runtime.log).note(clock.now(), &format!("the run failed: {reason}")),
        };
        failed_also(outcome, noted)
    }

    /// The tag its sandboxes carry: its id.
    fn tag(&self) -> RunTag {
        match RunTag::try_from(self.id.clone()) {
            Ok(tag) => tag,
            #[expect(
                clippy::unreachable,
                reason = "a run's id is a UTC time's digits and `-`, then lowercase hexadecimal \
                          digits (`record::create`), all of which a run tag allows, and starts \
                          with a digit; a run tag built from a time and a number, which couldn't \
                          fail, would be a `pub` item of the schema for this one caller"
            )]
            Err(error) => unreachable!("run {}'s id is no run tag: {error}", self.id),
        }
    }
}

/// A run's clock: the time since the run started, which the core takes with each event, and
/// when that was by the machine's clock.
#[derive(Clone, Copy, Debug)]
struct Clock {
    start: Instant,
    unix: SystemTime,
}

impl Clock {
    /// The clock of a run that starts now.
    fn start() -> Self {
        Self {
            start: Instant::now(),
            unix: SystemTime::now(),
        }
    }

    fn now(self) -> Duration {
        self.start.elapsed()
    }
}

/// `outcome`, failed also for `then`'s error, if it has one: both reasons are kept.
fn failed_also<T>(outcome: Result<T, String>, then: Result<(), String>) -> Result<T, String> {
    match (outcome, then) {
        (outcome, Ok(())) => outcome,
        (Ok(_), Err(error)) => Err(error),
        (Err(reason), Err(error)) => Err(format!("{reason}; {error}")),
    }
}

/// Why a workflow with a model route is refused. Parsing has refused a route on a judge.
fn model_route(workflow: &Workflow) -> Option<String> {
    let agent = &workflow.agent().sandbox;
    agent.model_route.as_ref()?;
    Some(format!(
        "`{}` has a model route, which the driver doesn't serve: remove the role's \
         `egress.model` to run without one",
        agent.name
    ))
}

/// The state of a run as it is driven.
struct Runtime<'r> {
    workflow: &'r Workflow,
    /// The longest the provider takes to start a sandbox.
    startup: Duration,
    /// The run's sandboxes, and its only way to its provider.
    sandboxes: Sandboxes<'r>,
    clock: Clock,
    log: Log,
    agent: Option<Agent>,
    /// Events that performing actions produced, to feed the core next.
    pending: VecDeque<(Duration, Event)>,
    /// What reaches the loop from other threads, which it waits on until the core's next tick.
    inbox: Receiver<Input>,
    /// The inbox's sender, which the runtime clones for each session's sink.
    // Kept, so that the inbox never disconnects while the loop waits on it, with or without a
    // session.
    sender: Sender<Input>,
}

/// What reaches the loop through its inbox.
enum Input {
    /// What the agent's session's reader handed over ([`Listener::accept`]): at most its
    /// command's exit, then its end, as the session takes no ops and runs one command.
    ///
    /// [`Listener::accept`]: pumpkin_gateway::Listener::accept
    Agent(Arrival),
}

impl Runtime<'_> {
    /// Drives the run from its start, `unix` after the Unix epoch, to its outcome.
    fn drive(&mut self, unix: Duration) -> Result<Outcome, String> {
        let (mut core, mut actions) = Core::start(self.workflow, self.startup);
        self.log.start(self.workflow, unix, &actions)?;
        loop {
            if let Some(outcome) = self.perform(actions)? {
                return Ok(outcome);
            }
            let (at, event) = self.next_event(&core);
            actions = core.handle(at, event.clone());
            self.log.event(self.workflow, at, &event, &actions)?;
        }
    }

    /// The run's hard end, since it started, as the core has it.
    fn hard_end(&self) -> Duration {
        lasts(self.workflow, self.startup)
    }

    /// Waits for the next event: one that performing an action produced; else the core's tick, if
    /// it is due; else what arrives in the inbox before the tick is due, and else the tick then.
    /// The wait ends at the tick's time, which arrivals the core doesn't wait on, dropped as they
    /// come, don't move.
    fn next_event(&mut self, core: &Core<'_>) -> (Duration, Event) {
        if let Some(event) = self.pending.pop_front() {
            return event;
        }
        #[expect(
            clippy::unreachable,
            reason = "`perform` returns on `Finish` or `Abandon` before this is called, and \
                      `tick_due` is `Some` in every phase where the run is still going, so \
                      `None` here is a bug"
        )]
        let Some(due) = core.tick_due() else {
            unreachable!("the run is still going, yet the core has no tick due")
        };
        loop {
            let now = self.clock.now();
            let Some(wait) = due.checked_sub(now).filter(|wait| !wait.is_zero()) else {
                return (now, Event::Tick);
            };
            let input = match self.inbox.recv_timeout(wait) {
                Ok(input) => input,
                // The tick is due, which the next turn finds.
                Err(RecvTimeoutError::Timeout) => continue,
                #[expect(
                    clippy::unreachable,
                    reason = "the runtime keeps `sender` for as long as it waits on the inbox; no \
                              type says so, since a receiver can't know of its owner's sender"
                )]
                Err(RecvTimeoutError::Disconnected) => {
                    unreachable!("the inbox disconnected, though the runtime keeps a sender")
                }
            };
            if let Some(event) = self.arrived(input) {
                return event;
            }
        }
    }

    /// The event that `input` brings the core, if the core waits on it, at the time the loop
    /// took it.
    fn arrived(&mut self, input: Input) -> Option<(Duration, Event)> {
        match input {
            Input::Agent(arrival) => {
                // No agent: the core waits on nothing from the agent's session. The loop tears the
                // agent down in the batch that hears of its end, or of its command's failed start,
                // so whatever of the session's it takes after that batch comes here, such as its
                // end as the teardown drops it, or the exit of a command whose start failed at its
                // deadline just as the sandbox said it started. So does the end of a session that
                // the start path dropped after its set-up failed.
                let event = self.agent.as_mut()?.arrived(arrival)?;
                Some((self.clock.now(), event))
            }
        }
    }

    /// Performs the core's actions, in order: the outcome once one finishes the run.
    fn perform(&mut self, actions: Vec<Action>) -> Result<Option<Outcome>, String> {
        for action in actions {
            match action {
                Action::StartAgent => {
                    // The agent starts at the run's start, so its startup ends long before the
                    // hard end, which counts it.
                    let by = Deadline::after(self.startup);
                    let sender = self.sender.clone();
                    let (agent, event) = Agent::start(&self.sandboxes, self.workflow, by, sender);
                    self.agent = agent;
                    self.pending.push_back((self.clock.now(), event));
                }
                #[expect(
                    clippy::unreachable,
                    reason = "the core replies to ops, and starts and stops judge calls, only for \
                              the ops it was given, and this runtime gives it none; a type that \
                              left these out would split the core's `Action`, which every \
                              runtime of the core shares"
                )]
                Action::Reply { .. } | Action::StartJudge { .. } | Action::StopJudge { .. } => {
                    unreachable!("the core asked for {action:?}, though it was given no op")
                }
                Action::TearDownAgent => self.tear_down_agent()?,
                Action::Finish { outcome } => return Ok(Some(outcome)),
                Action::Abandon => {
                    return Err(format!(
                        "the run was still going at its hard end, {} s after it started: \
                         expected it to end within its deadline, its finalize grace, its final \
                         judge's timeout and its sandboxes' startups",
                        self.hard_end().as_secs()
                    ));
                }
            }
        }
        Ok(None)
    }

    /// Deletes the agent's sandbox, which kills whatever runs in it.
    fn tear_down_agent(&mut self) -> Result<(), String> {
        let Some(agent) = self.agent.take() else {
            let note = "the agent's sandbox never started: nothing was collected";
            return self.log.note(self.clock.now(), note);
        };
        agent.delete(&self.sandboxes)
    }

    /// Deletes every sandbox the run's provider still lists of it, and notes which.
    fn sweep(&mut self) -> Result<(), String> {
        let deleted = self.sandboxes.sweep()?;
        if deleted.is_empty() {
            return Ok(());
        }
        let ids: Vec<&str> = deleted.iter().map(SandboxId::as_str).collect();
        let note = format!("deleted the sandboxes the run left: {}", ids.join(", "));
        self.log.note(self.clock.now(), &note)
    }
}
