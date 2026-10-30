//! The Docker provider: a sandbox for a role as a container that needs no root to start.
//!
//! [`start`] runs the `docker` command line, as an ordinary user in the `docker` group, and
//! returns a [`Container`] whose [`Session`] is connected to the `sandboxd` inside. The
//! container has:
//!
//! - no network but loopback; the session's socket, in a host directory mounted read-only,
//!   is the only way out;
//! - the role's user from its first process, no capabilities, and `no-new-privileges`;
//! - CPU, memory (swap included) and process limits;
//! - `sandboxd`, mounted read-only, as its first process, whose output alone is the
//!   container's log, which holds at most 1 MiB;
//! - `/pumpkin`, a memory-backed filesystem owned by the role's user, holding the `in`, `out`,
//!   `home` and `log` directories the driver puts there over the session;
//! - `/tmp`, a read-only mask, so the container's writable layer is the only other place on
//!   disk a role's code can write: that layer is its workspace. Docker's `/dev/shm` and
//!   `/dev/mqueue` are writable too, but memory-backed, as `/pumpkin` is, so what they hold
//!   counts against the memory limit.
//!
//! Files move only over the session, never through the Docker daemon.
//!
//! The workspace limit is soft. A thread per container reads the writable layer's size every
//! [`WORKSPACE_POLL`] and destroys the container once it passes the role's
//! `workspace_bytes`; [`Container::workspace_breach`] then says so.
//!
//! # Limits of this provider
//!
//! - The workspace limit is advisory against hostile code. The writable layer's size misses
//!   data in files that are unlinked but still open, anonymous volumes an image declares with
//!   `VOLUME`, and inodes used up by many empty files; and a breach is seen up to one poll
//!   interval late. A hard limit needs a storage quota, which the host lacks. The platform
//!   this replaces, which evicts a pod that outgrows its volume, has gaps of the same kind.
//! - The workspace limit fails open. A measurement that fails is tried again at the next poll,
//!   so a daemon that never reports the writable layer's size as a number never enforces the
//!   limit, and nothing says so. Destroying the container instead would end sandboxes
//!   whenever the daemon is briefly unavailable.
//! - A container that dies at once still costs [`CONNECT_TIMEOUT`] before [`start`] fails.
//! - Destroying a container waits for its watcher thread, and so for an `inspect` in flight.
//!
//! Every container carries a [`RunTag`] label, so that [`list`] and [`destroy_all`] find a
//! run's containers, and only those, even when the driver that started them is gone.

use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use pumpkin_gateway::{Listener, Put, Session};
use pumpkin_schema::ir::{AbsolutePath, Name};
use pumpkin_schema::sandbox::RelativePath;
use pumpkin_schema::workflow::Sandbox;

/// How often a container's writable layer is measured.
pub const WORKSPACE_POLL: Duration = Duration::from_secs(5);

/// How long a new container has to connect its `sandboxd` to the session's socket.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How often the watcher tries to remove a container it destroys, and how long it waits
/// between tries.
const REMOVE_ATTEMPTS: u32 = 3;
const REMOVE_RETRY: Duration = Duration::from_secs(1);

/// The most the container's log may hold, in bytes, in one file. The log is kept on the
/// host's disk, outside the container's other limits, so it is capped whatever writes there,
/// with the `json-file` driver named, so that the cap doesn't rest on the daemon's default.
/// It holds the output of `sandboxd` alone, a line when it stops: its commands' output goes
/// to files, and they can't open its own, since it is non-dumpable.
const LOG_BYTES: u64 = 1 << 20;

/// The most processes and threads a container may have.
const PIDS_LIMIT: u32 = 4096;

/// The size of `/pumpkin`, which counts against the container's memory limit while in use.
const PUMPKIN_TMPFS: &str = "1g";

/// Where `sandboxd` is mounted in the container, and where the session's socket directory is.
const SANDBOXD_PATH: &str = "/.pumpkin/sandboxd";
const GATEWAY_DIR: &str = "/.pumpkin/gateway";

/// The socket's name in its directory.
const SOCKET_NAME: &str = "s";

/// The label that carries a container's run tag.
const RUN_LABEL: &str = "pumpkin.run";

/// The directories the driver expects below `/pumpkin`.
const DIRS: [&str; 4] = ["in", "out", "home", "log"];

/// Names the containers of one run, for finding them again: 1 to 63 characters from a-z, 0-9
/// and `-`, starting with a letter or digit.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RunTag(String);

