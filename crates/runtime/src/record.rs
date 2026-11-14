//! A run's record on disk: its directory and name, the payloads stored by digest, and the
//! files written whole.
//!
//! Each run has a directory of its own, `<runs>/<id>/`, which [`create`] makes and names. It
//! holds:
//!
//! - `driver.pid`: the pid of the process that drives the run, locked by it until the run's last
//!   write: see [`Lock`] for the rule that makes the record's state readable;
//! - `driver.log`: what the driver wrote to its standard output and error once the run started;
//! - `record.json`, written first: the version of the record's form ([`RECORD_VERSION`]) and the
//!   build that made it ([`BUILD`]);
//! - `ir.json`: the workflow's IR;
//! - `provider.json`: what the run's provider declared ([`Ready`](provider::Ready));
//! - `log.jsonl`: the run log, which the `log` module lays out;
//! - `payloads/<hex>`: the payload of each op the core accepted, named by its SHA-256; a refused
//!   op's bytes aren't kept;
//! - `agent/`, for an agent whose sandbox started: its `out` and `log`, what the sandbox held in
//!   its [`out_dir()`](schema::sandbox::out_dir) and [`log_dir()`](schema::sandbox::log_dir) as
//!   the work phase ended, as far as collected; the run log notes what is incomplete;
//! - `jobs/<n>/`, for job `n`'s judge call: its `out` and `log`, what the judge's sandbox held in
//!   them once its command ended, as far as they were collected, but a result over
//!   [`RESULT_MAX_BYTES`](schema::sandbox::RESULT_MAX_BYTES), which is removed;
//! - `outcome.json`: how the run ended: `{"state": "finished"}` and the outcome's fields, in
//!   the run log's JSON form of an outcome (`log::outcome_json`); or
//!   `{"state": "failed", "reason": r}`.

use std::fmt;
use std::fs::{self, File, TryLockError};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use decision::{JobId, PayloadRef};
use schema::ir::Sha256Digest;
use schema::sandbox::{command_stderr, log_dir, out_dir};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use stdx::Context;

use super::{Durable, Host, log};

/// The version of the record's form, named as the IR's version is
/// ([`IrVersion`](schema::ir::IrVersion)), which a reader checks before it reads the rest: a
/// change to the form, a new file included, takes a new version. A record's `record.json` is
/// `{"record_version": v, "pumpkin": p}`: this version, and the build that made it ([`BUILD`]),
/// `{"version": v, "commit": c}`, with `c` `{"hash": h, "date": d}` or `null`.
const RECORD_VERSION: &str = "pumpkin.record/v0";

/// This build of Pumpkin, which `pumpkin --version` prints, and each record names. Its version
/// is the workspace's, which each of Pumpkin's crates takes.
pub const BUILD: Build = Build {
    version: env!("CARGO_PKG_VERSION"),
    commit: commit(env!("PUMPKIN_COMMIT_HASH"), env!("PUMPKIN_COMMIT_DATE")),
};

/// A build of Pumpkin: its version, and the commit it was built from.
///
/// A build has a commit if it was made in a git checkout of Pumpkin's repository. It is shown
/// as its version alone, `0.0.0`, or with the commit's first 9 hexadecimal digits and its date,
/// `0.0.0 (0f3a2b1c9 2026-11-08)`.
#[derive(Clone, Copy, Debug)]
pub struct Build {
    version: &'static str,
    commit: Option<Commit>,
}

/// A commit of Pumpkin's repository.
#[derive(Clone, Copy, Debug)]
struct Commit {
    /// Its hash, in hexadecimal digits.
    hash: &'static str,
    /// The day it was committed, as `YYYY-MM-DD`.
    date: &'static str,
}

/// The commit whose `hash` and `date` the build script found, or none if it gives both empty.
const fn commit(hash: &'static str, date: &'static str) -> Option<Commit> {
    // A constant that fails this fails to compile.
    assert!(
        hash.is_empty() == date.is_empty(),
        "the build script sets the commit's hash and date on every build, each empty if and only \
         if it found no commit"
    );
    if hash.is_empty() {
        None
    } else {
        Some(Commit { hash, date })
    }
}

