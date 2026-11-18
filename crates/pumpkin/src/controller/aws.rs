//! Finding a controller's EC2 instance by its tag, starting it, and enrolling this machine's
//! key with it through EC2 Instance Connect.

use std::fs::{self, DirBuilder};
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{env, thread};

use aws::{Bound, Cli};
use serde::Deserialize;
use stdx::process::Environment;
use stdx::{Context as _, Deadline};

use super::{Name, Reached, SHORT_TIMEOUT, USER, bounded, finished};
use crate::say;

// The tag that `deploy/aws` puts on the controller's instance, valued with the controller's name.
pub(super) const TAG: &str = "pumpkin:controller";

// How long a stopped controller has to run and be seen by SSM. An instance that EC2 starts runs
// and registers with SSM within a few minutes; past this, something else is wrong.
const START_TIMEOUT: Duration = Duration::from_mins(10);

const START_POLL: Duration = Duration::from_secs(5);

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Reservations {
    reservations: Vec<Reservation>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Reservation {
    instances: Vec<Instance>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct Instance {
    pub(super) instance_id: String,
    pub(super) state: State,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct State {
    pub(super) name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Information {
    instance_information_list: Vec<Seen>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Seen {
    ping_status: String,
}

/// The directory that holds this machine's `ssh` key to its controllers, `key` and `key.pub`,
/// which this makes if there is none, and `known_hosts`, which pins the host key that each
/// controller showed first, by its [`Reached::alias`]. Processes may call it at once.
// A credential, so beside the configuration, outside every workspace; one for the machine, so
// that each controller enrols it once, whatever workspace reaches it.
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
    let mut keygen = stdx::process::command("ssh-keygen", Environment::Only(&["PATH"]));
    keygen.args(["-q", "-t", "ed25519", "-N", "", "-C", "pumpkin", "-f"]);
    bounded(
        keygen.arg(made.path().join("key")),
        "ssh-keygen",
        SHORT_TIMEOUT,
    )?;
    if let Err(error) = fs::rename(made.path(), &kept)
        && !kept.join("key.pub").exists()
    {
        return Err(error).context(kept.display());
    }
    Ok(kept)
}

pub(super) fn instances(cli: &Cli, filter: &str) -> Result<Vec<Instance>, String> {
    let args = ["ec2", "describe-instances", "--filters", filter];
    let found: Reservations = cli.json(&args, Bound::Call)?;
    Ok(found
        .reservations
        .into_iter()
        .flat_map(|found| found.instances)
        .collect())
}

pub(super) fn ready(cli: &Cli, name: &Name, id: &str) -> Result<(), String> {
    let by = Deadline::after(START_TIMEOUT);
    let mut started = false;
    loop {
        let found = instances(cli, &format!("Name=instance-id,Values={id}"))?;
        let state = found.into_iter().next().map(|instance| instance.state.name);
        let state = state.unwrap_or_default();
        match state.as_str() {
            "running" if seen(cli, id)? => return Ok(()),
            "stopped" if !started => {
                say(format_args!(
                    "starting controller `{name}`, {id}, which is stopped"
                ));
                let start = ["ec2", "start-instances", "--instance-ids", id];
                cli.run(&start, Bound::Call)?;
                started = true;
                continue;
            }
            // On its way to running, or to being stopped, which a start then follows.
            "pending" | "running" | "stopping" | "stopped" => {}
            _ => {
                return Err(format!(
                    "controller `{name}`, {id}, is {state}: start it again once it can be"
                ));
            }
        }
        if by.remaining().is_none() {
            return Err(format!(
                "controller `{name}`, {id}, is {state} after {} s: expected it to run, and SSM to \
                 see it, by then",
                START_TIMEOUT.as_secs()
            ));
        }
        thread::sleep(START_POLL);
    }
}

fn seen(cli: &Cli, id: &str) -> Result<bool, String> {
    let filter = format!("Key=InstanceIds,Values={id}");
    let args = ["ssm", "describe-instance-information", "--filters", &filter];
    let information: Information = cli.json(&args, Bound::Call)?;
    Ok(information
        .instance_information_list
        .iter()
        .any(|seen| seen.ping_status == "Online"))
}

impl Reached {
    pub(super) fn enrol(&self, master: usize) -> Result<(), String> {
        let public = self.kept.join("key.pub");
        let public = fs::read(&public).context(public.display())?;
        let mut ssh = self.shared.command("ssh");
        ssh.args(self.ssh_options(master)).arg(self.host());
        ssh.arg("umask 077 && mkdir --parents .ssh && cat >> .ssh/authorized_keys");
        let fed = stdx::process::fed(&mut ssh, &public, Deadline::after(SHORT_TIMEOUT));
        finished(fed, "ssh", SHORT_TIMEOUT).map(|_| ())
    }

    /// Pushes this machine's public key to the instance with EC2 Instance Connect, which accepts it
    /// for a minute: the connection that uses it must begin within.
    pub(super) fn push(&self) -> Result<(), String> {
        let public = format!("file://{}", self.kept.join("key.pub").display());
        let args = [
            "ec2-instance-connect",
            "send-ssh-public-key",
            "--instance-id",
            &self.instance,
            "--instance-os-user",
            USER,
            "--ssh-public-key",
            &public,
        ];
        self.cli.run(&args, Bound::Call).map(|_| ())
    }
}
