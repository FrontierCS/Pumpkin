//! The calls that `pumpkin`'s own code makes on AWS, and each principal's policy ([`policy`]).
//!
//! A principal's policy is what IAM must allow of each call that it makes. The controller's role
//! makes its jobs' calls: they run the providers and read their settings. The providers'
//! other principals make their own calls. A client's calls reach its controller.

use aws::iam::{self, Allow, Call, Fill, Placeholder, Resource};
use clap::ValueEnum;

/// A principal of a deployment of the controller, or a part of one, whose policy
/// `pumpkin-admin policy` prints.
#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum Principal {
    /// The controller's role: its jobs' providers and their settings.
    Controller,
    /// The role of the EC2 timer, which terminates instances.
    Timer,
    /// The EC2 provider's instances' role, which pulls their images.
    Sandbox,
    /// The role that Modal assumes to import the roles' images.
    ModalPull,
    /// What a machine whose configuration names `[controllers.<name>]` needs.
    Client,
    /// What `pumpkin-admin check` needs to read the deployment.
    Check,
    /// What a contributor's `cargo xtask live` needs: the client's, the check's,
    /// `scheduler:ListSchedules`, and a read of the timer's schedules.
    Contributor,
}

pub(crate) const SETTINGS: Placeholder = Placeholder::new(
    "settings_parameter_arn",
    "the ARN of the SSM parameter of the controller's settings",
);

pub(crate) const CONTROLLER: Placeholder = Placeholder::new(
    "controller_instance_arn",
    "the ARN of the controller's instance",
);

pub(crate) const MEMBERS: Placeholder = Placeholder::new(
    "members_parameter_arn",
    "the ARN of the SSM parameter of the controller's members",
);

impl Principal {
    /// Its name, as `pumpkin-admin policy` takes it.
    pub(crate) fn name(self) -> String {
        let Some(name) = self.to_possible_value() else {
            unreachable!("every principal is a value of the command line's");
        };
        name.get_name().to_owned()
    }
}

/// The calls of a job of its own.
pub(crate) struct Job {
    pub(crate) settings: Call,
}

const READ_SETTINGS: Allow = Allow::new(&["ssm:GetParameter"], &[Resource::Arn(&SETTINGS)]);

pub(crate) const JOB: Job = Job {
    settings: Call::new(["ssm", "get-parameter"], READ_SETTINGS),
};

/// The calls of the controller's host.
pub(crate) struct Host {
    /// Reads the settings, whose `[deployment]` names the members' keys.
    pub(crate) settings: Call,
    /// Reads the members' keys.
    pub(crate) members: Call,
}

pub(crate) const HOST: Host = Host {
    settings: Call::new(["ssm", "get-parameter"], READ_SETTINGS),
    members: Call::new(["ssm", "get-parameter"], READ_MEMBERS),
};

const READ_MEMBERS: Allow = Allow::new(&["ssm:GetParameter"], &[Resource::Arn(&MEMBERS)]);

/// The calls of a client on its controller's instance.
pub(crate) struct Client {
    pub(crate) describe: Call,
    pub(crate) start: Call,
}

pub(crate) const CLIENT: Client = Client {
    describe: Call::new(
        ["ec2", "describe-instances"],
        Allow::new(&["ec2:DescribeInstances"], &[Resource::Any]),
    ),
    start: Call::new(
        ["ec2", "start-instances"],
        Allow::new(&["ec2:StartInstances"], &[Resource::Arn(&CONTROLLER)]),
    ),
};

/// The calls that `pumpkin-admin check` makes of its own. Each is a read.
pub(crate) struct Check {
    pub(crate) settings: Call,
    pub(crate) members: Call,
    /// Finds the controller's instance by its tag.
    pub(crate) instances: Call,
    /// Reads the controller's instance profile, for its role.
    pub(crate) profile: Call,
    /// Simulates a principal's policy.
    pub(crate) simulate: Call,
}

pub(crate) const CHECK: Check = Check {
    settings: Call::new(["ssm", "get-parameter"], READ_SETTINGS),
    members: Call::new(["ssm", "get-parameter"], READ_MEMBERS),
    instances: Call::new(
        ["ec2", "describe-instances"],
        Allow::new(&["ec2:DescribeInstances"], &[Resource::Any]),
    ),
    profile: Call::new(
        ["iam", "get-instance-profile"],
        Allow::new(
            &["iam:GetInstanceProfile"],
            &[Resource::Arn(&CONTROLLER_PROFILE)],
        ),
    ),
    simulate: Call::new(
        ["iam", "simulate-principal-policy"],
        Allow::new(
            &["iam:SimulatePrincipalPolicy"],
            &[
                Resource::Arn(&CONTROLLER_ROLE),
                Resource::Arn(&ec2::iam::TIMER_ROLE),
                Resource::Arn(&ec2::iam::SANDBOX_ROLE),
                Resource::Arn(&modal::iam::PULL_ROLE),
            ],
        ),
    ),
};