impl Build {
    /// Its JSON form in `record.json`.
    fn json(self) -> Value {
        let Self { version, commit } = self;
        let commit = commit.map(|Commit { hash, date }| json!({ "hash": hash, "date": date }));
        json!({ "version": version, "commit": commit })
    }
}

impl fmt::Display for Build {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { version, commit } = self;
        match commit {
            Some(Commit { hash, date }) => write!(f, "{version} ({hash:.9} {date})"),
            None => f.write_str(version),
        }
    }
}

/// The lock on a record's `driver.pid`, an exclusive `flock` held until it is dropped, by the
/// process that drives the run.
///
/// Only the holder writes the record, and drops the lock after the record's last write,
/// `outcome.json`. So a record that has an `outcome.json` has ended, one whose lock is held is
/// live, and one whose lock is free and has no `outcome.json` is lost: its driver ended without
/// one. The lock goes when the process does, however it ends, and `docker` children don't
/// inherit it, as [`File`]s are close-on-exec.
#[derive(Debug)]
pub(super) struct Lock {
    _file: File,
}

/// Makes a new record directory in `runs`, creating `runs` if need be, and returns the run's
/// id, which names the directory: `now` as a UTC time, and 8 random hexadecimal digits, as
/// `20261028-131811-1a2b3c4d`, which sorts by time, with its `record.json` and its locked
/// `driver.pid`. Syncs each directory that gains an entry on `host`, so that the record's
/// directory lasts. Fails, before it makes anything, if `now` is before the Unix epoch.
///
/// The record is made as `.<id>.partial` and renamed to `<id>` once it is locked, so that
/// nothing that reads `runs` sees a record before its lock.
pub(super) fn create(
    host: &impl Host,
    runs: &Path,
    now: SystemTime,
) -> Result<(String, PathBuf, Lock), String> {
    let seconds = since_epoch(now)?.as_secs();
    create_dirs(host, runs)?;
    let mut random = [0; 4];
    File::open("/dev/urandom")
        .and_then(|mut urandom| urandom.read_exact(&mut random))
        .context("reading /dev/urandom for the run's id failed")?;
    let id = format!("{}-{:08x}", utc_stamp(seconds), u32::from_be_bytes(random));
    let partial = runs.join(format!(".{id}.partial"));
    fs::create_dir(&partial).context(partial.display())?;
    let dir = runs.join(&id);
    match fill(host, &partial, &dir, runs) {
        Ok(lock) => Ok((id, dir, lock)),
        // What was made in `partial` is no record, and nothing else would remove it.
        Err(error) => match fs::remove_dir_all(&partial) {
            Ok(()) => Err(error),
            Err(removing) if removing.kind() == io::ErrorKind::NotFound => Err(error),
            Err(removing) => Err(format!(
                "{error}; {} is left: {removing}",
                partial.display()
            )),
        },
    }
}

/// Writes the files of a new record into `partial`, which is empty, locks its `driver.pid`, and
/// renames `partial` to `dir`, in `runs`, syncing on `host`.
fn fill(host: &impl Host, partial: &Path, dir: &Path, runs: &Path) -> Result<Lock, String> {
    write_json(
        host,
        partial,
        "record.json",
        &json!({ "record_version": RECORD_VERSION, "pumpkin": BUILD.json() }),
    )?;
    let log = partial.join("driver.log");
    File::create(&log).context(log.display())?;
    let pid_path = partial.join("driver.pid");
    let mut pid = File::create(&pid_path).context(pid_path.display())?;
    pid.lock().context(pid_path.display())?;
    writeln!(pid, "{}", std::process::id())
        .and_then(|()| host.sync(&pid, Durable::All))
        .context(pid_path.display())?;
    sync_dir(host, partial)?;
    // A record directory is never empty, so the rename fails rather than replace one that an id
    // collision names.
    fs::rename(partial, dir).context(dir.display())?;
    sync_dir(host, runs)?;
    Ok(Lock { _file: pid })
}

