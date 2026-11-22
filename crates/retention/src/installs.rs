//! The installs in the controller's workspace, `<sha256>/bin/`, one of each build.
//!
//! A link uploads a build's install to run its `serve`. Each of the build's jobs runs it too.
//!
//! An install lasts `KEPT_DAYS` days after a `serve` or a start last marked it, and while a running
//! job's link names it. So it outlasts the hard ends of the runs of a start and the boot after
//! them, which runs the install's reap, even if the controller stays stopped for days.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::KEPT;

/// The installs in the controller's workspace, as a prune observed them while no start could hand
/// one out.
#[derive(Clone, Debug)]
pub struct Installs {
    /// The name of the install of the `pumpkin` that prunes, if it runs from one.
    pub own: Option<String>,
    /// The target of each running job's link to its `pumpkin`, `<install>/bin/pumpkin`.
    pub links: Vec<PathBuf>,
    /// The entries of the workspace that a SHA-256 names, as it names an install.
    pub installs: Vec<Install>,
}

/// An entry of the controller's workspace that a SHA-256 names, as it names an install.
#[derive(Clone, Debug)]
pub struct Install {
    pub name: String,
    /// Whether it is a directory, as an install is.
    pub directory: bool,
    /// When a `serve` or a start last used it, as its time of modification says.
    pub used: SystemTime,
}

impl Installs {
    /// The installs that a prune removes at `now`, in their order.
    ///
    /// Each is a directory, and not `own`. No link names it. No `serve` or start used it in the
    /// `KEPT_DAYS` days before `now`.
    // A run may outlast `KEPT` after its start's mark, so the links hold an install too. A job that
    // runs with no link yet has just started, and its start marked its install.
    pub fn removed(&self, now: SystemTime) -> Vec<&Install> {
        let held: BTreeSet<&str> = (self.links.iter())
            .filter_map(|link| {
                let install = link.parent().and_then(Path::parent)?;
                install.file_name()?.to_str()
            })
            .collect();
        (self.installs.iter())
            .filter(|install| {
                let name = install.name.as_str();
                self.own.as_deref() != Some(name)
                    && !held.contains(name)
                    && install.directory
                    && crate::past(install.used, KEPT, now)
            })
            .collect()
    }
}
