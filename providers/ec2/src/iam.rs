//! What IAM must allow of the provider's calls, and of the envelope's other principals.
//!
//! The provider's calls are made with the credentials of the process that runs it ([`caller`]).
//! Its sandboxes rely on the timer's role ([`timer`]) and the sandbox instances' ([`sandbox`]).
//! A check of the envelope reads it with calls of its own ([`checker`]). A reader of the timers'
//! schedules reads them ([`timers_reader`]).

use aws::iam::{Allow, Call, Condition, Ecr, Key, List, Part, Placeholder, Resource, Test, Values};

use crate::launch::RUN_TAG;

/// The envelope's launch templates, the only ones a create launches from.
pub(crate) const LAUNCH_TEMPLATES: List = List::new(
    "launch_template_arns",
    "the ARNs of the envelope's launch templates",
);

/// The instance types of the envelope's launch templates, the only ones a create launches.
pub(crate) const INSTANCE_TYPES: List = List::new(
    "instance_types",
    "the instance types of the envelope's launch templates",
);

/// The role of the instance profile of the envelope's launch templates.
pub const SANDBOX_ROLE: Placeholder =
    Placeholder::new("sandbox_role_arn", "the ARN of the sandbox instances' role");

/// The instance profile of the envelope's launch templates.
pub(crate) const SANDBOX_PROFILE: Placeholder = Placeholder::new(
    "sandbox_profile_arn",
    "the ARN of the sandbox instances' instance profile",
);

pub const TIMER_ROLE: Placeholder =
    Placeholder::new("timer_role_arn", "the ARN of the timer's role");

pub(crate) const TIMER_GROUP: Placeholder =
    Placeholder::new("timer_group", "the name of the timer's schedule group");

pub(crate) const ROLES_REPOSITORY: Placeholder = Placeholder::new(
    "roles_repository_arn",
    "the ARN of the roles' images' repository",
);

pub(crate) const SANDBOXD_REPOSITORY: Placeholder = Placeholder::new(
    "sandboxd_repository_arn",
    "the ARN of the repository of the images of `sandboxd`",
);

const REPOSITORIES: &[Resource] = &[
    Resource::Arn(&ROLES_REPOSITORY),
    Resource::Arn(&SANDBOXD_REPOSITORY),
];

const INSTANCES: &[Resource] = &[Resource::In("ec2", &[Part::Text("instance/*")])];

/// The provider's own instances. A run launched each, with its run's tag, from the envelope's
/// templates. The templates' instance profile is the sandboxes'.
const SANDBOXES: &[Condition] = &[
    Condition::new(
        Test::Null,
        Key::Tag("ec2", RUN_TAG),
        Values::Are(&[Part::Text("false")]),
    ),
    Condition::new(
        Test::ArnEquals,
        Key::Named("ec2:InstanceProfile"),
        Values::Are(&[Part::Fill(&SANDBOX_PROFILE)]),
    ),
];

const TERMINATE: Allow = Allow::new(&["ec2:TerminateInstances"], INSTANCES).when(SANDBOXES);

/// What a launch tags (`launch::tag_specifications`): its instance and its volumes.
const TAGGED: &[Resource] = &[
    Resource::In("ec2", &[Part::Text("instance/*")]),
    Resource::In("ec2", &[Part::Text("volume/*")]),
];

const FROM_TEMPLATE: Condition = Condition::new(
    Test::ArnEquals,
    Key::Named("ec2:LaunchTemplate"),
    Values::Listed(&LAUNCH_TEMPLATES),
);

const TEMPLATE_RESOURCE: Condition = Condition::new(
    Test::Bool,
    Key::Named("ec2:IsLaunchTemplateResource"),
    Values::Are(&[Part::Text("true")]),
);

// The timer and a delete end only an instance that carries the run's tag.
const RUN_TAGGED: Condition = Condition::new(
    Test::Null,
    Key::RequestTag(RUN_TAG),
    Values::Are(&[Part::Text("false")]),
);

/// The calls on the envelope's repositories.
pub(crate) const ECR: Ecr = Ecr::on(REPOSITORIES);

/// The provider's calls on AWS but those on ECR ([`ECR`]).
pub(crate) struct Calls {
    pub(crate) identity: Call,
    pub(crate) launch: Call,
    pub(crate) set_timer: Call,
    pub(crate) unset_timer: Call,
    pub(crate) describe: Call,
    pub(crate) terminate: Call,
    pub(crate) console: Call,
}