/// A run's record on disk, found to be read: [`wait`](Self::wait) follows it to its end.
#[derive(Debug)]
pub struct Record {
    id: String,
    dir: PathBuf,
}

/// How a run ended, as its `outcome.json` says, each with a sentence that follows the run's id
/// and says how in words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ending {
    /// The run finished with a score: `finished with a score of <score>, after the work phase
    /// ended <how>`.
    Scored(String),
    /// The run finished without a score, as the agent submitted nothing or the verdict has
    /// none: `finished without a score: <why>`.
    Unscored(String),
    /// The run finished without a score as its final judge returned no result: `finished
    /// without a score: <why>`. `judge_stderr` is where the record holds that judge's standard
    /// error, which may not have been collected, or why the run log doesn't say.
    NoResult {
        why: String,
        judge_stderr: Result<PathBuf, String>,
    },
    /// The run failed: `failed: <reason>`.
    Failed(String),
}

impl Record {
    /// The record in the directory `dir`.
    ///
    /// # Errors
    ///
    /// Fails if `dir` is no directory, has no `record.json`, or has one that is not a record of
    /// this version of Pumpkin, or if `dir` is relative and the current directory can't be read.
    pub fn open(dir: &Path) -> Result<Self, String> {
        let Some(id) = dir.file_name().and_then(|name| name.to_str()) else {
            return Err(format!(
                "`{}` names no directory: expected the directory of a run's record",
                dir.display()
            ));
        };
        let path = dir.join("record.json");
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound && !dir.is_dir() => {
                return Err(format!(
                    "{} is not a directory: expected the directory of a run's record",
                    dir.display()
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(format!(
                    "{} has no record.json, so it is no run's record: expected the directory \
                     of one that `pumpkin run` made",
                    dir.display()
                ));
            }
            Err(error) => return Err(error).context(path.display()),
        };
        let record: serde_json::Value = serde_json::from_slice(&bytes).context(path.display())?;
        if record["record_version"] != RECORD_VERSION {
            return Err(format!(
                "{} holds record_version {}: expected \"{RECORD_VERSION}\", which this \
                 `pumpkin` reads",
                path.display(),
                record["record_version"]
            ));
        }
        let absolute = std::path::absolute(dir).map_err(|error| {
            format!(
                "the record {} can't be made absolute ({error}): name it by its absolute path",
                dir.display()
            )
        })?;
        Ok(Self {
            id: id.to_owned(),
            dir: absolute,
        })
    }

    /// The record that `run`, a record's directory name, a prefix of it or of its 8 random
    /// digits that no other record's name shares, names in `runs`.
    ///
    /// # Errors
    ///
    /// Fails if `run` is empty, if it names no record or several, which the error lists, if
    /// `runs` can't be read, or as [`open`](Self::open) does.
    pub fn find(runs: &Path, run: &str) -> Result<Self, String> {
        if run.is_empty() {
            return Err("the run is empty: name an id, a prefix of one, or a record's path".into());
        }
        let entries = fs::read_dir(runs).map_err(|error| {
            format!(
                "{}: {error}: expected the directory that runs are recorded in, as `--runs` \
                 names it",
                runs.display()
            )
        })?;
        let mut found = Vec::new();
        for entry in entries {
            let entry = entry.context(runs.display())?;
            let is_dir = entry.file_type().context(runs.display())?.is_dir();
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            // A name that starts with `.` is a record in the making, which isn't there yet.
            if name.starts_with('.') || !is_dir {
                continue;
            }
            let random = name
                .rsplit_once('-')
                .map_or(name.as_str(), |(_, random)| random);
            if name.starts_with(run) || random.starts_with(run) {
                found.push(name);
            }
        }
        found.sort();
        let [id] = found.as_slice() else {
            return Err(if found.is_empty() {
                format!(
                    "no run in {} matches `{run}`: name an id, a unique prefix of an id or of \
                     its random digits, or a record's path",
                    runs.display()
                )
            } else {
                let count = found.len();
                let listed = found.join("\n  ");
                format!("`{run}` matches {count} runs: name one by more of its id\n  {listed}")
            });
        };
        Self::open(&runs.join(id))
    }

    /// The run's id, which is the name of its directory.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The record's directory, absolute.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Waits until the run has ended, and returns how: at once if its `outcome.json` is there.
    /// Calls `following` first if the run is live, which is to say that the wait is a long one.
    ///
    /// # Errors
    ///
    /// Fails if the record has no `driver.pid`, as a record made before records were locked; if
    /// the run is lost, its driver having ended without an `outcome.json`; if `outcome.json` is
    /// not one this build knows; or if it or the lock can't be read.
    pub fn wait(&self, following: impl FnOnce()) -> Result<Ending, String> {
        let pid_path = self.dir.join("driver.pid");
        let pid = match File::open(&pid_path) {
            Ok(pid) => pid,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(format!(
                    "{} has no driver.pid: it was made before records were locked, which \
                     `wait` needs: name a record that this version of `pumpkin run` made",
                    self.dir.display()
                ));
            }
            Err(error) => return Err(error).context(pid_path.display()),
        };
        // An `outcome.json` is the record's last write, so it decides before the lock does.
        if let Some(ending) = self.ended()? {
            return Ok(ending);
        }
        match pid.try_lock_shared() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                following();
                pid.lock_shared().context(pid_path.display())?;
            }
            Err(TryLockError::Error(error)) => return Err(error).context(pid_path.display()),
        }
        // The lock is free: the driver wrote its `outcome.json`, if it did, before it let go.
        self.ended()?.ok_or_else(|| {
            format!(
                "run {id} is lost: its driver ended without writing outcome.json, so nothing says \
                 how it ended: remove what it left, which is its containers, labelled \
                 `pumpkin.run={id}`, and its socket directory, `pumpkin-{id}` in the temporary \
                 directory; its record is {dir}",
                id = self.id,
                dir = self.dir.display(),
            )
        })
    }

    /// How the run ended, if it has an `outcome.json`.
    fn ended(&self) -> Result<Option<Ending>, String> {
        let path = self.dir.join("outcome.json");
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context(path.display()),
        };
        let outcome: serde_json::Value = serde_json::from_slice(&bytes).context(path.display())?;
        let reason = outcome["reason"]
            .as_str()
            .unwrap_or("no reason is recorded");
        match outcome["state"].as_str() {
            Some("finished") => Ok(Some(finished(&outcome, &self.dir))),
            Some("failed") => Ok(Some(Ending::Failed(format!("failed: {reason}")))),
            Some(_) | None => Err(format!(
                "{}'s state is not one pumpkin knows: expected `finished` or `failed`",
                path.display()
            )),
        }
    }
}

