//! The Docker provider: a sandbox for a role as a container that needs no root to start.
//!
//! [`Engine::start`] runs the `docker` command line, as an ordinary user, against the engine of
//! the operator's [context](#the-envelope), and returns a [`Container`] whose [`Session`] is
//! connected to the `sandboxd` inside. The container has:
//!
//! - no network but loopback; the session's socket, in a host directory mounted read-only,
//!   is the only way out;
//! - the role's user from its first process, no capabilities, and `no-new-privileges`;
//! - CPU, memory (swap included) and process limits, and a core file size limit of 0;
//! - `sandboxd`, mounted read-only, as its first process, whose output alone is the
//!   container's log, which holds at most 1 MiB;
//! - [`/pumpkin`](pumpkin_dir), a tmpfs owned by the role's user, holding the
//!   [directories](pumpkin_subdirs) the driver puts there over the session. It holds at most
//!   [`PUMPKIN_BYTES`], and its files are in memory, counted against the role's memory limit,
//!   so a role can keep there no more than that limit, less what its processes use;
//! - `/tmp`, a read-only mask. On disk, a role's code can write only to the container's
//!   writable layer, which is its workspace, and to the anonymous volume Docker makes for each
//!   `VOLUME` its image declares, which is removed with the container. Neither has a size
//!   limit, so a role can fill the disk the Docker daemon stores containers on. Docker's
//!   `/dev/shm` and `/dev/mqueue` are writable too, but memory-backed, as `/pumpkin` is, so
//!   what they hold counts against the memory limit;
//! - for a role that makes ops ([`Start::ops`]), [`sandboxctl`](SANDBOXCTL_PATH), mounted
//!   read-only, and `sandboxd` serving ops; any other sandbox is
//!   [passive](pumpkin_schema::sandbox).
//!
//! Files move only over the session, never through the Docker daemon.
//!
//! # The envelope
//!
//! The operator provides the engine, once, as a Docker context, which they name; the provider
//! picks up none of its own, and nothing in its environment overrides the one named
//! ([`ContextName`]). [`Engine::connect`] reports the engine before anything starts
//! ([`Report`]). Within it, the provider creates only containers, known by their IDs, and the
//! anonymous volumes Docker makes for their images' `VOLUME`s, which are removed with them.
//! Each container is labelled with its run's [`RunTag`], so that [`Engine::destroy_all`] finds
//! a run's containers, and only those, even when the driver that started them is gone, and
//! with its role's name, so that `docker ps` shows which is which. The provider runs on a
//! rootful engine or a rootless one, and its report says which ([`Security::rootless`]).

use std::env;
use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use pumpkin_gateway::{Listener, Put, Session};
use pumpkin_schema::ir::{Resources, User};
use pumpkin_schema::sandbox::{
    OpPayloadLimit, PUMPKIN_BYTES, SANDBOXCTL_PATH, pumpkin_dir, pumpkin_subdirs,
};
use pumpkin_schema::workflow::Sandbox;
use serde::Deserialize;
use stdx::Context;

/// How long a new container has to connect its `sandboxd` to the session's socket.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// The most the container's log may hold, in bytes, in one file. The log is kept on the
/// host's disk, outside the container's other limits, so it is capped whatever writes there,
/// with the `json-file` driver named, so that the cap doesn't rest on the daemon's default.
/// It holds the output of `sandboxd` alone, which is a line if it fails: its commands' output
/// goes to files, and they can't open its own, since it is non-dumpable.
const LOG_BYTES: u64 = 1 << 20;

/// The most processes and threads a container may have.
const PIDS_LIMIT: u32 = 4096;

/// Where `sandboxd` is mounted in the container.
const SANDBOXD_PATH: &str = "/.pumpkin/sandboxd";

/// Where the session's socket directory is mounted in the container.
const GATEWAY_DIR: &str = "/.pumpkin/gateway";

/// The socket's name in its directory.
const SOCKET_NAME: &str = "s";

/// The label that carries a container's run tag.
const RUN_LABEL: &str = "pumpkin.run";

/// The label that carries the name of a container's role.
const ROLE_LABEL: &str = "pumpkin.role";

