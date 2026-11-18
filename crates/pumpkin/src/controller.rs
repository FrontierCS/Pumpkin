//! Reaching a person's controller from this machine, and the `ssh` connections that a
//! workspace's commands share to it.
//!
//! A controller is one EC2 instance, found by its tag and reached by `ssh` at its private
//! address. A workspace keeps up to `MASTERS` master `ssh` connections per controller, each
//! carrying up to `SESSIONS` sessions at once. A session is held by a lock on a file of the
//! workspace, so the commands of separate `pumpkin` processes share the connections and none
//! takes a session that another holds. With no connection to share, a command opens one, with
//! this machine's key, at the address where the workspace last found the controller; when that
//! doesn't get through, it finds the controller again, starts it if it is stopped, and waits until
//! its `sshd` admits logins. It enrols the key through EC2 Instance Connect once the controller
//! refuses it. What goes over the connections is the submodules': `send`, `copy`, `listing`,
//! `viewing` and `commands`; `listing` and `viewing` go over a link to `serve`, which `linking`
//! opens.
//!
//! A command on the controller is cut when its connection ends, and then says nothing of how far
//! it got, so a command repeats only where its caller knows that running it again is safe.

mod aws;
mod commands;
mod copy;
mod linking;
mod listing;
mod send;
mod viewing;

use std::collections::BTreeSet;
use std::fs::{self, DirBuilder, File, TryLockError};
use std::io::{self, BufRead as _, Write as _};
use std::ops::{Deref, DerefMut};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};
use std::{fmt, thread};

use ::aws::Region;
pub(crate) use commands::{build, cancel, here, reap, run, wait};
pub(crate) use listing::{ls, scratch};
use serde::Deserialize;
use stdx::process::Environment;
use stdx::{Context as _, Deadline};
pub(crate) use viewing::Viewing;

use self::aws::{Located, START_POLL, answers, find, kept};
use crate::job::JOBS;
use crate::workspace::Workspace;
use crate::{config, manifest, next, say};

// The user that every session logs in as on the controller, and that runs every Pumpkin process
// there.
const USER: &str = "pumpkin";

// How long a send may move no byte before it is given up. A send has no bound on its whole time,
// as its size sets it. `ssh` ends a dead connection by itself in 90 s (a `ServerAliveInterval` of
// 30 s and the default of 3 misses), so this is a little beyond that.
const STALL_TIMEOUT: Duration = Duration::from_mins(2);

// How long `ssh` may take to connect to the controller's address. Inside its VPC a whole login
// takes under a second, so an address that takes this long drops what `ssh` sends it, as an
// instance that has stopped since its `sshd` answered does. Ansible's default:
// https://github.com/ansible/ansible/blob/2a4acd774aec2625b09cca2284cb2a08654e4cdd/lib/ansible/config/base.yml#L1205-L1212
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

// The bound of a command whose time its input doesn't set.
const SHORT_TIMEOUT: Duration = Duration::from_mins(10);

// How long a master stays open after its last session ends: long enough for a batch of commands
// that start at once, and each command's calls one after another, to share it, and short enough
// that none lingers once they are done.
const PERSIST: Duration = Duration::from_mins(1);

// The most master connections to one controller, so that a batch can't open connections without
// end: `MASTERS` x `SESSIONS` commands go at once, and one past that waits.
const MASTERS: usize = 16;

// Half of the 10 sessions that `sshd` takes on one connection by default (`MaxSessions`). It
// frees a session that has closed only once it has handled all it has read, so a session that one
// command closes and one that the next opens, in what it reads at once, both count for a moment:
// `SESSIONS` commands at a time each take 2 at most, which 10 still takes.
// https://github.com/openssh/openssh-portable/blob/5dba72adacbd39736960543373737bc01e35f778/servconf.h#L42
const SESSIONS: usize = 5;

// A send's streams each go over a master of their own.
const _: () = assert!(manifest::STREAMS <= MASTERS);

// The pauses before each repeat of a command whose connection ended. The first is at once, as one
// connection's end leaves the controller reachable over another. The rest add up to about four
// minutes, long enough for the network to carry a connection again after a pause, and short
// enough that a connection that keeps ending ends the command soon after.
const AGAIN: [Duration; 6] = [
    Duration::ZERO,
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(30),
    Duration::from_mins(1),
    Duration::from_mins(2),
];

// The status with which `ssh` ends when it fails itself, not the command it ran.
const SSH_FAILED: i32 = 255;

