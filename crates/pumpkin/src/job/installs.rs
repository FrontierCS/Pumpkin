//! The prune of the job installs. A start keeps the build that its jobs run as an install in the
//! controller's workspace, `<sha256>/`. A job holds its install by its `OWN` link while it runs,
//! and for `KEPT` after it started. An install also lasts `KEPT` after a start last handed it out.
//! `KEPT` outlasts the hard ends of an install's runs and the boot after them, which runs each
//! install's reap. It does so even when the controller stays stopped for days.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::Path;
use std::time::SystemTime;
use std::{fs, io};

use schema::ir::Sha256Digest;
use stdx::Context as _;

use super::{JOBS, KEPT, OWN};
use crate::workspace::{self, Workspace};
use crate::{manifest, say};

// The prefix of the directory below the workspace's `tmp/` that holds an install while it is
// removed.
const ASIDE: &str = "install.";

/// Removes each install in the workspace in `home` that no job holds and no start has handed out
/// within `KEPT`. A job that runs has its directory in `jobs`. Keeps this `pumpkin`'s own install.
/// Also removes what an earlier removal cut short. Says which installs it removed. Says why it
/// couldn't remove an install, and goes on with the others. Removes none until the boot unit has
/// ended (`boot::after`). Removes none while a job that runs or started within `KEPT` names no
/// install.
pub(super) fn collect(home: &Path, jobs: &Path) -> Result<(), String> {
    let own = workspace::exe("expected it to keep its own install")?;
    let own = own
        .parent()
        .and_then(Path::parent)
        .and_then(Path::file_name);
    // So that no start hands an install out meanwhile.
    let Some(_locked) = super::boot::after(jobs)? else {
        return Ok(());
    };
    let Some(held) = held(home, jobs)? else {
        return Ok(());
    };
    let workspace = Workspace::find()?;
    cleared(&workspace.tmp());
    let dir = home.join(workspace::DIR);
    let mut removed = Vec::new();
    for entry in fs::read_dir(&dir).context(dir.display())? {
        let entry = entry.context(dir.display())?;
        let name = entry.file_name();
        let Some(digest) = name.to_str().and_then(manifest::digest) else {
            continue;
        };
        if Some(name.as_os_str()) == own || held.contains(&digest) {
            continue;
        }
        match idle_removed(&workspace, &entry.path()) {
            Ok(true) => removed.push(digest),
            Ok(false) => {}
            Err(error) => say(format_args!(
                "removing the install {}: {error}",
                &digest.hex()[..12]
            )),
        }
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

/// Removes the install at `path` if it is a directory that no start has handed out within `KEPT`.
/// Moves it into a directory of its own below `workspace`'s `tmp/` first. Returns whether it
/// removed the install.
fn idle_removed(workspace: &Workspace, path: &Path) -> Result<bool, String> {
    let metadata = fs::symlink_metadata(path).context(path.display())?;
    let modified = metadata.modified().context(path.display())?;
    let unused = SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default();
    if !metadata.is_dir() || unused <= KEPT {
        return Ok(false);
    }
    // A start takes an install as whole once its `bin/pumpkin` is there. Moving it aside first
    // keeps a removal cut short from leaving part of an install in place.
    let aside = workspace.scratch(ASIDE)?;
    let moved = aside.path().join(path.file_name().unwrap_or_default());
    fs::rename(path, &moved).context(path.display())?;
    aside.close().context(moved.display())?;
    Ok(true)
}

/// Removes each directory in `tmp` that an install's removal left. Says why it couldn't remove one.
fn cleared(tmp: &Path) {
    let Ok(entries) = fs::read_dir(tmp) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.as_encoded_bytes().starts_with(ASIDE.as_bytes()) {
            continue;
        }
        let path = entry.path();
        if let Err(error) = fs::remove_dir_all(&path) {
            say(format_args!("removing {}: {error}", path.display()));
        }
    }
}

/// The installs that the jobs in `home` hold, each named by the `OWN` link of its run's
/// directory. A job holds its install while it runs, as its directory in `jobs` shows, and
/// within `KEPT` of its start. Returns `None` if one of those jobs has no `OWN` link yet. A job
/// that is starting may run any install.
// Another build's start, such as an older client's, leaves the install it hands out unmarked. So
// the jobs' own links hold installs too, not only each start's mark.
fn held(home: &Path, jobs: &Path) -> Result<Option<BTreeSet<Sha256Digest>>, String> {
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
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context(link.display()),
        };
        // `<sha256>/bin/pumpkin`.
        let install = target.parent().and_then(Path::parent);
        let name = install.and_then(Path::file_name).and_then(OsStr::to_str);
        held.extend(name.and_then(manifest::digest));
    }
    Ok(Some(held))
}