/// The name of a Docker context, which says what engine `docker` reaches, and how. `default`
/// is Docker's own context, which reaches the engine at Docker's default socket.
///
/// Any name but the empty one, which `docker` would take for no name, and so reach the engine
/// of the command line's current context. A name no context has is refused by
/// [`Engine::connect`].
#[derive(Clone, Debug)]
pub struct ContextName(String);

impl ContextName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ContextName {
    type Error = String;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        if name.is_empty() {
            Err(
                "the Docker context's name is empty: name one that `docker context ls` lists"
                    .to_owned(),
            )
        } else {
            Ok(Self(name))
        }
    }
}

/// The Docker engine a context reaches, which every container of the provider's runs in, and
/// its report.
#[derive(Debug)]
pub struct Engine {
    report: Report,
}

/// The engine, as `docker` describes it before anything starts.
#[derive(Debug)]
pub struct Report {
    /// The context `docker` reaches the engine through.
    pub context: ContextName,
    /// The endpoint the context reaches the engine at, such as `unix:///var/run/docker.sock`.
    pub endpoint: String,
    /// The version of the `docker` command line.
    pub client_version: String,
    /// The engine's version.
    pub engine_version: String,
    /// What the engine's security options say.
    pub security: Security,
    /// The version of the kernel of the engine's host.
    pub kernel: String,
    /// The operating system of the engine's host.
    pub os: String,
}

/// What the engine's security options say.
#[derive(Debug)]
pub struct Security {
    /// Whether the engine runs as an ordinary user rather than as root.
    // Rootless Docker is the setup to choose: its engine, and so every container, runs as an
    // ordinary user, so escaping a container gains no more than that user has. A rootful engine
    // runs as root, and whoever may use its socket, as the `docker` group may, can make it do
    // anything root can.
    pub rootless: bool,
    /// The seccomp profile containers get unless told otherwise, `builtin` for Docker's own,
    /// or `None` if the engine has no seccomp.
    pub seccomp_profile: Option<String>,
    /// Whether the engine confines containers with `AppArmor`.
    pub apparmor: bool,
    /// Whether the engine maps containers' users into a user namespace of their own.
    pub userns: bool,
}

/// What `docker info` prints, as JSON.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
#[expect(
    clippy::struct_excessive_bools,
    reason = "`docker info` reports whether the engine supports each limit as a boolean"
)]
struct InfoJson {
    /// Whether the engine supports memory limits, which [`create_args`] sets.
    memory_limit: bool,
    /// Whether the engine supports swap limits, which [`create_args`] sets.
    swap_limit: bool,
    /// Whether the engine supports CPU limits, which [`create_args`] sets.
    cpu_cfs_quota: bool,
    /// Whether the engine supports process limits, which [`create_args`] sets.
    pids_limit: bool,
    security_options: Option<Vec<String>>,
    kernel_version: String,
    operating_system: String,
    /// The engine's version.
    server_version: String,
    /// The `docker` command line's version, among what it says of itself.
    client_info: ClientInfoJson,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ClientInfoJson {
    version: String,
}

/// The value of the label that finds a run's containers: every container of the run carries
/// it, and [`Engine::destroy_all`] removes the containers that do.
#[derive(Debug)]
pub struct RunTag(String);

impl RunTag {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for RunTag {
    fn from(tag: String) -> Self {
        Self(tag)
    }
}

/// What [`Engine::start`] needs to start a container.
#[derive(Debug)]
pub struct Start<'a> {
    /// The run the container belongs to.
    pub run: &'a RunTag,
    /// The role's name, image, user and resources. The image must be in the engine already: the
    /// provider never pulls one.
    pub sandbox: &'a Sandbox,
    /// A static `sandboxd` binary, which the role's user can execute.
    pub sandboxd: &'a Path,
    /// A directory for the session's socket, which only this container sees. Its parent must
    /// be private to the driver's user, since the socket accepts the first connection, whoever
    /// makes it. The container enters at the directory's mount itself, so the role's user needs
    /// to traverse only the directory, not its parent. Unix socket paths are short, so keep it
    /// short too.
    pub socket_dir: &'a Path,
    /// For a role whose commands make ops through `sandboxctl`, as an agent's do, the most
    /// bytes an op's payload may hold, which [`Engine::start`] accepts the session with
    /// ([`Listener::accept`]). `None` for a [passive](pumpkin_schema::sandbox) sandbox.
    pub ops: Option<OpPayloadLimit>,
}

/// A sandbox's container and the session with the `sandboxd` inside.
///
/// Dropping it, or calling [`destroy`](Self::destroy), removes the container and ends the
/// session.
#[derive(Debug)]
pub struct Container {
    context: ContextName,
    id: String,
    session: Session,
    removed: bool,
}

impl Container {
    /// The container's ID.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The session with the container's `sandboxd`, which the container's owner drives.
    pub fn session(&mut self) -> &mut Session {
        &mut self.session
    }

