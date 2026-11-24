//! The read of the envelope that the provider's [`Settings`] name, which fills the placeholders of
//! the provider's policies for their simulation ([`fill`]).
//!
//! The settings give the timer's role and group, the launch templates and the repositories. Each
//! class's launch template, at the version the settings name, gives the instance types. The one
//! instance profile that the templates name gives itself and its role. What else the envelope
//! holds, an apply of deploy/aws makes as the provider relies on, and the provider's own calls
//! fail on what a hand edit breaks.

use aws::iam::Filling;
use aws::{Bound, Cli, InstanceProfile, Problem};
use schema::ir::Name;
use serde::Deserialize;
use templates::{Data, Named};

use crate::iam::{
    CHECKS, INSTANCE_TYPES, LAUNCH_TEMPLATES, ROLES_REPOSITORY, SANDBOX_PROFILE, SANDBOX_ROLE,
    SANDBOXD_REPOSITORY, TIMER_GROUP, TIMER_ROLE,
};
use crate::{InstanceClass, Repositories, Settings, Timer};

mod templates;

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Versions {
    // The one that the call names: of a version that a template lacks, EC2 fails with
    // `InvalidLaunchTemplateId.VersionNotFound`.
    launch_template_versions: [Version; 1],
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Version {
    launch_template_data: Data,
}

/// Fills in `filling` the placeholders of the provider's policies with what they stand for in the
/// envelope that `settings` name, with read-only calls on AWS. Returns a problem for each read
/// that fails.
pub fn fill(settings: &Settings, filling: &mut Filling) -> Vec<Problem> {
    let Settings {
        region,
        classes,
        repositories: Repositories { roles, sandboxd },
        timer: Timer { group, role_arn },
    } = settings;
    filling.set(&TIMER_ROLE, role_arn.as_str().to_owned());
    filling.set(&TIMER_GROUP, group.0.clone());
    let arns = (classes.values())
        .map(|class| filling.arn("ec2", &format!("launch-template/{}", class.template.0)));
    filling.list(&LAUNCH_TEMPLATES, arns.collect());
    for (placeholder, name) in [(&ROLES_REPOSITORY, roles), (&SANDBOXD_REPOSITORY, sandboxd)] {
        let arn = filling.arn("ecr", &format!("repository/{}", name.as_str()));
        filling.set(placeholder, arn);
    }
    let cli = match Cli::connect(region.clone()) {
        Ok(cli) => cli,
        Err(error) => return vec![Problem::new("the EC2 provider's envelope", error)],
    };
    let mut problems = Vec::new();
    let mut templates = Vec::new();
    for (name, class) in classes {
        match template(&cli, class) {
            Ok(data) => templates.push(data),
            Err(error) => {
                problems.push(Problem::unread(named(name, class), "describing it", error));
            }
        }
    }
    let Named {
        instance_types,
        profiles,
    } = templates::named(&templates);
    // With no type to fill, the launch's statement goes unsimulated, not simulated for no type.
    if !instance_types.is_empty() {
        filling.list(&INSTANCE_TYPES, instance_types.into_iter().collect());
    }
    // Templates that name no profile, or several, leave the profile's statements unsimulated.
    let profiles: Vec<String> = profiles.into_iter().collect();
    if let [profile] = &profiles[..] {
        match cli.instance_profile(&CHECKS.profile, profile) {
            // IAM holds an instance profile to one role at most.
            Ok(InstanceProfile { arn, roles }) => {
                if let [role] = &roles[..] {
                    filling.set(&SANDBOX_PROFILE, arn);
                    filling.set(&SANDBOX_ROLE, role.arn.clone());
                }
            }
            Err(error) => problems.push(Problem::unread(
                format!("instance profile {profile}"),
                "reading it",
                error,
            )),
        }
    }
    problems
}

fn template(cli: &Cli, class: &InstanceClass) -> Result<Data, String> {
    let version = class.version.to_string();
    let args = [
        "--launch-template-id",
        &class.template.0,
        "--versions",
        &version,
    ];
    let Versions {
        launch_template_versions: [version],
    } = cli.json(&CHECKS.templates, &args, Bound::Call)?;
    Ok(version.launch_template_data)
}

/// How a problem names the template of class `class`, whose settings are `settings`.
fn named(class: &Name, settings: &InstanceClass) -> String {
    let InstanceClass {
        template, version, ..
    } = settings;
    format!(
        "launch template {}, version {version}, of class `{class}`",
        template.0
    )
}
