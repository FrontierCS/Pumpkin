//! A run's record: the directory, named by the run's id, in the runs directory, and the files in
//! it, which [`Record`] reads back. `ARCHITECTURE.md` says when each is written and who may write
//! it.
//!
//! - `record.json`, written first: `record_version`, which a reader refuses if it isn't its own;
//!   `args`, the words that started the run; `pumpkin`, the build that made it, `{"version",
//!   "commit"}`, with the commit `{"hash", "date"}` or null, which an older record lacks.
//! - `driver.pid`, locked while the driver lives and holding its process ID; `driver.log`, the
//!   driver's standard output and error since the run started.
//! - `ir.json`, the workflow's IR; `provider.json`, written before the log: `providers`, each with
//!   its `report` and `classes` (`startup_seconds`, and `usd_per_hour` if priced); `roles`, each
//!   with its `provider` and `class`; and `routes`, each role's entries (`api`, `upstream` and
//!   `models`, each with its `upstream_name` and `usd_per_mtok` if priced), which an older record
//!   lacks.
//! - `log.jsonl`, the run log (`log`); `payloads/<digest>`, the payload of each op the core
//!   accepted, by the SHA-256 of its bytes; `model/`, each model exchange (`model`).
//! - `agent/`, `jobs/<n>/` and `leases/<n>/`, a sandbox each, with its `sandbox.json`: `provider`,
//!   `id`, `gpus` once known, and `delete.state`, `deleted` or `failed`, once tried; its `out/`,
//!   `log/` and a body's `got/` as far as collected.
//! - `outcome.json`, the run's end: `state` is `finished`, with what `log::outcome_json` gives,
//!   `failed`, with a `reason`, or `interrupted`, with a `signal`; and `usage`, if the run began
//!   its log, which an older record lacks.
//! - `reaps.jsonl`, a line for each sandbox a reap deleted; `sockets/`, where private sandboxes
//!   meet the gateway, which a reap removes.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, DirBuilder, File, TryLockError};
use std::io::{self, Read, Write};
use std::ops::Range;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use decision::PayloadRef;
use jiff::Timestamp;
use jiff::civil::DateTime;
use jiff::tz::Offset;
use provider::{Provider, Ready, RunTag, SandboxId, Sweep, UnixTime};
use route::Serving;
use schema::ir::{self, Api, GpuModel, ModelName, Name, Sha256Digest};
use schema::sandbox::{log_dir, out_dir};
use schema::workflow::{ByProvider, ByRole, Class, Placed, Placement, Workflow};
use serde_json::{Map, Value, json};
use stdx::Context;

use super::log::{self, LogFold, Place, Readout, Used};
use super::model::{KeptPart, KeptRequest};
use super::read_error::{ReadError, failed, form, open_required, read_optional};
use super::sandbox::sweep;
use super::{Durable, Host, Signal, Stopped, System, TokenPrices};

mod ending;
mod payload;
mod writer;

pub use ending::Ending;
use ending::{State, finished, read_outcome};
pub(super) use payload::reference;
pub use writer::{Writer, writer};
use writer::{read_record_json, writer_of};

const LOST: &str = "its driver ended without an outcome";

// A build refuses a record of another version instead of misreading it. Until Pumpkin's first
// release the form changes under this version, so an older record may lack what a newer one has.
const RECORD_VERSION: &str = "pumpkin.record/v0";

/// This build of Pumpkin, which `pumpkin --version` prints, and each record names.
pub const BUILD: Build = Build {
    version: env!("CARGO_PKG_VERSION"),
    commit: commit(env!("PUMPKIN_COMMIT_HASH"), env!("PUMPKIN_COMMIT_DATE")),
};

/// A build of Pumpkin: its version, and the commit it was built from if it was made in a git
/// checkout of Pumpkin's repository.
///
/// It is shown as its version alone, `0.0.0`, or with the commit's first 9 hexadecimal digits and
/// its date, `0.0.0 (0f3a2b1c9 2026-11-08)`.
#[derive(Clone, Copy, Debug)]
pub struct Build {
    version: &'static str,
    commit: Option<Commit>,
}

#[derive(Clone, Copy, Debug)]
struct Commit {
    hash: &'static str,
    date: &'static str,
}

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
    /// The hash of the commit it was built from, if there was one.
    #[must_use]
    pub fn commit_hash(self) -> Option<&'static str> {
        self.commit.map(|commit| commit.hash)
    }

    fn json(self) -> Value {
        let Self { version, commit } = self;
        let commit = commit.map(|Commit { hash, date }| json!({ "hash": hash, "date": date }));
        json!({ "version": version, "commit": commit })
    }
}

impl fmt::Display for Build {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { version, commit } = self;
        let commit = commit.map(|Commit { hash, date }| (hash, date));
        write_build(f, version, commit)
    }
}

fn write_build(
    f: &mut impl fmt::Write,
    version: &str,
    commit: Option<(&str, &str)>,
) -> fmt::Result {
    match commit {
        Some((hash, date)) => write!(f, "{version} ({hash:.9} {date})"),
        None => f.write_str(version),
    }
}

/// The lock on a record's `driver.pid`, an exclusive `flock` held until it is dropped, by the
/// process that drives the run.
///
/// Only the holder writes the record, and drops the lock after its last write, `outcome.json`: so a
/// record with one has ended, one whose lock is held is live, and one whose lock is free without
/// one is lost, which only [`Record::reap`] writes then. The lock goes with the process, however it
/// ends, and `docker` children don't inherit it, as `File`s are close-on-exec.
#[derive(Debug)]
pub(super) struct Lock {
    _file: File,
}

