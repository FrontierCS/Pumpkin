//! The controller target: with `[controller]` in its configuration ([`config::Client`]), this
//! machine sends `run` and `build` to the person's controller, an EC2 instance that the module in
//! `deploy/aws/` makes, and follows them, lists, stops and reaps its runs, there, with the same
//! commands as it would run them itself.
//!
//! Each command reaches the controller ([`Reached::reach`]) over the connections that the
//! workspace's commands share to it ([`Shared`]), master `ssh`es to the controller's user through
//! SSM's `AWS-StartSSHSession`, each `ssh` ([`Reached::ssh`]) and `rsync` over one with a session
//! free. With no master to share, it finds the controller by its tag, starts it if it is stopped,
//! waits until SSM sees it, and opens one, with this machine's key ([`kept`]), which it enrols
//! through EC2 Instance Connect once the controller refuses it. A follow of a run, a send and a
//! copy whose connection ends go on over another, a few times ([`AGAIN`]). What runs there is the
//! controller's own side ([`crate::job`]), whose layout below the user's home its module doc
//! gives: a run or a build sends the files of its task's repository that the task reaches into a
//! tree of its own, with the upstreams of this machine's configuration, their keys, its Modal
//! provider and Modal's configuration file into its job's directory in tmpfs, by `tar` on the
//! connection's standard input, never in an argument nor in a file of this machine's ([`archive`]);
//! then starts its job, `pumpkin controller job`, in a unit of the user's systemd, which outlives
//! every connection. What the controller's commands say is forwarded as they say it, and the
//! record of a run that has ended is copied into this machine's runs directory.

use std::fs::{self, DirBuilder, File, TryLockError};
use std::io::{self, Read as _, Write as _};
use std::ops::{Deref, DerefMut};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{
    DirBuilderExt as _, FileExt as _, OpenOptionsExt as _, PermissionsExt as _,
};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant, SystemTime};
use std::{env, fmt, thread};

use aws::{Bound, Cli};
use provider::RunTag;
use serde::Deserialize;
use stdx::process::Environment;
use stdx::{Context as _, Deadline};
use tempfile::TempDir;

use crate::job::{BUILDS, JOBS, SENT_CONFIG, SENT_MODAL, TREES, WAITING};
use crate::workspace::{self, Workspace};
use crate::{cli, config, next, say};

/// The controller's user, which runs every Pumpkin process there.
const USER: &str = "pumpkin";

/// The tag by which the module names the controller of a name.
const TAG: &str = "pumpkin:controller";

/// How long a controller may take to start, and SSM to see it, before the command gives up.
// An instance that EC2 starts runs in about a minute, and its SSM agent registers in seconds
// more; past 10 minutes something else is wrong.
const START_TIMEOUT: Duration = Duration::from_mins(10);

/// How often a start is looked at.
const START_POLL: Duration = Duration::from_secs(5);

/// How long a send, or a copy, by `rsync` may move no byte before `rsync` gives up.
// There is no bound on a send's whole time: SSM carries well under 1 MB a second, so a task's
// datasets of a GB or more take as long as they take. A dead connection ends by itself, as
// `ServerAliveInterval` gives `ssh` 90 s, so this is a little beyond that, to ride out an SSM
// session that pauses.
const STALL_TIMEOUT: Duration = Duration::from_mins(2);

/// The deadline of a command that has no bound on its time, only on how long it may stall.
const UNBOUNDED: Duration = Duration::MAX;

/// How long a short command on the controller, such as the listing of its runs, may take.
const SHORT_TIMEOUT: Duration = Duration::from_mins(10);

/// How long a master of the [`Shared`] connections lasts with no command going over it.
// Long enough for a batch of commands that start at once, and each command's calls one after
// another, to share it; short enough that none lingers long once they are done.
const PERSIST: Duration = Duration::from_mins(1);

/// How many masters the workspace's commands open to a controller at most.
// So that a batch can't open SSM sessions without end: 16 masters of `SESSIONS` each take 80
// commands at once, a run of every task at once; a command past that waits.
const MASTERS: usize = 16;

/// How many commands go over one master at once: half the sessions that `sshd` takes on one
/// connection by default (`MaxSessions 10`).
// `sshd` frees a session that has closed only once it has handled all it has read, so a session
// that one command closes and one that the next opens, in what it reads at once, both count for
// a moment: 5 at a time each take 2 at most, which 10 still takes.
const SESSIONS: usize = 5;

/// How long a call whose connection ended waits before each attempt it makes again, over a
/// session of a master that answers, and so how many it makes ([`Reached::pause`]).
// The first at once, as one master's end leaves the controller reachable over another; then
// long enough in all, about 4 minutes, for SSM to carry a session again after a pause of its own,
// and short enough that a connection that keeps ending ends the command soon after.
const AGAIN: [Duration; 6] = [
    Duration::ZERO,
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(30),
    Duration::from_mins(1),
    Duration::from_mins(2),
];

/// The exit status of `ssh`'s own failure, by which it says that its connection ended.
const SSH_FAILED: i32 = 255;

/// The exit statuses by which `rsync` says that its connection ended: an error of its socket's
/// input or output, or of its protocol's stream, and its `ssh`'s own failure, which it passes on.
const RSYNC_ENDED: [i32; 3] = [10, 12, SSH_FAILED];

/// How often a command that finds every master's sessions held looks for one again.
const SESSION_POLL: Duration = Duration::from_millis(500);

/// How long a listing of the controller's runs may take to arrive before `ls` says when it was
/// made: a person reads the rows of a listing as they are now.
const STALE: Duration = Duration::from_secs(10);

/// The exit status of a command on the controller that finds no job of the id it was given.
const NO_JOB: i32 = 100;

/// The exit status of a command on the controller that finds a job that ended before its run
/// started, or its build printed an IR.
const UNSTARTED: i32 = 101;

/// The name of a person's controller, as the module's `name` variable takes it: 1 to 16 of
/// `a-z`, `0-9` and `-`, starting with a letter and ending with no `-`.
#[derive(Clone, Debug, Deserialize)]
#[serde(try_from = "String")]
pub(crate) struct Name(String);

impl TryFrom<String> for Name {
    type Error = String;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        let bytes = name.as_bytes();
        let allowed =
            |byte: &u8| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-';
        let fits = (1..=16).contains(&bytes.len())
            && bytes.first().is_some_and(u8::is_ascii_lowercase)
            && bytes.last() != Some(&b'-')
            && bytes.iter().all(allowed);
        if !fits {
            return Err(format!(
                "`{}` is not a controller's name: give the `name` that `deploy/aws` was applied \
                 with, 1 to 16 of a-z, 0-9 and `-`, starting with a letter and ending with no `-`",
                name.escape_debug()
            ));
        }
        Ok(Self(name))
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// What `aws ec2 describe-instances` prints of the instances it finds.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Reservations {
    reservations: Vec<Reservation>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Reservation {
    instances: Vec<Instance>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Instance {
    instance_id: String,
    state: State,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct State {
    name: String,
}

/// What `aws ssm describe-instance-information` prints of the instances SSM sees.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Information {
    instance_information_list: Vec<Seen>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Seen {
    ping_status: String,
}

/// A directory of this command's own, 0700, in the workspace's `tmp/` ([`Workspace::tmp`]),
/// which dropping removes: for the records that `ls` reads.
fn scratch() -> Result<TempDir, String> {
    let tmp = Workspace::find()?.tmp();
    fs::create_dir_all(&tmp).context(tmp.display())?;
    let made = tempfile::Builder::new().prefix("pumpkin-").tempdir_in(&tmp);
    made.context(tmp.display())
}

/// The directory of this machine's key to its controllers, `$HOME/.config/pumpkin/ssh/`: the
/// key, `key` and `key.pub`, which this makes if it has none, and `known_hosts`, which pins the
/// host key that each controller showed first, by its [`Reached::alias`].
// A credential, so beside the configuration, outside every workspace; one for the machine, so
// that each controller enrols it once, whatever workspace reaches it.
fn kept() -> Result<PathBuf, String> {
    let home = env::var_os("HOME").filter(|home| !home.is_empty()).ok_or(
        "HOME is unset or empty: set it, as this machine's key to its controllers is below it",
    )?;
    let config = Path::new(&home).join(".config/pumpkin");
    let kept = config.join("ssh");
    if kept.join("key.pub").exists() {
        return Ok(kept);
    }
    let made = DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&config);
    made.context(config.display())?;
    // Made apart and moved in whole, so that of the commands that start at once on a new
    // machine, the first to move its key in is the one all of them use; the others' go.
    let mut made = tempfile::Builder::new();
    let made = made
        .prefix(".ssh-")
        .permissions(fs::Permissions::from_mode(0o700));
    let made = made.tempdir_in(&config);
    let made = made.context(config.display())?;
    let mut keygen = stdx::process::command("ssh-keygen", Environment::Only(&["PATH"]));
    keygen.args(["-q", "-t", "ed25519", "-N", "", "-C", "pumpkin", "-f"]);
    bounded(
        keygen.arg(made.path().join("key")),
        "ssh-keygen",
        SHORT_TIMEOUT,
    )?;
    if let Err(error) = fs::rename(made.path(), &kept)
        && !kept.join("key.pub").exists()
    {
        return Err(error).context(kept.display());
    }
    Ok(kept)
}

/// The connections that the workspace's commands share to a controller, in a directory of its own
/// in the workspace's `controller/` ([`Workspace::controller`]), `<name>.<region>/`: up to
/// [`MASTERS`] master `ssh`es, each by its socket there, named by its index, `0` and on, and its
/// [`SESSIONS`] sessions, each an `flock` on a file beside it, `<index>-<session>` ([`Slot`]),
/// which a command holds while its `ssh` or `rsync` goes over the master, and its log,
/// `<index>.log`, which says why it ended, kept until the master opens again; the lock on opening
/// one, `lock` ([`Lock`]), so that a batch of commands that start at once open them one at a time;
/// the lock on sending each task's tree, `send-<repository>-<entry>` ([`Shared::sending`]); and
/// what `ls` was sent of each record that ended there, `ended/<id>/` ([`Shared::ended`]).
// Each `ssh` runs in that directory, and names a socket by its name alone: so its path, with the
// suffix that `ssh` adds while it binds it, fits a socket's address however deep the workspace
// is.
struct Shared {
    dir: PathBuf,
}

/// The lock on opening a [`Shared`] master, an exclusive `flock`, held until it is dropped. It
/// holds the instance that the masters reach, which an opener writes before it opens one, so
/// that while a master answers, it names that master's instance.
struct Lock {
    file: File,
    path: PathBuf,
}

/// A session of a [`Shared`] master, held until it is dropped.
struct Slot {
    /// The master's index.
    master: usize,
    _file: File,
}

/// What [`Shared::scan`] finds: a free session on a master that answers, else the first master
/// that doesn't, to open, else that every session is held.
enum Found {
    Free(Slot),
    Closed(usize),
    Held,
}

impl Shared {
    /// The connections to the controller of `alias`, `<name>.<region>`, whose directory, which
    /// only this user may enter, this makes if need be.
    fn new(workspace: &Workspace, alias: &str) -> Result<Self, String> {
        let dir = workspace.controller().join(alias);
        let made = DirBuilder::new().mode(0o700).recursive(true).create(&dir);
        made.context(dir.display())?;
        Ok(Self { dir })
    }

    /// Takes the lock on opening a master, waiting while another command holds it.
    fn lock(&self) -> Result<Lock, String> {
        let path = self.dir.join("lock");
        let file = private(&path)?;
        file.lock().context(path.display())?;
        Ok(Lock { file, path })
    }

    /// The directory of the files that `ls` reads of each record that ended on the controller,
    /// which never change, so that the controller sends them once.
    fn ended(&self) -> PathBuf {
        self.dir.join("ended")
    }

    /// The log of `master`, as `ssh` and its proxy write it.
    fn log(&self, master: usize) -> PathBuf {
        self.dir.join(format!("{master}.log"))
    }

    /// Takes the lock on sending `tree`, of its repository and its task's entry, which the job's
    /// start then links as the repository's latest, so that commands of one task that start at
    /// once send its files once, and the others link to what the first sent, while each task's
    /// own files go at once with every other's; says, once, that it waits while another holds it.
    fn sending(&self, name: &Name, tree: &Tree) -> Result<File, String> {
        let path = (self.dir).join(format!("send-{}-{}", tree.repo, plain(&tree.entry)));
        let file = private(&path)?;
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(error)) => return Err(error).context(path.display()),
        }
        say(format_args!(
            "waiting for another command's send of `{}` to controller `{name}`, which this one \
             links to",
            tree.entry
        ));
        file.lock().context(path.display())?;
        Ok(file)
    }

