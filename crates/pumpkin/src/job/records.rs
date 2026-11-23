//! The runs' records on the controller, and their prune. A run's record is evidence, and the
//! controller's copy may be the only one. So no shortage of room removes a record.
//! `retention::records` says how long a record lasts. `COPIED` in the run's job's directory notes
//! when its member last copied the record.

use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use members::Member;
use provider::RunTag;
use retention::KEPT_DAYS;
use runtime::Record;
use stdx::Context as _;

use super::{JOBS, ids};
use crate::{counted, next, say, size};

// The file in a run's job's directory that notes the member's copy of the run's record. It is
// there once the run's member holds a whole copy. Its time is when they last copied it.
const COPIED: &str = "copied";

/// Notes that `member` holds a whole copy of run `id`'s record as of now, in the run's job's
/// directory in `home`. Notes it only if `member` is the run's. Notes nothing if no job started
/// the run, as such a run has no job directory.
// Another member's copy may be gone without the run's member knowing.
pub(crate) fn mark_copied(home: &Path, id: &RunTag, member: &Member) -> Result<(), String> {
    let job = home.join(JOBS).join(id.as_str());
    match super::run_member(&job)? {
        Some(owner) if owner == *member => {}
        Some(_) | None => return Ok(()),
    }
    let path = job.join(COPIED);
    let marked = (OpenOptions::new().write(true).create(true).truncate(false)).open(&path);
    match marked {
        // The record lasts `KEPT_DAYS` days from the last copy, not the first.
        Ok(file) => file.set_modified(SystemTime::now()).context(path.display()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context(path.display()),
    }
}

/// Removes the records of the jobs in `home` that `retention::records::removed` picks. A job that
/// runs has its directory in `jobs`.
///
/// It leaves a record that something holds, or that was written since its copy
/// (`Record::remove`). It also finishes each removal cut short. It says which records it removed,
/// and the room they took. It says why it couldn't remove a record, and goes on with the others.
/// It removes none until the boot unit has ended (`boot::after`).
// The boot may reap a record.
pub(super) fn collect(home: &Path, jobs: &Path) -> Result<(), String> {
    let runs = PathBuf::from(crate::default_runs()?);
    let Some(_locked) = super::boot::after(jobs)? else {
        return Ok(());
    };
    for failed in Record::finish_removals(&runs) {
        say(format_args!("finishing the removal of a record: {failed}"));
    }
    let mut records = Vec::new();
    for id in ids(&home.join(JOBS))? {
        // A record that its member hasn't copied stays, so its job and record go unread.
        let record = copied(home, &id).and_then(|copied| {
            copied
                .map(|copied| observed(&runs, jobs, &id, Some(copied)))
                .transpose()
        });
        match record {
            Ok(record) => records.extend(record),
            Err(error) => say(format_args!("removing run {id}'s record: {error}")),
        }
    }
    let mut removed = Vec::new();
    let mut removed_bytes = 0;
    for (id, copied) in retention::records::removed(&records, SystemTime::now()) {
        match Record::remove(&runs.join(id), copied, size_bytes) {
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

/// What a refusal for want of room adds about the records that their members haven't copied. No
/// prune removes them (`retention::records::uncopied`). It counts the records whose jobs in `home`
/// have ended, as their directories in `jobs` are gone. It says how many there are, the room they
/// take, and the command that copies the oldest. It adds nothing if there are none. If it can't
/// tell, it adds nothing and says why. The command names the controller as `controller`, the
/// client's name for it.
pub(super) fn uncopied(home: &Path, jobs: &Path, controller: &crate::controller::Name) -> String {
    match uncopied_in(home, jobs, controller) {
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

fn uncopied_in(
    home: &Path,
    jobs: &Path,
    controller: &crate::controller::Name,
) -> Result<Option<String>, String> {
    let runs = PathBuf::from(crate::default_runs()?);
    let mut records = Vec::new();
    for id in ids(&home.join(JOBS))? {
        records.push(observed(&runs, jobs, &id, copied(home, &id)?)?);
    }
    let uncopied = retention::records::uncopied(&records);
    let uncopied_bytes: u64 = (uncopied.iter()).map(|id| size_bytes(&runs.join(id))).sum();
    // Ids sort as their runs started.
    let Some(oldest) = uncopied.first() else {
        return Ok(None);
    };
    Ok(Some(format!(
        "the records of {} that their members haven't copied take {}, and each lasts until its \
         member copies it, then {KEPT_DAYS} days; to copy the oldest: {}",
        counted(uncopied.len(), "ended run"),
        size(uncopied_bytes),
        next::of_run("wait", &next::At::Controller(controller.clone()), oldest)
    )))
}

/// Observes run `id` for `retention`: whether its job runs, as its directory in `jobs` says, and
/// whether its record is in `runs`. `copied` is when its member last copied the record, if they
/// have.
fn observed(
    runs: &Path,
    jobs: &Path,
    id: &str,
    copied: Option<SystemTime>,
) -> Result<retention::records::Record, String> {
    Ok(retention::records::Record {
        id: id.to_owned(),
        copied,
        running: exists(&jobs.join(id))?,
        recorded: exists(&runs.join(id))?,
    })
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

/// The room on its disk that what is below `dir` takes, counted by blocks as `du` counts it. It
/// counts as none what it can't read, or what another prune removes meanwhile.
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