/// A new run id: the UTC time `now` as `YYYYMMDD-HHMMSS`, then `-` and eight random lowercase
/// hexadecimal digits, so that ids sort by when they were made.
///
/// Fails if `now` is before the Unix epoch or the system's randomness can't be read.
pub fn new_id(now: SystemTime) -> Result<RunTag, String> {
    UnixTime::of(now)?;
    let stamp = Timestamp::try_from(now).context("the clock reads past what ids can name")?;
    let mut random = [0; 4];
    File::open("/dev/urandom")
        .and_then(|mut urandom| urandom.read_exact(&mut random))
        .context("reading /dev/urandom for the run's id failed")?;
    let id = format!(
        "{}-{:08x}",
        stamp.strftime(STAMP),
        u32::from_be_bytes(random)
    );
    match RunTag::try_from(id.clone()) {
        Ok(tag) => Ok(tag),
        Err(error) => unreachable!(
            "run {id}'s id is no run tag, though it is a UTC time's digits and `-`, then \
             lowercase hexadecimal digits, all of which a run tag allows, and starts with a \
             digit: {error}"
        ),
    }
}

/// The time in an id's first 15 characters.
const STAMP: &str = "%Y%m%d-%H%M%S";

/// When `id` was made, since the Unix epoch, to the second, as [`new_id`] wrote it into the id;
/// `None` if `id` isn't of its form.
pub fn minted(id: &str) -> Option<Duration> {
    let stamp = id.get(..15)?;
    let at = DateTime::strptime(STAMP, stamp).ok()?;
    // `strptime` reads more than `new_id` writes (a sign, fewer digits, a leap second): only a
    // time that writes back as the same characters is one `new_id` wrote.
    (at.strftime(STAMP).to_string() == stamp).then_some(())?;
    let seconds = Offset::UTC.to_timestamp(at).ok()?.as_second();
    Some(Duration::from_secs(u64::try_from(seconds).ok()?))
}

/// The one of `names`, the ids of runs, that `run` names: a prefix of an id or of its random
/// digits. Fails, saying so in terms of `among`, where the names are, if none matches or several
/// do.
pub fn named<'n>(names: &'n [String], run: &str, among: &str) -> Result<&'n str, String> {
    if run.is_empty() {
        return Err("the run is empty: name an id, a prefix of one, or a record's path".into());
    }
    let found = names.iter().filter(|name| {
        let random = name
            .rsplit_once('-')
            .map_or(name.as_str(), |(_, random)| random);
        name.starts_with(run) || random.starts_with(run)
    });
    let found: Vec<&String> = found.collect();
    let [id] = found.as_slice() else {
        return Err(if found.is_empty() {
            format!(
                "no run {among} matches `{run}`: name an id, a unique prefix of an id or of its \
                 random digits, or a record's path"
            )
        } else {
            let count = found.len();
            let listed: Vec<&str> = found.iter().map(|name| name.as_str()).collect();
            let listed = listed.join("\n  ");
            format!("`{run}` matches {count} runs: name one by more of its id\n  {listed}")
        });
    };
    Ok(id)
}

