//! The helper's `create` and `fill`: what each is asked to make, and what it answers.
//!
//! The request states what each container of the sandbox starts with. That is `sandboxd`'s
//! argument vector, and the environment that holds what `sandboxd` alone is given. For `main`, it
//! is also the ports that get a tunnel.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::time::Duration;

use provider::{
    CompanionDial, Create, Created, HeldAsset, ListenDials, Listened, Phase, Role, RunTag,
    SandboxId, SandboxdRef, UnixTime,
};
use schema::ir::{AssetFile, AssetFiles, AssetPath, User};
use schema::sandbox::{
    Endpoint, LISTEN_PORT, Listening, Relayed, SANDBOXCTL_PATH, SANDBOXD_GRANT, SANDBOXD_PATH,
    SANDBOXD_TLS_CERTIFICATE, SANDBOXD_TLS_KEY,
};
use serde::{Deserialize, Serialize};

use crate::images::{ImageId, asset_image, imported};

/// The bytes in a MiB, the unit in which Modal gives a sandbox memory.
const MIB: u64 = 1024 * 1024;

/// The port at which `main`'s `sandboxd` relays to the sandbox's companions, behind a tunnel of
/// its own.
// The port after `LISTEN_PORT`, which IANA's registry leaves unassigned too.
const RELAY_PORT: u16 = 7435;

/// What a published `sandboxd`'s reference holds between its path and its digest.
const DIGEST: &str = "@sha256:";

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

/// How many companions a sandbox holds beside its own container, each a sidecar of Modal's. It
/// holds one for each `cpu_millis_each` thousandths of a CPU and `memory_bytes_each` that its role
/// needs, and `most` at most.
struct Companions {
    cpu_millis_each: u64,
    memory_bytes_each: u64,
    most: usize,
}

/// The companions that a sandbox of any class holds.
// Modal refuses a sidecar past what the sandbox reserves: its refusal says "limit is derived from
// sandbox reservation". It counts no container of the sandbox's own. Its refusals at that edge
// show that each sidecar takes 32 thousandths of a CPU and 32 MiB of the reservation. Its guide
// caps them at 250 whatever the reservation: https://modal.com/docs/guide/sandbox-sidecars
// A sandbox reserves what its role needs (`request`), which is what `companions_held` counts by.
const COMPANIONS: Companions = Companions {
    cpu_millis_each: 32,
    memory_bytes_each: 32 * MIB,
    most: 250,
};

/// Fails if `role` declares more companions than its sandboxes hold ([`COMPANIONS`]), saying what
/// would hold them.
pub(crate) fn companions_held(role: &Role) -> Result<(), String> {
    let Companions {
        cpu_millis_each,
        memory_bytes_each,
        most,
    } = COMPANIONS;
    let (cpu_millis, memory_bytes) = (role.resources.cpu_millis, role.resources.memory_bytes);
    let fewest = (cpu_millis.get() / cpu_millis_each).min(memory_bytes.get() / memory_bytes_each);
    // A count past `usize::MAX` is past `most` too.
    let held = usize::try_from(fewest).unwrap_or(usize::MAX).min(most);
    let declared = role.companions;
    if declared <= held {
        return Ok(());
    }
    let s = if declared == 1 { "" } else { "s" };
    let asked = format!("role `{}` declares {declared} companion{s}", role.name);
    let fewer = match held {
        0 => "declare none".to_owned(),
        _ => format!("declare at most {held}"),
    };
    if held == most {
        return Err(format!(
            "{asked}, and a Modal sandbox holds at most {most}: {fewer}"
        ));
    }
    let holds = format!(
        "{asked}, and a Modal sandbox holds {held} for needs of {cpu_millis} thousandths of a \
         CPU and {}",
        binary(memory_bytes.get())
    );
    if declared > most {
        return Err(format!("{holds}: {fewer}"));
    }
    // In `u128`, where no product of a count and an amount overflows.
    let each = |amount: u64| declared as u128 * u128::from(amount);
    // Only what falls short.
    let cpu = (each(cpu_millis_each) > u128::from(cpu_millis.get()))
        .then(|| format!("{} thousandths of a CPU", each(cpu_millis_each)));
    let memory = (each(memory_bytes_each) > u128::from(memory_bytes.get()))
        .then(|| binary(each(memory_bytes_each)));
    let more = match (cpu, memory) {
        (Some(cpu), None) => cpu,
        (None, Some(memory)) => memory,
        (Some(cpu), Some(memory)) => format!("{cpu} and {memory}"),
        // `held`, below `most`, is the fewer of the counts that the CPU and the memory give, each
        // rounded down. So `declared`, which is more, takes more than the role needs of one.
        (None, None) => {
            unreachable!("a sandbox short of companions below its cap is short of CPU or memory")
        }
    };
    Err(format!("{holds}: need at least {more}, or {fewer}"))
}

