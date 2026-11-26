//! What `ls` prints of a listing: its rows as lines, or as JSON.
//!
//! The lines give the live runs first, each with its steps, then the runs that ended, the newest
//! first, then a line of totals. A pipe gets them plain. A terminal gets them styled: each state
//! in its colour, and each step's time against its bound as a bar. JSON gives each row whole.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::time::Duration;

use anstyle::{AnsiColor, Style};
use serde::Serialize;
use serde_json::Value;

use super::{
    Entry, Listing, RESULT_SHOWN_BYTES, Row, Spent, State, Step, Unlisted, column_span,
    number_shown, span, usd,
};
use crate::next;

/// How many of the runs that ended within the last day `ls` shows without `--all` or a prefix,
/// the newest. With a live run's steps and the totals, they fit a terminal of 24 rows.
const ENDED_SHOWN: usize = 5;

/// How many cells a step's bar takes, and the halves of a cell that it fills.
const BAR_CELLS: usize = 10;
const BAR_HALVES: u64 = 20;
const _: () = assert!(BAR_HALVES == 2 * BAR_CELLS as u64);

/// Which of a listing's runs `ls` shows.
#[derive(Clone, Copy)]
pub(crate) enum Scope {
    /// The runs that are live or ended within the last day: each live one, and the newest
    /// [`ENDED_SHOWN`] of those that ended.
    Day,
    /// Each run of the listing, as `--all` or a prefix lists them.
    Every,
}

/// How `ls` lays its lines out.
pub(crate) struct Layout {
    pub(crate) scope: Scope,
    /// How many columns a line may take, which the values of a result are fitted into. `None` is
    /// no bound, as off a terminal.
    pub(crate) width: Option<usize>,
    /// Whether the lines are styled for a terminal.
    pub(crate) styled: bool,
}

/// The lines of `ls` of `listing`, laid out as `layout` says, their commands naming the runs
/// where `at` says. A listing of no run gives none.
pub(crate) fn lines(listing: &Listing, at: &next::At, layout: &Layout) -> String {
    let rows: Vec<&Row> = listing.runs.values().rev().filter_map(Entry::row).collect();
    let older = (listing.runs.values())
        .filter(|entry| matches!(entry, Entry::Older))
        .count();
    let (live, ended): (Vec<&Row>, Vec<&Row>) = rows.iter().partition(|row| row.live());
    let members: BTreeSet<String> = rows.iter().map(|row| member(row)).collect();
    // A column that repeats one name says nothing: a machine's own runs name none, and a
    // controller of one member names it in every row.
    let membered = members.len() > 1;
    let shown = match layout.scope {
        Scope::Day => &ended[..ended.len().min(ENDED_SHOWN)],
        Scope::Every => &ended[..],
    };
    let paint = Paint(layout.styled);
    let mut sections = Vec::new();
    if !live.is_empty() {
        sections.push(live_section(&live, membered, &paint));
    }
    if !shown.is_empty() {
        let head = if shown.len() < ended.len() {
            format!("ended, newest {} of {}", shown.len(), ended.len())
        } else {
            "ended".to_owned()
        };
        let (mut section, left_out) = ended_section(shown, membered, layout, &paint);
        section.insert_str(0, &format!("{}\n", paint.on(HEAD, &head)));
        if left_out {
            let wait = next::of_run("wait --outcome", at, "<run>");
            let note = format!("+n counts the values of a result left out: to see them: {wait}");
            let _ = writeln!(section, "{}", paint.on(DIM, &note));
        }
        sections.push(section);
    }
    let hidden = Hidden {
        days: (shown.len() < ended.len()).then(|| days(&rows)),
        older,
    };
    let totals = totals(&rows, &hidden, at, &paint);
    sections.extend(totals.map(|totals| format!("{totals}\n")));
    sections.join("\n")
}

/// The JSON that `ls --json` prints of `listing` and `unlisted`, on a line, or on many if
/// `pretty`. It holds each run's row, the newest first, and the ids of the runs that ended over a
/// day ago, which aren't read. Then it holds the runs that can't be listed.
pub(crate) fn json(listing: &Listing, unlisted: &[Unlisted], pretty: bool) -> String {
    #[derive(Serialize)]
    struct Listed<'a> {
        runs: Vec<&'a Row>,
        older: Vec<&'a str>,
        unlisted: &'a [Unlisted],
    }
    let runs = listing.runs.values().rev().filter_map(Entry::row).collect();
    let older = (listing.runs.iter())
        .filter(|(_, entry)| matches!(entry, Entry::Older))
        .map(|(id, _)| id.as_str())
        .rev()
        .collect();
    let listed = Listed {
        runs,
        older,
        unlisted,
    };
    let json = if pretty {
        serde_json::to_string_pretty(&listed)
    } else {
        serde_json::to_string(&listed)
    };
    match json {
        Ok(json) => json + "\n",
        // Serializing a map whose keys aren't strings fails, and a row holds none.
        Err(error) => unreachable!("a listing serializes: {error}"),
    }
}

