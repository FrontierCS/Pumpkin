//! The Docker provider: a sandbox for a role as a container that needs no root to start.
//!
//! [`Engine`], a [`Provider`], runs `docker` as an ordinary user against the engine of the
//! operator's [context](#the-envelope), and makes each sandbox a container that has:
//!
//! - no network but loopback; the dial's socket, in a host directory mounted read-only, is the
//!   only way out;
//! - the role's user from its first process, no capabilities, and `no-new-privileges`;
//! - CPU, memory (swap included) and process limits, and a core file size limit of 0;
//! - `sandboxd`, mounted read-only, as its first process, dialing the [`Dial`] its create
//!   gives, and whose output alone is the container's log, which holds at most 1 MiB;
//! - [`/pumpkin`](pumpkin_dir), a tmpfs owned by the role's user, for the session's owner to
//!   put [directories](pumpkin_schema::sandbox::pumpkin_subdirs) in. It holds at most
//!   [`PUMPKIN_BYTES`], and its files are in memory, counted against the role's memory limit,
//!   so a role can keep there no more than that limit, less what its processes use;
//! - `/tmp`, a read-only mask. On disk, a role's code can write only to the container's
//!   writable layer, which is its workspace, and to the anonymous volume Docker makes for each
//!   `VOLUME` its image declares, which is removed with the container. Neither has a size
//!   limit, so a role can fill the disk the Docker daemon stores containers on. Docker's
//!   `/dev/shm` and `/dev/mqueue` are writable too, but memory-backed, as `/pumpkin` is, so
//!   what they hold counts against the memory limit;
//! - for a role that makes ops ([`Create::ops`]), [`sandboxctl`](SANDBOXCTL_PATH), mounted
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
//! anonymous volumes Docker makes for their images' `VOLUME`s, which are removed with them; it
//! pulls no image, so its publish checks that the engine has a role's. Each container is
//! labelled with its run's [`RunTag`], so that [`Provider::list`] finds a run's containers, and
//! only those, even when the process that started them is gone, and with its role's name, so
//! that `docker ps` shows which is which. The provider runs on a rootful engine or a rootless
//! one, and its report says which ([`Security::rootless`]).

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::num::NonZeroU64;
use std::path::Path;
use std::process::{ExitStatus, Output, Stdio};

use pumpkin_provider::Provider;
use pumpkin_schema::ir::{Resources, User};
use pumpkin_schema::provider::{
    Capabilities, Console, Create, Created, Dial, Listed, Local, Published, Ready, Role, RunTag,
    SOCKET_NAME, SandboxId,
};
use pumpkin_schema::sandbox::{PUMPKIN_BYTES, SANDBOXCTL_PATH, pumpkin_dir};
use serde::{Deserialize, Serialize};
use stdx::Context;
use stdx::process::Environment;

/// The most seconds a sandbox's start takes, as [`Capabilities::startup_seconds`] declares it.
const STARTUP_SECONDS: NonZeroU64 = NonZeroU64::new(30).unwrap();

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
#[derive(Clone, Debug, Serialize)]
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
    /// What [`Provider::ready`] gives: the report as JSON, and the capabilities.
    ready: Ready,
}

/// The engine, as `docker` describes it before anything starts.
#[derive(Debug, Serialize)]
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
#[derive(Debug, Serialize)]
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
            let todo = match &error {
                Failure::Exited { stderr, .. } if stderr.contains("permission denied") => {
                    "let this user reach the engine's socket: by the `docker` group, which is \
                     root-equivalent, or by running rootless Docker and naming its context"
                }
                Failure::Unrun(_) | Failure::Exited { .. } => {
                    "start the engine, or name a context whose engine runs"
                }
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
        let report = Report {
            context,
            endpoint,
            client_version: client_info.version,
            engine_version: server_version,
            security: Security::parse(&security_options.unwrap_or_default()),
            kernel: kernel_version,
            os: operating_system,
        };
        let ready = ready(&report);
        Ok(Self { report, ready })
    }

    /// The engine, as it was when [`connect`](Self::connect) reached it.
    pub fn report(&self) -> &Report {
        &self.report
    }
}

impl Provider for Engine {
    fn ready(&self) -> &Ready {
        &self.ready
    }

