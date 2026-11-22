//! What the provider's [`Settings`] name on AWS, as the placeholders of the provider's policies
//! stand for it: where Modal imports the provider's images from, and the role that Modal assumes
//! to ([`fill`]).

use aws::iam::Filling;

use crate::iam::{IMAGES, PULL_ROLE};
use crate::{Images, Settings};

/// Fills in `filling` the placeholders of the provider's policies with what they stand for in
/// `settings`, and [`PULL_ROLE`] with the role that Modal assumes.
pub fn fill(settings: &Settings, filling: &mut Filling) {
    let Images {
        region,
        repository,
        role_arn,
    } = &settings.images;
    let resource = format!("repository/{}", repository.as_str());
    let repository = filling.arn_in("ecr", region.as_str(), &resource);
    filling.set(&IMAGES, repository);
    filling.set(&PULL_ROLE, role_arn.as_str().to_owned());
}
