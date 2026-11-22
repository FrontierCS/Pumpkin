//! The end of an ended record. A reader that holds the record keeps it whole. Its removal takes it
//! whole once nothing holds it. So a reader of the runs directory finds all of a record or none of
//! it. Both lock the record's directory, which a reap holds whole while it writes. Both only test
//! the lock on its `driver.pid`, which stays its driver's alone.

use std::ffi::OsStr;
use std::fs::{self, File, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use stdx::Context as _;

use crate::Record;
use crate::record::REAPS;

// The suffix, after `.<id>`, of the name of a record's directory that its removal moved aside.
const REMOVED: &str = ".removed";

impl Record {
    /// Calls `read` while it holds the ended record in `dir`. It shares the lock on the record's
    /// directory, and waits for it while a reap or a removal holds it. [`Record::remove`] leaves
    /// the record be meanwhile. Returns `None`, and calls nothing, while the record's driver still
    /// holds it. Reads no `record.json`, so it holds a record of any build's.
    pub fn released<T>(dir: &Path, read: impl FnOnce() -> T) -> Result<Option<T>, String> {
        let locked = File::open(dir).context(dir.display())?;
        locked.lock_shared().context(dir.display())?;
        if !let_go(dir)? {
            return Ok(None);
        }
        Ok(Some(read()))
    }

    /// Removes the record in `dir`. Leaves it if its driver holds it, a reader holds it
    /// ([`Record::released`]) or a reap is under way. Also leaves it if its driver or a reap has
    /// written its end or a reap's line since `since`. Before the removal, calls `measure` on the
    /// record where no reader finds it. Returns what `measure` gave, or `None` if it left the
    /// record. A record that is gone gives `None` too.
    pub fn remove<T>(
        dir: &Path,
        since: SystemTime,
        measure: impl FnOnce(&Path) -> T,
    ) -> Result<Option<T>, String> {
        let locked = match File::open(dir) {
            Ok(locked) => locked,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context(dir.display()),
        };
        match locked.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Error(error)) => return Err(error).context(dir.display()),
        }
        if !let_go(dir)? || written_since(dir, since)? {
            return Ok(None);
        }
        // Aside first, under a name that a reader of the runs directory skips, as it skips a record
        // in the making.
        let aside = aside(dir)?;
        fs::rename(dir, &aside).context(dir.display())?;
        let measured = measure(&aside);
        // The lock moves with the directory. So `finish_removals` can't take this removal for one
        // cut short.
        fs::remove_dir_all(&aside).context(aside.display())?;
        Ok(Some(measured))
    }

    /// Finishes each removal of a record in `runs` that was cut short ([`Record::remove`]). No
    /// reader finds the directory of such a record. Leaves each removal that is under way. Returns
    /// why it couldn't finish each removal that it couldn't.
    pub fn finish_removals(runs: &Path) -> Vec<String> {
        let entries = match fs::read_dir(runs) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Vec::new(),
            Err(error) => return vec![format!("{}: {error}", runs.display())],
        };
        let mut failed = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let bytes = name.as_encoded_bytes();
            if !(bytes.starts_with(b".") && bytes.ends_with(REMOVED.as_bytes())) {
                continue;
            }
            let path = entry.path();
            let finished = File::open(&path).and_then(|locked| match locked.try_lock() {
                Ok(()) => fs::remove_dir_all(&path),
                Err(TryLockError::WouldBlock) => Ok(()),
                Err(TryLockError::Error(error)) => Err(error),
            });
            match finished {
                Err(error) if error.kind() != io::ErrorKind::NotFound => {
                    failed.push(format!("{}: {error}", path.display()));
                }
                _ => {}
            }
        }
        failed
    }
}

/// Whether the driver of the record in `dir` has let go of its lock, which this only tests.
fn let_go(dir: &Path) -> Result<bool, String> {
    let path = dir.join("driver.pid");
    let pid = File::open(&path).context(path.display())?;
    match pid.try_lock_shared() {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(error)) => Err(error).context(path.display()),
    }
}

/// Whether the end of the record in `dir`, or a line of its reaps, was written after `since`.
fn written_since(dir: &Path, since: SystemTime) -> Result<bool, String> {
    for name in ["outcome.json", REAPS] {
        let path = dir.join(name);
        match fs::metadata(&path).and_then(|metadata| metadata.modified()) {
            Ok(modified) if modified > since => return Ok(true),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context(path.display()),
        }
    }
    Ok(false)
}

/// Where the removal of the record in `dir` moves it: `.<id>.removed` beside it.
fn aside(dir: &Path) -> Result<PathBuf, String> {
    let (Some(runs), Some(id)) = (dir.parent(), dir.file_name()) else {
        return Err(format!(
            "`{}` names no directory: expected the directory of a run's record",
            dir.display()
        ));
    };
    let mut name = OsStr::new(".").to_owned();
    name.push(id);
    name.push(REMOVED);
    Ok(runs.join(name))
}
