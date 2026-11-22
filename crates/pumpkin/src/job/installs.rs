//! The installs in the controller's workspace, `<sha256>/bin/`, one of each build, and their prune.
//!
//! `retention::installs` decides which installs go. This module observes the workspace and removes
//! them.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use std::{fs, io};

use retention::KEPT_DAYS;
use retention::installs::{Install, Installs};
use stdx::Context as _;

use super::{JOBS, OWN};
use crate::workspace::{self, Workspace};
use crate::{manifest, say};

// The prefix of the directory below the workspace's `tmp/` that an install is moved into to be
// removed.
const ASIDE: &str = "install.";

/// Removes the installs that `Installs::removed` picks from the workspace in `home`. A job that
/// runs has its directory in `jobs`.
///
/// It also removes what a cut-short removal left. It says which installs it removed. It says why
/// it couldn't remove an install, and goes on with the others. It removes none until the boot unit
/// has ended (`boot::after`).
pub(super) fn collect(home: &Path, jobs: &Path) -> Result<(), String> {
    let own = workspace::exe("expected it to keep its own install")?;
    let own = own
        .parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .and_then(OsStr::to_str)
        .map(str::to_owned);
    // So that no start hands an install out meanwhile.
    let Some(_locked) = super::boot::after(jobs)? else {
        return Ok(());
    };
    let links = links(home, jobs)?;
    let workspace = Workspace::find()?;
    cleared(&workspace.tmp());
    let dir = home.join(workspace::DIR);
    let mut installs = Vec::new();
    for entry in fs::read_dir(&dir).context(dir.display())? {
        let entry = entry.context(dir.display())?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if manifest::digest(&name).is_none() {
            continue;
        }
        match observed(&entry.path(), name.clone()) {
            Ok(install) => installs.push(install),
            Err(error) => say(format_args!(
                "removing the install {}: {error}",
                &name[..12]
            )),
        }
    }
    let installs = Installs {
        own,
        links,
        installs,
    };
    let mut removed = Vec::new();
    for install in installs.removed(SystemTime::now()) {
        let name = &install.name[..12];
        match aside(&workspace, &dir.join(&install.name)) {
            Ok(()) => removed.push(name),
            Err(error) => say(format_args!("removing the install {name}: {error}")),
        }
    }
    if !removed.is_empty() {
        say(format_args!(
            "the prune removed the installs of {} that none used for {KEPT_DAYS} days: {}",
            crate::counted(removed.len(), "build"),
            removed.join(", ")
        ));
    }
    Ok(())
}

/// Reads the workspace entry `name`, at `path`, as an install.
fn observed(path: &Path, name: String) -> Result<Install, String> {
    let metadata = fs::symlink_metadata(path).context(path.display())?;
    let used = metadata.modified().context(path.display())?;
    Ok(Install {
        name,
        directory: metadata.is_dir(),
        used,
    })
}

/// Removes the install at `path`, moving it into a directory of its own below `workspace`'s
/// `tmp/` first.
fn aside(workspace: &Workspace, path: &Path) -> Result<(), String> {
    // A link and a start each take an install as whole once its `bin/pumpkin` is there. Moving it
    // aside first keeps a removal cut short from leaving part of an install in place.
    let aside = workspace.scratch(ASIDE)?;
    let moved = aside.path().join(path.file_name().unwrap_or_default());
    fs::rename(path, &moved).context(path.display())?;
    aside.close().context(moved.display())
}

/// Removes each directory in `tmp` that a cut-short removal left. Says why it couldn't remove one.
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

/// The targets of the `OWN` links of the jobs in `home` that run, `<install>/bin/pumpkin`. A job
/// that runs has its directory in `jobs`. A job with no link yet adds none.
fn links(home: &Path, jobs: &Path) -> Result<Vec<PathBuf>, String> {
    let mut links = Vec::new();
    for id in super::ids(&home.join(JOBS))? {
        let running = jobs.join(&id);
        if !running.try_exists().context(running.display())? {
            continue;
        }
        let link = home.join(JOBS).join(&id).join(OWN);
        match fs::read_link(&link) {
            Ok(target) => links.push(target),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context(link.display()),
        }
    }
    Ok(links)
}
