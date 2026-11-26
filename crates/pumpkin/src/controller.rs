//! Direct SSH connections to a controller. A workspace keeps its last known address. A command
//! tries the actual request there first and repairs a failed opening under the workspace's lock.
//! Each link owns its SSH process. Dropping it frees the connection.

mod aws;
mod commands;
mod copy;
mod follow;
mod linking;
mod listing;
mod location;
mod send;
mod viewing;

use std::fmt;
use std::fs::{self, File, TryLockError};
use std::net::SocketAddr;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub(crate) use commands::{cancel, here, reap, run, wait};
pub(crate) use listing::{ls, scratch, short_of};
use location::{Found, Located, Location};
use serde::{Deserialize, Serialize};
use stdx::Context as _;
use stdx::process::Environment;
pub(crate) use viewing::{Unviewed, Viewing};

use self::aws::{find, kept};
pub(crate) use self::aws::{instances, live, tag, tagged};
use crate::config::{self, Find, Interface};
use crate::workspace::Workspace;
use crate::{next, say};

/// The controller's user, which runs every Pumpkin process there.
pub(crate) const USER: &str = "pumpkin";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const SHORT_TIMEOUT: Duration = Duration::from_mins(10);

// The pauses between attempts after a link ends before its answer is whole.
const AGAIN: [Duration; 6] = [
    Duration::ZERO,
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(30),
    Duration::from_mins(1),
    Duration::from_mins(2),
];

// Eight openings, after Ansible's backoff for a connection that did not get through.
// https://github.com/ansible/ansible/blob/2a4acd774aec2625b09cca2284cb2a08654e4cdd/lib/ansible/plugins/connection/ssh.py#L550-L610
const OPENINGS: u32 = 8;

/// A controller's name, as a client's `[controllers.<name>]` names it: the `name` of its settings'
/// `[deployment]`, as `deploy/aws` was applied with it. It starts with a letter, so it is never a
/// run's id, which starts with a digit.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct Name(String);

impl TryFrom<String> for Name {
    type Error = String;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        let bytes = name.as_bytes();
        let allowed =
            |byte: &u8| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-';
        let fits = (1..=16).contains(&bytes.len())
            && bytes.first().is_some_and(u8::is_ascii_lowercase)
            && bytes.last() != Some(&b'-')
            && bytes.iter().all(allowed);
        if !fits {
            return Err(format!(
                "`{}` is not a controller's name: give the `name` of its settings' `[deployment]`, \
                 1 to 16 of a-z, 0-9 and `-`, starting with a letter and ending with no `-`",
                name.escape_debug()
            ));
        }
        Ok(Self(name))
    }
}

impl From<Name> for String {
    fn from(name: Name) -> Self {
        name.0
    }
}

impl AsRef<Path> for Name {
    fn as_ref(&self) -> &Path {
        Path::new(&self.0)
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Where a controller's `sshd` listens, as `[controllers.<name>] address` names it: a host, by its
/// name or its IP address, and a port.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct Address {
    /// A name of letters, digits, `.` and `-` that starts with a letter or a digit, or an IP
    /// address, without brackets, so never an option of `ssh`.
    host: String,
    port: u16,
}

impl TryFrom<String> for Address {
    type Error = String;

    fn try_from(address: String) -> Result<Self, Self::Error> {
        let parsed = match address.parse::<SocketAddr>() {
            // An address's scope, as of a link-local IPv6 address, is no part of a host.
            Ok(SocketAddr::V6(socket)) if socket.scope_id() != 0 => None,
            Ok(socket) => Some((socket.ip().to_string(), socket.port())),
            Err(_) => address.rsplit_once(':').and_then(|(host, port)| {
                let bytes = host.as_bytes();
                let allowed =
                    |byte: &u8| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-');
                let fits = bytes.first().is_some_and(u8::is_ascii_alphanumeric)
                    && bytes.iter().all(allowed);
                fits.then_some((host.to_owned(), port.parse().ok()?))
            }),
        };
        match parsed {
            Some((host, port)) if port != 0 => Ok(Self { host, port }),
            _ => Err(format!(
                "`{}` is not a controller's address: give its host and the port of its `sshd`, \
                 such as `lab.example.org:2222` or `192.0.2.1:2222`",
                address.escape_debug()
            )),
        }
    }
}

impl From<Address> for String {
    fn from(address: Address) -> Self {
        address.to_string()
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { host, port } = self;
        if host.contains(':') {
            write!(f, "[{host}]:{port}")
        } else {
            write!(f, "{host}:{port}")
        }
    }
}

/// The directory of this machine's copies of the records of controller `name`'s runs, `<name>` in
/// the default runs directory, so that no two controllers' copies, nor this machine's own runs,
/// share one.
// As git keeps the branches of each remote apart, in `refs/remotes/<name>/`:
// https://github.com/git/git/blob/f7de81456940dd6f3274e461149bba12965de609/Documentation/glossary-content.adoc#L615-L622
pub(crate) fn copies(name: &Name) -> Result<PathBuf, String> {
    Ok(PathBuf::from(crate::default_runs()?).join(name))
}

/// When a pause of `pause` ends, as a message says it: `now`, or `in <seconds> s`.
fn when(pause: Duration) -> String {
    match pause.as_secs() {
        0 => "now".to_owned(),
        seconds => format!("in {seconds} s"),
    }
}

/// The file at `path`, made 0600 if it isn't there, once this process holds a lock of it; while
/// another holds it, says `waiting` first.
fn locked(path: &Path, waiting: fmt::Arguments<'_>) -> Result<File, String> {
    let file = private(path)?;
    match file.try_lock() {
        Ok(()) => return Ok(file),
        Err(TryLockError::WouldBlock) => {}
        Err(TryLockError::Error(error)) => return Err(error).context(path.display()),
    }
    say(waiting);
    file.lock().context(path.display())?;
    Ok(file)
}

fn private(path: &Path) -> Result<File, String> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path);
    file.context(path.display())
}

