//! `pumpkin ls` on the controller: the controller lists its runs, and its jobs yet to start one,
//! with its own health, and sends the files of each record that `ls` reads, from which this
//! machine reads each run as it reads its own.
//!
//! The files of a record that ended are kept in the workspace's connections to the controller
//! ([`super::Shared::ended`]), as they never change, so the controller sends them once. What a
//! listing shows, and how, is [`crate::progress`]'s.

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
            // The lock that says a run is live is the controller's, so its copy here is read
            // as live.
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

/// The record of run `id` that ended, as `ls` keeps it in `live` ([`grown`]), moved into `kept`,
/// which [`Shared::ended`](super::Shared::ended) names, and read there.
fn keep(live: &Path, kept: &Path, id: &RunTag) -> Result<runtime::Record, String> {
    let to = kept.join(id.as_str());
    let moved = fs::create_dir_all(kept).and_then(|()| fs::rename(live.join(id.as_str()), &to));
    moved.map_err(|error| format!("{}: {error}: {}", to.display(), afresh(live, id)))?;
    runtime::Record::open(&to)
}

/// What to do once `ls` failed to keep its copy of run `id` in `live`.
fn afresh(live: &Path, id: &RunTag) -> String {
    let copy = live.join(id.as_str());
    format!(
        "remove {}, and run `ls` again, which is then sent the run whole",
        copy.display()
    )
}

/// Run `id`, which controller `name` listed, once its copy in `live` is grown by what was `sent`
/// of it ([`grown`]); refuses an `id` that is no run's, and so names no copy here.
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

/// What [`listing`] prints on `reached` of the runs whose ids start with one of `prefixes`, or
/// every run without any, of those that ended at `since` or after, in seconds since the Unix
/// epoch, given what this machine holds of them, `held` ([`holding`]).
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

/// What [`listing`] reads on its standard input of what this machine holds: a line `<id>` for each
/// run that `listing`, which [`listed_here`] made, has ended or older, then one for each copy in
/// `live` ([`Shared::live`](super::Shared::live)), `<id> <byte> <file>...`: the length of its
/// log, as [`whole`] cuts it, then each of [`LISTED_FILES`] but the log that it holds.
/// Removes each copy in `live` of a run that `listing` has ended or older, as `wait` copied it
/// since.
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

/// How many bytes of a copy's log [`whole`] reads at a time, from its end back, to find its last
/// newline: more than most of a log's lines hold, and a longer line takes another read.
const TAIL_BYTES: u64 = 4096;

/// The length of `log`, a copy of a run's log, once what follows its last newline, if anything,
/// is cut off; 0 if there is none.
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

/// Grows the copy of run `id` in `live` by what [`listing`] `sent` of it, whose log starts at byte
/// `from` of the controller's: appends that to the copy's log, and moves in each other file; or
/// makes what was sent the copy, if there is none, or `from` is 0 or none, as the controller then
/// sent the run whole.
fn grown(sent: &Path, live: &Path, id: &RunTag, from: Option<u64>) -> Result<(), String> {
    let (sent, copy) = (sent.join(id.as_str()), live.join(id.as_str()));
    let entries = sent_files(&sent, id)?;
    let grew = growing(&sent, &copy, entries, from);
    grew.map_err(|error| format!("{error}: {}", afresh(live, id)))
}

/// The files that [`listing`] sent of run `id`, in `sent`, none if it sent nothing; refuses what
/// isn't a directory of regular files, as no run's record is.
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

/// What [`grown`] does with the `entries` of `sent`, but for saying what to do once it fails.
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
/// `prefixes`, or every run without any: its health, `up <seconds>`, `boot <property>=<value>` of
/// its boot's unit, and `disk <used>%`; for each job yet to start its run, `<id> waiting` for a
/// build slot, or else `<id> building`, or `<id> unstarted` once it has ended; for each run that
/// its standard input ([`holding`]) does not hold as ended, `<id> <live or ended> <byte>`, or
/// `<id> old` if it, or its job, ended before `since`, as its `outcome.json`, or its log, was last
/// written, and it holds no copy of; then `tar`, and an archive of each run's files that `ls`
/// reads ([`LISTED_FILES`]): its log from `<byte>`, its copy's length if over 0 and its log is
/// that long, else 0 and every file, to its last newline if live, and the files its copy lacks.
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
    let mut tar = untar(sent)?;
    let extracted = stdx::process::fed(&mut tar, archive, Deadline::after(SHORT_TIMEOUT));
    finished(extracted, "tar --extract", SHORT_TIMEOUT)?;
    Ok(&printed[..at])
}