const CONTROLLER_PROFILE: Placeholder = Placeholder::new(
    "controller_profile_arn",
    "the ARN of the controller's instance profile",
);

const CONTROLLER_ROLE: Placeholder =
    Placeholder::new("controller_role_arn", "the ARN of the controller's role");

/// What `cargo xtask live` allows itself beyond the client's calls and the check's: it lists the
/// schedules of the timer that a run left, as `aws scheduler list-schedules` does, which
/// `xtask` runs itself.
const LIST_SCHEDULES: Allow = Allow::new(&["scheduler:ListSchedules"], &[Resource::Any]);

/// What IAM must allow `principal`.
// A call that its crate declares but lists in no principal's statements compiles all the same.
// IAM then refuses it. `cargo xtask live ec2` and `cargo xtask live modal` show that as AWS's
// `AccessDenied` once a deployment applies these policies.
pub(crate) fn allows(principal: Principal) -> Vec<&'static Allow> {
    match principal {
        Principal::Controller => controller(true),
        Principal::Timer => ec2::iam::timer().collect(),
        Principal::Sandbox => ec2::iam::sandbox().collect(),
        Principal::ModalPull => modal::iam::puller().collect(),
        Principal::Client => {
            let Client { describe, start } = &CLIENT;
            [describe, start]
                .into_iter()
                .flat_map(Call::allows)
                .collect()
        }
        Principal::Check => {
            let Check {
                settings,
                members,
                instances,
                profile,
                simulate,
            } = &CHECK;
            [settings, members, instances, profile, simulate]
                .into_iter()
                .flat_map(Call::allows)
                .chain(ec2::iam::checker())
                .collect()
        }
        Principal::Contributor => {
            let mut allows = self::allows(Principal::Client);
            allows.extend(self::allows(Principal::Check));
            allows.push(&LIST_SCHEDULES);
            allows.extend(ec2::iam::timers_reader());
            allows
        }
    }
}

/// What IAM must allow the controller's role, whose jobs run the Modal provider if `runs_modal`.
pub(crate) fn controller(runs_modal: bool) -> Vec<&'static Allow> {
    let Job { settings } = &JOB;
    let Host {
        settings: host_settings,
        members,
    } = &HOST;
    let modal_calls = runs_modal.then(modal::iam::caller).into_iter().flatten();
    (ec2::iam::caller())
        .chain(modal_calls)
        .chain(settings.allows())
        .chain(host_settings.allows())
        .chain(members.allows())
        .collect()
}

/// `principal`'s policy, as IAM's JSON, with each value that only a deployment knows as
/// Terraform's `templatefile` takes it.
pub(crate) fn policy(principal: Principal) -> String {
    iam::policy(&allows(principal))
}

/// The help of `pumpkin-admin policy`'s placeholders. It says what each stands for, and which
/// each principal's policy holds.
pub(crate) fn placeholders() -> String {
    let principals = Principal::value_variants().iter();
    let all: Vec<&Allow> = principals
        .clone()
        .flat_map(|principal| allows(*principal))
        .collect();
    let mut lines = vec![
        "Placeholders, which `templatefile` fills with a string where a policy holds".to_owned(),
        "${name}, and with a list where it holds ${jsonencode(name)}:".to_owned(),
    ];
    for (name, Fill { about, list }) in iam::placeholders(&all) {
        let about = if list {
            format!("a list of {about}")
        } else {
            about.to_owned()
        };
        lines.extend(wrapped(&format!("  {name}:"), about.split(' ')));
    }
    lines.push(String::new());
    lines.push("Each principal's placeholders:".to_owned());
    for principal in principals {
        let names: Vec<&str> = iam::placeholders(&allows(*principal)).into_keys().collect();
        let joined = names.join(", ");
        lines.extend(wrapped(
            &format!("  {}:", principal.name()),
            joined.split(' '),
        ));
    }
    lines.join("\n")
}

/// `words` after `head`, a space between each two, in lines of 80 columns at most. A longer word
/// takes a line of its own. Each line after the first is indented by 4.
fn wrapped<'a>(head: &str, words: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut lines = vec![head.to_owned()];
    for word in words {
        let Some(line) = lines.last_mut() else {
            unreachable!("the lines start with the head's");
        };
        if line.len() + 1 + word.len() > 80 {
            lines.push(format!("    {word}"));
        } else {
            line.push(' ');
            line.push_str(word);
        }
    }
    lines
}