impl RunTag {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RunTag {
    type Error = String;

    fn try_from(tag: String) -> Result<Self, Self::Error> {
        let mut chars = tag.chars();
        let valid = tag.len() <= 63
            && chars
                .next()
                .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
            && chars.all(|char| char.is_ascii_lowercase() || char.is_ascii_digit() || char == '-');
        if valid {
            Ok(Self(tag))
        } else {
            Err(format!(
                "`{}` is not a valid run tag: use 1 to 63 characters from a-z, 0-9 and `-`, \
                 starting with a letter or digit",
                tag.escape_debug()
            ))
        }
    }
}

/// What [`start`] needs to start a container.
#[derive(Clone, Copy, Debug)]
pub struct Start<'a> {
    /// The run the container belongs to.
    pub run: &'a RunTag,
    /// The container's name, which must not be taken.
    pub name: &'a Name,
    /// The role's image, user and resources.
    pub sandbox: &'a Sandbox,
    /// A static `sandboxd` binary, which the role's user can execute.
    pub sandboxd: &'a Path,
    /// A directory for the session's socket, which only this container sees. Unix socket paths
    /// are short, so keep it short too.
    pub socket_dir: &'a Path,
}

/// A sandbox's container and the session with the `sandboxd` inside.
///
/// Dropping it, or calling [`destroy`](Self::destroy), ends the session and removes the
/// container.
#[derive(Debug)]
pub struct Container {
    id: String,
    name: Name,
    session: Arc<Session>,
    breach: Arc<OnceLock<WorkspaceBreach>>,
    watcher: Option<Watcher>,
    removed: bool,
}

/// The thread that measures a container's workspace, and the way to stop it.
#[derive(Debug)]
struct Watcher {
    stop: Sender<()>,
    thread: JoinHandle<()>,
}

/// A container destroyed for outgrowing its workspace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkspaceBreach {
    /// The writable layer's size when it was measured, in bytes.
    pub used: u64,
    /// The role's `workspace_bytes`.
    pub limit: u64,
}

impl Container {
    /// The container's ID.
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn name(&self) -> &Name {
        &self.name
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Whether the container was destroyed for outgrowing its workspace. When the session
    /// ends and this is `Some`, that is why.
    pub fn workspace_breach(&self) -> Option<WorkspaceBreach> {
        self.breach.get().copied()
    }

    /// Ends the session and removes the container, reporting a failure to remove it.
    ///
    /// # Errors
    ///
    /// Fails if `docker` can't remove the container.
    pub fn destroy(mut self) -> io::Result<()> {
        self.stop_watching();
        self.teardown()
    }

    fn stop_watching(&mut self) {
        // Dropping the sender wakes the watcher, which then returns. Stopping it before the
        // container goes keeps a destroyed container from also being blamed for its workspace.
        if let Some(Watcher { stop, thread }) = self.watcher.take() {
            drop(stop);
            #[expect(
                clippy::unreachable,
                reason = "a panic is a bug, which ends the whole process under `panic = \
                          \"abort\"`, so `join` sees one only in tests, which unwind"
            )]
            let Ok(()) = thread.join() else {
                unreachable!("the watcher of {} panicked", self.id)
            };
        }
    }

    fn teardown(&mut self) -> io::Result<()> {
        if std::mem::replace(&mut self.removed, true) {
            return Ok(());
        }
        self.session.end();
        remove(&self.id)
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        self.stop_watching();
        // Best effort: `destroy` is the way to learn that removing failed.
        let _ = self.teardown();
    }
}

/// Starts a container for the role, connects its session, and puts the directories `in`, `out`,
/// `home` and `log` under `/pumpkin`.
///
/// # Errors
///
/// Fails if `docker` can't create or start the container, if its `sandboxd` doesn't connect
/// within [`CONNECT_TIMEOUT`], or if the directories can't be put. Whatever container `docker`
/// created is removed first.
pub fn start(start: &Start<'_>) -> io::Result<Container> {
    let socket = start.socket_dir.join(SOCKET_NAME);
    let listener = Listener::bind(socket)?;
    // Creating and starting apart gives the container's ID before anything can fail with a
    // container left behind, so a failure removes this container by ID, and never another
    // that has its name.
    let id = docker(&create_args(start)?)?.trim().to_owned();
    let started = docker(["start", &id]).and_then(|_| connect(listener, &id));
    match started {
        Ok(session) => Ok(spawn_watcher(start, id, session)),
        Err(error) => {
            let logs = log_tail(&id).unwrap_or_default();
            let _ = remove(&id);
            let message = match logs.trim() {
                "" => error.to_string(),
                logs => format!("{error}; the container's logs end: {logs}"),
            };
            Err(io::Error::new(error.kind(), message))
        }
    }
}

