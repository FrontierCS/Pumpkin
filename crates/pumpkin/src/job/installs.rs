//! The prune of the job installs, each build that a start keeps for its jobs in the controller's
//! workspace, `<sha256>/`. An install is held by each job that runs it, or that started within
//! `KEPT`, by its `OWN` link, and lasts `KEPT` after a start last handed it out, as `serve`'s
//! installs last a day after a `serve` last used them: long enough to outlast its runs' hard ends
//! and the boot after them, which runs each install's reap, though the controller stays stopped
//! for days.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::SystemTime;
use std::{fs, io};

use schema::ir::Sha256Digest;
use stdx::Context as _;

use super::{JOBS, KEPT, OWN};
use crate::link::Boot;
use crate::workspace::{self, Workspace};
use crate::{manifest, say};

/// Removes each install in the workspace in `home`, but this `pumpkin`'s, that no job runs, a job
/// that runs having its directory in `jobs`, that no job started within `KEPT` ran, and that no
/// start has handed out for as long, while the jobs' directory is locked, so that no start hands
/// one out meanwhile; and says which it removed. Removes none while the boot unit runs.
pub(super) fn collect(home: &Path, jobs: &Path) -> Result<(), String> {
    // The boot may be running an old install's reap, as after a stop of days.
    if matches!(crate::serve::boot(home), Boot::Running) {
        return Ok(());
    }
    let own = workspace::exe("expected it to keep its own install")?;
    let own = own
        .parent()
        .and_then(Path::parent)
        .and_then(Path::file_name);
    let locked = super::open_jobs(jobs)?;
    locked.lock().context(super::jobs_named(jobs))?;
    let held = held(home, jobs)?;
    let dir = home.join(workspace::DIR);
    let now = SystemTime::now();
    let mut removed = Vec::new();
    for entry in fs::read_dir(&dir).context(dir.display())? {
        let entry = entry.context(dir.display())?;
        let name = entry.file_name();
        let Some(digest) = name.to_str().and_then(manifest::digest) else {
            continue;
        };
        let path = entry.path();
        let metadata = entry.metadata().context(path.display())?;
        let modified = metadata.modified().context(path.display())?;
        if !metadata.is_dir()
            || Some(name.as_os_str()) == own
            || held.contains(&digest)
            || now.duration_since(modified).unwrap_or_default() <= KEPT
        {
            continue;
        }
        // Moved aside first, so that a removal cut short leaves no install that a start takes as
        // whole, as a start looks only for its `bin/pumpkin`.
        let aside = Workspace::find()?.scratch("install.")?;
        let moved = aside.path().join(&name);
        fs::rename(&path, &moved).context(path.display())?;
        aside.close().context(moved.display())?;
        removed.push(digest);
    }
    if !removed.is_empty() {
        let days = KEPT.as_secs() / (24 * 60 * 60);
        let named: Vec<&str> = removed.iter().map(|digest| &digest.hex()[..12]).collect();
        say(format_args!(
            "the prune removed the installs of {} that no job used for {days} days: {}",
            crate::counted(removed.len(), "build"),
            named.join(", ")
        ));
    }
    Ok(())
}

/// The installs of the jobs in `home` that run, as their directories in `jobs` say, or that
/// started within `KEPT`, each named by the `OWN` link of its run's directory, once the
/// job has made it.
// By the jobs' own links too, not only by the mark of each start: another build's start, as of
// an older client, leaves the install it hands out unmarked.
fn held(home: &Path, jobs: &Path) -> Result<BTreeSet<Sha256Digest>, String> {
    let cutoff = crate::unix_now().since_epoch().saturating_sub(KEPT);
    let mut held = BTreeSet::new();
    for id in super::ids(&home.join(JOBS))? {
        let recent = runtime::minted(&id).is_some_and(|minted| minted >= cutoff);
        let running = jobs.join(&id);
        if !recent && !running.try_exists().context(running.display())? {
            continue;
        }
        let link = home.join(JOBS).join(&id).join(OWN);
        let target = match fs::read_link(&link) {
            Ok(target) => target,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context(link.display()),
        };
        // `<sha256>/bin/pumpkin`.
        let install = target.parent().and_then(Path::parent);
        let name = install
            .and_then(Path::file_name)
            .and_then(|name| name.to_str());
        held.extend(name.and_then(manifest::digest));
    }
    Ok(held)
}
