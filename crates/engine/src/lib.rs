//! The operator's Docker engine, on which Pumpkin builds the images that its sandboxes run,
//! whichever provider runs them, and from which it publishes them.
//!
//! [`Engine`] runs `docker` as an ordinary user against the engine of the context that the
//! operator names ([`ContextName`]), which nothing in the environment overrides, and reports it
//! ([`Report`]). It builds images ([`Engine::build`]), writes a recipe's fetch stage
//! ([`Engine::lock`]), loads the image of a `sandboxd` ([`Engine::load_sandboxd`]) and pushes
//! ([`Pusher::push`]); [`confinement`] confines a
//! role's container wherever a provider's sandbox is one. Outside the engine, it writes only in
//! the directory it is given. It needs Docker 28.0 or later, the first whose `docker image
//! inspect` says what an image's ID is the digest of ([`Engine::check_platform`]), with the
//! containerd image store, rootful or rootless ([`Security::rootless`]).

use std::ffi::OsStr;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output};
use std::time::Duration;
use std::{fmt, io};

use base64::Engine as _;
use schema::ir::{Image, MILLIS_PER_CPU, Resources, User};
use schema::sandbox::{PUMPKIN_BYTES, pumpkin_dir};
use serde::{Deserialize, Serialize};
use stdx::Deadline;
use stdx::process::Environment;

mod build;
mod lock;
mod sandboxd;

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

/// How long a push may run before it is killed ([`Engine`]).
// A push uploads the layers the registry lacks, so its time grows with their size and falls
// with the host's bandwidth, which no bound can know: this one is sized only to turn a push that
// hangs into a failure, rather than a wait without end. A later push of the image uploads only
// the layers the registry still lacks.
const PUSH_TIMEOUT: Duration = Duration::from_hours(2);

/// The first release of Docker's command line whose push takes its login from its environment
/// alone, from `DOCKER_AUTH_CONFIG` ([`Pusher`]).
// An older one ignores the variable, and pushes with whatever the operator's own configuration
// holds for the registry, such as another identity's, from a credential helper.
const PUSHING_CLI: [u64; 3] = [28, 3, 0];

/// The most the container's log may hold, in bytes, in one file. The log is kept on the
/// host's disk, outside the container's other limits, so it is capped whatever writes there,
/// with the `json-file` driver named, so that the cap doesn't rest on the daemon's default.
/// It holds the output of `sandboxd` alone, which is a line if it fails: its commands' output
/// goes to files, and they can't open its own, since it is non-dumpable.
const LOG_BYTES: u64 = 1 << 20;

/// The most processes and threads a container may have.
const PIDS_LIMIT: u32 = 4096;

/// The entry of `docker info`'s `DriverStatus` that an engine whose images are in the containerd
/// image store reports.
const CONTAINERD_STORE: (&str, &str) = ("driver-type", "io.containerd.snapshotter.v1");

/// The media types of an image's manifest: the OCI image format's, and Docker's schema 2, which
/// it was made from, and which some engines' builds still write.
pub const MANIFEST_TYPES: [&str; 2] = [
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
];

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

/// A platform that images are for, as an image's configuration names it: an operating system
/// and an architecture, which Docker writes `<os>/<architecture>`, such as `linux/amd64`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Platform {
    os: String,
    architecture: String,
}

impl Platform {
    /// Linux on x86-64, `linux/amd64`: the platform of the static `sandboxd`, which is built for
    /// it alone.
    pub fn sandboxd() -> Self {
        Self {
            os: "linux".to_owned(),
            architecture: "amd64".to_owned(),
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { os, architecture } = self;
        write!(f, "{os}/{architecture}")
    }
}

impl Serialize for Platform {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// The credentials of an image registry, with which [`Pusher::push`] pushes to it. Its `Debug`
/// shows no password.
pub struct Login {
    registry: String,
    username: String,
    password: String,
}

impl Login {
    /// The credentials `username` and `password` for the registry `registry`, named as Docker
    /// names it in an image's name, such as `registry.example` or `127.0.0.1:5000`.
    pub fn new(registry: String, username: String, password: String) -> Self {
        Self {
            registry,
            username,
            password,
        }
    }

