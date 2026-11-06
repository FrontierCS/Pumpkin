//! The runtime: runs a workflow on a [`Provider`], as the [core](super::core) decides, and keeps
//! the run's record.
//!
//! [`Run::prepare`] parses a workflow and begins its record, and [`Run::execute`] runs it to
//! its outcome on one thread, driving the core as [`Core`] says. The record's files and the run
//! log's records, and when each is written, are laid out in the `record` and `log` modules.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use pumpkin_provider::Provider;
use pumpkin_schema::ir;
use pumpkin_schema::workflow::Workflow;
use serde_json::json;

use crate::driver::core::{Action, Core, Event, Outcome, lasts};

mod log;
mod record;

use log::Log;

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

    /// The run's id, which names its record.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The run's record.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Runs the workflow to its outcome on `provider`. The outcome, or why the run stopped
    /// without one, is also in the record's `outcome.json`.
    ///
    /// # Errors
    ///
    /// Fails if the run was still going at its hard end ([`lasts`]), if the machine's clock
    /// reads before 1970, or if the record can't be written.
    pub fn execute(self, provider: &dyn Provider) -> Result<Outcome, String> {
        let outcome = self.run_on(provider);
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

    /// Records what `provider` declared, then starts the run's clock and drives the run, keeping
    /// its log.
    fn run_on(&self, provider: &dyn Provider) -> Result<Outcome, String> {
        record::write_json(&self.dir, "provider.json", &json!(provider.ready()))?;
        let clock = Clock::start();
        let unix = record::since_epoch(clock.unix)?;
        let mut runtime = Runtime {
            workflow: &self.workflow,
            provider,
            clock,
            log: Log::create(&self.dir)?,
            pending: VecDeque::new(),
        };
        let outcome = runtime.drive(unix);
        let noted = match &outcome {
            Ok(_) => Ok(()),
            Err(reason) => (runtime.log).note(clock.now(), &format!("the run failed: {reason}")),
        };
        failed_also(outcome, noted)
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
fn failed_also(
    outcome: Result<Outcome, String>,
    then: Result<(), String>,
) -> Result<Outcome, String> {
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
    provider: &'r dyn Provider,
    clock: Clock,
    log: Log,
    /// Events that performing actions produced, to feed the core next.
    pending: VecDeque<(Duration, Event)>,
}

impl Runtime<'_> {
    /// Drives the run from its start, `unix` after the Unix epoch, to its outcome.
    fn drive(&mut self, unix: Duration) -> Result<Outcome, String> {
        let (mut core, mut actions) = Core::start(self.workflow, self.startup());
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

    /// The longest the provider takes to start a sandbox.
    fn startup(&self) -> Duration {
        Duration::from_secs(self.provider.ready().capabilities.startup_seconds.get())
    }

    /// Waits for the next event: one that performing an action produced, or else the core's tick,
    /// once it is due. Nothing outside the run produces events yet, so the wait is a sleep until
    /// the tick's time by the run's clock, which `thread::sleep` does not cut short.
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
        thread::sleep(due.saturating_sub(self.clock.now()));
        (self.clock.now(), Event::Tick)
    }

    /// Performs the core's actions, in order: the outcome once one finishes the run.
    fn perform(&mut self, actions: Vec<Action>) -> Result<Option<Outcome>, String> {
        for action in actions {
            match action {
                Action::StartAgent => {
                    let reason = "this runtime starts no sandbox yet".to_owned();
                    let event = Event::AgentFailed { reason };
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
                Action::TearDownAgent => {
                    let note = "the agent's sandbox never started: nothing was collected";
                    self.log.note(self.clock.now(), note)?;
                }
                Action::Finish { outcome } => return Ok(Some(outcome)),
                Action::Abandon => {
                    return Err(format!(
                        "the run was still going at its hard end, {} s after it started: \
                         expected it to end within its deadline, its finalize grace, its final \
                         judge's timeout and its sandboxes' startups",
                        lasts(self.workflow, self.startup()).as_secs()
                    ));
                }
            }
        }
        Ok(None)
    }
}