    /// Removes the container and ends the session, reporting a failure to remove the
    /// container.
    ///
    /// # Errors
    ///
    /// Fails if `docker` can't remove the container.
    pub fn destroy(mut self) -> Result<(), String> {
        self.teardown().map_err(fix_reported)
    }

    /// Removes the container, once. The session ends as the container drops it.
    fn teardown(&mut self) -> Result<(), String> {
        if std::mem::replace(&mut self.removed, true) {
            return Ok(());
        }
        remove(&self.context, &self.id)
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        // Best effort: `destroy` is the way to learn that removing failed.
        let _ = self.teardown();
    }
}

impl Engine {
    /// Reaches the engine through `context`, and reports it. Nothing is started.
    ///
    /// # Errors
    ///
    /// Fails if the context can't be read, or the engine can't be reached through it, saying
    /// what to do; if `docker` describes the engine in JSON of another form; or if the engine
    /// can't enforce the CPU, memory, swap and process limits every container has.
    pub fn connect(context: ContextName) -> Result<Self, String> {
        let name = context.as_str();
        let template = "{{.Endpoints.docker.Host}}";
        // After `--`, a name that begins with `-` can't be taken for an option.
        let inspect = ["context", "inspect", "--format", template, "--", name];
        let inspected = docker(&context, inspect).map_err(|error| {
            format!(
                "the Docker context `{name}` can't be read: {error}: name one that \
                 `docker context ls` lists"
            )
        })?;
        let endpoint = String::from_utf8_lossy(&inspected.stdout).trim().to_owned();
        let info = docker(&context, ["info", "--format", "{{json .}}"]).map_err(|error| {
            let todo = if error.contains("permission denied") {
                "let this user reach the engine's socket: by the `docker` group, which is \
                 root-equivalent, or by running rootless Docker and naming its context"
            } else {
                "start the engine, or name a context whose engine runs"
            };
            format!("the Docker engine isn't reachable through context `{name}`: {error}: {todo}")
        })?;
        let InfoJson {
            memory_limit,
            swap_limit,
            cpu_cfs_quota,
            pids_limit,
            security_options,
            kernel_version,
            operating_system,
            server_version,
            client_info,
        } = serde_json::from_slice(&info.stdout).map_err(|error| {
            format!(
                "the JSON `docker info` printed isn't of the form expected: {error}: use \
                 Docker's own command line, which prints it"
            )
        })?;
        check_limits(
            &context,
            [
                ("MemoryLimit", memory_limit),
                ("SwapLimit", swap_limit),
                ("CpuCfsQuota", cpu_cfs_quota),
                ("PidsLimit", pids_limit),
            ],
        )?;
        Ok(Self {
            report: Report {
                context,
                endpoint,
                client_version: client_info.version,
                engine_version: server_version,
                security: Security::parse(&security_options.unwrap_or_default()),
                kernel: kernel_version,
                os: operating_system,
            },
        })
    }

    /// The engine, as it was when [`connect`](Self::connect) reached it.
    pub fn report(&self) -> &Report {
        &self.report
    }

