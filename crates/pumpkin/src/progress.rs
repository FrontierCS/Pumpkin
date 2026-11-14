//! What `ls` shows of runs ([`Listed`], [`lines`]): of a live run, its phase, how long it has
//! been in it, and the bound at which Pumpkin ends it, which a run's log and the bounds of its
//! workflow and classes give ([`runtime::Progress`]); of an ended run, how it ended and how long
//! it ran; and a line of totals. And the summary of a run that its ending says ([`summary`]).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::time::Duration;

use runtime::{
    CallEnd, CallSpan, Closed, Ending, LeaseCommand, LeaseSpan, Place, Progress, Record, Sandbox,
    Work,
};
use stdx::Utc;

use crate::next;

/// How long a phase may go on past its bound before `ls` says that it is a bug: the core acts at
/// the bound, and its act is logged at once, but the run's log is read over a connection and
/// clocks differ by seconds.
const PAST_BOUND_SLACK: Duration = Duration::from_mins(1);

/// How long after a run ended `ls` lists it without a prefix that names it: what is live, and
/// how the runs of the last day ended, are what a person asks it for.
const LISTED_FOR: Duration = Duration::from_hours(24);

/// Before when, since the Unix epoch, a run that ended is left out of `ls`, at `now`, given its
/// `prefixes`: none once a prefix names the runs to list.
pub(crate) fn listed_since(prefixes: &[String], now: Duration) -> Option<Duration> {
    prefixes.is_empty().then(|| now.saturating_sub(LISTED_FOR))
}

/// What `ls` lists: runs, by id, and the ids of the runs that it leaves out, as they ended before
/// [`listed_since`].
#[derive(Default)]
pub(crate) struct Listing {
    pub(crate) runs: BTreeMap<String, Listed>,
    pub(crate) older: BTreeSet<String>,
}

/// A run as `ls` lists it.
pub(crate) enum Listed {
    /// A job on the controller that builds the run, which `pumpkin run` asked for at `asked`,
    /// since the Unix epoch.
    Building { asked: Option<Duration> },
    /// A job on the controller that has ended, whose run has no record in the controller's runs
    /// directory.
    Unstarted,
    /// A run whose record is begun.
    Run(Box<Run>),
}

/// A run whose record is begun: its task, if its record names it yet, how it ended, `None`
/// while it is live, and what its log says, if it has one yet.
pub(crate) struct Run {
    task: Option<String>,
    ending: Option<Ending>,
    progress: Option<Progress>,
    asked: Option<Duration>,
}

impl Listed {
    /// The run of `record`, which ended as `ending` says, or is live.
    pub(crate) fn run(record: &Record, ending: Option<Ending>) -> Result<Self, String> {
        Ok(Self::Run(Box::new(Run {
            task: record.task()?,
            ending,
            progress: record.progress()?,
            asked: runtime::minted(record.id()),
        })))
    }

    /// Whether the run is live, or its job builds it.
    pub(crate) fn live(&self) -> bool {
        match self {
            Self::Run(run) => run.ending.is_none(),
            Self::Building { .. } => true,
            Self::Unstarted => false,
        }
    }

    /// Whether the run has ended with an `outcome.json`, which no later look changes.
    pub(crate) fn ended(&self) -> bool {
        match self {
            Self::Run(run) => !matches!(run.ending, None | Some(Ending::Lost)),
            Self::Building { .. } | Self::Unstarted => false,
        }
    }

    /// The state that `ls` counts the run in.
    fn state(&self) -> State {
        match self {
            Self::Building { .. } => State::Building,
            Self::Unstarted => State::Unstarted,
            Self::Run(run) => match &run.ending {
                None => State::Running,
                Some(Ending::Scored { .. } | Ending::Unscored(_) | Ending::NoResult { .. }) => {
                    State::Finished
                }
                Some(Ending::Failed(_)) => State::Failed,
                Some(Ending::Interrupted(_)) => State::Interrupted,
                Some(Ending::Lost) => State::Lost,
            },
        }
    }
}

/// A run's state, as `ls` names it, in the order in which its totals list them.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum State {
    Building,
    Running,
    Finished,
    Failed,
    Interrupted,
    Lost,
    Unstarted,
}

impl State {
    fn name(self) -> &'static str {
        match self {
            Self::Building => "building",
            Self::Running => "running",
            Self::Finished => "finished",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
            Self::Lost => "lost",
            Self::Unstarted => "unstarted",
        }
    }
}

/// The lines of `ls` of `listing`, its runs by id, so oldest first, and so the live ones, which
/// started last, nearest the line of totals that follows them, each ending in a newline, at `now`,
/// since the Unix epoch; then that line, which ends with the command that lists the runs left out,
/// unless there are none; `runs` is their runs directory, as commands name it.
pub(crate) fn lines(listing: &Listing, now: Duration, runs: Option<&str>) -> String {
    let mut lines = String::new();
    let mut totals = Totals::default();
    for (id, listed) in &listing.runs {
        totals.add(listed);
        lines.push_str(&line(id, listed, now, runs));
        lines.push('\n');
        lines.push_str(&under(id, listed, now, runs));
    }
    let mut parts = Vec::new();
    if !listing.runs.is_empty() {
        parts.push(totals.line());
    }
    if !listing.older.is_empty() {
        if listing.runs.is_empty() {
            parts.push("no run is live, or ended within a day".to_owned());
        }
        let (are, them) = match listing.older.len() {
            1 => ("isn't", "it"),
            _ => ("aren't", "them"),
        };
        parts.push(format!(
            "{} that ended over a day ago {are} listed: to list {them}: {}",
            crate::counted(listing.older.len(), "run"),
            next::of_run("ls", runs, &prefixes(&listing.older).join(" "))
        ));
    }
    if !parts.is_empty() {
        lines.push_str(&parts.join("; "));
        lines.push('\n');
    }
    lines
}

