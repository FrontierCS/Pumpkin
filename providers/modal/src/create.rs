//! The helper's `create`: what it is asked to make, and what it answers.
//!
//! The request states what each container of the sandbox starts with. That is `sandboxd`'s
//! argument vector, and the environment that holds what `sandboxd` alone is given. For `main`, it
//! is also the ports that get a tunnel. The helper passes each on to Modal as it is.
//!
//! `main` listens for the gateway behind a tunnel. In a sandbox that holds companions, it also
//! relays to them, behind a second tunnel. Each companion is a sidecar of Modal's, whose
//! `sandboxd` listens under TLS of its own, where only the sandbox's containers reach it
//! (`schema::sandbox::Relayed`). Modal starts a sidecar's command as root, whatever its image's
//! `USER` says, so `sandboxd` becomes the companion's user itself (`--user`), as `main`'s does.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::time::Duration;

use provider::{
    CompanionDial, Create, Created, ListenDials, Listened, Phase, Role, SandboxId, SandboxdRef,
    UnixTime,
};
use schema::ir::User;
use schema::sandbox::{
    Endpoint, Listening, RELAYED_PORT, Relayed, SANDBOXCTL_PATH, SANDBOXD_GRANT, SANDBOXD_PATH,
    SANDBOXD_TLS_CERTIFICATE, SANDBOXD_TLS_KEY,
};
use serde::{Deserialize, Serialize};

use crate::images::imported;

/// The bytes in a MiB, the unit in which Modal gives a sandbox memory.
pub(crate) const MIB: u64 = 1024 * 1024;

/// The port at which `main`'s `sandboxd` listens for the gateway, behind a tunnel.
const MAIN_PORT: u16 = 7434;

/// The port at which `main`'s `sandboxd` relays to the sandbox's companions, behind a tunnel of
/// its own.
const RELAY_PORT: u16 = 7435;

/// What a published `sandboxd`'s reference holds between its path and its digest.
const DIGEST: &str = "@sha256:";

/// The memory of `role`'s sandboxes in MiB, or why Modal can't give it.
pub(crate) fn memory_mib(role: &Role) -> Result<u64, String> {
    let memory_bytes = role.resources.memory_bytes.get();
    if !memory_bytes.is_multiple_of(MIB) {
        return Err(format!(
            "role `{}` asks for {memory_bytes} bytes of memory, which Modal gives in whole MiB: \
             ask for a multiple of {MIB}",
            role.name
        ));
    }
    Ok(memory_bytes / MIB)
}

/// The published `sandboxd`, for the helper to put into an image. It is the file at `path` on
/// this machine, and goes at each path of `at` in the image. The helper first checks that the
/// file's SHA-256 is still `sha256`.
#[derive(Serialize)]
pub(crate) struct Sandboxd<'a> {
    path: &'a str,
    sha256: &'a str,
    at: Vec<&'static str>,
}

impl<'a> Sandboxd<'a> {
    /// `sandboxd` as this provider published it, for an image that is to have it at each of `at`.
    pub(crate) fn published(
        sandboxd: &'a SandboxdRef,
        at: Vec<&'static str>,
    ) -> Result<Self, String> {
        let Some((path, sha256)) = sandboxd.as_str().rsplit_once(DIGEST) else {
            return Err(format!(
                "`{}` is no sandboxd this provider published: publish one first",
                sandboxd.as_str()
            ));
        };
        Ok(Self { path, sha256, at })
    }
}

