//! `pumpkin ls` on the controller: the controller lists its runs, and its jobs yet to start one,
//! with its own health, and sends the files of each record that `ls` reads, from which this
//! machine reads each run as it reads its own.
//!
//! The files of a record that ended are kept in the workspace's connections to the controller
//! ([`super::Shared::ended`]), as they never change, so the controller sends them once. What a
//! listing shows, and how, is [`crate::progress`]'s.

use std::fs::{self, DirBuilder};
use std::io::Write as _;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use stdx::process::Environment;
use stdx::{Context as _, Deadline};
use tempfile::TempDir;

use super::{Name, Reached, SHORT_TIMEOUT, finished};
use crate::job::{JOBS, WAITING};
use crate::workspace::Workspace;
use crate::{config, next, say};

/// How long a listing of the controller's runs may take to arrive before `ls` says when it was
/// made: a person reads the rows of a listing as they are now.
const STALE: Duration = Duration::from_secs(10);

/// A directory of this command's own, 0700, in the workspace's `tmp/` ([`Workspace::scratch`]).
fn scratch() -> Result<TempDir, String> {
    Workspace::find()?.scratch("pumpkin-")
}

/// `pumpkin ls` on the controller: lists its runs whose ids start with one of `prefixes`, or
/// every run without any but those that ended over a day ago ([`progress::listed_since`]), the
/// jobs yet to start one as `waiting` for a build slot, or else `building`, or `unstarted` once
/// they have ended, beside the copies in this machine's default runs directory, by id, the
/// controller's of a run that both hold. Each run is read here, from the files of its record that
/// `ls` reads, which the controller sends once the record has ended unless its copy here has.
///
/// [`progress::listed_since`]: crate::progress::listed_since
pub(crate) fn ls(client: &config::Client, prefixes: &[String]) -> Result<ExitCode, String> {
    let reached = Reached::reach(&client.controller)?;
    let now = crate::unix_now();
    let since = crate::progress::listed_since(prefixes, now);
    let kept = reached.shared.ended();
    let (mut listing, mut unlisted) = listed_here(&kept, prefixes, since)?;
    let mut held = String::new();
    let ended = (listing.runs.iter()).filter_map(|(id, run)| run.ended().then_some(id));
    for id in ended.chain(&listing.older) {
        held.push_str(id);
        held.push('\n');
    }
    let since = since.map_or(0, |since| since.as_secs());
    let printed = listed_there(&reached, prefixes, since, &held)?;
    let scratch = scratch()?;
    let sent = scratch.path().join("runs");
    let states = extract(&printed, &sent)?;
    let mut health = Health::default();
    for line in String::from_utf8_lossy(states).lines() {
        let (id, state) = line.split_once(' ').unwrap_or((line, ""));
        if health.take(id, state) {
            continue;
        }
        let run = match state {
            "old" => {
                listing.older.insert(id.to_owned());
                continue;
            }
            "building" => Ok(crate::progress::Listed::Building {
                asked: runtime::minted(id),
            }),
            "waiting" => Ok(crate::progress::Listed::Waiting {
                asked: runtime::minted(id),
            }),
            "unstarted" => Ok(crate::progress::Listed::Unstarted),
            // The lock that says a run is live is the controller's, so its copy here is read
            // as live.
            "live" => runtime::Record::open(&sent.join(id))
                .and_then(|record| crate::progress::Listed::run(&record, None)),
            "ended" => keep(&sent, &kept, id)
                .and_then(|record| crate::progress::Listed::run(&record, record.ending()?)),
            _ => Err(format!(
                "controller `{}` listed `{}`, which is none of the lines that its listing prints",
                reached.name,
                line.escape_debug()
            )),
        };
        match run {
            Ok(run) => {
                listing.runs.insert(id.to_owned(), run);
            }
            Err(error) => {
                say(error);
                unlisted += 1;
            }
        }
    }
    let jobs = listing.runs.values().filter(|run| run.live()).count();
    say(health.line(&reached.name, jobs));
    // As of when the controller began the listing, which is when it read what each run does.
    let lines = crate::progress::lines(&listing, now, None);
    crate::print(lines.as_bytes()).context("printing the runs failed")?;
    said_when(now.since_epoch());
    if unlisted > 0 {
        return Err("a run can't be listed, as said above".to_owned());
    }
    Ok(ExitCode::SUCCESS)
}