/// The prefixes that name `ids`, each a run's id, as `ls` takes them: the longest that starts
/// the days of them all, the first 8 digits of each id, or else each of those days.
fn prefixes(ids: &BTreeSet<String>) -> Vec<String> {
    let days: BTreeSet<&str> = ids.iter().map(|id| id.get(..8).unwrap_or(id)).collect();
    let mut common = days.first().copied().unwrap_or_default();
    for day in &days {
        let shared = common.chars().zip(day.chars()).take_while(|(a, b)| a == b);
        common = &common[..shared.map(|(c, _)| c.len_utf8()).sum::<usize>()];
    }
    if common.is_empty() {
        return days.into_iter().map(str::to_owned).collect();
    }
    vec![common.to_owned()]
}

/// What the runs that `ls` lists come to.
#[derive(Default)]
struct Totals {
    /// How many are in each state.
    states: BTreeMap<State, usize>,
    /// How many finished without a score.
    unscored: usize,
    /// When the last of the live runs ends at the latest, since the Unix epoch.
    ends: Option<Duration>,
    /// What the runs' sandboxes cost, in dollars, at their classes' prices, and whether some
    /// lasted in a class without one.
    usd: Option<f64>,
    unpriced: bool,
}

impl Totals {
    /// Counts `listed`.
    fn add(&mut self, listed: &Listed) {
        *self.states.entry(listed.state()).or_default() += 1;
        let Listed::Run(run) = listed else {
            return;
        };
        if let Some(Ending::Unscored(_) | Ending::NoResult { .. }) = run.ending {
            self.unscored += 1;
        }
        let Some(progress) = &run.progress else {
            return;
        };
        if run.ending.is_none() {
            let ends = progress.started.saturating_add(progress.hard_end);
            self.ends = self.ends.max(Some(ends));
        }
        match cost(progress) {
            Some((usd, unpriced)) => {
                *self.usd.get_or_insert(0.0) += usd;
                self.unpriced |= unpriced;
            }
            None => self.unpriced = true,
        }
    }

    fn line(&self) -> String {
        let runs = crate::counted(self.states.values().sum::<usize>(), "run");
        let mut states = Vec::new();
        for (state, count) in &self.states {
            let mut shown = format!("{count} {}", state.name());
            if *state == State::Finished && self.unscored > 0 {
                let _ = write!(shown, " ({} without a score)", self.unscored);
            }
            states.push(shown);
        }
        let mut line = format!("{runs}: {}", states.join(", "));
        if let Some(usd) = self.usd {
            let _ = write!(line, "; ${usd:.2} so far");
            if self.unpriced {
                line.push_str(", and more in classes without a price");
            }
        }
        if let Some(ends) = self.ends {
            let Utc { hour, minute, .. } = Utc::from_unix(ends.as_secs());
            let _ = write!(line, "; every live run ends by {hour:02}:{minute:02} UTC");
        }
        line
    }
}

/// What the sandboxes of the run whose log says `progress` cost, in dollars, at their classes'
/// prices, and whether some lasted in a class without one; `None` if no class has a price.
fn cost(progress: &Progress) -> Option<(f64, bool)> {
    (!progress.prices.is_empty()).then(|| progress.usd())
}

/// The line of `ls` of `listed`, run `id`, at `now`.
fn line(id: &str, listed: &Listed, now: Duration, runs: Option<&str>) -> String {
    let column = |state: &str, time: &str, bound: &str, usd: &str, rest: &str| {
        format!("{id} {state:<11} {time:>7}  {bound:<9} {usd:>7}  {rest}")
    };
    let state = listed.state().name();
    let since =
        |asked: &Option<Duration>| asked.map(|asked| column_span(now.saturating_sub(asked)));
    let run = match listed {
        Listed::Building { asked } => {
            return column(state, since(asked).as_deref().unwrap_or("-"), "", "-", "-");
        }
        Listed::Unstarted => {
            let why = format!(
                "-: its job ended before the run started: to see why: {}",
                next::of_run("wait", runs, id)
            );
            return column(state, "-", "", "-", &why);
        }
        Listed::Run(run) => run,
    };
    let task = run.task.as_deref().unwrap_or("-");
    let usd = match run.progress.as_ref().map(cost) {
        None | Some(None) => "-".to_owned(),
        Some(Some((usd, unpriced))) => {
            let more = if unpriced { "+" } else { "" };
            format!("${usd:.2}{more}")
        }
    };
    let Some(ending) = &run.ending else {
        let Some(progress) = &run.progress else {
            return column(
                "publishing",
                since(&run.asked).as_deref().unwrap_or("-"),
                "",
                &usd,
                task,
            );
        };
        let at = now.saturating_sub(progress.started);
        let cancel = next::of_run("cancel", runs, id);
        let (name, time, bound, doing) = shown(phase(progress, at), at, &cancel);
        return column(&name, &time, &bound, &usd, &format!("{task}: {doing}"));
    };
    let score = match ending {
        Ending::Scored { score, after: _ } => shown_score(score),
        Ending::Unscored(_)
        | Ending::NoResult { .. }
        | Ending::Failed(_)
        | Ending::Interrupted(_)
        | Ending::Lost => "-".to_owned(),
    };
    let ran =
        (run.progress.as_ref()).map(|progress| column_span(progress.end.unwrap_or(progress.last)));
    column(state, ran.as_deref().unwrap_or("-"), &score, &usd, task)
}

