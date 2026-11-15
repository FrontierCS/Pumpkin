//! A run's record on disk: its directory and name, the payloads stored by digest, and the
//! files written whole.
//!
//! Each run has a directory of its own, `<runs>/<id>/`, which [`create`] makes and names:
//!
//! - `driver.pid`: the pid of the process that drives the run, locked by it until the run's last
//!   write: see [`Lock`] for the rule that makes the record's state readable;
//! - `driver.log`: the driver process's standard output and error since the run's start;
//! - `record.json`, written first: the version of the record's form ([`RECORD_VERSION`]), the
//!   build that made it ([`BUILD`]) and the arguments of its `pumpkin run` ([`Record::args`]);
//! - `ir.json`: the workflow's IR;
//! - `provider.json`: where the roles are placed, and each one's model route ([`provided`]);
//! - `log.jsonl`: the run log, which the `log` module lays out;
//! - `payloads/<hex>`: the payload of each op the core accepted, named by its SHA-256;
//! - `agent/`, `jobs/<n>/` and `leases/<n>/`, for the sandbox of the agent, of job `n`'s judge
//!   call and of lease `n`'s runner, once created: its `sandbox.json` ([`write_sandbox`]), and its
//!   `out` and `log` as far as collected, but a result over
//!   [`RESULT_MAX_BYTES`](schema::sandbox::RESULT_MAX_BYTES);
//! - `model/`: each model exchange, each conversation item and each field kept whole, such as a
//!   list of tools, once (`Exchange`);
//! - `sockets/`: while the run lasts, where its sandboxes meet the gateway ([`sockets`]);
//! - `reaps.jsonl`: each sandbox that a reap deleted ([`REAPS`]);
//! - `outcome.json`: how the run ended, and what it used (`log::usage_of`): `{"state":
//!   "finished"}` and the outcome's fields (`log::outcome_json`), `{"state": "failed", "reason":
//!   r}`, as [`Record::reap`] records a lost run too, or `{"state": "interrupted", "signal": s}`.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, DirBuilder, File, TryLockError};
use std::io::{self, Read, Write};
use std::ops::Range;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use decision::PayloadRef;
use provider::{Provider, Ready, RunTag, SandboxId, Sweep, UnixTime};
use route::Serving;
use schema::ir::{Api, GpuModel, Name, Sha256Digest};
use schema::sandbox::{command_stderr, log_dir, out_dir};
use schema::workflow::{ByProvider, ByRole, Class, Placed, Placement, Workflow};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use stdx::{Context, Utc};

use super::log::{self, LogFold, Place, Readout, Used};
use super::model::{KeptPart, KeptRequest};
use super::sandbox::sweep;
use super::{Durable, Host, Signal, Stopped, System};

/// Why [`Record::reap`] records a lost run as failed.
const LOST: &str = "its driver ended without an outcome";

/// The version of the record's form, named as the IR's version is
/// ([`IrVersion`](schema::ir::IrVersion)), which a reader checks before it reads the rest. Until
/// Pumpkin's first release, the form changes under this version; after it, each change to the
/// form, a new file included, takes a new version, so that a build refuses a record whose form it
/// doesn't know instead of misreading it. A record's `record.json` is
/// `{"record_version": v, "pumpkin": p, "args": a}`: this version, the build that made it
/// ([`BUILD`]), `{"version": v, "commit": c}`, with `c` `{"hash": h, "date": d}` or `null`
/// ([`Record::other_build`]), and the `pumpkin run`'s arguments, an array of words
/// ([`Record::args`]).
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
    /// The hash of the commit it was built from, in hexadecimal digits, if it has one.
    #[must_use]
    pub fn commit_hash(self) -> Option<&'static str> {
        self.commit.map(|commit| commit.hash)
    }

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
        let commit = commit.map(|Commit { hash, date }| (hash, date));
        write_build(f, version, commit)
    }
}

/// Writes the build of `version`, made from the commit of hash and date `commit`, if it has one,
/// as a [`Build`] is shown.
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

/// The build that `build`, as [`Build::json`] writes one, names, shown as a [`Build`] is; `None`
/// if it names none.
fn shown(build: &Value) -> Option<String> {
    let version = build["version"].as_str()?;
    let commit = match &build["commit"] {
        Value::Null => None,
        commit => Some((commit["hash"].as_str()?, commit["date"].as_str()?)),
    };
    let mut shown = String::new();
    write_build(&mut shown, version, commit).ok()?;
    Some(shown)
}