    /// How many sends of a task's tree from this workspace are under way: the locks on sending
    /// ([`sending`](Self::sending)) that a command holds.
    // A look takes the lock for an instant, in which a send that starts would say that it waits
    // for another, then go on as the look lets go.
    fn sends(&self) -> Result<usize, String> {
        let mut held = 0;
        for entry in fs::read_dir(&self.dir).context(self.dir.display())? {
            let path = entry.context(self.dir.display())?.path();
            let named = path.file_name().and_then(|name| name.to_str());
            if !named.is_some_and(|name| name.starts_with("send-")) {
                continue;
            }
            let file = File::open(&path).context(path.display())?;
            match file.try_lock_shared() {
                Ok(()) => {}
                Err(TryLockError::WouldBlock) => held += 1,
                Err(TryLockError::Error(error)) => return Err(error).context(path.display()),
            }
        }
        Ok(held)
    }

    /// `program`, as [`Cli::proxied`] makes it, run in the connections' directory.
    fn command(&self, program: &str) -> Command {
        let mut command = Cli::proxied(program);
        command.current_dir(&self.dir);
        command
    }

    /// Whether `master` answers on its socket, as `ssh -O check` of `host` finds it.
    fn answers(&self, master: usize, host: &str) -> bool {
        if !self.dir.join(master.to_string()).exists() {
            return false;
        }
        let mut check = self.command("ssh");
        let control = format!("ControlPath={master}");
        check.args(["-F", "/dev/null", "-o", &control, "-O", "check", host]);
        bounded(&mut check, "ssh -O check", SHORT_TIMEOUT).is_ok()
    }

    /// Whether any master answers, as [`answers`](Self::answers) finds it.
    fn any_answers(&self, host: &str) -> bool {
        (0..MASTERS).any(|master| self.answers(master, host))
    }

    /// Looks at each master in turn, of `host`: one whose sessions are all held is in use, and so
    /// open; of one with a session free, it takes that session, if the master answers.
    fn scan(&self, host: &str) -> Result<Found, String> {
        let mut closed = None;
        for master in 0..MASTERS {
            if !self.dir.join(master.to_string()).exists() {
                closed = closed.or(Some(master));
                continue;
            }
            let Some(slot) = self.free(master)? else {
                continue;
            };
            if self.answers(master, host) {
                return Ok(Found::Free(slot));
            }
            closed = closed.or(Some(master));
        }
        Ok(closed.map_or(Found::Held, Found::Closed))
    }

    /// A session of `master` that no other command holds, if any.
    fn free(&self, master: usize) -> Result<Option<Slot>, String> {
        for session in 0..SESSIONS {
            let path = self.dir.join(format!("{master}-{session}"));
            let file = private(&path)?;
            match file.try_lock() {
                Ok(()) => {
                    return Ok(Some(Slot {
                        master,
                        _file: file,
                    }));
                }
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Error(error)) => return Err(error).context(path.display()),
            }
        }
        Ok(None)
    }
}

/// The file `path`, made if need be, which only this user may read.
fn private(path: &Path) -> Result<File, String> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path);
    file.context(path.display())
}

impl Lock {
    /// The instance that the last master opened reaches, as [`note`](Self::note) wrote it; none
    /// before the first.
    fn instance(&self) -> Result<Option<String>, String> {
        let mut text = Vec::new();
        (&self.file)
            .read_to_end(&mut text)
            .context(self.path.display())?;
        let text = String::from_utf8_lossy(&text);
        Ok(Some(text.trim())
            .filter(|id| !id.is_empty())
            .map(str::to_owned))
    }

    /// Notes `instance` as the one that the master about to be opened reaches.
    fn note(&self, instance: &str) -> Result<(), String> {
        let noted = self.file.set_len(0);
        let noted = noted.and_then(|()| self.file.write_all_at(instance.as_bytes(), 0));
        noted.context(self.path.display())
    }
}

/// An `ssh` or `rsync` to the controller, as [`Command`], with the session of the master it goes
/// over, which it holds until it is dropped.
struct Session {
    command: Command,
    slot: Slot,
}

impl Session {
    /// What the command gives, with `input`, if any, on its standard input, killed `by`.
    fn output(&mut self, input: Option<&[u8]>, by: Deadline) -> io::Result<Option<Output>> {
        match input {
            Some(input) => stdx::process::fed(&mut self.command, input, by),
            None => stdx::process::output(&mut self.command, by),
        }
    }
}

impl Deref for Session {
    type Target = Command;

    fn deref(&self) -> &Command {
        &self.command
    }
}

impl DerefMut for Session {
    fn deref_mut(&mut self) -> &mut Command {
        &mut self.command
    }
}

/// How a command on the controller that [`Reached::attached`] ran ended, and the shared master
/// that it went over.
#[derive(Clone, Copy)]
struct Attached {
    status: ExitStatus,
    master: usize,
}

/// What starting a shared master came to ([`Reached::master`]), but a failure.
enum Master {
    Open,
    /// The controller refused this machine's key, which it hasn't enrolled, as this error says.
    Unenrolled(String),
    /// The controller showed a host key other than the one pinned for it.
    HostKeyChanged,
}

/// The controller, reached: running, seen by SSM, over the connection that the workspace's
/// commands share to it, with this machine's key, for opening it again.
pub(crate) struct Reached {
    cli: Cli,
    name: Name,
    /// The name by which `known_hosts` pins its host key, `<name>.<region>`.
    alias: String,
    instance: String,
    kept: PathBuf,
    shared: Shared,
}

impl Reached {
    /// Reaches the controller that `settings` name, with this machine's key, made if need be:
    /// over the workspace's connections to it if a master answers there; else finds it by its
    /// tag, starts it if it is stopped, waits until SSM sees it, and opens a master
    /// ([`open`](Self::open)), holding the lock on opening one throughout.
    ///
    /// # Errors
    ///
    /// Fails if the Session Manager plugin isn't on `PATH`, `aws` fails, the tag names no one
    /// instance, SSM sees none by [`START_TIMEOUT`], or the connection or key can't be made.
    pub(crate) fn reach(settings: &config::Controller) -> Result<Self, String> {
        let config::Controller {
            name,
            region,
            modal: _,
        } = settings;
        plugin()?;
        let cli = Cli::connect(region.clone())?;
        let kept = kept()?;
        let alias = format!("{name}.{}", region.as_str());
        let shared = Shared::new(&Workspace::find()?, &alias)?;
        let lock = shared.lock()?;
        // A master that answers reaches a controller that runs, so it needs no finding.
        if let Some(instance) = lock.instance()?
            && shared.any_answers(&format!("{USER}@{instance}"))
        {
            return Ok(Self {
                cli,
                name: name.clone(),
                alias,
                instance,
                kept,
                shared,
            });
        }
        let tag = format!("Name=tag:{TAG},Values={name}");
        let live = |instance: &Instance| {
            !matches!(instance.state.name.as_str(), "shutting-down" | "terminated")
        };
        let mut instances = instances(&cli, &tag)?.into_iter().filter(live);
        let (Some(instance), None) = (instances.next(), instances.next()) else {
            return Err(format!(
                "no one instance in {} carries the tag {TAG}={name}: name the controller that \
                 `deploy/aws` made, with the `name` and `region` it was applied with",
                region.as_str()
            ));
        };
        let id = instance.instance_id;
        ready(&cli, name, &id)?;
        let reached = Self {
            cli,
            name: name.clone(),
            alias,
            instance: id,
            kept,
            shared,
        };
        // None answers, so the first is closed.
        reached.open(&lock, 0)?;
        Ok(reached)
    }
}