const HEAD: Style = Style::new().bold();
const DIM: Style = Style::new().dimmed();
// Cargo's palette: green for what went well, yellow for a warning, red for an error.
// https://github.com/rust-lang/cargo/blob/111443f3dc4d82a65e2306a93028f122406401c1/crates/cargo-util-terminal/src/style.rs#L4-L15
const LIVE: Style = AnsiColor::Cyan.on_default();
const GOOD: Style = AnsiColor::Green.on_default();
const WARN: Style = AnsiColor::Yellow.on_default();
const BAD: Style = AnsiColor::Red.on_default().bold();

/// The style of a run that is in `state`, with a result if `result`.
fn state_style(state: State, result: bool) -> Style {
    match state {
        State::Waiting | State::Building | State::Running => LIVE,
        State::Finished if result => GOOD,
        State::Finished | State::Interrupted | State::Unstarted => WARN,
        State::Failed | State::Lost => BAD,
        State::Unread => DIM,
    }
}

/// Styles text where the lines are styled, and leaves it plain where they aren't.
struct Paint(bool);

impl Paint {
    fn on(&self, style: Style, text: &str) -> String {
        if self.0 && !text.is_empty() {
            format!("{style}{text}{style:#}")
        } else {
            text.to_owned()
        }
    }
}

/// The widths of a section's columns, each its longest cell's.
#[derive(Default)]
struct Widths {
    member: usize,
    task: usize,
    state: usize,
    bound: usize,
    usd: usize,
}

impl Widths {
    fn of<'r>(
        rows: impl IntoIterator<Item = &'r Row>,
        membered: bool,
        state: impl Fn(&Row) -> String,
    ) -> Self {
        let mut widths = Self::default();
        for row in rows {
            if membered {
                widths.member = widths.member.max(member(row).chars().count());
            }
            widths.task = widths.task.max(task(row).chars().count());
            widths.state = widths.state.max(state(row).chars().count());
            let bound = row
                .phase
                .as_ref()
                .map_or(0, |phase| bound(phase).chars().count());
            widths.bound = widths.bound.max(bound);
            widths.usd = widths.usd.max(spent(row.spent).chars().count());
        }
        widths
    }
}

fn member(row: &Row) -> String {
    row.member
        .as_ref()
        .map_or("-".to_owned(), ToString::to_string)
}

fn task(row: &Row) -> &str {
    row.task.as_deref().unwrap_or("-")
}

fn spent(spent: Option<Spent>) -> String {
    match spent {
        Some(Spent {
            usd: Some(cost),
            partial,
        }) => usd(cost, partial),
        Some(Spent { usd: None, .. }) | None => "-".to_owned(),
    }
}

/// How long `step` has lasted, as the time column shows it.
pub(crate) fn time(step: &Step) -> String {
    (step.seconds).map_or("-".to_owned(), |seconds| {
        column_span(Duration::from_secs(seconds))
    })
}

/// The bound of `step`, as `of 30m`, or nothing if it has none.
pub(crate) fn bound(step: &Step) -> String {
    (step.bound_seconds).map_or_else(String::new, |bound| {
        format!("of {}", span(Duration::from_secs(bound)))
    })
}

/// The cells that start a row: its id, and its member if the section names members.
fn started(row: &Row, membered: bool, widths: &Widths) -> String {
    let mut line = format!("  {}", row.id);
    if membered {
        let _ = write!(line, "  {:<width$}", member(row), width = widths.member);
    }
    line
}

/// A step's time, its bound, and on a terminal its bar, `bound` columns wide.
fn timed(step: &Step, bound_width: usize, paint: &Paint) -> String {
    let time = format!("{:>6}", time(step));
    let style = if step.past_bound { BAD } else { Style::new() };
    let mut cells = format!(
        "{} {:<bound_width$}",
        paint.on(style, &time),
        self::bound(step)
    );
    if paint.0 {
        cells.push(' ');
        cells.push_str(&bar(step, paint));
    }
    cells
}