/// What `ls` lists, given `prefixes` and `since` ([`crate::listed_in`]), of the runs that this
/// machine holds what it needs of: the copies in the default runs directory, and what `ls` kept
/// in `kept` of each record that ended; a copy, which `wait` made, rather than what was kept of
/// the same run. Says each run that can't be listed, and returns how many.
fn listed_here(
    kept: &Path,
    prefixes: &[String],
    since: Option<Duration>,
) -> Result<(crate::progress::Listing, usize), String> {
    let runs = PathBuf::from(crate::default_runs()?);
    let (mut listing, unlisted) = crate::listed_in(&runs, true, prefixes, since)?;
    let (ended, ended_unlisted) = crate::listed_in(kept, true, prefixes, since)?;
    for (id, run) in ended.runs {
        listing.runs.entry(id).or_insert(run);
    }
    let older = ended.older.into_iter();
    let older: Vec<String> = older.filter(|id| !listing.runs.contains_key(id)).collect();
    listing.older.extend(older);
    Ok((listing, unlisted + ended_unlisted))
}

/// Says when the controller began a listing, at `began`, since the Unix epoch, if it took
/// [`STALE`] or more to arrive.
fn said_when(began: Duration) {
    let took = crate::unix_now().since_epoch().saturating_sub(began);
    if took < STALE {
        return;
    }
    let stdx::Utc {
        hour,
        minute,
        second,
        ..
    } = stdx::Utc::from_unix(began.as_secs());
    say(format_args!(
        "the runs are listed as they were at {hour:02}:{minute:02}:{second:02} UTC, {} ago, as \
         the listing took that long to arrive",
        crate::progress::span(took)
    ));
}

/// The record of run `id` that ended, as `ls` was sent it in `sent`, moved into `kept`, which
/// [`Shared::ended`](super::Shared::ended) names, and read there.
fn keep(sent: &Path, kept: &Path, id: &str) -> Result<runtime::Record, String> {
    fs::create_dir_all(kept).context(kept.display())?;
    let to = kept.join(id);
    fs::rename(sent.join(id), &to).context(to.display())?;
    runtime::Record::open(&to)
}

/// What [`listing`] prints on `reached` of the runs whose ids start with one of `prefixes`, or
/// every run without any, of those that ended at `since` or after, but those whose ids `held`
/// lists, a line each, whose copies here have ended.
fn listed_there(
    reached: &Reached,
    prefixes: &[String],
    since: u64,
    held: &str,
) -> Result<Vec<u8>, String> {
    let mut ssh = reached.ssh(&listing(prefixes, since))?;
    let fed = stdx::process::fed(&mut ssh, held.as_bytes(), Deadline::after(SHORT_TIMEOUT));
    let output = match fed {
        Ok(Some(output)) => output,
        Ok(None) => {
            return Err(format!(
                "listing the runs on controller `{}` took over {} s",
                reached.name,
                SHORT_TIMEOUT.as_secs()
            ));
        }
        Err(error) => return Err(format!("`ssh` didn't run: {error}: install it on PATH")),
    };
    let _ = std::io::stderr().write_all(&output.stderr);
    if !output.status.success() {
        let failed = format!(
            "listing the runs on controller `{}` failed ({}), as said above",
            reached.name, output.status
        );
        return Err(reached.lost(ssh.slot.master, failed));
    }
    Ok(output.stdout)
}

/// What the controller said of itself as it listed its runs ([`listing`]): how long it has been
/// up, the state of its boot's unit, `pumpkin-boot.service`, and how full its disk is.
#[derive(Default)]
struct Health {
    up: Option<Duration>,
    /// The unit's properties that `systemctl show` printed, by name.
    boot: std::collections::BTreeMap<String, String>,
    /// The share of the disk in use, as `df` gives it, such as `61%`.
    disk: Option<String>,
}

impl Health {
    /// Takes in a line that [`listing`] printed, its first word `word` and then `rest`, if it is
    /// one of the controller's health; returns whether it was.
    fn take(&mut self, word: &str, rest: &str) -> bool {
        match word {
            "up" => {
                let seconds = rest
                    .split('.')
                    .next()
                    .and_then(|seconds| seconds.parse().ok());
                self.up = seconds.map(Duration::from_secs);
            }
            "boot" => {
                if let Some((name, value)) = rest.split_once('=') {
                    self.boot.insert(name.to_owned(), value.to_owned());
                }
            }
            "disk" => self.disk = Some(rest.trim().to_owned()),
            _ => return false,
        }
        true
    }

    /// The line that says the health of controller `name`, which runs `jobs` jobs.
    fn line(&self, name: &Name, jobs: usize) -> String {
        let mut said = Vec::new();
        if let Some(up) = self.up {
            said.push(format!("up {}", crate::progress::span(up)));
        }
        said.push(crate::counted(jobs, "job"));
        if let Some(disk) = &self.disk {
            said.push(format!("disk {disk} used"));
        }
        let property = |name: &str| self.boot.get(name).map_or("", String::as_str);
        let boot = match property("ActiveState") {
            "failed" => format!(
                "; its boot, pumpkin-boot.service, failed with status {} at {}: read why there, \
                 in `journalctl --unit pumpkin-boot.service`",
                property("ExecMainStatus"),
                property("ExecMainExitTimestamp")
            ),
            "activating" => "; its boot, pumpkin-boot.service, is still running".to_owned(),
            _ => String::new(),
        };
        format!("controller `{name}`: {}{boot}", said.join(", "))
    }
}

