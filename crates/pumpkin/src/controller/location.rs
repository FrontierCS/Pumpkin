//! The controller address saved by a workspace and the lock held only while repairing it.
//! Each repair replaces the file, so even finding the same address again lets a command that
//! waited for the repair use its result.

use std::fs::{self, DirBuilder, File};
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use stdx::Context as _;

use super::aws::Located;
use super::{Name, locked};

/// How long commands share a failed repair before trying again, the first nonzero retry pause.
// As git's automatic gc shares the recent error through gc.log and tries again when it expires:
// https://github.com/git/git/blob/fb255012acbcbe81189cb8014da0280b09c66ecc/builtin/gc.c#L494-L530
const FAILURE_KEPT: Duration = Duration::from_secs(5);

pub(super) struct Location {
    dir: PathBuf,
}

#[derive(Clone)]
pub(super) struct Found {
    pub(super) at: Located,
    inode: u64,
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

    pub(super) fn found(&self) -> Result<Option<Found>, String> {
        let path = self.dir.join("found");
        let Some(file) = open(&path)? else {
            return Ok(None);
        };
        let inode = file.metadata().context(path.display())?.ino();
        Ok(serde_json::from_reader(file)
            .ok()
            .map(|at| Found { at, inode }))
    }

    pub(super) fn repair(
        &self,
        name: &Name,
        previous: Option<&Found>,
        repair: impl FnOnce() -> Result<Located, String>,
    ) -> Result<Found, String> {
        let _lock = self.lock(name)?;
        if let Some(found) = self.found()?
            && previous.is_none_or(|old| old.inode != found.inode)
        {
            return Ok(found);
        }
        self.recent()?;
        let at = match repair() {
            Ok(at) => at,
            Err(error) => {
                self.write("failed", error.as_bytes())?;
                return Err(error);
            }
        };
        let json = serde_json::to_vec(&at)
            .unwrap_or_else(|error| unreachable!("an instance id and address are JSON: {error}"));
        let inode = self.write("found", &json)?;
        let failed = self.dir.join("failed");
        match fs::remove_file(&failed) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            result => result.context(failed.display())?,
        }
        Ok(Found { at, inode })
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
        if self.found()?.is_some_and(|now| now.inode == found.inode) {
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

    fn write(&self, name: &str, bytes: &[u8]) -> Result<u64, String> {
        let mut aside = tempfile::NamedTempFile::new_in(&self.dir).context(self.dir.display())?;
        aside.write_all(bytes).context(aside.path().display())?;
        let file = self.dir.join(name);
        let saved = aside.persist(&file).context(file.display())?;
        Ok(saved.metadata().context(file.display())?.ino())
    }
}

fn open(path: &Path) -> Result<Option<File>, String> {
    match File::open(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        result => result.map(Some).context(path.display()),
    }
}
