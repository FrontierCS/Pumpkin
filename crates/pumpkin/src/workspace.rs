//! The workspace's `.pumpkin` directory, which holds all that `pumpkin` writes on this machine
//! ([`Workspace`]), so that what a repository's Pumpkin builds, records and fetches stays with
//! the repository, apart from every other's, and goes with the directory.

use std::env;
use std::path::PathBuf;

/// The name of the directory that a workspace keeps Pumpkin in.
pub(crate) const DIR: &str = ".pumpkin";

/// The runs directory of a workspace's [`DIR`], its default one.
pub(crate) const RUNS: &str = "runs";

/// The workspace's `.pumpkin` directory: the nearest one of that name that holds the executable
/// of this `pumpkin`, which `cargo xtask install` puts in a checkout's `.pumpkin/bin`. It holds
/// the default runs directory, [`RUNS`], and a directory for the providers of each kind, by its
/// name.
pub(crate) struct Workspace(PathBuf);

/// This `pumpkin`'s executable, by its canonical path, as Linux gives it.
///
/// # Errors
///
/// Fails, saying then what was `expected` of it, if it can't be found.
pub(crate) fn exe(expected: &str) -> Result<PathBuf, String> {
    let exe = env::current_exe();
    exe.map_err(|error| format!("this `pumpkin`'s executable can't be found ({error}): {expected}"))
}

impl Workspace {
    /// The workspace's directory of this `pumpkin`.
    ///
    /// # Errors
    ///
    /// Fails if this `pumpkin`'s executable can't be found, or is in no `.pumpkin` directory.
    pub(crate) fn find() -> Result<Self, String> {
        let exe = exe(&format!(
            "expected it in a workspace's `{DIR}` directory, which holds all it writes"
        ))?;
        let found = exe
            .ancestors()
            .find(|dir| dir.file_name() == Some(DIR.as_ref()));
        let Some(dir) = found else {
            return Err(format!(
                "this `pumpkin`, {}, is in no directory named `{DIR}`, which would hold all it \
                 writes: run the one that `cargo xtask install` installs in a checkout's \
                 `{DIR}/bin`",
                exe.display()
            ));
        };
        Ok(Self(dir.to_owned()))
    }

    /// The default runs directory.
    pub(crate) fn runs(&self) -> PathBuf {
        self.0.join(RUNS)
    }

    /// The directory of what the Docker engine writes on this machine ([`engine::Engine`]).
    pub(crate) fn docker(&self) -> PathBuf {
        self.0.join("docker")
    }

    /// The directory of what providers of kind `ec2` write on this machine ([`ec2::Ec2`]).
    pub(crate) fn ec2(&self) -> PathBuf {
        self.0.join("ec2")
    }
}