/// How `outcome`, a finished run's `outcome.json`, says the run ended: with its score, or why it
/// has none, from its verdict, and how its work phase ended; its record is in `dir`.
fn finished(outcome: &serde_json::Value, dir: &Path) -> Ending {
    let work_end = &outcome["work_end"];
    let status = &work_end["status"];
    let ended = match work_end["kind"].as_str() {
        Some("deadline") => "at its deadline".to_owned(),
        Some("finalized") => "when the agent finalized".to_owned(),
        Some("agent_exited") => match (status["code"].as_i64(), status["signal"].as_i64()) {
            (Some(code), _) => format!("when the agent exited with status {code}"),
            (None, Some(signal)) => format!("when signal {signal} killed the agent"),
            (None, None) => "when the agent exited".to_owned(),
        },
        Some("agent_failed") => {
            let reason = work_end["reason"]
                .as_str()
                .unwrap_or("no reason is recorded");
            format!("when the agent failed: {reason}")
        }
        Some(_) | None => "in a way this pumpkin doesn't know".to_owned(),
    };
    let score = &outcome["score"];
    if !score.is_null() {
        return Ending::Scored(format!(
            "finished with a score of {score}, after the work phase ended {ended}"
        ));
    }
    let unscored = |why: String| format!("finished without a score: {why}");
    match outcome["verdict"]["kind"].as_str() {
        Some("no_submission") => Ending::Unscored(unscored(format!(
            "the agent submitted nothing, and the work phase ended {ended}"
        ))),
        Some("no_result") => {
            let failure = &outcome["verdict"]["failure"];
            let kind = failure["kind"].as_str().unwrap_or("of an unknown kind");
            let detail = failure["reason"]
                .as_str()
                .map_or_else(String::new, |r| format!(": {r}"));
            Ending::NoResult {
                why: unscored(format!(
                    "the final judge returned no result ({kind}{detail}), after the work phase \
                     ended {ended}"
                )),
                judge_stderr: final_judge_stderr(dir),
            }
        }
        Some(_) | None => Ending::Unscored(unscored(format!(
            "the verdict has no score, after the work phase ended {ended}"
        ))),
    }
}