/// `bytes` exactly, in the largest binary unit that divides it, as "3 GiB" or "1536 MiB".
fn binary(bytes: impl Into<u128>) -> String {
    let bytes = bytes.into();
    let units = [(1 << 30, "GiB"), (1 << 20, "MiB"), (1 << 10, "KiB")];
    match units
        .into_iter()
        .find(|(unit, _)| bytes.is_multiple_of(*unit))
    {
        Some((unit, name)) => format!("{} {name}", bytes / unit),
        None => format!("{bytes} bytes"),
    }
}

// Modal keeps its own files here in each container. An image mounted here hides them from Modal,
// which then refuses each later mount in the sandbox as "not a directory".
const MODAL_FILES: &str = "/__modal";

pub(crate) fn asset_held(path: &AssetPath) -> Result<(), String> {
    let rest = path.as_str().strip_prefix(MODAL_FILES);
    if rest.is_some_and(|rest| rest.is_empty() || rest.starts_with('/')) {
        return Err(format!(
            "Modal keeps its own files in `{MODAL_FILES}`: hold the asset outside it"
        ));
    }
    Ok(())
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
    /// The images that `main` mounts, in their role's order.
    assets: Vec<Mount<'a>>,
}

/// An image that `main` mounts at `path`. Its `image` is a bare id in Modal, `im-…`, with no
/// `@sha256:` digest.
#[derive(Serialize)]
struct Mount<'a> {
    path: &'a str,
    image: &'a str,
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
        assets,
        sandboxd,
        ops,
        model,
        hard_end,
        by,
    } = create;
    let ListenDials { main, companions } = dials;
    let Listening { grant } = main;
    let mut at = vec![SANDBOXD_PATH];
    let mut args = listening(*by, role.user);
    if *ops {
        at.push(SANDBOXCTL_PATH);
        args.push("--ops".to_owned());
    }
    if *model {
        args.push("--model".to_owned());
    }
    let mut ports = vec![LISTEN_PORT];
    // `sandboxd` refuses a relay that carries to no companion, so a sandbox without any has none.
    if !companions.is_empty() {
        args.extend(["--relay".to_owned(), format!("0.0.0.0:{RELAY_PORT}")]);
        for companion in companions {
            args.extend(["--relayed".to_owned(), companion.name.to_string()]);
        }
        ports.push(RELAY_PORT);
    }
    let sidecars = companions.iter().map(|companion| sidecar(companion, *by));
    let mounts = (assets.iter()).map(|HeldAsset { path, reference }| {
        Ok(Mount {
            path: path.as_str(),
            image: asset_image(reference)?,
        })
    });
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
        assets: mounts.collect::<Result<_, String>>()?,
    })
}

/// The sidecar of the companion that `companion` describes, in a sandbox whose start is due by
/// `by`.
fn sidecar(companion: &CompanionDial, by: UnixTime) -> Result<Sidecar<'_>, String> {
    let CompanionDial {
        name,
        image,
        user,
        relayed: Relayed { grant, identity },
    } = companion;
    let mut args = listening(by, *user);
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

