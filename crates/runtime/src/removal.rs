//! The end of an ended record: a reader that holds it keeps it whole, and its removal takes it
//! whole once nothing holds it, so that a reader of the runs directory finds all of a record or
//! none of it.

use std::fs::{self, File, TryLockError};
use std::io;
use std::path::Path;

use stdx::Context as _;

use crate::Record;

impl Record {
    /// Calls `read` while it holds the record in `dir` as ended, sharing the lock on its
    /// `driver.pid` that its driver has let go of, so that [`Record::remove`] leaves the record be
    /// meanwhile; returns `None`, calling nothing, while its driver still holds it. Reads no
    /// `record.json`, so it holds a record of any build's.
    pub fn released<T>(dir: &Path, read: impl FnOnce() -> T) -> Result<Option<T>, String> {
        let path = dir.join("driver.pid");
        let pid = File::open(&path).context(path.display())?;
        match pid.try_lock_shared() {
            Ok(()) => Ok(Some(read())),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(error).context(path.display()),
        }
    }

    /// Removes the record of run `id` in `runs`, which a reader then finds none of, unless its
    /// driver holds it, a reader holds it ([`Record::released`]) or a reap is under way; returns
    /// whether it removed it, which it can't once it is gone.
    pub fn remove(runs: &Path, id: &str) -> Result<bool, String> {
        let dir = runs.join(id);
        let path = dir.join("driver.pid");
        // Reaps take turns by the directory's lock.
        let Some(_driver) = locked(&path)? else {
            return Ok(false);
        };
        let Some(_reaps) = locked(&dir)? else {
            return Ok(false);
        };
        // Aside first, by a name that a reader of `runs` skips, as one in the making is.
        let aside = runs.join(format!(".{id}.removed"));
        match fs::remove_dir_all(&aside) {
            // Left by a removal cut short.
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context(aside.display()),
        }
        match fs::rename(&dir, &aside) {
            Ok(()) => {}
            // Removed by another that held the lock first.
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error).context(dir.display()),
        }
        fs::remove_dir_all(&aside).context(aside.display())?;
        Ok(true)
    }
}

/// The file at `path`, locked, unless another holds its lock or the file is gone.
fn locked(path: &Path) -> Result<Option<File>, String> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context(path.display()),
    };
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(error)) => Err(error).context(path.display()),
    }
}
