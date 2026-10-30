//! The Docker provider: a sandbox for a role as a container that needs no root to start.
//!
//! [`Engine::start`] runs the `docker` command line, as an ordinary user, against the engine of
//! the operator's [context](#the-envelope), and returns a [`Container`] whose [`Session`] is
//! connected to the `sandboxd` inside. The container has:
//!
//! - no network but loopback; the session's socket, in a host directory mounted read-only,
//!   is the only way out;
//! - the role's user from its first process, no capabilities, and `no-new-privileges`;
//! - CPU, memory (swap included) and process limits;
//! - `sandboxd`, mounted read-only, as its first process, whose output alone is the
//!   container's log, which holds at most 1 MiB;
//! - [`/pumpkin`](PUMPKIN_DIR), a memory-backed filesystem of [`PUMPKIN_BYTES`] owned by the
//!   role's user, holding the [directories](PUMPKIN_SUBDIRS) the driver puts there over the
//!   session;
//! - `/tmp`, a read-only mask, so the container's writable layer is the only other place on
//!   disk a role's code can write: that layer is its workspace. Docker's `/dev/shm` and
//!   `/dev/mqueue` are writable too, but memory-backed, as `/pumpkin` is, so what they hold
//!   counts against the memory limit.
//!
//! Files move only over the session, never through the Docker daemon.
//!
//! # Limits of this provider
//!
//! - The provider puts no size limit on the container's writable layer, the role's
//!   workspace, so a role can fill the disk the Docker daemon stores containers on.
//! - A container that dies at once still costs [`CONNECT_TIMEOUT`] before [`Engine::start`]
//!   fails.
//!
//! # The envelope
//!
//! The operator provides the engine, once, as a Docker context, which they name; the provider
//! picks up none of its own, and nothing in its environment overrides the one named
//! ([`ContextName`]). [`Engine::connect`] reports the engine before anything starts
//! ([`Report`]). Within it, the provider creates only containers, each labelled with its run's
//! [`RunTag`], so that [`Engine::list`] and [`Engine::destroy_all`] find a run's containers,
//! and only those, even when the driver that started them is gone.
//!
//! Rootless Docker is the setup to choose: its engine, and so every container, runs as an
//! ordinary user, so escaping a container gains no more than that user has. A rootful engine
//! runs as root, and whoever may use its socket, as the `docker` group may, can make it do
//! anything root can. The provider runs on either, and its report says which.

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use pumpkin_gateway::{Listener, Put, Session};
use pumpkin_schema::ir::{AbsolutePath, Name};
use pumpkin_schema::sandbox::{PUMPKIN_BYTES, PUMPKIN_DIR, PUMPKIN_SUBDIRS, RelativePath};
use pumpkin_schema::workflow::Sandbox;
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};

/// How long a new container has to connect its `sandboxd` to the session's socket.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// The most the container's log may hold, in bytes, in one file. The log is kept on the
/// host's disk, outside the container's other limits, so it is capped whatever writes there,
/// with the `json-file` driver named, so that the cap doesn't rest on the daemon's default.
/// It holds the output of `sandboxd` alone, a line when it stops: its commands' output goes
/// to files, and they can't open its own, since it is non-dumpable.
const LOG_BYTES: u64 = 1 << 20;

/// The most processes and threads a container may have.
const PIDS_LIMIT: u32 = 4096;

/// Where `sandboxd` is mounted in the container, and where the session's socket directory is.
const SANDBOXD_PATH: &str = "/.pumpkin/sandboxd";
const GATEWAY_DIR: &str = "/.pumpkin/gateway";

/// The socket's name in its directory.
const SOCKET_NAME: &str = "s";

/// The label that carries a container's run tag.
const RUN_LABEL: &str = "pumpkin.run";

/// The name of a Docker context, which says what engine `docker` reaches, and how.
///
/// It is a letter or digit, then at least one more letter, digit, `_`, `.`, `+` or `-`, as
/// `docker context create` requires. `default` is Docker's own context, which reaches the
/// engine at Docker's default socket.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct ContextName(String);

impl ContextName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ContextName {
    type Error = String;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        let mut chars = name.chars();
        let valid = chars
            .next()
            .is_some_and(|first| first.is_ascii_alphanumeric())
            && !chars.as_str().is_empty()
            && chars.all(|char| char.is_ascii_alphanumeric() || "_.+-".contains(char));
        if valid {
            Ok(Self(name))
        } else {
            Err(format!(
                "`{}` is not a valid Docker context name: use a letter or digit, then at least \
                 one more letter, digit, `_`, `.`, `+` or `-`, as `docker context ls` lists them",
                name.escape_debug()
            ))
        }
    }
}

