//! The prune of the runs' records. A run's record is evidence, and the controller's may be its only
//! copy, so no shortage of room removes it: it lasts until a machine has copied it whole, as
//! `COPIED` in the run's job's directory says, then `KEPT` from that copy, the time that the
//! controller keeps the run's tree to run it again. With `[publish]`, the records are the public
//! page's, and the prune keeps them all.

use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use provider::RunTag;
use runtime::Record;
use stdx::Context as _;

use super::{JOBS, KEPT, KEPT_DAYS, ids};
use crate::{counted, next, say};

// The file in a run's job's directory that says that a machine holds a whole copy of the run's
// record, by being there, and when one last copied it, by its time.
const COPIED: &str = "copied";

/// Notes, in its job's directory in `home`, that a machine holds a whole copy of run `id`'s record
/// as of now, unless no job started the run, so that the run has no such directory.
pub(crate) fn mark_copied(home: &Path, id: &RunTag) -> Result<(), String> {
    let path = home.join(JOBS).join(id.as_str()).join(COPIED);
    let marked = (OpenOptions::new().write(true).create(true).truncate(false)).open(&path);
    match marked {
        // As of this copy, not the first: the record lasts `KEPT` from the last.
        Ok(file) => file.set_modified(SystemTime::now()).context(path.display()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context(path.display()),
    }
}

/// Removes, of the records in `runs`, each whose job in `home` no longer runs, as its directory in
/// `jobs` is gone, and was last copied more than `KEPT` ago, unless something holds it
/// (`Record::remove`); and says which it removed, with the room they took.
pub(super) fn collect(home: &Path, runs: &Path, jobs: &Path) -> Result<(), String> {
    let now = SystemTime::now();
    let mut removed = Vec::new();
    let mut removed_bytes = 0;
    for id in ids(&home.join(JOBS))? {
        let Some(copied) = copied(home, &id)? else {
            continue;
        };
        let record = runs.join(&id);
        let kept = now.duration_since(copied).unwrap_or_default() <= KEPT;
        if kept || exists(&jobs.join(&id))? || !exists(&record)? {
            continue;
        }
        let size_bytes = size_bytes(&record)?;
        if Record::remove(runs, &id)? {
            removed.push(id);
            removed_bytes += size_bytes;
        }
    }
    if !removed.is_empty() {
        say(format_args!(
            "the prune removed the records of {} copied over {KEPT_DAYS} days ago, {}: {}",
            counted(removed.len(), "run"),
            gigabytes(removed_bytes),
            removed.join(", ")
        ));
    }
    Ok(())
}

/// What a refusal for want of room adds of the records whose jobs in `home` have ended, as their
/// directories in `jobs` are gone, and that no machine has copied, which no prune removes: how
/// many, the room they take, and the command that copies the oldest; nothing if there are none,
/// or saying why it can't tell.
pub(super) fn uncopied(home: &Path, jobs: &Path) -> String {
    match uncopied_in(home, jobs) {
        Ok(Some(uncopied)) => format!("; {uncopied}"),
        Ok(None) => String::new(),
        Err(error) => {
            say(format_args!(
                "finding the records that no machine has copied failed: {error}"
            ));
            String::new()
        }
    }
}

fn uncopied_in(home: &Path, jobs: &Path) -> Result<Option<String>, String> {
    let runs = PathBuf::from(crate::default_runs()?);
    let mut uncopied = Vec::new();
    let mut uncopied_bytes = 0;
    for id in ids(&home.join(JOBS))? {
        let record = runs.join(&id);
        if copied(home, &id)?.is_some() || exists(&jobs.join(&id))? || !exists(&record)? {
            continue;
        }
        uncopied_bytes += size_bytes(&record)?;
        uncopied.push(id);
    }
    // Ids sort as their runs started.
    let Some(oldest) = uncopied.first() else {
        return Ok(None);
    };
    Ok(Some(format!(
        "the records of {} that no machine has copied take {}, and stay until one copies them; \
         to copy the oldest: {}",
        counted(uncopied.len(), "ended run"),
        gigabytes(uncopied_bytes),
        next::of_run("wait", None, oldest)
    )))
}

/// When a machine last copied the record of run `id`, whose job is in `home`, if one has.
fn copied(home: &Path, id: &str) -> Result<Option<SystemTime>, String> {
    let path = home.join(JOBS).join(id).join(COPIED);
    match fs::metadata(&path).and_then(|metadata| metadata.modified()) {
        Ok(modified) => Ok(Some(modified)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context(path.display()),
    }
}

fn exists(path: &Path) -> Result<bool, String> {
    path.try_exists().context(path.display())
}

/// The room on its disk that what is below `dir` takes, as `du` counts it, by its blocks.
fn size_bytes(dir: &Path) -> Result<u64, String> {
    let mut size_bytes = 0;
    let mut dirs = vec![dir.to_owned()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).context(dir.display())? {
            let entry = entry.context(dir.display())?;
            let metadata = entry.metadata().context(entry.path().display())?;
            // `st_blocks` counts 512-byte units, whatever the disk's block size.
            size_bytes += metadata.blocks() * 512;
            if metadata.is_dir() {
                dirs.push(entry.path());
            }
        }
    }
    Ok(size_bytes)
}

/// `bytes` in gigabytes of 10^9 bytes, to a tenth.
fn gigabytes(bytes: u64) -> String {
    let tenths = bytes / 100_000_000;
    format!("{}.{} GB", tenths / 10, tenths % 10)
}