/// Where the record in `dir` holds the final judge's standard error,
/// `jobs/<n>/log/command.stderr`, for the job `n` that its run log names
/// ([`log::final_judge`]), or why the run log doesn't say.
fn final_judge_stderr(dir: &Path) -> Result<PathBuf, String> {
    let JobId(job) = log::final_judge(dir)?;
    let job = dir.join(format!("jobs/{job}"));
    Ok(job.join(command_stderr().as_str()))
}

/// Makes `dir` and those of its ancestors that are missing, as [`fs::create_dir_all`] does,
/// and syncs the parent of each directory it makes on `host`, so that each lasts.
fn create_dirs(host: &impl Host, dir: &Path) -> Result<(), String> {
    let missing: Vec<&Path> = (dir.ancestors())
        .take_while(|path| !path.as_os_str().is_empty() && !path.exists())
        .collect();
    fs::create_dir_all(dir).context(dir.display())?;
    for made in missing.into_iter().rev() {
        // The parent of a relative path's first component is the empty path: the current
        // directory.
        let parent = made
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty());
        sync_dir(host, parent.unwrap_or(Path::new(".")))?;
    }
    Ok(())
}

/// The time since the Unix epoch of `time`, a time by the machine's clock: a time before the
/// epoch is the clock's error.
pub(super) fn since_epoch(time: SystemTime) -> Result<Duration, String> {
    (time.duration_since(UNIX_EPOCH))
        .map_err(|_| "the clock reads before 1970-01-01: set the system clock".to_owned())
}

/// `seconds` since the Unix epoch as `YYYYMMDD-hhmmss`, in UTC.
fn utc_stamp(seconds: u64) -> String {
    let (days, time) = (seconds / 86_400, seconds % 86_400);
    // Days to a civil date, after Howard Hinnant's `civil_from_days`, for days from 1970 on.
    let shifted = days + 719_468;
    let (era, day_of_era) = (shifted / 146_097, shifted % 146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    let (hour, minute, second) = (time / 3600, time % 3600 / 60, time % 60);
    format!("{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}")
}

/// Writes `value` as the file `name` in `dir`, a directory of the record, in pretty-printed
/// JSON, by [`replace`] on `host`.
pub(super) fn write_json(
    host: &impl Host,
    dir: &Path,
    name: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    replace(host, dir, name, format!("{value:#}\n").as_bytes())
}

/// Writes `bytes` as the file `name` in `dir`, whole or not at all, even if the machine stops:
/// to a temporary file in `dir`, synced on `host`, then renamed over `name`, and `dir` synced.
/// The temporary file is removed if it can't be written or renamed.
fn replace(host: &impl Host, dir: &Path, name: &str, bytes: &[u8]) -> Result<(), String> {
    let temporary = dir.join(format!(".{name}.partial"));
    let path = dir.join(name);
    let written = File::create(&temporary).and_then(|mut file| {
        file.write_all(bytes)?;
        host.sync(&file, Durable::All)
    });
    let renamed = (written.context(temporary.display()))
        .and_then(|()| fs::rename(&temporary, &path).context(path.display()));
    if let Err(error) = renamed {
        // A temporary file that wasn't renamed holds nothing the record needs.
        return match fs::remove_file(&temporary) {
            Ok(()) => Err(error),
            Err(removing) if removing.kind() == io::ErrorKind::NotFound => Err(error),
            Err(removing) => Err(format!(
                "{error}; {} is left: {removing}",
                temporary.display()
            )),
        };
    }
    sync_dir(host, dir)
}

/// The reference to an op's payload: its size and SHA-256.
pub(super) fn reference(bytes: &[u8]) -> PayloadRef {
    let sha256: [u8; 32] = Sha256::digest(bytes).into();
    PayloadRef {
        size: bytes.len() as u64,
        digest: Sha256Digest::from(sha256),
    }
}

/// The payloads of the ops a run accepted, each stored once in `payloads/`, under its SHA-256
/// in hexadecimal, and synced on the run's host, `H`.
pub(super) struct Payloads<'h, H> {
    dir: PathBuf,
    host: &'h H,
}