/// The command of a `sandboxd` that becomes `user` as it starts. It listens at `LISTEN_PORT`, at
/// every address of its container, until `by`.
// Modal starts it as root, behind its own first process, `dumb-init`.
fn listening(by: UnixTime, user: User) -> Vec<String> {
    let User { uid, gid } = user;
    let by = by.since_epoch().as_secs();
    let args = [
        SANDBOXD_PATH.to_owned(),
        format!("0.0.0.0:{LISTEN_PORT}"),
        "--listen".to_owned(),
        "--by".to_owned(),
        by.to_string(),
        "--user".to_owned(),
        format!("{uid}:{gid}"),
    ];
    args.into()
}

// It bounds a fill's fetch of all of an asset's files. It is sized only to end the fetch of a
// server that trickles, as the engine's `PUSH_TIMEOUT` ends a push that hangs. At 10 MiB a
// second, it admits 70 GiB.
pub(crate) const FETCH_TIMEOUT: Duration = Duration::from_hours(2);

// Each connect and each read of a fill's fetch may wait this long, the time that cargo lets a
// download stall:
// https://github.com/rust-lang/cargo/blob/ecb9851afd3095e988daaa35a48bc7f3cb748e04/src/cargo/util/network/http.rs#L202-L226
const STALL_TIMEOUT: Duration = Duration::from_secs(30);

// It turns a snapshot that hangs into a failure, and nothing else sizes it. A snapshot's time grows
// with its files, and the SDK's default of 55 s leaves large files little room:
// https://github.com/modal-labs/modal-client/blob/e8a24c8e304766d1b3b6905f297e3f43d08e1d93/py/modal/sandbox/_sandbox.py#L863-L870
pub(crate) const SNAPSHOT_TIMEOUT: Duration = Duration::from_mins(10);

/// The CPU and memory of a fill's sandbox, with room for the fetches that `FETCH` runs at once,
/// and their hashes. Its files go to the mounted image, so memory doesn't cap an asset's size.
const FILL_CPU_MILLIS: u64 = 4000;
const FILL_MEMORY_MIB: u64 = 4096;

const FILL_DIR: &str = "/fill";

const FILL_PYTHON: &str = "3.12";

/// The program of a fill's fetch, which `python3` runs with the directory, the fetch's bound and
/// each read's bound, in seconds. It takes the files as JSON on its standard input. It exits 0 once
/// each file is in the directory with its size and SHA-256, and all beneath it is read-only.
/// Otherwise it prints why the first file in the input's order failed, and exits 1.
// It follows at most 40 redirects, as Bazel's downloader does, and only those to `https`:
// https://github.com/bazelbuild/bazel/blob/06deebfb5b73f848de5a0ea0e00fcfaa26788d1f/src/main/java/com/google/devtools/build/lib/bazel/repository/downloader/HttpConnector.java#L56
const FETCH: &str = r#"
import hashlib, json, os, sys, time, urllib.error, urllib.request
from concurrent.futures import ThreadPoolExecutor

root, bound, stall = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
deadline = time.monotonic() + bound


class HttpsOnly(urllib.request.HTTPRedirectHandler):
    max_redirections = 40

    def redirect_request(self, request, fp, code, msg, headers, url):
        if not url.startswith("https://"):
            raise urllib.error.URLError(f"a redirect leads to {url}, which isn't `https`")
        return super().redirect_request(request, fp, code, msg, headers, url)


opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), HttpsOnly())


def fetch(file):
    path, url, size, sha256 = file["path"], file["url"], file["size"], file["sha256"]
    to = os.path.join(root, path)
    os.makedirs(os.path.dirname(to), exist_ok=True)
    digest, fetched, most = hashlib.sha256(), 0, size + 1
    try:
        with opener.open(url, timeout=stall) as response, open(to, "xb") as kept:
            while fetched < most:
                if time.monotonic() > deadline:
                    return f"fetching `{path}` from {url} failed: the fetch took over {bound} s"
                chunk = response.read1(min(1 << 20, most - fetched))
                if not chunk:
                    break
                digest.update(chunk)
                kept.write(chunk)
                fetched += len(chunk)
    except Exception as error:
        return f"fetching `{path}` from {url} failed: {type(error).__name__}: {error}"
    if fetched != size:
        held = f"ends after {fetched} of its {size} bytes"
        if fetched > size:
            held = f"holds more than its {size} bytes"
        return f"`{path}` from {url} {held}: give the size of the file that the URL serves"
    if digest.hexdigest() != sha256:
        return (
            f"`{path}` from {url} has the SHA-256 sha256:{digest.hexdigest()}, not the pinned "
            f"sha256:{sha256}: give the SHA-256 of the file that the URL serves"
        )
    os.chmod(to, 0o444)
    return None