    /// Starts a container for the role, connects its session, which takes ops within
    /// [`Start::ops`]'s limit, if it gives one, and puts [`pumpkin_subdirs()`] under
    /// [`pumpkin_dir()`].
    ///
    /// # Errors
    ///
    /// Fails, naming the path, if the session's socket can't be made in the socket directory,
    /// or that directory or `sandboxd` can't be mounted; if `docker` can't create or start the
    /// container; if its `sandboxd` doesn't connect within 30 s, which a `sandboxd` that exits
    /// at once still costs; or if the directories can't be put. Whatever container `docker`
    /// created is removed before the error returns, which quotes the end of the container's
    /// logs, and says if removing it failed.
    pub fn start(&self, start: &Start<'_>) -> Result<Container, String> {
        let context = &self.report.context;
        let socket = start.socket_dir.join(SOCKET_NAME);
        let listener = Listener::bind(socket.clone()).map_err(|error| {
            format!(
                "the session's socket can't be made at {}: {error}: expected a directory that \
                 exists, which this user can write to, by a path short enough for a Unix \
                 socket, and nothing at `{SOCKET_NAME}`, where a driver that was killed leaves \
                 its socket",
                socket.display()
            )
        })?;
        // Creating and starting apart gives the container's ID before anything can fail with
        // a container left behind, so a failure removes this container by its ID.
        let created = docker(context, &create_args(start)?).map_err(fix_reported)?;
        let id = String::from_utf8_lossy(&created.stdout).trim().to_owned();
        let started = docker(context, ["start", &id]).map_err(fix_reported);
        match started.and_then(|_| connect(listener, &id, start.ops)) {
            Ok(session) => Ok(Container {
                context: context.clone(),
                id,
                session,
                removed: false,
            }),
            Err(error) => {
                let mut parts = vec![error];
                match log_tail(context, &id).context("the container's logs couldn't be read") {
                    Ok(logs) if logs.trim().is_empty() => {}
                    Ok(logs) => parts.push(format!("the container's logs end: {}", logs.trim())),
                    Err(error) => parts.push(error),
                }
                let left = format_args!("container {id} is left behind, as removing it failed");
                if let Err(removal) = remove(context, &id).context(left) {
                    parts.push(removal);
                }
                Err(parts.join("; "))
            }
        }
    }

