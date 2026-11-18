//! `ls` of a machine whose runs go to a controller: the controller lists its runs, and its jobs
//! yet to start one, with its own health, and sends the files of each record that `ls` reads, from
//! which this machine reads each run as it reads its own.
//!
//! A run that has ended is copied once, and the copy is kept. A run that is live has a copy that
//! each `ls` grows: the controller is told what each copy holds, and sends only the log past its
//! last whole line and the small files it lacks. So what a listing carries is what changed since
//! the last, whatever the runs' number.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use provider::RunTag;
use stdx::{Context as _, Deadline};
use tempfile::TempDir;

use super::copy::untar;
use super::{Name, Reached, SHORT_TIMEOUT, finished};
use crate::job::{JOBS, WAITING};
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
/// they have ended. Fails, after listing the rest, if a run can't be listed.
pub(crate) fn ls(client: &config::Client, prefixes: &[String]) -> Result<ExitCode, String> {
    let reached = Reached::reach(&client.controller)?;
    let now = crate::unix_now();
    let since = crate::progress::listed_since(prefixes, now);
    let kept = reached.shared.ended();
    let live = reached.shared.live();
    let _listing = reached.shared.listing(&reached.name)?;
    let (mut listing, mut unlisted) = listed_here(&kept, prefixes, since)?;
    let held = holding(&listing, &live)?;
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
        // A run's state, then the byte of the controller's log that what it sent of it starts at.
        let (state, from) = match state.split_once(' ') {
            Some((state, from)) => (state, from.parse().ok()),
            None => (state, None),
        };
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
            // The lock that says a run is live is the controller's, so its copy here is read as
            // live.
            "live" => copied(&reached.name, (&sent, &live), id, from)
                .and_then(|id| runtime::Record::open(&live.join(id.as_str())))
                .and_then(|record| crate::progress::Listed::run(&record, None)),
            "ended" => copied(&reached.name, (&sent, &live), id, from)
                .and_then(|id| keep(&live, &kept, &id))
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

/// The runs that this workspace's copies, ended and live, list, and how many it couldn't list.
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

fn said_when(began: Duration) {
    let took = crate::unix_now().since_epoch().saturating_sub(began);
    if took < STALE {
        return;
    }
    let began_at = crate::utc(began).strftime("%H:%M:%S");
    say(format_args!(
        "the runs are listed as they were at {began_at} UTC, {} ago, as \
         the listing took that long to arrive",
        crate::progress::span(took)
    ));
}

/// Moves run `id`'s copy, which has ended, from the live copies to the kept ones, which no
/// listing changes again.
fn keep(live: &Path, kept: &Path, id: &RunTag) -> Result<runtime::Record, String> {
    let to = kept.join(id.as_str());
    let moved = fs::create_dir_all(kept).and_then(|()| fs::rename(live.join(id.as_str()), &to));
    moved.map_err(|error| format!("{}: {error}: {}", to.display(), afresh(live, id)))?;
    runtime::Record::open(&to)
}

fn afresh(live: &Path, id: &RunTag) -> String {
    let copy = live.join(id.as_str());
    format!(
        "remove {}, and run `ls` again, which is then sent the run whole",
        copy.display()
    )
}

/// Places what the controller sent of run `id`, in `sent`, in its live copy. `from` is the byte
/// of the log that the sent part of it starts at, none if the run is new here.
fn copied(
    name: &Name,
    (sent, live): (&Path, &Path),
    id: &str,
    from: Option<u64>,
) -> Result<RunTag, String> {
    let tag = RunTag::try_from(id.to_owned()).map_err(|_| {
        format!(
            "controller `{name}` listed `{}` as a run, which is no run's id, so `ls` keeps \
             nothing of it: see what `~/{}` holds there, and run `ls` again",
            id.escape_debug(),
            crate::job::runs()
        )
    })?;
    grown(sent, live, &tag, from)?;
    Ok(tag)
}

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

/// What this machine holds, in the form that the controller's script reads on its standard input:
/// a line `<id>` for each run held as ended or too old to list, and `<id> <bytes> <file>…` for
/// each live copy, with the length of its log up to its last whole line (which it is cut to) and
/// the other files it holds. A live copy of a run held as ended is removed.
fn holding(listing: &crate::progress::Listing, live: &Path) -> Result<String, String> {
    let ended = (listing.runs.iter()).filter_map(|(id, run)| run.ended().then_some(id));
    let ended: BTreeSet<&String> = ended.chain(&listing.older).collect();
    let mut held = String::new();
    for id in &ended {
        held.push_str(id);
        held.push('\n');
    }
    let entries = match fs::read_dir(live) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(held),
        Err(error) => return Err(error).context(live.display()),
    };
    for entry in entries {
        let copy = entry.context(live.display())?.path();
        let id = copy.file_name().unwrap_or_default().to_string_lossy();
        if ended.contains(&id.to_string()) {
            fs::remove_dir_all(&copy).context(copy.display())?;
            continue;
        }
        held.push_str(&id);
        held.push(' ');
        held.push_str(&whole(&copy.join("log.jsonl"))?.to_string());
        for file in LISTED_FILES.split(' ').filter(|file| *file != "log.jsonl") {
            if copy.join(file).try_exists().context(copy.display())? {
                held.push(' ');
                held.push_str(file);
            }
        }
        held.push('\n');
    }
    Ok(held)
}

