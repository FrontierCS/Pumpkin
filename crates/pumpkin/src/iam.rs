//! The calls that `pumpkin`'s own code makes on AWS, and what IAM must allow of each: the
//! controller's, as its jobs read their settings and publish, and a client's, as it reaches its
//! controller.

use aws::iam::{Allow, Call, Condition, Key, Part, Placeholder, Resource, Test, Values};

use crate::controller::USER;

const SETTINGS: Placeholder = Placeholder::new(
    "settings_parameter_arn",
    "the ARN of the SSM parameter of the controller's settings",
);

const CONTROLLER: Placeholder = Placeholder::new(
    "controller_instance_arn",
    "the ARN of the controller's instance",
);

/// The calls of a job of its own.
pub(crate) struct Job {
    /// Its read of its settings.
    pub(crate) settings: Call,
}

pub(crate) const JOB: Job = Job {
    settings: Call::new(
        ["ssm", "get-parameter"],
        Allow::new(&["ssm:GetParameter"], &[Resource::Arn(&SETTINGS)]),
    ),
};

/// The calls of a client on its controller's instance.
pub(crate) struct Client {
    pub(crate) describe: Call,
    pub(crate) start: Call,
    pub(crate) push_key: Call,
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
    push_key: Call::new(
        ["ec2-instance-connect", "send-ssh-public-key"],
        Allow::new(
            &["ec2-instance-connect:SendSSHPublicKey"],
            &[Resource::Arn(&CONTROLLER)],
        )
        .when(&[Condition::new(
            Test::StringEquals,
            Key::Named("ec2:osuser"),
            Values::Are(&[Part::Text(USER)]),
        )]),
    ),
};