fn connect(listener: Listener, id: &str) -> io::Result<Session> {
    let session = listener.accept(CONNECT_TIMEOUT)?;
    let root = AbsolutePath::try_from("/pumpkin".to_owned());
    let dirs = DIRS.map(|dir| RelativePath::try_from(dir.to_owned()));
    #[expect(
        clippy::unreachable,
        reason = "`/pumpkin` is an absolute path, and each of `DIRS` one component, neither \
                  empty, `.` nor `..`"
    )]
    let (Ok(root), Ok(dirs)) = (root, dirs.into_iter().collect::<Result<Vec<_>, _>>()) else {
        unreachable!("`/pumpkin` or one of {DIRS:?} isn't a valid path")
    };
    let puts: Vec<Put<'_>> = dirs.iter().map(Put::Dir).collect();
    session
        .put(&root, &puts)
        .map_err(|error| io::Error::new(error.kind(), format!("{id}: {error}")))?;
    Ok(session)
}

fn spawn_watcher(start: &Start<'_>, id: String, session: Session) -> Container {
    let breach = Arc::new(OnceLock::new());
    let session = Arc::new(session);
    let (stop, stopped) = mpsc::channel();
    let watched = Watched {
        id: id.clone(),
        limit: start.sandbox.resources.workspace_bytes.get(),
        session: Arc::clone(&session),
        breach: Arc::clone(&breach),
    };
    let thread = thread::spawn(move || watch(&watched, &stopped));
    Container {
        id,
        name: start.name.clone(),
        session,
        breach,
        watcher: Some(Watcher { stop, thread }),
        removed: false,
    }
}

/// What the watcher thread of a container shares with it.
struct Watched {
    id: String,
    limit: u64,
    session: Arc<Session>,
    breach: Arc<OnceLock<WorkspaceBreach>>,
}

/// Measures the container's writable layer until `stopped` disconnects or the layer passes the
/// limit, which destroys the container.
///
/// The breach is recorded, then the session ends, and only then is the container removed: so a
/// caller whose session fails and who then looks for a breach finds it, and can't use the
/// session in between. A removal that keeps failing is left to [`Container::destroy`], which
/// tries again; the session has ended, which stops `sandboxd` meanwhile.
fn watch(watched: &Watched, stopped: &Receiver<()>) {
    let Watched {
        id,
        limit,
        session,
        breach,
    } = watched;
    while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(WORKSPACE_POLL) {
        // A failed measurement is retried: the daemon may be busy, or the container gone.
        let Ok(used) = workspace_bytes(id) else {
            continue;
        };
        if used > *limit {
            #[expect(
                clippy::unreachable,
                reason = "only this thread records the breach, and it returns right after"
            )]
            let Ok(()) = breach.set(WorkspaceBreach {
                used,
                limit: *limit,
            }) else {
                unreachable!("{id}'s workspace breach was recorded twice")
            };
            session.end();
            for _ in 0..REMOVE_ATTEMPTS {
                if remove(id).is_ok() {
                    break;
                }
                thread::sleep(REMOVE_RETRY);
            }
            return;
        }
    }
}

fn workspace_bytes(id: &str) -> io::Result<u64> {
    let size = docker([
        "container",
        "inspect",
        "--size",
        "--format",
        "{{.SizeRw}}",
        id,
    ])?;
    size.trim().parse().map_err(|_| {
        io::Error::other(format!(
            "docker reported the size of {id}'s writable layer as `{}`: expected a number of bytes",
            size.trim()
        ))
    })
}

