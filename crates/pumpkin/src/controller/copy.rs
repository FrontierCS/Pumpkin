//! An ended run's files travel whole, one at a time. Each complete file moves into the copy by
//! rename. A cut loses only the file in progress, and the next request names what is now held.

use std::ffi::OsStr;
use std::fs::{self, File, TryLockError};
use std::io::{self, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use provider::RunTag;
use schema::ir::Mode;
use stdx::Context as _;
use tempfile::NamedTempFile;

use super::Reached;
use super::linking::Build;
use crate::link::copy::{CopiedRequest, CopyRequest, Item, Stamp, held};
use crate::link::{Ask, Link, MAX_REQUEST_BODY_BYTES, Unanswered};
use crate::{next, say};

// Match tempfile's default suffix so a copy recognizes scratch directories left by older copies.
// https://github.com/Stebalien/tempfile/blob/5c8fa12eb584931b4f1bccfde87eb72fbfa7dc61/src/lib.rs#L195
const SCRATCH_RANDOM: usize = 6;

impl Reached {
    pub(super) fn copy(&mut self, build: &Build, id: &str) -> Result<Option<String>, String> {
        let id = RunTag::try_from(id.to_owned())?;
        let runs = PathBuf::from(crate::default_runs()?);
        fs::create_dir_all(&runs).context(runs.display())?;
        let copy = directory(&runs, Path::new(id.as_str()))?;
        say(format_args!(
            "copying run {}'s record from controller `{}` to {}",
            id.as_str(),
            self.name,
            copy.display()
        ));
        let runs_lock = File::open(&runs).context(runs.display())?;
        runs_lock.lock().context(runs.display())?;
        cleared(&runs, &id)?;
        let scratch = tempfile::Builder::new()
            .prefix(&format!(".{}.", id.as_str()))
            .rand_bytes(SCRATCH_RANDOM)
            .tempdir_in(&runs)
            .context(runs.display())?;
        let lock = File::open(scratch.path()).context(scratch.path().display())?;
        lock.lock().context(scratch.path().display())?;
        drop(runs_lock);
        let what = format!("copying run {}'s record", id.as_str());
        let answer = self.carrying_on(build, &what, |reached, linked, _, progress| {
            let held = held(&copy, MAX_REQUEST_BODY_BYTES);
            let ask = Ask::Copy(CopyRequest {
                id: id.as_str().to_owned(),
                held_bytes: held.len(),
            });
            linked.stream(reached, |link| {
                link.send(&ask, &held)?;
                receive(link, &copy, scratch.path(), progress)
            })
        })?;
        let writer = answer.map_err(|message| {
            format!(
                "controller `{}` copying run {}: {message}",
                self.name,
                id.as_str()
            )
        })?;
        // Only once the copy is whole. The controller then removes its record in time.
        let what = format!("noting the copy of run {}'s record", id.as_str());
        let noted = self.asked(build, &what, |reached, linked| {
            let id = id.as_str().to_owned();
            linked.ask(reached, CopiedRequest { id })
        });
        if let Err(error) | Ok(Err(error)) = noted {
            say(format_args!(
                "controller `{}` didn't note the copy, so it keeps run {id}'s record until it \
                 notes one: {error}; to copy it again: {}",
                self.name,
                next::of_run("wait", None, id.as_str()),
                id = id.as_str()
            ));
        }
        Ok(writer)
    }
}

fn receive(
    link: &mut Link,
    copy: &Path,
    scratch: &Path,
    progress: &mut dyn FnMut(),
) -> Result<Result<Option<String>, String>, Unanswered> {
    let failed = |error| Unanswered::Failed(error);
    let mut record = None;
    loop {
        let (item, body) = match link.item::<Item>()? {
            Ok(item) => item,
            Err(message) => return Ok(Err(message)),
        };
        if !body.is_empty() || record.is_some() && !matches!(item, Item::Copied { .. }) {
            return Err(failed(
                "expected record.json last, then the end of the copy, and data only in file parts"
                    .to_owned(),
            ));
        }
        match item {
            Item::Directory { path } => {
                let path = relative(&path).map_err(failed)?;
                directory(copy, path).map_err(failed)?;
            }
            Item::File { path, stamp, mode } => {
                let path = relative(&path).map_err(failed)?;
                let file = NamedTempFile::new_in(scratch)
                    .context(scratch.display())
                    .map_err(failed)?;
                let file = match contents(link, file, stamp, mode)? {
                    Ok(file) => file,
                    Err(message) => return Ok(Err(message)),
                };
                if path == Path::new("record.json") {
                    record = Some(file);
                } else if place(file, copy, path).map_err(failed)? != Some(stamp) {
                    progress();
                }
            }
            Item::Data => {
                return Err(failed(
                    "file data arrived before its name and size".to_owned(),
                ));
            }
            Item::Copied {
                unread: Some(message),
                ..
            } => {
                return Ok(Err(format!(
                    "{message}: the copy keeps the files that came whole. Once the controller can read the record, copy it again"
                )));
            }
            Item::Copied {
                unread: None,
                writer,
            } => {
                if let Some(file) = record {
                    place(file, copy, Path::new("record.json")).map_err(failed)?;
                } else if !fs::symlink_metadata(copy.join("record.json")).is_ok_and(|m| m.is_file())
                {
                    return Err(failed("the copy ended without its record.json".to_owned()));
                }
                return Ok(Ok(writer));
            }
        }
    }
}

fn contents(
    link: &mut Link,
    mut file: NamedTempFile,
    stamp: Stamp,
    mode: Mode,
) -> Result<Result<NamedTempFile, String>, Unanswered> {
    let mut left = stamp.size_bytes;
    while left > 0 {
        match link.item::<Item>()? {
            Ok((Item::Data, body)) if !body.is_empty() && body.len() as u64 <= left => {
                file.write_all(&body)
                    .context(file.path().display())
                    .map_err(Unanswered::Failed)?;
                left -= body.len() as u64;
            }
            Ok(_) => {
                return Err(Unanswered::Failed(format!(
                    "expected 1 to {left} more bytes of the file being copied"
                )));
            }
            Err(message) => return Ok(Err(message)),
        }
    }
    let delta = Duration::from_secs(stamp.modified_seconds.unsigned_abs());
    let at = if stamp.modified_seconds >= 0 {
        UNIX_EPOCH.checked_add(delta)
    } else {
        UNIX_EPOCH.checked_sub(delta)
    };
    let at = at.ok_or_else(|| {
        Unanswered::Failed("the file's modification time is out of range".to_owned())
    })?;
    file.as_file()
        .set_modified(at)
        .and_then(|()| {
            file.as_file()
                .set_permissions(fs::Permissions::from_mode(mode.bits()))
        })
        .context(file.path().display())
        .map_err(Unanswered::Failed)?;
    Ok(Ok(file))
}

fn relative(name: &OsStr) -> Result<&Path, String> {
    let path = Path::new(name);
    if name.is_empty()
        || !path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        return Err(format!(
            "`{}` is not a path below the run's record",
            name.display()
        ));
    }
    Ok(path)
}

