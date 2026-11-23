//! `ls` of a machine whose runs go to a controller: the controller's `serve` ([`crate::serve`])
//! answers with its health and the rows of its runs and of its jobs yet to start one, made where
//! the records are, and `ls` adds the rows of the copies of its runs here that it lacks.

use std::time::Duration;

use provider::UnixTime;
use stdx::Context as _;
use tempfile::TempDir;

use super::linking::Build;
use super::{Name, Reached};
use crate::cli::Exit;
use crate::link::{Boot, Health, HealthRequest, ListRequest, Listed};
use crate::progress::{Entry, Listing, Unlisted};
use crate::workspace::Workspace;
use crate::{config, next, say};

// How long a listing may take before `ls` says when it was made: a person reads the rows of a
// listing as they are now.
const STALE: Duration = Duration::from_secs(10);

/// A directory in the workspace for what a command makes and throws away, removed when dropped.
pub(crate) fn scratch() -> Result<TempDir, String> {
    Workspace::find()?.scratch("pumpkin-")
}

/// Lists the runs of the controller whose ids start with one of `prefixes`, or, with none, all but
/// those that ended over a day ago (`progress::listed_since`), as `ls` lists those of this machine,
/// and the jobs yet to start a run, as `waiting` for a build slot, `building`, or `unstarted` once
/// they have ended, with the copies here of the runs that the controller lacks. Fails, after
/// listing the rest, if a run can't be listed.
pub(crate) fn ls(client: &config::Client, prefixes: &[String]) -> Result<Exit, String> {
    let mut reached = Reached::reach(&client.controller)?;
    let now = crate::unix_now();
    let since = crate::progress::listed_since(prefixes, now);
    let Answers {
        health,
        mut listing,
        unlisted,
        asked,
    } = answered(&mut reached, prefixes)?;
    crate::said_beside(&listing, &unlisted);
    let at = next::At::Controller(reached.name.clone());
    let runs = super::copies(&reached.name)?;
    let (here, unlisted_here) = crate::listed_in(&runs, prefixes, since, now, &at)?;
    crate::said_beside(&here, &unlisted_here);
    // The controller's word on a run wins over a copy's.
    for (id, entry) in here.runs {
        listing.runs.entry(id).or_insert(entry);
    }
    let rows = listing.runs.values().filter_map(Entry::row);
    let jobs = rows.filter(|row| row.live()).count();
    // What it says of itself is no reason to fail the listing.
    say(health.map_or_else(
        |refused| refused,
        |health| said(&reached.name, &health, jobs),
    ));
    let lines = crate::progress::lines(&listing, &at);
    crate::print(lines.as_bytes()).context("printing the runs failed")?;
    said_when(asked);
    let unlisted = unlisted.len() + unlisted_here.len();
    if unlisted > 0 {
        let runs = crate::counted(unlisted, "run");
        return Err(format!("{runs} can't be listed, as said above"));
    }
    Ok(Exit::Success)
}

/// What the controller's `serve` answers, as [`Reached::asked`] asks it.
fn answered(reached: &mut Reached, prefixes: &[String]) -> Result<Answers, String> {
    let build = Build::of_this("expected to send it to the controller, to list the runs there")?;
    let (health, listed, asked) =
        reached.asked(&build, "listing the runs", |reached, linked| {
            let health = linked.ask(reached, HealthRequest {})?;
            let asked = crate::unix_now();
            let prefixes = prefixes.to_vec();
            let controller = reached.name.clone();
            let listed = linked.ask(
                reached,
                ListRequest {
                    prefixes,
                    controller,
                },
            )?;
            Ok((health, listed, asked))
        })?;
    answers(&reached.name, health, listed, asked)
}

/// What `serve` answered to `Health` and to `List`.
struct Answers {
    /// The controller's health, or why there is none.
    health: Result<Health, String>,
    listing: Listing,
    /// The runs that couldn't be listed.
    unlisted: Vec<Unlisted>,
    /// When the listing was asked for, as `serve` makes its rows then.
    asked: UnixTime,
}

fn answers(
    name: &Name,
    health: Result<Health, String>,
    listed: Result<Listed, String>,
    asked: UnixTime,
) -> Result<Answers, String> {
    let Listed { listing, unlisted } =
        listed.map_err(|message| format!("controller `{name}` refused: {message}"))?;
    let health = health
        .map_err(|message| format!("controller `{name}` says nothing of its health: {message}"));
    Ok(Answers {
        health,
        listing,
        unlisted,
        asked,
    })
}

/// What `ls` says of the controller, whose `health` that is, with `jobs` live.
fn said(name: &Name, health: &Health, jobs: usize) -> String {
    let Health {
        up_seconds,
        disk_size_bytes,
        disk_free_bytes,
        boot,
    } = health;
    // The share of the disk that an ordinary user may not write, rounded up: it counts the blocks
    // reserved for root as used, so it reads a few points above `df`'s.
    let used = disk_size_bytes
        .saturating_sub(*disk_free_bytes)
        .saturating_mul(100);
    let used_percent = used.div_ceil((*disk_size_bytes).max(1)).min(100);
    let unit = crate::job::BOOT_UNIT;
    let boot = match boot {
        Boot::Failed { status, at, log } => {
            let at = at
                .as_ref()
                .map_or_else(String::new, |at| format!(" at {at}"));
            let tail = if log.is_empty() {
                String::new()
            } else {
                format!("\n{log}")
            };
            format!(
                "; its boot, {unit}, failed with status {status}{at}. See ~/boot.log on the controller{tail}"
            )
        }
        Boot::Running => format!("; its boot, {unit}, is still running"),
        Boot::Done | Boot::Unknown => String::new(),
    };
    format!(
        "controller `{name}`: up {}, {}, disk {used_percent}% used{boot}",
        crate::progress::span(Duration::from_secs(*up_seconds)),
        crate::counted(jobs, "job")
    )
}

fn said_when(began: UnixTime) {
    let took = crate::unix_now().since(began);
    if took < STALE {
        return;
    }
    let began_at = crate::utc(began.since_epoch()).strftime("%H:%M:%S");
    say(format_args!(
        "the runs are listed as they were at {began_at} UTC, {} ago, as \
         the listing took that long to arrive",
        crate::progress::span(took)
    ));
}
