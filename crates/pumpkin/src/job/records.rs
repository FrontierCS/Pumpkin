//! The prune of the runs' records. A run's record is evidence, and the controller's may be its only
//! copy, so no shortage of room removes it: it lasts until the run's member has copied it whole, as
//! `COPIED` in the run's job's directory says, then `KEPT` from that copy, the time that the
//! controller keeps the run's tree to run it again. With `[publish]`, the records are the public
//! page's, and the job's caller prunes none.

use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use provider::RunTag;
use runtime::Record;
use stdx::Context as _;

use super::{JOBS, KEPT, KEPT_DAYS, ids};
use crate::member::Member;
use crate::{counted, next, say, size};

// The file in a run's job's directory that says that the run's member holds a whole copy of its
// record, by being there, and when they last copied it, by its time.
const COPIED: &str = "copied";

/// Notes, in its job's directory in `home`, that `member` holds a whole copy of run `id`'s record
/// as of now, if `member` is the run's, or the run names none; notes nothing if no job started the
/// run, so that the run has no such directory.
// Only the member's copy, as another member's may be gone without the run's member knowing.
pub(crate) fn mark_copied(home: &Path, id: &RunTag, member: Option<&Member>) -> Result<(), String> {
    let job = home.join(JOBS).join(id.as_str());
    match super::member(&job)? {
        Some(owner) if Some(&owner) != member => return Ok(()),
        Some(_) | None => {}
    }
    let path = job.join(COPIED);
    let marked = (OpenOptions::new().write(true).create(true).truncate(false)).open(&path);
    match marked {
        // As of this copy, not the first: the record lasts `KEPT` from the last.
        Ok(file) => file.set_modified(SystemTime::now()).context(path.display()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context(path.display()),
    }
}

/// Removes each record whose job in `home` no longer runs, as its directory in `jobs` is gone,
/// and whose member last copied it more than `KEPT` ago, unless something holds it or wrote it
/// since (`Record::remove`), and what a removal cut short left; says which it removed, with the
/// room they took, and why it couldn't remove one, going on past it. Removes none until the boot
/// unit has ended (`boot::after`), as the boot may reap a record.
pub(super) fn collect(home: &Path, jobs: &Path) -> Result<(), String> {
    let runs = PathBuf::from(crate::default_runs()?);
    let Some(_locked) = super::boot::after(jobs)? else {
        return Ok(());
    };
    for failed in Record::finish_removals(&runs) {
        say(format_args!("finishing the removal of a record: {failed}"));
    }
    let mut removed = Vec::new();
    let mut removed_bytes = 0;
    for id in ids(&home.join(JOBS))? {
        match removed_record(home, &runs, jobs, &id) {
            Ok(Some(size_bytes)) => {
                removed.push(id);
                removed_bytes += size_bytes;
            }
            Ok(None) => {}
            Err(error) => say(format_args!("removing run {id}'s record: {error}")),
        }
    }
    if !removed.is_empty() {
        say(format_args!(
            "the prune removed the records of {} copied over {KEPT_DAYS} days ago, {}: {}",
            counted(removed.len(), "run"),
            size(removed_bytes),
            removed.join(", ")
        ));
    }
    Ok(())
}

/// Removes the record of run `id` in `runs` as `collect` says, returning the room it took, or
/// `None` if it is kept.
fn removed_record(home: &Path, runs: &Path, jobs: &Path, id: &str) -> Result<Option<u64>, String> {
    let Some(copied) = copied(home, id)? else {
        return Ok(None);
    };
    let since = SystemTime::now().duration_since(copied).unwrap_or_default();
    if since <= KEPT || exists(&jobs.join(id))? {
        return Ok(None);
    }
    Record::remove(&runs.join(id), copied, size_bytes)
}

/// What a refusal for want of room adds of the records whose jobs in `home` have ended, as their
/// directories in `jobs` are gone, and that their members haven't copied, which no prune removes:
/// how many, the room they take, and the command that copies the oldest; nothing if there are
/// none, or saying why it can't tell.
pub(super) fn uncopied(home: &Path, jobs: &Path) -> String {
    match uncopied_in(home, jobs) {
        Ok(Some(uncopied)) => format!("; {uncopied}"),
        Ok(None) => String::new(),
        Err(error) => {
            say(format_args!(
                "finding the records that their members haven't copied failed: {error}"
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
        uncopied_bytes += size_bytes(&record);
        uncopied.push(id);
    }
    // Ids sort as their runs started.
    let Some(oldest) = uncopied.first() else {
        return Ok(None);
    };
    Ok(Some(format!(
        "the records of {} that their members haven't copied take {}, and each lasts until its \
         member copies it, then {KEPT_DAYS} days; to copy the oldest: {}",
        counted(uncopied.len(), "ended run"),
        size(uncopied_bytes),
        next::of_run("wait", None, oldest)
    )))
}

/// When the member of run `id`, whose job is in `home`, last copied its record, if they have.
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

/// The room on its disk that what is below `dir` takes, as `du` counts it, by its blocks, of what
/// it can read: what another prune removes meanwhile, or this can't read, it counts as none.
fn size_bytes(dir: &Path) -> u64 {
    let mut size_bytes = 0;
    let mut dirs = vec![dir.to_owned()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            // `st_blocks` counts 512-byte units, whatever the disk's block size.
            size_bytes += metadata.blocks() * 512;
            if metadata.is_dir() {
                dirs.push(entry.path());
            }
        }
    }
    size_bytes
}
