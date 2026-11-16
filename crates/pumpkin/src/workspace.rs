use std::path::{Path, PathBuf};
use std::{env, fs};

use stdx::Context as _;
use tempfile::TempDir;

pub(crate) const DIR: &str = ".pumpkin";

pub(crate) const RUNS: &str = "runs";

/// The nearest [`DIR`] above the executable of this `pumpkin`: all that `pumpkin` writes on this
/// machine is below it, so that what a repository's Pumpkin builds and records goes with the
/// repository.
pub(crate) struct Workspace(PathBuf);

/// The path of this executable. `expected` ends the message of a failure to find it.
pub(crate) fn exe(expected: &str) -> Result<PathBuf, String> {
    let exe = env::current_exe();
    exe.map_err(|error| format!("this `pumpkin`'s executable can't be found ({error}): {expected}"))
}

impl Workspace {
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
                 writes: run the one that a repository's `./pumpkin` builds, or that `cargo \
                 xtask install` installs in a checkout's `{DIR}/bin`",
                exe.display()
            ));
        };
        Ok(Self(dir.to_owned()))
    }

    /// The directory that holds [`DIR`]: the repository whose `./pumpkin` builds this `pumpkin`.
    pub(crate) fn repository(&self) -> &Path {
        self.0.parent().unwrap_or(&self.0)
    }

    pub(crate) fn runs(&self) -> PathBuf {
        self.0.join(RUNS)
    }

    pub(crate) fn tmp(&self) -> PathBuf {
        self.0.join("tmp")
    }

    /// A directory of this command's own, 0700, named from `prefix`, below [`Self::tmp`]; dropping
    /// it removes the directory.
    pub(crate) fn scratch(&self, prefix: &str) -> Result<TempDir, String> {
        let tmp = self.tmp();
        fs::create_dir_all(&tmp).context(tmp.display())?;
        let made = tempfile::Builder::new().prefix(prefix).tempdir_in(&tmp);
        made.context(tmp.display())
    }

    pub(crate) fn slots(&self) -> PathBuf {
        self.0.join("slots")
    }

    pub(crate) fn controller(&self) -> PathBuf {
        self.0.join("controller")
    }

    pub(crate) fn docker(&self) -> PathBuf {
        self.0.join("docker")
    }

    pub(crate) fn ec2(&self) -> PathBuf {
        self.0.join("ec2")
    }

    pub(crate) fn modal(&self) -> PathBuf {
        self.0.join("modal")
    }
}
