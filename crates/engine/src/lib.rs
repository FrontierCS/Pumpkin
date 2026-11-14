//! The operator's Docker engine, on which Pumpkin builds the images that its sandboxes run,
//! whichever provider runs them.
//!
//! [`Engine`] runs `docker` as an ordinary user against the engine of the context that the
//! operator names ([`ContextName`]), which nothing in the environment overrides, and reports it
//! ([`Report`]). It builds images ([`Engine::build`]); [`confinement`] confines a role's
//! container wherever a provider's sandbox is one. Outside the engine, it writes only in the
//! system's temporary directory. It needs Docker 25.0 or later, rootful or rootless
//! ([`Security::rootless`]).

use std::ffi::OsStr;
use std::io::ErrorKind;
use std::process::{ExitStatus, Output};
use std::time::Duration;
use std::{fmt, io};

use schema::ir::{MILLIS_PER_CPU, Resources, User};
use schema::sandbox::{PUMPKIN_BYTES, pumpkin_dir};
use serde::{Deserialize, Serialize};
use stdx::Deadline;
use stdx::process::Environment;

mod build;

pub use build::{BuildContext, Building, Built, Recipe, Started, finish, mounted};

/// How long a `docker` command that is neither a create's nor a removal may run before it is
/// killed ([`Engine`]).
// Each such command makes one request of the engine, or a few, and none waits on a pull or a
// build.
const CALL_TIMEOUT: Duration = Duration::from_mins(2);

/// How long a removal that isn't a create's may run before it is killed ([`Engine`]).
// A removal deletes the container's writable layer and anonymous volumes, which have no size
// limit, so its time grows with what the sandbox wrote there, as a tree of many small files.
// And a run tries each removal once, and fails with it, while the engine goes on to finish one
// whose `docker rm` was killed: so the bound is sized only to catch an engine that hangs.
pub const REMOVE_TIMEOUT: Duration = Duration::from_mins(10);

/// The most the container's log may hold, in bytes, in one file. The log is kept on the
/// host's disk, outside the container's other limits, so it is capped whatever writes there,
/// with the `json-file` driver named, so that the cap doesn't rest on the daemon's default.
/// It holds the output of `sandboxd` alone, which is a line if it fails: its commands' output
/// goes to files, and they can't open its own, since it is non-dumpable.
const LOG_BYTES: u64 = 1 << 20;

/// The most processes and threads a container may have.
const PIDS_LIMIT: u32 = 4096;

/// The name of a Docker context, which says what engine `docker` reaches, and how. `default`
/// is Docker's own context, which reaches the engine at Docker's default socket.
///
/// Any name but the empty one, which `docker` would take for no name, and so reach the engine
/// of the command line's current context. A name no context has is refused by
/// [`Engine::connect`].
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(try_from = "String")]
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

/// The Docker engine a context reaches, and its report.
///
/// Each `docker` command it runs but `docker build` is killed if it hasn't ended in time, so
/// that each call returns within a bound: a removal at [`REMOVE_TIMEOUT`], any other at
/// `CALL_TIMEOUT`, each after it starts, or one that its caller
/// bounds ([`docker_to`]) at that bound.
#[derive(Debug)]
pub struct Engine {
    report: Report,
}

