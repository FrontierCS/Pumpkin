//! The Docker provider: a sandbox for a role as a container that needs no root to start.
//!
//! [`Docker`], a [`Provider`], makes each sandbox a container on the operator's [`Engine`], with:
//!
//! - no network but loopback; the dial's socket, in a host directory mounted read-only, is the
//!   only way out, and files move only over the session;
//! - the role's [`confinement`]: its user from its first process, no capabilities,
//!   `no-new-privileges`, CPU, memory, swap and process limits, no core file, `/pumpkin` and a
//!   read-only `/tmp`;
//! - `sandboxd`, mounted read-only, as its first process, dialing the [`Dial`] its create
//!   gives, and whose output alone is the container's log, which holds at most 1 MiB;
//! - for a role that makes ops ([`Create::ops`]), [`sandboxctl`](SANDBOXCTL_PATH), mounted
//!   read-only, and `sandboxd` serving ops; other sandboxes are [passive](schema::sandbox);
//! - for a role with a model route ([`Create::model`]), `sandboxd` serving the model endpoint.
//!
//! On the engine, it creates only containers, labelled with their run's [`RunTag`] for
//! [`Provider::list`], and their anonymous volumes, and pulls no image. It needs an engine that
//! enforces every limit that a container has.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;
use std::path::Path;
use std::process::Output;

use engine::{ContextName, Engine, Failure, confinement, docker, fix_reported, image_id};
use provider::Provider;
use schema::ir::{MILLIS_PER_CPU, Name, Resources};
use schema::provider::{
    Class, Console, Create, Created, Dial, Listed, Local, Published, Ready, Role, RunTag,
    SOCKET_NAME, SandboxId,
};
use schema::sandbox::SANDBOXCTL_PATH;
use serde::Deserialize;
use serde_json::json;
use stdx::Context;

/// The name of the provider's one class of sandbox, a container on the engine's host, which
/// gives a role as many CPUs and as much memory as the host has.
const CLASS: &str = "container";

/// The most seconds a sandbox's start takes, as [`Class::startup_seconds`] declares it.
const STARTUP_SECONDS: NonZeroU64 = NonZeroU64::new(540).unwrap();

/// Where `sandboxd` is mounted in the container.
const SANDBOXD_PATH: &str = "/.pumpkin/sandboxd";

/// Where the session's socket directory is mounted in the container.
const GATEWAY_DIR: &str = "/.pumpkin/gateway";

/// The label that carries a container's run tag.
const RUN_LABEL: &str = "pumpkin.run";

/// The label that carries the name of a container's role.
const ROLE_LABEL: &str = "pumpkin.role";

/// The provider of sandboxes as containers on an [`Engine`], which every container of its runs
/// in.
#[derive(Debug)]
pub struct Docker<'e> {
    engine: &'e Engine,
    /// What [`Provider::ready`] gives: the engine's report as JSON, and the one class.
    ready: Ready,
}

/// What `docker info` prints of the engine's host and the limits it enforces, as JSON.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
#[expect(
    clippy::struct_excessive_bools,
    reason = "`docker info` reports whether the engine supports each limit as a boolean"
)]
struct HostJson {
    /// Whether the engine supports memory limits, which a container's confinement sets.
    memory_limit: bool,
    /// Whether the engine supports swap limits, which a container's confinement sets.
    swap_limit: bool,
    /// Whether the engine supports CPU limits, which a container's confinement sets.
    cpu_cfs_quota: bool,
    /// Whether the engine supports process limits, which a container's confinement sets.
    pids_limit: bool,
    /// The CPUs of the engine's host, which a container's CPU limit can't pass.
    #[serde(rename = "NCPU")]
    ncpu: NonZeroU64,
    /// The memory of the engine's host, in bytes.
    mem_total: NonZeroU64,
}