/// The lines of `ls` under the line of `listed`, run `id`, at `now`, each ending in a newline:
/// of a live run, one for each judge call of a grant and each lease that hasn't ended.
fn under(id: &str, listed: &Listed, now: Duration, runs: Option<&str>) -> String {
    let mut lines = String::new();
    let Listed::Run(run) = listed else {
        return lines;
    };
    let (None, Some(progress)) = (&run.ending, &run.progress) else {
        return lines;
    };
    let at = now.saturating_sub(progress.started);
    let cancel = next::of_run("cancel", runs, id);
    for child in children(progress, at) {
        let (name, time, bound, doing) = shown(child, at, &cancel);
        let _ = writeln!(
            lines,
            "{:24} {name:<11} {time:>7}  {bound:<9} {:>7}  {doing}",
            "", ""
        );
    }
    lines
}

/// What `ls` shows of `phase`, a live run's or one of its sandboxes', at `at`, since the run
/// started: its name, how long it has gone on, its bound, and what it does, with `cancel`, the
/// command that stops the run, if it is past its bound.
fn shown(phase: Phase, at: Duration, cancel: &str) -> (String, String, String, String) {
    let Phase {
        name,
        since,
        bound,
        mut doing,
    } = phase;
    let bound = bound.map(|bound| {
        if at > bound.saturating_add(PAST_BOUND_SLACK) {
            let _ = write!(
                doing,
                "; it is past its bound, which is a bug: to stop it: {cancel}"
            );
        }
        format!("of {}", span(bound.saturating_sub(since)))
    });
    let time = column_span(at.saturating_sub(since));
    (name, time, bound.unwrap_or_default(), doing)
}

/// Where a live run, or one of its sandboxes, is: the phase's name, since when, and the bound by
/// which Pumpkin ends it, each since the run started, if its record says, and what it does.
struct Phase {
    name: String,
    since: Duration,
    bound: Option<Duration>,
    doing: String,
}

/// The phase of the run whose log says `progress`, at `at`, since the run started.
fn phase(progress: &Progress, at: Duration) -> Phase {
    let Progress {
        hard_end,
        deadline,
        agent,
        calls,
        leases,
        model,
        work_end,
        ..
    } = progress;
    // Once the work phase has ended, the core ends the run by its hard end.
    if let Some(work_end) = *work_end {
        if let Some(call) = calls.iter().find(|call| call.grant.is_none()) {
            return Phase {
                name: "final judge".to_owned(),
                since: call.sandbox.asked,
                // The core collects what the final judge left by the hard end at the latest.
                bound: call_bound(call).map(|bound| bound.min(*hard_end)),
                doing: on(call),
            };
        }
        let agent = (agent.sandbox.gone.is_none()).then(|| "the agent's sandbox".to_owned());
        let calls = calls.iter().filter(|call| call.sandbox.gone.is_none());
        let calls = calls.map(|call| format!("job {}", call.job));
        let leases = leases.iter().filter(|lease| lease.sandbox.gone.is_none());
        let leases = leases.map(|lease| format!("lease {}", lease.lease));
        let waited: Vec<String> = agent.into_iter().chain(calls).chain(leases).collect();
        let doing = match waited.split_last() {
            None => "the final judge's start".to_owned(),
            Some((last, [])) => format!("the end of {last}"),
            Some((last, rest)) => format!("the end of {} and {last}", rest.join(", ")),
        };
        return Phase {
            name: "ending".to_owned(),
            since: work_end,
            bound: Some(*hard_end),
            doing,
        };
    }
    let on = placed(&agent.sandbox);
    let Some(started) = agent.sandbox.started else {
        return Phase {
            name: "starting".to_owned(),
            since: Duration::ZERO,
            bound: agent.sandbox.startup,
            doing: format!("the agent on {on}"),
        };
    };
    let requests = crate::counted(model.requests, "model request");
    // What wasn't good: each stated failure, and each status once one isn't a whole 2xx.
    let statuses = answered(model).filter(|_| model.not_2xx > 0);
    let not_good: Vec<String> = failed_upstream(model).into_iter().chain(statuses).collect();
    let not_good = not_good.join("; ");
    let doing = match (agent.routed, model.last_good) {
        (false, _) => format!("the agent on {on}"),
        (true, Some(last)) if not_good.is_empty() => format!(
            "{requests}, the last answered {} ago",
            span(at.saturating_sub(last))
        ),
        (true, Some(last)) => format!(
            "{requests}, {not_good}; the last good one answered {} ago",
            span(at.saturating_sub(last))
        ),
        (true, None) if model.requests == 0 => "no model request answered yet".to_owned(),
        (true, None) => format!("{requests}, {not_good}; no good one answered yet"),
    };
    Phase {
        name: "agent".to_owned(),
        since: started,
        bound: deadline.map(|deadline| started + deadline),
        doing,
    }
}

