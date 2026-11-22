//! The calls that `pumpkin`'s own code makes on AWS, and each principal's policy ([`policy`]).
//!
//! A principal's policy is what IAM must allow of each call that it makes. The controller's role
//! makes its jobs' calls: they run the providers and read their settings. The providers'
//! other principals make their own calls. A client's calls reach its controller.

use aws::iam::{
    self, Allow, Call, Condition, Fill, Key, Part, Placeholder, Resource, Test, Values,
};
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
    /// What a machine whose configuration names `[controller]` needs.
    Client,
    /// What `pumpkin-admin check` needs to read the deployment.
    Check,
    /// What a contributor's `cargo xtask live` needs: the client's, the check's,
    /// `scheduler:ListSchedules`, and the launch and the end of a member's machine.
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
    /// Its read of its settings.
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
    // On any profile and any principal. The controller's instance names its profile, and so its
    // role, and no placeholder stands for either.
    profile: Call::new(
        ["iam", "get-instance-profile"],
        Allow::new(&["iam:GetInstanceProfile"], &[Resource::Any]),
    ),
    simulate: Call::new(
        ["iam", "simulate-principal-policy"],
        Allow::new(&["iam:SimulatePrincipalPolicy"], &[Resource::Any]),
    ),
};

/// What `cargo xtask live` allows itself beyond the client's calls and the check's: it lists the
/// schedules of the timer that a run left, as `aws scheduler list-schedules` does, which
/// `xtask` runs itself.
const LIST_SCHEDULES: Allow = Allow::new(&["scheduler:ListSchedules"], &[Resource::Any]);

const MEMBER_PROFILE: Placeholder = Placeholder::new(
    "member_profile_arn",
    "the ARN of the instance profile of a member's machine",
);

const MEMBER_ROLE: Placeholder = Placeholder::new(
    "member_role_arn",
    "the ARN of the role of a member's machine",
);

/// The tag of each machine that `cargo xtask live join` launches, and of its security group.
const JOIN_TAG: &str = "pumpkin:join";

/// The one instance type of the machine that `cargo xtask live join` launches, which bounds what
/// a launch costs.
const JOIN_INSTANCE_TYPE: &str = "t3.micro";

const JOIN_REQUESTED: Condition = Condition::new(
    Test::Null,
    Key::Named("aws:RequestTag/pumpkin:join"),
    Values::Are(&[Part::Text("false")]),
);

const JOIN_INSTANCES: &[Resource] = &[Resource::In("ec2", &[Part::Text("instance/*")])];

const JOIN_GROUPS: &[Resource] = &[Resource::In("ec2", &[Part::Text("security-group/*")])];

const MEMBER_INSTANCE: Condition = Condition::new(
    Test::ArnEquals,
    Key::Named("ec2:InstanceProfile"),
    Values::Are(&[Part::Fill(&MEMBER_PROFILE)]),
);

const TAGGED: Condition = Condition::new(
    Test::Null,
    Key::Tag("ec2", JOIN_TAG),
    Values::Are(&[Part::Text("false")]),
);

/// What `cargo xtask live join` allows itself beyond the client's calls, each by the `aws`
/// command that it runs. It launches a member's machine with the members' instance profile, in
/// a security group of its own that admits `ssh` from the VPC. It then ends both. It may end only
/// the machines and groups that carry its tag.
// As Packer's builder makes a temporary group for the instance it builds from, and deletes it once
// the instance is terminated:
// https://github.com/hashicorp/packer/blob/0ca418d656cd5c28798ed5accdd132680979a6de/builder/amazon/common/step_security_group.go#L80-L188
static JOIN: [Allow; 9] = [
    // `ec2 run-instances`: the instance, tagged, of its one type, with the members' profile alone.
    Allow::new(&["ec2:RunInstances"], JOIN_INSTANCES).when(&[
        MEMBER_INSTANCE,
        JOIN_REQUESTED,
        Condition::new(
            Test::StringEquals,
            Key::Named("ec2:InstanceType"),
            Values::Are(&[Part::Text(JOIN_INSTANCE_TYPE)]),
        ),
    ]),
    // The launch's other resources: its image, which a public AMI's ARN names without an account,
    // its subnet, group, interface and volume. Of RunInstances' resource types, only the instance
    // has `ec2:InstanceType`, so this statement allows no instance:
    // https://github.com/salesforce/policy_sentry/blob/2575f2b873e1c1f0fb604a448dee392f39ad74ad/policy_sentry/shared/data/iam-definition.json#L112478-L112507
    Allow::new(&["ec2:RunInstances"], &[Resource::Any]).when(&[Condition::new(
        Test::Null,
        Key::Named("ec2:InstanceType"),
        Values::Are(&[Part::Text("true")]),
    )]),
    // The tags of the launch and of `ec2 create-security-group`.
    Allow::new(
        &["ec2:CreateTags"],
        &[
            Resource::In("ec2", &[Part::Text("instance/*")]),
            Resource::In("ec2", &[Part::Text("volume/*")]),
            Resource::In("ec2", &[Part::Text("security-group/*")]),
        ],
    )
    .when(&[Condition::new(
        Test::StringEquals,
        Key::Named("ec2:CreateAction"),
        Values::Are(&[
            Part::Text("RunInstances"),
            Part::Text("CreateSecurityGroup"),
        ]),
    )]),
    // The profile's role, which the launch names.
    Allow::new(&["iam:PassRole"], &[Resource::Arn(&MEMBER_ROLE)]).when(&[Condition::new(
        Test::StringEquals,
        Key::Named("iam:PassedToService"),
        Values::Are(&[Part::Text("ec2.amazonaws.com")]),
    )]),
    // `ec2 terminate-instances`.
    Allow::new(&["ec2:TerminateInstances"], JOIN_INSTANCES).when(&[TAGGED, MEMBER_INSTANCE]),
    // `ec2 create-security-group`, of a tagged group, in a VPC.
    Allow::new(
        &["ec2:CreateSecurityGroup"],
        &[Resource::In("ec2", &[Part::Text("vpc/*")])],
    ),
    Allow::new(&["ec2:CreateSecurityGroup"], JOIN_GROUPS).when(&[JOIN_REQUESTED]),
    // `ec2 authorize-security-group-ingress` and `ec2 delete-security-group`.
    Allow::new(
        &[
            "ec2:AuthorizeSecurityGroupIngress",
            "ec2:DeleteSecurityGroup",
        ],
        JOIN_GROUPS,
    )
    .when(&[TAGGED]),
    // `ec2 describe-images`, which finds the AMI, `ec2 wait security-group-exists`, and `ec2
    // describe-vpcs`, which reads the VPC's blocks.
    Allow::new(
        &[
            "ec2:DescribeImages",
            "ec2:DescribeSecurityGroups",
            "ec2:DescribeVpcs",
        ],
        &[Resource::Any],
    ),
];

/// What IAM must allow `principal`.
// A call that its crate declares but lists in no principal's statements compiles all the same.
// IAM then refuses it. `cargo xtask live ec2` and `cargo xtask live modal` show that as AWS's
// `AccessDenied` once a deployment applies these policies.
pub(crate) fn allows(principal: Principal) -> Vec<&'static Allow> {
    match principal {
        Principal::Controller => {
            let Job { settings } = &JOB;
            let Host {
                settings: host_settings,
                members,
            } = &HOST;
            (ec2::iam::caller())
                .chain(modal::iam::caller())
                .chain(settings.allows())
                .chain(host_settings.allows())
                .chain(members.allows())
                .collect()
        }
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
            allows.extend(&JOIN);
            allows
        }
    }
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
