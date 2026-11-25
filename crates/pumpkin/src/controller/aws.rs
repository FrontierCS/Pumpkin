//! Finding a controller's EC2 instance by its tag, starting it, and waiting until its `sshd`
//! answers at the address that the configuration's `ssh_interface` names.

use std::fs::{self, DirBuilder};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use std::{env, io, thread};

use aws::iam::Call;
use aws::{Bound, Cli, Region};
use jiff::Timestamp;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use stdx::process::{self, Echo, Environment, Run};
use stdx::{Context as _, Deadline};

use super::{Located, Name, SHORT_TIMEOUT};
use crate::config::Interface;
use crate::iam::CLIENT;
use crate::say;

// The tag that `deploy/aws` puts on the controller's instance, valued with the controller's name.
const TAG: &str = "pumpkin:controller";

// How long a controller has, from its launch, to run and have its `sshd` answer. An instance that
// EC2 starts does both within a few minutes; past this, something else is wrong.
const START_TIMEOUT: Duration = Duration::from_mins(10);

const START_POLL: Duration = Duration::from_secs(5);

// How long a probe of `sshd` waits to connect. Inside the controller's VPC a connection takes
// milliseconds, so a longer wait would only delay the next probe. SkyPilot's:
// https://github.com/skypilot-org/skypilot/blob/052914aaccb29a9a2364dd73654f2e61f0af42a0/sky/provision/provisioner.py#L500-L501
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Reservations<T> {
    reservations: Vec<Reservation<T>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Reservation<T> {
    instances: Vec<T>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Instance {
    #[serde(rename = "InstanceId")]
    id: String,
    state: State,
    /// When it last started, as RFC 3339.
    launch_time: String,
    private_ip_address: Option<IpAddr>,
    public_ip_address: Option<IpAddr>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct State {
    name: String,
}

/// The directory that holds this machine's `ssh` key to its controllers, `key` and `key.pub`,
/// which this makes if there is none, and `known_hosts`, which pins the host key that each
/// controller showed first, by its [`Reached::alias`](super::Reached::alias). Processes may call
/// it at once.
// A credential, so beside the configuration, outside every workspace; one for the machine, so
// that a controller's admin lists it once, whatever workspace reaches it.
pub(super) fn kept() -> Result<PathBuf, String> {
    let home = env::var_os("HOME").filter(|home| !home.is_empty()).ok_or(
        "HOME is unset or empty: set it, as this machine's key to its controllers is below it",
    )?;
    let config = Path::new(&home).join(".config/pumpkin");
    let kept = config.join("ssh");
    if kept.join("key.pub").exists() {
        return Ok(kept);
    }
    let made = DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&config);
    made.context(config.display())?;
    // Made apart and moved in whole, so that of the commands that start at once on a new machine,
    // the first to move its key in is the one all of them use; the others' go.
    let mut made = tempfile::Builder::new();
    let made = made
        .prefix(".ssh-")
        .permissions(fs::Permissions::from_mode(0o700));
    let made = made.tempdir_in(&config);
    let made = made.context(config.display())?;
    let mut keygen = process::command("ssh-keygen", Environment::Only(&["PATH"]));
    keygen.args(["-q", "-t", "ed25519", "-N", "", "-C", "pumpkin", "-f"]);
    keygen.arg(made.path().join("key"));
    let how = Run::new(process::Bound::Within(SHORT_TIMEOUT), Echo::Silent);
    process::run(&mut keygen, how).map_err(|failure| failure.to_string())?;
    if let Err(error) = fs::rename(made.path(), &kept)
        && !kept.join("key.pub").exists()
    {
        return Err(error).context(kept.display());
    }
    Ok(kept)
}

/// Finds controller `name` in `region` by its tag, starts it if it is stopped, and waits until its
/// `sshd` answers at its address that `interface` names, and returns where it is.
pub(super) fn find(region: &Region, name: &Name, interface: Interface) -> Result<Located, String> {
    let cli = Cli::connect(region.clone())?;
    let live = |instance: &Instance| live(&instance.state.name);
    let found: Vec<Instance> = instances(&cli, &CLIENT.describe, &tagged(name))?;
    let mut instances = found.into_iter().filter(live);
    let (Some(instance), None) = (instances.next(), instances.next()) else {
        return Err(format!(
            "no one instance in {} carries the tag {TAG}={name}: name the controller that \
             `deploy/aws` made, with the `name` and the region it was applied with",
            region.as_str()
        ));
    };
    ready(&cli, name, &instance.id, interface)
}

/// The filter of `aws ec2 describe-instances` that matches the instances of controller `name` by
/// their tag. It matches terminated instances too.
pub(crate) fn tagged(name: &Name) -> String {
    format!("Name=tag:{TAG},Values={name}")
}

/// The tag of controller `name`'s instance, as `key=value`.
pub(crate) fn tag(name: &Name) -> String {
    format!("{TAG}={name}")
}

/// Whether an instance in `state`, as EC2 names it, is live. An instance is live unless it is
/// `shutting-down` or `terminated`.
pub(crate) fn live(state: &str) -> bool {
    !matches!(state, "shutting-down" | "terminated")
}

/// The instances that `filter` matches, each as `T` reads what `aws ec2 describe-instances` prints
/// of it, by `call`.
pub(crate) fn instances<T: DeserializeOwned>(
    cli: &Cli,
    call: &'static Call,
    filter: &str,
) -> Result<Vec<T>, String> {
    let found: Reservations<T> = cli.json(call, &["--filters", filter], Bound::Call)?;
    Ok(found
        .reservations
        .into_iter()
        .flat_map(|found| found.instances)
        .collect())
}

fn ready(cli: &Cli, name: &Name, id: &str, interface: Interface) -> Result<Located, String> {
    let by = Deadline::after(START_TIMEOUT);
    let mut started = None;
    loop {
        let instance = instance(cli, name, id)?;
        match instance.state.name.as_str() {
            "running" => return answering(name, instance, interface, started),
            "stopped" if started.is_none() => {
                say(format_args!(
                    "starting controller `{name}`, {id}, which is stopped"
                ));
                start(cli, id)?;
                started = Some(Instant::now());
                continue;
            }
            // On its way to running, or to being stopped, which a start then follows.
            "pending" | "stopping" | "stopped" => {}
            state => {
                return Err(format!(
                    "controller `{name}`, {id}, is {state}: start it again once it can be"
                ));
            }
        }
        if by.remaining().is_none() {
            return Err(format!(
                "controller `{name}`, {id}, is {} after {} s: expected it to run by then",
                instance.state.name,
                START_TIMEOUT.as_secs()
            ));
        }
        thread::sleep(START_POLL);
    }
}

/// The state that EC2 gives instance `id` of controller `name`, as it names it, such as `running`
/// or `stopped`.
pub(super) fn state(cli: &Cli, name: &Name, id: &str) -> Result<String, String> {
    instance(cli, name, id).map(|instance| instance.state.name)
}

/// Starts instance `id`, as `start-instances` does: one that runs or is starting is left so.
pub(super) fn start(cli: &Cli, id: &str) -> Result<(), String> {
    let start = ["--instance-ids", id];
    cli.run(&CLIENT.start, &start, Bound::Call).map(drop)
}

fn instance(cli: &Cli, name: &Name, id: &str) -> Result<Instance, String> {
    let filter = format!("Name=instance-id,Values={id}");
    let found: Vec<Instance> = instances(cli, &CLIENT.describe, &filter)?;
    found.into_iter().next().ok_or_else(|| {
        format!("controller `{name}`, {id}, is no longer there: run the command again")
    })
}

/// Waits until the `sshd` of `instance`, which runs, answers at its address that `interface`
/// names, until `START_TIMEOUT` after its launch, and says how long that took if this command
/// `started` it.
fn answering(
    name: &Name,
    instance: Instance,
    interface: Interface,
    started: Option<Instant>,
) -> Result<Located, String> {
    let Instance {
        id,
        state: _,
        launch_time,
        private_ip_address,
        public_ip_address,
    } = instance;
    let (address, expected) = match interface {
        Interface::PrivateIp => (
            private_ip_address,
            "the one of its subnet that `deploy/aws` gives it",
        ),
        Interface::PublicIp => (
            public_ip_address,
            "the one that EC2 gives it at each start, as `deploy/aws` asks",
        ),
    };
    let Some(address) = address else {
        return Err(format!(
            "controller `{name}`, {id}, runs with no {}: expected {expected}",
            interface.address()
        ));
    };
    let launched: Timestamp = launch_time.parse().map_err(|error| {
        format!(
            "the launch time that `aws ec2 describe-instances` printed, `{launch_time}`, isn't \
             a time: {error}"
        )
    })?;
    let launched = Duration::try_from(launched.as_duration()).unwrap_or_default();
    let by = Deadline::at_unix(launched.saturating_add(START_TIMEOUT));
    let waiting = Instant::now();
    loop {
        let Err(error) = answers(address) else {
            if let Some(started) = started {
                say(format_args!(
                    "controller `{name}` answers, after {} s",
                    started.elapsed().as_secs()
                ));
            }
            return Ok(Located::Ec2 {
                instance: id,
                address,
                interface,
            });
        };
        if by.remaining().is_none() {
            return Err(format!(
                "controller `{name}`, {id}, didn't answer SSH at its {}, {address}, in the {} s \
                 it was waited for: {error}: {}",
                interface.address(),
                waiting.elapsed().as_secs(),
                unanswered(&error, interface)
            ));
        }
        thread::sleep(START_POLL);
    }
}

/// Why `sshd` of a controller that runs didn't answer a probe ([`answers`]) at its address that
/// `interface` names, which failed with `error`, and what to do.
pub(super) fn unanswered(error: &io::Error, interface: Interface) -> &'static str {
    match (error.kind(), interface) {
        // A security group drops what it doesn't admit, so a refusal is the instance's.
        (io::ErrorKind::ConnectionRefused, _) => {
            "nothing listens there, as when the controller is stopping though EC2 says it runs, \
             or its `sshd` is down: run the command again shortly"
        }
        (
            io::ErrorKind::TimedOut
            | io::ErrorKind::HostUnreachable
            | io::ErrorKind::NetworkUnreachable,
            Interface::PrivateIp,
        ) => {
            "this machine doesn't reach the controller from an address of the controller's VPC: \
             run the command from a machine in that VPC, or through a VPN that translates its \
             addresses into the VPC's"
        }
        (
            io::ErrorKind::TimedOut
            | io::ErrorKind::HostUnreachable
            | io::ErrorKind::NetworkUnreachable,
            Interface::PublicIp,
        ) => {
            "this machine doesn't reach the controller from an address that deploy/aws's \
             `ssh_from` lists: ask its admin to list this machine's public IPv4 address there, \
             and to apply"
        }
        _ => "run the command again",
    }
}

/// Whether a connection to `sshd`'s port at `address` opens within `PROBE_TIMEOUT`, or why not.
// A connection alone: a booting controller's `sshd` takes one before it admits logins, and one
// past `MaxStartups` sends no banner, "Not allowed at this time" in its place, so a login still has
// to be tried, as SkyPilot tries one after its probe:
// https://github.com/skypilot-org/skypilot/blob/052914aaccb29a9a2364dd73654f2e61f0af42a0/sky/provision/provisioner.py#L478-L515
// https://github.com/openssh/openssh-portable/blob/5dba72adacbd39736960543373737bc01e35f778/sshd.c#L589-L639
pub(super) fn answers(address: IpAddr) -> io::Result<()> {
    // The port of `sshd`, which `deploy/aws` admits.
    TcpStream::connect_timeout(&SocketAddr::new(address, 22), PROBE_TIMEOUT).map(drop)
}