/// Creates the directories below `root`, refusing a link in their place.
fn directory(root: &Path, below: &Path) -> Result<PathBuf, String> {
    let mut path = root.to_owned();
    for part in below {
        path.push(part);
        match fs::create_dir(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if !fs::symlink_metadata(&path)
                    .context(path.display())?
                    .is_dir()
                {
                    return Err(format!(
                        "{} is not a directory: expected a directory inside the copy",
                        path.display()
                    ));
                }
            }
            Err(error) => return Err(error).context(path.display()),
        }
    }
    Ok(path)
}

fn place(file: NamedTempFile, copy: &Path, below: &Path) -> Result<Option<Stamp>, String> {
    let parent = below.parent().unwrap_or(Path::new(""));
    directory(copy, parent)?;
    let to = copy.join(below);
    let previous = fs::symlink_metadata(&to)
        .ok()
        .filter(fs::Metadata::is_file)
        .as_ref()
        .map(Stamp::of);
    file.persist(&to).context(to.display())?;
    Ok(previous)
}

/// Removes the scratch directories that earlier copies of run `id` left in `runs`, but not one
/// that a copy running now holds.
fn cleared(runs: &Path, id: &RunTag) -> Result<(), String> {
    let prefix = format!(".{}.", id.as_str());
    for entry in fs::read_dir(runs).context(runs.display())? {
        let entry = entry.context(runs.display())?;
        let name = entry.file_name();
        let random = name.to_str().and_then(|name| name.strip_prefix(&prefix));
        let left = random.is_some_and(|random| {
            random.len() == SCRATCH_RANDOM
                && random.bytes().all(|byte| byte.is_ascii_alphanumeric())
        });
        if !left || !entry.file_type().context(entry.path().display())?.is_dir() {
            continue;
        }
        let dir = File::open(entry.path()).context(entry.path().display())?;
        match dir.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => continue,
            Err(TryLockError::Error(error)) => return Err(error).context(entry.path().display()),
        }
        fs::remove_dir_all(entry.path()).context(entry.path().display())?;
    }
    Ok(())
}