    /// The credentials as `DOCKER_AUTH_CONFIG` holds them: the user's name and the password,
    /// joined by a colon, in base64, by the registry's name.
    fn auth_config(&self) -> String {
        let Self {
            registry,
            username,
            password,
        } = self;
        let auth =
            base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
        serde_json::json!({ "auths": { registry: { "auth": auth } } }).to_string()
    }
}

impl fmt::Debug for Login {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            registry,
            username,
            password: _,
        } = self;
        (f.debug_struct("Login"))
            .field("registry", registry)
            .field("username", username)
            .finish_non_exhaustive()
    }
}

/// The Docker engine a context reaches, and its report.
///
/// Each `docker` command it runs but `docker build` is killed if it hasn't ended in time, so
/// that each call returns within a bound: a removal at [`REMOVE_TIMEOUT`], a [push](Pusher::push)
/// at `PUSH_TIMEOUT`, any other at `CALL_TIMEOUT`, each after it starts, or one that its caller
/// bounds ([`docker_to`]) at that bound.
#[derive(Debug)]
pub struct Engine {
    report: Report,
    /// The directory on this machine that [`connect`](Self::connect) was given.
    dir: PathBuf,
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
    /// The platform whose images the engine runs.
    pub platform: Platform,
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
    /// What the engine says of the store its images are in, as pairs of a name and a value.
    driver_status: Option<Vec<(String, String)>>,
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
    /// Reaches the engine through `context`, and reports it. What it writes on this machine goes
    /// in `dir`, made once a build or a load needs it: a directory of its own for each while it
    /// runs, and Buildx's state, in `buildx`.
    ///
    /// # Errors
    ///
    /// Fails, saying what to do, if the engine can't be reached or read, or keeps no containerd
    /// image store.
    pub fn connect(context: ContextName, dir: &Path) -> Result<Self, String> {
        let Reach { endpoint, info } = reach(&context)?;
        let InfoJson {
            driver_status,
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
        check_store(&context, &driver_status.unwrap_or_default())?;
        let platform = platform(&context)?;
        let report = Report {
            context,
            endpoint,
            client_version: client_info.version,
            engine_version: server_version,
            security: Security::parse(&security_options.unwrap_or_default()),
            kernel: kernel_version,
            os: operating_system,
            platform,
        };
        Ok(Self {
            report,
            dir: dir.to_owned(),
        })
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

    /// Checks that `image`, the digest of an image's manifest that the engine has, is for
    /// `platform`, which `runs`, such as `Modal runs sandboxes on`, says what runs.
    ///
    /// # Errors
    ///
    /// Fails if it is for another platform; if the engine has no such image, as it pulls none; if
    /// `image` names anything but a manifest, such as an index; or if `docker` can't say what it
    /// is.
    pub fn check_platform(
        &self,
        image: &Image,
        platform: &Platform,
        runs: &str,
    ) -> Result<(), String> {
        // An image of another platform would run, if at all, only under emulation, beside a
        // `sandboxd` built for this one.
        let of = self.platform_of(image)?;
        if of != *platform {
            return Err(format!(
                "{image} is an image for {of}, but {runs} {platform}: name an image for {platform}"
            ));
        }
        Ok(())
    }

    /// The platform that `image` is for, or why it can't be told, as
    /// [`check_platform`](Self::check_platform) says.
    fn platform_of(&self, image: &Image) -> Result<Platform, String> {
        let context = &self.report.context;
        let name = context.as_str();
        let inspect = [
            "image",
            "inspect",
            "--format",
            "{{json .}}",
            "--",
            image.as_str(),
        ];
        // One call, rather than a listing of every image before it, as many runs that publish at
        // once each call the engine. `docker image inspect` fails alike whatever went wrong, so
        // an image it lacks is told by its text, as `image_id` tells one; and in the containerd
        // image store an image's full ID names that image alone.
        let inspected = match docker(context, inspect) {
            Ok(inspected) => inspected,
            Err(Failure::Exited { stderr, .. }) if stderr.contains("No such image") => {
                return Err(format!(
                    "the engine of context `{name}` has no image {image}: build it there, or \
                     `docker load` an archive that holds it, as Pumpkin pulls none onto it"
                ));
            }
            Err(error) => return Err(fix_reported(&error)),
        };
        let ImageJson {
            os,
            architecture,
            descriptor,
        } = serde_json::from_slice(&inspected.stdout).map_err(|error| {
            format!(
                "the JSON `docker image inspect` printed of {image} isn't of the form expected: \
                 {error}: use Docker's own command line, which prints it"
            )
        })?;
        let Some(DescriptorJson { media_type }) = descriptor else {
            return Err(format!(
                "the engine of context `{name}` doesn't say what {image} is the digest of, as \
                 `docker image inspect` gives it no `Descriptor`: use Docker 28.0 or later, whose \
                 engine and command line give one"
            ));
        };
        // An index names a manifest for each of several platforms, of which a container runs the
        // engine's: so the digest the workflow names, which the run's record holds, wouldn't be
        // the digest of what ran. A build for one platform makes an index too when it attaches
        // an attestation, as `docker build` does by default. And the engine finds no image by the
        // digest of a manifest that an index holds, so an image to name is built as a manifest
        // alone.
        if !MANIFEST_TYPES.contains(&media_type.as_str()) {
            return Err(format!(
                "{image} names an `{media_type}`, not an image's manifest: name an image built \
                 for one platform with no attestation, as by `docker build --provenance=false`, \
                 whose ID is its manifest's digest"
            ));
        }
        // Without a variant, such as arm's `v7`, as the engine's own platform names none.
        Ok(Platform { os, architecture })
    }

    /// The engine, as one that can [push](Pusher::push).
    ///
    /// # Errors
    ///
    /// Fails if the engine's Docker command line is older than `PUSHING_CLI`, or its version
    /// can't be read as one.
    pub fn pusher(&self) -> Result<Pusher<'_>, String> {
        let version = &self.report.client_version;
        // As semantic versioning orders versions, such as `28.3.1`, `28.3.0-rc.1` or
        // `26.1.5+dfsg1`: build metadata doesn't count, and a pre-release comes before its
        // release, as `28.3.0-rc.1`, which ignores the variable, does.
        let release = version
            .split_once('+')
            .map_or(version.as_str(), |(release, _)| release);
        let (release, pre) = release
            .split_once('-')
            .map_or((release, false), |(release, _)| (release, true));
        let numbers: Option<Vec<u64>> = release.split('.').map(|part| part.parse().ok()).collect();
        let recent = numbers
            .and_then(|numbers| <[u64; 3]>::try_from(numbers).ok())
            .is_some_and(|numbers| numbers > PUSHING_CLI || (numbers == PUSHING_CLI && !pre));
        if recent {
            return Ok(Pusher { engine: self });
        }
        let [major, minor, patch] = PUSHING_CLI;
        Err(format!(
            "the Docker command line of context `{}` is `{}`: expected {major}.{minor}.{patch} or \
             later, the first whose push takes its login from its environment alone",
            self.report.context.as_str(),
            version.escape_debug()
        ))
    }
}

/// An [`Engine`] whose Docker command line can push with a [`Login`] that reaches that push
/// alone ([`Engine::pusher`]).
#[derive(Debug)]
pub struct Pusher<'e> {
    engine: &'e Engine,
}