/// Makes the record directory of run `id` in `runs`, creating `runs` if need be, with its
/// `record.json` and its locked `driver.pid`, syncing on `host` each directory that gains an entry;
/// returns its path, in the canonical path of `runs`.
///
/// It is made as `.<id>.partial`, renamed to `<id>` once locked, so that no reader of `runs` sees
/// it before its lock. Fails, before it makes anything, if another user could rename an entry on
/// the path to `runs`.
pub(super) fn create(
    host: &impl Host,
    runs: &Path,
    id: &str,
    args: &[String],
) -> Result<(PathBuf, Lock), String> {
    create_dirs(host, runs)?;
    // Only the checked path is used from here on: a symlink on `runs` as given may sit in a
    // directory another user may write, who could repoint it once the check is done.
    let runs = &checked(runs)?;
    let partial = runs.join(format!(".{id}.partial"));
    (DirBuilder::new().mode(DIR_MODE))
        .create(&partial)
        .context(partial.display())?;
    let dir = runs.join(id);
    match fill(host, &partial, &dir, runs, args) {
        Ok(lock) => Ok((dir, lock)),
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

fn fill(
    host: &impl Host,
    partial: &Path,
    dir: &Path,
    runs: &Path,
    args: &[String],
) -> Result<Lock, String> {
    write_json(
        host,
        partial,
        "record.json",
        &json!({ "record_version": RECORD_VERSION, "pumpkin": BUILD.json(), "args": args }),
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
    // Last, so that a record that exists is whole and its `driver.pid` locked. A record directory
    // is never empty, so the rename fails rather than replace one that an id collision names.
    fs::rename(partial, dir).context(dir.display())?;
    sync_dir(host, runs)?;
    Ok(Lock { _file: pid })
}

/// A run's record on disk, which only [`Record::reap`] writes. Its methods read the files as they
/// are, which change while the run lives.
#[derive(Debug)]
pub struct Record {
    id: String,
    dir: PathBuf,
    args: Vec<String>,
    other_build: Option<String>,
}

/// The file of a record in which each [`Record::reap`] appends a line for each sandbox it deleted,
/// which a later reap then leaves be.
///
/// A line is `{"at_ns": t, "provider": p, "id": i, "reaped": "removed"}`: at `t` nanoseconds since
/// the Unix epoch, a decimal string, as the run log gives times, the reap deleted sandbox `i` of
/// provider `p`, as `p` listed it or the record held it undeleted.
const REAPS: &str = "reaps.jsonl";

/// What [`Record::reap`] deleted.
#[derive(Debug)]
pub struct Reaped {
    /// The sandboxes that each provider it was given still listed, by the provider's name, now
    /// deleted.
    pub sandboxes: Vec<(Name, Vec<SandboxId>)>,
    /// The sandboxes that the record held undeleted on a provider it was given, by the provider's
    /// name, which that provider no longer listed, as they had ended, now deleted with what each
    /// left there.
    pub ended: Vec<(Name, SandboxId)>,
    /// The directory of sockets removed, if there was one.
    pub sockets: Option<PathBuf>,
    /// Whether the run had no outcome: the reap recorded it as failed.
    pub lost: bool,
}

impl Record {
    /// Opens the record in `dir`, whose name is the run's id. Fails if `dir` holds no
    /// `record.json`, or one that isn't this build's `record_version`, which is a
    /// [`ReadError::Form`].
    pub fn open(dir: &Path) -> Result<Self, ReadError> {
        let Some(id) = dir.file_name().and_then(|name| name.to_str()) else {
            return Err(ReadError::Failed(format!(
                "`{}` names no directory: expected the directory of a run's record",
                dir.display()
            )));
        };
        let path = dir.join("record.json");
        let record = read_record_json(dir, &path)?;
        if record["record_version"] != RECORD_VERSION {
            return Err(ReadError::Form(format!(
                "{} holds record_version {}: expected \"{RECORD_VERSION}\", which this \
                 `pumpkin` reads",
                path.display(),
                record["record_version"]
            )));
        }
        let args = serde_json::from_value(record["args"].clone()).map_err(|_| {
            ReadError::Form(format!(
                "{} holds args {}: expected an array of strings, the words that started the run",
                path.display(),
                record["args"]
            ))
        })?;
        let other_build = match writer_of(&path, &record)? {
            Writer::Other(build) => Some(build),
            Writer::This | Writer::Older => None,
        };
        let absolute = std::path::absolute(dir).map_err(|error| {
            ReadError::Failed(format!(
                "the record {} can't be made absolute ({error}): name it by its absolute path",
                dir.display()
            ))
        })?;
        Ok(Self {
            id: id.to_owned(),
            dir: absolute,
            args,
            other_build,
        })
    }

    /// Opens the record in `runs` that `run` names, as [`named`] reads it.
    pub fn find(runs: &Path, run: &str) -> Result<Self, String> {
        let names = names(runs)?;
        let id = named(&names, run, &format!("in {}", runs.display()))?;
        Ok(Self::open(&runs.join(id))?)
    }

    /// Opens each record in `runs`, by its id; a directory that holds no `record.json` is
    /// skipped, and one that doesn't open is an error in its place.
    pub fn all(runs: &Path) -> Result<BTreeMap<String, Result<Self, ReadError>>, String> {
        let mut records = BTreeMap::new();
        for name in names(runs)? {
            let dir = runs.join(&name);
            let has = dir.join("record.json").try_exists();
            let record = match has.map_err(failed(dir.display())) {
                Ok(false) => continue,
                Ok(true) => Self::open(&dir),
                Err(error) => Err(error),
            };
            records.insert(name, record);
        }
        Ok(records)
    }

    /// The names of the directories in `runs` that may hold a record, in order, which is by id,
    /// oldest first, but for those that start with `.`, each a record in the making, which isn't
    /// there yet.
    pub fn names(runs: &Path) -> Result<Vec<String>, String> {
        names(runs)
    }

    /// The run's id, which is the name of its directory.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The record's directory, absolute.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The arguments of the `pumpkin run` that started the run, flags first and then the task's
    /// entry, which a command that runs it again takes.
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// The name of the workflow, or `None` if `ir.json` isn't written.
    pub fn task(&self) -> Result<Option<String>, ReadError> {
        let path = self.dir.join("ir.json");
        let Some(bytes) = read_optional(&path)? else {
            return Ok(None);
        };
        let ir: Value = serde_json::from_slice(&bytes).map_err(form(path.display()))?;
        match ir["name"].as_str() {
            Some(name) => Ok(Some(name.to_owned())),
            None => Err(ReadError::Form(format!(
                "{} holds no name: expected a workflow's IR",
                path.display()
            ))),
        }
    }

    /// The workflow's IR, or `None` if `ir.json` isn't written.
    pub fn ir(&self) -> Result<Option<ir::Workflow>, ReadError> {
        log::ir_in(&self.dir)
    }

    /// Where each role is placed, by the role's name, or `None` if `provider.json` isn't written.
    pub fn places(&self) -> Result<Option<BTreeMap<String, Place>>, ReadError> {
        let path = self.dir.join("provider.json");
        let Some(bytes) = read_optional(&path)? else {
            return Ok(None);
        };
        let provided: Value = serde_json::from_slice(&bytes).map_err(form(path.display()))?;
        Ok(Some(log::places(&provided)))
    }

    /// The lines of the run log within `bytes`, each without its newline and with the byte it
    /// starts at; the range begins and ends at lines' ends, as
    /// [`TakenLine::byte`](crate::TakenLine::byte) gives them. Fails if the log can't be read, or
    /// ends before `bytes.end`.
    pub fn log_lines(&self, bytes: Range<u64>) -> Result<Vec<(u64, Vec<u8>)>, String> {
        log::lines(&self.dir, bytes)
    }

    /// What the whole run log says now.
    pub fn progress(&self) -> Result<Readout, ReadError> {
        log::progress(&self.dir)
    }

    /// A fold of the run log, which has taken nothing yet.
    pub fn fold(&self) -> LogFold {
        LogFold::new(&self.dir)
    }

    /// Model request `number`'s body, rebuilt from the items that the record keeps, as it went
    /// upstream or, if the route refused it, as its client sent it. Fails if the log names no such
    /// request, an item is missing, or the rebuilt body's SHA-256 isn't the one that the log holds.
    pub fn model_request(&self, number: u64) -> Result<Vec<u8>, String> {
        super::model::rebuilt_request(&self.dir, number)
    }

    /// Model request `number`'s body as the record keeps it: apart, as `kept`, the `kept.request`
    /// of the log's `model_answered` line for it, says, or else whole.
    pub fn kept_request(&self, number: u64, kept: &Value) -> Result<KeptRequest, String> {
        let Value::Object(kept) = kept else {
            return Err(format!(
                "the run log's `kept.request` is {kept}: expected an object"
            ));
        };
        if let Some(parts) = super::model::kept_parts(&self.dir, number, kept)? {
            return Ok(parts);
        }
        let path = super::model::kept_file(&self.dir, number, "request")?;
        let bytes = fs::read(&path).context(path.display())?;
        let Ok(Value::Object(fields)) = serde_json::from_slice(&bytes) else {
            return Err(format!("{}: the body is no JSON object", path.display()));
        };
        let fields = fields.into_iter();
        Ok(KeptRequest {
            conversation: None,
            fields: fields
                .map(|(field, value)| (field, KeptPart::Sent(value)))
                .collect(),
        })
    }

    /// The bytes of model request `number`'s response as the record keeps it; while it is in
    /// flight, as far as it has come. Fails if the file can't be read, or is a symbolic link.
    pub fn model_response(&self, number: u64) -> Result<Vec<u8>, String> {
        super::model::kept_response(&self.dir, number)
    }

    /// The JSON text of the kept item whose SHA-256 is `digest`.
    pub fn kept_item(&self, digest: &Sha256Digest) -> Result<Vec<u8>, String> {
        super::model::kept_item(&self.dir, digest)
    }

    /// The build of Pumpkin that made the record, shown as a [`Build`] is, if it isn't [`BUILD`];
    /// `None` for a record of this build, or one made before records named theirs.
    pub fn other_build(&self) -> Option<&str> {
        self.other_build.as_deref()
    }

    /// Waits until the run has ended, or been lost, and returns how: at once if its `outcome.json`
    /// is there. Calls `following` first if the run is live, which is to say that the wait is a
    /// long one.
    pub fn wait(&self, following: impl FnOnce()) -> Result<Ending, String> {
        if let Some(ending) = self.ending()? {
            return Ok(ending);
        }
        following();
        let (pid, pid_path) = self.pid_file()?;
        pid.lock_shared().context(pid_path.display())?;
        // The lock is free: the driver wrote its `outcome.json`, if it did, before it let go.
        Ok(self.ended()?.unwrap_or(Ending::Lost))
    }

    /// How the run ended, or was lost, as [`wait`](Self::wait) finds it; `None` while the run is
    /// live. Does not wait.
    pub fn ending(&self) -> Result<Option<Ending>, ReadError> {
        let (pid, pid_path) = self.pid_file()?;
        // An `outcome.json` is the record's last write, so it decides before the lock does.
        if let Some(ending) = self.ended()? {
            return Ok(Some(ending));
        }
        match pid.try_lock_shared() {
            Ok(()) => Ok(Some(self.ended()?.unwrap_or(Ending::Lost))),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(failed(pid_path.display())(error)),
        }
    }

    /// The process ID of the run's driver, or `None` if it has let go of its lock.
    pub fn driver(&self) -> Result<Option<u32>, String> {
        let (pid, path) = self.pid_file()?;
        match pid.try_lock_shared() {
            Ok(()) => return Ok(None),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(error)) => return Err(error).context(path.display()),
        }
        let text = fs::read_to_string(&path).context(path.display())?;
        let read = text.trim().parse();
        read.map(Some).map_err(|error| {
            format!(
                "{} holds `{}`, which is no process ID ({error}): expected the one its driver \
                 wrote",
                path.display(),
                text.trim()
            )
        })
    }

    /// Waits for the run's driver, if one runs, to end, then deletes each sandbox of the run that
    /// `providers`, by name, list or the record holds undeleted, removes its `sockets/` and records
    /// a lost run as failed; each deletion is appended to `reaps.jsonl`. Two reaps of a record take
    /// turns. A provider left out deletes nothing, so a sandbox on it stays.
    pub fn reap(&self, providers: &[(&Name, &dyn Sweep)]) -> Result<Reaped, String> {
        let (pid, pid_path) = self.pid_file()?;
        let run = RunTag::try_from(self.id.clone()).map_err(|error| {
            format!(
                "the record {} is named by no run tag ({error}): expected a record that \
                 `pumpkin run` made",
                self.dir.display()
            )
        })?;
        pid.lock_shared().context(pid_path.display())?;
        // A driver never takes the lock again once it has let go, so the run stays ended.
        drop(pid);
        // Reaps take turns by the directory's lock, which no driver takes, so that `driver.pid`'s
        // lock stays the driver's alone, which `driver` reads as a live run.
        let dir = File::open(&self.dir).context(self.dir.display())?;
        dir.lock().context(self.dir.display())?;
        let lost = State::read(&self.dir)?.is_none();
        if lost {
            // A reap is no run's, so it syncs on the machine.
            write_json(
                &System,
                &self.dir,
                "outcome.json",
                &json!({ "state": "failed", "reason": LOST }),
            )?;
        }
        let undeleted = self.undeleted()?;
        let swept = (providers.iter()).map(|&(name, provider)| {
            // A sandbox that its provider lists no more may have ended and left what only its
            // delete removes.
            let held: Vec<SandboxId> = (undeleted.iter())
                .filter(|(on, _)| on == name)
                .map(|(_, id)| id.clone())
                .collect();
            // A lost run's sandboxes' records keep what its own deletes said.
            let after = |_: &[SandboxId], _: &Result<(), String>| Ok(());
            sweep(name, provider, &run, &held, after).map(|ids| (name.clone(), ids))
        });
        let sandboxes: Vec<(Name, Vec<SandboxId>)> = swept.collect::<Result<_, _>>()?;
        let ended: Vec<(Name, SandboxId)> = (undeleted.into_iter())
            .filter(|(name, id)| {
                let swept = sandboxes.iter().find(|(swept, _)| swept == name);
                swept.is_some_and(|(_, ids)| !ids.contains(id))
            })
            .collect();
        self.note_reaped(&sandboxes, &ended)?;
        let sockets = sockets(&self.dir);
        let sockets = match fs::remove_dir_all(&sockets) {
            Ok(()) => Some(sockets),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context(sockets.display()),
        };
        Ok(Reaped {
            sandboxes,
            ended,
            sockets,
            lost,
        })
    }

    /// Whether the run's own end finished it, with every sandbox that the record holds deleted, so
    /// that a reap would find of it only what a create that gave up made after the end's sweep,
    /// which ends by the run's hard end ([`Creator::create`](provider::Creator::create)).
    pub fn ended_clean(&self) -> Result<bool, String> {
        let finished = match State::read(&self.dir)? {
            Some(State::Finished) => true,
            Some(State::Failed | State::Interrupted) | None => false,
        };
        Ok(finished && self.undeleted()?.is_empty())
    }

    /// The `sandbox.json` of each sandbox that has one, with its directory below the record, in
    /// order.
    pub fn sandboxes(&self) -> Result<Vec<(PathBuf, Value)>, String> {
        let mut dirs = vec![PathBuf::from("agent")];
        for of in ["jobs", "leases"] {
            let path = self.dir.join(of);
            match fs::read_dir(&path) {
                Ok(entries) => {
                    for entry in entries {
                        dirs.push(Path::new(of).join(entry.context(path.display())?.file_name()));
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context(path.display()),
            }
        }
        dirs.sort();
        let mut sandboxes = Vec::new();
        for dir in dirs {
            let path = self.dir.join(&dir).join("sandbox.json");
            let Some(bytes) = read_optional(&path)? else {
                continue;
            };
            let sandbox: Value = serde_json::from_slice(&bytes).context(path.display())?;
            sandboxes.push((dir, sandbox));
        }
        Ok(sandboxes)
    }

    fn undeleted(&self) -> Result<Vec<(Name, SandboxId)>, String> {
        let reaps = self.dir.join(REAPS);
        let reaped = read_optional(&reaps)?.unwrap_or_default();
        let reaped = String::from_utf8(reaped).context(reaps.display())?;
        let mut removed = Vec::new();
        for line in reaped.lines() {
            let line: Value = serde_json::from_str(line).context(reaps.display())?;
            if line["reaped"] == "removed" {
                removed.push((line["provider"].clone(), line["id"].clone()));
            }
        }
        let mut undeleted = Vec::new();
        for (dir, sandbox) in self.sandboxes()? {
            let path = self.dir.join(dir).join("sandbox.json");
            let reaped = (removed.iter())
                .any(|(provider, id)| *provider == sandbox["provider"] && *id == sandbox["id"]);
            if sandbox["delete"]["state"] == "deleted" || reaped {
                continue;
            }
            let (Some(provider), Some(id)) = (sandbox["provider"].as_str(), sandbox["id"].as_str())
            else {
                return Err(format!(
                    "{} names no provider and id: expected a sandbox that the run created",
                    path.display()
                ));
            };
            let provider = Name::try_from(provider.to_owned()).context(path.display())?;
            undeleted.push((provider, SandboxId::from(id.to_owned())));
        }
        Ok(undeleted)
    }

    fn note_reaped(
        &self,
        listed: &[(Name, Vec<SandboxId>)],
        ended: &[(Name, SandboxId)],
    ) -> Result<(), String> {
        let at = log::nanos(UnixTime::of(SystemTime::now())?.since_epoch());
        let listed = (listed.iter()).flat_map(|(name, ids)| ids.iter().map(move |id| (name, id)));
        let lines = (listed.chain(ended.iter().map(|(name, id)| (name, id)))).map(|(name, id)| {
            let line =
                json!({ "at_ns": at, "provider": name, "id": id.as_str(), "reaped": "removed" });
            format!("{line}\n")
        });
        let lines: String = lines.collect();
        if lines.is_empty() {
            return Ok(());
        }
        let mut text = read_optional(&self.dir.join(REAPS))?.unwrap_or_default();
        text.extend_from_slice(lines.as_bytes());
        // A reap is no run's, so it syncs on the machine.
        replace(&System, &self.dir, REAPS, &text)
    }

    /// What each provider that the run's roles are placed on reported of its environment, by its
    /// name: the `report` that `provider.json` holds of it, in the provider's own form; empty if
    /// the run didn't get as far as writing `provider.json`.
    pub fn provider_reports(&self) -> Result<BTreeMap<Name, Value>, String> {
        let path = self.dir.join("provider.json");
        let Some(bytes) = read_optional(&path)? else {
            return Ok(BTreeMap::new());
        };
        let mut provided: Value = serde_json::from_slice(&bytes).context(path.display())?;
        let refused = |what: &str| {
            format!(
                "{} {what}: expected what each of the run's providers declared, by its name",
                path.display()
            )
        };
        let Some(providers) = provided["providers"].as_object_mut() else {
            return Err(refused("holds no `providers` object"));
        };
        let reports = (providers.iter_mut()).map(|(name, declared)| {
            let report = declared.get_mut("report").map(Value::take);
            let report =
                report.ok_or_else(|| refused(&format!("holds no `report` of `{name}`")))?;
            let name = Name::try_from(name.clone())
                .map_err(|error| refused(&format!("names a provider `{name}`: {error}")))?;
            Ok((name, report))
        });
        reports.collect()
    }

    /// Each role's model route as `provider.json` records it, by the role's name, each entry in the
    /// route's order; a role that the table leaves out has no route. `None` if the record's build
    /// kept no routes, which says nothing of which roles have one.
    pub fn routes(&self) -> Result<Option<BTreeMap<String, Vec<RouteEntry>>>, ReadError> {
        #[derive(serde::Deserialize)]
        struct Provided {
            routes: Option<BTreeMap<String, Vec<Entry>>>,
        }
        #[derive(serde::Deserialize)]
        struct Entry {
            api: Api,
            upstream: String,
            models: BTreeMap<String, Model>,
        }
        #[derive(serde::Deserialize)]
        struct Model {
            upstream_name: String,
        }
        let path = self.dir.join("provider.json");
        let bytes = fs::read(&path).map_err(failed(path.display()))?;
        let Provided { routes } = serde_json::from_slice(&bytes).map_err(form(path.display()))?;
        let entry = |entry: Entry| {
            let Entry {
                api,
                upstream,
                models,
            } = entry;
            let models = models.into_iter();
            let models = models.map(|(model, Model { upstream_name })| (model, upstream_name));
            RouteEntry {
                api,
                upstream,
                models: models.collect(),
            }
        };
        Ok(routes.map(|routes| {
            let routes = routes.into_iter();
            let routes = routes.map(|(role, entries)| (role, entries.into_iter().map(entry)));
            routes
                .map(|(role, entries)| (role, entries.collect()))
                .collect()
        }))
    }

    /// What the run used, as its `outcome.json` records it; `None` while it has none, or if it has
    /// one without what the run used, as a reap writes of a lost run.
    pub fn used(&self) -> Result<Option<Used>, ReadError> {
        let path = self.dir.join("outcome.json");
        let Some(outcome) = read_outcome(&path)? else {
            return Ok(None);
        };
        match outcome.get("usage") {
            None => Ok(None),
            Some(usage) => log::used(usage).map(Some).map_err(form(path.display())),
        }
    }

    fn pid_file(&self) -> Result<(File, PathBuf), ReadError> {
        let path = self.dir.join("driver.pid");
        let pid = open_required(&path)?;
        Ok((pid, path))
    }

    fn ended(&self) -> Result<Option<Ending>, ReadError> {
        let path = self.dir.join("outcome.json");
        let Some(outcome) = read_outcome(&path)? else {
            return Ok(None);
        };
        let reason = outcome["reason"]
            .as_str()
            .unwrap_or("no reason is recorded");
        match State::of(&outcome, &path)? {
            State::Finished => Ok(Some(finished(&outcome, &self.dir)?)),
            State::Failed => Ok(Some(Ending::Failed(format!("failed: {reason}")))),
            State::Interrupted => {
                let signal = outcome["signal"].as_str().and_then(Signal::named);
                let signal = signal.ok_or_else(|| {
                    ReadError::Form(format!(
                        "{}'s signal is not one pumpkin knows: expected `SIGINT`, `SIGTERM` or \
                         `SIGHUP`",
                        path.display()
                    ))
                })?;
                let stopped = Stopped::Interrupted(signal);
                Ok(Some(Ending::Interrupted(stopped.to_string())))
            }
        }
    }
}

/// A route entry as `provider.json` has it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteEntry {
    /// The API that the entry serves.
    pub api: Api,
    /// The deployment's name of the upstream that serves it.
    pub upstream: String,
    /// The name that the upstream takes for each model the entry allows, by the name the route
    /// allows.
    pub models: BTreeMap<String, String>,
}

fn names(runs: &Path) -> Result<Vec<String>, String> {
    let entries = fs::read_dir(runs).map_err(|error| {
        format!(
            "{}: {error}: expected the directory that runs are recorded in, as `--runs` names it",
            runs.display()
        )
    })?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.context(runs.display())?;
        let is_dir = entry.file_type().context(runs.display())?.is_dir();
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if is_dir && !name.starts_with('.') {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

pub(super) fn sockets(dir: &Path) -> PathBuf {
    dir.join("sockets")
}

// The most that the mode of a directory that `create` makes may be: a umask can only take bits from
// it, so no other user may write the record's directory, as `gateway::Sockets::create` needs of the
// directory where it makes the sockets.
const DIR_MODE: u32 = 0o755;

fn create_dirs(host: &impl Host, dir: &Path) -> Result<(), String> {
    let missing: Vec<&Path> = (dir.ancestors())
        .take_while(|path| !path.as_os_str().is_empty() && !path.exists())
        .collect();
    (DirBuilder::new().recursive(true).mode(DIR_MODE))
        .create(dir)
        .context(dir.display())?;
    for made in missing.into_iter().rev() {
        // The parent of a relative path's first component is the empty path: the current directory.
        let parent = made
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty());
        sync_dir(host, parent.unwrap_or(Path::new(".")))?;
    }
    Ok(())
}

/// `runs` made canonical, if no directory on its path, from the root down, can be written by a user
/// other than this process's, or root, or by a group of more than that user, unless it is sticky,
/// as `/tmp` is: else another user could rename the record, or a directory above it, and put one of
/// their own in its place. Fails naming the first that can, and how to close it.
fn checked(runs: &Path) -> Result<PathBuf, String> {
    let user = effective_uid()?;
    let canonical = fs::canonicalize(runs).context(runs.display())?;
    let ancestors: Vec<&Path> = canonical.ancestors().collect();
    // Read only once a directory writable by its group is met, and then once.
    let mut private = None;
    for dir in ancestors.into_iter().rev() {
        let metadata = fs::metadata(dir).context(dir.display())?;
        let (owner, mode) = (metadata.uid(), metadata.mode());
        if owner != user && owner != 0 {
            return Err(format!(
                "`{}` is owned by uid {owner}: expected a directory owned by this process's user, \
                 uid {user}, or by root",
                dir.display()
            ));
        }
        if mode & 0o1000 != 0 {
            continue;
        }
        let shared = mode & 0o020 != 0 && {
            let private = match private {
                Some(private) => private,
                None => *private.insert(private_group(user)?),
            };
            private != Some(metadata.gid())
        };
        let (writers, chmod) = match (shared, mode & 0o002 != 0) {
            (false, false) => continue,
            (true, false) => ("its group", "g-w"),
            (false, true) => ("others", "o-w"),
            (true, true) => ("its group and others", "g-w,o-w"),
        };
        return Err(format!(
            "`{dir}` is writable by {writers}: expected a directory that only its owner, or a \
             group that holds only this process's user, may write, or a sticky one, as \
             `chmod {chmod} {dir}` makes it",
            dir = dir.display()
        ));
    }
    Ok(canonical)
}

const STATUS: &str = "/proc/self/status";

fn effective_uid() -> Result<u32, String> {
    let status = fs::read_to_string(STATUS).context(STATUS)?;
    (status.lines())
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|uids| uids.split_whitespace().nth(1)?.parse().ok())
        .ok_or_else(|| {
            format!(
                "{STATUS} gives no effective uid: expected a line `Uid: <real> <effective> \
                 <saved> <filesystem>`, as Linux writes it"
            )
        })
}

const PASSWD: &str = "/etc/passwd";

const GROUP: &str = "/etc/group";

/// The primary group of `user`, if no one else is in it, so that it is as private as the user. A
/// user or a group that `/etc/passwd` and `/etc/group` don't list, as one that only a network
/// database holds, has none, and a member that only such a database adds goes unseen.
fn private_group(user: u32) -> Result<Option<u32>, String> {
    let passwd = fs::read_to_string(PASSWD).context(PASSWD)?;
    let group = fs::read_to_string(GROUP).context(GROUP)?;
    let users: Vec<(&str, u32, Option<u32>)> = entries(&passwd)
        .map(|(name, uid, mut rest)| (name, uid, rest.next().and_then(|gid| gid.parse().ok())))
        .collect();
    let Some(&(name, _, Some(gid))) = users.iter().find(|&&(_, uid, _)| uid == user) else {
        return Ok(None);
    };
    let others = (users.iter()).any(|&(_, uid, primary)| uid != user && primary == Some(gid));
    let groups: Vec<&str> = entries(&group)
        .filter(|&(_, id, _)| id == gid)
        .map(|(_, _, mut rest)| rest.next().unwrap_or(""))
        .collect();
    let alone = !groups.is_empty()
        && (groups.iter().flat_map(|members| members.split(',')))
            .all(|member| member.is_empty() || member == name);
    Ok((alone && !others).then_some(gid))
}

fn entries(file: &str) -> impl Iterator<Item = (&str, u32, std::str::Split<'_, char>)> {
    file.lines().filter_map(|line| {
        let mut fields = line.split(':');
        let name = fields.next()?;
        let id = fields.nth(1)?.parse().ok()?;
        Some((name, id, fields))
    })
}

/// The content of `provider.json`: what each provider declared (`report`, `classes`), each role's
/// place, and each role's route, with the prices that the machine's configuration gave.
pub(super) fn provided(
    placed: &Placed,
    providers: &ByProvider<&dyn Provider>,
    routes: &ByRole<Vec<Serving>>,
    prices: &BTreeMap<ModelName, TokenPrices>,
) -> Value {
    let declared = (providers.iter())
        .map(|(name, provider)| {
            let Ready { report, classes } = provider.ready();
            let classes = (classes.iter()).map(|(name, class)| {
                let Class {
                    offers: _,
                    startup_seconds,
                    usd_per_hour,
                } = class;
                let mut class = json!({ "startup_seconds": startup_seconds });
                if let Some(price) = usd_per_hour {
                    class["usd_per_hour"] = json!(price);
                }
                (name.to_string(), class)
            });
            let declared = json!({ "report": report, "classes": classes.collect::<Map<_, _>>() });
            (name.to_string(), declared)
        })
        .collect::<Map<_, _>>();
    let workflow = placed.workflow();
    let roles = workflow.roles().map(|(role, sandbox)| {
        let Placement {
            provider,
            class,
            declared: _,
        } = placed.placement(role);
        let at = json!({ "provider": provider, "class": class });
        (sandbox.name.to_string(), at)
    });
    // Names alone: the upstream's URL and key are the deployment's, and stay out of the record.
    let entry = |serving: &Serving| {
        let Serving {
            name,
            api,
            url: _,
            key: _,
            models,
            headers: _,
        } = serving;
        let models = (models.iter())
            .map(|(model, upstream_name)| {
                let mut served = json!({ "upstream_name": upstream_name });
                if let Some(price) = prices.get(model) {
                    served["usd_per_mtok"] = json!(price);
                }
                (model.to_string(), served)
            })
            .collect::<Map<_, _>>();
        json!({ "api": api, "upstream": name, "models": models })
    };
    let served = workflow.roles().filter_map(|(role, sandbox)| {
        let servings = routes.get(role);
        let entries: Vec<Value> = servings.iter().map(entry).collect();
        (!entries.is_empty()).then(|| (sandbox.name.to_string(), Value::Array(entries)))
    });
    json!({
        "providers": declared,
        "roles": roles.collect::<Map<_, _>>(),
        "routes": served.collect::<Map<_, _>>(),
    })
}

/// The workflow's IR as a run's `ir.json` holds it: the IR's one encoding as text, so an IR printed
/// elsewhere is byte for byte what a run of it records.
pub fn ir_json(workflow: &Workflow) -> String {
    // Not through a `Value`, which sorts an object's keys, so that the text keeps the IR's order.
    let Ok(ir) = serde_json::to_string_pretty(workflow.ir()) else {
        unreachable!("the IR's maps are keyed by strings, which JSON can hold");
    };
    format!("{ir}\n")
}

pub(super) fn write_json(
    host: &impl Host,
    dir: &Path,
    name: &str,
    value: &Value,
) -> Result<(), String> {
    replace(host, dir, name, format!("{value:#}\n").as_bytes())
}

pub(super) fn write_ir(host: &impl Host, dir: &Path, workflow: &Workflow) -> Result<(), String> {
    replace(host, dir, "ir.json", ir_json(workflow).as_bytes())
}

/// Writes `dir`'s `sandbox.json`, `dir` made if need be: the sandbox's provider and id, its GPUs if
/// known, and how its latest delete went, if one was tried.
pub(super) fn write_sandbox(
    host: &impl Host,
    dir: &Path,
    provider: &Name,
    id: &SandboxId,
    gpus: Option<&[GpuModel]>,
    deleted: Option<&Result<(), String>>,
) -> Result<(), String> {
    let mut sandbox = json!({ "provider": provider, "id": id.as_str() });
    if let Some(gpus) = gpus {
        sandbox["gpus"] = json!(gpus);
    }
    if let Some(deleted) = deleted {
        sandbox["delete"] = match deleted {
            Ok(()) => json!({ "state": "deleted" }),
            Err(_) => json!({ "state": "failed" }),
        };
    }
    fs::create_dir_all(dir).context(dir.display())?;
    write_json(host, dir, "sandbox.json", &sandbox)
}

/// Makes `dir/name` hold `bytes`, as it is or as it was, whenever the machine fails: written and
/// synced aside, then renamed over it, then the directory synced.
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

/// The record's `payloads/`, where each accepted payload is kept under the digest of its bytes.
pub(super) struct Payloads<'h, H> {
    dir: PathBuf,
    host: &'h H,
}

impl<'h, H: Host> Payloads<'h, H> {
    pub(super) fn create(host: &'h H, record: &Path) -> Result<Self, String> {
        let dir = record.join("payloads");
        fs::create_dir(&dir).context(dir.display())?;
        sync_dir(host, record)?;
        Ok(Self { dir, host })
    }

    /// Keeps `bytes`, whose reference is `payload`, unless a payload of that digest is kept
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

    pub(super) fn stored(&self, payload: &PayloadRef) -> Stored {
        Stored {
            path: self.path(&payload.digest),
            reference: payload.clone(),
        }
    }

    fn path(&self, digest: &Sha256Digest) -> PathBuf {
        self.dir.join(digest.hex())
    }
}

pub(super) struct Stored {
    path: PathBuf,
    reference: PayloadRef,
}

impl Stored {
    /// The payload's bytes. Fails if the file's size or digest isn't the reference's.
    pub(super) fn read(&self) -> Result<Vec<u8>, String> {
        let bytes = fs::read(&self.path).context(self.path.display())?;
        let read = reference(&bytes);
        if read != self.reference {
            let (PayloadRef { size, digest }, expected) = (read, &self.reference);
            return Err(format!(
                "{} holds {size} bytes of digest {}: expected the payload's {} bytes of digest {}",
                self.path.display(),
                digest.as_str(),
                expected.size,
                expected.digest.as_str()
            ));
        }
        Ok(bytes)
    }
}

/// What `dir`, where a sandbox's `out` and `log` were collected, holds of each, for the run log:
/// `out, 0 files of 0 bytes; log, 2 files of 481 bytes`.
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

#[derive(Clone, Copy, Debug, Default)]
struct TreeSize {
    files: u64,
    bytes: u64,
}

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