/// `step`'s time against its bound as [`BAR_CELLS`] cells, half a cell at a time, or blank cells
/// if it has no bound.
fn bar(step: &Step, paint: &Paint) -> String {
    let (Some(seconds), Some(bound)) = (step.seconds, step.bound_seconds) else {
        return " ".repeat(BAR_CELLS);
    };
    let done = (seconds.saturating_mul(BAR_HALVES) / bound.max(1)).min(BAR_HALVES);
    let done = usize::try_from(done).unwrap_or(BAR_CELLS * 2);
    let (full, half) = (done / 2, done % 2);
    let mut filled = "━".repeat(full);
    if half == 1 {
        filled.push('╸');
    }
    let left = "─".repeat(BAR_CELLS - full - half);
    let style = if step.past_bound { BAD } else { LIVE };
    format!("{}{}", paint.on(style, &filled), paint.on(DIM, &left))
}

fn live_section(rows: &[&Row], membered: bool, paint: &Paint) -> String {
    let name = |row: &Row| (row.phase.as_ref()).map_or(String::new(), |phase| phase.name.clone());
    let widths = Widths::of(rows.iter().copied(), membered, name);
    let steps = rows.iter().flat_map(|row| &row.steps);
    let (step_name, step_bound) = steps.fold((0, 0), |(name, bound), step| {
        let step_bound = self::bound(step).chars().count();
        (name.max(step.name.chars().count()), bound.max(step_bound))
    });
    let mut section = format!("{}\n", paint.on(HEAD, "live"));
    for row in rows {
        let mut line = started(row, membered, &widths);
        let _ = write!(line, "  {:<width$}", task(row), width = widths.task);
        let state = format!("{:<width$}", name(row), width = widths.state);
        let _ = write!(line, "  {}", paint.on(LIVE, &state));
        if let Some(phase) = &row.phase {
            let _ = write!(line, "  {}", timed(phase, widths.bound, paint));
        }
        let usd = format!("{:>width$}", spent(row.spent), width = widths.usd);
        let _ = write!(line, "  {}", paint.on(dim_if_none(&usd), &usd));
        let doing = (row.phase.as_ref()).map_or("", |phase| phase.doing.as_str());
        if !doing.is_empty() {
            let _ = write!(line, "  {doing}");
        }
        let _ = writeln!(section, "{}", line.trim_end());
        for step in &row.steps {
            let mut line = format!("    {:<step_name$}", step.name);
            let _ = write!(line, "  {}", timed(step, step_bound, paint));
            if !step.doing.is_empty() {
                let _ = write!(line, "  {}", step.doing);
            }
            let _ = writeln!(section, "{}", line.trim_end());
        }
    }
    section
}

fn dim_if_none(cell: &str) -> Style {
    if cell.trim() == "-" {
        DIM
    } else {
        Style::new()
    }
}

/// The lines of `rows`, runs that ended, and whether a row left a value of its result out.
fn ended_section(rows: &[&Row], membered: bool, layout: &Layout, paint: &Paint) -> (String, bool) {
    let state = |row: &Row| row.state.name().to_owned();
    let widths = Widths::of(rows.iter().copied(), membered, state);
    let mut section = String::new();
    let mut left_out = false;
    for row in rows {
        let mut line = started(row, membered, &widths);
        let _ = write!(line, "  {:<width$}", task(row), width = widths.task);
        let state = format!("{:<width$}", row.state.name(), width = widths.state);
        let style = state_style(row.state, row.result.is_some());
        let _ = write!(line, "  {}", paint.on(style, &state));
        let ran = (row.ran_seconds).map_or("-".to_owned(), |seconds| {
            column_span(Duration::from_secs(seconds))
        });
        let ran = format!("{ran:>6}");
        let _ = write!(line, "  {}", paint.on(dim_if_none(&ran), &ran));
        let usd = format!("{:>width$}", spent(row.spent), width = widths.usd);
        let _ = write!(line, "  {}", paint.on(dim_if_none(&usd), &usd));
        // What the plain line has taken so far: styles take no column.
        let taken = anstream::adapter::strip_str(&line)
            .to_string()
            .chars()
            .count()
            + 2;
        let rest = match (&row.result, &row.note) {
            (Some(values), _) => {
                let room = layout.width.map(|width| width.saturating_sub(taken));
                let (shown, left) = fitted(values, room, paint);
                left_out |= left > 0;
                shown
            }
            (None, Some(note)) => paint.on(DIM, note),
            (None, None) if row.state == State::Finished => paint.on(DIM, "no result"),
            (None, None) => String::new(),
        };
        if !rest.is_empty() {
            let _ = write!(line, "  {rest}");
        }
        let _ = writeln!(section, "{}", line.trim_end());
    }
    (section, left_out)
}