/// The instances that `filter`, of `aws ec2 describe-instances`, finds.
fn instances(cli: &Cli, filter: &str) -> Result<Vec<Instance>, String> {
    let args = ["ec2", "describe-instances", "--filters", filter];
    let found: Reservations = cli.json(&args, Bound::Call)?;
    Ok(found
        .reservations
        .into_iter()
        .flat_map(|found| found.instances)
        .collect())
}

/// Waits until controller `name`, instance `id`, runs, and SSM sees it, starting it first if it
/// is stopped, for [`START_TIMEOUT`] at most.
fn ready(cli: &Cli, name: &Name, id: &str) -> Result<(), String> {
    let by = Deadline::after(START_TIMEOUT);
    let mut started = false;
    loop {
        let found = instances(cli, &format!("Name=instance-id,Values={id}"))?;
        let state = found.into_iter().next().map(|instance| instance.state.name);
        let state = state.unwrap_or_default();
        match state.as_str() {
            "running" if seen(cli, id)? => return Ok(()),
            "stopped" if !started => {
                say(format_args!(
                    "starting controller `{name}`, {id}, which is stopped"
                ));
                let start = ["ec2", "start-instances", "--instance-ids", id];
                cli.run(&start, Bound::Call)?;
                started = true;
                continue;
            }
            // On its way to running, or to being stopped, which a start then follows.
            "pending" | "running" | "stopping" | "stopped" => {}
            _ => {
                return Err(format!(
                    "controller `{name}`, {id}, is {state}: start it again once it can be"
                ));
            }
        }
        if by.remaining().is_none() {
            return Err(format!(
                "controller `{name}`, {id}, is {state} after {} s: expected it to run, and SSM to \
                 see it, by then",
                START_TIMEOUT.as_secs()
            ));
        }
        thread::sleep(START_POLL);
    }
}

/// Whether SSM sees the instance `id` online.
fn seen(cli: &Cli, id: &str) -> Result<bool, String> {
    let filter = format!("Key=InstanceIds,Values={id}");
    let args = ["ssm", "describe-instance-information", "--filters", &filter];
    let information: Information = cli.json(&args, Bound::Call)?;
    Ok(information
        .instance_information_list
        .iter()
        .any(|seen| seen.ping_status == "Online"))
}

/// Checks that the Session Manager plugin, which `aws ssm start-session` runs, is on `PATH`.
fn plugin() -> Result<(), String> {
    let path = env::var_os("PATH").unwrap_or_default();
    if env::split_paths(&path).any(|dir| dir.join("session-manager-plugin").is_file()) {
        return Ok(());
    }
    Err(
        "the Session Manager plugin, `session-manager-plugin`, isn't on PATH, and `aws ssm \
         start-session` needs it to reach the controller: install it, as AWS's Session Manager \
         docs say"
            .to_owned(),
    )
}

/// Runs `command`, shown as `shown`, killing it after `timeout`, and returns its standard
/// output.
pub(crate) fn bounded(
    command: &mut Command,
    shown: &str,
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    finished(
        stdx::process::output(command, Deadline::after(timeout)),
        shown,
        timeout,
    )
}

