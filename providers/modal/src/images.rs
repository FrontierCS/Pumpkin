//! The references of the images that the provider publishes. Each of a role's or a companion's is
//! the image's id in Modal, then `@` and the image's digest, such as `im-…@sha256:…`. Each of an
//! asset's is the id alone ([`ImageId`]).

use std::collections::{BTreeMap, BTreeSet};

use provider::{AssetRef, ImageRef};
use schema::ir::Image;
use serde::Deserialize;

const ID_PREFIX: &str = "im-";

/// The id of an image of Modal's: `im-`, then letters and digits.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub(crate) struct ImageId(String);

impl ImageId {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ImageId {
    type Error = String;

    fn try_from(id: String) -> Result<Self, Self::Error> {
        let rest = id.strip_prefix(ID_PREFIX).unwrap_or_default();
        if rest.is_empty() || !rest.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            return Err(format!(
                "`{}` is no id of an image of Modal's: expected `{ID_PREFIX}`, then letters and \
                 digits",
                id.escape_debug()
            ));
        }
        Ok(Self(id))
    }
}

impl From<ImageId> for AssetRef {
    fn from(id: ImageId) -> Self {
        Self::from(id.0)
    }
}

pub(crate) fn asset_image(asset: &AssetRef) -> Result<&str, String> {
    match ImageId::try_from(asset.as_str().to_owned()) {
        Ok(_) => Ok(asset.as_str()),
        Err(_) => Err(format!(
            "`{}` is no asset this provider published: publish one first",
            asset.as_str()
        )),
    }
}

/// The reference of `image`, which Modal imported as `imported`, its id there.
pub(crate) fn reference(imported: &str, image: &Image) -> ImageRef {
    ImageRef::from(format!("{imported}@{image}"))
}

/// The id in Modal of `image`, as its reference holds it.
pub(crate) fn imported(image: &ImageRef) -> Result<&str, String> {
    match image.as_str().split_once('@') {
        Some((imported, _)) => Ok(imported),
        None => Err(format!(
            "`{}` is no image this provider published: publish one first",
            image.as_str()
        )),
    }
}

/// The images of one publish, of which Modal imports each once, however often it is named.
pub(crate) struct Batch<'a> {
    images: &'a [Image],
    distinct: BTreeSet<&'a Image>,
}

impl<'a> Batch<'a> {
    pub(crate) fn of(images: &'a [Image]) -> Self {
        Self {
            images,
            distinct: images.iter().collect(),
        }
    }

    /// Each image of the batch once, in the order in which [`references`](Self::references)
    /// takes their ids.
    pub(crate) fn distinct(&self) -> impl Iterator<Item = &'a Image> {
        self.distinct.iter().copied()
    }

    /// One reference for each image of the batch, in the batch's order. `imported` are the ids in
    /// Modal of the batch's [`distinct`](Self::distinct) images, in their order.
    ///
    /// Fails if `imported` isn't one id for each distinct image.
    pub(crate) fn references(self, imported: Vec<String>) -> Result<Vec<ImageRef>, String> {
        let Self { images, distinct } = self;
        if imported.len() != distinct.len() {
            return Err(format!(
                "the Modal helper's `import` answered {} ids for {} images: expected one for each",
                imported.len(),
                distinct.len()
            ));
        }
        let imported: BTreeMap<&Image, String> = distinct.into_iter().zip(imported).collect();
        // Each image of `images` is one of `distinct`, which is made of them.
        let references = (images.iter()).map(|image| reference(&imported[image], image));
        Ok(references.collect())
    }
}
