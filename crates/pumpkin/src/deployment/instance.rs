//! What the check of the controller read of its instance ([`Described`]), and what is wrong with
//! that, decided of those values alone.

use std::net::Ipv4Addr;

use aws::{InstanceProfile, Problem};
use serde::Deserialize;

/// What the check of the controller read of its instance, each part as AWS describes it, or why
/// reading it failed.
pub(crate) struct Described {
    pub(crate) instance: Instance,
    /// The instance profile that it names, if it names one.
    pub(crate) profile: Option<Result<InstanceProfile, String>>,
}

/// What `aws ec2 describe-instances` prints of an instance, as the check reads it.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct Instance {
    #[serde(rename = "InstanceId")]
    pub(crate) id: String,
    pub(crate) state: State,
    public_ip_address: Option<Ipv4Addr>,
    pub(crate) iam_instance_profile: Option<ProfileNamed>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct State {
    pub(crate) name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct ProfileNamed {
    pub(crate) arn: String,
}

impl Described {
    /// What is wrong with the instance, each problem of `resource`, which names it: its instance
    /// profile, whose role the check simulates, and, while it runs, its public address.
    pub(crate) fn problems(&self, resource: &str) -> Vec<Problem> {
        let mut problems = Vec::new();
        match &self.profile {
            None => problems.push(Problem::new(
                resource,
                "it has no instance profile, whose role its jobs call AWS with: apply deploy/aws, \
                 which gives it one",
            )),
            Some(Err(error)) => {
                problems.push(Problem::unread(
                    resource,
                    "reading its instance profile",
                    error,
                ));
            }
            Some(Ok(profile)) if profile.roles.is_empty() => problems.push(Problem::new(
                resource,
                format!(
                    "its instance profile {} holds no role: give it the controller's, as \
                     deploy/aws does",
                    profile.arn
                ),
            )),
            Some(Ok(_)) => {}
        }
        // An apply leaves the instance's public address as it is, so a hand edit holds until the
        // instance is replaced.
        if self.instance.state.name == "running" && self.instance.public_ip_address.is_none() {
            problems.push(Problem::new(
                resource,
                "it runs with no public IPv4 address, which its jobs reach AWS and Modal \
                 through, and which Modal's tunnels admit: give it one, as its launch by \
                 deploy/aws does",
            ));
        }
        problems
    }

    /// What of the instance goes unchecked while it doesn't run, as a problem of `resource`,
    /// which names it, that fails nothing. It says to start it by `pumpkin ls` of controller
    /// `name`.
    pub(crate) fn unchecked(&self, resource: &str, name: &str) -> Option<Problem> {
        let wrong = match self.instance.state.name.as_str() {
            "running" => return None,
            "pending" => {
                "it is starting, so its public IPv4 address is unchecked: check again once it runs"
                    .to_owned()
            }
            state => format!(
                "it is {state}, so its public IPv4 address is unchecked: run `pumpkin ls \
                 --controller {name}` to start it, then check again"
            ),
        };
        Some(Problem::new(resource, wrong))
    }

    /// The ARN of the role of its instance profile, if IAM describes one.
    pub(crate) fn role(&self) -> Option<&str> {
        let Some(Ok(profile)) = &self.profile else {
            return None;
        };
        // IAM holds an instance profile to one role at most.
        let [role] = &profile.roles[..] else {
            return None;
        };
        Some(&role.arn)
    }
}