/// The values of a result that fit [`RESULT_SHOWN_BYTES`] and `room` characters, if it bounds
/// them, in their order, as `name=value`. They end with `+n` for the `n` left out, and come with
/// that count. Only a scalar is shown: a number, a boolean, `null` or a string of one line. A
/// list, an object, or a string of more than one line is left out. So is each value from the
/// first that doesn't fit.
fn fitted(values: &[super::row::Returned], room: Option<usize>, paint: &Paint) -> (String, usize) {
    // The mark is ASCII, so its bytes are its characters.
    let mark_size = format!(" +{}", values.len()).len();
    let (mut plain, mut styled, mut count) = (String::new(), String::new(), 0);
    for (index, super::row::Returned { name, value }) in values.iter().enumerate() {
        let Some(value) = scalar(value) else {
            continue;
        };
        let space = if plain.is_empty() { "" } else { " " };
        let item = format!("{space}{name}={value}");
        // The mark is needed unless this value is the last and each before it is shown.
        let last = index + 1 == values.len() && count == index;
        let mark = if last { 0 } else { mark_size };
        let needed_size = plain.len() + item.len() + mark;
        let needed_characters = plain.chars().count() + item.chars().count() + mark;
        let past_room = room.is_some_and(|room| needed_characters > room);
        if needed_size > RESULT_SHOWN_BYTES || past_room {
            break;
        }
        plain.push_str(&item);
        let _ = write!(
            styled,
            "{space}{}{value}",
            paint.on(DIM, &format!("{name}="))
        );
        count += 1;
    }
    let left = values.len() - count;
    if left > 0 {
        let space = if plain.is_empty() { "" } else { " " };
        styled.push_str(space);
        styled.push_str(&paint.on(DIM, &format!("+{left}")));
    }
    (styled, left)
}

/// `value` as a row shows it, if it is a scalar.
fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::Null | Value::Bool(_) => Some(value.to_string()),
        Value::Number(number) => number.as_f64().map(number_shown),
        Value::String(text) if !text.contains('\n') => Some(value.to_string()),
        Value::String(_) | Value::Array(_) | Value::Object(_) => None,
    }
}

/// The runs of a listing that its lines don't show.
struct Hidden {
    /// The days, as the prefixes of their ids, of the runs listed, if some that ended aren't shown.
    days: Option<Vec<String>>,
    /// How many ended over a day ago, which the listing doesn't read.
    older: usize,
}

/// The days of `rows`, as the prefixes of their ids, such as `20261125`.
fn days(rows: &[&Row]) -> Vec<String> {
    let days: BTreeSet<&str> = rows.iter().filter_map(|row| row.id.get(..8)).collect();
    days.into_iter().map(str::to_owned).collect()
}

/// The line of totals of `rows`. It says how many there are in each state and what they cost.
/// It names the commands that list the runs `hidden` holds. `None` for a listing of no run.
fn totals(rows: &[&Row], hidden: &Hidden, at: &next::At, paint: &Paint) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(days) = &hidden.days {
        // The days' prefixes read only the records of those days, where `--all` reads every one.
        let them = next::of_run("ls", at, &days.join(" "));
        parts.push(format!("to list them all: {them}"));
    }
    let every = format!("{} --all", next::of_runs("ls", at));
    if rows.is_empty() {
        let older = crate::counted(hidden.older, "older run");
        return (hidden.older > 0).then(|| {
            format!("no run is live, or ended within a day · to list the {older}: {every}")
        });
    }
    if hidden.older > 0 {
        let older = crate::counted(hidden.older, "older run");
        parts.push(format!("to list the {older} too: {every}"));
    }
    let mut counts: BTreeMap<State, usize> = BTreeMap::new();
    let (mut live, mut without_result, mut cost, mut partial) = (0, 0, None, false);
    for row in rows {
        if row.live() {
            live += 1;
        } else {
            *counts.entry(row.state).or_default() += 1;
        }
        if row.state == State::Finished && row.result.is_none() {
            without_result += 1;
        }
        match row.spent {
            Some(Spent {
                usd: Some(usd),
                partial: part,
            }) => {
                *cost.get_or_insert(0.0) += usd;
                partial |= part;
            }
            Some(Spent { usd: None, .. }) => partial = true,
            None => {}
        }
    }
    let mut states = Vec::new();
    if live > 0 {
        states.push(paint.on(LIVE, &format!("{live} live")));
    }
    for (state, count) in counts {
        let mut shown = paint.on(
            state_style(state, true),
            &format!("{count} {}", state.name()),
        );
        if state == State::Finished && without_result > 0 {
            let without = format!("({without_result} without a result)");
            let _ = write!(shown, " {}", paint.on(WARN, &without));
        }
        states.push(shown);
    }
    let runs = crate::counted(rows.len(), "run");
    let mut said = vec![format!("{runs}: {}", states.join(", "))];
    if let Some(cost) = cost {
        said.push(usd(cost, partial));
    }
    said.extend(parts);
    Some(said.join(" · "))
}