/// The engine, as `docker` describes it before anything starts.
#[derive(Clone, Debug, Serialize)]
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
#[derive(Clone, Debug, Serialize)]
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
struct InfoJson {
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
    /// Reaches the engine through `context`, and reports it.
    ///
    /// # Errors
    ///
    /// Fails, saying what to do, if the engine can't be reached or read.
    pub fn connect(context: ContextName) -> Result<Self, String> {
        let Reach { endpoint, info } = reach(&context)?;
        let InfoJson {
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
        let report = Report {
            context,
            endpoint,
            client_version: client_info.version,
            engine_version: server_version,
            security: Security::parse(&security_options.unwrap_or_default()),
            kernel: kernel_version,
            os: operating_system,
        };
        Ok(Self { report })
    }

    /// Reaches the engine through `context`, as [`connect`](Self::connect) does, but checks
    /// nothing of it, for a caller that only lists and removes containers.
    ///
    /// # Errors
    ///
    /// Fails if the context can't be read, or the engine can't be reached through it, saying
    /// what to do.
    pub fn reach(context: &ContextName) -> Result<(), String> {
        reach(context).map(drop)
    }

    /// The engine, as it was when [`connect`](Self::connect) reached it.
    pub fn report(&self) -> &Report {
        &self.report
    }
}

/// What `docker` says of the engine that a context reaches, once it has reached it.
struct Reach {
    /// The endpoint the context reaches the engine at.
    endpoint: String,
    /// What `docker info` printed, as JSON.
    info: Output,
}

/// Reaches the engine through `context`: reads the context, and asks the engine what it is.
fn reach(context: &ContextName) -> Result<Reach, String> {
    let name = context.as_str();
    let template = "{{.Endpoints.docker.Host}}";
    // After `--`, a name that begins with `-` can't be taken for an option.
    let inspect = ["context", "inspect", "--format", template, "--", name];
    let inspected = docker(context, inspect).map_err(|error| {
        format!(
            "the Docker context `{name}` can't be read: {error}: name one that \
             `docker context ls` lists"
        )
    })?;
    let endpoint = String::from_utf8_lossy(&inspected.stdout).trim().to_owned();
    let info = docker(context, ["info", "--format", "{{json .}}"]).map_err(|error| {
        let todo = match &error {
            Failure::Exited { stderr, .. } if stderr.contains("permission denied") => {
                "let this user reach the engine's socket: by the `docker` group, which is \
                 root-equivalent, or by running rootless Docker and naming its context"
            }
            Failure::Unrun(_)
            | Failure::Unwatched { .. }
            | Failure::Unended { .. }
            | Failure::Exited { .. } => "start the engine, or name a context whose engine runs",
        };
        format!("the Docker engine isn't reachable through context `{name}`: {error}: {todo}")
    })?;
    Ok(Reach { endpoint, info })
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

/// The options of `docker create` or `docker run` that confine a container to its role's `user`
/// and `resources`, wherever a provider's sandbox is a Docker container.
///
/// The container's network, bind mounts and command, which say how it reaches its gateway and
/// what it runs, are the caller's to give.
pub fn confinement(user: User, resources: Resources) -> Vec<String> {
    let User { uid, gid } = user;
    let Resources {
        cpu_millis,
        memory_bytes,
    } = resources;
    let (millis, memory) = (cpu_millis.get(), memory_bytes.get());
    vec![
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
        format!(
            "--cpus={}.{:03}",
            millis / MILLIS_PER_CPU,
            millis % MILLIS_PER_CPU
        ),
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
    ]
}

/// The ID of the image that `reference`, such as `<repository>:<tag>`, names on the engine of
/// `context`, or `None` if the engine has no such image.
///
/// # Errors
///
/// Fails if `docker` does, but for an image it doesn't have.
pub fn image_id(context: &ContextName, reference: &str) -> Result<Option<String>, Failure> {
    let inspect = ["image", "inspect", "--format", "{{.Id}}", "--", reference];
    match docker(context, inspect) {
        Ok(inspected) => Ok(Some(
            String::from_utf8_lossy(&inspected.stdout).trim().to_owned(),
        )),
        Err(Failure::Exited { stderr, .. }) if stderr.contains("No such image") => Ok(None),
        Err(error) => Err(error),
    }
}

/// `error`, from a `docker` command that failed for a reason its caller doesn't foresee, and
/// what to do about it.
///
/// That is to check the engine, if it was killed at its bound, as Docker reported nothing; or
/// else to fix what Docker reports, which says what is wrong.
pub fn fix_reported(error: &Failure) -> String {
    let todo = match error {
        Failure::Unended { .. } => "check that the engine runs and isn't overloaded",
        Failure::Unrun(_) | Failure::Unwatched { .. } | Failure::Exited { .. } => {
            "fix what Docker reports"
        }
    };
    format!("{error}: {todo}")
}

/// Runs `docker` against `context` alone with `args`, as [`docker_to`] does, within
/// `CALL_TIMEOUT`.
///
/// # Errors
///
/// Fails as [`docker_to`] does.
pub fn docker<I, S>(context: &ContextName, args: I) -> Result<Output, Failure>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    docker_to(context, args, Bound::Within(CALL_TIMEOUT))
}

/// Runs `docker` against `context` alone with `args`, killing it at `bound`, and returns its
/// output. Of the environment, `docker` gets only `HOME`, `PATH` and `DOCKER_CONFIG`, so that the
/// context named alone steers it.
///
/// # Errors
///
/// Fails if the command fails, or is killed at its bound, saying which it was and what Docker
/// reported, for the caller to add what that means and what to do; or if `docker` doesn't run.
pub fn docker_to<I, S>(context: &ContextName, args: I, bound: Bound) -> Result<Output, Failure>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<S> = args.into_iter().collect();
    // `docker`, its command, such as `container ls`, and its argument, as in `start <id>`, but
    // none of its options, which can be long.
    let mut shown = "docker".to_owned();
    let words = args.iter().map(|arg| arg.as_ref().to_string_lossy());
    for word in words.take(2).take_while(|word| !word.starts_with('-')) {
        shown.push(' ');
        shown.push_str(&word);
    }
    let mut command = docker_command(context, &args);
    let by = match bound {
        Bound::Within(timeout) => Deadline::after(timeout),
        Bound::Until(by) => by,
    };
    let output = match stdx::process::output(&mut command, by) {
        Ok(Some(output)) => output,
        Ok(None) => {
            let command = shown;
            return Err(Failure::Unended { command, bound });
        }
        // A spawn fails so when it finds no `docker`, or one it can't execute; any other
        // error, such as a failed read of its output, isn't one that installing `docker`
        // would mend.
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::PermissionDenied
            ) =>
        {
            return Err(Failure::Unrun(error));
        }
        Err(error) => {
            let command = shown;
            return Err(Failure::Unwatched { command, error });
        }
    };
    if output.status.success() {
        return Ok(output);
    }
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    Err(Failure::Exited {
        command: shown,
        status: output.status,
        stderr,
    })
}

