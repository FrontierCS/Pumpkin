//! `ls` of a machine whose runs go to a controller: the controller's `serve` ([`crate::serve`])
//! answers with its health and the rows of its runs and of its jobs yet to start one, made where
//! the records are, and `ls` adds the rows of the copies of its runs here that it lacks.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use provider::UnixTime;
use schema::ir::Sha256Digest;
use stdx::Context as _;
use tempfile::TempDir;

use super::{Name, Reached, SHORT_TIMEOUT, Session};
use crate::link::{self, Answer, Ask, Boot, Health, Link, Unanswered};
use crate::progress::{Entry, Listing};
use crate::workspace::{self, Workspace};
use crate::{config, manifest, say};

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
pub(crate) fn ls(client: &config::Client, prefixes: &[String]) -> Result<ExitCode, String> {
    let reached = Reached::reach(&client.controller)?;
    let now = crate::unix_now();
    let since = crate::progress::listed_since(prefixes, now);
    let list = Ask::List {
        prefixes: prefixes.to_vec(),
        since_unix_secs: since.map(|since| since.since_epoch().as_secs()),
    };
    let (health, mut listing, unlisted) = answered(&reached, &list)?;
    let runs = PathBuf::from(crate::default_runs()?);
    let (here, unlisted_here) = crate::listed_in(&runs, prefixes, since, now, None)?;
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
    let lines = crate::progress::lines(&listing, None);
    crate::print(lines.as_bytes()).context("printing the runs failed")?;
    said_when(now);
    let unlisted = unlisted + unlisted_here;
    if unlisted > 0 {
        let runs = crate::counted(unlisted, "run");
        return Err(format!("{runs} can't be listed, as said above"));
    }
    Ok(ExitCode::SUCCESS)
}

/// What the controller's `serve` answers, which this installs there first if there is none;
/// asked again over another link, after each of `AGAIN`'s pauses, while one ends before it has
/// answered, as asking only reads.
fn answered(reached: &Reached, list: &Ask) -> Result<Answers, String> {
    let exe = workspace::exe("expected to send it to the controller, to list the runs there")?;
    let (digest, _) = manifest::file_sha256(&exe)?;
    let name = &reached.name;
    let (mut attempt, mut installed) = (0, false);
    loop {
        let Session { command, slot } = reached.ssh(&link::command(&digest))?;
        let master = slot.master;
        let asked = Link::open(command, slot).and_then(|link| match link {
            Some(mut link) => Ok(Some((link.ask(&Ask::Health)?, link.ask(list)?))),
            None => Ok(None),
        });
        let said = match asked {
            Ok(Some((health, listed))) => return answers(name, health, listed),
            Ok(None) if !installed => {
                install(reached, &exe, &digest)?;
                installed = true;
                continue;
            }
            Ok(None) => {
                return Err(format!(
                    "controller `{name}` has no serve of this pumpkin though it was just \
                     installed there, in `~/{}/{}`: see that its user may run it, and run the \
                     command again",
                    link::installs(),
                    digest.hex()
                ));
            }
            Err(Unanswered::Unreached(said)) => said,
            Err(Unanswered::Failed(error)) => {
                return Err(format!("linking to controller `{name}` failed: {error}"));
            }
        };
        let failed = format!("listing the runs on controller `{name}` failed, as `ssh` {said}");
        if !reached.pause(attempt, master, "listing the runs again") {
            return Err(reached.lost(master, failed));
        }
        attempt += 1;
    }
}

/// Uploads this executable, `exe`, whose SHA-256 is `digest`, to the controller, as the `serve`
/// that a link runs there.
fn install(reached: &Reached, exe: &Path, digest: &Sha256Digest) -> Result<(), String> {
    let name = &reached.name;
    say(format_args!(
        "controller `{name}` has no serve of this pumpkin: installing it there"
    ));
    let bytes = fs::read(exe).context(exe.display())?;
    let mut session = reached.ssh(&link::installing(digest))?;
    let installed = reached.call(&mut session, Some(&bytes), "ssh", SHORT_TIMEOUT);
    let installed = installed.map_err(|error| format!("installing it failed: {error}"));
    installed.map(drop)
}

/// The health, or why there is none, the listing and how many runs couldn't be listed.
type Answers = (Result<Health, String>, Listing, usize);

/// What `serve` answered to `Health` and to `List`.
fn answers(
    name: &Name,
    health: (Answer, Vec<u8>),
    listed: (Answer, Vec<u8>),
) -> Result<Answers, String> {
    let other = |asked: &str| {
        format!(
            "controller `{name}` answered `{asked}` with the answer to another request: expected \
             its own, as this pumpkin's `serve` gives it"
        )
    };
    let refused = |message: String| format!("controller `{name}` refused: {message}");
    let health = match health.0 {
        Answer::Health(health) => Ok(health),
        Answer::Refused { message } => Err(format!(
            "controller `{name}` says nothing of its health: {message}"
        )),
        Answer::Listed { .. } => return Err(other("health")),
    };
    let (listing, unlisted_count) = match listed {
        (
            Answer::Listed {
                body_bytes: _,
                unlisted_count,
            },
            body,
        ) => (serde_json::from_slice(&body), unlisted_count),
        (Answer::Refused { message }, _) => return Err(refused(message)),
        (Answer::Health(_), _) => return Err(other("list")),
    };
    let listing = listing.map_err(|error| {
        format!("controller `{name}` sent a listing that doesn't read: {error}")
    })?;
    let unlisted = usize::try_from(unlisted_count).unwrap_or(usize::MAX);
    Ok((health, listing, unlisted))
}

/// What `ls` says of the controller, whose `health` that is, with `jobs` live.
fn said(name: &Name, health: &Health, jobs: usize) -> String {
    let Health {
        up_seconds,
        disk_used_percent,
        boot,
    } = health;
    let unit = "pumpkin-boot.service";
    let boot = match boot {
        Boot::Failed { status, at } => format!(
            "; its boot, {unit}, failed with status {status} at {at}: read why there, in \
             `journalctl --unit {unit}`"
        ),
        Boot::Running => format!("; its boot, {unit}, is still running"),
        Boot::Done | Boot::Unknown => String::new(),
    };
    format!(
        "controller `{name}`: up {}, {}, disk {disk_used_percent}% used{boot}",
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
