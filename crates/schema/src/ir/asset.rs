//! The files that a role's sandboxes hold ([`Role::assets`](super::Role::assets)).

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::values::{AssetPath, HttpsUrl, RelativePath, Sha256Digest};

/// Files at a path in each of a role's sandboxes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Asset {
    pub path: AssetPath,
    pub files: AssetFiles,
}

/// The files of an [`Asset`], each pinned by its SHA-256: 1 to 100,000 files, no two at one path,
/// and none below another's path.
///
/// A digest names them, by which a provider keeps them once for every asset that holds them. It
/// is `sha256:` and the SHA-256 of one line for each file, in the bytewise order of their paths.
/// A line is the file's SHA-256 as 64 hexadecimal digits, two spaces, its path and a newline. The
/// URLs and sizes stay out, as where a file is kept isn't what it is.
// The lines are those of Go's module hash, which it hashes again too:
// https://github.com/golang/mod/blob/b71060237c896c7c9fc602ab66e33ea6079659fa/sumdb/dirhash/hash.go#L30-L65
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<AssetFile>", into = "Vec<AssetFile>")]
pub struct AssetFiles(Vec<AssetFile>);

/// A file of an asset.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetFile {
    /// Where it is below its asset's path. It holds no newline.
    pub path: RelativePath,
    pub url: HttpsUrl,
    pub size: u64,
    pub sha256: Sha256Digest,
}

/// The most files that an asset holds.
// As many as the entries that the host collects from a sandbox in one tree, the most that Pumpkin
// handles in any tree (`runtime`'s `ENTRIES`). An asset is a tree in each of its sandboxes.
const MAX_ASSET_FILES: usize = 100_000;

impl AssetFiles {
    /// Each file, in the IR's order.
    pub fn as_slice(&self) -> &[AssetFile] {
        &self.0
    }
}

impl TryFrom<Vec<AssetFile>> for AssetFiles {
    type Error = String;

    fn try_from(files: Vec<AssetFile>) -> Result<Self, Self::Error> {
        if files.is_empty() {
            return Err("the asset holds no file: give one at least".to_owned());
        }
        if files.len() > MAX_ASSET_FILES {
            return Err(format!(
                "the asset holds {} files: give at most {MAX_ASSET_FILES}",
                files.len()
            ));
        }
        let mut paths = BTreeSet::new();
        for file in &files {
            let path = file.path.as_str();
            // The digest gives each file a line.
            if path.contains('\n') {
                return Err(format!(
                    "`{}` holds a newline: give the file a path without one",
                    path.escape_debug()
                ));
            }
            if !paths.insert(path) {
                return Err(format!(
                    "`{path}` is the path of more than one file: give each file a path of its own"
                ));
            }
        }
        for path in &paths {
            let mut parents = path.match_indices('/').map(|(slash, _)| &path[..slash]);
            if let Some(file) = parents.find(|parent| paths.contains(parent)) {
                return Err(format!(
                    "`{path}` is below `{file}`, which is a file of the asset: put it in a \
                     directory"
                ));
            }
        }
        Ok(Self(files))
    }
}

impl From<AssetFiles> for Vec<AssetFile> {
    fn from(files: AssetFiles) -> Self {
        files.0
    }
}
