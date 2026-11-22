//! Starts a job of the build that the start names, once for the id the client chose.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use members::Member;
use provider::RunTag;
use retention::KEPT_DAYS;
use schema::ir::Sha256Digest;
use stdx::process::Environment;
use stdx::{Context as _, Deadline};

use super::{JOBS, OWN, SENT_CONFIG, SENT_MODAL, TREES};
use crate::link::{self, JobStart, StartAnswer};
use crate::{manifest, workspace};

// The bound on asking systemd to start a job.
const START_TIMEOUT: Duration = Duration::from_mins(10);

// In the job's persistent directory once systemd has confirmed the start.
const STARTED: &str = "started";

/// Starts the job of `start`, with the credentials in `body`, as `member`'s.
pub(crate) fn start(
    home: &Path,
    start: JobStart,
    body: &[u8],
    member: &Member,
) -> Result<StartAnswer, String> {
    let id = RunTag::try_from(start.id.clone())?;
    let name = super::unit_name(&id);
    let id = id.as_str();
    let jobs = super::jobs()?;
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&jobs)
        .context(jobs.display())?;
    let lock = File::open(&jobs).context(jobs.display())?;
    lock.lock().context(jobs.display())?;
    let dir = home.join(JOBS).join(id);
    if dir.try_exists().context(dir.display())? {
        // The job itself makes OWN, including one whose start wasn't recorded.
        if dir.join(STARTED).is_file() || dir.join(OWN).is_symlink() {
            return Ok(StartAnswer::AlreadyStarted);
        }
        return Ok(unconfirmed(
            &name,
            &jobs.join(id),
            "a previous start reserved this job id",
        ));
    }
    if Path::new(&start.repo).file_name() != Some(start.repo.as_ref()) {
        return Err("a job's repository name must be a single directory name".to_owned());
    }
    if start.key_bytes.len() != start.sent.upstreams.key_files().len() {
        return Err("the job must send one key for each upstream in its configuration".to_owned());
    }
    manifest::store::required(&home.join(super::STORE), &start.tree)?;
    let pumpkin = installed(home, &start.install)?;
    let job = new_job(&home.join(JOBS), id, member)?;
    let sent = new_job_dir(&jobs, id, 0o700)?;
    let tree = new_job_dir(&home.join(TREES).join(&start.repo), id, 0o755)?;
    let workspace = home.join(workspace::DIR);
    symlink(&workspace, tree.path().join(workspace::DIR)).context(tree.path().display())?;
    write_sent(&start, sent.path(), body)?;
    let answer = launch(&pumpkin, &name, start, tree.path(), sent.path(), job.path())?;
    let job = job.keep();
    drop((sent.keep(), tree.keep()));
    if matches!(answer, StartAnswer::Started) {
        let receipt = job.join(STARTED);
        if let Err(error) = File::create(&receipt) {
            crate::say(format_args!(
                "{name} started but writing {} failed: {error}",
                receipt.display()
            ));
        }
    }
    Ok(answer)
}

/// Makes the job's directory, `id` in `jobs`, which names `member` and this build as soon as it is
/// there. It is made under a name that no listing reads, then renamed, so that no listing finds the
/// job without its member. It is removed on a failure before the unit takes it.
// The caller holds the jobs' lock, under which it found no `id` in `jobs`.
fn new_job(jobs: &Path, id: &str, member: &Member) -> Result<Made, String> {
    fs::create_dir_all(jobs).context(jobs.display())?;
    // A listing reads only names that start with a digit, as each id does.
    let staged = tempfile::Builder::new()
        .prefix(".")
        .permissions(fs::Permissions::from_mode(0o755))
        .tempdir_in(jobs)
        .context(jobs.display())?;
    super::write_member(staged.path(), member)?;
    super::release::write(staged.path())?;
    let dir = jobs.join(id);
    fs::rename(staged.path(), &dir).context(dir.display())?;
    drop(staged.keep());
    Ok(Made {
        path: dir,
        kept: false,
    })
}

/// A directory that the start made, which it removes when dropped, unless it is kept.
struct Made {
    path: PathBuf,
    kept: bool,
}

impl Made {
    fn path(&self) -> &Path {
        &self.path
    }

    fn keep(mut self) -> PathBuf {
        self.kept = true;
        self.path.clone()
    }
}