    // A sandbox runs what the engine has, as it pulls nothing, so a publish checks that the
    // engine has each image and that `sandboxd` can be mounted, and names each as it is.
    fn publish(&self, locals: &[Local]) -> Result<Vec<Published>, String> {
        let context = &self.report.context;
        let published = locals.iter().map(|local| match local {
            Local::Role { image } => {
                let image = image.to_string();
                let inspect = ["image", "inspect", "--format", "{{.Id}}", "--", &image];
                match docker(context, inspect) {
                    Ok(_) => Ok(Published::from(image)),
                    Err(Failure::Exited { stderr, .. }) if stderr.contains("No such image") => {
                        Err(format!(
                            "the engine of context `{}` has no image {image}: build or load it \
                             there first, as the provider pulls none",
                            context.as_str()
                        ))
                    }
                    Err(error) => Err(fix_reported(error)),
                }
            }
            Local::Sandboxd { path } => (mountable(path, SANDBOXD_PATH))
                .context("publishing `sandboxd` failed")
                .map(Published::from),
        });
        published.collect()
    }

    fn create(&self, create: &Create) -> Result<Created, String> {
        let context = &self.report.context;
        // Creating and starting apart gives the container's ID before anything can fail with
        // a container left behind, so a failure removes this container by its ID.
        let args = create_args(create).context("creating the container failed")?;
        let created = docker(context, &args).map_err(fix_reported)?;
        let id = String::from_utf8_lossy(&created.stdout).trim().to_owned();
        if let Err(error) = docker(context, ["start", &id]) {
            let mut message = fix_reported(error);
            let left = format_args!("container {id} is left behind, as removing it failed");
            if let Err(removal) = remove(context, &id).context(left) {
                message = format!("{message}; {removal}");
            }
            return Err(message);
        }
        Ok(Created {
            id: SandboxId::from(id),
        })
    }

    fn delete(&self, run: &RunTag, ids: &[SandboxId]) -> Result<Vec<SandboxId>, String> {
        let context = &self.report.context;
        let ours: BTreeSet<String> = of_run(context, run)?.into_iter().collect();
        let (known, unknown): (Vec<&SandboxId>, Vec<&SandboxId>) =
            ids.iter().partition(|id| ours.contains(id.as_str()));
        let failures: Vec<String> = (known.iter())
            .filter_map(|id| remove(context, id.as_str()).err())
            .collect();
        if failures.is_empty() {
            Ok(unknown.into_iter().cloned().collect())
        } else {
            Err(fix_reported(failures.join("; ")))
        }
    }

    fn list(&self, run: &RunTag) -> Result<Vec<Listed>, String> {
        let ids = of_run(&self.report.context, run)?;
        let listed = ids.into_iter().map(|id| Listed {
            id: SandboxId::from(id),
        });
        Ok(listed.collect())
    }

    fn console(&self, run: &RunTag, id: &SandboxId) -> Result<Console, String> {
        let context = &self.report.context;
        let id = id.as_str();
        if !of_run(context, run)?.iter().any(|ours| ours == id) {
            return Err(format!(
                "container {id} is no sandbox of run {}: name one that the run's list finds",
                run.as_str()
            ));
        }
        let text = log_tail(context, id).map_err(fix_reported)?;
        Ok(Console { text })
    }
}

/// What [`Provider::ready`] gives of an engine that `report` describes.
fn ready(report: &Report) -> Ready {
    #[expect(
        clippy::unreachable,
        reason = "a `Report` holds only strings, bools and `Option<String>`s, through derived \
                  impls, none of which serde_json fails to write"
    )]
    let Ok(json) = serde_json::to_value(report) else {
        unreachable!("{report:?} can't be written as JSON")
    };
    Ready {
        report: json,
        capabilities: Capabilities {
            startup_seconds: STARTUP_SECONDS,
        },
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

/// The arguments of `docker create` for the container.
fn create_args(create: &Create) -> Result<Vec<String>, String> {
    let Create {
        run,
        role,
        image,
        sandboxd,
        ops,
        dial,
    } = create;
    let Dial::Socket { dir } = dial;
    let Role {
        name,
        user,
        resources,
    } = role;
    let sandboxd = Path::new(sandboxd.as_str());
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
        bind_mount(dir, GATEWAY_DIR)?,
    ];
    if *ops {
        args.push(bind_mount(sandboxd, SANDBOXCTL_PATH)?);
    }
    args.extend([
        format!("--entrypoint={SANDBOXD_PATH}"),
        "--".to_owned(),
        image.as_str().to_owned(),
        format!("{GATEWAY_DIR}/{SOCKET_NAME}"),
    ]);
    if *ops {
        args.push("--ops".to_owned());
    }
    Ok(args)
}

