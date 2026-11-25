//! The references of the images that the provider publishes. Each is the image's id in Modal,
//! then `@` and the image's digest, such as `im-…@sha256:…`.

use std::collections::{BTreeMap, BTreeSet};

use provider::ImageRef;
use schema::ir::Image;

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
