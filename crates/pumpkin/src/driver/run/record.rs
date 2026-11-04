//! A run's record on disk: its directory and name, the payloads stored by digest, and the
//! files written whole.
//!
//! Each run has a directory of its own, `<runs>/<id>/`, where the id is the UTC time and a
//! random suffix, as `20261028-131811-1a2b3c4d`. It holds:
//!
//! - `ir.json`: the workflow's IR;
//! - `engine.json`: the Docker engine's [report](pumpkin_docker::Report);
//! - `log.jsonl`: the run log;
//! - `payloads/<hex>`: the payload of each call the core accepted, named by its SHA-256. A
//!   refused call's payload appears in the log by its size and digest, but its bytes are not
//!   kept;
//! - `jobs/<n>/out` and `jobs/<n>/log`: judge call `n`'s result, evidence and logs;
//! - `agent/out` and `agent/log`: the agent's outputs and logs, collected when the work phase
//!   ends;
//! - `outcome.json`: how the run ended: `{"state": "finished"}` and the outcome's fields, in the
//!   run log's JSON form of an outcome (`log::outcome_json`); or `{"state": "refused" |
//!   "failed", "reason": r}`.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pumpkin_schema::ir::Sha256Digest;
use pumpkin_schema::sandbox::{log_dir, out_dir};
use sha2::{Digest, Sha256};
use stdx::Context;

use crate::driver::core::PayloadRef;

/// Makes a new record directory in `runs`, creating `runs` if need be, and returns the run's
/// id, which names the directory: the UTC time and 8 random hexadecimal digits, as
/// `20261028-131811-1a2b3c4d`, which sorts by time. Syncs `runs`, so that the directory lasts.
pub(super) fn create(runs: &Path) -> Result<(String, PathBuf), String> {
    fs::create_dir_all(runs).context(runs.display())?;
    let mut random = [0; 4];
    File::open("/dev/urandom")
        .and_then(|mut urandom| urandom.read_exact(&mut random))
        .context("reading /dev/urandom for the run's id failed")?;
    let seconds = since_epoch().as_secs();
    let id = format!("{}-{:08x}", utc_stamp(seconds), u32::from_be_bytes(random));
    let dir = runs.join(&id);
    fs::create_dir(&dir).context(dir.display())?;
    sync_dir(runs)?;
    Ok((id, dir))
}

/// The time since the Unix epoch now, by this machine's clock: zero if the clock is set before
/// 1970.
pub(super) fn since_epoch() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
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
/// JSON, by [`replace`].
pub(super) fn write_json(dir: &Path, name: &str, value: &serde_json::Value) -> Result<(), String> {
    replace(dir, name, format!("{value:#}\n").as_bytes())
}

/// Writes `bytes` as the file `name` in `dir`, whole or not at all, even if the machine stops:
/// to a temporary file in `dir`, synced, then renamed over `name`, and `dir` synced.
fn replace(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), String> {
    let temporary = dir.join(format!(".{name}.partial"));
    let written = File::create(&temporary).and_then(|mut file| {
        file.write_all(bytes)?;
        file.sync_all()
    });
    written.context(temporary.display())?;
    let path = dir.join(name);
    fs::rename(&temporary, &path).context(path.display())?;
    sync_dir(dir)
}

/// Syncs `dir`, so that the entries made in it, or renamed into it, last.
pub(super) fn sync_dir(dir: &Path) -> Result<(), String> {
    File::open(dir)
        .and_then(|dir| dir.sync_all())
        .context(dir.display())
}

/// The reference to an op's payload: its size and SHA-256.
pub(super) fn reference(bytes: &[u8]) -> PayloadRef {
    let sha256: [u8; 32] = Sha256::digest(bytes).into();
    PayloadRef {
        size: bytes.len() as u64,
        digest: Sha256Digest::from(sha256),
    }
}

/// The payloads of the calls a run accepted, each stored once in `payloads/`, under its
/// SHA-256 in hexadecimal.
#[derive(Debug)]
pub(super) struct Payloads {
    dir: PathBuf,
}

impl Payloads {
    /// Makes `payloads/` in the record's directory `record`, and syncs `record`, so that it
    /// lasts.
    pub(super) fn create(record: &Path) -> Result<Self, String> {
        let dir = record.join("payloads");
        fs::create_dir(&dir).context(dir.display())?;
        sync_dir(record)?;
        Ok(Self { dir })
    }

    /// Stores `bytes`, whose reference is `payload`, by [`replace`], unless they are stored
    /// already: a file has a digest's name only once it holds all of its bytes.
    pub(super) fn store(&self, bytes: &[u8], payload: &PayloadRef) -> Result<(), String> {
        let path = self.path(&payload.digest);
        match fs::symlink_metadata(&path) {
            Ok(_) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                replace(&self.dir, hex(&payload.digest), bytes)
            }
            Err(error) => Err(error).context(path.display()),
        }
    }

    /// Where the payload with `digest` is stored.
    pub(super) fn path(&self, digest: &Sha256Digest) -> PathBuf {
        self.dir.join(hex(digest))
    }
}

/// `digest`'s hexadecimal digits, which name its payload's file.
fn hex(digest: &Sha256Digest) -> &str {
    digest.as_str().trim_start_matches("sha256:")
}

/// What the files below a directory hold, in all.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct TreeSize {
    /// The entries that aren't directories.
    pub(super) files: u64,
    pub(super) bytes: u64,
}

/// What the files below `dir` hold, in all.
pub(super) fn tree_size(dir: &Path) -> io::Result<TreeSize> {
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
