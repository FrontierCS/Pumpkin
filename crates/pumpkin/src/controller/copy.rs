//! The copy of an ended run's record from the controller into this machine's runs directory.
//!
//! The controller lists the files and directories of the record; the files that differ from this
//! machine's copy, or that it lacks, come in one archive, which the controller's `tar` streams
//! and this machine's extracts as it comes, into a directory beside the copy, from which
//! [`placed`] moves each into it.
//!
//! The connections it goes over, and how often one that ends is asked again, are the parent's
//! ([`super`]); the files of a live record that `ls` keeps are [`super::listing`]'s, which takes
//! its extraction of an archive from here.

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
    /// Copies the record of run `id` from the controller into this machine's default runs
    /// directory, saying so first, as a large record takes a while: asks for the record's list
    /// ([`listed_record`]), then for the files that differ from those here or that are missing,
    /// as `rsync`'s quick check finds them, by size and modification time ([`held`]), and has
    /// them placed ([`placed`]). Once a connection ends, it asks again, so only what is still
    /// missing comes, as [`pause`](Self::pause) allows. A file that the controller no longer holds
    /// stays here.
    // rsync's quick check:
    // https://github.com/RsyncProject/rsync/blob/6118037350efb253fdcc11139f1f58a2582d5fe7/generator.c#L666-L690
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
            // copy lasts, so that another copy of the run leaves it ([`cleared`]).
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

    /// What [`record_list`] prints of run `id`'s record, asked again, saying it does `what`, if
    /// the connection that went over ends.
    fn list_record(&self, id: &RunTag, what: &str) -> Result<Vec<u8>, String> {
        let script = record_list(id);
        let listing = || self.ssh(&script);
        self.again(listing, None, "ssh", &[SSH_FAILED], SHORT_TIMEOUT, what)
    }

    /// Extracts into `sent` below `scratch`, a new directory, the archive of the files of run
    /// `id`'s record that `wanted` names, which the controller streams as it reads them, and
    /// says what the call said, but the lines of the controller's `tar` that [`Archived::Unread`]
    /// gives.
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
        // call by itself (`STALL_TIMEOUT`).
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

/// What a call that ran `tar` on the controller said, `said`, but `tar`'s own lines, then those
/// lines, but the last, which only says that it failed.
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

/// How an archive of a record's files ended ([`Reached::archived`]).
enum Archived {
    /// At the end of what it was asked for.
    Whole,
    /// Cut part way, as the connection over this shared master ended.
    Cut(usize),
    /// Without the files that the controller's `tar` couldn't read, as this error says, with what
    /// `tar` said of each.
    Unread(String),
}

/// `tar` that extracts the archive on its standard input into `sent`, a new directory, 0700,
/// which this makes: each member below it, as `tar` takes none above it.
pub(super) fn untar(sent: &Path) -> Result<Command, String> {
    DirBuilder::new()
        .mode(0o700)
        .create(sent)
        .context(sent.display())?;
    let mut tar = stdx::process::command("tar", Environment::Only(&["PATH"]));
    tar.args(["--extract", "--file=-", "--directory"]).arg(sent);
    Ok(tar)
}

/// How many files and directories a run's record holds at most for `wait` to copy it.
// Several times what a run whose agent wrote many files holds; the list of a record is a line
// an entry, about 100 bytes, so 10 MB at most.
const RECORD_FILES: usize = 100_000;

/// How many random letters end the name of the directory that a copy of a record extracts into,
/// after `.<id>.`.
const SCRATCH_RANDOM: usize = 6;

/// A file of a run's record, as [`record_list`] lists it.
struct Listed {
    size_bytes: u64,
    /// Its modification time, in whole seconds since the Unix epoch.
    modified_seconds: i64,
    /// Its path below the record.
    name: PathBuf,
}

/// What a command on the controller runs to list the regular files and the directories below
/// run `id`'s record, each `<f or d> <size> <seconds>.<fraction> <path>`, ended by a NUL byte, as
/// its name may hold a newline; [`RECORD_FILES`] and one more at most, so that a longer list
/// shows itself.
fn record_list(id: &RunTag) -> String {
    format!(
        "cd \"$HOME/{}/{}\" && find . -mindepth 1 \\( -type f -o -type d \\) -printf '%y %s %T@ \
         %P\\0' | head --zero-terminated --lines={}",
        crate::job::runs(),
        id.as_str(),
        RECORD_FILES + 1
    )
}

/// The files and the directories that controller `name` listed of run `id`'s record
/// ([`record_list`]); refuses a list past [`RECORD_FILES`], and a line that names no path below
/// the record.
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

/// Whether `copy`, of a run's record, holds `file` as the controller listed it: a regular file
/// of its size and modification time.
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

/// Removes from `runs` each directory that a copy of run `id`'s record extracted into and left,
/// as it was killed, named as [`SCRATCH_RANDOM`] says, whose lock no copy holds.
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

/// Makes each of `dirs`, below `copy`, of a run's record.
fn made(copy: &Path, dirs: &[PathBuf]) -> Result<(), String> {
    for dir in dirs {
        let dir = copy.join(dir);
        fs::create_dir_all(&dir).context(dir.display())?;
    }
    Ok(())
}

/// Moves into `copy`, of run `id`'s record, each file below `sent` ([`Reached::archived`]) that
/// is whole, as of its size and modification time in `wanted`, and drops the rest; once every
/// other file of `wanted` is in place, makes each of `dirs`, then moves `record.json`, if wanted
/// and whole; returns how many files it moved.
/// Refuses, and moves nothing past, an entry that is neither a directory nor a regular file, or
/// a file that `wanted` doesn't name, as controller `name` was not asked for it.
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
/// mode alone ([`schema::sandbox::tree_mode`]).
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