impl<'h, H: Host> Payloads<'h, H> {
    /// Makes `payloads/` in the record's directory `record`, and syncs `record` on `host`, so
    /// that it lasts.
    pub(super) fn create(host: &'h H, record: &Path) -> Result<Self, String> {
        let dir = record.join("payloads");
        fs::create_dir(&dir).context(dir.display())?;
        sync_dir(host, record)?;
        Ok(Self { dir, host })
    }

    /// Stores `bytes`, whose reference is `payload`, by [`replace`], unless they are stored
    /// already: a file has a digest's name only once it holds all of its bytes.
    pub(super) fn store(&self, bytes: &[u8], payload: &PayloadRef) -> Result<(), String> {
        let path = self.path(&payload.digest);
        match fs::symlink_metadata(&path) {
            Ok(_) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                replace(self.host, &self.dir, payload.digest.hex(), bytes)
            }
            Err(error) => Err(error).context(path.display()),
        }
    }

    /// Where the payload whose digest is `digest` is stored.
    pub(super) fn path(&self, digest: &Sha256Digest) -> PathBuf {
        self.dir.join(digest.hex())
    }
}

/// What `dir`, where a sandbox's [`out_dir()`] and [`log_dir()`] were collected, holds of each,
/// for the run log: `out, 0 files of 0 bytes; log, 2 files of 481 bytes`.
pub(super) fn collected(dir: &Path) -> String {
    let parts = [out_dir(), log_dir()].map(|part| match tree_size(&dir.join(part.as_str())) {
        Ok(TreeSize { files, bytes }) => {
            let noun = if files == 1 { "file" } else { "files" };
            format!("{part}, {files} {noun} of {bytes} bytes")
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => format!("{part}, nothing"),
        Err(error) => format!("{part}, which can't be read: {error}"),
    });
    parts.join("; ")
}

/// What the files below a directory hold, in all.
#[derive(Clone, Copy, Debug, Default)]
struct TreeSize {
    /// The entries that aren't directories.
    files: u64,
    bytes: u64,
}

/// What the files below `dir` hold, in all.
fn tree_size(dir: &Path) -> io::Result<TreeSize> {
    let mut size = TreeSize::default();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            let below = tree_size(&entry.path())?;
            size.files += below.files;
            size.bytes += below.bytes;
        } else {
            size.files += 1;
            size.bytes += metadata.len();
        }
    }
    Ok(size)
}

/// Syncs `dir` on `host`, so that the entries made in it, or renamed into it, last.
pub(super) fn sync_dir(host: &impl Host, dir: &Path) -> Result<(), String> {
    File::open(dir)
        .and_then(|dir| host.sync(&dir, Durable::All))
        .context(dir.display())
}