/// The phase of each judge call of a grant, and each lease, of the run whose log says
/// `progress`, that hasn't ended, at `at`, since the run started, in the order the core began
/// them: a ready lease's says how long of its time so far a command or transfer ran.
fn children(progress: &Progress, at: Duration) -> Vec<Phase> {
    let calls = progress.calls.iter().filter(|call| call.grant.is_some());
    let calls = calls
        .filter(|call| call.sandbox.gone.is_none())
        .map(|call| {
            let asked = call.sandbox.asked;
            let bound = call_bound(call);
            let grant = call.grant.as_deref().unwrap_or_default();
            let mut doing = format!("{grant} judge on {}", placed(&call.sandbox));
            if call.sandbox.started.is_none() {
                doing.push_str(", starting");
            }
            let phase = Phase {
                name: format!("job {}", call.job),
                since: asked,
                bound,
                doing,
            };
            ((asked, asked), phase)
        });
    let leases = progress.leases.iter();
    let leases = leases
        .filter(|lease| lease.sandbox.gone.is_none())
        .map(|lease| {
            let Sandbox {
                startup,
                asked,
                started,
                ..
            } = &lease.sandbox;
            let mut on = lease.grant.clone();
            if let Some(job) = lease.holder {
                let _ = write!(on, " of job {job}");
            }
            let _ = write!(on, " on {}", placed(&lease.sandbox));
            let (since, bound, mut doing) = match (lease.closed, started) {
                (Some((closed, _)), _) => {
                    let doing = format!("{on}, {}; its sandbox isn't gone yet", lease_end(lease));
                    (closed, None, doing)
                }
                (None, Some(ready)) => {
                    let bound = lease.ready_for.map(|ready_for| *ready + ready_for);
                    let busy = span(lease.busy(at));
                    (*ready, bound, format!("{on}, ready, {busy} busy"))
                }
                (None, None) => {
                    let bound = startup.map(|startup| *asked + startup);
                    (*asked, bound, format!("{on}, starting"))
                }
            };
            if let Some(last) = &lease.last {
                let _ = write!(doing, "; {}", command(last));
            }
            let phase = Phase {
                name: format!("lease {}", lease.lease),
                since,
                bound,
                doing,
            };
            (sorted(progress, lease), phase)
        });
    let mut children: Vec<((Duration, Duration), Phase)> = calls.chain(leases).collect();
    // By when they began, stably, so that a call and a lease begun at once keep the core's order.
    children.sort_by_key(|(begun, _)| *begun);
    children.into_iter().map(|(_, child)| child).collect()
}

/// By when, since the run started, judge call `call` ends at the latest: its command's timeout
/// after its sandbox started, or, before that, after its class's startup, if the record says.
fn call_bound(call: &CallSpan) -> Option<Duration> {
    let Sandbox {
        asked,
        startup,
        started,
        ..
    } = &call.sandbox;
    let started = started.or_else(|| startup.map(|startup| *asked + startup));
    started.map(|started| started + call.timeout)
}

/// When the sandbox of `lease`, of the run whose log says `progress`, comes among the run's: under
/// the judge call that holds it, if one does, by when the core began each.
fn sorted(progress: &Progress, lease: &LeaseSpan) -> (Duration, Duration) {
    let asked = lease.sandbox.asked;
    let call = (progress.calls.iter()).find(|call| Some(call.job) == lease.holder);
    (call.map_or(asked, |call| call.sandbox.asked), asked)
}

/// What a judge call does: its job, and where it runs.
fn on(call: &CallSpan) -> String {
    let starting = if call.sandbox.started.is_some() {
        ""
    } else {
        ", starting"
    };
    format!("job {} on {}{starting}", call.job, placed(&call.sandbox))
}

/// `place` as `<provider>/<class>`, or `-` if the record doesn't say.
fn place(place: Option<&Place>) -> String {
    place.map_or("-".to_owned(), |place| {
        format!("{}/{}", place.provider, place.class)
    })
}

/// Where `sandbox` is ([`place`]), with its GPUs once it has started ([`gpus`]).
fn placed(sandbox: &Sandbox) -> String {
    format!("{}{}", place(sandbox.place.as_ref()), gpus(sandbox))
}

