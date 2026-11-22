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
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use ::aws::Region;
pub(crate) use commands::{cancel, here, reap, run, wait};
pub(crate) use listing::{ls, scratch};
use location::{Found, Location};
use serde::Deserialize;
use stdx::Context as _;
use stdx::process::Environment;
pub(crate) use viewing::{Unviewed, Viewing};

use self::aws::{Located, find, kept};
pub(crate) use self::aws::{instances, live, tag, tagged};
use crate::workspace::Workspace;
use crate::{config, next, say};

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

/// A controller's name: the `name` that `deploy/aws` was applied with.
#[derive(Clone, Debug, Deserialize)]
#[serde(try_from = "String")]
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
                "`{}` is not a controller's name: give the `name` that `deploy/aws` was applied \
                 with, 1 to 16 of a-z, 0-9 and `-`, starting with a letter and ending with no `-`",
                name.escape_debug()
            ));
        }
        Ok(Self(name))
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
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
    region: Region,
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
            region,
            modal: _,
        } = settings;
        let alias = format!("{name}.{}", region.as_str());
        let location = Location::new(Workspace::find()?.controller().join(&alias))?;
        let kept = kept()?;
        Ok(Self {
            region: region.clone(),
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
            None => self
                .location
                .repair(&self.name, None, || find(&self.region, &self.name)),
        }
    }

    fn host_refused(&self, said: &str) -> String {
        let hosts = self.kept.join("known_hosts");
        format!(
            "{said}: verify the host key for controller `{}` with its administrator. If its key changed as expected, unpin the old key: ssh-keygen -R {} -f {}",
            self.name,
            self.alias,
            next::quoted(&hosts.to_string_lossy())
        )
    }

    /// Why the controller refused this machine's key, and the line its admin adds to admit it.
    fn key_refused(&self) -> Result<String, String> {
        let public = self.kept.join("key.pub");
        let line = fs::read_to_string(&public).context(public.display())?;
        Ok(format!(
            "controller `{}` refused this machine's key: ask its admin to add this line under \
             your name in deploy/aws's `members`, and to apply: \"{}\". If the key is listed \
             already, run the command again in a minute, once the controller has copied the list",
            self.name,
            line.trim_end()
        ))
    }

    fn repair(&self, previous: &Found, said: &str) -> Result<Found, String> {
        self.location.repair(&self.name, Some(previous), || {
            let probe = aws::answers(previous.at.address);
            find(&self.region, &self.name).map_err(|error| match probe {
                Ok(()) => format!("{said}; finding the controller again: {error}"),
                Err(probe) => format!("{said}; SSH at its last private address: {probe}: {}; finding the controller again: {error}", aws::unanswered(&probe)),
            })
        })
    }

    fn ssh_command() -> Command {
        stdx::process::command("ssh", Environment::Only(&["HOME", "PATH"]))
    }

    fn remote(&self, ssh: &mut Command, at: &Located, remote: &str) {
        ssh.args(self.ssh_options())
            .arg(format!("{USER}@{}", at.address))
            .arg(remote);
    }

    fn ssh_options(&self) -> Vec<String> {
        let mut words = vec!["-F".to_owned(), "/dev/null".to_owned(), "-i".to_owned()];
        words.push(self.kept.join("key").display().to_string());
        let known_hosts = self.kept.join("known_hosts").display().to_string();
        words.extend([
            "-o".to_owned(),
            format!("UserKnownHostsFile={known_hosts}"),
            "-o".to_owned(),
            // By the controller's name, rather than the instance that the tag finds, so that the
            // tag that names the controller can't lead this machine's key to another instance.
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