const SESSION_POLL: Duration = Duration::from_millis(500);

/// A controller's name: the `name` that `deploy/aws` was applied with.
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

/// The connections that the workspace's commands share to one controller, in
/// `controller/<name>.<region>/` of the workspace: up to `MASTERS` masters, each by its control
/// socket, named by its index, its `SESSIONS` session locks, `<index>-<session>`, and its log,
/// `<index>.log`; the locks `lock` and `upload`; and `found`, which [`Lock::found`] reads.
// Each `ssh` runs in that directory and names a socket by its name alone, so its path, with the
// suffix that `ssh` adds while it binds it, fits a socket's address however deep the workspace is.
struct Shared {
    dir: PathBuf,
}

/// The lock that serialises opening connections to a controller, and finding it.
struct Lock {
    _file: File,
    dir: PathBuf,
}

/// A session of connection `master`, held for as long as the slot lives.
struct Slot {
    master: usize,
    _file: File,
}

/// What `Shared::scan` finds: a free session on a master that answers, else the first master
/// that doesn't, to open, else that every session is held.
enum Found {
    Free(Slot),
    Closed(usize),
    Held,
}

impl Shared {
    fn new(workspace: &Workspace, alias: &str) -> Result<Self, String> {
        let dir = workspace.controller().join(alias);
        let made = DirBuilder::new().mode(0o700).recursive(true).create(&dir);
        made.context(dir.display())?;
        Ok(Self { dir })
    }

    fn lock(&self) -> Result<Lock, String> {
        let path = self.dir.join("lock");
        let file = private(&path)?;
        file.lock().context(path.display())?;
        Ok(Lock {
            _file: file,
            dir: self.dir.clone(),
        })
    }

    fn log(&self, master: usize) -> PathBuf {
        self.dir.join(format!("{master}.log"))
    }

    // One send at a time in a workspace, so that no piece goes twice and a batch's starts don't
    // refuse a blob that two sends wrote at once, part written, as `ARCHITECTURE.md`'s invariant
    // on the store says. A send from another workspace can still make a start refuse it, and
    // sending again mends that. In return, sends that each lack little go in turn rather than at
    // once, and a send holds a session for each of its streams, so the rest stay for the
    // workspace's other commands.
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

    fn command(&self, program: &str) -> Command {
        // `HOME`, below which `ssh` keeps a directory of its own.
        let mut command = stdx::process::command(program, Environment::Only(&["HOME", "PATH"]));
        command.current_dir(&self.dir);
        command
    }

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