/// The GPUs of `sandbox`, once it has started, as its `sandboxd` saw them, such as ` with NVIDIA
/// A100-SXM4-80GB`, and those its role needs where they differ, as ` (its role needs …)`; or
/// ` with unknown GPUs` where its role needs some and it saw none. Nothing for one whose role
/// needs none and that saw none.
fn gpus(sandbox: &Sandbox) -> String {
    let needed = (sandbox.needs.as_ref()).map(|gpu| {
        let count = usize::try_from(gpu.count.get()).unwrap_or(usize::MAX);
        vec![gpu.model.as_str(); count]
    });
    let seen: Option<Vec<&str>> =
        (sandbox.gpus.as_ref()).map(|gpus| gpus.iter().map(String::as_str).collect());
    match (sandbox.started, needed, seen) {
        (None, _, _) | (Some(_), None, None) => String::new(),
        (Some(_), Some(_), None) => " with unknown GPUs".to_owned(),
        (Some(_), needed, Some(seen)) => {
            let mut shown = format!(" with {}", models(&seen));
            if needed.as_ref() != Some(&seen) {
                let needed = needed.map_or("no GPU".to_owned(), |needed| models(&needed));
                let _ = write!(shown, " (its role needs {needed})");
            }
            shown
        }
    }
}

/// `models`, GPUs' models, each once with how many there are of it where there are more than
/// one, such as `2 × NVIDIA H100 80GB HBM3`.
fn models(models: &[&str]) -> String {
    let mut counted: Vec<(&str, usize)> = Vec::new();
    for &model in models {
        match counted.iter_mut().find(|(counted, _)| *counted == model) {
            Some((_, count)) => *count += 1,
            None => counted.push((model, 1)),
        }
    }
    let counted = counted.into_iter().map(|(model, count)| match count {
        1 => model.to_owned(),
        count => format!("{count} × {model}"),
    });
    counted.collect::<Vec<_>>().join(", ")
}

/// `score`, a JSON number, as every line that `pumpkin` says shows a score, for a glance: to 2
/// decimals from 1 on, to 4 below, without the zeros that end them; or as it is if it is no
/// number this reads. The outcome's JSON keeps the score exact.
pub(crate) fn shown_score(score: &str) -> String {
    let Ok(value) = score.parse::<f64>() else {
        return score.to_owned();
    };
    let decimals = if value.abs() >= 1.0 { 2 } else { 4 };
    let shown = format!("{value:.decimals$}");
    let shown = shown.trim_end_matches('0').trim_end_matches('.');
    if shown == "-0" { "0" } else { shown }.to_owned()
}

/// `span` to the second, for a glance, as a sentence says it: as `45s`, `3m05s`, or past an hour
/// `2h13m`, and without a last unit of zero, as `3m` or `2h`, which a column keeps
/// ([`column_span`]).
pub(crate) fn span(span: Duration) -> String {
    let column = column_span(span);
    let sentence = (column.strip_suffix("00s")).or_else(|| column.strip_suffix("00m"));
    sentence.map_or_else(|| column.clone(), str::to_owned)
}

/// `span` as a column of spans shows it, each with two units past a minute, as `3m00s` beside
/// `2m41s`, so that they line up.
fn column_span(span: Duration) -> String {
    let seconds = span.as_secs();
    let (hours, minutes, seconds) = (seconds / 3600, seconds % 3600 / 60, seconds % 60);
    match (hours, minutes, seconds) {
        (0, 0, seconds) => format!("{seconds}s"),
        (0, minutes, seconds) => format!("{minutes}m{seconds:02}s"),
        (hours, minutes, _) => format!("{hours}h{minutes:02}m"),
    }
}

/// The summary of the run of `record`, which has ended, that its ending says, its lines each
/// ending in a newline: when it started and how long it ran; when the agent started and how it
/// ended; each judge call, with how long it took to start and then ran, how it ended, and, of one
/// without a score, where the record holds its standard error and evidence, and each lease, with
/// how long its runner lived and was charged for, and busy, its commands and how it ended, in the
/// order the core began them, under a call its own model requests; the run's model requests, how
/// they were answered and the tokens reported; and the sandboxes of each place, how long they
/// lasted, and how many were deleted. `None` for a run that has no log.
pub(crate) fn summary(record: &Record) -> Result<Option<String>, String> {
    let Some(progress) = record.progress()? else {
        return Ok(None);
    };
    let ran = span(progress.end.unwrap_or(progress.last));
    let mut summary = match runtime::minted(record.id()) {
        Some(asked) => format!(
            "run {} started {} after it was asked for, and ran {ran}:\n",
            record.id(),
            span(progress.started.saturating_sub(asked))
        ),
        None => format!("run {} ran {ran}:\n", record.id()),
    };
    let agent = &progress.agent;
    let mut line = format!("agent on {}", placed(&agent.sandbox));
    if let Some(started) = agent.sandbox.started {
        let _ = write!(line, ", started at {}", clock(started));
    }
    match &agent.end {
        Some((at, runtime::AgentEnd::Exited(status))) => {
            let _ = write!(line, ", {} at {}", exited(status), clock(*at));
        }
        Some((at, runtime::AgentEnd::Failed(reason))) => {
            let reason = crate::escaped(reason);
            let _ = write!(line, ", failed at {}: {reason}", clock(*at));
        }
        None => {}
    }
    if let Some(work_end) = progress.work_end {
        let _ = write!(line, "; the work phase ended at {}", clock(work_end));
    }
    let _ = writeln!(summary, "  {:>7} {line}", clock(Duration::ZERO));
    let now = progress.end.unwrap_or(progress.last);
    let calls = (progress.calls.iter()).map(|call| {
        let asked = call.sandbox.asked;
        let mut line = called(call, record);
        // Its own model requests, under it, as its leases are.
        if call.model.requests > 0 {
            let _ = write!(line, "\n{:>9}   model: {}", "", model(&call.model));
        }
        ((asked, asked), line)
    });
    let leases = (progress.leases.iter()).map(|lease| {
        let step = if lease.holder.is_some() { "  " } else { "" };
        (
            sorted(&progress, lease),
            format!("{step}{}", leased(lease, now)),
        )
    });
    let mut children: Vec<((Duration, Duration), String)> = calls.chain(leases).collect();
    // By time, stably, so that a call and a lease begun at once keep the core's order.
    children.sort_by_key(|(begun, _)| *begun);
    for ((_, asked), line) in children {
        let _ = writeln!(summary, "  {:>7} {line}", clock(asked));
    }
    // A run whose roles have no route made no model request.
    if progress.agent.routed || progress.model.requests > 0 {
        let _ = writeln!(summary, "  model: {}", model(&progress.model));
    }
    let _ = writeln!(summary, "  sandboxes: {}", sandboxes(&progress));
    Ok(Some(summary))
}