impl<'e> Docker<'e> {
    /// The provider on `engine`.
    ///
    /// # Errors
    ///
    /// Fails, saying what to do, if the engine can't be read or can't enforce every container's
    /// limits.
    pub fn connect(engine: &'e Engine) -> Result<Self, String> {
        let report = engine.report();
        let info =
            docker(&report.context, ["info", "--format", "{{json .}}"]).map_err(fix_reported)?;
        let HostJson {
            memory_limit,
            swap_limit,
            cpu_cfs_quota,
            pids_limit,
            ncpu,
            mem_total,
        } = serde_json::from_slice(&info.stdout).map_err(|error| {
            format!(
                "the JSON `docker info` printed isn't of the form expected: {error}: use \
                 Docker's own command line, which prints it"
            )
        })?;
        check_limits(
            &report.context,
            [
                ("MemoryLimit", memory_limit),
                ("SwapLimit", swap_limit),
                ("CpuCfsQuota", cpu_cfs_quota),
                ("PidsLimit", pids_limit),
            ],
        )?;
        let Some(cpu_millis) = NonZeroU64::new(ncpu.get().saturating_mul(MILLIS_PER_CPU)) else {
            unreachable!("a product of CPUs and `MILLIS_PER_CPU`, neither of them 0, is 0")
        };
        let host = Resources {
            cpu_millis,
            memory_bytes: mem_total,
        };
        Ok(Self {
            engine,
            ready: ready(report, host),
        })
    }

    /// The context `docker` reaches the engine through.
    fn context(&self) -> &'e ContextName {
        &self.engine.report().context
    }
}

impl Provider for Docker<'_> {
    fn ready(&self) -> &Ready {
        &self.ready
    }

    // A sandbox runs what the engine has, as it pulls nothing, so a publish checks that the
    // engine has each image and that `sandboxd` can be mounted, and names each as it is.
    fn publish(&self, locals: &[Local]) -> Result<Vec<Published>, String> {
        let context = self.context();
        let published = locals.iter().map(|local| match local {
            Local::Role { image } => {
                let image = image.to_string();
                match image_id(context, &image) {
                    Ok(Some(_)) => Ok(Published::from(image)),
                    Ok(None) => Err(format!(
                        "the engine of context `{}` has no image {image}: build or load it \
                         there first, as the provider pulls none",
                        context.as_str()
                    )),
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
        let context = self.context();
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
        let context = self.context();
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
        let ids = of_run(self.context(), run)?;
        let listed = ids.into_iter().map(|id| Listed {
            id: SandboxId::from(id),
        });
        Ok(listed.collect())
    }

    fn console(&self, run: &RunTag, id: &SandboxId) -> Result<Console, String> {
        let context = self.context();
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

/// What [`Provider::ready`] gives of an engine that `report` describes, whose host has `host`.
fn ready(report: &engine::Report, host: Resources) -> Ready {
    // `Name` checks its value as it is made, and has no constant constructor.
    let Ok(name) = Name::try_from(CLASS.to_owned()) else {
        unreachable!("the class `{CLASS}` has no name, though it is a constant that is a name")
    };
    let class = Class {
        resources: host,
        startup_seconds: STARTUP_SECONDS,
    };
    Ready {
        report: json!(report),
        classes: BTreeMap::from([(name, class)]),
    }
}

/// Fails unless the engine of `context` supports each of `limits`, a name in `docker info`
/// and whether it reports the limit supported. Those are the limits a container's confinement
/// sets (`engine::confinement`), and the engine discards one it doesn't support with only a warning from `docker create`, so a
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
        model,
        dial,
    } = create;
    let Dial::Socket { dir } = dial;
    let Role {
        name,
        user,
        resources,
    } = role;
    let sandboxd = Path::new(sandboxd.as_str());
    let mut args = vec![
        "create".to_owned(),
        format!("--label={RUN_LABEL}={}", run.as_str()),
        format!("--label={ROLE_LABEL}={name}"),
        "--network=none".to_owned(),
    ];
    args.extend(confinement(*user, *resources));
    args.extend([
        bind_mount(sandboxd, SANDBOXD_PATH)?,
        bind_mount(dir, GATEWAY_DIR)?,
    ]);
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
    if *model {
        args.push("--model".to_owned());
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
        // Docker 29 exits 0 here; older engines, down to the engine's floor, Docker 25.0, fail.
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
