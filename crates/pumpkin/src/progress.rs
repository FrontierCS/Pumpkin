//! The words that say what a run is doing or did, made of the `Progress` that `runtime` reads of
//! its record: the rows of `ls`, a run's summary and the milestones that following it says.
//!
//! `view` says of a run what these say, so the page and `ls` never differ. The words for what a
//! run cost are in [`cost`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::time::Duration;

use provider::UnixTime;
use runtime::{
    CallEnd, CallSpan, Closed, Direction, Ending, LeaseSpan, Place, Progress, Readout, Record,
    Sandbox, Unread, Work,
};
use schema::sandbox::ExitStatus;
use schema::sandbox::op::{CommandEnd, TransferEnd};
use stdx::Utc;

use crate::next;

pub(crate) mod cost;
mod model;

pub(crate) use cost::usd;
pub(crate) use model::median;
use model::{answered, failed_upstream, model};

/// How long a phase may go on past its bound before `ls` says that it is a bug: the core acts at
/// the bound, and its act is logged at once, but the run's log is read over a connection and
/// clocks differ by seconds.
const PAST_BOUND_SLACK: Duration = Duration::from_mins(1);

/// How long after a run ended `ls` lists it without a prefix that names it: what is live, and
/// how the runs of the last day ended, are what a person asks it for.
const LISTED_FOR: Duration = Duration::from_hours(24);

/// The time since the epoch before which an ended run isn't listed: none, if `prefixes` name runs.
pub(crate) fn listed_since(prefixes: &[String], now: UnixTime) -> Option<Duration> {
    prefixes
        .is_empty()
        .then(|| now.since_epoch().saturating_sub(LISTED_FOR))
}

/// The runs that `ls` lists, by id.
#[derive(Default)]
pub(crate) struct Listing {
    pub(crate) runs: BTreeMap<String, Listed>,
    /// The ids of the runs that ended before `listed_since`, which `ls` names in a line, not a row.
    pub(crate) older: BTreeSet<String>,
}

/// A run as `ls` shows it: a job of a controller that has yet to start the run, or a run that
/// has a record.
pub(crate) enum Listed {
    /// A job on the controller that builds the run, which `pumpkin run` asked for at `asked`,
    /// since the Unix epoch.
    Building { asked: Option<Duration> },
    /// A job on the controller that waits for a build slot, as [`Listed::Building`] says.
    Waiting { asked: Option<Duration> },
    /// A job on the controller that has ended, whose run has no record in the controller's runs
    /// directory.
    Unstarted,
    /// A run whose record is begun.
    Run(Box<Run>),
}

pub(crate) struct Run {
    task: Option<String>,
    ending: Option<Ending>,
    progress: Option<Progress>,
    asked: Option<Duration>,
}

impl Listed {
    /// The run of `record`, which says on the standard error what of its log this `pumpkin`
    /// can't read.
    pub(crate) fn run(record: &Record, ending: Option<Ending>) -> Result<Self, String> {
        let Readout { progress, unread } = record.progress()?;
        if let Some(unread) = self::unread(&unread) {
            crate::say(format_args!("run {}: {unread}", record.id()));
        }
        Ok(Self::of(record, record.task()?, ending, progress))
    }

    pub(crate) fn of(
        record: &Record,
        task: Option<String>,
        ending: Option<Ending>,
        progress: Option<Progress>,
    ) -> Self {
        Self::Run(Box::new(Run {
            task,
            ending,
            progress,
            asked: runtime::minted(record.id()),
        }))
    }

    /// Whether the run is live, or its job builds it.
    pub(crate) fn live(&self) -> bool {
        match self {
            Self::Run(run) => run.ending.is_none(),
            Self::Building { .. } | Self::Waiting { .. } => true,
            Self::Unstarted => false,
        }
    }

    /// Whether the run has ended with an `outcome.json`, which no later look changes: a lost run
    /// has none, so it is neither live nor ended.
    pub(crate) fn ended(&self) -> bool {
        match self {
            Self::Run(run) => !matches!(run.ending, None | Some(Ending::Lost)),
            Self::Building { .. } | Self::Waiting { .. } | Self::Unstarted => false,
        }
    }