    fn scan(&self, host: &str, held: &BTreeSet<usize>) -> Result<Found, String> {
        let mut closed = None;
        for master in (0..MASTERS).filter(|master| !held.contains(master)) {
            if !self.dir.join(master.to_string()).exists() {
                closed = closed.or(Some(master));
                continue;
            }
            // A master whose sessions are all held is in use, so it is open.
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
    /// Where the controller was last found, kept in `found` as JSON, unless that is missing or
    /// doesn't read, when it is to be found again.
    fn found(&self) -> Result<Option<Located>, String> {
        let path = self.dir.join("found");
        match fs::read(&path) {
            Ok(json) => Ok(serde_json::from_slice(&json).ok()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).context(path.display()),
        }
    }

    /// Keeps `located` in `found`, and returns it.
    fn keep(&self, located: Located) -> Result<Located, String> {
        let path = self.dir.join("found");
        let json = serde_json::to_vec(&located)
            .unwrap_or_else(|error| unreachable!("an instance's id and address are JSON: {error}"));
        // Made aside, 0600, and moved in whole, so that a command killed as it writes leaves the
        // last one whole.
        let aside = tempfile::Builder::new()
            .prefix(".found-")
            .tempfile_in(&self.dir);
        let mut aside = aside.context(self.dir.display())?;
        aside.write_all(&json).context(aside.path().display())?;
        aside.persist(&path).context(path.display())?;
        Ok(located)
    }
}

/// A `ssh` command to run over the connection that `slot` holds a session of, held until the
/// session is dropped.
struct Session {
    command: Command,
    slot: Slot,
}

impl Session {
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

/// How an `ssh` run with this process's own streams ended, and the connection it went over.
#[derive(Clone, Copy)]
struct Attached {
    status: ExitStatus,
    master: usize,
}

/// What opening a master connection came to.
enum Master {
    Open,
    /// `ssh` didn't get through to the controller's `sshd`, as the error says.
    Unreached(String),
    /// The controller refused this machine's key, as the error says.
    Unenrolled(String),
    /// The controller's host key isn't the one pinned for it.
    HostKeyChanged,
}

/// What `Reached::open` came to, but a failure.
enum Opened {
    Open,
    /// The controller may no longer be where it was found: `ssh` didn't get through, or met a
    /// host key other than the one pinned, as the error says.
    Unreached(String),
}

/// A controller that this machine has found, and its connections.
pub(crate) struct Reached {
    region: Region,
    name: Name,
    /// The name by which `known_hosts` pins its host key, `<name>.<region>`.
    alias: String,
    located: Located,
    kept: PathBuf,
    shared: Shared,
}

impl Reached {
    /// Reaches the controller that `settings` names: opens a connection that accepts this
    /// machine's key, enrolling the key if the controller doesn't know it, at the address where
    /// the workspace last found the controller, else where it finds it now, which starts it if it
    /// is stopped and waits until its `sshd` answers.
    ///
    /// # Errors
    ///
    /// Fails if no one live instance carries the controller's tag, if its `sshd` doesn't answer
    /// in time, or if no connection opens.
    pub(crate) fn reach(settings: &config::Controller) -> Result<Self, String> {
        let config::Controller {
            name,
            region,
            modal: _,
        } = settings;
        let kept = kept()?;
        let alias = format!("{name}.{}", region.as_str());
        let shared = Shared::new(&Workspace::find()?, &alias)?;
        let lock = shared.lock()?;
        let (located, booting) = if let Some(located) = lock.found()? {
            (located, None)
        } else {
            let (located, by) = find(region, name)?;
            (lock.keep(located)?, Some(by))
        };
        let mut reached = Self {
            region: region.clone(),
            name: name.clone(),
            alias,
            located,
            kept,
            shared,
        };
        if reached.shared.any_answers(&reached.host()) {
            return Ok(reached);
        }
        // No master answers, so the first, master 0, is closed, and can be opened.
        if let Some(by) = booting {
            return match reached.open(&lock, 0, by)? {
                Opened::Open => Ok(reached),
                Opened::Unreached(error) => Err(error),
            };
        }
        let address = reached.located.address;
        let unreached = match answers(address) {
            Err(error) => format!(
                "controller `{name}` didn't answer SSH at {address}, where it was last found: \
                 {error}"
            ),
            Ok(()) => match reached.open(&lock, 0, Deadline::after(Duration::ZERO))? {
                Opened::Open => return Ok(reached),
                Opened::Unreached(error) => error,
            },
        };
        // Stopped, or made anew: found again, so that a start, or a push of the key, goes to the
        // instance it is now.
        let again = find(region, name).and_then(|(located, by)| {
            reached.located = lock.keep(located)?;
            match reached.open(&lock, 0, by)? {
                Opened::Open => Ok(()),
                Opened::Unreached(error) => Err(error),
            }
        });
        again.map_err(|error| format!("{unreached}; finding it again: {error}"))?;
        Ok(reached)
    }
}

/// Runs `command` to its end and returns its standard output, or an error that names the command
/// as `shown` if it fails or is still running after `timeout`, when it is killed.
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

fn finished(
    output: io::Result<Option<Output>>,
    shown: &str,
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    match output {
        Ok(Some(output)) if output.status.success() => Ok(output.stdout),
        Ok(Some(output)) => {
            let said = String::from_utf8_lossy(&output.stderr);
            match said.trim() {
                "" => Err(format!("`{shown}` failed with {}", output.status)),
                said => Err(format!("`{shown}` failed with {}: {said}", output.status)),
            }
        }
        Ok(None) => Err(format!(
            "`{shown}` was still running after {} s, so it was killed",
            timeout.as_secs()
        )),
        Err(error) => Err(format!("`{shown}` didn't run: {error}: install it on PATH")),
    }
}

impl Reached {
    fn slot(&self) -> Result<Slot, String> {
        self.slot_apart(&BTreeSet::new())
    }

    /// Takes a session over a connection that isn't in `held`, so as to run beside the sessions
    /// of those, opening a connection if none is open and waiting while every one is held. When
    /// no such connection can be had, it takes a session over any.
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
                    match self.open(&lock, master, Deadline::after(Duration::ZERO)) {
                        Ok(Opened::Open) => {}
                        Ok(Opened::Unreached(_)) | Err(_) if !held.is_empty() => {
                            drop(lock);
                            say(format_args!(
                                "connection {master} to controller `{}` can't be opened, as its \
                                 log says: {}: this goes on over those it holds",
                                self.name,
                                self.shared.log(master).display()
                            ));
                            return self.slot();
                        }
                        Ok(Opened::Unreached(error)) | Err(error) => return Err(error),
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

    /// Opens connection `master`, enrolling this machine's key with the controller if it refuses
    /// it, or says that the controller may no longer be where it was found. An opening that
    /// doesn't get through is tried again every `START_POLL` until `booting` passes, as a
    /// controller that boots answers before it admits logins. The `_lock` is the one that
    /// serialises this.
    fn open(&self, _lock: &Lock, master: usize, booting: Deadline) -> Result<Opened, String> {
        // Whether the key was pushed, and whether no opening has failed to get through since, so
        // that a refusal now is one of the key that the controller was sent.
        let (mut pushed, mut just_pushed) = (false, false);
        loop {
            match self.master(master)? {
                Master::Open if pushed => return self.enrol(master).map(|()| Opened::Open),
                Master::Open => return Ok(Opened::Open),
                Master::HostKeyChanged => {
                    return Ok(Opened::Unreached(self.host_key_changed(master)));
                }
                Master::Unenrolled(error) if just_pushed => return Err(error),
                Master::Unenrolled(_) => {
                    if !pushed {
                        say(format_args!(
                            "enrolling this machine's key, {}, with controller `{}`",
                            self.kept.join("key.pub").display(),
                            self.name
                        ));
                    }
                    self.push()?;
                    (pushed, just_pushed) = (true, true);
                }
                Master::Unreached(error) if booting.remaining().is_none() => {
                    return Ok(Opened::Unreached(error));
                }
                // Pushed again if refused next, as a push lasts a minute, which a boot may outlast.
                Master::Unreached(_) => {
                    just_pushed = false;
                    thread::sleep(START_POLL);
                }
            }
        }
    }

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

    /// Starts connection `master`, which `ssh` leaves in the background once it has authenticated,
    /// until it has had no session for `PERSIST`. If it doesn't start, says whether the controller
    /// refused the key or the host key, as its log says, or, if `ssh` failed itself, that it didn't
    /// get through, and otherwise fails.
    fn master(&self, master: usize) -> Result<Master, String> {
        // The last master's log goes as this one opens.
        let log = self.shared.log(master);
        private(&log)?.set_len(0).context(log.display())?;
        let mut ssh = self.shared.command("ssh");
        // Before the options, as `ssh` takes the first value it is given of each: its log, at the
        // level that says why a connection ends, as when the server stops answering.
        let named = format!("{master}.log");
        ssh.args(["-E", &named, "-o", "LogLevel=INFO"]);
        ssh.args(self.ssh_options(master));
        let persist = format!("ControlPersist={}", PERSIST.as_secs());
        // `auto` rather than `yes`, so that `ssh` removes a socket that a killed master left, which
        // would keep it from listening, and leave it a connection of its own that never returns.
        ssh.args(["-N", "-o", "ControlMaster=auto", "-o", &persist]);
        ssh.arg(self.host());
        let output = stdx::process::output(&mut ssh, Deadline::after(SHORT_TIMEOUT));
        let failed =
            matches!(&output, Ok(Some(output)) if output.status.code() == Some(SSH_FAILED));
        let Err(error) = finished(output, "ssh", SHORT_TIMEOUT) else {
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
        if failed {
            return Ok(Master::Unreached(error));
        }
        Err(error)
    }

    /// Runs the session to its end within `timeout` and returns its standard output; an error says
    /// if the connection it went over has ended.
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

    /// Runs the session that `make` makes, and again after a pause if the connection ended with a
    /// status in `ended`, up to the pauses of `AGAIN`; `what` says in the message what is repeated.
    /// `input` goes to each run's standard input.
    ///
    /// The command must be safe to run again: a cut connection says nothing of how far it got.
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

    /// Whether `status` is one of `codes` and the connection `master` no longer answers, so that
    /// the command was cut rather than failed.
    fn ended_connection(&self, status: ExitStatus, codes: &[i32], master: usize) -> bool {
        status.code().is_some_and(|code| codes.contains(&code))
            && !self.shared.answers(master, &self.host())
    }

    /// Whether a call whose connection ended goes on again: after attempt `attempt` in a row,
    /// counted from 0 since the call last made progress, it says where the connection's log is and
    /// `what` it does again, and waits `AGAIN`'s pause. False once the pauses have run out.
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

    /// `error`, with where the log of connection `master` is if the connection has ended, which
    /// says why.
    fn lost(&self, master: usize, error: String) -> String {
        if self.shared.answers(master, &self.host()) {
            return error;
        }
        format!(
            "{error}; the connection it went over has ended, as its log says: {}",
            self.shared.log(master).display()
        )
    }

    fn ssh_options(&self, master: usize) -> Vec<String> {
        let mut words = vec!["-F".to_owned(), "/dev/null".to_owned(), "-i".to_owned()];
        words.push(self.kept.join("key").display().to_string());
        let known_hosts = self.kept.join("known_hosts").display().to_string();
        words.extend([
            "-o".to_owned(),
            format!("UserKnownHostsFile={known_hosts}"),
            "-o".to_owned(),
            // By the controller's name, rather than the instance that the tag finds, so that the
            // tag that names the controller can't lead this machine's key to another instance.
            format!("HostKeyAlias={}", self.alias),
        ]);
        let options = [
            "IdentitiesOnly=yes",
            "BatchMode=yes",
            "StrictHostKeyChecking=accept-new",
            "LogLevel=ERROR",
            // So that a connection whose other end is gone, as when this machine slept, ends within
            // minutes, rather than when TCP gives up.
            "ServerAliveInterval=30",
        ];
        for option in options {
            words.extend(["-o".to_owned(), option.to_owned()]);
        }
        words.extend([
            "-o".to_owned(),
            format!("ConnectTimeout={}", CONNECT_TIMEOUT.as_secs()),
            "-o".to_owned(),
            format!("ControlPath={master}"),
        ]);
        words
    }

    /// An `ssh` command that opens a shell on the controller, for a person to run.
    fn shell(&self) -> String {
        let key = self.kept.join("key");
        let known_hosts = format!(
            "UserKnownHostsFile={}",
            self.kept.join("known_hosts").display()
        );
        format!(
            "ssh -i {} -o {} -o HostKeyAlias={} {}",
            next::quoted(&key.to_string_lossy()),
            next::quoted(&known_hosts),
            self.alias,
            self.host()
        )
    }

    fn host(&self) -> String {
        format!("{USER}@{}", self.located.address)
    }

    fn ssh(&self, script: &str) -> Result<Session, String> {
        Ok(self.session(script, self.slot()?))
    }

    fn session(&self, script: &str, slot: Slot) -> Session {
        let mut command = self.shared.command("ssh");
        let options = self.ssh_options(slot.master);
        command.args(options).arg(self.host()).arg(script);
        Session { command, slot }
    }

    /// Runs `script` with this process's standard output and error, and `input` on its standard
    /// input if there is any, until it ends, however long that takes.
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

    /// Runs `script` and hands `line` each complete line of its standard output as it arrives.
    /// Returns how it ended, and what it wrote after its last newline.
    fn relayed(
        &self,
        script: &str,
        mut line: impl FnMut(&[u8]),
    ) -> Result<(Attached, Vec<u8>), String> {
        let mut ssh = self.ssh(script)?;
        let child = ssh.stdout(Stdio::piped()).spawn();
        let mut child = child.context("running `ssh` failed")?;
        let Some(stdout) = child.stdout.take() else {
            unreachable!("the child's standard output is missing, though it is piped just above")
        };
        let mut stdout = io::BufReader::new(stdout);
        let mut read = Vec::new();
        let relayed = loop {
            read.clear();
            match stdout.read_until(b'\n', &mut read) {
                Ok(_) if read.ends_with(b"\n") => line(&read),
                Ok(_) => break Ok(()),
                Err(error) => break Err(error),
            }
        };
        drop(stdout);
        let status = child.wait().context("running `ssh` failed")?;
        relayed.context("reading from `ssh` failed")?;
        let master = ssh.slot.master;
        Ok((Attached { status, master }, read))
    }

    /// The exit code of what `attached` ran. `ssh`'s own failure, or its end by a signal, is an
    /// error that ends with `again`, which says how to go on, and says where the connection's log
    /// is if it has ended: the command's own code never says that.
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

    /// The id of the job on the controller that `run` names, which may be a prefix of it.
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
}
