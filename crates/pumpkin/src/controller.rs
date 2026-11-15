//! The controller target: with `[controller]` in its configuration ([`config::Client`]), this
//! machine sends `run` and `build` to the person's controller, an EC2 instance that the module in
//! `deploy/aws/` makes, and follows them, lists, stops and reaps its runs, there, with the same
//! commands as it would run them itself.
//!
//! Each command reaches the controller over the connections that the workspace's commands share
//! to it, master `ssh`es to the controller's user through SSM's `AWS-StartSSHSession`, each `ssh`
//! and `rsync` over one with a session free. With no master to share, it finds the controller by
//! its tag, starts it if it is stopped, waits until SSM sees it, and opens one, with this
//! machine's key, which it enrols through EC2 Instance Connect once the controller refuses it. A
//! follow of a run, a send and a copy whose connection ends go on over another, a few times. What
//! runs there is the controller's own side ([`crate::job`]), whose layout below the user's home
//! its module doc gives.
//!
//! What each command does over the connections is a module's of its own: a run's or a build's
//! send of its job ([`send`]), `ls`'s listing of the runs ([`listing`]), and the other commands
//! ([`commands`]).

mod commands;
mod listing;
mod send;

use std::collections::BTreeSet;
use std::fs::{self, DirBuilder, File, TryLockError};
use std::io::{self, Read as _, Write as _};
use std::ops::{Deref, DerefMut};
use std::os::unix::fs::{
    DirBuilderExt as _, FileExt as _, OpenOptionsExt as _, PermissionsExt as _,
};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};
use std::{env, fmt, thread};

use aws::{Bound, Cli};
pub(crate) use commands::{build, cancel, here, reap, run, wait};
pub(crate) use listing::ls;
use serde::Deserialize;
use stdx::process::Environment;
use stdx::{Context as _, Deadline};

use crate::job::JOBS;
use crate::workspace::Workspace;
use crate::{config, manifest, next, say};

/// The controller's user, which runs every Pumpkin process there.
const USER: &str = "pumpkin";

/// The tag by which the module names the controller of a name.
const TAG: &str = "pumpkin:controller";

/// How long a controller may take to start, and SSM to see it, before the command gives up.
// An instance that EC2 starts runs in about a minute, and its SSM agent registers in seconds
// more; past 10 minutes something else is wrong.
const START_TIMEOUT: Duration = Duration::from_mins(10);

const START_POLL: Duration = Duration::from_secs(5);

/// How long a copy by `rsync`, or a send of a task's files, may move no byte before it gives up.
// There is no bound on a copy's or a send's whole time: SSM carries well under 1 MB a second,
// so a record or a dataset of a GB or more takes as long as it takes. A dead connection ends by
// itself, as `ServerAliveInterval` gives `ssh` 90 s, so this is a little beyond that, to ride out
// an SSM session that pauses.
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

// A send's streams each go over a master of their own.
const _: () = assert!(manifest::STREAMS <= MASTERS);

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