/// The arguments of `docker create` for the container.
fn create_args(start: &Start<'_>) -> io::Result<Vec<String>> {
    let Start {
        run,
        name,
        sandbox,
        sandboxd,
        socket_dir,
    } = *start;
    let Sandbox {
        image,
        user,
        resources,
        ..
    } = sandbox;
    let (uid, gid) = (user.uid, user.gid);
    let millis = resources.cpu_millis.get();
    let memory = resources.memory_bytes.get();
    Ok(vec![
        "create".to_owned(),
        format!("--name={name}"),
        format!("--label={RUN_LABEL}={}", run.as_str()),
        "--network=none".to_owned(),
        format!("--user={uid}:{gid}"),
        "--cap-drop=ALL".to_owned(),
        "--security-opt=no-new-privileges".to_owned(),
        format!("--cpus={}.{:03}", millis / 1000, millis % 1000),
        format!("--memory={memory}"),
        format!("--memory-swap={memory}"),
        "--log-driver=json-file".to_owned(),
        format!("--log-opt=max-size={LOG_BYTES}"),
        "--log-opt=max-file=1".to_owned(),
        format!("--pids-limit={PIDS_LIMIT}"),
        format!("--tmpfs=/pumpkin:exec,uid={uid},gid={gid},mode=0755,size={PUMPKIN_TMPFS}"),
        "--tmpfs=/tmp:ro,size=4k".to_owned(),
        bind_mount(sandboxd, SANDBOXD_PATH)?,
        bind_mount(socket_dir, GATEWAY_DIR)?,
        format!("--entrypoint={SANDBOXD_PATH}"),
        "--".to_owned(),
        image.to_string(),
        format!("{GATEWAY_DIR}/{SOCKET_NAME}"),
    ])
}

/// A read-only `--mount` of a host path, which must exist.
fn bind_mount(host: &Path, container: &str) -> io::Result<String> {
    let host = host.canonicalize()?;
    let host = host
        .to_str()
        .filter(|path| !path.contains(','))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} can't be mounted: use a path of UTF-8 without commas",
                    host.display()
                ),
            )
        })?;
    Ok(format!(
        "--mount=type=bind,source={host},target={container},readonly"
    ))
}

/// The IDs of the run's containers, running or not.
///
/// # Errors
///
/// Fails if `docker` can't list them.
pub fn list(run: &RunTag) -> io::Result<Vec<String>> {
    let filter = format!("label={RUN_LABEL}={}", run.as_str());
    let ids = docker([
        "container",
        "ls",
        "--all",
        "--quiet",
        "--no-trunc",
        "--filter",
        &filter,
    ])?;
    Ok(ids.lines().map(str::to_owned).collect())
}

/// Removes every container of the run, and returns their IDs. Containers of other runs stay.
///
/// # Errors
///
/// Fails if `docker` can't list them, or can't remove one; the others are removed regardless.
pub fn destroy_all(run: &RunTag) -> io::Result<Vec<String>> {
    let ids = list(run)?;
    let failures: Vec<String> = ids
        .iter()
        .filter_map(|id| remove(id).err().map(|error| error.to_string()))
        .collect();
    if failures.is_empty() {
        Ok(ids)
    } else {
        Err(io::Error::other(failures.join("; ")))
    }
}

/// Removes the container with ID `id`, and its anonymous volumes. A container already gone is
/// removed.
fn remove(id: &str) -> io::Result<()> {
    match docker(["rm", "--force", "--volumes", id]) {
        Err(error) if error.to_string().contains("No such container") => Ok(()),
        other => other.map(|_| ()),
    }
}

/// The Docker Engine's version, for the run record.
///
/// # Errors
///
/// Fails if `docker` can't reach the engine.
pub fn engine_version() -> io::Result<String> {
    let version = docker(["version", "--format", "{{.Server.Version}}"])?;
    Ok(version.trim().to_owned())
}

/// The last lines of the container's log: those of `sandboxd`'s standard output, then those of
/// its standard error, which `docker logs` writes to its own two streams.
fn log_tail(id: &str) -> io::Result<String> {
    let Output { stdout, stderr, .. } = docker_output(["logs", "--tail", "20", id])?;
    Ok(String::from_utf8_lossy(&[stdout, stderr].concat()).into_owned())
}

/// Runs `docker` with `args` and returns its standard output.
fn docker<I, S>(args: I) -> io::Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = docker_output(args)?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Runs `docker` with `args` and returns its output, or fails with its standard error.
fn docker_output<I, S>(args: I) -> io::Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<S> = args.into_iter().collect();
    let output = Command::new("docker")
        .args(&args)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| io::Error::new(error.kind(), format!("docker didn't run: {error}")))?;
    if output.status.success() {
        Ok(output)
    } else {
        let mut shown = String::new();
        for arg in args.iter().take(2) {
            shown.push(' ');
            shown.push_str(&arg.as_ref().to_string_lossy());
        }
        Err(io::Error::other(format!(
            "`docker{shown}` failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}