impl<'e> Pusher<'e> {
    /// The engine that pushes.
    pub fn engine(&self) -> &'e Engine {
        self.engine
    }

    /// Pushes `image`, an image the engine has, by its ID, to `repository`, such as `team/app`,
    /// of the registry that `login` names, as `<registry>/<repository>:<tag>`, which the image
    /// keeps as a tag; with `login`'s credentials, which reach that push alone: neither the
    /// operator's Docker configuration nor a credential store holds them.
    ///
    /// # Errors
    ///
    /// Fails, naming the push's target, if `docker` can't tag or push the image.
    pub fn push(
        &self,
        image: &Image,
        login: &Login,
        repository: &str,
        tag: &str,
    ) -> Result<(), String> {
        let context = &self.engine.report.context;
        let target = format!("{}/{repository}:{tag}", login.registry);
        let tagged = docker(context, ["tag", "--", image.as_str(), &target]);
        let push = ["push", "--quiet", "--", &target];
        let pushed =
            tagged.and_then(|_| docker_as(context, push, Bound::Within(PUSH_TIMEOUT), Some(login)));
        pushed
            .map(drop)
            .map_err(|error| format!("pushing `{target}` failed: {}", fix_reported(&error)))
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

/// The platform that the engine of `context` is built for.
fn platform(context: &ContextName) -> Result<Platform, String> {
    // `docker version` names the platform the engine is built for as an image's configuration
    // names one; `docker info` names the host's machine as the kernel does, such as `x86_64` for
    // `amd64`.
    let format = "{{.Server.Os}}/{{.Server.Arch}}";
    let unread = |error: String| {
        let name = context.as_str();
        format!("the platform of the Docker engine of context `{name}` can't be read: {error}")
    };
    let version = docker(context, ["version", "--format", format])
        .map_err(|error| unread(fix_reported(&error)))?;
    let printed = String::from_utf8_lossy(&version.stdout);
    let Some((os, architecture)) = printed.trim().split_once('/') else {
        return Err(unread(format!(
            "`docker version` printed `{}`: expected `<os>/<architecture>`",
            printed.trim().escape_debug()
        )));
    };
    Ok(Platform {
        os: os.to_owned(),
        architecture: architecture.to_owned(),
    })
}

/// Fails unless the engine of `context` keeps its images in the containerd image store, as
/// `status`, the `DriverStatus` of its `docker info`, says.
fn check_store(context: &ContextName, status: &[(String, String)]) -> Result<(), String> {
    // There an image's ID is the digest of what its name points to, a manifest for an image of
    // one platform, which a workflow names it by; in the classic store, it is the digest of the
    // image's configuration, which no workflow names.
    let (key, value) = CONTAINERD_STORE;
    if status.iter().any(|(name, set)| name == key && set == value) {
        return Ok(());
    }
    Err(format!(
        "the Docker engine of context `{}` doesn't keep its images in the containerd image \
         store, as `docker info` reports no `{key}` of `{value}`, so it can't find an image by \
         its manifest's digest: turn the store on, as `\"features\": {{\"containerd-snapshotter\": \
         true}}` in the engine's `daemon.json` does",
        context.as_str()
    ))
}

/// What `docker image inspect` prints of an image, as JSON.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ImageJson {
    /// The operating system the image's configuration names.
    os: String,
    /// The architecture the image's configuration names.
    architecture: String,
    /// What the image's ID is the digest of, which an engine of Docker 28.0 or later describes.
    // Moby's API marks the field experimental, liable to change without notice. An engine that
    // drops it gives none here, and one that renames it, none by this name, so either fails
    // every publish, as an engine before 28.0 does, rather than taking an image unchecked.
    descriptor: Option<DescriptorJson>,
}

/// An OCI descriptor, as `docker image inspect` prints one.
#[derive(Deserialize)]
struct DescriptorJson {
    #[serde(rename = "mediaType")]
    media_type: String,
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
fn image_id(context: &ContextName, reference: &str) -> Result<Option<String>, Failure> {
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
    docker_as(context, args, bound, None)
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

/// Runs `docker` as [`docker_to`] does, with `login`'s credentials, if given, in its environment,
/// as `DOCKER_AUTH_CONFIG`, which no command line shows.
fn docker_as<I, S>(
    context: &ContextName,
    args: I,
    bound: Bound,
    login: Option<&Login>,
) -> Result<Output, Failure>
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
    if let Some(login) = login {
        command.env("DOCKER_AUTH_CONFIG", login.auth_config());
    }
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