    /// Removes every container of the run, and returns their IDs. Containers of other runs
    /// stay.
    ///
    /// # Errors
    ///
    /// Fails if `docker` can't list them, or can't remove one; the others are removed
    /// regardless.
    pub fn destroy_all(&self, run: &RunTag) -> Result<Vec<String>, String> {
        let context = &self.report.context;
        // The IDs of the run's containers, running or not.
        let filter = format!("label={RUN_LABEL}={}", run.as_str());
        let ls = [
            "container",
            "ls",
            "--all",
            "--quiet",
            "--no-trunc",
            "--filter",
            &filter,
        ];
        let listed = docker(context, ls).map_err(fix_reported)?;
        let ids = String::from_utf8_lossy(&listed.stdout);
        let ids: Vec<String> = ids.lines().map(str::to_owned).collect();
        let failures: Vec<String> = (ids.iter())
            .filter_map(|id| remove(context, id).err())
            .collect();
        if failures.is_empty() {
            Ok(ids)
        } else {
            Err(fix_reported(failures.join("; ")))
        }
    }
}

impl Security {
    /// The security options `docker info` lists, each a list of `key=value`, such as
    /// `name=seccomp,profile=builtin`.
    fn parse(options: &[String]) -> Self {
        let mut security = Self {
            rootless: false,
            seccomp_profile: None,
            apparmor: false,
            userns: false,
        };
        for option in options {
            let mut name = None;
            let mut profile = None;
            for pair in option.split(',') {
                match pair.split_once('=') {
                    Some(("name", value)) => name = Some(value),
                    Some(("profile", value)) => profile = Some(value),
                    _ => {}
                }
            }
            match name {
                Some("rootless") => security.rootless = true,
                Some("seccomp") => {
                    security.seccomp_profile = Some(profile.unwrap_or_default().to_owned());
                }
                Some("apparmor") => security.apparmor = true,
                Some("userns") => security.userns = true,
                _ => {}
            }
        }
        security
    }
}

/// Fails unless the engine of `context` supports each of `limits`, a name in `docker info`
/// and whether it reports the limit supported. Those are the limits [`create_args`] sets, and
/// the engine discards one it doesn't support with only a warning from `docker create`, so a
/// container would start without it.
fn check_limits(context: &ContextName, limits: [(&str, bool); 4]) -> Result<(), String> {
    let unsupported: Vec<&str> = (limits.iter())
        .filter_map(|&(name, supported)| (!supported).then_some(name))
        .collect();
    if unsupported.is_empty() {
        return Ok(());
    }
    Err(format!(
        "the Docker engine of context `{}` can't enforce every limit a sandbox has: \
         `docker info` reports {} false: use an engine that reports MemoryLimit, SwapLimit, \
         CpuCfsQuota and PidsLimit true",
        context.as_str(),
        unsupported.join(", ")
    ))
}

/// Accepts the session of container `id`'s `sandboxd`, which takes ops within `ops`, the
/// payload limit, if any, and puts the directories a role expects.
fn connect(listener: Listener, id: &str, ops: Option<OpPayloadLimit>) -> Result<Session, String> {
    let mut session = listener.accept(CONNECT_TIMEOUT, ops).map_err(|error| {
        format!(
            "the `sandboxd` of container {id} didn't connect: {error}: expected it to connect \
             as it starts"
        )
    })?;
    let (root, subdirs) = (pumpkin_dir(), pumpkin_subdirs());
    let puts: Vec<Put<'_>> = subdirs.iter().map(Put::Dir).collect();
    session.put(&root, &puts).map_err(|error| {
        format!(
            "the directories under `{root}` can't be put in container {id}: {error}: expected \
             its `sandboxd` to put them, as the role's user, who owns `{root}`"
        )
    })?;
    Ok(session)
}

/// The arguments of `docker create` for the container.
fn create_args(start: &Start<'_>) -> Result<Vec<String>, String> {
    let &Start {
        run,
        sandbox,
        sandboxd,
        socket_dir,
        ops,
    } = start;
    let Sandbox {
        name,
        image,
        user,
        resources,
        model_route: _,
    } = sandbox;
    let User { uid, gid } = *user;
    let Resources {
        cpu_millis,
        memory_bytes,
    } = *resources;
    let (millis, memory) = (cpu_millis.get(), memory_bytes.get());
    let mut args = vec![
        "create".to_owned(),
        format!("--label={RUN_LABEL}={}", run.as_str()),
        format!("--label={ROLE_LABEL}={name}"),
        "--network=none".to_owned(),
        format!("--user={uid}:{gid}"),
        "--cap-drop=ALL".to_owned(),
        "--security-opt=no-new-privileges".to_owned(),
        // The daemon's `init` setting would otherwise make `docker-init` the first process, in
        // place of `sandboxd`.
        "--init=false".to_owned(),
        // An image's `HEALTHCHECK` would otherwise run as the role's user outside `sandboxd`,
        // after freezing too, with its output kept on the host.
        "--no-healthcheck".to_owned(),
        // Images are the operator's or the build's: a pull would fetch one that no run owns,
        // with the operator's registry credentials.
        "--pull=never".to_owned(),
        format!("--cpus={}.{:03}", millis / 1000, millis % 1000),
        format!("--memory={memory}"),
        format!("--memory-swap={memory}"),
        "--log-driver=json-file".to_owned(),
        format!("--log-opt=max-size={LOG_BYTES}"),
        "--log-opt=max-file=1".to_owned(),
        format!("--pids-limit={PIDS_LIMIT}"),
        // A crash would otherwise dump core where the host's `core_pattern` says: into the
        // container's own files, such as its writable layer, or to a helper that may store it
        // on the host's disk, outside the session and every limit. A helper still runs, but is
        // told the limit is 0.
        "--ulimit=core=0".to_owned(),
        format!(
            "--tmpfs={}:exec,uid={uid},gid={gid},mode=0755,size={PUMPKIN_BYTES}",
            pumpkin_dir()
        ),
        "--tmpfs=/tmp:ro".to_owned(),
        bind_mount(sandboxd, SANDBOXD_PATH)?,
        bind_mount(socket_dir, GATEWAY_DIR)?,
    ];
    if ops.is_some() {
        args.push(bind_mount(sandboxd, SANDBOXCTL_PATH)?);
    }
    args.extend([
        format!("--entrypoint={SANDBOXD_PATH}"),
        "--".to_owned(),
        image.to_string(),
        format!("{GATEWAY_DIR}/{SOCKET_NAME}"),
    ]);
    if ops.is_some() {
        args.push("--ops".to_owned());
    }
    Ok(args)
}

/// A read-only `--mount` of `host`, which must exist, at `container`.
fn bind_mount(host: &Path, container: &str) -> Result<String, String> {
    let host = host.canonicalize().map_err(|error| {
        format!(
            "{} can't be mounted at {container}: {error}: name a path that exists",
            host.display()
        )
    })?;
    let Some(source) = host.to_str().filter(|path| !path.contains(',')) else {
        return Err(format!(
            "{} can't be mounted at {container}: name a path of UTF-8 without commas",
            host.display()
        ));
    };
    Ok(format!(
        "--mount=type=bind,source={source},target={container},readonly"
    ))
}

/// Removes the container with ID `id`, and its anonymous volumes. `--force` kills the container
/// first if it runs. A container already gone is removed.
fn remove(context: &ContextName, id: &str) -> Result<(), String> {
    match docker(context, ["rm", "--force", "--volumes", id]) {
        // Docker 29 exits 0 here, but no minimum version is pinned, and older engines fail.
        Err(error) if error.contains("No such container") => Ok(()),
        other => other.map(|_| ()),
    }
}

/// The last lines of the container's log: those of `sandboxd`'s standard output, then those of
/// its standard error, which `docker logs` writes to its own two streams.
fn log_tail(context: &ContextName, id: &str) -> Result<String, String> {
    let logs = ["logs", "--tail", "20", id];
    let Output {
        status: _,
        stdout,
        stderr,
    } = docker(context, logs)?;
    Ok(String::from_utf8_lossy(&[stdout, stderr].concat()).into_owned())
}

/// `error`, from a `docker` command that failed for a reason the provider doesn't foresee, and
/// what to do about it: Docker's error says what is wrong.
fn fix_reported(error: String) -> String {
    error + ": fix what Docker reports"
}

/// Runs `docker` against `context` alone with `args`, and returns its output. If the command
/// fails, the error says which it was and what Docker reported, and the caller adds what that
/// means and what to do; if `docker` doesn't run, the error asks for it on `PATH`.
///
/// `docker` keeps only what it needs to find Docker's own configuration: `HOME`, for
/// `~/.docker` and the configuration, contexts and CLI plugins there; `PATH`, for credential
/// helpers; and `DOCKER_CONFIG`, if set, Docker's own variable for where that configuration
/// lives. The rest of the environment is dropped, so that the context named alone steers the
/// command line: otherwise `DOCKER_HOST`, `DOCKER_TLS_VERIFY` and `DOCKER_CERT_PATH`, among
/// others, would set the endpoint and TLS of `default`. A context reached over `ssh://` would
/// need `SSH_AUTH_SOCK` too, which isn't passed.
fn docker<I, S>(context: &ContextName, args: I) -> Result<Output, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<S> = args.into_iter().collect();
    let kept = ["HOME", "PATH", "DOCKER_CONFIG"]
        .into_iter()
        .filter_map(|name| Some((name, env::var_os(name)?)));
    let output = Command::new("docker")
        .env_clear()
        .envs(kept)
        // One argument, so that a name that begins with `-` can't be taken for an option.
        .arg(format!("--context={}", context.as_str()))
        .args(&args)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| {
            format!("`docker` didn't run: {error}: install Docker's command line on `PATH`")
        })?;
    if output.status.success() {
        Ok(output)
    } else {
        // The command, such as `container ls`, and its argument, as in `start <id>`, but none of
        // its options, which can be long.
        let mut shown = String::new();
        let words = args.iter().map(|arg| arg.as_ref().to_string_lossy());
        for word in words.take(2).take_while(|word| !word.starts_with('-')) {
            shown.push(' ');
            shown.push_str(&word);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "`docker{shown}` failed with {}: {}",
            output.status,
            stderr.trim()
        ))
    }
}