files = json.load(sys.stdin)
with ThreadPoolExecutor(8) as pool:
    failed = [why for why in pool.map(fetch, files) if why is not None]
if failed:
    print(failed[0], file=sys.stderr)
    sys.exit(1)
dirs = set()
for file in files:
    parts = file["path"].split("/")[:-1]
    dirs.update(os.path.join(root, *parts[:count]) for count in range(1, len(parts) + 1))
for dir in sorted(dirs, key=len, reverse=True):
    os.chmod(dir, 0o555)
"#;

/// What the helper's `fill` is asked: an image of an asset's `files`, made as `how` says, by a
/// sandbox that carries the tag of `run`.
#[derive(Serialize)]
pub(crate) struct Fill<'a> {
    #[serde(flatten)]
    pub(crate) how: Filling,
    run: &'a str,
    files: Vec<Fetched<'a>>,
}

/// How a fill makes an image, whatever its files. Its sandbox runs `args`, on Modal's
/// `debian_slim` with `python`, and has Internet access, `cpu_millis` and `memory_mib`. Modal ends
/// it after `timeout_seconds`. It mounts an empty image at `at`, then runs `fetch` with the files
/// as JSON on its standard input. Once that exits 0, the helper snapshots `at` within
/// `snapshot_seconds`, as an image that expires after `ttl_seconds`, or never.
#[derive(Serialize)]
pub(crate) struct Filling {
    args: [&'static str; 2],
    python: &'static str,
    cpu_millis: u64,
    memory_mib: u64,
    timeout_seconds: u64,
    at: &'static str,
    fetch: Vec<String>,
    snapshot_seconds: u64,
    ttl_seconds: Option<u64>,
}

/// A file of an asset, as a fill's fetch takes it: its `sha256` is the hexadecimal digits alone.
#[derive(Serialize)]
struct Fetched<'a> {
    path: &'a str,
    url: &'a str,
    size: u64,
    sha256: &'a str,
}

/// The helper's request for an image of `files`, for `run`, from a sandbox that Modal ends after
/// `timeout`.
pub(crate) fn fill<'a>(run: &'a RunTag, files: &'a AssetFiles, timeout: Duration) -> Fill<'a> {
    let files = (files.as_slice().iter()).map(|file| {
        let AssetFile {
            path,
            url,
            size,
            sha256,
        } = file;
        Fetched {
            path: path.as_str(),
            url: url.as_str(),
            size: *size,
            sha256: sha256.hex(),
        }
    });
    let fetch = [
        "python3",
        "-I",
        "-c",
        FETCH,
        FILL_DIR,
        &FETCH_TIMEOUT.as_secs().to_string(),
        &STALL_TIMEOUT.as_secs().to_string(),
    ];
    let how = Filling {
        // The sandbox waits while its fetch runs and its directory is snapshot, until it is ended.
        args: ["sleep", "infinity"],
        python: FILL_PYTHON,
        cpu_millis: FILL_CPU_MILLIS,
        memory_mib: FILL_MEMORY_MIB,
        timeout_seconds: timeout.as_secs(),
        at: FILL_DIR,
        fetch: fetch.map(str::to_owned).into(),
        snapshot_seconds: SNAPSHOT_TIMEOUT.as_secs(),
        // So that the image lasts as long as the machine's record of it.
        ttl_seconds: None,
    };
    Fill {
        how,
        run: run.as_str(),
        files: files.collect(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Filled {
    pub(crate) id: ImageId,
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
