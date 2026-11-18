//! The files a copy holds, and the files and directories that fill it in.
//! Paths travel as Unix names, including ones that aren't UTF-8.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use schema::ir::Mode;
use serde::{Deserialize, Serialize};
use stdx::Context as _;

// Several times the entries of a record whose agent wrote many files.
const RECORD_ENTRIES: usize = 100_000;

#[derive(Serialize, Deserialize)]
pub(crate) struct CopyRequest {
    pub(crate) id: String,
    pub(crate) held_bytes: usize,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Item {
    Directory {
        path: OsString,
    },
    File {
        path: OsString,
        stamp: Stamp,
        mode: Mode,
    },
    /// Bytes of the preceding file, up to the length it named.
    Data,
    Copied {
        unread: Option<String>,
        writer: Option<String>,
    },
}

/// As in rsync's quick check, a file of the same size and modification time is already held.
// https://github.com/RsyncProject/rsync/blob/6118037350efb253fdcc11139f1f58a2582d5fe7/generator.c#L666-L690
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Stamp {
    pub(crate) size_bytes: u64,
    pub(crate) modified_seconds: i64,
}

impl Stamp {
    pub(crate) fn of(metadata: &fs::Metadata) -> Self {
        Self {
            size_bytes: metadata.len(),
            modified_seconds: metadata.mtime(),
        }
    }
}

#[derive(Default)]
pub(crate) struct Entries {
    pub(crate) dirs: Vec<PathBuf>,
    pub(crate) files: BTreeMap<PathBuf, Stamp>,
}

impl Entries {
    pub(crate) fn at(root: &Path) -> Result<Self, String> {
        let mut found = Self::default();
        let mut walk = vec![PathBuf::new()];
        while let Some(below) = walk.pop() {
            let dir = root.join(&below);
            for entry in fs::read_dir(&dir).context(dir.display())? {
                let entry = entry.context(dir.display())?;
                let path = below.join(entry.file_name());
                let meta = fs::symlink_metadata(entry.path()).context(entry.path().display())?;
                if meta.is_dir() {
                    walk.push(path.clone());
                    found.dirs.push(path);
                } else if meta.is_file() {
                    found.files.insert(path, Stamp::of(&meta));
                }
                if found.dirs.len() + found.files.len() > RECORD_ENTRIES {
                    return Err(format!(
                        "{} holds over {RECORD_ENTRIES} files and directories: pack the largest \
                         directories into archives and copy the record again",
                        root.display()
                    ));
                }
            }
        }
        Ok(found)
    }

    /// A JSON stream of pairs, each a file's name and stamp, in a bounded request body.
    /// A file left out at the bound is sent again.
    pub(crate) fn held(&self, limit: usize) -> Vec<u8> {
        let mut body = Vec::new();
        for (path, stamp) in &self.files {
            let before = body.len();
            serde_json::to_writer(&mut body, &(path.as_os_str(), stamp)).unwrap_or_else(|error| {
                unreachable!("a Unix name and two integers written into a Vec are JSON: {error}")
            });
            if body.len() > limit {
                body.truncate(before);
                break;
            }
        }
        body
    }
}
