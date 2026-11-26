//! A run as a listing holds it, made where the run's record is: what `ls` and the page's list say
//! of the run at the time the row was made. `ls`'s lines, `ls --json` and the page each show a
//! row, and none of them adds a fact of its own.

use std::fmt::Write as _;
use std::time::Duration;

use members::Member;
use provider::UnixTime;
use runtime::{Ending, Progress, Stopped};
use schema::ir::Name;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Listed, Phase, Run, State, Unread, children, phase};
use crate::next;

/// A run in a listing, made at one time, with its commands naming where its runs are.
#[derive(Serialize, Deserialize)]
pub(crate) struct Row {
    pub(crate) id: String,
    /// The member whose job the run is, on a controller whose job names one.
    pub(crate) member: Option<Member>,
    /// The run's workflow's name, once its record has an `ir.json`.
    pub(crate) task: Option<String>,
    pub(crate) state: State,
    /// What a live run, or a job yet to start one, is doing.
    pub(crate) phase: Option<Step>,
    /// Under a live run's phase, each of its agents, calls and leases that hasn't ended, in the
    /// order they began.
    pub(crate) steps: Vec<Step>,
    /// How long an ended run ran, if its record says.
    pub(crate) ran_seconds: Option<u64>,
    /// What the run cost, once its log has begun.
    pub(crate) spent: Option<Spent>,
    /// How many model requests the run made, once its log has begun.
    pub(crate) requests: Option<u64>,
    /// The tokens of the run's model requests, once its log has begun.
    pub(crate) tokens: Option<Tokens>,
    /// The values that a finished run's final call returned, in their order. A run that finished
    /// without a result has none.
    pub(crate) result: Option<Vec<Returned>>,
    /// What a row says in place of a phase or a result. It says why a job's run never started, or
    /// which build made a record that this one can't read.
    pub(crate) note: Option<String>,
    /// What of the run's log this `pumpkin` can't read, as `progress::unread` says it, if any.
    pub(crate) unread: Option<String>,
}

/// A step of a live run: its phase, or one of its agents, calls and leases.
#[derive(Serialize, Deserialize)]
pub(crate) struct Step {
    /// The phase's name, such as `work`, or the step's, such as `job 0`.
    pub(crate) name: String,
    /// How long the step has lasted, if the record says since when.
    pub(crate) seconds: Option<u64>,
    /// How long the step may last in all, if it has a bound.
    pub(crate) bound_seconds: Option<u64>,
    /// Whether the step has gone on past its bound, which is a bug. `doing` then ends with the
    /// command that stops the run.
    pub(crate) past_bound: bool,
    /// What the step is doing, in words. It is empty if there is nothing to add to its name.
    pub(crate) doing: String,
}

/// What a run cost in US dollars, as far as its record says.
#[derive(Clone, Copy, Serialize, Deserialize)]
pub(crate) struct Spent {
    /// The cost, or `None` if no part of it has a price.
    pub(crate) usd: Option<f64>,
    /// Whether `usd` leaves a part out, as [`runtime::Cost::partial`] says: it is then a lower
    /// bound.
    pub(crate) partial: bool,
}

/// The tokens of a run's model requests. `input` and `output` are `None` if requests were made and
/// no response reported its tokens, which is not none used.
#[derive(Serialize, Deserialize)]
pub(crate) struct Tokens {
    pub(crate) input: Option<u64>,
    pub(crate) output: Option<u64>,
    pub(crate) cached: Option<u64>,
}

/// A value of a run's result, by its name.
#[derive(Serialize, Deserialize)]
pub(crate) struct Returned {
    pub(crate) name: Name,
    pub(crate) value: Value,
}

impl Row {
    /// The row of run `id`, `listed`, at `now`, with its commands naming where `at` says.
    pub(crate) fn of(id: &str, listed: &Listed, now: UnixTime, at: &next::At) -> Self {
        let age = |asked: Option<Duration>| {
            asked.map(|asked| now.since_epoch().saturating_sub(asked).as_secs())
        };
        let (state, phase, note) = match listed {
            Listed::Waiting { waiting_since } => {
                let waited = now.since(*waiting_since).as_secs();
                let phase = unbounded("waiting", Some(waited), "for a build slot");
                (State::Waiting, Some(phase), None)
            }
            Listed::Building { held_since } => {
                let phase = match held_since {
                    Some(since) => {
                        let held = now.since(*since).as_secs();
                        unbounded("building", Some(held), "holding a build slot")
                    }
                    None => unbounded("building", age(runtime::minted(id)), ""),
                };
                (State::Building, Some(phase), None)
            }
            Listed::Unstarted => {
                let note = format!(
                    "its job ended before the run started: to see why: {}",
                    next::of_run("wait", at, id)
                );
                (State::Unstarted, None, Some(note))
            }
            Listed::Unread(unread) => {
                let note = match unread {
                    Unread::Build(build) => format!("made by pumpkin {build}"),
                    Unread::Older => "made by an older pumpkin".to_owned(),
                };
                (State::Unread, None, Some(note))
            }
            Listed::Run(run) => return Self::of_run(id, run, now, at),
        };
        Self {
            phase,
            note,
            ..Self::new(id, state)
        }
    }