/// The lock on a record's `driver.pid`, an exclusive `flock` held until it is dropped, by the
/// process that drives the run.
///
/// Only the holder writes the record, and drops the lock after its last write, `outcome.json`:
/// so a record with one has ended, one whose lock is held is live, and one whose lock is free
/// without one is lost, which only [`Record::reap`] writes then. The lock goes with the process,
/// however it ends, and `docker` children don't inherit it, as [`File`]s are close-on-exec.
#[derive(Debug)]
pub(super) struct Lock {
    _file: File,
}

/// A new run's id, which names its record and tags its sandboxes: `now` as a UTC time, and 8
/// random hexadecimal digits, as `20261028-131811-1a2b3c4d`, which sorts by the time of the call.
///
/// Fails if `now` is before the Unix epoch.
pub fn new_id(now: SystemTime) -> Result<RunTag, String> {
    let seconds = UnixTime::of(now)?.since_epoch().as_secs();
    let mut random = [0; 4];
    File::open("/dev/urandom")
        .and_then(|mut urandom| urandom.read_exact(&mut random))
        .context("reading /dev/urandom for the run's id failed")?;
    let id = format!("{}-{:08x}", utc_stamp(seconds), u32::from_be_bytes(random));
    match RunTag::try_from(id.clone()) {
        Ok(tag) => Ok(tag),
        Err(error) => unreachable!(
            "run {id}'s id is no run tag, though it is a UTC time's digits and `-`, then \
             lowercase hexadecimal digits, all of which a run tag allows, and starts with a \
             digit: {error}"
        ),
    }
}

/// When `pumpkin run` minted the run id `id` ([`new_id`]), since the Unix epoch, to the second,
/// as its digits say; `None` if it is no id of that form.
pub fn minted(id: &str) -> Option<Duration> {
    let digits = |range: std::ops::Range<usize>| -> Option<u64> {
        let digits = id.get(range)?;
        digits
            .bytes()
            .all(|byte| byte.is_ascii_digit())
            .then(|| digits.parse().ok())?
    };
    if id.as_bytes().get(8) != Some(&b'-') {
        return None;
    }
    let utc = Utc {
        year: digits(0..4)?,
        month: digits(4..6)?,
        day: digits(6..8)?,
        hour: digits(9..11)?,
        minute: digits(11..13)?,
        second: digits(13..15)?,
    };
    Some(Duration::from_secs(utc.to_unix()))
}

/// The one of `names`, the ids of the runs `among` says where, such as `in <dir>`, that `run`, an
/// id, a prefix of one, or of its 8 random digits, names: the one that no other id shares it with.
///
/// Fails if `run` is empty, or names no id of `names` or several, which the error lists.
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
/// `record.json` and its locked `driver.pid`, syncing on `host` each directory that gains an
/// entry; returns its path, in the canonical path of `runs`.
///
/// It is made as `.<id>.partial`, renamed to `<id>` once locked, so that no reader of `runs` sees
/// it before its lock. What it makes is at most [`DIR_MODE`], so only its owner may write it, as
/// `gateway::Sockets::create` needs. Fails, before it makes anything, if another user could
/// rename an entry on the path to `runs` ([`checked`]).
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

/// Writes the files of a new record, of a run started with `args`, into `partial`, which is
/// empty, locks its `driver.pid`, and renames `partial` to `dir`, in `runs`, syncing on `host`.
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
    args: Vec<String>,
    other_build: Option<String>,
}

/// How a run ended, as its `outcome.json` says, each with a sentence that follows the run's id
/// and says how in words, but a score, which the reader words as it shows any score.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ending {
    /// The run finished with a score, the JSON number `score`, after its work phase ended as
    /// `after` says: `after the work phase ended <how>`.
    Scored { score: String, after: String },
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
    /// The run stopped without an outcome, as [`Stopped`](crate::Stopped) says in words:
    /// `failed: <reason>`.
    Failed(String),
    /// A signal stopped the run, as [`Stopped`](crate::Stopped) says in words: `was interrupted
    /// by <signal>`.
    Interrupted(String),
    /// The run is lost: its driver ended without an `outcome.json`.
    Lost,
}

