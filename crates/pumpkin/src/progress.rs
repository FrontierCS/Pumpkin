//! The words that say what a run is doing or did, made of the `Progress` that `runtime` reads of
//! its record: the rows of `ls`, a run's summary and the milestones that following it says.
//!
//! `view` says of a run what these say, so the page and `ls` never differ. The words for what a
//! run cost are in [`cost`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use provider::UnixTime;
use runtime::{
    CallEnd, CallOf, CallSpan, CallerName, Ending, LeaseSpan, Place, Progress, ReadError, Readout,
    Record, Sandbox, Writer,
};
use schema::ir::Name;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::next;

mod agent;
mod child;
pub(crate) mod cost;
mod model;
mod row;

pub(crate) use agent::{caller_lane, line as agent_line};
use child::{STOPPED, call_name, command, lease_end, lease_name, over_budget};
pub(crate) use child::{called, leased};
pub(crate) use cost::usd;
pub(crate) use model::median;
use model::model;
pub(crate) use row::Row;
use row::{Spent, Stage};

/// How long a phase may go on past its bound before `ls` says that it is a bug: the core acts at
/// the bound, and its act is logged at once, but the run's log is read over a connection and
/// clocks differ by seconds.
const PAST_BOUND_SLACK: Duration = Duration::from_mins(1);

/// How long after a run ended `ls` lists it without a prefix that names it: what is live, and
/// how the runs of the last day ended, are what a person asks it for.
const LISTED_FOR: Duration = Duration::from_hours(24);

/// The time before which an ended run isn't listed: none, if `prefixes` name runs.
pub(crate) fn listed_since(prefixes: &[String], now: UnixTime) -> Option<UnixTime> {
    let since = || UnixTime::after_epoch(now.since_epoch().saturating_sub(LISTED_FOR));
    prefixes.is_empty().then(since)
}

/// The runs that `ls` lists, by id.
#[derive(Default, Serialize, Deserialize)]
pub(crate) struct Listing {
    pub(crate) runs: BTreeMap<String, Entry>,
}

/// A run in a listing.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Entry {
    Row(Row),
    /// A run that ended before `listed_since`, which `ls` names in a line, not a row.
    Older,
}

impl Entry {
    pub(crate) fn row(&self) -> Option<&Row> {
        match self {
            Self::Row(row) => Some(row),
            Self::Older => None,
        }
    }
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
    /// A run whose record this `pumpkin` can't read, made by another build.
    Unread(Unread),
    /// A run whose record is begun.
    Run(Box<Run>),
}

/// The build that made a record that this `pumpkin` can't read.
pub(crate) enum Unread {
    /// A build shown as a [`runtime::Build`] is.
    Build(String),
    /// A build from before records named theirs.
    Older,
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
    pub(crate) fn run(record: &Record, ending: Option<Ending>) -> Result<Self, ReadError> {
        let Readout { progress, unread } = record.progress()?;
        if let Some(unread) = self::unread(&unread) {
            crate::say(format_args!("run {}: {unread}", record.id()));
        }
        Ok(Self::of(record, record.task()?, ending, progress))
    }

    /// The run in `dir`, whose record failed to read with `error`, if the record is in a form
    /// that this build doesn't read and another build made it; else `error`.
    pub(crate) fn unread(dir: &Path, error: ReadError) -> Result<Self, String> {
        match error {
            // A record that couldn't be read says nothing of its form, whoever made it.
            ReadError::Failed(error) => Err(error),
            ReadError::Form(error) => match runtime::writer(dir) {
                Ok(Writer::Other(build)) => Ok(Self::Unread(Unread::Build(build))),
                Ok(Writer::Older) => Ok(Self::Unread(Unread::Older)),
                // Only a bug or a damaged file makes this build fail to read its own record.
                Ok(Writer::This) | Err(_) => Err(error),
            },
        }
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
}

/// What a run is doing or how it ended, by name, and in the order that the totals list them.
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
    Unread,
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
            Self::Unread => "unread",
        }
    }
}