/// What [`bounded`] returns of `output`.
fn finished(
    output: io::Result<Option<Output>>,
    shown: &str,
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    match output {
        Ok(Some(output)) if output.status.success() => Ok(output.stdout),
        Ok(Some(output)) => Err(format!(
            "`{shown}` failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )),
        Ok(None) => Err(format!(
            "`{shown}` was still running after {} s, so it was killed",
            timeout.as_secs()
        )),
        Err(error) => Err(format!("`{shown}` didn't run: {error}: install it on PATH")),
    }
}

impl Reached {
    /// A session of a master that answers on the shared connections, for one `ssh` or `rsync`:
    /// one free on a master that is open; else, with the lock on opening one, one on the first
    /// master that is closed, which this opens; else, once every master's sessions are held, the
    /// first to be let go, which it says it waits for.
    fn slot(&self) -> Result<Slot, String> {
        let host = self.host();
        let mut waiting = None;
        loop {
            if let Found::Free(slot) = self.shared.scan(&host)? {
                return Ok(self.waited(slot, waiting));
            }
            let lock = self.shared.lock()?;
            match self.shared.scan(&host)? {
                Found::Free(slot) => return Ok(self.waited(slot, waiting)),
                Found::Closed(master) => {
                    self.open(&lock, master)?;
                    // Held by commands over a master that has ended, which end with it.
                    if let Some(slot) = self.shared.free(master)? {
                        return Ok(self.waited(slot, waiting));
                    }
                }
                Found::Held => {
                    drop(lock);
                    if waiting.is_none() {
                        say(format_args!(
                            "waiting for a session of controller `{}`: all {SESSIONS} of each of \
                             its {MASTERS} connections are held",
                            self.name
                        ));
                        waiting = Some(Instant::now());
                    }
                    thread::sleep(SESSION_POLL);
                }
            }
        }
    }

    /// `slot`, once this has said how long it waited for it, if it did, since `waiting`.
    fn waited(&self, slot: Slot, waiting: Option<Instant>) -> Slot {
        if let Some(since) = waiting {
            say(format_args!(
                "took a session of controller `{}` after waiting {} s",
                self.name,
                since.elapsed().as_secs()
            ));
        }
        slot
    }

    /// Opens shared master `master`, with the lock on opening one, `lock`: notes the instance
    /// there, and starts the master ([`master`](Self::master)); if the controller hasn't enrolled
    /// this machine's key, as on a new machine, or once the controller is made anew, it pushes
    /// the key through EC2 Instance Connect, starts the master with it, and enrols the key over
    /// it ([`enrol`](Self::enrol)).
    fn open(&self, lock: &Lock, master: usize) -> Result<(), String> {
        lock.note(&self.instance)?;
        match self.master(master)? {
            Master::Open => return Ok(()),
            Master::HostKeyChanged => return Err(self.host_key_changed(master)),
            Master::Unenrolled(_) => {}
        }
        say(format_args!(
            "enrolling this machine's key, {}, with controller `{}`",
            self.kept.join("key.pub").display(),
            self.name
        ));
        self.push()?;
        match self.master(master)? {
            Master::Open => self.enrol(master),
            Master::HostKeyChanged => Err(self.host_key_changed(master)),
            Master::Unenrolled(error) => Err(error),
        }
    }

    /// The refusal of a controller whose host key isn't the one pinned for it, as the log of
    /// shared master `master` says, which says what to do.
    fn host_key_changed(&self, master: usize) -> String {
        let known_hosts = self.kept.join("known_hosts");
        format!(
            "controller `{}` showed a host key other than the one {} pins for it as `{}`, as its \
             log, {}, says: unless it was made anew since, ask its administrator why before \
             connecting again; if it was, unpin the old key, and the next command pins the new \
             one: ssh-keygen -R {} -f {}",
            self.name,
            known_hosts.display(),
            self.alias,
            self.shared.log(master).display(),
            self.alias,
            next::quoted(&known_hosts.to_string_lossy())
        )
    }

    /// Adds this machine's key to the `authorized_keys` of the controller's user, over shared
    /// master `master`, which the key pushed through EC2 Instance Connect opened.
    fn enrol(&self, master: usize) -> Result<(), String> {
        let public = self.kept.join("key.pub");
        let public = fs::read(&public).context(public.display())?;
        let mut ssh = self.shared.command("ssh");
        ssh.args(self.ssh_options(master)).arg(self.host());
        ssh.arg("umask 077 && mkdir --parents .ssh && cat >> .ssh/authorized_keys");
        let fed = stdx::process::fed(&mut ssh, &public, Deadline::after(SHORT_TIMEOUT));
        finished(fed, "ssh", SHORT_TIMEOUT).map(|_| ())
    }

    /// Starts shared master `master`, which returns once it has authenticated, and goes on in
    /// the background until it has had no command for [`PERSIST`]; says, if it didn't start,
    /// whether the controller refused the key, or the host key, as its log says, or else fails.
    fn master(&self, master: usize) -> Result<Master, String> {
        // The last master's log goes as this one opens.
        let log = self.shared.log(master);
        private(&log)?.set_len(0).context(log.display())?;
        let mut ssh = self.shared.command("ssh");
        // Before the options, as `ssh` takes the first value it is given of each: its log, at the
        // level that says why a connection ends, as when the server stops answering, and its
        // proxy's own errors, which `ssh` gives no standard error once the master is in the
        // background.
        let proxy = format!("ProxyCommand={} 2>>{master}.log", self.cli.ssh_proxy());
        let named = format!("{master}.log");
        ssh.args(["-E", &named, "-o", "LogLevel=INFO", "-o", &proxy]);
        ssh.args(self.ssh_options(master));
        let persist = format!("ControlPersist={}", PERSIST.as_secs());
        // `auto` rather than `yes`, so that `ssh` removes a socket that a killed master left,
        // which would keep it from listening, and leave it a connection of its own that never
        // returns.
        ssh.args(["-N", "-o", "ControlMaster=auto", "-o", &persist]);
        ssh.arg(self.host());
        let Err(error) = bounded(&mut ssh, "ssh", SHORT_TIMEOUT) else {
            return Ok(Master::Open);
        };
        let said = fs::read_to_string(&log).unwrap_or_default();
        if said.contains("Host key verification failed") {
            return Ok(Master::HostKeyChanged);
        }
        let error = format!("{error}; its log, {}, says: {}", log.display(), said.trim());
        if said.contains("Permission denied (") {
            return Ok(Master::Unenrolled(error));
        }
        Err(error)
    }

    /// Runs `session` as [`bounded`] does, with `input`, if any, on its standard input; if it
    /// fails, saying where the log of the master it went over is, if that has ended since
    /// ([`lost`](Self::lost)).
    fn call(
        &self,
        session: &mut Session,
        input: Option<&[u8]>,
        shown: &str,
        timeout: Duration,
    ) -> Result<Vec<u8>, String> {
        let output = session.output(input, Deadline::after(timeout));
        let master = session.slot.master;
        finished(output, shown, timeout).map_err(|error| self.lost(master, error))
    }

    /// Runs the `rsync` that `make` makes, with `input`, if any, on its standard input, as
    /// [`call`](Self::call) does, and again, carrying on from what it sent ([`rsync`](Self::rsync)),
    /// each time its connection ended, as [`pause`](Self::pause) allows.
    fn sent(
        &self,
        make: impl Fn() -> Result<Session, String>,
        input: Option<&[u8]>,
        what: &str,
    ) -> Result<Vec<u8>, String> {
        let mut attempt = 0;
        loop {
            let mut rsync = make()?;
            let output = rsync.output(input, Deadline::after(UNBOUNDED));
            let master = rsync.slot.master;
            drop(rsync);
            let ended = matches!(&output, Ok(Some(output)) if self.ended_connection(output.status, &RSYNC_ENDED, master));
            if !(ended && self.pause(attempt, master, &format!("{what} again, carrying on"))) {
                return finished(output, "rsync", UNBOUNDED)
                    .map_err(|error| self.lost(master, error));
            }
            attempt += 1;
        }
    }

    /// Whether `status`, of a call over shared master `master`, says that its connection ended,
    /// as one of `codes` does, and the master no longer answers.
    fn ended_connection(&self, status: ExitStatus, codes: &[i32], master: usize) -> bool {
        status.code().is_some_and(|code| codes.contains(&code))
            && !self.shared.answers(master, &self.host())
    }

    /// Whether a call whose connection over shared master `master` ended is to be made again,
    /// after attempt `attempt`, counted from 0: once it has said so, saying where the master's log
    /// is and `what` it does again, and has waited [`AGAIN`]'s pause for it; not once those have
    /// run out.
    fn pause(&self, attempt: usize, master: usize, what: &str) -> bool {
        let Some(pause) = AGAIN.get(attempt) else {
            return false;
        };
        let when = match pause.as_secs() {
            0 => "now".to_owned(),
            seconds => format!("in {seconds} s"),
        };
        say(format_args!(
            "the connection to controller `{}` ended, as its log says: {}: {what}, {when}, \
             attempt {} of {}",
            self.name,
            self.shared.log(master).display(),
            attempt + 2,
            AGAIN.len() + 1
        ));
        thread::sleep(*pause);
        true
    }

    /// `error`, of a call over shared master `master`, with where that master's log is, if it
    /// has ended since, which says why.
    fn lost(&self, master: usize, error: String) -> String {
        if self.shared.answers(master, &self.host()) {
            return error;
        }
        format!(
            "{error}; the connection it went over has ended, as its log says: {}",
            self.shared.log(master).display()
        )
    }

    /// Pushes this machine's key through EC2 Instance Connect, for a connection that begins
    /// within the minute that the push is good for.
    fn push(&self) -> Result<(), String> {
        let public = format!("file://{}", self.kept.join("key.pub").display());
        let args = [
            "ec2-instance-connect",
            "send-ssh-public-key",
            "--instance-id",
            &self.instance,
            "--instance-os-user",
            USER,
            "--ssh-public-key",
            &public,
        ];
        self.cli.run(&args, Bound::Call).map(|_| ())
    }

    /// The options of each `ssh` to the controller, which goes over shared master `master`, as
    /// the directory it runs in names it ([`Shared::command`]).
    fn ssh_options(&self, master: usize) -> Vec<String> {
        let mut words = vec!["-F".to_owned(), "/dev/null".to_owned(), "-i".to_owned()];
        words.push(self.kept.join("key").display().to_string());
        let known_hosts = self.kept.join("known_hosts").display().to_string();
        words.extend([
            "-o".to_owned(),
            format!("UserKnownHostsFile={known_hosts}"),
            // By the controller's name, rather than the instance that the tag finds, so that the
            // tag that names the controller can't lead this machine's key to another instance.
            "-o".to_owned(),
            format!("HostKeyAlias={}", self.alias),
        ]);
        let options = [
            "IdentitiesOnly=yes",
            "BatchMode=yes",
            "StrictHostKeyChecking=accept-new",
            "LogLevel=ERROR",
            // So that a connection whose other end is gone, as when this machine slept, ends
            // within minutes, rather than when TCP gives up.
            "ServerAliveInterval=30",
        ];
        for option in options {
            words.extend(["-o".to_owned(), option.to_owned()]);
        }
        words.extend([
            "-o".to_owned(),
            format!("ProxyCommand={}", self.cli.ssh_proxy()),
            "-o".to_owned(),
            format!("ControlPath={master}"),
        ]);
        words
    }

    /// The controller's user at its instance, as `ssh` names it.
    fn host(&self) -> String {
        format!("{USER}@{}", self.instance)
    }

    /// `ssh` that runs `script` on the controller, as its user's shell runs it, over a session of
    /// a shared master ([`slot`](Self::slot)).
    fn ssh(&self, script: &str) -> Result<Session, String> {
        let slot = self.slot()?;
        let mut command = self.shared.command("ssh");
        let options = self.ssh_options(slot.master);
        command.args(options).arg(self.host()).arg(script);
        Ok(Session { command, slot })
    }

    /// `rsync` over `ssh` to the controller, as [`ssh`](Self::ssh) is, which ends after
    /// [`STALL_TIMEOUT`] with no byte moved, and keeps a file it was part way through, which a
    /// later `rsync` of it carries on from; its operands name a path there as
    /// [`remote`](Self::remote) does, and one here by its absolute path.
    fn rsync(&self) -> Result<Session, String> {
        let slot = self.slot()?;
        let mut words = vec!["ssh".to_owned()];
        words.extend(self.ssh_options(slot.master));
        let shell: Vec<String> = words.iter().map(|word| next::quoted(word)).collect();
        let mut rsync = self.shared.command("rsync");
        rsync.arg("--rsh").arg(shell.join(" "));
        rsync.arg(format!("--timeout={}", STALL_TIMEOUT.as_secs()));
        rsync.arg("--partial");
        Ok(Session {
            command: rsync,
            slot,
        })
    }

    /// `path`, below the controller's user's home, as `rsync` names it there.
    fn remote(&self, path: &str) -> String {
        format!("{}:{path}", self.host())
    }

    /// Runs `script` on the controller, with this machine's standard output and error, and
    /// `input` on its standard input, if any, else this machine's, until it ends, however long
    /// that takes.
    fn attached(&self, script: &str, input: Option<&[u8]>) -> Result<Attached, String> {
        let mut ssh = self.ssh(script)?;
        let Some(input) = input else {
            let status = ssh.status().context("running `ssh` failed")?;
            return Ok(Attached {
                status,
                master: ssh.slot.master,
            });
        };
        let child = ssh.stdin(Stdio::piped()).spawn();
        let mut child = child.context("running `ssh` failed")?;
        let Some(mut stdin) = child.stdin.take() else {
            unreachable!("the child's standard input is missing, though it is piped just above")
        };
        // Then dropped, so that the script reads its end.
        let written = stdin.write_all(input);
        drop(stdin);
        let status = child.wait().context("running `ssh` failed")?;
        written.context("writing to `ssh` failed")?;
        Ok(Attached {
            status,
            master: ssh.slot.master,
        })
    }

    /// The exit status that `attached`, a command on the controller that [`attached`] ran,
    /// gives this command, if it ended so: `ssh`'s own failure, or its end by a signal, is an
    /// error that says how to `follow` it again, and where the log of its master is, if that has
    /// ended.
    ///
    /// [`attached`]: Self::attached
    fn ended(&self, attached: Attached, again: &str) -> Result<i32, String> {
        let Attached { status, master } = attached;
        match status.code() {
            Some(SSH_FAILED) | None => {
                let ended = format!(
                    "the connection to controller `{}` ended ({status}), as said above",
                    self.name
                );
                Err(format!("{}: {again}", self.lost(master, ended)))
            }
            Some(code) => Ok(code),
        }
    }

    /// The id of the job of the controller's that `run`, an id, a prefix of one or of its random
    /// digits, names, as [`runtime::named`] finds it.
    fn resolve(&self, run: &str) -> Result<String, String> {
        let script = format!("ls --format=single-column \"$HOME/{JOBS}\" 2>/dev/null || true");
        let listed = self.call(&mut self.ssh(&script)?, None, "ssh", SHORT_TIMEOUT)?;
        let names: Vec<String> = String::from_utf8_lossy(&listed)
            .lines()
            .map(str::to_owned)
            .collect();
        let among = format!("on controller `{}`", self.name);
        runtime::named(&names, run, &among).map(str::to_owned)
    }

    /// Copies the record of run `id` from the controller into this machine's default runs
    /// directory, saying so first, as a large record takes a while.
    fn copy(&self, id: &str) -> Result<(), String> {
        let runs = PathBuf::from(crate::default_runs()?);
        fs::create_dir_all(&runs).context(runs.display())?;
        say(format_args!(
            "copying run {id}'s record from controller `{}` to {}",
            self.name,
            runs.join(id).display()
        ));
        let copying = || {
            let mut rsync = self.rsync()?;
            // But the sockets a run's `sockets/` may hold, which name nothing here.
            rsync.args(["--archive", "--no-D"]);
            rsync.arg(self.remote(&format!("{}/{id}/", crate::job::runs())));
            rsync.arg(runs.join(id));
            Ok(rsync)
        };
        self.sent(copying, None, &format!("copying run {id}'s record"))?;
        Ok(())
    }
}

/// The SSM parameter that holds the settings of the controller of `name`, as the module names
/// it.
fn parameter(name: &Name) -> String {
    format!("/pumpkin-{name}/settings")
}

/// A task's repository, as this machine sends it to the controller.
struct Tree {
    /// Its top, as git finds it from the entry's directory.
    top: PathBuf,
    /// The files git lists there, tracked, or untracked but not ignored, each ended by a NUL.
    files: Vec<u8>,
    /// How many files there are, and what they hold in all, in bytes, as this machine has them.
    files_count: usize,
    size_bytes: u64,
    /// The entry's path below the top.
    entry: String,
    /// The repository's name, the top's own as [`plain`] gives it, which its trees on the
    /// controller are below.
    repo: String,
    /// The commit of Pumpkin that its `.pumpkinversion` names.
    commit: String,
    /// The git bundle of that commit that `./pumpkin` keeps once it has built it, from which the
    /// controller builds the same Pumpkin.
    bundle: PathBuf,
}

/// The repository of the task whose entry is `entry`, given `args`, as the controller is sent it:
/// of the files that git lists there, those that checking the task reaches
/// ([`crate::star::reached`]), all that its build reads, with `./pumpkin` and `.pumpkinversion`.
///
/// # Errors
///
/// Fails if the entry is in no git repository, or git fails, or the repository has no
/// `./pumpkin`, the consumer entry, which the job builds Pumpkin with; or, saying them, if the
/// task has problems.
fn tree(entry: &crate::star::Entry, args: &lang::Args) -> Result<Tree, String> {
    let path = fs::canonicalize(entry.path()).context(entry.path())?;
    let dir = path.parent().unwrap_or(Path::new("/"));
    let git = |dir: &Path, args: &[&str]| {
        let mut git = stdx::process::command("git", Environment::Only(&["HOME", "PATH"]));
        git.arg("-C").arg(dir).args(args);
        bounded(&mut git, &format!("git {}", args.join(" ")), SHORT_TIMEOUT)
    };
    let top = git(dir, &["rev-parse", "--show-toplevel"]).map_err(|error| {
        format!("{entry}: {error}: a run on the controller sends the entry's git repository: put it in one")
    })?;
    let top = PathBuf::from(String::from_utf8_lossy(&top).trim_end_matches('\n'));
    let top = fs::canonicalize(&top).context(top.display())?;
    if !top.join("pumpkin").is_file() {
        return Err(format!(
            "{} has no ./pumpkin, with which the controller builds the Pumpkin that \
             .pumpkinversion names: copy Pumpkin's deploy/pumpkin there",
            top.display()
        ));
    }
    let (commit, bundle) = pumpkin_version(&top)?;
    let files = git(
        &top,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )?;
    let files = needed(entry, args, &top, &files)?;
    let mut files_count = 0;
    let mut size_bytes = 0;
    for file in files
        .split(|byte| *byte == 0)
        .filter(|file| !file.is_empty())
    {
        files_count += 1;
        // As `rsync --archive` sends it: a link as a link; one gone since git listed it, not.
        let file = top.join(std::ffi::OsStr::from_bytes(file));
        size_bytes += fs::symlink_metadata(&file).map_or(0, |metadata| metadata.len());
    }
    let below = path.strip_prefix(&top).ok().and_then(Path::to_str);
    let Some(below) = below else {
        unreachable!(
            "{} is below {}, as git found it from the entry's directory",
            path.display(),
            top.display()
        )
    };
    let name = top
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let repo = plain(&name);
    Ok(Tree {
        top,
        files,
        files_count,
        size_bytes,
        entry: below.to_owned(),
        repo,
        commit,
        bundle,
    })
}

/// `name` with each character but an ASCII letter or digit, `.`, `_` and `-` as `_`, as a name of a
/// file or directory.
fn plain(name: &str) -> String {
    let kept = |c: char| c.is_ascii_alphanumeric() || "._-".contains(c);
    name.chars()
        .map(|c| if kept(c) { c } else { '_' })
        .collect()
}

/// Of `listed`, the files that git lists in `top`, each ended by a NUL, those below it that
/// checking the task whose entry is `entry`, given `args`, reaches, as files or as directories
/// that hold them, with `./pumpkin` and `.pumpkinversion`, which the job runs.
fn needed(
    entry: &crate::star::Entry,
    args: &lang::Args,
    top: &Path,
    listed: &[u8],
) -> Result<Vec<u8>, String> {
    let (root, reached) = match crate::star::reached(entry, args)? {
        Ok(reached) => reached,
        Err(problems) => return Err(crate::task_problems(entry, &problems)),
    };
    // At or below the top, unless the marker is above the repository that holds the entry.
    let prefix = root.strip_prefix(top).map_err(|_| {
        format!(
            "{entry}: the task's root, {}, is above its git repository, {}, which is all that a \
             run on the controller is sent: put the root's `{}` in the repository",
            root.display(),
            top.display(),
            crate::star::MARKER
        )
    })?;
    let prefix = prefix.to_string_lossy();
    let prefix = prefix.as_ref();
    let under = |path: &str| match prefix {
        "" => path.to_owned(),
        prefix if path.is_empty() => prefix.to_owned(),
        prefix => format!("{prefix}/{path}"),
    };
    let mut kept: Vec<String> = reached.iter().map(|path| under(path)).collect();
    kept.extend(["pumpkin".to_owned(), ".pumpkinversion".to_owned()]);
    let wanted = |file: &str| {
        kept.iter().any(|path| {
            path.is_empty()
                || file == path
                || file
                    .strip_prefix(path.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        })
    };
    let mut files = Vec::new();
    for file in listed
        .split(|byte| *byte == 0)
        .filter(|file| !file.is_empty())
    {
        if wanted(&String::from_utf8_lossy(file)) {
            files.extend_from_slice(file);
            files.push(0);
        }
    }
    Ok(files)
}

/// The commit that the `.pumpkinversion` in `top` names, after its tag, and the bundle of it that
/// `./pumpkin` keeps beside its build, `.pumpkin/<commit>/pumpkin.bundle`.
fn pumpkin_version(top: &Path) -> Result<(String, PathBuf), String> {
    let path = top.join(".pumpkinversion");
    let text = fs::read_to_string(&path).context(path.display())?;
    let commit = text.split_whitespace().nth(1).unwrap_or_default();
    let hex = |c: char| c.is_ascii_digit() || ('a'..='f').contains(&c);
    if commit.len() != 40 || !commit.chars().all(hex) {
        return Err(format!(
            "{} names no commit after its tag: write the tag's full 40-digit commit",
            path.display()
        ));
    }
    let bundle = top.join(".pumpkin").join(commit).join("pumpkin.bundle");
    if !bundle.is_file() {
        return Err(format!(
            "{} isn't there, which the controller builds Pumpkin {commit} from: build it here \
             with `./pumpkin --version`, after removing {} if it was built without one",
            bundle.display(),
            top.join(".pumpkin").join(commit).display()
        ));
    }
    Ok((commit.to_owned(), bundle))
}

/// Sends `tree` to `reached` as job `id`'s, which runs `words`, `run` or `build` with their
/// arguments, in its directory below `kind`, with what `client` sends with each job, then starts
/// the job in a unit of its own.
///
/// # Errors
///
/// Fails if `rsync` or `ssh` fails, a key or Modal's configuration file can't be read, or
/// `systemd-run` refuses the unit.
fn send(
    reached: &Reached,
    client: &config::Client,
    id: &RunTag,
    kind: &str,
    tree: &Tree,
    words: &[String],
) -> Result<(), String> {
    let id = id.as_str();
    // Until the job's start has linked the tree as the repository's latest.
    let _sending = reached.shared.sending(&reached.name, tree)?;
    send_tree(reached, id, tree)?;
    let (config, keys) = client.sent()?;
    let mut files = vec![(SENT_CONFIG.to_owned(), config.into_bytes())];
    for (file, key) in keys {
        let read = fs::read(&key).map_err(|error| {
            format!(
                "the key file {} can't be read ({error}): put the key there",
                key.display()
            )
        })?;
        files.push((file, read));
    }
    if let Some(modal) = client.modal_file()? {
        files.push((SENT_MODAL.to_owned(), modal));
    }
    let archive = archive(&files);
    let script = starting(&client.controller, id, kind, tree, words);
    let mut ssh = reached.ssh(&script)?;
    reached
        .call(&mut ssh, Some(&archive), "ssh", SHORT_TIMEOUT)
        .map(|_| ())
}

/// A `tar` archive, in POSIX's ustar format, of `files`, each a name and what the file holds,
/// each file 0600: what [`send`] sends a job, made in memory, so that what it holds of keys is
/// never written on this machine but in the files that hold them.
fn archive(files: &[(String, Vec<u8>)]) -> Vec<u8> {
    const BLOCK: usize = 512;
    let mut archive = Vec::new();
    for (name, holds) in files {
        let mut header = [0; BLOCK];
        // The name, `config.toml`, `modal.toml` or an upstream's name of at most 64 bytes with
        // `.key`, fits its field's 100 bytes.
        header[..name.len()].copy_from_slice(name.as_bytes());
        // Its mode, owner, group, size and time, each a field of octal digits that a NUL ends.
        let numbers = [
            (100, format!("{:07o}", 0o600)),
            (108, format!("{:07o}", 0)),
            (116, format!("{:07o}", 0)),
            (124, format!("{:011o}", holds.len())),
            (136, format!("{:011o}", 0)),
        ];
        for (at, number) in numbers {
            header[at..at + number.len()].copy_from_slice(number.as_bytes());
        }
        // A regular file, of the ustar format, whose checksum is the sum of the header's bytes,
        // its own counted as spaces.
        header[156] = b'0';
        header[257..265].copy_from_slice(b"ustar\x0000");
        header[148..156].fill(b' ');
        let sum: u32 = header.iter().copied().map(u32::from).sum();
        header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        archive.extend_from_slice(&header);
        archive.extend_from_slice(holds);
        archive.resize(archive.len().next_multiple_of(BLOCK), 0);
    }
    // Two blocks of zeros end it.
    archive.resize(archive.len() + 2 * BLOCK, 0);
    archive
}

/// What a command on `controller` runs to start job `id`, below `kind`, which runs `words` in
/// its copy of `tree`, building the Pumpkin it pins from the bundle that [`send_tree`] sent, with
/// what the job is sent on its standard input, as `tar`.
fn starting(
    controller: &config::Controller,
    id: &str,
    kind: &str,
    tree: &Tree,
    words: &[String],
) -> String {
    let (repo, commit) = (&tree.repo, &tree.commit);
    let config::Controller {
        name,
        region,
        modal: _,
    } = controller;
    let words: Vec<String> = words.iter().map(|word| next::quoted(word)).collect();
    let (parameter, region) = (next::quoted(&parameter(name)), region.as_str());
    format!(
        r#"set -o errexit -o nounset
sent="$XDG_RUNTIME_DIR/pumpkin/{id}"
tree="$HOME/{TREES}/{repo}/{id}"
mkdir --parents --mode=0700 "$XDG_RUNTIME_DIR/pumpkin" "$sent"
# Until the unit takes the keys, whose end removes them, a failure here does.
trap 'rm --recursive --force "$sent"' EXIT
tar --extract --directory="$sent"
mkdir --parents "$HOME/{kind}/{id}" "$HOME/{WORKSPACE}"
ln --symbolic "$HOME/{WORKSPACE}" "$tree/.pumpkin"
ln --symbolic --force --no-dereference {id} "$HOME/{TREES}/{repo}/latest"
systemd-run --user --quiet --expand-environment=no --unit=pumpkin-job-{id} \
    --working-directory="$tree" --property=ExitType=cgroup \
    --property="ExecStopPost=/usr/bin/rm --recursive --force $sent" \
    --property="StandardOutput=append:$HOME/{kind}/{id}/out" \
    --property="StandardError=append:$HOME/{kind}/{id}/log" \
    -- "$tree/pumpkin" --pumpkin-repository="$HOME/{WORKSPACE}/{commit}.bundle" \
    controller job {id} --region {region} --parameter {parameter} -- {}
trap - EXIT
"#,
        words.join(" "),
        WORKSPACE = workspace::DIR
    )
}

/// Sends `tree` as job `id`'s, once it has said how much it holds, and the bundle of the Pumpkin
/// it pins into the controller's one workspace, as `<commit>.bundle`, which a later send of the
/// same commit skips; each carries on over another connection if its own ends ([`Reached::sent`]).
fn send_tree(reached: &Reached, id: &str, tree: &Tree) -> Result<(), String> {
    let repo = &tree.repo;
    say(format_args!(
        "sending the task's tree, {} in {}, to controller `{}`",
        size(tree.size_bytes),
        crate::counted(tree.files_count, "file"),
        reached.name
    ));
    let sending = || {
        let mut rsync = reached.rsync()?;
        // Each job's tree is of its own, and shares with the repository's last one each file
        // that hasn't changed since, which `--link-dest` links rather than sends.
        rsync.args([
            "--archive",
            "--from0",
            "--files-from=-",
            "--ignore-missing-args",
            "--mkpath",
            "--link-dest=../latest",
        ]);
        rsync.arg(format!("{}/", tree.top.display()));
        rsync.arg(reached.remote(&format!("{TREES}/{repo}/{id}/")));
        Ok(rsync)
    };
    reached.sent(sending, Some(&tree.files), "sending the task's tree")?;
    let bundle = format!("{}/{}.bundle", workspace::DIR, tree.commit);
    let bundling = || {
        let mut rsync = reached.rsync()?;
        rsync.args(["--archive", "--mkpath"]).arg(&tree.bundle);
        rsync.arg(reached.remote(&bundle));
        Ok(rsync)
    };
    reached
        .sent(bundling, None, "sending Pumpkin's bundle")
        .map(|_| ())
}

/// `bytes` for a glance, to a tenth of the largest unit of 1024 that it reaches, as `3.4 MiB`.
fn size(bytes: u64) -> String {
    let units = [(1 << 30, "GiB"), (1 << 20, "MiB"), (1 << 10, "KiB")];
    let Some((unit, name)) = units.into_iter().find(|(unit, _)| bytes >= *unit) else {
        return crate::counted(bytes, "byte");
    };
    let tenths = bytes * 10 / unit;
    format!("{}.{} {name}", tenths / 10, tenths % 10)
}

/// What a command on the controller runs to follow job `id`, below `kind`: shows its unit's
/// standard error, `log`, as it grows, until the job ends or `done`, a test in its directory,
/// holds, then runs `then`.
// `tail` follows the log until the loop before it ends, then shows what the log holds by then.
fn following(kind: &str, id: &str, done: &str, then: &str) -> String {
    format!(
        r#"cd "$HOME/{kind}/{id}" 2>/dev/null || exit {NO_JOB}
pid=$(systemctl --user show --property=MainPID --value pumpkin-job-{id})
while [ "${{pid:-0}}" -gt 0 ] && kill -0 "$pid" 2>/dev/null && ! {done}; do sleep 1; done &
tail --lines=+1 --follow --pid=$! log >&2 &
wait
{then}
"#
    )
}

/// Refuses `flags` of `run` or `build` that name what this machine's runs would use, but its
/// configuration sends them to the controller.
fn unsendable(client: &config::Client, flags: &[(&str, bool)]) -> Result<(), String> {
    let given: Vec<&str> = flags
        .iter()
        .filter(|(_, given)| *given)
        .map(|(flag, _)| *flag)
        .collect();
    if given.is_empty() {
        return Ok(());
    }
    Err(format!(
        "{} names this machine's, but its configuration sends runs to controller `{}`: leave it \
         out",
        given.join(" and "),
        client.controller.name
    ))
}

/// `pumpkin run` on the controller: sends the task's repository and starts the job, which
/// builds the task, then runs it with the id minted here, and says so at once; then prints the
/// id, or with `wait` follows the run as [`wait`] does.
pub(crate) fn run(client: &config::Client, flags: cli::Run) -> Result<ExitCode, String> {
    let cli::Run {
        entry,
        args: cli::TaskArgs(args),
        runs,
        config,
        sandboxd,
        wait,
    } = flags;
    unsendable(
        client,
        &[
            ("`--runs`", runs.is_some()),
            ("`--sandboxd`", sandboxd.is_some()),
        ],
    )?;
    let id = runtime::new_id(SystemTime::now())?;
    let name = &client.controller.name;
    let tree = tree(&entry, &args)?;
    say(format_args!(
        "run {} goes to controller `{name}`",
        id.as_str()
    ));
    let reached = Reached::reach(&client.controller)?;
    let mut words = vec!["run".to_owned()];
    words.extend(crate::arg_flags(&args));
    words.extend(["--".to_owned(), tree.entry.clone()]);
    send(&reached, client, &id, JOBS, &tree, &words)?;
    let id = id.as_str();
    say(format_args!(
        "run {id} is a job on controller `{name}`, which builds it, then runs it"
    ));
    if wait {
        return follow(&reached, id, config.as_deref());
    }
    crate::advise("follow it", &next::of_run("wait", None, id));
    crate::advise("stop it", &next::of_run("cancel", None, id));
    crate::print(format!("{id}\n").as_bytes())
        .context(format_args!("run {id} is sent, but printing its id failed"))?;
    Ok(ExitCode::SUCCESS)
}

/// `pumpkin wait` on the controller, of the run that `run` names there.
pub(crate) fn wait(client: &config::Client, run: &str) -> Result<ExitCode, String> {
    let reached = Reached::reach(&client.controller)?;
    let id = reached.resolve(run)?;
    follow(&reached, &id, None)
}

/// Follows run `id`'s job on `reached` until it has started the run, or ended, then the run,
/// saying its milestones as they come, by the `pumpkin` that the job runs
/// ([`crate::job::follow`]), and copies the run's record here, from which it says the run's
/// ending, which ends with the command that runs it again from here, with the configuration at
/// `config`, if given ([`here`]); returns the exit status that `wait`'s help gives.
fn follow(reached: &Reached, id: &str, config: Option<&str>) -> Result<ExitCode, String> {
    // A job prints its run's id once it has said all it says of the run's start.
    let started = "[ -s out ]";
    // A job that predates its own link is followed by the controller's `wait`, which says the
    // run's ending as the controller would run it again.
    let then = format!(
        "cd\n[ -d {runs}/{id} ] || exit {UNSTARTED}\n\
         [ ! -x {JOBS}/{id}/{own} ] || exec {JOBS}/{id}/{own} controller follow {id}\n\
         exec ./{current} wait {id}",
        runs = crate::job::runs(),
        own = crate::job::OWN,
        current = crate::job::CURRENT
    );
    let again = format!("follow it again: {}", next::of_run("wait", None, id));
    let mut attempt = 0;
    let code = loop {
        let attached = reached.attached(&following(JOBS, id, started, &then), None)?;
        let ended = reached.ended_connection(attached.status, &[SSH_FAILED], attached.master);
        let what = format!("following run {id} again from the start of its log");
        if ended && reached.pause(attempt, attached.master, &what) {
            attempt += 1;
            continue;
        }
        break reached.ended(attached, &again)?;
    };
    let name = &reached.name;
    match code {
        NO_JOB => Err(format!("controller `{name}` has no run {id}")),
        UNSTARTED => Err(format!(
            "run {id}'s job on controller `{name}` ended before the run started, as its log \
             says above: once that is fixed, run the task again as it was sent; to see that log \
             again: {}",
            next::of_run("wait", None, id)
        )),
        code if code == i32::from(crate::job::ENDED) => said_here(reached, id, config),
        code => {
            reached.copy(id)?;
            Ok(exit(code))
        }
    }
}

/// Copies the record of run `id`, which has ended on `reached`, here, and says its ending from the
/// copy, as `wait` does, ending with the command that runs it again from here, with the
/// configuration at `config`, if given ([`here`]); returns the exit status that `wait`'s help
/// gives.
fn said_here(reached: &Reached, id: &str, config: Option<&str>) -> Result<ExitCode, String> {
    reached.copy(id)?;
    let runs = PathBuf::from(crate::default_runs()?);
    let record = runtime::Record::open(&runs.join(id))?;
    let Some(ending) = record.ending()? else {
        return Err(format!(
            "run {id}'s copy, {}, reads as live, though its run has ended: wait for what holds \
             its driver.pid, then follow it again: {}",
            record.dir().display(),
            next::of_run("wait", None, id)
        ));
    };
    crate::end(&record, &ending, None, &here(&record, config)?)
}

/// The arguments of the `pumpkin run` that runs the run of `record` again from here, of a copy of
/// a record that a job on the controller made: the job's, but its configuration, which is this
/// machine's at `config`, if given, else the default one, and its entry, in this workspace's
/// repository, whose files the job's tree, named by the run's id, holds where the repository does.
/// Those of a record that no job made are its own.
pub(crate) fn here(record: &runtime::Record, config: Option<&str>) -> Result<Vec<String>, String> {
    let args = record.args();
    let tree = format!("/{}/", record.id());
    let below = args.split_last().and_then(|(entry, flags)| {
        let (_, below) = entry.split_once(&tree)?;
        Some((below, flags))
    });
    let Some((below, flags)) = below else {
        return Ok(args.to_vec());
    };
    let mut words = Vec::new();
    if let Some(config) = config {
        words.extend([
            "--config".to_owned(),
            crate::absolute_utf8(config, "the configuration")?,
        ]);
    }
    let mut flags = flags.iter();
    while let Some(flag) = flags.next() {
        if flag == "--config" {
            flags.next();
        } else {
            words.push(flag.clone());
        }
    }
    let entry = Workspace::find()?.repository().join(below);
    words.push(entry.to_string_lossy().into_owned());
    Ok(words)
}

/// The exit status `code`, of a command on the controller, as this command's.
fn exit(code: i32) -> ExitCode {
    u8::try_from(code).map_or(ExitCode::FAILURE, ExitCode::from)
}

/// `pumpkin build` on the controller: sends the task's repository, starts the job, which builds
/// the task, follows it, and prints the IR it printed.
pub(crate) fn build(client: &config::Client, flags: cli::Build) -> Result<ExitCode, String> {
    let cli::Build {
        entry,
        args: cli::TaskArgs(args),
        config: _,
    } = flags;
    let id = runtime::new_id(SystemTime::now())?;
    let tree = tree(&entry, &args)?;
    let reached = Reached::reach(&client.controller)?;
    let mut words = vec!["build".to_owned()];
    words.extend(crate::arg_flags(&args));
    words.extend(["--".to_owned(), tree.entry.clone()]);
    send(&reached, client, &id, BUILDS, &tree, &words)?;
    let then = format!("[ -s out ] || exit {UNSTARTED}\nexec cat out");
    // Its IR, in `out`, is whole only once the job has ended.
    let status = reached.attached(&following(BUILDS, id.as_str(), "false", &then), None)?;
    let name = &reached.name;
    let again = format!(
        "the build goes on there: its IR is in ~/{BUILDS}/{}/out there once it ends",
        id.as_str()
    );
    match reached.ended(status, &again)? {
        UNSTARTED => Err(format!(
            "the build on controller `{name}` failed, as its log says above"
        )),
        code => Ok(exit(code)),
    }
}

/// `pumpkin ls` on the controller: lists its runs whose ids start with one of `prefixes`, or
/// every run without any but those that ended over a day ago ([`progress::listed_since`]), the
/// jobs yet to start one as `waiting` for a build slot, or else `building`, or `unstarted` once
/// they have ended, beside the copies in this machine's default runs directory, by id, the
/// controller's of a run that both hold. Each run is read here, from the files of its record that
/// `ls` reads, which the controller sends once the record has ended unless its copy here has.
/// Then says how many tasks this workspace is sending the controller ([`said_sends`]), if any.
///
/// [`progress::listed_since`]: crate::progress::listed_since
pub(crate) fn ls(client: &config::Client, prefixes: &[String]) -> Result<ExitCode, String> {
    let reached = Reached::reach(&client.controller)?;
    let now = crate::unix_now();
    let since = crate::progress::listed_since(prefixes, now);
    let kept = reached.shared.ended();
    let (mut listing, mut unlisted) = listed_here(&kept, prefixes, since)?;
    let mut held = String::new();
    let ended = (listing.runs.iter()).filter_map(|(id, run)| run.ended().then_some(id));
    for id in ended.chain(&listing.older) {
        held.push_str(id);
        held.push('\n');
    }
    let since = since.map_or(0, |since| since.as_secs());
    let printed = listed_there(&reached, prefixes, since, &held)?;
    let scratch = scratch()?;
    let sent = scratch.path().join("runs");
    let states = extract(&printed, &sent)?;
    let mut health = Health::default();
    for line in String::from_utf8_lossy(states).lines() {
        let (id, state) = line.split_once(' ').unwrap_or((line, ""));
        if health.take(id, state) {
            continue;
        }
        let run = match state {
            "old" => {
                listing.older.insert(id.to_owned());
                continue;
            }
            "building" => Ok(crate::progress::Listed::Building {
                asked: runtime::minted(id),
            }),
            "waiting" => Ok(crate::progress::Listed::Waiting {
                asked: runtime::minted(id),
            }),
            "unstarted" => Ok(crate::progress::Listed::Unstarted),
            // The lock that says a run is live is the controller's, so its copy here is read
            // as live.
            "live" => runtime::Record::open(&sent.join(id))
                .and_then(|record| crate::progress::Listed::run(&record, None)),
            "ended" => keep(&sent, &kept, id)
                .and_then(|record| crate::progress::Listed::run(&record, record.ending()?)),
            _ => Err(format!(
                "controller `{}` listed `{}`, which is none of the lines that its listing prints",
                reached.name,
                line.escape_debug()
            )),
        };
        match run {
            Ok(run) => {
                listing.runs.insert(id.to_owned(), run);
            }
            Err(error) => {
                say(error);
                unlisted += 1;
            }
        }
    }
    let jobs = listing.runs.values().filter(|run| run.live()).count();
    say(health.line(&reached.name, jobs));
    // As of when the controller began the listing, which is when it read what each run does.
    let lines = crate::progress::lines(&listing, now, None);
    crate::print(lines.as_bytes()).context("printing the runs failed")?;
    said_sends(&reached)?;
    said_when(now.since_epoch());
    if unlisted > 0 {
        return Err("a run can't be listed, as said above".to_owned());
    }
    Ok(ExitCode::SUCCESS)
}

/// What `ls` lists, given `prefixes` and `since` ([`crate::listed_in`]), of the runs that this
/// machine holds what it needs of: the copies in the default runs directory, and what `ls` kept
/// in `kept` of each record that ended; a copy, which `wait` made, rather than what was kept of
/// the same run. Says each run that can't be listed, and returns how many.
fn listed_here(
    kept: &Path,
    prefixes: &[String],
    since: Option<Duration>,
) -> Result<(crate::progress::Listing, usize), String> {
    let runs = PathBuf::from(crate::default_runs()?);
    let (mut listing, unlisted) = crate::listed_in(&runs, true, prefixes, since)?;
    let (ended, ended_unlisted) = crate::listed_in(kept, true, prefixes, since)?;
    for (id, run) in ended.runs {
        listing.runs.entry(id).or_insert(run);
    }
    let older = ended.older.into_iter();
    let older: Vec<String> = older.filter(|id| !listing.runs.contains_key(id)).collect();
    listing.older.extend(older);
    Ok((listing, unlisted + ended_unlisted))
}

/// Says how many tasks this workspace is sending to `reached` ([`Shared::sends`]), if any: a
/// run's job, which `ls` lists, starts only once its task's tree is sent.
fn said_sends(reached: &Reached) -> Result<(), String> {
    let sends = reached.shared.sends()?;
    if sends > 0 {
        let is = if sends == 1 { "is" } else { "are" };
        say(format_args!(
            "{} {is} being sent to controller `{}` from this workspace",
            crate::counted(sends, "task"),
            reached.name
        ));
    }
    Ok(())
}

/// Says when the controller began a listing, at `began`, since the Unix epoch, if it took
/// [`STALE`] or more to arrive.
fn said_when(began: Duration) {
    let took = crate::unix_now().since_epoch().saturating_sub(began);
    if took < STALE {
        return;
    }
    let stdx::Utc {
        hour,
        minute,
        second,
        ..
    } = stdx::Utc::from_unix(began.as_secs());
    say(format_args!(
        "the runs are listed as they were at {hour:02}:{minute:02}:{second:02} UTC, {} ago, as \
         the listing took that long to arrive",
        crate::progress::span(took)
    ));
}

/// The record of run `id` that ended, as `ls` was sent it in `sent`, moved into `kept`, which
/// [`Shared::ended`] names, and read there.
fn keep(sent: &Path, kept: &Path, id: &str) -> Result<runtime::Record, String> {
    fs::create_dir_all(kept).context(kept.display())?;
    let to = kept.join(id);
    fs::rename(sent.join(id), &to).context(to.display())?;
    runtime::Record::open(&to)
}

/// What [`listing`] prints on `reached` of the runs whose ids start with one of `prefixes`, or
/// every run without any, of those that ended at `since` or after, but those whose ids `held`
/// lists, a line each, whose copies here have ended.
fn listed_there(
    reached: &Reached,
    prefixes: &[String],
    since: u64,
    held: &str,
) -> Result<Vec<u8>, String> {
    let mut ssh = reached.ssh(&listing(prefixes, since))?;
    let fed = stdx::process::fed(&mut ssh, held.as_bytes(), Deadline::after(SHORT_TIMEOUT));
    let output = match fed {
        Ok(Some(output)) => output,
        Ok(None) => {
            return Err(format!(
                "listing the runs on controller `{}` took over {} s",
                reached.name,
                SHORT_TIMEOUT.as_secs()
            ));
        }
        Err(error) => return Err(format!("`ssh` didn't run: {error}: install it on PATH")),
    };
    let _ = std::io::stderr().write_all(&output.stderr);
    if !output.status.success() {
        let failed = format!(
            "listing the runs on controller `{}` failed ({}), as said above",
            reached.name, output.status
        );
        return Err(reached.lost(ssh.slot.master, failed));
    }
    Ok(output.stdout)
}

/// What the controller said of itself as it listed its runs ([`listing`]): how long it has been
/// up, the state of its boot's unit, `pumpkin-boot.service`, and how full its disk is.
#[derive(Default)]
struct Health {
    up: Option<Duration>,
    /// The unit's properties that `systemctl show` printed, by name.
    boot: std::collections::BTreeMap<String, String>,
    /// The share of the disk in use, as `df` gives it, such as `61%`.
    disk: Option<String>,
}

impl Health {
    /// Takes in a line that [`listing`] printed, its first word `word` and then `rest`, if it is
    /// one of the controller's health; returns whether it was.
    fn take(&mut self, word: &str, rest: &str) -> bool {
        match word {
            "up" => {
                let seconds = rest
                    .split('.')
                    .next()
                    .and_then(|seconds| seconds.parse().ok());
                self.up = seconds.map(Duration::from_secs);
            }
            "boot" => {
                if let Some((name, value)) = rest.split_once('=') {
                    self.boot.insert(name.to_owned(), value.to_owned());
                }
            }
            "disk" => self.disk = Some(rest.trim().to_owned()),
            _ => return false,
        }
        true
    }

    /// The line that says the health of controller `name`, which runs `jobs` jobs.
    fn line(&self, name: &Name, jobs: usize) -> String {
        let mut said = Vec::new();
        if let Some(up) = self.up {
            said.push(format!("up {}", crate::progress::span(up)));
        }
        said.push(crate::counted(jobs, "job"));
        if let Some(disk) = &self.disk {
            said.push(format!("disk {disk} used"));
        }
        let property = |name: &str| self.boot.get(name).map_or("", String::as_str);
        let boot = match property("ActiveState") {
            "failed" => format!(
                "; its boot, pumpkin-boot.service, failed with status {} at {}: read why there, \
                 in `journalctl --unit pumpkin-boot.service`",
                property("ExecMainStatus"),
                property("ExecMainExitTimestamp")
            ),
            "activating" => "; its boot, pumpkin-boot.service, is still running".to_owned(),
            _ => String::new(),
        };
        format!("controller `{name}`: {}{boot}", said.join(", "))
    }
}

/// The files of a record that `ls` reads, which the controller sends.
const LISTED_FILES: &str = "record.json ir.json provider.json log.jsonl outcome.json driver.pid";

/// What a command on the controller runs to list its runs whose ids start with one of
/// `prefixes`, or every run without any: its health, `up <seconds>`, `boot <property>=<value>`
/// for each that `systemctl show` gives of its boot's unit, and `disk <used>%`; a line for each
/// job yet to start its run, `<id> waiting` while it waits for a build slot, or else
/// `<id> building`, or `<id> unstarted` once it has ended; a line for each run, `<id> live`, or
/// `<id> ended` unless its id is one of those its standard input lists; of a run that ended, or a
/// job, before `since`, in seconds since the Unix epoch, as its `outcome.json`, or its log, was
/// last written, `<id> old` instead; then `tar`, and an archive of the files of the records of the runs
/// that are live or ended that `ls` reads ([`LISTED_FILES`]).
// A run may write its log while GNU tar reads it, which tar says by its exit status 1 alone: the
// log then ends in a torn line, which a reader skips.
fn listing(prefixes: &[String], since: u64) -> String {
    let picked = if prefixes.is_empty() {
        String::new()
    } else {
        let patterns: Vec<String> = (prefixes.iter())
            .map(|prefix| format!("{}*", next::quoted(prefix)))
            .collect();
        format!("case $id in {}) ;; *) continue ;; esac", patterns.join("|"))
    };
    format!(
        r#"cd
held=$(cat)
echo "up $(cut --delimiter=' ' --fields=1 /proc/uptime)"
systemctl show --property=ActiveState --property=ExecMainStatus --property=ExecMainExitTimestamp pumpkin-boot.service | sed 's/^/boot /'
echo "disk $(df --output=pcent / | tail --lines=1)"
runs={runs}
recent() {{ [ ! -e "$1" ] || [ "$(stat --format=%Y "$1")" -ge {since} ]; }}
for id in $(ls {JOBS} 2>/dev/null); do
    {picked}
    [ ! -e "$runs/$id" ] || continue
    if [ -e "$XDG_RUNTIME_DIR/pumpkin/$id/{WAITING}" ]; then echo "$id waiting"
    elif [ -d "$XDG_RUNTIME_DIR/pumpkin/$id" ]; then echo "$id building"
    elif recent "{JOBS}/$id/log"; then echo "$id unstarted"; else echo "$id old"; fi
done
sent=
for id in $(ls "$runs" 2>/dev/null); do
    {picked}
    [ -e "$runs/$id/record.json" ] || continue
    if [ ! -e "$runs/$id/outcome.json" ] && ! flock --nonblock --shared "$runs/$id/driver.pid" true 2>/dev/null; then
        state=live
    else
        state=ended
        if printf '%s
' "$held" | grep --quiet --line-regexp --fixed-strings "$id"; then continue; fi
        if ! recent "$runs/$id/outcome.json"; then echo "$id old"; continue; fi
    fi
    echo "$id $state"
    sent="$sent $id"
done
echo tar
cd "$runs" 2>/dev/null || true
for id in $sent; do
    for file in {LISTED_FILES}; do [ ! -e "$id/$file" ] || echo "$id/$file"; done
done | tar --create --warning=no-file-changed --file=- --files-from=- || [ $? -eq 1 ]
"#,
        runs = crate::job::runs()
    )
}

/// The lines of the states that [`listing`] printed, once the archive after them is extracted
/// into `sent`, a new directory.
fn extract<'p>(printed: &'p [u8], sent: &Path) -> Result<&'p [u8], String> {
    let mut at = 0;
    let mut archive: &[u8] = &[];
    for line in printed.split_inclusive(|byte| *byte == b'\n') {
        if line == b"tar\n" {
            archive = &printed[at + line.len()..];
            break;
        }
        at += line.len();
    }
    DirBuilder::new()
        .mode(0o700)
        .create(sent)
        .context(sent.display())?;
    let mut tar = stdx::process::command("tar", Environment::Only(&["PATH"]));
    tar.args(["--extract", "--file=-", "--directory"]).arg(sent);
    let extracted = stdx::process::fed(&mut tar, archive, Deadline::after(SHORT_TIMEOUT));
    finished(extracted, "tar --extract", SHORT_TIMEOUT)?;
    Ok(&printed[..at])
}

/// The script of `cancel` or `reap` on the controller that `body` makes of the flag that names
/// Modal's configuration file of `client`'s, if it has one ([`config::Client::modal_file`]), and
/// what the script is fed: that file, which it takes first into a directory of its own in tmpfs,
/// which its end removes, so that the command reaches the Modal providers of runs whose jobs,
/// which held the file, have ended. Without one, the flag is empty, and nothing is fed.
fn with_modal(
    client: &config::Client,
    body: impl FnOnce(&str) -> String,
) -> Result<(String, Option<Vec<u8>>), String> {
    let Some(file) = client.modal_file()? else {
        return Ok((body(""), None));
    };
    let script = format!(
        "umask 077\nmodal=$(mktemp --directory \"$XDG_RUNTIME_DIR/pumpkin-modal.XXXXXX\")\n\
         trap 'rm --recursive --force \"$modal\"' EXIT\ncat > \"$modal/modal.toml\"\n{}",
        body(" --modal-config \"$modal/modal.toml\"")
    );
    Ok((script, Some(file)))
}

/// `pumpkin cancel` on the controller, of the run that `run` names there: stops its job's unit
/// while the run builds, and removes the job's directory, unless the run has a record by then;
/// and cancels the run as the controller's `pumpkin cancel` does once it has started
/// ([`with_modal`]), by the `pumpkin` that its job runs ([`crate::job::cancel`]), then says its
/// ending from its copy here ([`said_here`]).
pub(crate) fn cancel(client: &config::Client, run: &str, force: bool) -> Result<ExitCode, String> {
    let reached = Reached::reach(&client.controller)?;
    let id = reached.resolve(run)?;
    let force = if force { " --force" } else { "" };
    let (script, modal) = with_modal(client, |flag| {
        format!(
            "cd\n[ -d {JOBS}/{id} ] || exit {NO_JOB}\n\
             if [ -d {runs}/{id} ]; then\n\
             [ ! -x {JOBS}/{id}/{own} ] || {{ {JOBS}/{id}/{own} controller cancel{force}{flag} {id}; exit; }}\n\
             ./{current} cancel{force}{flag} {id}; exit; fi\n\
             systemctl --user stop pumpkin-job-{id}\n[ -d {runs}/{id} ] || rm --recursive --force {JOBS}/{id}\n\
             exit {UNSTARTED}\n",
            runs = crate::job::runs(),
            own = crate::job::OWN,
            current = crate::job::CURRENT
        )
    })?;
    let again = format!("cancel it again: {}", next::of_run("cancel", None, &id));
    let name = &reached.name;
    match reached.ended(reached.attached(&script, modal.as_deref())?, &again)? {
        NO_JOB => Err(format!("controller `{name}` has no run {id}")),
        UNSTARTED => {
            say(format_args!(
                "run {id}'s job on controller `{name}` is stopped, before the run started"
            ));
            Ok(ExitCode::SUCCESS)
        }
        // `cancel`'s exit says only that the run has ended, however that was, and whether what
        // it left on its provider is gone, as a local one's does.
        code if code == i32::from(crate::job::ENDED) => {
            said_here(&reached, &id, None)?;
            Ok(ExitCode::SUCCESS)
        }
        code if code == i32::from(crate::job::ENDED_UNREACHED) => {
            said_here(&reached, &id, None)?;
            crate::advise(
                "remove what it left once its provider can be reached",
                &next::reap(None),
            );
            Ok(ExitCode::from(crate::UNREACHED))
        }
        code => {
            reached.copy(&id)?;
            Ok(exit(code))
        }
    }
}

/// `pumpkin reap` on the controller, as its own `pumpkin reap` does ([`with_modal`]).
pub(crate) fn reap(client: &config::Client) -> Result<ExitCode, String> {
    let reached = Reached::reach(&client.controller)?;
    let current = crate::job::CURRENT;
    // Before its first job, the controller has no `pumpkin`, and no run to reap.
    let (script, modal) = with_modal(client, |flag| {
        format!(
            "cd\n[ -x {current} ] && [ -d {runs} ] || exit 0\n./{current} reap{flag}\n",
            runs = crate::job::runs()
        )
    })?;
    let again = "reap again: pumpkin reap";
    Ok(exit(reached.ended(
        reached.attached(&script, modal.as_deref())?,
        again,
    )?))
}
