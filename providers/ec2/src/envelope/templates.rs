//! What the read of an envelope takes of its launch templates, as EC2 describes them ([`Data`]):
//! the instance types and the instance profiles that they name ([`named`]).

use std::collections::BTreeSet;

use serde::Deserialize;

/// What a version of a launch template names, of what the read takes, as `aws ec2
/// describe-launch-template-versions` prints its `LaunchTemplateData`.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct Data {
    instance_type: Option<String>,
    iam_instance_profile: Option<ProfileNamed>,
}

/// An instance profile, which a template names by its ARN or by its name.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ProfileNamed {
    arn: Option<String>,
    name: Option<String>,
}

/// What a set of templates names, each value once.
#[derive(Debug, Default)]
pub(crate) struct Named {
    pub(crate) instance_types: BTreeSet<String>,
    /// The names of the instance profiles.
    pub(crate) profiles: BTreeSet<String>,
}

/// What `templates` name.
pub(crate) fn named<'d>(templates: impl IntoIterator<Item = &'d Data>) -> Named {
    let mut named = Named::default();
    for data in templates {
        named.instance_types.extend(data.instance_type.clone());
        let profile = data.iam_instance_profile.as_ref();
        let name = profile.and_then(|profile| match (&profile.name, &profile.arn) {
            (Some(name), _) => Some(name.as_str()),
            // Of an ARN, the name follows the path.
            (None, Some(arn)) => Some(arn.rsplit_once('/').map_or(arn.as_str(), |(_, name)| name)),
            (None, None) => None,
        });
        named.profiles.extend(name.map(str::to_owned));
    }
    named
}