/// The files of a record that `ls` reads, which the controller sends.
const LISTED_FILES: &str = "record.json ir.json provider.json log.jsonl outcome.json driver.pid";

/// What a command on the controller runs to list its runs whose ids start with one of
/// `prefixes`, or every run without any: its health, `up <seconds>`, `boot <property>=<value>`
/// for each that `systemctl show` gives of its boot's unit, and `disk <used>%`; a line for each
/// job yet to start its run, `<id> waiting` while it waits for a build slot, or else
/// `<id> building`, or `<id> unstarted` once it has ended; a line for each run, `<id> live`, or
/// `<id> ended` unless its id is one of those its standard input lists; of a run that ended, or a
/// job, before `since`, in seconds since the Unix epoch, as its `outcome.json`, or its log, was
/// last written, `<id> old` instead; then `tar`, and an archive of the files that `ls` reads
/// ([`LISTED_FILES`]) of the records of the runs that are live or ended.
// A run may write its log while GNU tar reads it, which tar says by its exit status 1 alone: the
// log then ends in a torn line, which a reader skips.
fn listing(prefixes: &[String], since: u64) -> String {
    let picked = if prefixes.is_empty() {
        String::new()
    } else {
        let patterns: Vec<String> = (prefixes.iter())
            .map(|prefix| format!("{}*", next::quoted(prefix)))
            .collect();
        format!("case $id in {}) ;; *) continue ;; esac", patterns.join("|"))
    };
    format!(
        r#"cd
held=$(cat)
echo "up $(cut --delimiter=' ' --fields=1 /proc/uptime)"
systemctl show --property=ActiveState --property=ExecMainStatus --property=ExecMainExitTimestamp pumpkin-boot.service | sed 's/^/boot /'
echo "disk $(df --output=pcent / | tail --lines=1)"
runs={runs}
recent() {{ [ ! -e "$1" ] || [ "$(stat --format=%Y "$1")" -ge {since} ]; }}
for id in $(ls {JOBS} 2>/dev/null); do
    {picked}
    [ ! -e "$runs/$id" ] || continue
    if [ -e "$XDG_RUNTIME_DIR/pumpkin/$id/{WAITING}" ]; then echo "$id waiting"
    elif [ -d "$XDG_RUNTIME_DIR/pumpkin/$id" ]; then echo "$id building"
    elif recent "{JOBS}/$id/log"; then echo "$id unstarted"; else echo "$id old"; fi
done
sent=
for id in $(ls "$runs" 2>/dev/null); do
    {picked}
    [ -e "$runs/$id/record.json" ] || continue
    if [ ! -e "$runs/$id/outcome.json" ] && ! flock --nonblock --shared "$runs/$id/driver.pid" true 2>/dev/null; then
        state=live
    else
        state=ended
        if printf '%s
' "$held" | grep --quiet --line-regexp --fixed-strings "$id"; then continue; fi
        if ! recent "$runs/$id/outcome.json"; then echo "$id old"; continue; fi
    fi
    echo "$id $state"
    sent="$sent $id"
done
echo tar
cd "$runs" 2>/dev/null || true
for id in $sent; do
    for file in {LISTED_FILES}; do [ ! -e "$id/$file" ] || echo "$id/$file"; done
done | tar --create --warning=no-file-changed --file=- --files-from=- || [ $? -eq 1 ]
"#,
        runs = crate::job::runs()
    )
}

/// The lines of the states that [`listing`] printed, once the archive after them is extracted
/// into `sent`, a new directory.
fn extract<'p>(printed: &'p [u8], sent: &Path) -> Result<&'p [u8], String> {
    let mut at = 0;
    let mut archive: &[u8] = &[];
    for line in printed.split_inclusive(|byte| *byte == b'\n') {
        if line == b"tar\n" {
            archive = &printed[at + line.len()..];
            break;
        }
        at += line.len();
    }
    DirBuilder::new()
        .mode(0o700)
        .create(sent)
        .context(sent.display())?;
    let mut tar = stdx::process::command("tar", Environment::Only(&["PATH"]));
    tar.args(["--extract", "--file=-", "--directory"]).arg(sent);
    let extracted = stdx::process::fed(&mut tar, archive, Deadline::after(SHORT_TIMEOUT));
    finished(extracted, "tar --extract", SHORT_TIMEOUT)?;
    Ok(&printed[..at])
}