    pub(crate) fn state(&self) -> State {
        match self {
            Self::Waiting { .. } => State::Waiting,
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

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum State {
    Waiting,
    Building,
    Running,
    Finished,
    Failed,
    Interrupted,
    Lost,
    Unstarted,
}

impl State {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
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
/// started last, nearest the line of totals that follows them, each live run with a line for
/// each of its judge calls and leases that hasn't ended. The totals line ends with the command
/// that lists the runs left out, unless there are none; `runs` is their runs directory, as
/// commands name it.
pub(crate) fn lines(listing: &Listing, now: UnixTime, runs: Option<&str>) -> String {
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

/// The prefixes that name `ids`: the characters of their days, the first eight of an id, that all
/// share, or else each day.
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

pub(crate) fn unread(unread: &[Unread]) -> Option<String> {
    let first = unread.first()?;
    Some(format!(
        "{} of its log can't be read by this pumpkin, the first at {}",
        crate::counted(unread.len(), "part"),
        crate::escaped(&first.to_string())
    ))
}

#[derive(Default)]
struct Totals {
    states: BTreeMap<State, usize>,
    unscored: usize,
    ends: Option<UnixTime>,
    usd: Option<f64>,
    unpriced: bool,
    unreported: u64,
}

impl Totals {
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
        match progress.cost() {
            Some(cost) => {
                *self.usd.get_or_insert(0.0) += cost.usd();
                self.unpriced |= !cost.unpriced_models.is_empty() || cost.unpriced_classes;
                self.unreported += cost.unreported;
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
            line.push_str(&cost::more(
                self.unpriced.then_some("without a price"),
                self.unreported,
            ));
        }
        if let Some(ends) = self.ends {
            let Utc { hour, minute, .. } = Utc::from_unix(ends.since_epoch().as_secs());
            let _ = write!(line, "; every live run ends by {hour:02}:{minute:02} UTC");
        }
        line
    }
}

fn line(id: &str, listed: &Listed, now: UnixTime, runs: Option<&str>) -> String {
    let Columns {
        state,
        time,
        bound,
        usd,
        rest,
    } = columns(id, listed, now, runs);
    format!("{id} {state:<11} {time:>7}  {bound:<9} {usd:>7}  {rest}")
}

/// The cells of a run's row in `ls`.
pub(crate) struct Columns {
    pub(crate) state: String,
    pub(crate) time: String,
    pub(crate) bound: String,
    pub(crate) usd: String,
    pub(crate) rest: String,
}

pub(crate) fn columns(id: &str, listed: &Listed, now: UnixTime, runs: Option<&str>) -> Columns {
    let column = |state: &str, time: &str, bound: &str, usd: &str, rest: &str| Columns {
        state: state.to_owned(),
        time: time.to_owned(),
        bound: bound.to_owned(),
        usd: usd.to_owned(),
        rest: rest.to_owned(),
    };
    let state = listed.state().name();
    let since = |asked: &Option<Duration>| {
        asked.map(|asked| column_span(now.since_epoch().saturating_sub(asked)))
    };
    let run = match listed {
        Listed::Building { asked } | Listed::Waiting { asked } => {
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
    let usd = match run.progress.as_ref().and_then(Progress::cost) {
        None => "-".to_owned(),
        Some(cost) => usd(cost.usd(), cost.partial()),
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
        let at = now.since(progress.started);
        let cancel = next::of_run("cancel", runs, id);
        let (name, time, bound, doing) = shown(phase(progress, at), at, &cancel);
        return column(&name, &time, &bound, &usd, &format!("{task}: {doing}"));
    };
    let ran =
        (run.progress.as_ref()).map(|progress| column_span(progress.end.unwrap_or(progress.last)));
    column(
        state,
        ran.as_deref().unwrap_or("-"),
        &score(ending),
        &usd,
        task,
    )
}

fn score(ending: &Ending) -> String {
    match ending {
        Ending::Scored { score, after: _ } => shown_score(score),
        Ending::Unscored(_)
        | Ending::NoResult { .. }
        | Ending::Failed(_)
        | Ending::Interrupted(_)
        | Ending::Lost => "-".to_owned(),
    }
}

fn under(id: &str, listed: &Listed, now: UnixTime, runs: Option<&str>) -> String {
    let mut lines = String::new();
    let Listed::Run(run) = listed else {
        return lines;
    };
    let (None, Some(progress)) = (&run.ending, &run.progress) else {
        return lines;
    };
    let at = now.since(progress.started);
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

/// A step of a live run. `since` and `bound` are times since the run started.
struct Phase {
    name: String,
    since: Duration,
    bound: Option<Duration>,
    doing: String,
}

fn phase(progress: &Progress, at: Duration) -> Phase {
    let flying = in_flight(&progress.model, at);
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
                doing: on(call) + &flying,
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
            doing: doing + &flying,
        };
    }
    let on = placed(&agent.sandbox);
    let Some(started) = agent.sandbox.started else {
        return Phase {
            name: "starting".to_owned(),
            since: Duration::ZERO,
            bound: agent.sandbox.startup,
            doing: format!("the agent on {on}") + &flying,
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
        doing: doing + &flying,
    }
}

fn in_flight(model: &runtime::ModelUse, at: Duration) -> String {
    let Some((request, since)) = (model.in_flight.iter()).min_by_key(|(_, since)| **since) else {
        return String::new();
    };
    let longest = span(at.saturating_sub(*since));
    match model.in_flight.len() {
        1 => format!("; model request {request} in flight for {longest}"),
        count => {
            format!("; {count} model requests in flight, the longest, {request}, for {longest}")
        }
    }
}

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
pub(crate) fn sorted(progress: &Progress, lease: &LeaseSpan) -> (Duration, Duration) {
    let asked = lease.sandbox.asked;
    let call = (progress.calls.iter()).find(|call| Some(call.job) == lease.holder);
    (call.map_or(asked, |call| call.sandbox.asked), asked)
}

fn on(call: &CallSpan) -> String {
    let starting = if call.sandbox.started.is_some() {
        ""
    } else {
        ", starting"
    };
    format!("job {} on {}{starting}", call.job, placed(&call.sandbox))
}

fn place(place: Option<&Place>) -> String {
    place.map_or("-".to_owned(), |place| {
        format!("{}/{}", place.provider, place.class)
    })
}

pub(crate) fn placed(sandbox: &Sandbox) -> String {
    format!("{}{}", place(sandbox.place.as_ref()), gpus(sandbox))
}

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

/// `score` with two decimals, four if it is below one, and no trailing zero; text that isn't a
/// number is kept as it is.
pub(crate) fn shown_score(score: &str) -> String {
    let Ok(value) = score.parse::<f64>() else {
        return score.to_owned();
    };
    let decimals = if value.abs() >= 1.0 { 2 } else { 4 };
    let shown = format!("{value:.decimals$}");
    let shown = shown.trim_end_matches('0').trim_end_matches('.');
    if shown == "-0" { "0" } else { shown }.to_owned()
}

/// [`column_span`] without a trailing `00s` or `00m`, such as `3m` or `2h`.
pub(crate) fn span(span: Duration) -> String {
    let column = column_span(span);
    let sentence = (column.strip_suffix("00s")).or_else(|| column.strip_suffix("00m"));
    sentence.map_or_else(|| column.clone(), str::to_owned)
}

/// `42s`, `3m07s` or `2h05m`: of a span below a minute, below an hour, and longer.
pub(crate) fn column_span(span: Duration) -> String {
    let seconds = span.as_secs();
    let (hours, minutes, seconds) = (seconds / 3600, seconds % 3600 / 60, seconds % 60);
    match (hours, minutes, seconds) {
        (0, 0, seconds) => format!("{seconds}s"),
        (0, minutes, seconds) => format!("{minutes}m{seconds:02}s"),
        (hours, minutes, _) => format!("{hours}h{minutes:02}m"),
    }
}

pub(crate) fn summary(record: &Record) -> Result<Option<String>, String> {
    Ok(summary_of(record, &record.progress()?))
}

/// The summary of what `readout` says of `record`'s run, or `None` if it says nothing.
pub(crate) fn summary_of(record: &Record, readout: &Readout) -> Option<String> {
    let Readout { progress, unread } = readout;
    let unread = self::unread(unread);
    let Some(progress) = progress else {
        return unread.map(|unread| format!("run {}: {unread}\n", record.id()));
    };
    let ran = span(progress.end.unwrap_or(progress.last));
    let mut summary = match runtime::minted(record.id()) {
        Some(asked) => format!(
            "run {} started {} after it was asked for, and ran {ran}:\n",
            record.id(),
            span(progress.started.since_epoch().saturating_sub(asked))
        ),
        None => format!("run {} ran {ran}:\n", record.id()),
    };
    let _ = writeln!(
        summary,
        "  {:>7} {}",
        clock(Duration::ZERO),
        agent_line(progress)
    );
    let now = progress.end.unwrap_or(progress.last);
    let cost = progress.cost();
    // Only a run that has a cost says what its tokens cost.
    let prices = cost.is_some().then_some(&progress.token_prices);
    let calls = (progress.calls.iter()).map(|call| {
        let asked = call.sandbox.asked;
        let mut line = called(call, record.dir());
        // Its own model requests, under it, as its leases are.
        if call.model.requests > 0 {
            let model = model(&call.model, prices);
            let _ = write!(line, "\n{:>9}   model: {model}", "");
        }
        ((asked, asked), line)
    });
    let leases = (progress.leases.iter()).map(|lease| {
        let step = if lease.holder.is_some() { "  " } else { "" };
        (
            sorted(progress, lease),
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
        let model = model(&progress.model, prices);
        let _ = writeln!(summary, "  model: {model}");
    }
    let _ = writeln!(summary, "  sandboxes: {}", sandboxes(progress));
    if let Some(cost) = &cost {
        let _ = writeln!(summary, "  cost: {}", cost::line(cost));
    }
    if let Some(unread) = unread {
        let _ = writeln!(summary, "  {unread}");
    }
    Some(summary)
}

pub(crate) fn agent_line(progress: &Progress) -> String {
    let agent = &progress.agent;
    let mut line = format!("agent on {}", placed(&agent.sandbox));
    if let Some(started) = agent.sandbox.started {
        let _ = write!(line, ", started at {}", clock(started));
    }
    match &agent.end {
        Some((at, runtime::AgentEnd::Exited(status))) => {
            let _ = write!(line, ", {} at {}", exited(*status), clock(*at));
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
    line
}

fn judge(call: &CallSpan) -> String {
    let which = call.grant.as_deref().unwrap_or("final");
    format!("{which} judge job {}", call.job)
}

/// The line of a judge's call; the paths of what it kept are shown below `dir`, the record's.
pub(crate) fn called(call: &CallSpan, dir: &std::path::Path) -> String {
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
        let path = path.strip_prefix(dir).unwrap_or(path);
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

/// The line of a lease; `now` is a time since the run started, by which an open one is charged.
pub(crate) fn leased(lease: &LeaseSpan, now: Duration) -> String {
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

fn command(work: &Work) -> String {
    let shown = |text: &str, chars| clipped(&crate::escaped(text), chars);
    let (what, ended) = match work {
        Work::Command { argv, end } => {
            let ended = end.map(|end| match end {
                CommandEnd::TimedOut => "timed out".to_owned(),
                CommandEnd::Unstarted => "didn't start".to_owned(),
                CommandEnd::Exited(status) => exited(status),
            });
            let what = format!("`{}`", shown(&argv.join(" "), COMMAND_SHOWN_CHARS));
            (what, ended)
        }
        Work::Transfer {
            direction,
            from,
            to,
            end,
        } => {
            let (from, to) = (shown(from, PATH_SHOWN_CHARS), shown(to, PATH_SHOWN_CHARS));
            let what = match direction {
                Direction::Put => format!("put {from} to {to}"),
                Direction::Replace => format!("put {from} to {to}, replacing it"),
                Direction::Get => format!("get {from} to {to}"),
            };
            let ended = end.as_ref().map(|end| match end {
                TransferEnd::Moved { entries, bytes } => format!(
                    "moved {}, {}",
                    crate::counted(*entries, "entry"),
                    crate::counted(*bytes, "byte")
                ),
                TransferEnd::Failed(reason) => format!("failed: {}", crate::escaped(reason)),
            });
            (what, ended)
        }
    };
    format!("{what} {}", ended.as_deref().unwrap_or("running"))
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

fn exited(status: ExitStatus) -> String {
    match status {
        ExitStatus::Code(code) => format!("exited {code}"),
        ExitStatus::Signal(signal) => format!("ended by signal {signal}"),
    }
}

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

/// A time since the run started, as `3:07` or `2:05:00`.
fn clock(at: Duration) -> String {
    let seconds = at.as_secs();
    let (hours, minutes, seconds) = (seconds / 3600, seconds % 3600 / 60, seconds % 60);
    if hours == 0 {
        format!("{minutes}:{seconds:02}")
    } else {
        format!("{hours}:{minutes:02}:{seconds:02}")
    }
}

/// The lines that say what the run has done so far, in the order of the time that each says.
pub(crate) fn milestones(progress: &Progress) -> Vec<String> {
    let mut milestones = Vec::new();
    let agent = &progress.agent.sandbox;
    if let Some(started) = agent.started {
        milestones.push((started, format!("the agent started on {}", placed(agent))));
    }
    // Before the leases and the calls, so that the stable sort keeps it ahead of what follows it
    // at the same instant: the ends of the leases it closes, then the final judge's ask, which
    // waits for them. Not before the agent's start: an agent that exits at once has its exit
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