/// What the helper's `create` is asked: a sandbox of `run`, for `role`. Its `main` runs `image`,
/// by its id in Modal, with `sandboxd` attached, and starts as `main` says.
#[derive(Serialize)]
pub(crate) struct Request<'a> {
    run: &'a str,
    role: &'a str,
    image: &'a str,
    sandboxd: Sandboxd<'a>,
    #[serde(flatten)]
    main: Start,
    /// The ports of `main` that each get a tunnel. The answer gives the tunnels in this order.
    ports: Vec<u16>,
    cpu_millis: u64,
    memory_mib: u64,
    gpu: Option<&'a str>,
    /// The sandbox's hard end, in whole seconds since the Unix epoch. The helper takes the
    /// sandbox's timeout from it as it creates it, as Modal counts that from then.
    hard_end: u64,
    /// The only source, a CIDR, that the sandbox's tunnels take connections from.
    source: String,
    /// The sandbox's companions, in their declared order.
    sidecars: Vec<Sidecar<'a>>,
}

/// What a container starts with: its command, which is `sandboxd`'s, and the variables of its
/// environment, which hold what `sandboxd` alone is given.
#[derive(Serialize)]
struct Start {
    args: Vec<String>,
    env: BTreeMap<&'static str, String>,
}

/// A companion, as the sidecar that the helper makes for it. It has its name in the sandbox, and
/// what it starts with. Its `image`, by its id in Modal, holds `sandboxd`.
#[derive(Serialize)]
struct Sidecar<'a> {
    name: &'a str,
    image: &'a str,
    #[serde(flatten)]
    start: Start,
}

/// The helper's request for the sandbox that `create` describes, whose containers are given
/// `dials`. Its class has `gpu`, as Modal names one, and its tunnels take connections from
/// `source` alone. Fails if Modal can't make that sandbox, or the provider published none of what
/// it names.
pub(crate) fn request<'a>(
    create: &'a Create,
    dials: &'a ListenDials,
    gpu: Option<&'a str>,
    source: Ipv4Addr,
) -> Result<Request<'a>, String> {
    let Create {
        run,
        // Modal's names are unique in an App, which holds every run's sandboxes, and a run's
        // names only in the run, so its sandboxes go unnamed.
        name: _,
        role,
        // Each sandbox has resources of its own, whichever instance of its role it is.
        instance: _,
        // Taken as the parameter `gpu`.
        class: _,
        image,
        sandboxd,
        ops,
        model,
        hard_end,
        by,
    } = create;
    let ListenDials { main, companions } = dials;
    let Listening { grant } = main;
    let mut at = vec![SANDBOXD_PATH];
    let mut args = listening(MAIN_PORT, *by, role.user);
    if *ops {
        at.push(SANDBOXCTL_PATH);
        args.push("--ops".to_owned());
    }
    if *model {
        args.push("--model".to_owned());
    }
    let mut ports = vec![MAIN_PORT];
    // `sandboxd` refuses a relay that carries to no companion, so a sandbox without any has none.
    if !companions.is_empty() {
        args.extend(["--relay".to_owned(), format!("0.0.0.0:{RELAY_PORT}")]);
        for companion in companions {
            args.extend(["--relayed".to_owned(), companion.name.to_string()]);
        }
        ports.push(RELAY_PORT);
    }
    let sidecars = companions.iter().map(|companion| sidecar(companion, *by));
    Ok(Request {
        run: run.as_str(),
        role: role.name.as_str(),
        image: imported(image)?,
        sandboxd: Sandboxd::published(sandboxd, at)?,
        main: Start {
            args,
            env: BTreeMap::from([(SANDBOXD_GRANT, grant.digits())]),
        },
        ports,
        cpu_millis: role.resources.cpu_millis.get(),
        memory_mib: memory_mib(role)?,
        gpu,
        hard_end: hard_end.since_epoch().as_secs(),
        source: format!("{source}/32"),
        sidecars: sidecars.collect::<Result<_, _>>()?,
    })
}