pub(crate) const CALLS: Calls = Calls {
    identity: Call::open(["sts", "get-caller-identity"]),
    // From the envelope's templates alone, with none of the resources they name overridden. Only
    // the resources that a request tags have `aws:RequestTag`, so the others, which the second
    // statement allows, don't require the run's tag, as EC2's example and Karpenter's split them:
    // https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/ExamplePolicies_EC2.html#iam-example-runinstances-launch-templates
    // https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/ExamplePolicies_EC2.html#iam-example-runinstances-tags
    // https://github.com/aws/karpenter-provider-aws/blob/475815f556945b1ce8cf4f38b478e39523bcb772/website/content/en/docs/reference/cloudformation.md#L164-L197
    // https://github.com/salesforce/policy_sentry/blob/2575f2b873e1c1f0fb604a448dee392f39ad74ad/policy_sentry/shared/data/iam-definition.json#L112453-L112698
    launch: Call::new(
        ["ec2", "run-instances"],
        Allow::new(&["ec2:RunInstances"], TAGGED).when(&[
            FROM_TEMPLATE,
            TEMPLATE_RESOURCE,
            // A launch may override a template's instance type, which is no resource. So the
            // statement holds the instance to the templates' types. Of RunInstances' resource
            // types, only the instance has this key. IAM's guide holds a launch's type in one
            // statement this way:
            // https://github.com/salesforce/policy_sentry/blob/2575f2b873e1c1f0fb604a448dee392f39ad74ad/policy_sentry/shared/data/iam-definition.json#L112478-L112507
            // https://github.com/awsdocs/iam-user-guide/blob/e2b8637a825b9f292bb04af61277d067dfcc5676/doc_source/reference_policies_elements_condition_operators.md?plain=1#L332-L374
            Condition::new(
                Test::StringEqualsIfExists,
                Key::Named("ec2:InstanceType"),
                Values::Listed(&INSTANCE_TYPES),
            ),
            RUN_TAGGED,
        ]),
    )
    .also(&[
        Allow::all_but(&["ec2:RunInstances"], TAGGED).when(&[FROM_TEMPLATE, TEMPLATE_RESOURCE]),
        // The launch's tags, on the instance and its volumes (`launch::tag_specifications`).
        Allow::new(&["ec2:CreateTags"], TAGGED).when(&[
            Condition::new(
                Test::StringEquals,
                Key::Named("ec2:CreateAction"),
                Values::Are(&[Part::Text("RunInstances")]),
            ),
            RUN_TAGGED,
        ]),
        // The templates' instance profile's role.
        Allow::new(&["iam:PassRole"], &[Resource::Arn(&SANDBOX_ROLE)]).when(&[Condition::new(
            Test::StringEquals,
            Key::Named("iam:PassedToService"),
            Values::Are(&[Part::Text("ec2.amazonaws.com")]),
        )]),
    ]),
    set_timer: Call::new(
        ["scheduler", "create-schedule"],
        Allow::new(&["scheduler:CreateSchedule"], TIMER_SCHEDULES),
    )
    // The target's role, which the schedule names.
    .also(&[
        Allow::new(&["iam:PassRole"], &[Resource::Arn(&TIMER_ROLE)]).when(&[Condition::new(
            Test::StringEquals,
            Key::Named("iam:PassedToService"),
            Values::Are(&[Part::Text("scheduler.amazonaws.com")]),
        )]),
    ]),
    unset_timer: Call::new(
        ["scheduler", "delete-schedule"],
        Allow::new(&["scheduler:DeleteSchedule"], TIMER_SCHEDULES),
    ),
    describe: Call::new(
        ["ec2", "describe-instances"],
        Allow::new(&["ec2:DescribeInstances"], &[Resource::Any]),
    ),
    terminate: Call::new(["ec2", "terminate-instances"], TERMINATE),
    console: Call::new(
        ["ec2", "get-console-output"],
        Allow::new(&["ec2:GetConsoleOutput"], INSTANCES).when(SANDBOXES),
    ),
};

const TIMER_SCHEDULES: &[Resource] = &[Resource::In(
    "scheduler",
    &[
        Part::Text("schedule/"),
        Part::Fill(&TIMER_GROUP),
        Part::Text("/*"),
    ],
)];

pub(crate) struct Checks {
    pub(crate) templates: Call,
    pub(crate) profile: Call,
}

pub(crate) const CHECKS: Checks = Checks {
    templates: Call::new(
        ["ec2", "describe-launch-template-versions"],
        Allow::new(&["ec2:DescribeLaunchTemplateVersions"], &[Resource::Any]),
    ),
    profile: Call::new(
        ["iam", "get-instance-profile"],
        Allow::new(
            &["iam:GetInstanceProfile"],
            &[Resource::Arn(&SANDBOX_PROFILE)],
        ),
    ),
};

/// What IAM must allow the credentials that read the envelope for a check of it.
pub fn checker() -> impl Iterator<Item = &'static Allow> {
    let Checks { templates, profile } = &CHECKS;
    [templates, profile].into_iter().flat_map(Call::allows)
}

/// What IAM must allow the credentials of a process that runs the provider.
pub fn caller() -> impl Iterator<Item = &'static Allow> {
    let Calls {
        identity,
        launch,
        set_timer,
        unset_timer,
        describe,
        terminate,
        console,
    } = &CALLS;
    let calls = [
        identity,
        launch,
        set_timer,
        unset_timer,
        describe,
        terminate,
        console,
    ];
    calls.into_iter().flat_map(Call::allows).chain(ECR.allows())
}

/// What IAM must allow the credentials that read the schedules of the timer, each instance's
/// (`launch::Ec2::set_timer`), as `aws scheduler get-schedule` does.
pub fn timers_reader() -> impl Iterator<Item = &'static Allow> {
    static READ: Allow = Allow::new(&["scheduler:GetSchedule"], TIMER_SCHEDULES);
    [&READ].into_iter()
}

/// What IAM must allow the timer's role. Each schedule's target terminates its instance with it
/// (`launch::Ec2::set_timer`).
pub fn timer() -> impl Iterator<Item = &'static Allow> {
    [&TERMINATE].into_iter()
}

/// What IAM must allow the sandbox instances' role. ECR's credential helper, as the instance's user
/// data configures it, gets a token with it for each pull. It pulls the role's image and
/// `sandboxd`'s.
// AWS's `AmazonEC2ContainerRegistryPullOnly` without `ecr:BatchImportUpstreamImage`, which only a
// pull-through cache needs:
// https://docs.aws.amazon.com/aws-managed-policy/latest/reference/AmazonEC2ContainerRegistryPullOnly.html
pub fn sandbox() -> impl Iterator<Item = &'static Allow> {
    static PULLS: [Allow; 2] = [
        Allow::new(&["ecr:GetAuthorizationToken"], &[Resource::Any]),
        Allow::new(
            &["ecr:BatchGetImage", "ecr:GetDownloadUrlForLayer"],
            REPOSITORIES,
        ),
    ];
    PULLS.iter()
}