impl Drop for Made {
    fn drop(&mut self) {
        if !self.kept {
            // A failure here leaves a directory that a start of the same id reports as reserved.
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

// Each directory is made anew, and removed on a failure before the unit takes it.
fn new_job_dir(parent: &Path, id: &str, mode: u32) -> Result<tempfile::TempDir, String> {
    fs::create_dir_all(parent).context(parent.display())?;
    tempfile::Builder::new()
        .prefix(id)
        .rand_bytes(0)
        .permissions(fs::Permissions::from_mode(mode))
        .tempdir_in(parent)
        .context(parent.join(id).display())
}

const SYSTEMD: Environment = Environment::Only(&[
    "HOME",
    "PATH",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
]);

fn launch(
    pumpkin: &Path,
    name: &str,
    start: JobStart,
    tree: &Path,
    sent: &Path,
    job: &Path,
) -> Result<StartAnswer, String> {
    let mut unit = stdx::process::command("systemd-run", SYSTEMD);
    unit.args(["--user", "--quiet", "--expand-environment=no"])
        .arg(format!("--unit={name}"))
        .arg(format!("--working-directory={}", tree.display()))
        .arg("--property=ExitType=cgroup")
        .arg(format!(
            "--property=ExecStopPost=/usr/bin/rm --recursive --force {}",
            sent.display()
        ))
        .arg(format!(
            "--property=StandardOutput=append:{}/out",
            job.display()
        ))
        .arg(format!(
            "--property=StandardError=append:{}/log",
            job.display()
        ))
        .arg("--")
        .arg(pumpkin)
        .args([
            "controller",
            "job",
            &start.id,
            "--tree",
            start.tree.hex(),
            "--",
            "run",
        ])
        .args(start.words);
    let (problem, exited) = match stdx::process::output(&mut unit, Deadline::after(START_TIMEOUT)) {
        Ok(Some(o)) if o.status.success() => return Ok(StartAnswer::Started),
        Ok(Some(o)) => (
            format!(
                "systemd-run failed with {}: {}",
                o.status,
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            o.status.code().is_some(),
        ),
        Ok(None) => ("systemd-run timed out".to_owned(), false),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
            ) =>
        {
            return Err(format!(
                "systemd-run could not start: {error}: install it on PATH"
            ));
        }
        Err(error) => (format!("systemd-run failed: {error}"), false),
    };
    after_launch(name, sent, job, &problem, exited)
}

fn after_launch(
    name: &str,
    sent: &Path,
    job: &Path,
    problem: &str,
    exited: bool,
) -> Result<StartAnswer, String> {
    if job.join(OWN).is_symlink() {
        return Ok(StartAnswer::Started);
    }
    let absent = exited && {
        let mut show = stdx::process::command("systemctl", SYSTEMD);
        show.args(["--user", "show", "--property=LoadState", "--value", name]);
        let state = stdx::process::output(&mut show, Deadline::after(Duration::from_secs(10)));
        matches!(state, Ok(Some(state)) if state.stdout == b"not-found\n")
    };
    // A short unit may have run ExecStopPost while systemctl was looking it up.
    match sent.try_exists() {
        Ok(false) => Ok(StartAnswer::Started),
        Ok(true) if absent => Err(format!(
            "{problem}: fix what systemd-run reports and run the command again"
        )),
        _ => Ok(unconfirmed(name, sent, problem)),
    }
}

fn unconfirmed(name: &str, sent: &Path, problem: &str) -> StartAnswer {
    StartAnswer::Unconfirmed {
        message: format!(
            "{problem}; the start of {name} is unconfirmed: check `systemctl --user status {name}` and `journalctl --user -u {name}` on the controller; if it never ran, remove {} there and run the task again",
            sent.display()
        ),
    }
}

fn write_sent(start: &JobStart, sent: &Path, body: &[u8]) -> Result<(), String> {
    let config = toml::to_string(&start.sent)
        .map_err(|error| format!("writing the job's configuration: {error}"))?;
    let write = |name: &str, bytes: &[u8]| {
        let path = sent.join(name);
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .and_then(|mut file| file.write_all(bytes))
            .context(path.display())
    };
    write(SENT_CONFIG, config.as_bytes())?;
    let mut rest = body;
    for (name, len) in start.sent.upstreams.key_files().zip(&start.key_bytes) {
        let (key, left) = rest.split_at(*len);
        write(&name, key)?;
        rest = left;
    }
    if start.modal_bytes.is_some() {
        write(SENT_MODAL, rest)?;
    }
    Ok(())
}

/// The `pumpkin` of build `install`'s install in the workspace in `home`. Fails if the install is
/// missing. Marks the install as used now, and the prune of installs reads that mark.
fn installed(home: &Path, install: &Sha256Digest) -> Result<PathBuf, String> {
    let root = home.join(link::install(install));
    let pumpkin = root.join("bin").join(install::File::Pumpkin.name());
    // The lock of the jobs' directory is held, so no prune of installs removes the install before
    // the mark below.
    if !pumpkin.try_exists().context(pumpkin.display())? {
        return Err(format!(
            "{} is missing, as a prune removes an install that none used for {KEPT_DAYS} days: \
             run the command again to install it there",
            pumpkin.display()
        ));
    }
    let marked = File::open(&root).and_then(|root| root.set_modified(SystemTime::now()));
    marked.context(root.display())?;
    Ok(pumpkin)
}
