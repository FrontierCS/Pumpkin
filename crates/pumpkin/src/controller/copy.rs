//! Copying an ended run's record from the controller to this machine.
//!
//! The controller lists the files and directories of the record. The files that differ from this
//! machine's copy, or that it lacks, come in one archive, which the controller's `tar` streams
//! and this machine's extracts as it comes, into a directory beside the copy, from which `placed`
//! moves each into it.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, DirBuilder, File, TryLockError};
use std::io::{self, Write as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use provider::RunTag;
use stdx::Context as _;
use stdx::process::Environment;

use super::{AGAIN, Name, Reached, SHORT_TIMEOUT, SSH_FAILED};
use crate::say;

impl Reached {
    /// Copies run `id`'s record from the controller into this machine's runs directory, keeping the
    /// files that an earlier copy placed whole. A connection that ends meanwhile is opened again,
    /// and the copy goes on with the files it still lacks.
    ///
    /// A file that the controller no longer holds stays here. `record.json` is placed last, so
    /// that a copy that reads as a record holds every file of it.
    pub(super) fn copy(&self, id: &str) -> Result<(), String> {
        let id = RunTag::try_from(id.to_owned()).map_err(|_| {
            format!(
                "controller `{}` named `{}` as a run, which is no run's id, so nothing of it is \
                 copied: see what `~/{}` holds there",
                self.name,
                id.escape_debug(),
                crate::job::runs()
            )
        })?;
        let runs = PathBuf::from(crate::default_runs()?);
        let copy = runs.join(id.as_str());
        fs::create_dir_all(&runs).context(runs.display())?;
        say(format_args!(
            "copying run {}'s record from controller `{}` to {}",
            id.as_str(),
            self.name,
            copy.display()
        ));
        cleared(&runs, &id)?;
        let what = format!("copying run {}'s record again, carrying on", id.as_str());
        let mut attempt = 0;
        loop {
            let (files, dirs) = listed_record(&self.name, &id, &self.list_record(&id, &what)?)?;
            let mut wanted = BTreeMap::new();
            for file in files {
                if !held(&copy, &file)? {
                    wanted.insert(file.name.clone(), file);
                }
            }
            if wanted.is_empty() {
                return made(&copy, &dirs);
            }
            // Beside the copy, so that a file moves into it by a rename; its name starts with a
            // dot, which no run's id does, so no listing of the runs reads it. Locked while this
            // copy lasts, so that another copy of the run leaves it (`cleared`).
            let scratch = tempfile::Builder::new()
                .prefix(&format!(".{}.", id.as_str()))
                .rand_bytes(SCRATCH_RANDOM)
                .tempdir_in(&runs);
            let scratch = scratch.context(runs.display())?;
            let lock = File::open(scratch.path()).and_then(|dir| dir.lock().map(|()| dir));
            let _lock = lock.context(scratch.path().display())?;
            let sent = scratch.path().join("sent");
            let archived = self.archived(&id, &wanted, scratch.path())?;
            let placed_count = placed(&self.name, &id, (&sent, &copy), (&wanted, &dirs))?;
            let master = match archived {
                Archived::Whole if placed_count < wanted.len() => {
                    return Err(format!(
                        "run {}'s record changed on controller `{}` as it was copied: run the \
                         command again",
                        id.as_str(),
                        self.name
                    ));
                }
                Archived::Whole => return Ok(()),
                Archived::Unread(error) => return Err(error),
                Archived::Cut(master) => master,
            };
            if placed_count > 0 {
                attempt = 0;
            }
            if !self.pause(attempt, master, &what) {
                return Err(format!(
                    "a connection to controller `{}` ended at {} attempts of the copy in a row, \
                     the last as its log says: {}: run the command again",
                    self.name,
                    AGAIN.len() + 1,
                    self.shared.log(master).display()
                ));
            }
            attempt += 1;
        }
    }

    /// The listing of the record's files and directories, which `record_list` makes.
    fn list_record(&self, id: &RunTag, what: &str) -> Result<Vec<u8>, String> {
        let script = record_list(id);
        let listing = || self.ssh(&script);
        self.again(listing, None, "ssh", &[SSH_FAILED], SHORT_TIMEOUT, what)
    }

    fn archived(
        &self,
        id: &RunTag,
        wanted: &BTreeMap<PathBuf, Listed>,
        scratch: &Path,
    ) -> Result<Archived, String> {
        let names = wanted
            .keys()
            .map(|name| [name.as_os_str().as_bytes(), b"\0"].concat());
        let names = names.collect::<Vec<_>>().concat();
        let mut tar = untar(&scratch.join("sent"))?;
        // Each file's mode as the controller's, which this machine's umask would cut.
        tar.arg("--preserve-permissions");
        let tar = tar.stdin(Stdio::piped()).spawn();
        let mut tar = tar.context("running `tar` failed")?;
        let Some(into) = tar.stdin.take() else {
            unreachable!("the child's standard input is missing, though it is piped just above")
        };
        let script = format!(
            "cd \"$HOME/{}/{}\" && exec tar --create --null --verbatim-files-from \
             --no-recursion --files-from=- --file=-",
            crate::job::runs(),
            id.as_str()
        );
        // To a file, which no pipe's size bounds, as the call is only read once it ends.
        let said_path = scratch.join("said");
        let said = File::create_new(&said_path).context(said_path.display())?;
        let mut ssh = self.ssh(&script)?;
        let child = ssh.stdin(Stdio::piped()).stdout(into).stderr(said).spawn();
        let mut child = child.context("running `ssh` failed")?;
        let Some(mut stdin) = child.stdin.take() else {
            unreachable!("the child's standard input is missing, though it is piped just above")
        };
        // Then dropped, so that the controller's `tar` reads its end.
        let written = stdin.write_all(&names);
        drop(stdin);
        // With no deadline, as a record takes as long as its size sets; a dead connection ends the
        // call by itself, as `ServerAliveInterval` says.
        let status = child.wait().context("running `ssh` failed")?;
        let master = ssh.slot.master;
        // Which holds the other end of `tar`'s standard input, which `tar` reads to its end.
        drop(ssh);
        let extracted = tar.wait().context("running `tar` failed")?;
        let said = fs::read(&said_path).context(said_path.display())?;
        let (other, unread) = tar_said(&said);
        io::stderr()
            .write_all(&other)
            .context("writing to standard error failed")?;
        if self.ended_connection(status, &[SSH_FAILED], master) {
            return Ok(Archived::Cut(master));
        }
        if !status.success() && !unread.is_empty() {
            return Ok(Archived::Unread(format!(
                "controller `{}` sent run {}'s record without what its `tar` couldn't read ({status}): \
                 {}: the copy here holds what came whole, but not its record.json, so no command \
                 reads it yet: once the controller's user can read each file named (`chmod u+r` \
                 there, below `~/{}/{}`), run the command again",
                self.name,
                id.as_str(),
                unread.join("; "),
                crate::job::runs(),
                id.as_str()
            )));
        }
        if !status.success() {
            let failed = format!(
                "copying run {}'s record from controller `{}` failed ({status}), as said above",
                id.as_str(),
                self.name
            );
            return Err(self.lost(master, failed));
        }
        written.context("writing to `ssh` failed")?;
        if !extracted.success() {
            return Err(format!(
                "extracting run {}'s record, as controller `{}` sent it, failed ({extracted}), \
                 as said above",
                id.as_str(),
                self.name
            ));
        }
        Ok(Archived::Whole)
    }
}

/// Splits what `tar` on the controller wrote to its standard error into the lines that aren't
/// its own, and its complaints, without the line that only says it failed.
fn tar_said(said: &[u8]) -> (Vec<u8>, Vec<String>) {
    let (tar, other): (Vec<&[u8]>, Vec<&[u8]>) = said
        .split_inclusive(|byte| *byte == b'\n')
        .partition(|line| line.starts_with(b"tar: "));
    let tar = tar
        .iter()
        .filter(|line| !line.starts_with(b"tar: Exiting with failure status"))
        .map(|line| String::from_utf8_lossy(line.trim_ascii_end()).into_owned());
    (other.concat(), tar.collect())
}

/// How a transfer of an archive of the wanted files ended.
enum Archived {
    /// The archive arrived whole.
    Whole,
    /// The connection, which this is the number of, ended first.
    Cut(usize),
    /// The controller's `tar` couldn't read some files; the error says which.
    Unread(String),
}

/// Makes the directory `sent`, for the owner alone, and returns the `tar` that extracts its
/// standard input into it: each member below it, as `tar` takes none above it.
fn untar(sent: &Path) -> Result<Command, String> {
    DirBuilder::new()
        .mode(0o700)
        .create(sent)
        .context(sent.display())?;
    let mut tar = stdx::process::command("tar", Environment::Only(&["PATH"]));
    tar.args(["--extract", "--file=-", "--directory"]).arg(sent);
    Ok(tar)
}

// The most files and directories of one record that a copy takes, several times what a run whose
// agent wrote many files holds. The listing is a line an entry, about 100 bytes, so it stays
// within about 10 MB.
const RECORD_FILES: usize = 100_000;

// The length of the random part of a scratch directory's name, by which `cleared` tells the
// leftovers of copies from other files.
const SCRATCH_RANDOM: usize = 6;

/// A file of a record on the controller, with what tells whether the copy holds it whole.
struct Listed {
    size_bytes: u64,
    /// Its modification time, in whole seconds since the Unix epoch.
    modified_seconds: i64,
    /// Its path below the record.
    name: PathBuf,
}

/// The script that lists the regular files and directories below run `id`'s record, each as
/// `<f or d> <size> <seconds>.<fraction> <path>` ended by a NUL byte, as a name may hold a
/// newline: `RECORD_FILES` and one more at most, so that a longer list shows itself.
fn record_list(id: &RunTag) -> String {
    format!(
        "cd \"$HOME/{}/{}\" && find . -mindepth 1 \\( -type f -o -type d \\) -printf '%y %s %T@ \
         %P\\0' | head --zero-terminated --lines={}",
        crate::job::runs(),
        id.as_str(),
        RECORD_FILES + 1
    )
}

/// The files and directories of the record, from what `record_list`'s script printed. Fails if
/// there are more than `RECORD_FILES`, or a path isn't below the record.
fn listed_record(
    name: &Name,
    id: &RunTag,
    printed: &[u8],
) -> Result<(Vec<Listed>, Vec<PathBuf>), String> {
    let there = format!("~/{}/{}", crate::job::runs(), id.as_str());
    let lines = printed.split(|byte| *byte == b'\0');
    let lines: Vec<&[u8]> = lines.filter(|line| !line.is_empty()).collect();
    if lines.len() > RECORD_FILES {
        return Err(format!(
            "run {}'s record on controller `{name}` holds over {RECORD_FILES} files and \
             directories, more than a copy takes: in a shell on it (`aws ssm start-session`), \
             pack the directories of `{there}` that hold the most into an archive each (`tar \
             --create --remove-files`), then run the command again",
            id.as_str()
        ));
    }
    let (mut files, mut dirs) = (Vec::new(), Vec::new());
    for line in lines {
        let mut words = line.splitn(4, |byte| *byte == b' ');
        let (kind, size, modified, path) = (words.next(), words.next(), words.next(), words.next());
        let size_bytes = size.and_then(|size| str::from_utf8(size).ok()?.parse().ok());
        let modified_seconds = modified.and_then(|modified| {
            let modified = str::from_utf8(modified).ok()?;
            modified.split('.').next()?.parse().ok()
        });
        let path = path.map(|path| PathBuf::from(OsStr::from_bytes(path)));
        let below = path.as_ref().is_some_and(|path| {
            path.components().count() > 0
                && path
                    .components()
                    .all(|part| matches!(part, Component::Normal(_)))
        });
        match (kind, size_bytes, modified_seconds, path, below) {
            (Some(b"f"), Some(size_bytes), Some(modified_seconds), Some(name), true) => {
                files.push(Listed {
                    size_bytes,
                    modified_seconds,
                    name,
                });
            }
            (Some(b"d"), Some(_), Some(_), Some(name), true) => dirs.push(name),
            _ => {
                return Err(format!(
                    "controller `{name}` listed `{}` as a file or a directory of run {}'s \
                     record, which names none below it, so nothing of it is copied: see what \
                     `{there}` holds there",
                    line.escape_ascii(),
                    id.as_str()
                ));
            }
        }
    }
    Ok((files, dirs))
}

/// Whether `copy` holds `file` as it is on the controller: a regular file of its size and
/// modification time.
// As `rsync`'s quick check finds a file unchanged:
// https://github.com/RsyncProject/rsync/blob/6118037350efb253fdcc11139f1f58a2582d5fe7/generator.c#L666-L690
fn held(copy: &Path, file: &Listed) -> Result<bool, String> {
    let path = copy.join(&file.name);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => Ok(metadata.is_file()
            && metadata.len() == file.size_bytes
            && metadata.mtime() == file.modified_seconds),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context(path.display()),
    }
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

fn made(copy: &Path, dirs: &[PathBuf]) -> Result<(), String> {
    for dir in dirs {
        let dir = copy.join(dir);
        fs::create_dir_all(&dir).context(dir.display())?;
    }
    Ok(())
}

/// Moves the files of `wanted` that `sent` holds whole, by their size and modification time, into
/// `copy`, drops the rest, and returns how many it moved. `record.json` moves only when every
/// other file of `wanted` is in the copy, with the directories. Fails, moving nothing past it, if
/// `sent` holds a directory entry that isn't a directory or a regular file, or a file that
/// `wanted` doesn't name.
fn placed(
    name: &Name,
    id: &RunTag,
    (sent, copy): (&Path, &Path),
    (wanted, dirs): (&BTreeMap<PathBuf, Listed>, &[PathBuf]),
) -> Result<usize, String> {
    // Last, as a copy reads as a run's record, and one that has ended as whole, once it is there.
    let record = Path::new("record.json");
    let mut record_whole = false;
    let mut placed_count = 0;
    let mut below_dirs = vec![PathBuf::new()];
    while let Some(dir) = below_dirs.pop() {
        let entries = fs::read_dir(sent.join(&dir));
        for entry in entries.context(sent.join(&dir).display())? {
            let entry = entry.context(sent.join(&dir).display())?;
            let below = dir.join(entry.file_name());
            let file_type = entry.file_type().context(entry.path().display())?;
            if file_type.is_dir() {
                below_dirs.push(below);
                continue;
            }
            let file = wanted.get(&below).filter(|_| file_type.is_file());
            let Some(file) = file else {
                return Err(format!(
                    "controller `{name}` sent `{}` of run {}'s record, which it was not asked \
                     for as a file, so the copy keeps nothing of it: see what `~/{}/{}` holds \
                     there, and run the command again",
                    below.display(),
                    id.as_str(),
                    crate::job::runs(),
                    id.as_str()
                ));
            };
            if !held(sent, file)? {
                continue;
            }
            if below == record {
                record_whole = true;
                continue;
            }
            moved(sent, copy, &below)?;
            placed_count += 1;
        }
    }
    if placed_count + usize::from(wanted.contains_key(record)) < wanted.len() {
        return Ok(placed_count);
    }
    made(copy, dirs)?;
    if record_whole {
        moved(sent, copy, record)?;
        placed_count += 1;
    }
    Ok(placed_count)
}

/// Moves file `below` of `sent` to the same path below `copy`, with the permission bits of its
/// mode alone (`schema::sandbox::tree_mode`).
fn moved(sent: &Path, copy: &Path, below: &Path) -> Result<(), String> {
    let from = sent.join(below);
    let mode = fs::symlink_metadata(&from).context(from.display())?.mode();
    let kept = schema::sandbox::tree_mode(mode).bits();
    if kept != mode & 0o7777 {
        fs::set_permissions(&from, fs::Permissions::from_mode(kept)).context(from.display())?;
    }
    let to = copy.join(below);
    let parent = to.parent().unwrap_or(copy);
    fs::create_dir_all(parent).context(parent.display())?;
    fs::rename(&from, &to).context(to.display())
}
