//! The controller address saved by a workspace and the lock held only while repairing it.
//! Each repair replaces the file, so even finding the same address again lets a command that
//! waited for the repair use its result.

use std::fs::{self, DirBuilder, File};
use std::io::{self, Read as _, Write as _};
use std::net::IpAddr;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use stdx::Context as _;

use super::{Address, Name, locked};

/// How long commands share a failed repair before trying again, the first nonzero retry pause.
// As git's automatic gc shares the recent error through gc.log and tries again when it expires:
// https://github.com/git/git/blob/fb255012acbcbe81189cb8014da0280b09c66ecc/builtin/gc.c#L494-L530
const FAILURE_KEPT: Duration = super::AGAIN[1];

/// Where a controller was found.
// Untagged, so that an EC2 controller's is the object `{instance, address}` that a workspace's
// `found` holds.
#[derive(Deserialize, Serialize)]
#[serde(untagged)]
pub(super) enum Located {
    /// Its EC2 instance, and the private address at which its `sshd` answered, at port 22.
    Ec2 { instance: String, address: IpAddr },
    /// The address that the configuration names.
    Address(Address),
}

pub(super) struct Location {
    dir: PathBuf,
}

pub(super) struct Found {
    pub(super) at: Located,
    // Keep the file open so a later repair cannot reuse its inode, as GNU find does for -samefile.
    // https://git.savannah.gnu.org/cgit/findutils.git/tree/find/parser.c?id=71d158fdd51b755fd85cc763435fb675b541acd9#n2253
    file: File,
}

impl Found {
    fn same_file(&self, other: &Self, path: &Path) -> Result<bool, String> {
        let inode = |file: &File| {
            file.metadata()
                .map(|stat| (stat.dev(), stat.ino()))
                .context(path.display())
        };
        Ok(inode(&self.file)? == inode(&other.file)?)
    }
}

impl Location {
    pub(super) fn new(dir: PathBuf) -> Result<Self, String> {
        DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(&dir)
            .context(dir.display())?;
        Ok(Self { dir })
    }

    pub(super) fn diagnostics(&self) -> Result<tempfile::NamedTempFile, String> {
        tempfile::Builder::new()
            .prefix(".ssh-")
            .tempfile_in(&self.dir)
            .context(self.dir.display())
    }

    pub(super) fn found(&self) -> Result<Option<Found>, String> {
        let path = self.dir.join("found");
        let Some(mut file) = open(&path)? else {
            return Ok(None);
        };
        let mut json = Vec::new();
        file.read_to_end(&mut json).context(path.display())?;
        Ok(serde_json::from_slice(&json)
            .ok()
            .map(|at| Found { at, file }))
    }

    pub(super) fn repair(
        &self,
        name: &Name,
        previous: Option<&Found>,
        repair: impl FnOnce() -> Result<Located, String>,
    ) -> Result<Found, String> {
        let _lock = self.lock(name)?;
        self.recent()?;
        if let Some(found) = self.found()?
            && !matches!(previous, Some(old) if old.same_file(&found, &self.dir.join("found"))?)
        {
            return Ok(found);
        }
        let at = match repair() {
            Ok(at) => at,
            Err(error) => {
                self.write("failed", error.as_bytes())?;
                return Err(error);
            }
        };
        let json = serde_json::to_vec(&at).context("encoding the controller's location")?;
        let file = self.write("found", &json)?;
        let failed = self.dir.join("failed");
        match fs::remove_file(&failed) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            result => result.context(failed.display())?,
        }
        Ok(Found { at, file })
    }

    pub(super) fn recent(&self) -> Result<(), String> {
        let path = self.dir.join("failed");
        if let Some(mut failed) = open(&path)? {
            let modified = failed
                .metadata()
                .and_then(|metadata| metadata.modified())
                .context(path.display())?;
            if modified.elapsed().is_ok_and(|age| age < FAILURE_KEPT) {
                let mut error = String::new();
                failed.read_to_string(&mut error).context(path.display())?;
                return Err(error);
            }
        }
        Ok(())
    }

    pub(super) fn failed(&self, name: &Name, found: &Found, error: &str) -> Result<(), String> {
        let _lock = self.lock(name)?;
        if let Some(now) = self.found()?
            && now.same_file(found, &self.dir.join("found"))?
        {
            self.write("failed", error.as_bytes())?;
        }
        Ok(())
    }

    fn lock(&self, name: &Name) -> Result<File, String> {
        locked(
            &self.dir.join("lock"),
            format_args!("waiting for another command, which is reaching controller `{name}`"),
        )
    }

    fn write(&self, name: &str, bytes: &[u8]) -> Result<File, String> {
        let mut aside = tempfile::NamedTempFile::new_in(&self.dir).context(self.dir.display())?;
        aside.write_all(bytes).context(aside.path().display())?;
        let file = self.dir.join(name);
        aside.persist(&file).context(file.display())
    }
}

fn open(path: &Path) -> Result<Option<File>, String> {
    match File::open(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        result => result.map(Some).context(path.display()),
    }
}