    /// The row of `run`, whose record is begun, as [`Row::of`] makes it.
    fn of_run(id: &str, run: &Run, now: UnixTime, at: &next::At) -> Self {
        let mut row = Self::new(id, State::Running);
        row.task.clone_from(&run.task);
        let progress = run.progress.as_ref();
        let used = run.used.as_ref().filter(|_| run.ending.is_some());
        // What an ended run used is what its driver recorded at its end, where it did. A lost run,
        // or a reap's record of one, says it only in its log.
        match (used, progress) {
            (Some(used), _) => {
                row.ran_seconds = Some(used.ran.as_secs());
                row.spent = Some(Spent {
                    usd: used.usd,
                    partial: used.partial,
                });
                row.requests = Some(used.model_requests);
                row.tokens = Some(Tokens::of(&used.tokens, used.model_requests));
            }
            (None, Some(progress)) => {
                if run.ending.is_some() {
                    let ran = progress.end.unwrap_or(progress.last);
                    row.ran_seconds = Some(ran.as_secs());
                }
                row.spent = Some(Spent::of(progress));
                row.requests = Some(progress.model.requests);
                row.tokens = Some(Tokens::of(&progress.model.tokens, progress.model.requests));
            }
            (None, None) => {}
        }
        let Some(ending) = &run.ending else {
            let Some(progress) = progress else {
                let asked = run
                    .asked
                    .map(|asked| now.since_epoch().saturating_sub(asked));
                let asked = asked.map(|age| age.as_secs());
                row.phase = Some(unbounded("publishing", asked, ""));
                return row;
            };
            let at_run = now.since(progress.started);
            let cancel = next::of_run("cancel", at, id);
            row.phase = Some(step(phase(progress, at_run), at_run, &cancel));
            row.steps = (children(progress, at_run).into_iter())
                .map(|child| step(child, at_run, &cancel))
                .collect();
            return row;
        };
        row.state = match ending {
            Ending::Returned { values } => {
                let values = values.iter().map(|(name, value)| Returned {
                    name: name.clone(),
                    value: value.clone(),
                });
                row.result = Some(values.collect());
                State::Finished
            }
            Ending::NoResult(_) => State::Finished,
            Ending::Stopped(Stopped::Failed(_)) => State::Failed,
            Ending::Stopped(Stopped::Interrupted(_)) => State::Interrupted,
            Ending::Lost => State::Lost,
        };
        row
    }

    /// The row of run `id` in `state` that says nothing else.
    fn new(id: &str, state: State) -> Self {
        Self {
            id: id.to_owned(),
            member: None,
            task: None,
            state,
            phase: None,
            steps: Vec::new(),
            ran_seconds: None,
            spent: None,
            requests: None,
            tokens: None,
            result: None,
            note: None,
            unread: None,
        }
    }

    /// Whether the run is live, or its job is yet to start it.
    pub(crate) fn live(&self) -> bool {
        self.state.live()
    }
}

/// A step that has lasted `seconds`, with no bound.
fn unbounded(name: &str, seconds: Option<u64>, doing: &str) -> Step {
    Step {
        name: name.to_owned(),
        seconds,
        bound_seconds: None,
        past_bound: false,
        doing: doing.to_owned(),
    }
}

/// How long a phase may go on past its bound before `ls` says that it is a bug. The core acts at
/// the bound, and its act is logged at once. But the run's log is read over a connection, and
/// clocks differ by seconds.
const PAST_BOUND_SLACK: Duration = Duration::from_mins(1);

/// `phase` as a step at `at`, a time since the run started. If the step is past its bound, its
/// words end with `cancel`, the command that stops the run.
fn step(phase: Phase, at: Duration, cancel: &str) -> Step {
    let Phase {
        name,
        since,
        bound,
        mut doing,
    } = phase;
    let past_bound = bound.is_some_and(|bound| at > bound.saturating_add(PAST_BOUND_SLACK));
    if past_bound {
        if !doing.is_empty() {
            doing.push_str("; ");
        }
        let _ = write!(
            doing,
            "it is past its bound, which is a bug: to stop it: {cancel}"
        );
    }
    let bound_seconds = bound.map(|bound| bound.saturating_sub(since).as_secs());
    Step {
        name,
        seconds: Some(at.saturating_sub(since).as_secs()),
        bound_seconds,
        past_bound,
        doing,
    }
}

impl Spent {
    fn of(progress: &Progress) -> Self {
        match progress.cost() {
            None => Self {
                usd: None,
                partial: true,
            },
            Some(cost) => Self {
                usd: Some(cost.usd()),
                partial: cost.partial(),
            },
        }
    }
}

impl Tokens {
    /// The tokens of `requests` model requests, as `tokens` counts them.
    pub(crate) fn of(tokens: &runtime::Tokens, requests: u64) -> Self {
        let runtime::Tokens {
            reported,
            input,
            output,
            cached,
            cache_write: _,
            reasoning: _,
        } = tokens;
        let known = *reported > 0 || requests == 0;
        Self {
            input: known.then_some(*input),
            output: known.then_some(*output),
            cached: *cached,
        }
    }
}