pub(crate) struct Reached {
    find: Find,
    name: Name,
    alias: String,
    kept: PathBuf,
    location: Location,
    linked: Option<(linking::Linked, Found)>,
}

impl Reached {
    pub(crate) fn reach(settings: &config::Controller) -> Result<Self, String> {
        let config::Controller {
            name,
            find,
            modal: _,
        } = settings;
        let alias = match find {
            // By the controller's name, rather than the instance that the tag finds, so that the
            // tag that names the controller can't lead this machine's key to another instance.
            // One for each of its addresses, so that a host that answered at the other, as one in
            // this machine's VPC may at a private address, pins no key and keeps no `found` here.
            Find::Ec2 {
                region,
                interface: Interface::PrivateIp,
            } => format!("{name}.{}", region.as_str()),
            Find::Ec2 {
                region,
                interface: Interface::PublicIp,
            } => format!("{name}.{}.public_ip", region.as_str()),
            // As `ssh` names a host at a port other than 22 in `known_hosts`.
            Find::Address(address) => format!("[{}]:{}", address.host, address.port),
        };
        let location = Location::new(Workspace::find()?.controller().join(&alias))?;
        let kept = kept()?;
        Ok(Self {
            find: find.clone(),
            name: name.clone(),
            alias,
            kept,
            location,
            linked: None,
        })
    }

    fn found(&self) -> Result<Found, String> {
        self.location.recent()?;
        match self.location.found()? {
            Some(found) => Ok(found),
            None => self.location.repair(&self.name, None, || self.locate()),
        }
    }

    /// Where the controller is: the instance that EC2 finds by its tag, which this starts if it is
    /// stopped, or the address that the configuration names.
    fn locate(&self) -> Result<Located, String> {
        match &self.find {
            Find::Ec2 { region, interface } => find(region, &self.name, *interface),
            Find::Address(address) => Ok(Located::Address(address.clone())),
        }
    }

    fn host_refused(&self, said: &str) -> String {
        let hosts = self.kept.join("known_hosts");
        format!(
            "{said}: verify the host key for controller `{}` with its administrator. If its key changed as expected, unpin the old key: ssh-keygen -R {} -f {}",
            self.name,
            next::quoted(&self.alias),
            next::quoted(&hosts.to_string_lossy())
        )
    }

    /// Why the controller refused this machine's key, and the line its admin adds to admit it.
    fn key_refused(&self) -> Result<String, String> {
        let public = self.kept.join("key.pub");
        let line = fs::read_to_string(&public).context(public.display())?;
        let list = match self.find {
            Find::Ec2 { .. } => "in deploy/aws's `members`, and to apply",
            Find::Address(_) => "to the controller's members' list",
        };
        Ok(format!(
            "controller `{}` refused this machine's key: ask its admin to add this line under \
             your name {list}: \"{}\". If the key is listed already, run the command again in a \
             minute, once the controller has copied the list",
            self.name,
            line.trim_end()
        ))
    }

    fn repair(&self, previous: &Found, said: &str) -> Result<Found, String> {
        self.location.repair(&self.name, Some(previous), || match (&self.find, &previous.at) {
            (Find::Ec2 { interface, .. }, Located::Ec2 { address, .. }) => {
                let probe = aws::answers(*address);
                self.locate().map_err(|error| match probe {
                    Ok(()) => format!("{said}; finding the controller again: {error}"),
                    Err(probe) => format!("{said}; SSH at its last {}: {probe}: {}; finding the controller again: {error}", interface.address(), aws::unanswered(&probe, *interface)),
                })
            }
            (Find::Address(_), _) | (Find::Ec2 { .. }, Located::Address(_)) => self.locate(),
        })
    }

    fn ssh_command() -> Command {
        stdx::process::command("ssh", Environment::Only(&["HOME", "PATH"]))
    }

    fn remote(&self, ssh: &mut Command, at: &Located, remote: &str) {
        ssh.args(self.ssh_options());
        let host = match at {
            Located::Ec2 { address, .. } => address.to_string(),
            Located::Address(Address { host, port }) => {
                ssh.args(["-o".to_owned(), format!("Port={port}")]);
                host.clone()
            }
        };
        ssh.arg(format!("{USER}@{host}")).arg(remote);
    }

    fn ssh_options(&self) -> Vec<String> {
        let mut words = vec!["-F".to_owned(), "/dev/null".to_owned(), "-i".to_owned()];
        words.push(self.kept.join("key").display().to_string());
        let known_hosts = self.kept.join("known_hosts").display().to_string();
        words.extend([
            "-o".to_owned(),
            format!("UserKnownHostsFile={known_hosts}"),
            "-o".to_owned(),
            format!("HostKeyAlias={}", self.alias),
        ]);
        let options = [
            "IdentitiesOnly=yes",
            "BatchMode=yes",
            "StrictHostKeyChecking=accept-new",
            "LogLevel=ERROR",
            "Compression=no",
            // So that a connection whose other end is gone, as when this machine slept, ends within
            // minutes, rather than when TCP gives up.
            "ServerAliveInterval=30",
        ];
        for option in options {
            words.extend(["-o".to_owned(), option.to_owned()]);
        }
        words.extend([
            "-o".to_owned(),
            format!("ConnectTimeout={}", CONNECT_TIMEOUT.as_secs()),
        ]);
        words
    }
}