/// A judge call's name: its judge, by the grant that the agent called it under, or the final
/// one, and its job's number, as `train judge job 0` or `final judge job 4`.
fn judge(call: &CallSpan) -> String {
    let which = call.grant.as_deref().unwrap_or("final");
    format!("{which} judge job {}", call.job)
}

/// A judge call's line of a summary: which ([`judge`]), how long its sandbox took to start once
/// asked for, as `wait` says its start, and then ran, or that it never started, and how it ended.
fn called(call: &CallSpan, record: &Record) -> String {
    let Some(at) = call.sandbox.gone else {
        return format!(
            "{} on {}, which never ended",
            judge(call),
            placed(&call.sandbox)
        );
    };
    let asked = call.sandbox.asked;
    let took = match call.sandbox.started {
        Some(started) => format!(
            "started after {}, ran {}",
            span(started.saturating_sub(asked)),
            span(at.saturating_sub(started))
        ),
        None => format!(
            "never started, ended after {}",
            span(at.saturating_sub(asked))
        ),
    };
    let mut line = format!("{} on {}, {took}: ", judge(call), placed(&call.sandbox));
    let Some(end) = &call.end else {
        line.push_str(STOPPED);
        return line;
    };
    let scored = match end {
        CallEnd::Scored(score) => {
            line.push_str(&shown_score(score));
            true
        }
        CallEnd::Unscored(returned) => {
            let _ = write!(
                line,
                "no score, returned {}",
                clipped(returned, RETURNED_SHOWN_CHARS)
            );
            false
        }
        CallEnd::Invalid(reason) => {
            let _ = write!(line, "an invalid result: {}", crate::escaped(reason));
            false
        }
        CallEnd::Failed(reason) => {
            let _ = write!(line, "failed: {}", crate::escaped(reason));
            false
        }
    };
    for (said, what) in [(call.stopped, "stopped"), (call.timed_out, "timed out")] {
        if said {
            let _ = write!(line, " ({what})");
        }
    }
    let below = |path: &std::path::Path| {
        let path = path.strip_prefix(record.dir()).unwrap_or(path);
        path.display().to_string()
    };
    let mut kept = Vec::new();
    if let Some(stderr) = call.stderr.as_deref().filter(|_| !scored) {
        kept.push(format!("standard error {}", below(stderr)));
    }
    if let Some(evidence) = call.evidence.as_deref().filter(|_| !scored) {
        kept.push(format!("evidence {}", below(evidence)));
    }
    if !kept.is_empty() {
        let _ = write!(line, "; its {}", kept.join(", its "));
    }
    line
}

/// A lease's line of a summary, at `now`, since the run started: which, how long its runner
/// lived and was charged for, and how much of that busy, its commands, and how it ended.
fn leased(lease: &LeaseSpan, now: Duration) -> String {
    let on = format!("{} on {}", lease_name(lease), placed(&lease.sandbox));
    let Some(gone) = lease.sandbox.gone else {
        return format!("{on}, which never ended");
    };
    let commands = usize::try_from(lease.commands).unwrap_or(usize::MAX);
    let mut line = format!(
        "{on}, {}: charged {}, {} of it busy, {}",
        span(gone.saturating_sub(lease.sandbox.asked)),
        span(lease.charged(now)),
        span(lease.busy(now)),
        crate::counted(commands, "command")
    );
    if let Some(last) = &lease.last {
        let _ = write!(line, ", the last {}", command(last));
    }
    let _ = write!(line, "; {}", lease_end(lease));
    line
}

/// A lease's name: its grant's, its number, and the judge call that holds it, if one does.
fn lease_name(lease: &LeaseSpan) -> String {
    let name = format!("{} lease {}", lease.grant, lease.lease);
    match lease.holder {
        Some(job) => format!("{name} of job {job}"),
        None => name,
    }
}

/// How a sandbox that the core never ended ended: deleted as the run stopped short of the core's
/// end.
const STOPPED: &str = "deleted as the run stopped";