/// A read-only `--mount` of `host`, which must exist, at `container`.
fn bind_mount(host: &Path, container: &str) -> Result<String, String> {
    let source = mountable(host, container)?;
    Ok(format!(
        "--mount=type=bind,source={source},target={container},readonly"
    ))
}

/// The path that a `--mount` of `host`, which must exist, at `container` names: `host`'s own,
/// resolved, in UTF-8 and without commas, which `--mount` would read as the start of another
/// option.
fn mountable(host: &Path, container: &str) -> Result<String, String> {
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
    Ok(source.to_owned())
}

/// Removes the container with ID `id`, and its anonymous volumes. `--force` kills the container
/// first if it runs. A container already gone is removed.
fn remove(context: &ContextName, id: &str) -> Result<(), String> {
    match docker(context, ["rm", "--force", "--volumes", id]) {
        Ok(_) => Ok(()),
        // Docker 29 exits 0 here, but no minimum version is pinned, and older engines fail.
        Err(Failure::Exited { stderr, .. }) if stderr.contains("No such container") => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

/// The IDs of run `run`'s containers, running or not: those whose run label holds its tag.
fn of_run(context: &ContextName, run: &RunTag) -> Result<Vec<String>, String> {
    let filter = format!("label={RUN_LABEL}={}", run.as_str());
    let format = format!("{{{{.ID}}}} {{{{.Label \"{RUN_LABEL}\"}}}}");
    let ls = [
        "container",
        "ls",
        "--all",
        "--no-trunc",
        "--filter",
        &filter,
        "--format",
        &format,
    ];
    let listed = docker(context, ls).map_err(fix_reported)?;
    // Each row's label is compared here too, so that neither a bug in the filter nor how
    // `docker` reads it reaches another run's container, whatever deletes what this finds.
    let rows = String::from_utf8_lossy(&listed.stdout);
    let ids = rows.lines().filter_map(|row| {
        let (id, label) = row.split_once(' ')?;
        (label == run.as_str()).then(|| id.to_owned())
    });
    Ok(ids.collect())
}

/// The last lines of the container's log: those of `sandboxd`'s standard output, then those of
/// its standard error, which `docker logs` writes to its own two streams.
fn log_tail(context: &ContextName, id: &str) -> Result<String, Failure> {
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
fn fix_reported(error: impl fmt::Display) -> String {
    format!("{error}: fix what Docker reports")
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
fn docker<I, S>(context: &ContextName, args: I) -> Result<Output, Failure>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<S> = args.into_iter().collect();
    let kept = Environment::Only(&["HOME", "PATH", "DOCKER_CONFIG"]);
    let output = stdx::process::command("docker", kept)
        // One argument, so that a name that begins with `-` can't be taken for an option.
        .arg(format!("--context={}", context.as_str()))
        .args(&args)
        .stdin(Stdio::null())
        .output()
        .map_err(Failure::Unrun)?;
    if output.status.success() {
        Ok(output)
    } else {
        // `docker`, its command, such as `container ls`, and its argument, as in `start <id>`,
        // but none of its options, which can be long.
        let mut command = "docker".to_owned();
        let words = args.iter().map(|arg| arg.as_ref().to_string_lossy());
        for word in words.take(2).take_while(|word| !word.starts_with('-')) {
            command.push(' ');
            command.push_str(&word);
        }
        Err(Failure::Exited {
            command,
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Why a [`docker`] command failed, displayed as [`docker`] describes its error.
enum Failure {
    /// `docker` didn't run.
    Unrun(io::Error),
    /// It failed: `command` shows which, such as `docker container ls`, and `stderr` is what
    /// Docker reported.
    Exited {
        command: String,
        status: ExitStatus,
        stderr: String,
    },
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unrun(error) => write!(
                f,
                "`docker` didn't run: {error}: install Docker's command line on `PATH`"
            ),
            Self::Exited {
                command,
                status,
                stderr,
            } => write!(f, "`{command}` failed with {status}: {}", stderr.trim()),
        }
    }
}