/// The lines of `ls` of `listing`, its runs by id, so oldest first, and so the live ones, which
/// started last, nearest the line of totals that follows them, each live run with a line for
/// each of its calls and leases that hasn't ended. The totals line ends with the command
/// that lists the runs left out, unless there are none; `runs` is their runs directory, as
/// commands name it.
pub(crate) fn lines(listing: &Listing, runs: Option<&str>) -> String {
    let mut lines = String::new();
    let mut totals = Totals::default();
    let mut older = Vec::new();
    for (id, entry) in &listing.runs {
        let row = match entry {
            Entry::Row(row) => row,
            Entry::Older => {
                older.push(id.as_str());
                continue;
            }
        };
        totals.add(row);
        lines.push_str(&line(id, &row.cells));
        lines.push('\n');
        lines.push_str(row.under());
    }
    let mut parts = Vec::new();
    let rows = !totals.states.is_empty();
    if rows {
        parts.push(totals.line());
    }
    if !older.is_empty() {
        if !rows {
            parts.push("no run is live, or ended within a day".to_owned());
        }
        let (are, them) = match older.len() {
            1 => ("isn't", "it"),
            _ => ("aren't", "them"),
        };
        parts.push(format!(
            "{} that ended over a day ago {are} listed: to list {them}: {}",
            crate::counted(older.len(), "run"),
            next::of_run("ls", runs, &prefixes(&older).join(" "))
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
fn prefixes(ids: &[&str]) -> Vec<String> {
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

pub(crate) fn unread(unread: &[runtime::Unread]) -> Option<String> {
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
    without_result: usize,
    ends_unix_seconds: Option<u64>,
    usd: Option<f64>,
    unpriced: bool,
    unreported: u64,
}

impl Totals {
    fn add(&mut self, row: &Row) {
        let Row {
            cells: _,
            stage,
            spent,
        } = row;
        *self.states.entry(stage.state()).or_default() += 1;
        match stage {
            Stage::Running {
                ends_unix_seconds,
                under: _,
            } => self.ends_unix_seconds = self.ends_unix_seconds.max(Some(*ends_unix_seconds)),
            Stage::Finished { result: false } => self.without_result += 1,
            Stage::Finished { result: true }
            | Stage::Waiting
            | Stage::Building
            | Stage::Unstarted
            | Stage::Failed
            | Stage::Interrupted
            | Stage::Lost
            | Stage::Unread
            | Stage::Publishing => {}
        }
        match spent {
            Spent::Unread => {}
            Spent::Unpriced => self.unpriced = true,
            Spent::Priced {
                usd,
                partly_unpriced,
                unreported_responses,
            } => {
                *self.usd.get_or_insert(0.0) += usd;
                self.unpriced |= partly_unpriced;
                self.unreported += unreported_responses;
            }
        }
    }

    fn line(&self) -> String {
        let runs = crate::counted(self.states.values().sum::<usize>(), "run");
        let mut states = Vec::new();
        for (state, count) in &self.states {
            let mut shown = format!("{count} {}", state.name());
            if *state == State::Finished && self.without_result > 0 {
                let _ = write!(shown, " ({} without a result)", self.without_result);
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
        if let Some(ends) = self.ends_unix_seconds {
            let ends = crate::utc(Duration::from_secs(ends)).strftime("%H:%M");
            let _ = write!(line, "; every live run ends by {ends} UTC");
        }
        line
    }
}

fn line(id: &str, cells: &Columns) -> String {
    let Columns {
        state,
        time,
        bound,
        usd,
        rest,
    } = cells;
    format!("{id} {state:<11} {time:>7}  {bound:<9} {usd:>7}  {rest}")
}

/// The cells of a run's row in `ls`.
#[derive(Serialize, Deserialize)]
pub(crate) struct Columns {
    pub(crate) state: String,
    pub(crate) time: String,
    pub(crate) bound: String,
    pub(crate) usd: String,
    pub(crate) rest: String,
}

fn columns(id: &str, listed: &Listed, state: State, now: UnixTime, runs: Option<&str>) -> Columns {
    let column = |state: &str, time: &str, bound: &str, usd: &str, rest: &str| Columns {
        state: state.to_owned(),
        time: time.to_owned(),
        bound: bound.to_owned(),
        usd: usd.to_owned(),
        rest: rest.to_owned(),
    };
    let state = state.name();
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
        Listed::Unread(unread) => {
            let made = match unread {
                Unread::Build(build) => format!("made by pumpkin {build}"),
                Unread::Older => "made by an older pumpkin".to_owned(),
            };
            return column(state, "-", "", "-", &made);
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
    let rest = match ending {
        Ending::Returned { values } if !values.is_empty() => {
            format!("{task}: {}", shown_values(values))
        }
        Ending::Returned { .. } | Ending::NoResult(_) | Ending::Stopped(_) | Ending::Lost => {
            task.to_owned()
        }
    };
    column(state, ran.as_deref().unwrap_or("-"), "-", &usd, &rest)
}

/// The lines under a live run that has begun its log, each ending in a newline: one for each of
/// its calls and leases that hasn't ended.
fn under(id: &str, progress: &Progress, now: UnixTime, runs: Option<&str>) -> String {
    let mut lines = String::new();
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
        agents,
        calls,
        leases,
        work_end,
        ..
    } = progress;
    // Once the work phase has ended, the core ends the run by its hard end.
    if let Some(work_end) = *work_end {
        if let Some(call) = calls.iter().find(|call| call.of == CallOf::Final) {
            return Phase {
                name: "final call".to_owned(),
                since: call.sandbox.asked,
                // The core collects what the final call left by the hard end at the latest.
                bound: call_bound(call).map(|bound| bound.min(*hard_end)),
                doing: on(call) + &flying,
            };
        }
        let agents = agents
            .values()
            .filter(|agent| agent.sandbox.gone.is_none())
            .map(|agent| format!("{}'s sandbox", agent.role));
        let calls = calls.iter().filter(|call| call.sandbox.gone.is_none());
        let calls = calls.map(|call| format!("job {}", call.job));
        let leases = leases.iter().filter(|lease| lease.sandbox.gone.is_none());
        let leases = leases.map(|lease| format!("lease {}", lease.lease));
        let waited: Vec<String> = agents.chain(calls).chain(leases).collect();
        let doing = match waited.split_last() {
            None => "the final call's start".to_owned(),
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
    agent::work(progress, at)
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
    let calls = progress.calls.iter().filter_map(|call| match &call.of {
        CallOf::Function(function) => Some((call, function)),
        CallOf::Final => None,
    });
    let calls = calls
        .filter(|(call, _)| call.sandbox.gone.is_none())
        .map(|(call, function)| {
            let asked = call.sandbox.asked;
            let bound = call_bound(call);
            let mut doing = format!("{function} call on {}", placed(&call.sandbox));
            if let Some(caller) = call.caller {
                let _ = write!(doing, " for {}", caller_lane(caller));
            }
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
            let _ = write!(on, " of {}", caller_lane(lease.holder));
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
    let members = agent::children(progress, at);
    let mut children: Vec<((Duration, Duration), Phase)> =
        members.chain(calls).chain(leases).collect();
    // By when they began, stably, so that a call and a lease begun at once keep the core's order.
    children.sort_by_key(|(begun, _)| *begun);
    children.into_iter().map(|(_, child)| child).collect()
}

/// By when, since the run started, call `call` ends at the latest: its timeout after its sandbox
/// started, or, before that, after its class's startup, if the record says.
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
/// the call that holds it, if one does, by when the core began each.
pub(crate) fn sorted(progress: &Progress, lease: &LeaseSpan) -> (Duration, Duration) {
    let asked = lease.sandbox.asked;
    let call = (progress.calls.iter()).find(|call| CallerName::Job(call.job) == lease.holder);
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

/// `values` as `name=value`, separated by spaces: a number with two decimals, four if it is below
/// one, and no trailing zero; any other value as compact JSON.
pub(crate) fn shown_values(values: &[(Name, Value)]) -> String {
    let shown = values.iter().map(|(name, value)| match value.as_f64() {
        Some(number) => {
            let decimals = if number.abs() >= 1.0 { 2 } else { 4 };
            let shown = format!("{number:.decimals$}");
            let shown = shown.trim_end_matches('0').trim_end_matches('.');
            format!("{name}={}", if shown == "-0" { "0" } else { shown })
        }
        None => format!("{name}={value}"),
    });
    shown.collect::<Vec<_>>().join(" ")
}

/// What a call's result shows, or `a result` if it shows nothing.
fn result(values: &[(Name, Value)]) -> String {
    if values.is_empty() {
        "a result".to_owned()
    } else {
        shown_values(values)
    }
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
    let readout = record.progress()?;
    Ok(summary_of(record, &readout))
}

/// The summary of what `readout` says of `record`'s run, or `None` if it says nothing.
pub(crate) fn summary_of(record: &Record, readout: &Readout) -> Option<String> {
    let Readout { progress, unread } = readout;
    let unread = self::unread(unread);
    let Some(progress) = progress else {
        return unread.map(|unread| format!("run {}: {unread}\n", record.id()));
    };
    let now = progress.end.unwrap_or(progress.last);
    let ran = span(now);
    let mut summary = match runtime::minted(record.id()) {
        Some(asked) => format!(
            "run {} started {} after it was asked for, and ran {ran}:\n",
            record.id(),
            span(progress.started.since_epoch().saturating_sub(asked))
        ),
        None => format!("run {} ran {ran}:\n", record.id()),
    };
    let prices = &progress.token_prices;
    agent::summarize(&mut summary, progress);
    shared_summary(&mut summary, progress);
    let calls = (progress.calls.iter()).map(|call| {
        let asked = call.sandbox.asked;
        let mut line = called(call);
        // Its own model requests, under it, as its leases are.
        if call.model.requests > 0 {
            let model = model(&call.model, prices);
            let _ = write!(line, "\n{:>9}   model: {model}", "");
        }
        ((asked, asked), line)
    });
    let leases = (progress.leases.iter()).map(|lease| {
        let step = if matches!(lease.holder, CallerName::Job(_)) {
            "  "
        } else {
            ""
        };
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
    if progress.agents.values().any(|agent| agent.model.is_some()) || progress.model.requests > 0 {
        let model = model(&progress.model, prices);
        let _ = writeln!(summary, "  model: {model}");
    }
    match record.ir() {
        Ok(ir) => {
            for line in ir.iter().flat_map(|ir| cost::budgets(progress, ir)) {
                let _ = writeln!(summary, "  {line}");
            }
        }
        Err(error) => {
            let error = crate::escaped(&error.to_string());
            let _ = writeln!(summary, "  its budgets can't be read: {error}");
        }
    }
    let _ = writeln!(summary, "  sandboxes: {}", sandboxes(progress));
    let cost = progress.cost().map_or_else(
        || "unavailable (no priced usage)".to_owned(),
        |cost| cost::line(&cost, &progress.model),
    );
    let _ = writeln!(summary, "  estimated cost: {cost}");
    if let Some(unread) = unread {
        let _ = writeln!(summary, "  {unread}");
    }
    Some(summary)
}

fn shared_summary(summary: &mut String, progress: &Progress) {
    for service in progress.shared.values() {
        let _ = writeln!(
            summary,
            "  {:>7} {}",
            clock(service.sandbox.asked),
            work_target_line(service)
        );
    }
}

pub(crate) fn work_target_line(service: &runtime::SharedSpan) -> String {
    let state = if let Some(reason) = &service.failure {
        format!("failed: {}", crate::escaped(reason))
    } else if service.sandbox.gone.is_some() {
        "ended".to_owned()
    } else if service.sandbox.started.is_some() {
        "running".to_owned()
    } else {
        "starting".to_owned()
    };
    format!(
        "work-lifetime target {} on {}: {state}",
        service.role,
        placed(&service.sandbox)
    )
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
                let _ = write!(placed, ", {} at ${price} an hour", cost::dollars(usd));
            } else {
                placed.push_str(", no hourly rate");
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
    for agent in progress.agents.values() {
        if let Some(at) = agent.sandbox.started {
            let on = place(agent.sandbox.place.as_ref());
            milestones.push((at, format!("{} started on {on}", agent.role)));
        }
    }
    // Ahead of the ends of the leases it closes and the final call's ask at the same instant.
    if let Some(work_end) = progress.work_end {
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
        milestones.push((
            call.sandbox.asked,
            format!("{} asked on {on}", call_name(call)),
        ));
        if let Some(started) = call.sandbox.started {
            let started_line = format!("{} started{}", call_name(call), gpus(&call.sandbox));
            milestones.push((started, started_line));
        }
        if let Some(at) = call.sandbox.gone {
            let ended = match &call.end {
                Some((CallEnd::Result(values), _)) => result(values),
                Some((CallEnd::Failed(reason), _)) => format!("failed: {}", crate::escaped(reason)),
                None => STOPPED.to_owned(),
            };
            let over_budget = over_budget(call);
            milestones.push((
                at,
                format!("{} ended: {ended}{over_budget}", call_name(call)),
            ));
        }
    }
    // By time, stably, so that a call asked and ended at once keeps its order.
    milestones.sort_by_key(|(at, _)| *at);
    milestones
        .into_iter()
        .map(|(at, milestone)| format!("{:>7} {milestone}", clock(at)))
        .collect()
}