/// The Docker engine a context reaches, which every container of the provider's runs in, and
/// its report.
#[derive(Clone, Debug)]
pub struct Engine {
    report: Report,
}

/// The engine, as `docker` describes it before anything starts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Report {
    /// The context, and the endpoint it reaches the engine at.
    pub context: ContextName,
    pub endpoint: String,
    /// The versions of the `docker` command line and of the engine.
    pub client_version: String,
    pub engine_version: String,
    /// Whether the `docker` command line's configuration, `config.json`, sets `proxies`, from
    /// which `docker` gives every container it creates, and every build it runs, proxy
    /// variables such as `HTTP_PROXY`: an input to the sandboxes' environment that no workflow
    /// names.
    pub client_proxies: bool,
    /// The cgroup version, `1` or `2`, and the driver of containers' cgroups.
    pub cgroup_version: String,
    pub cgroup_driver: String,
    /// The storage driver, which holds containers' writable layers, and whether images are in
    /// containerd's store, the driver being one of its snapshotters.
    pub storage_driver: String,
    pub containerd_store: bool,
    /// What the engine's security options say.
    pub security: Security,
    /// The kernel's version, and the operating system, of the engine's host.
    pub kernel: String,
    pub os: String,
}

/// What the engine's security options say.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Security {
    /// Whether the engine runs as an ordinary user rather than as root: see
    /// [the envelope](crate#the-envelope).
    pub rootless: bool,
    /// The seccomp profile containers get unless told otherwise, `builtin` for Docker's own,
    /// or `None` if the engine has no seccomp.
    pub seccomp_profile: Option<String>,
    /// Whether the engine confines containers with `AppArmor`.
    pub apparmor: bool,
    /// Whether the engine maps containers' users into a user namespace of their own.
    pub userns: bool,
}

/// What `docker context inspect` prints of a context, as JSON.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ContextJson {
    endpoints: EndpointsJson,
}

#[derive(Deserialize)]
struct EndpointsJson {
    docker: EndpointJson,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct EndpointJson {
    host: String,
}

/// What `docker version` prints, as JSON.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct VersionJson {
    client: VersionedJson,
    server: VersionedJson,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct VersionedJson {
    version: String,
}

/// What `docker info` prints, as JSON.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InfoJson {
    cgroup_version: String,
    cgroup_driver: String,
    driver: String,
    /// Pairs of a name and a value, such as `driver-type` and `io.containerd.snapshotter.v1`.
    driver_status: Option<Vec<(String, String)>>,
    security_options: Option<Vec<String>>,
    kernel_version: String,
    operating_system: String,
}

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