/// The sidecar of the companion that `companion` describes, in a sandbox whose start is due by
/// `by`.
fn sidecar(companion: &CompanionDial, by: UnixTime) -> Result<Sidecar<'_>, String> {
    let CompanionDial {
        name,
        image,
        user,
        // Each command is given its directory and environment as it is started, as `main`'s are.
        cwd: _,
        env: _,
        relayed: Relayed { grant, identity },
    } = companion;
    let mut args = listening(RELAYED_PORT, by, *user);
    args.push("--tls".to_owned());
    let env = BTreeMap::from([
        (SANDBOXD_GRANT, grant.digits()),
        (SANDBOXD_TLS_CERTIFICATE, identity.certificate_digits()),
        (SANDBOXD_TLS_KEY, identity.key_digits()),
    ]);
    Ok(Sidecar {
        name: name.as_str(),
        image: imported(image)?,
        start: Start { args, env },
    })
}

/// The command of a `sandboxd` that becomes `user` as it starts. It listens at `port`, at every
/// address of its container, until `by`.
// Modal starts it as root, behind its own first process, `dumb-init`.
fn listening(port: u16, by: UnixTime, user: User) -> Vec<String> {
    let User { uid, gid } = user;
    let by = by.since_epoch().as_secs();
    let args = [
        SANDBOXD_PATH.to_owned(),
        format!("0.0.0.0:{port}"),
        "--listen".to_owned(),
        "--by".to_owned(),
        by.to_string(),
        "--user".to_owned(),
        format!("{uid}:{gid}"),
    ];
    args.into()
}

/// What the helper's `create` answers: the sandbox's id, and the steps that the helper timed. Its
/// `tunnels` are those to the request's ports, in their order.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Made {
    id: String,
    tunnels: Vec<Tunnel>,
    phases: Vec<Timed>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Tunnel {
    host: String,
    port: u16,
}

/// A step of the helper's `create`, with when it began and how long it took, in nanoseconds. It
/// began by the machine's monotonic clock, which `stdx::time::Machine` reads too.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Timed {
    name: String,
    began_ns: u64,
    took_ns: u64,
}

impl Timed {
    /// The step as a phase of a create that began at `began`, by the machine's monotonic clock.
    fn phase(self, began: Duration) -> Phase {
        let Self {
            name,
            began_ns,
            took_ns,
        } = self;
        Phase {
            name,
            // The helper reads the clock after `began`, so only its bug makes a step begin before.
            // Such a step shows at the create's start, rather than fail the create.
            began: Duration::from_nanos(began_ns).saturating_sub(began),
            took: Duration::from_nanos(took_ns),
        }
    }
}

impl Made {
    /// The sandbox as its create returns it. The create began at `began`, by the machine's
    /// monotonic clock, and its request had a relay if `relayed`.
    ///
    /// Fails, naming the sandbox, if the answer isn't a tunnel for each port of that request, or a
    /// tunnel is no endpoint.
    pub(crate) fn listened(self, began: Duration, relayed: bool) -> Result<Listened, String> {
        let Self {
            id,
            tunnels,
            phases,
        } = self;
        let miscounted = |tunnels: Vec<Tunnel>| {
            format!(
                "the Modal helper's `create` gave sandbox {id}'s tunnels as a list of {}: \
                 expected one for each port that it was asked for",
                tunnels.len()
            )
        };
        let endpoint = |what: &str, tunnel: Tunnel| {
            Endpoint::new(tunnel.host, tunnel.port).map_err(|error| {
                format!("Modal's tunnel to sandbox {id}'s {what} isn't an endpoint: {error}")
            })
        };
        let (main, relay) = if relayed {
            let [main, relay] = <[Tunnel; 2]>::try_from(tunnels).map_err(miscounted)?;
            (main, Some(endpoint("relay", relay)?))
        } else {
            let [main] = <[Tunnel; 1]>::try_from(tunnels).map_err(miscounted)?;
            (main, None)
        };
        let main = endpoint("`main`", main)?;
        let phases = phases.into_iter().map(|timed| timed.phase(began));
        let created = Created {
            phases: phases.collect(),
            id: SandboxId::from(id),
        };
        Ok(Listened {
            created,
            main,
            relay,
        })
    }
}
