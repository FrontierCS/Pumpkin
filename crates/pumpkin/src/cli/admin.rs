//! `pumpkin-admin`'s command line. Its commands are the controller admin's, and those that the
//! units written by the admin's install run. `pumpkin` parses this command line when it runs by
//! the name [`NAME`].
// Each audience has a command of its own, so a member's `pumpkin --help` lists none of the admin's
// commands. Teleport splits its users' `tsh` from its admins' `tctl`, and Kubernetes its `kubectl`
// from the `kubeadm` that sets up a cluster's machines:
// https://github.com/gravitational/teleport/blob/0c2973dbae77a1a19ebb8bb1d7f059659a674038/tool/tctl/common/usage.go#L20
// https://github.com/gravitational/teleport/blob/0c2973dbae77a1a19ebb8bb1d7f059659a674038/tool/tsh/tsh.go#L516
// https://github.com/kubernetes/kubernetes/blob/a866cbe2e5bbaa01cfd5e969aa3e033f3282a8a2/cmd/kubeadm/app/cmd/cmd.go#L37
// https://github.com/kubernetes/kubernetes/blob/a866cbe2e5bbaa01cfd5e969aa3e033f3282a8a2/staging/src/k8s.io/kubectl/pkg/cmd/cmd.go#L273

use std::ffi::OsString;
use std::iter;
use std::path::PathBuf;

use aws::ParameterArn;
use clap::{CommandFactory as _, FromArgMatches as _, Parser};

use super::notes::ADMIN;
use super::{Command, Program, Refusal, first_word, flagged, noted, printed};
use crate::iam::{self, Principal};

/// The name that makes `pumpkin` run as `pumpkin-admin`. An install holds a link to `pumpkin` by
/// that name.
pub(crate) const NAME: &str = install::File::PumpkinAdmin.name();

/// Administer a Pumpkin controller.
///
/// The controller's admin runs `install` as root, from the release they pin. The admin runs
/// `check`, `check-members` and `policy` anywhere. The units that `install` writes run
/// `boot-all`, `idle` and `members`.
#[derive(Parser)]
#[command(
    name = NAME,
    disable_help_flag = true,
    disable_help_subcommand = true,
    disable_version_flag = true,
    subcommand_value_name = "command",
    subcommand_help_heading = "Commands"
)]
enum Parsed {
    /// Install the controller host's software, as root.
    ///
    /// `install` makes the user `pumpkin`, lingering, in Docker's group. It installs this
    /// `pumpkin-admin`, the host file /etc/pumpkin/controller.toml, the units that run `boot-all`,
    /// `idle` and `members`, and sshd's settings. Those admit only the members' keys for `pumpkin`.
    ///
    /// It copies the members' keys first, and changes nothing else if the copy fails. It needs
    /// Docker and AWS CLI v2 installed first. It writes a file only where it differs, reloads only
    /// what changed, and says each change, so a second run says nothing.
    Install {
        /// The ARN of the SSM parameter that holds the controller's settings. The host file names
        /// it.
        #[arg(long, value_name = "arn")]
        #[arg(value_parser = |arn: &str| ParameterArn::try_from(arn.to_owned()))]
        settings: ParameterArn,
    },
    /// Check the controller's deployment, with read-only calls on AWS.
    ///
    /// `check` reads the settings that the ARN names, what they name, and the controller's
    /// instance. It asks IAM's policy simulator about the policy of each role that `pumpkin-admin
    /// policy` prints. It runs every check, and says each problem on a line of its own: the
    /// resource, what is wrong, and what to fix. It says nothing if there is none. It starts
    /// nothing, so it leaves unchecked what only a running controller shows, and says so.
    Check {
        /// The ARN of the SSM parameter of the controller's settings.
        #[arg(long, value_name = "arn")]
        #[arg(value_parser = |arn: &str| ParameterArn::try_from(arn.to_owned()))]
        settings: ParameterArn,
    },
    /// Print the IAM policy of a principal of the controller's deployment.
    ///
    /// `policy` prints the policy as IAM's JSON. It holds each statement that this build's calls
    /// on AWS need as that principal, and nothing else. A value that only the deployment knows,
    /// such as its account, is a placeholder that Terraform's `templatefile` fills.
    #[command(after_help = iam::placeholders())]
    Policy {
        /// The principal whose policy to print.
        #[arg(value_name = "principal")]
        principal: Principal,
    },
    /// Run each installed build's boot.
    #[command(name = "boot-all")]
    BootAll,
    /// Stop the controller once it has been idle for 30 minutes.
    Idle,
    /// Copy the members' keys, which sshd admits, from SSM.
    Members,
    /// Check a directory of the members' keys, as a team's repository keeps them.
    ///
    /// `check-members` reads each `<member>.pub` file in the directory. Each of its lines is one
    /// of the member's keys, as the member's ~/.config/pumpkin/ssh/key.pub holds it. Blank lines
    /// are skipped. It refuses a name or a key that the controller refuses, and says where it is.
    #[command(name = "check-members")]
    CheckMembers {
        /// The directory of the members' files.
        #[arg(value_name = "dir")]
        dir: PathBuf,
    },
}

/// The admin's command line, with the examples and statuses that end each help.
pub(super) fn command() -> clap::Command {
    noted(flagged(Parsed::command()), ADMIN)
}

/// Reads `args` into the admin's command that they ask for, as [`super::parse`] does for
/// `pumpkin`. `args` leave out the program's name.
pub(crate) fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Command, Refusal> {
    let args: Vec<OsString> = args.into_iter().collect();
    let mut cli = command();
    let (first, named) = first_word(&cli, &args);
    if let Some(print) = printed(Program::Admin, &args, first, named.as_deref(), None) {
        return Ok(print);
    }
    let refusal = |error| Refusal {
        error,
        help: match &named {
            Some(name) => format!("{NAME} {name} --help"),
            None => format!("{NAME} --help"),
        },
    };
    let line = iter::once(OsString::from(NAME)).chain(args);
    let matches = cli.try_get_matches_from_mut(line).map_err(refusal)?;
    let parsed = Parsed::from_arg_matches(&matches);
    let parsed = parsed.map_err(|error| refusal(error.format(&mut cli)))?;
    Ok(match parsed {
        Parsed::Install { settings } => Command::Install(settings),
        Parsed::Check { settings } => Command::CheckDeployment(settings),
        Parsed::Policy { principal } => Command::Print(iam::policy(principal).into()),
        Parsed::BootAll => Command::BootAll,
        Parsed::Idle => Command::Idle,
        Parsed::Members => Command::Members,
        Parsed::CheckMembers { dir } => Command::CheckMembers(dir),
    })
}