// How much of a log `whole` reads at a time, backwards from its end, to find a newline: more than
// most of a log's lines hold, and a longer line takes another read.
const TAIL_BYTES: u64 = 4096;

/// The length of the copy of a log up to its last newline, to which it cuts it, so that what is
/// appended to it later starts a line; 0 if it has none.
fn whole(log: &Path) -> Result<u64, String> {
    let metadata = match fs::symlink_metadata(log) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error).context(log.display()),
    };
    if !metadata.is_file() {
        return Err(format!(
            "{} is no regular file, as a copy of a log is: remove it, and run `ls` again",
            log.display()
        ));
    }
    let file = fs::OpenOptions::new().read(true).write(true).open(log);
    let mut file = file.context(log.display())?;
    let length = metadata.len();
    let (mut end, mut chunk) = (length, Vec::new());
    let whole = loop {
        let start = end.saturating_sub(TAIL_BYTES);
        chunk.clear();
        let read = (file.seek(io::SeekFrom::Start(start)))
            .and_then(|_| (&file).take(end - start).read_to_end(&mut chunk));
        read.context(log.display())?;
        if let Some(at) = chunk.iter().rposition(|byte| *byte == b'\n') {
            break start + at as u64 + 1;
        }
        if start == 0 {
            break 0;
        }
        end = start;
    };
    if whole < length {
        file.set_len(whole).context(log.display())?;
    }
    Ok(whole)
}

/// Grows the copy of run `id` in `live` by what the controller sent of it, whose log starts at
/// byte `from` of the controller's: appends that to the copy's log, and moves in each other file;
/// or makes what was sent the copy, if there is none, or `from` is 0 or none, as the controller
/// then sent the run whole.
fn grown(sent: &Path, live: &Path, id: &RunTag, from: Option<u64>) -> Result<(), String> {
    let (sent, copy) = (sent.join(id.as_str()), live.join(id.as_str()));
    let entries = sent_files(&sent, id)?;
    let grew = growing(&sent, &copy, entries, from);
    grew.map_err(|error| format!("{error}: {}", afresh(live, id)))
}

fn sent_files(sent: &Path, id: &RunTag) -> Result<Vec<fs::DirEntry>, String> {
    let refused = |what: String| {
        format!(
            "the controller sent {what}, which a run's record never holds, so `ls` keeps \
             nothing of it: see what `~/{}/{id}` holds there, and run `ls` again",
            crate::job::runs(),
            id = id.as_str()
        )
    };
    let entries = match fs::symlink_metadata(sent) {
        Ok(metadata) if metadata.is_dir() => fs::read_dir(sent).and_then(Iterator::collect),
        Ok(_) => return Err(refused(format!("`{}` as no directory", id.as_str()))),
        // Nothing was sent: its log didn't grow, and its copy holds every other file.
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    };
    let entries: Vec<fs::DirEntry> = entries.context(sent.display())?;
    for entry in &entries {
        let file_type = entry.file_type().context(entry.path().display())?;
        if !file_type.is_file() {
            let name = entry.file_name();
            let what = format!(
                "`{}/{}` as no regular file",
                id.as_str(),
                name.to_string_lossy()
            );
            return Err(refused(what));
        }
    }
    Ok(entries)
}

fn growing(
    sent: &Path,
    copy: &Path,
    entries: Vec<fs::DirEntry>,
    from: Option<u64>,
) -> Result<(), String> {
    if from.unwrap_or(0) == 0 || !copy.try_exists().context(copy.display())? {
        match fs::remove_dir_all(copy) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                return Err(error).context(copy.display());
            }
            _ => {}
        }
        let live = copy.parent().unwrap_or(copy);
        fs::create_dir_all(live).context(live.display())?;
        return fs::rename(sent, copy).context(copy.display());
    }
    for entry in entries {
        let from = entry.path();
        let to = copy.join(entry.file_name());
        if entry.file_name() == "log.jsonl" {
            let part = fs::read(&from).context(from.display())?;
            let log = fs::OpenOptions::new().append(true).create(true).open(&to);
            log.and_then(|mut log| log.write_all(&part))
                .context(to.display())?;
        } else {
            fs::rename(&from, &to).context(to.display())?;
        }
    }
    Ok(())
}