/// How a lease ended, or that it hasn't.
fn lease_end(lease: &LeaseSpan) -> String {
    match (&lease.failed, lease.closed) {
        (Some(reason), _) => format!("failed: {}", crate::escaped(reason)),
        (None, Some((_, Closed::Asked))) => "closed by its holder".to_owned(),
        (None, Some((_, Closed::WorkEnded))) => "closed as the work phase ended".to_owned(),
        (None, Some((_, Closed::CallEnded))) => "closed as its call ended".to_owned(),
        (None, Some((_, Closed::Expired))) => "expired".to_owned(),
        (None, None) if lease.sandbox.gone.is_some() => STOPPED.to_owned(),
        (None, None) => "open".to_owned(),
    }
}

/// A lease's command or transfer, on one line ([`crate::escaped`]), and how it ended, or that it
/// runs.
fn command(command: &LeaseCommand) -> String {
    let shown = |text: &str, chars| clipped(&crate::escaped(text), chars);
    let what = match &command.work {
        Work::Command(argv) => format!("`{}`", shown(&argv.join(" "), COMMAND_SHOWN_CHARS)),
        Work::Transfer {
            direction,
            from,
            to,
        } => {
            let (from, to) = (shown(from, PATH_SHOWN_CHARS), shown(to, PATH_SHOWN_CHARS));
            // A put with `--replace` is logged as the direction `replace`.
            match direction.as_str() {
                "replace" => format!("put {from} to {to}, replacing it"),
                direction => format!("{direction} {from} to {to}"),
            }
        }
    };
    let Some(end) = &command.end else {
        return format!("{what} running");
    };
    let moved = &end["moved"];
    let ended = match end.as_str() {
        Some("timed_out") => "timed out".to_owned(),
        Some("unstarted") => "didn't start".to_owned(),
        Some(other) => format!("ended {other}"),
        None if end["exited"].is_object() => exited(&end["exited"]),
        None if moved.is_object() => match (moved["entries"].as_u64(), moved["bytes"].as_u64()) {
            (Some(entries), Some(bytes)) => format!(
                "moved {}, {}",
                crate::counted(entries, "entry"),
                crate::counted(bytes, "byte")
            ),
            _ => format!("ended {end}"),
        },
        None if end["failed"].is_string() => {
            let reason = end["failed"].as_str().unwrap_or_default();
            format!("failed: {}", crate::escaped(reason))
        }
        None => format!("ended {end}"),
    };
    format!("{what} {ended}")
}

/// The most characters of what a judge returned that a summary shows.
const RETURNED_SHOWN_CHARS: usize = 160;

/// The most characters of a lease's command that `ls` and a summary show.
const COMMAND_SHOWN_CHARS: usize = 60;

/// The most characters of each path of a lease's transfer that `ls` and a summary show.
const PATH_SHOWN_CHARS: usize = 40;

/// `text`, cut after `chars`, as `…` ends it then.
fn clipped(text: &str, chars: usize) -> String {
    match text.char_indices().nth(chars) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_owned(),
    }
}

/// How an agent's command ended, from its status as the log gives it.
fn exited(status: &serde_json::Value) -> String {
    match (status["code"].as_i64(), status["signal"].as_i64()) {
        (Some(code), _) => format!("exited {code}"),
        (None, Some(signal)) => format!("ended by signal {signal}"),
        (None, None) => format!("ended with status {status}"),
    }
}

/// The model requests' line of a summary.
fn model(model: &runtime::ModelUse) -> String {
    let requests = crate::counted(
        usize::try_from(model.requests).unwrap_or(usize::MAX),
        "request",
    );
    let mut line = match answered(model) {
        Some(answered) => format!("{requests}, {answered}"),
        None => requests,
    };
    if let Some(failed) = failed_upstream(model) {
        let _ = write!(line, "; {failed}");
    }
    let runtime::Tokens {
        reported,
        input,
        output,
        cached,
        reasoning,
    } = &model.tokens;
    if *reported == 0 {
        if model.requests > 0 {
            line.push_str("; no response reported its tokens");
        }
        return line;
    }
    let _ = write!(line, "; tokens {input} in");
    if let Some(cached) = cached {
        let _ = write!(line, " ({cached} cached)");
    }
    let _ = write!(line, ", {output} out");
    if let Some(reasoning) = reasoning {
        let _ = write!(line, " ({reasoning} reasoning)");
    }
    if *reported < model.requests {
        let responses = crate::counted(*reported, "response");
        let _ = write!(line, ", as {responses} reported them");
    }
    line
}

/// How `model`'s requests were answered, as `all answered 200` or `answered 30 200, 5 429`;
/// `None` if none was.
fn answered(model: &runtime::ModelUse) -> Option<String> {
    let answers: Vec<String> = (model.answers.iter())
        .map(|(answer, count)| format!("{count} {answer}"))
        .collect();
    match (model.answers.keys().next(), model.answers.len()) {
        (None, _) => None,
        (Some(answer), 1) => Some(format!("all answered {answer}")),
        _ => Some(format!("answered {}", answers.join(", "))),
    }
}