/// The file of a record in which each [`Record::reap`] appends a line for each sandbox it
/// deleted, which a later reap then leaves be.
///
/// A line is `{"at_ns": t, "provider": p, "id": i, "reaped": "removed"}`: at `t` nanoseconds
/// since the Unix epoch, a decimal string, as the run log gives times, the reap deleted sandbox
/// `i` of provider `p`, as `p` listed it or the record held it undeleted ([`write_sandbox`]).
const REAPS: &str = "reaps.jsonl";

/// What [`Record::reap`] removed of a run that had ended, or was lost.
#[derive(Debug)]
pub struct Reaped {
    /// The sandboxes that each provider it was given still listed, by the provider's name, now
    /// deleted.
    pub sandboxes: Vec<(Name, Vec<SandboxId>)>,
    /// The sandboxes that the record held undeleted on a provider it was given, by the
    /// provider's name, which that provider no longer listed, as they had ended, now deleted
    /// with what each left there.
    pub ended: Vec<(Name, SandboxId)>,
    /// Its socket directory, if one was left, now removed.
    pub sockets: Option<PathBuf>,
    /// Whether it was lost, and is now recorded as failed, as its driver ended without an
    /// outcome.
    pub lost: bool,
}

impl Record {
    /// The record in the directory `dir`.
    ///
    /// # Errors
    ///
    /// Fails if `dir` is no directory, has no `record.json`, or has one that is not a record of
    /// this version of Pumpkin.
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
        let args = serde_json::from_value(record["args"].clone()).map_err(|_| {
            format!(
                "{} holds args {}: expected an array of strings, the words that started the run",
                path.display(),
                record["args"]
            )
        })?;
        let other_build = match &record["pumpkin"] {
            Value::Null => None,
            build if *build == BUILD.json() => None,
            build => Some(shown(build).ok_or_else(|| {
                format!(
                    "{} holds pumpkin {build}: expected the build that made it, \
                     {{\"version\": v, \"commit\": c}}",
                    path.display()
                )
            })?),
        };
        let absolute = std::path::absolute(dir).map_err(|error| {
            format!(
                "the record {} can't be made absolute ({error}): name it by its absolute path",
                dir.display()
            )
        })?;
        Ok(Self {
            id: id.to_owned(),
            dir: absolute,
            args,
            other_build,
        })
    }

    /// The record that `run`, a record's directory name, a prefix of it or of its 8 random
    /// digits that no other record's name shares, names in `runs`. Fails if `run` is empty, or
    /// names no record or several, which the error lists.
    pub fn find(runs: &Path, run: &str) -> Result<Self, String> {
        let names = names(runs)?;
        let id = named(&names, run, &format!("in {}", runs.display()))?;
        Self::open(&runs.join(id))
    }

    /// Every record in `runs`, by its id, oldest first, each opened as [`open`](Self::open)
    /// does, or why it can't be; a directory there that holds no `record.json` is no record.
    pub fn all(runs: &Path) -> Result<Vec<Result<Self, String>>, String> {
        let mut records = Vec::new();
        for name in names(runs)? {
            let dir = runs.join(&name);
            let has = dir.join("record.json").try_exists();
            match has.context(dir.display()) {
                Ok(false) => {}
                Ok(true) => records.push(Self::open(&dir)),
                Err(error) => records.push(Err(error)),
            }
        }
        Ok(records)
    }

    /// The name of each directory in `runs` that may hold a record, sorted, so by id, oldest
    /// first, as [`all`](Self::all) opens them, without opening any.
    ///
    /// # Errors
    ///
    /// Fails if `runs` can't be listed.
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

    /// The name of the run's workflow, from its `ir.json`; `None` if the record has none yet, as
    /// one just begun.
    pub fn task(&self) -> Result<Option<String>, String> {
        let Some(ir) = self.ir()? else {
            return Ok(None);
        };
        match ir["name"].as_str() {
            Some(name) => Ok(Some(name.to_owned())),
            None => Err(format!(
                "{} holds no name: expected a workflow's IR",
                self.dir.join("ir.json").display()
            )),
        }
    }

    /// The run's workflow, as its `ir.json` keeps it ([`ir_json`]); `None` if the record has none
    /// yet, as one just begun.
    ///
    /// # Errors
    ///
    /// Fails if `ir.json` can't be read, or isn't JSON.
    pub fn ir(&self) -> Result<Option<Value>, String> {
        let path = self.dir.join("ir.json");
        let Some(bytes) = read_optional(&path)? else {
            return Ok(None);
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .context(path.display())
    }

    /// Where each of the run's roles is placed, by the role's name, as its `provider.json`
    /// records it; `None` if the record has none yet, as one just begun.
    ///
    /// # Errors
    ///
    /// Fails if `provider.json` can't be read, or isn't JSON.
    pub fn places(&self) -> Result<Option<BTreeMap<String, Place>>, String> {
        let path = self.dir.join("provider.json");
        let Some(bytes) = read_optional(&path)? else {
            return Ok(None);
        };
        let provided: Value = serde_json::from_slice(&bytes).context(path.display())?;
        Ok(Some(log::places(&provided)))
    }

    /// Each whole line of the run's log from byte `bytes.start`, where one starts, to byte
    /// `bytes.end`, where one ends, as a [`LogFold`] took them, in the log's order: the byte it
    /// starts at, and its bytes, without the newline that ends it.
    ///
    /// # Errors
    ///
    /// Fails if the log can't be read, or ends before `bytes.end`.
    pub fn log_lines(&self, bytes: Range<u64>) -> Result<Vec<(u64, Vec<u8>)>, String> {
        log::lines(&self.dir, bytes)
    }

    /// What the run's log says of it so far, as far as this build can read it ([`Readout`]).
    ///
    /// # Errors
    ///
    /// Fails if a file of the record can't be read, or, once the log exists, its `ir.json` or
    /// `provider.json` isn't JSON.
    pub fn progress(&self) -> Result<Readout, String> {
        log::progress(&self.dir)
    }

    /// The run's log, none of it read yet, to read as the run appends to it ([`LogFold`]).
    pub fn fold(&self) -> LogFold {
        LogFold::new(&self.dir)
    }

    /// Model request `number`'s body as it went upstream, or as its client sent it, rebuilt from
    /// where the record keeps it, as its log's `model_answered` names it (`model::Exchange`).
    ///
    /// # Errors
    ///
    /// Fails if the log names no request `number` that the record kept, if a file it names can't
    /// be read or isn't the form the record keeps, or if the log names the SHA-256 of the body
    /// sent and the rebuilt body has another.
    pub fn model_request(&self, number: u64) -> Result<Vec<u8>, String> {
        super::model::rebuilt_request(&self.dir, number)
    }

    /// The body of model request `number`, whose log's `model_answered`'s `kept.request` is
    /// `kept`, as the record keeps it: the items of its conversation and the fields it keeps
    /// whole each by its digest, read without the items themselves; each field as it was sent if
    /// the record kept none apart.
    ///
    /// # Errors
    ///
    /// Fails if `kept` is no object, or if the request's file can't be read or isn't a JSON
    /// object of the form that `kept` names.
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
    /// flight, as far as it has come.
    ///
    /// # Errors
    ///
    /// Fails if the file can't be read, or is a symbolic link.
    pub fn model_response(&self, number: u64) -> Result<Vec<u8>, String> {
        super::model::kept_response(&self.dir, number)
    }

    /// The bytes of the conversation's item, or the field kept whole, that `digest` names, as
    /// the record keeps it in `model/items/`.
    ///
    /// # Errors
    ///
    /// Fails if the record keeps no such item, or it can't be read.
    pub fn kept_item(&self, digest: &Sha256Digest) -> Result<Vec<u8>, String> {
        super::model::kept_item(&self.dir, digest)
    }

    /// The build of Pumpkin that made the record, shown as a [`Build`] is, if it isn't [`BUILD`];
    /// `None` for a record of this build, or one made before records named theirs.
    pub fn other_build(&self) -> Option<&str> {
        self.other_build.as_deref()
    }

    /// Waits until the run has ended, or been lost, and returns how: at once if its
    /// `outcome.json` is there. Calls `following` first if the run is live, which is to say that
    /// the wait is a long one.
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
    pub fn ending(&self) -> Result<Option<Ending>, String> {
        let (pid, pid_path) = self.pid_file()?;
        // An `outcome.json` is the record's last write, so it decides before the lock does.
        if let Some(ending) = self.ended()? {
            return Ok(Some(ending));
        }
        match pid.try_lock_shared() {
            Ok(()) => Ok(Some(self.ended()?.unwrap_or(Ending::Lost))),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(error).context(pid_path.display()),
        }
    }

    /// The process ID that the run's driver wrote to `driver.pid`, while it holds the record's
    /// lock; `None` once the run has ended or been lost.
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
    /// a lost run as failed. Two reaps of a record take turns. A provider left out deletes nothing.
    ///
    /// # Errors
    ///
    /// Fails, writing nothing, if the record has no `driver.pid`, its id is no run tag, or its
    /// locks can't be taken; then if `outcome.json` can't be written, the run's sandboxes can't
    /// be listed or deleted, or its socket directory can't be removed.
    pub fn reap(&self, providers: &[(&Name, &dyn Sweep)]) -> Result<Reaped, String> {
        let (pid, pid_path) = self.pid_file()?;
        let run = RunTag::try_from(self.id.clone()).map_err(|error| {
            format!(
                "the record {} is named by no run tag ({error}): expected a record that \
                 `pumpkin run` made",
                self.dir.display()
            )
        })?;
        // A driver never takes the lock again once it has let go, so the run stays ended.
        pid.lock_shared().context(pid_path.display())?;
        drop(pid);
        // Reaps take turns by the directory's lock, which no driver takes, so that `driver.pid`'s
        // lock stays the driver's alone, which `driver` reads as a live run.
        let dir = File::open(&self.dir).context(self.dir.display())?;
        dir.lock().context(self.dir.display())?;
        let lost = self.ended()?.is_none();
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

    /// Whether the run's own end finished it, with every sandbox that the record holds deleted,
    /// so that a reap would find of it only what a create that gave up made after the end's
    /// sweep, which ends by the run's hard end ([`Creator::create`](provider::Creator::create)).
    pub fn ended_clean(&self) -> Result<bool, String> {
        let finished = match self.ended()? {
            Some(Ending::Scored { .. } | Ending::Unscored(_) | Ending::NoResult { .. }) => true,
            Some(Ending::Failed(_) | Ending::Interrupted(_) | Ending::Lost) | None => false,
        };
        Ok(finished && self.undeleted()?.is_empty())
    }

    /// Each sandbox that the record holds without a delete that succeeded, the agent's, each
    /// job's and each lease's, by its provider's name, but those that an earlier reap removed
    /// ([`REAPS`]).
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
        let mut dirs = vec![self.dir.join("agent")];
        for of in ["jobs", "leases"] {
            let of = self.dir.join(of);
            match fs::read_dir(&of) {
                Ok(entries) => {
                    for entry in entries {
                        dirs.push(entry.context(of.display())?.path());
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context(of.display()),
            }
        }
        dirs.sort();
        let mut undeleted = Vec::new();
        for dir in dirs {
            let path = dir.join("sandbox.json");
            let Some(bytes) = read_optional(&path)? else {
                continue;
            };
            let sandbox: Value = serde_json::from_slice(&bytes).context(path.display())?;
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

    /// Appends to the record's [`REAPS`] a line for each of `listed` and of `ended`, at once.
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

    /// What each provider that the run's roles are placed on reported of its environment, by
    /// its name: the `report` that `provider.json` holds of it, in the provider's own form; none
    /// if the run didn't get as far as writing `provider.json`.
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

    /// Each role's model route as `provider.json` records it, by the role's name, each entry in
    /// the route's order; a role that the table leaves out has no route. `None` if the record's
    /// build kept no routes, which says nothing of which roles have one.
    ///
    /// # Errors
    ///
    /// Fails if `provider.json` can't be read, as before a run writes it, or its `routes` isn't
    /// the form a record keeps.
    pub fn routes(&self) -> Result<Option<BTreeMap<String, Vec<RouteEntry>>>, String> {
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
        let bytes = fs::read(&path).context(path.display())?;
        let Provided { routes } = serde_json::from_slice(&bytes).context(path.display())?;
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

    /// What the run used, as its `outcome.json` records it ([`Used`]); `None` while it has none,
    /// or if it has one without what the run used, as a reap writes of a lost run.
    ///
    /// # Errors
    ///
    /// Fails if `outcome.json` can't be read, or its `usage` isn't the form a run writes.
    pub fn used(&self) -> Result<Option<Used>, String> {
        let path = self.dir.join("outcome.json");
        let Some(bytes) = read_optional(&path)? else {
            return Ok(None);
        };
        let outcome: Value = serde_json::from_slice(&bytes).context(path.display())?;
        match outcome.get("usage") {
            None => Ok(None),
            Some(usage) => log::used(usage).map(Some).context(path.display()),
        }
    }

    /// The record's `driver.pid`, opened, and its path.
    fn pid_file(&self) -> Result<(File, PathBuf), String> {
        let path = self.dir.join("driver.pid");
        let pid = File::open(&path).context(path.display())?;
        Ok((pid, path))
    }

    /// How the run ended, if it has an `outcome.json`.
    fn ended(&self) -> Result<Option<Ending>, String> {
        let path = self.dir.join("outcome.json");
        let Some(bytes) = read_optional(&path)? else {
            return Ok(None);
        };
        let outcome: serde_json::Value = serde_json::from_slice(&bytes).context(path.display())?;
        let reason = outcome["reason"]
            .as_str()
            .unwrap_or("no reason is recorded");
        match outcome["state"].as_str() {
            Some("finished") => Ok(Some(finished(&outcome, &self.dir))),
            Some("failed") => Ok(Some(Ending::Failed(format!("failed: {reason}")))),
            Some("interrupted") => {
                let signal = outcome["signal"].as_str().and_then(Signal::named);
                let signal = signal.ok_or_else(|| {
                    format!(
                        "{}'s signal is not one pumpkin knows: expected `SIGINT`, `SIGTERM` or \
                         `SIGHUP`",
                        path.display()
                    )
                })?;
                let stopped = Stopped::Interrupted(signal);
                Ok(Some(Ending::Interrupted(stopped.to_string())))
            }
            Some(_) | None => Err(format!(
                "{}'s state is not one pumpkin knows: expected `finished`, `failed` or \
                 `interrupted`",
                path.display()
            )),
        }
    }
}

/// An entry of a role's model route, as `provider.json` records it.
///
/// Its API, the deployment's name of the upstream that serves it, and the name the upstream takes
/// for each model it allows, by the model's name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteEntry {
    pub api: Api,
    pub upstream: String,
    pub models: BTreeMap<String, String>,
}

/// The bytes of the file at `path`, or `None` if there is no such file.
pub(super) fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context(path.display()),
    }
}

/// The names of the directories in `runs`, sorted, but for those that start with `.`, each a
/// record in the making, which isn't there yet.
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
        return Ending::Scored {
            score: score.to_string(),
            after: format!("after the work phase ended {ended}"),
        };
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

pub(super) fn sockets(dir: &Path) -> PathBuf {
    dir.join("sockets")
}

/// Where the record in `dir` holds the final judge's standard error,
/// `jobs/<n>/log/command.stderr`, for the job `n` that its run log names
/// ([`log::final_judge`]), or why the run log doesn't say.
fn final_judge_stderr(dir: &Path) -> Result<PathBuf, String> {
    let job = dir.join(format!("jobs/{}", log::final_judge(dir)?));
    Ok(job.join(command_stderr().as_str()))
}

/// The most that the mode of a directory that [`create`] makes may be: a umask can only take bits
/// from it.
const DIR_MODE: u32 = 0o755;

/// Makes `dir` and those of its ancestors that are missing, as [`fs::create_dir_all`] does,
/// each at most [`DIR_MODE`], and syncs the parent of each directory it makes on `host`, so that
/// each lasts.
fn create_dirs(host: &impl Host, dir: &Path) -> Result<(), String> {
    let missing: Vec<&Path> = (dir.ancestors())
        .take_while(|path| !path.as_os_str().is_empty() && !path.exists())
        .collect();
    (DirBuilder::new().recursive(true).mode(DIR_MODE))
        .create(dir)
        .context(dir.display())?;
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

/// The canonical path of `runs`, once it is checked that it and each of its ancestors are owned
/// by root or by this process's effective user ([`effective_uid`]), and that only their owner,
/// or the user's private group ([`private_group`]), may write them, unless they are sticky, as
/// `/tmp` is: else another user could rename the record, or a directory above it, and put one of
/// their own in its place. Fails at the first one, from `/` down, that isn't so.
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

/// The effective uid of this process, the second of the four on [`STATUS`]'s `Uid:` line.
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

/// The gid of the private group of `user`, if it has one: its primary group in [`PASSWD`], when
/// [`GROUP`] lists it with no member but `user`, and no other user in [`PASSWD`] has it as their
/// primary group. A user or a group these files don't list, as one that only a network database
/// holds, has none, and a member that only such a database adds goes unseen.
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

/// The entries of `file`, a file in the form of [`PASSWD`] or [`GROUP`], as their name, their
/// id, and the fields after it, skipping the lines that hold none.
fn entries(file: &str) -> impl Iterator<Item = (&str, u32, std::str::Split<'_, char>)> {
    file.lines().filter_map(|line| {
        let mut fields = line.split(':');
        let name = fields.next()?;
        let id = fields.nth(1)?.parse().ok()?;
        Some((name, id, fields))
    })
}

/// `seconds` since the Unix epoch as `YYYYMMDD-hhmmss`, in UTC.
fn utc_stamp(seconds: u64) -> String {
    let Utc {
        year,
        month,
        day,
        hour,
        minute,
        second,
    } = Utc::from_unix(seconds);
    format!("{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}")
}

/// What `provider.json` holds of `placed`, whose roles are placed on `providers` and served by
/// `routes`: `{"providers": {p: {"report": r, "classes": {c: {"startup_seconds": s,
/// "usd_per_hour": h}}}}, "roles": {n: {"provider": p, "class": c}}, "routes": {n: [{"api": a,
/// "upstream": u, "models": {m: {"upstream_name": v}}}]}}`, where `r` is `p`'s report and each
/// class `c` takes `s` to start and costs `h`, if `p` states a price, as `p` declared
/// ([`Ready`](provider::Ready)); role `n` is placed on `p`, in class `c`; and each entry of
/// `n`'s model route, in its order, takes API `a` to the upstream that the deployment names `u`,
/// which takes model `m` as `v`. A role without a model route is not in `routes`.
pub(super) fn provided(
    placed: &Placed,
    providers: &ByProvider<&dyn Provider>,
    routes: &ByRole<Vec<Serving>>,
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
                (model.to_string(), json!({ "upstream_name": upstream_name }))
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

/// What a run of `workflow` keeps in its record's `ir.json`, as [`Run::prepare`] writes it.
///
/// It is the workflow's IR in a record's JSON form (`json_text`), and the IR's one encoding as
/// text, so an IR printed elsewhere is byte for byte what a run of it records.
///
/// [`Run::prepare`]: crate::Run::prepare
pub fn ir_json(workflow: &Workflow) -> String {
    json_text(&json!(workflow.ir()))
}

/// `value` as each JSON file of a record holds it: pretty-printed, with each object's keys in
/// order, then a line feed.
fn json_text(value: &Value) -> String {
    format!("{value:#}\n")
}

/// Writes `value` as the file `name` in `dir`, a directory of the record, in a record's JSON
/// form ([`json_text`]), by [`replace`] on `host`.
pub(super) fn write_json(
    host: &impl Host,
    dir: &Path,
    name: &str,
    value: &Value,
) -> Result<(), String> {
    replace(host, dir, name, json_text(value).as_bytes())
}

pub(super) fn write_ir(host: &impl Host, dir: &Path, workflow: &Workflow) -> Result<(), String> {
    replace(host, dir, "ir.json", ir_json(workflow).as_bytes())
}

/// Writes `sandbox.json` in `dir`, a sandbox's directory in the record, made if need be, by
/// [`replace`] on `host`: `{"provider": p, "id": i}`, sandbox `i` that provider `p` created;
/// with `"gpus": [m, ...]` once its session says the GPUs its `sandboxd` saw
/// ([`Session::gpus`](gateway::Session::gpus)); and, once `deleted` is the outcome of its latest
/// delete, `"delete": {"state": "deleted"}`, or `{"state": "failed"}`.
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

    /// The payload whose reference is `payload`, as [`Payloads::store`] stored it.
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

/// A payload in `payloads/`, which [`Stored::read`] reads back.
pub(super) struct Stored {
    path: PathBuf,
    reference: PayloadRef,
}

impl Stored {
    /// The payload's bytes, checked against its size and SHA-256.
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

#[derive(Clone, Copy, Debug, Default)]
struct TreeSize {
    /// The entries that aren't directories.
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