/// What the controller says of itself as it lists its runs: how long it has been up, the state of
/// its boot's unit, `pumpkin-boot.service`, and how full its disk is.
#[derive(Default)]
struct Health {
    up: Option<Duration>,
    /// The unit's properties that `systemctl show` printed, by name.
    boot: std::collections::BTreeMap<String, String>,
    /// The share of the disk in use, as `df` gives it, such as `61%`.
    disk: Option<String>,
}

impl Health {
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

// The files of a record that `ls` reads. `log.jsonl` is the one that grows, so it goes by offset.
const LISTED_FILES: &str = "record.json ir.json provider.json log.jsonl outcome.json driver.pid";

/// The script that runs on the controller and lists its runs whose ids start with one of
/// `prefixes`, or all: its health, as `up <seconds>`, `boot <property>=<value>` and `disk <used>%`;
/// each job yet to start its run, as `<id> waiting`, `<id> building` or `<id> unstarted`; each run
/// that its standard input (`holding`) doesn't hold as ended, as `<id> <live or ended> <byte>`, or
/// `<id> old` if nothing of it or its job was written since Unix second `since` and no copy is
/// held; then `tar` and an archive of the `LISTED_FILES` that the copy lacks, with the log from
/// `<byte>`, which is the copy's length if it is over 0 and the log is that long, else 0.
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
sent=$(mktemp --directory "$XDG_RUNTIME_DIR/pumpkin-listed.XXXXXX")
trap 'rm --recursive --force "${{sent:?}}"' EXIT
trap 'exit 1' HUP INT PIPE TERM
for id in $(ls "$runs" 2>/dev/null); do
    {picked}
    [ -e "$runs/$id/record.json" ] || continue
    if [ ! -e "$runs/$id/outcome.json" ] && ! flock --nonblock --shared "$runs/$id/driver.pid" true 2>/dev/null; then
        state=live
    else
        state=ended
        if printf '%s\n' "$held" | grep --quiet --line-regexp --fixed-strings "$id"; then continue; fi
    fi
    copy=$(printf '%s\n' "$held" | grep --max-count=1 "^$id " || true)
    if [ "$state" = ended ] && [ -z "$copy" ] && ! recent "$runs/$id/outcome.json"; then echo "$id old"; continue; fi
    log="$runs/$id/log.jsonl"
    from=0
    holds=
    if [ -n "$copy" ]; then
        set -- $copy
        if [ "$2" -gt 0 ] && [ "$2" -le "$(stat --format=%s "$log" 2>/dev/null || echo 0)" ]; then from=$2; shift 2; holds=" $* "; fi
    fi
    echo "$id $state $from"
    mkdir "$sent/$id"
    for file in {small}; do
        case $holds in *" $file "*) continue ;; esac
        [ ! -e "$runs/$id/$file" ] || ln --symbolic "$HOME/$runs/$id/$file" "$sent/$id/$file"
    done
    part="$sent/$id/log.jsonl"
    if [ -e "$log" ] && ! tail --bytes=+$((from + 1)) "$log" > "$part"; then
        full=
        [ "$(df --output=avail "$XDG_RUNTIME_DIR" | tail --lines=1)" -gt 0 ] || full=" $XDG_RUNTIME_DIR, where the listing is put together, is full: free some of it, then"
        echo "reading the log of run $id for the listing failed, as said above:$full run the command again" >&2
        exit 1
    fi
    if [ "$state" = live ] && [ -s "$part" ] && [ -n "$(tail --bytes=1 "$part")" ]; then
        truncate --size=$(($(stat --format=%s "$part") - $(tail --lines=1 "$part" | wc --bytes))) "$part"
    fi
    [ ! -e "$part" ] || [ -s "$part" ] || rm "$part"
    rmdir --ignore-fail-on-non-empty "${{sent:?}}/$id"
done
echo tar
ls "$sent" | tar --create --blocking-factor=1 --dereference --warning=no-file-changed --directory="$sent" --file=- --files-from=- || [ $? -eq 1 ]
"#,
        small = LISTED_FILES.replace(" log.jsonl", ""),
        runs = crate::job::runs()
    )
}

/// Unpacks into `sent` the archive that follows the line `tar` of what `listing`'s script
/// printed, and returns the lines before it.
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
    let mut tar = untar(sent)?;
    let extracted = stdx::process::fed(&mut tar, archive, Deadline::after(SHORT_TIMEOUT));
    finished(extracted, "tar --extract", SHORT_TIMEOUT)?;
    Ok(&printed[..at])
}
