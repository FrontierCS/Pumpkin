//! A run's record on disk: its directory and name, and the files written whole.
//!
//! Each run has a directory of its own, `<runs>/<id>/`, which [`create`] makes and names. It
//! holds:
//!
//! - `record.json`: `{"record_version": "pumpkin.record/v0"}`, the version of the record's
//!   form ([`RECORD_VERSION`]), written first;
//! - `ir.json`: the workflow's IR;
//! - `provider.json`: what the run's provider declared, its
//!   [`Ready`](pumpkin_schema::provider::Ready);
//! - `log.jsonl`: the run log, which the `log` module lays out;
//! - `outcome.json`: how the run ended: `{"state": "finished"}` and the outcome's fields, in
//!   the run log's JSON form of an outcome (`log::outcome_json`); or
//!   `{"state": "refused" | "failed", "reason": r}`.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;
use stdx::Context;

/// The version of the record's form, which a record's `record.json` holds, named as the IR's
/// version is ([`IrVersion`](pumpkin_schema::ir::IrVersion)): a reader is to check it before it
/// reads the rest. Once records are kept, a change to their form, a new file included, takes a
/// new version.
const RECORD_VERSION: &str = "pumpkin.record/v0";

/// Makes a new record directory in `runs`, creating `runs` if need be, and returns the run's
/// id, which names the directory: `now` as a UTC time, and 8 random hexadecimal digits, as
/// `20261028-131811-1a2b3c4d`, which sorts by time, and writes its `record.json`. Syncs each
/// directory that gains an entry, so that the record's directory lasts. Fails, before it makes
/// anything, if `now` is before the Unix epoch.
pub(super) fn create(runs: &Path, now: SystemTime) -> Result<(String, PathBuf), String> {
    let seconds = since_epoch(now)?.as_secs();
    create_dirs(runs)?;
    let mut random = [0; 4];
    File::open("/dev/urandom")
        .and_then(|mut urandom| urandom.read_exact(&mut random))
        .context("reading /dev/urandom for the run's id failed")?;
    let id = format!("{}-{:08x}", utc_stamp(seconds), u32::from_be_bytes(random));
    let dir = runs.join(&id);
    fs::create_dir(&dir).context(dir.display())?;
    sync_dir(runs)?;
    write_json(
        &dir,
        "record.json",
        &json!({ "record_version": RECORD_VERSION }),
    )?;
    Ok((id, dir))
}

/// Makes `dir` and those of its ancestors that are missing, as [`fs::create_dir_all`] does,
/// and syncs the parent of each directory it makes, so that each lasts.
fn create_dirs(dir: &Path) -> Result<(), String> {
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
        sync_dir(parent.unwrap_or(Path::new(".")))?;
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
/// JSON, by [`replace`].
pub(super) fn write_json(dir: &Path, name: &str, value: &serde_json::Value) -> Result<(), String> {
    replace(dir, name, format!("{value:#}\n").as_bytes())
}

/// Writes `bytes` as the file `name` in `dir`, whole or not at all, even if the machine stops:
/// to a temporary file in `dir`, synced, then renamed over `name`, and `dir` synced. The
/// temporary file is removed if it can't be written or renamed.
fn replace(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), String> {
    let temporary = dir.join(format!(".{name}.partial"));
    let path = dir.join(name);
    let written = File::create(&temporary).and_then(|mut file| {
        file.write_all(bytes)?;
        file.sync_all()
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
    sync_dir(dir)
}

/// Syncs `dir`, so that the entries made in it, or renamed into it, last.
pub(super) fn sync_dir(dir: &Path) -> Result<(), String> {
    File::open(dir)
        .and_then(|dir| dir.sync_all())
        .context(dir.display())
}