/// What [`Engine::start`] needs to start a container.
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
    context: ContextName,
    id: String,
    name: Name,
    session: Session,
    removed: bool,
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

    /// Ends the session and removes the container, reporting a failure to remove it.
    ///
    /// # Errors
    ///
    /// Fails if `docker` can't remove the container.
    pub fn destroy(mut self) -> io::Result<()> {
        self.teardown()
    }

    fn teardown(&mut self) -> io::Result<()> {
        if std::mem::replace(&mut self.removed, true) {
            return Ok(());
        }
        self.session.end();
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
    /// what to do; if `docker` describes either in JSON of another form; or if the `docker`
    /// command line's configuration can't be read, or isn't JSON.
    pub fn connect(context: ContextName) -> io::Result<Self> {
        let inspect = [
            "context",
            "inspect",
            "--format",
            "{{json .}}",
            context.as_str(),
        ];
        let inspected = docker(&context, inspect).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "the Docker context `{}` can't be read: {error}: name one that `docker \
                     context ls` lists",
                    context.as_str()
                ),
            )
        })?;
        let ContextJson { endpoints } = parse_json("docker context inspect", &inspected)?;
        let reach =
            |args: [&str; 3]| docker(&context, args).map_err(|error| unreached(&context, &error));
        let version = reach(["version", "--format", "{{json .}}"])?;
        let VersionJson { client, server } = parse_json("docker version", &version)?;
        let info = reach(["info", "--format", "{{json .}}"])?;
        let InfoJson {
            cgroup_version,
            cgroup_driver,
            driver,
            driver_status,
            security_options,
            kernel_version,
            operating_system,
        } = parse_json("docker info", &info)?;
        let containerd_store = (driver_status.unwrap_or_default().iter())
            .any(|(name, value)| name == "driver-type" && value == "io.containerd.snapshotter.v1");
        Ok(Self {
            report: Report {
                context,
                endpoint: endpoints.docker.host,
                client_version: client.version,
                engine_version: server.version,
                client_proxies: client_proxies()?,
                cgroup_version,
                cgroup_driver,
                storage_driver: driver,
                containerd_store,
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

    /// Starts a container for the role, connects its session, and puts [`PUMPKIN_SUBDIRS`]
    /// under [`PUMPKIN_DIR`].
    ///
    /// # Errors
    ///
    /// Fails if `docker` can't create or start the container, if its `sandboxd` doesn't
    /// connect within [`CONNECT_TIMEOUT`], or if the directories can't be put. Whatever
    /// container `docker` created is removed first.
    pub fn start(&self, start: &Start<'_>) -> io::Result<Container> {
        let context = &self.report.context;
        let socket = start.socket_dir.join(SOCKET_NAME);
        let listener = Listener::bind(socket)?;
        // Creating and starting apart gives the container's ID before anything can fail with
        // a container left behind, so a failure removes this container by ID, and never
        // another that has its name.
        let id = docker(context, &create_args(start)?)?.trim().to_owned();
        let started = docker(context, ["start", &id]).and_then(|_| connect(listener, &id));
        match started {
            Ok(session) => Ok(Container {
                context: context.clone(),
                id,
                name: start.name.clone(),
                session,
                removed: false,
            }),
            Err(error) => {
                let logs = log_tail(context, &id).unwrap_or_default();
                let _ = remove(context, &id);
                let message = match logs.trim() {
                    "" => error.to_string(),
                    logs => format!("{error}; the container's logs end: {logs}"),
                };
                Err(io::Error::new(error.kind(), message))
            }
        }
    }

    /// The IDs of the run's containers, running or not.
    ///
    /// # Errors
    ///
    /// Fails if `docker` can't list them.
    pub fn list(&self, run: &RunTag) -> io::Result<Vec<String>> {
        let filter = format!("label={RUN_LABEL}={}", run.as_str());
        let ids = docker(
            &self.report.context,
            [
                "container",
                "ls",
                "--all",
                "--quiet",
                "--no-trunc",
                "--filter",
                &filter,
            ],
        )?;
        Ok(ids.lines().map(str::to_owned).collect())
    }

    /// Removes every container of the run, and returns their IDs. Containers of other runs
    /// stay.
    ///
    /// # Errors
    ///
    /// Fails if `docker` can't list them, or can't remove one; the others are removed
    /// regardless.
    pub fn destroy_all(&self, run: &RunTag) -> io::Result<Vec<String>> {
        let ids = self.list(run)?;
        let failures: Vec<String> = ids
            .iter()
            .filter_map(|id| {
                remove(&self.report.context, id)
                    .err()
                    .map(|error| error.to_string())
            })
            .collect();
        if failures.is_empty() {
            Ok(ids)
        } else {
            Err(io::Error::other(failures.join("; ")))
        }
    }
}

impl Security {
    /// The security options `docker info` lists, each a list of `key=value`, such as
    /// `name=seccomp,profile=builtin`.
    fn parse(options: &[String]) -> Self {
        let mut security = Self::default();
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

/// Why the engine can't be reached through `context`, from `docker`'s `error`, and what to do.
fn unreached(context: &ContextName, error: &io::Error) -> io::Error {
    let todo = if error.to_string().contains("permission denied") {
        "let this user reach the engine's socket: by the `docker` group, which is \
         root-equivalent, or by running rootless Docker and naming its context"
    } else {
        "start the engine, or name a context whose engine runs"
    };
    io::Error::new(
        error.kind(),
        format!(
            "the Docker engine isn't reachable through context `{}`: {error}: {todo}",
            context.as_str()
        ),
    )
}

/// Parses `json`, which `command` printed.
fn parse_json<T: for<'de> Deserialize<'de>>(command: &str, json: &str) -> io::Result<T> {
    serde_json::from_str(json).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "the JSON `{command}` printed isn't of the form expected: {error}: use Docker's \
                 own command line, which prints it"
            ),
        )
    })
}