/// How many of `model`'s answers stated that they failed, and with which codes, as
/// `53 failed upstream: rate_limit_exceeded`; `None` if none did.
fn failed_upstream(model: &runtime::ModelUse) -> Option<String> {
    let count: u64 = model.failed.values().sum();
    let failed: Vec<(&Option<String>, &u64)> = model.failed.iter().collect();
    match failed[..] {
        [] => None,
        [(Some(code), _)] => Some(format!("{count} failed upstream: {code}")),
        [(None, _)] => Some(format!("{count} failed upstream, with no code")),
        _ => {
            let codes = failed.iter().map(|(code, count)| match code {
                Some(code) => format!("{count} {code}"),
                None => format!("{count} with no code"),
            });
            let codes: Vec<String> = codes.collect();
            Some(format!("{count} failed upstream: {}", codes.join(", ")))
        }
    }
}

/// The sandboxes' line of a summary: for each place, how many, how long they lasted, how long
/// its leases' runners were charged for, what they cost at its class's price, if it has one, and
/// how many the record says were created, and deleted.
fn sandboxes(progress: &Progress) -> String {
    let placed: Vec<String> = (progress.sandboxes().into_iter())
        .map(|(place, used)| {
            let mut placed = format!(
                "{} on {}/{}, {} in all",
                used.count,
                place.provider,
                place.class,
                span(used.lasted)
            );
            if let Some((charged, busy)) = used.charged {
                let _ = write!(placed, ", leases charged {}", span(charged));
                let _ = write!(placed, ", {} of it busy", span(busy));
            }
            if let (Some(price), Some(usd)) = (used.usd_per_hour, used.usd()) {
                let _ = write!(placed, ", ${usd:.2} at ${price} an hour");
            }
            let _ = match (used.created, used.deleted) {
                (0, _) => write!(placed, ", none created"),
                (created, deleted) if deleted == created => {
                    write!(placed, ", {created} created, all deleted")
                }
                (created, deleted) => write!(placed, ", {created} created, {deleted} deleted"),
            };
            placed
        })
        .collect();
    if placed.is_empty() {
        return "none".to_owned();
    }
    placed.join("; ")
}

/// `at`, since the run started, as a clock shows it: `m:ss`, or past an hour `h:mm:ss`.
fn clock(at: Duration) -> String {
    let seconds = at.as_secs();
    let (hours, minutes, seconds) = (seconds / 3600, seconds % 3600 / 60, seconds % 60);
    if hours == 0 {
        format!("{minutes}:{seconds:02}")
    } else {
        format!("{hours}:{minutes:02}:{seconds:02}")
    }
}

/// The milestones of the run whose log says `progress`, in the order it records them, each a
/// line that `wait` says as it follows the run: the agent's start, each lease's ask, its
/// runner's readiness and its end, each judge call's ask, its sandbox's start and its end, and
/// the work phase's end, each at its time since the run started.
pub(crate) fn milestones(progress: &Progress) -> Vec<String> {
    let mut milestones = Vec::new();
    let agent = &progress.agent.sandbox;
    if let Some(started) = agent.started {
        milestones.push((started, format!("the agent started on {}", placed(agent))));
    }
    // Before the calls, so that the final judge's, which its end, and its leases' ends, start,
    // comes after them. Not before the agent's start: an agent that exits at once has its exit
    // stamped by its session's reader, which may be before the loop stamps the start.
    if let Some(work_end) = progress.work_end {
        let work_end = work_end.max(agent.started.unwrap_or_default());
        milestones.push((work_end, "the work phase ended".to_owned()));
    }
    for lease in &progress.leases {
        let which = lease_name(lease);
        let on = place(lease.sandbox.place.as_ref());
        milestones.push((lease.sandbox.asked, format!("{which} asked on {on}")));
        if let Some(ready) = lease.sandbox.started {
            milestones.push((ready, format!("{which} ready{}", gpus(&lease.sandbox))));
        }
        if let Some(gone) = lease.sandbox.gone {
            milestones.push((gone, format!("{which} ended: {}", lease_end(lease))));
        }
    }
    for call in &progress.calls {
        let on = place(call.sandbox.place.as_ref());
        milestones.push((call.sandbox.asked, format!("{} asked on {on}", judge(call))));
        if let Some(started) = call.sandbox.started {
            let started_line = format!("{} started{}", judge(call), gpus(&call.sandbox));
            milestones.push((started, started_line));
        }
        if let Some(at) = call.sandbox.gone {
            let ended = match &call.end {
                Some(CallEnd::Scored(score)) => shown_score(score),
                Some(CallEnd::Unscored(_)) => "no score".to_owned(),
                Some(CallEnd::Invalid(_)) => "an invalid result".to_owned(),
                Some(CallEnd::Failed(reason)) => format!("failed: {}", crate::escaped(reason)),
                None => STOPPED.to_owned(),
            };
            milestones.push((at, format!("{} ended: {ended}", judge(call))));
        }
    }
    // By time, stably, so that a call asked and ended at once keeps its order.
    milestones.sort_by_key(|(at, _)| *at);
    (milestones.into_iter())
        .map(|(at, milestone)| format!("{:>7} {milestone}", clock(at)))
        .collect()
}