/// The command that runs `docker` with `args` against the engine of `context`, with the
/// environment that [`docker_to`] gives it.
fn docker_command<I, S>(context: &ContextName, args: I) -> std::process::Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    // A context reached over `ssh://` would need `SSH_AUTH_SOCK` too, which isn't passed.
    let kept = Environment::Only(&["HOME", "PATH", "DOCKER_CONFIG"]);
    let mut command = stdx::process::command("docker", kept);
    // One argument, so that a name that begins with `-` can't be taken for an option.
    command
        .arg(format!("--context={}", context.as_str()))
        .args(args);
    command
}

/// Why a [`docker`] command failed, displayed as [`docker`] describes its error.
pub enum Failure {
    /// `docker` didn't run.
    Unrun(io::Error),
    /// It ran, but couldn't be followed to its end, as when its output couldn't be read:
    /// `command` shows which, as for [`Exited`](Self::Exited).
    Unwatched { command: String, error: io::Error },
    /// It hadn't ended by `bound`, and was killed then, or never started, if a deadline had
    /// passed before it.
    Unended { command: String, bound: Bound },
    /// It failed: `command` shows which, such as `docker container ls`, and `stderr` is what
    /// Docker reported, or, where its report went to this process's standard error, that it
    /// says why.
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
            Self::Unwatched { command, error } => write!(f, "running `{command}` failed: {error}"),
            Self::Unended {
                command,
                bound: Bound::Within(timeout),
            } => {
                let secs = timeout.as_secs();
                write!(
                    f,
                    "`{command}` didn't end within {secs} s, so it was killed"
                )
            }
            // A command whose deadline had passed before it began never started.
            Self::Unended {
                command,
                bound: Bound::Until(_),
            } => write!(f, "the deadline passed before `{command}` ended"),
            Self::Exited {
                command,
                status,
                stderr,
            } => write!(f, "`{command}` failed with {status}: {}", stderr.trim()),
        }
    }
}

/// When a `docker` command is killed if it hasn't ended ([`Engine`]).
#[derive(Clone, Copy)]
pub enum Bound {
    /// This long after it starts.
    Within(Duration),
    /// At this deadline.
    Until(Deadline),
}