/// Whether the `docker` command line's configuration has a `proxies` key.
///
/// The file is `config.json` in `DOCKER_CONFIG`, if that is set and not empty, or else in
/// `.docker` in the home directory: `HOME`, if that is set and not empty, or else the user's in
/// the user database. That is where `docker` looks, given the variables [`docker_output`]
/// passes it; a missing file is no configuration. The file is read as `docker` reads it: its
/// first JSON value, if it has one, with `null` as no configuration, and keys matched
/// regardless of case (here, of ASCII case). Only the names of its keys are kept, never their
/// values, which may be a registry's credentials.
fn client_proxies() -> io::Result<bool> {
    let dir = (env::var_os("DOCKER_CONFIG").filter(|dir| !dir.is_empty())).map_or_else(
        || env::home_dir().unwrap_or_default().join(".docker"),
        PathBuf::from,
    );
    let path = dir.join("config.json");
    let json = match fs::read(&path) {
        Ok(json) => json,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!(
                    "the Docker command line's configuration {} can't be read: {error}: make it \
                     readable by this user, as `docker` reads it",
                    path.display()
                ),
            ));
        }
    };
    #[expect(
        clippy::zero_sized_map_values,
        reason = "a JSON object's keys are read as a map's, whose values are skipped"
    )]
    let first = (serde_json::Deserializer::from_slice(&json))
        .into_iter::<Option<BTreeMap<String, IgnoredAny>>>()
        .next();
    match first {
        None | Some(Ok(None)) => Ok(false),
        Some(Ok(Some(keys))) => Ok(keys.keys().any(|key| key.eq_ignore_ascii_case("proxies"))),
        // serde_json's message may quote the file, so only where it went wrong is said.
        Some(Err(error)) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "the Docker command line's configuration {} isn't a JSON object, at line {}, \
                 column {}: make it one, as `docker` reads it",
                path.display(),
                error.line(),
                error.column()
            ),
        )),
    }
}

fn connect(listener: Listener, id: &str) -> io::Result<Session> {
    let session = listener.accept(CONNECT_TIMEOUT)?;
    let root = AbsolutePath::try_from(PUMPKIN_DIR.to_owned());
    let dirs = PUMPKIN_SUBDIRS.map(|dir| RelativePath::try_from(dir.to_owned()));
    #[expect(
        clippy::unreachable,
        reason = "`PUMPKIN_DIR` is an absolute path, and each of `PUMPKIN_SUBDIRS` one \
                  component, neither empty, `.` nor `..`"
    )]
    let (Ok(root), Ok(dirs)) = (root, dirs.into_iter().collect::<Result<Vec<_>, _>>()) else {
        unreachable!("`{PUMPKIN_DIR}` or one of {PUMPKIN_SUBDIRS:?} isn't a valid path")
    };
    let puts: Vec<Put<'_>> = dirs.iter().map(Put::Dir).collect();
    session
        .put(&root, &puts)
        .map_err(|error| io::Error::new(error.kind(), format!("{id}: {error}")))?;
    Ok(session)
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
        format!("--tmpfs={PUMPKIN_DIR}:exec,uid={uid},gid={gid},mode=0755,size={PUMPKIN_BYTES}"),
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

/// Removes the container with ID `id`, and its anonymous volumes. A container already gone is
/// removed.
fn remove(context: &ContextName, id: &str) -> io::Result<()> {
    match docker(context, ["rm", "--force", "--volumes", id]) {
        Err(error) if error.to_string().contains("No such container") => Ok(()),
        other => other.map(|_| ()),
    }
}

/// The last lines of the container's log: those of `sandboxd`'s standard output, then those of
/// its standard error, which `docker logs` writes to its own two streams.
fn log_tail(context: &ContextName, id: &str) -> io::Result<String> {
    let Output { stdout, stderr, .. } = docker_output(context, ["logs", "--tail", "20", id])?;
    Ok(String::from_utf8_lossy(&[stdout, stderr].concat()).into_owned())
}

/// Runs `docker` against `context` with `args` and returns its standard output.
fn docker<I, S>(context: &ContextName, args: I) -> io::Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = docker_output(context, args)?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Runs `docker` against `context` alone with `args`, and returns its output, or fails with
/// its standard error.
///
/// `docker` keeps only what it needs to find Docker's own configuration: `HOME`, for
/// `~/.docker` and the configuration, contexts and CLI plugins there; `PATH`, for credential
/// helpers and plugins; and `DOCKER_CONFIG`, if set, Docker's own variable for where that
/// configuration lives. The rest of the environment is dropped, so that the context named
/// alone steers the command line: otherwise `DOCKER_HOST`, `DOCKER_TLS_VERIFY` and
/// `DOCKER_CERT_PATH`, among others, would set the endpoint and TLS of `default`. A context
/// reached over `ssh://` would need `SSH_AUTH_SOCK` too, which isn't passed.
fn docker_output<I, S>(context: &ContextName, args: I) -> io::Result<Output>
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
        .args(["--context", context.as_str()])
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