const SESSION_POLL: Duration = Duration::from_millis(500);

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
/// the lock on sending the controller what its store lacks, `upload` ([`Shared::upload`]); and
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

    /// Takes the lock on sending the controller what its store lacks, saying, once, that it waits
    /// while another command holds it: a command that holds it asks again what the store lacks,
    /// so that of the commands that start at once, each sends only what the others haven't.
    // One send at a time: the sends of a workspace go one after another, so a batch's sends that
    // each lack a piece's worth or less go in turn, each over one connection, where they could go
    // at once over one each. In return no piece goes twice, and a batch's starts don't refuse one:
    // GNU tar replaces a file by removing it, then writing a new one, so of a piece that two sends
    // wrote at once, a start could take one part written, and so refuse its blob, which it checks
    // under a name of its own. A send from another workspace, which this lock doesn't hold back,
    // can still fail a start so, never the store: sent again, it is whole. And a send holds a
    // session for each of its streams, `manifest::STREAMS` at most of the `MASTERS` x `SESSIONS`,
    // so the rest stay for the workspace's other commands.
    fn upload(&self, name: &Name) -> Result<File, String> {
        let path = self.dir.join("upload");
        let file = private(&path)?;
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(error)) => return Err(error).context(path.display()),
        }
        say(format_args!(
            "waiting for another command's send to controller `{name}`, which may send some of \
             what this one would"
        ));
        file.lock().context(path.display())?;
        Ok(file)
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

    fn any_answers(&self, host: &str) -> bool {
        (0..MASTERS).any(|master| self.answers(master, host))
    }

    /// Looks at each master in turn, of `host`, but those of `held`: one whose sessions are all
    /// held is in use, and so open; of one with a session free, it takes that session, if the
    /// master answers.
    fn scan(&self, host: &str, held: &BTreeSet<usize>) -> Result<Found, String> {
        let mut closed = None;
        for master in (0..MASTERS).filter(|master| !held.contains(master)) {
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
    /// Fails if the Session Manager plugin isn't on `PATH`, the tag names no one instance, or SSM
    /// sees none by [`START_TIMEOUT`].
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
        // No master answers, so the first, master 0, is closed, and can be opened.
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
        self.slot_apart(&BTreeSet::new())
    }

    /// A session as [`slot`](Self::slot) gives one, but of a master other than those of `held`,
    /// as long as one can be had; else, once every other master's sessions are held, or the
    /// master to open can't be opened, which it says, of any.
    fn slot_apart(&self, held: &BTreeSet<usize>) -> Result<Slot, String> {
        let host = self.host();
        let mut waiting = None;
        loop {
            if let Found::Free(slot) = self.shared.scan(&host, held)? {
                return Ok(self.waited(slot, waiting));
            }
            let lock = self.shared.lock()?;
            match self.shared.scan(&host, held)? {
                Found::Free(slot) => return Ok(self.waited(slot, waiting)),
                Found::Closed(master) => {
                    match self.open(&lock, master) {
                        Ok(()) => {}
                        Err(_) if !held.is_empty() => {
                            drop(lock);
                            say(format_args!(
                                "connection {master} to controller `{}` can't be opened, as its \
                                 log says: {}: this goes on over those it holds",
                                self.name,
                                self.shared.log(master).display()
                            ));
                            return self.slot();
                        }
                        Err(error) => return Err(error),
                    }
                    // Held by commands over a master that has ended, which end with it.
                    if let Some(slot) = self.shared.free(master)? {
                        return Ok(self.waited(slot, waiting));
                    }
                }
                Found::Held if !held.is_empty() => {
                    drop(lock);
                    return self.slot();
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
        let error = format!(
            "{error}; its log, {}, says: {}: run it again",
            log.display(),
            said.trim()
        );
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

    /// Runs the session that `make` makes, `shown`, with `input`, if any, on its standard input,
    /// as [`call`](Self::call) does, killed after `timeout`; and again, saying it does `what`,
    /// each time it exited with one of `ended`, as its connection ended, as
    /// [`pause`](Self::pause) allows.
    fn again(
        &self,
        make: impl Fn() -> Result<Session, String>,
        input: Option<&[u8]>,
        shown: &str,
        ended: &[i32],
        timeout: Duration,
        what: &str,
    ) -> Result<Vec<u8>, String> {
        let mut attempt = 0;
        loop {
            let mut session = make()?;
            let output = session.output(input, Deadline::after(timeout));
            let master = session.slot.master;
            drop(session);
            let cut = matches!(&output, Ok(Some(output)) if self.ended_connection(output.status, ended, master));
            if !(cut && self.pause(attempt, master, what)) {
                return finished(output, shown, timeout).map_err(|error| self.lost(master, error));
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
            // An SSM session carries under 1 MB a second, far less than zlib compresses in one,
            // so what compression saves in bytes it saves in time. A master compresses its whole
            // connection, so each command that joins it sends compressed, as ssh_config(5) says:
            // https://github.com/openssh/openssh-portable/blob/5dba72adacbd39736960543373737bc01e35f778/ssh_config.5#L685-L693
            "Compression=yes",
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
        Ok(self.session(script, self.slot()?))
    }

    /// `ssh` that runs `script` on the controller, as [`ssh`](Self::ssh) does, over `slot`.
    fn session(&self, script: &str, slot: Slot) -> Session {
        let mut command = self.shared.command("ssh");
        let options = self.ssh_options(slot.master);
        command.args(options).arg(self.host()).arg(script);
        Session { command, slot }
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
        let what = format!("copying run {id}'s record again, carrying on");
        self.again(copying, None, "rsync", &RSYNC_ENDED, UNBOUNDED, &what)?;
        Ok(())
    }
}
