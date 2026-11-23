//! The files a copy holds, and the files and directories that fill it in.
//! Paths travel as Unix names, including ones that aren't UTF-8.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::{fs, io};

use schema::ir::Mode;
use serde::{Deserialize, Serialize};
use stdx::Context as _;

use super::{Ask, Link, Request, Unanswered};

// Set from the largest controller record measured:
// https://github.com/FrontierCS/Pumpkin/pull/121
const RECORD_ENTRIES: usize = 100_000;

#[derive(Serialize, Deserialize)]
pub(crate) struct CopyRequest {
    pub(crate) id: String,
    pub(crate) held_bytes: usize,
}

/// That the client holds a whole copy of run `id`'s record, for the member that its link acts for.
/// If that member is the run's, the controller keeps its record `retention::KEPT_DAYS` days from
/// then.
#[derive(Serialize, Deserialize)]
pub(crate) struct CopiedRequest {
    pub(crate) id: String,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum CopiedHead {
    Noted,
}

impl Request for CopiedRequest {
    type Answer = ();
    type Head = CopiedHead;

    fn ask(self) -> Ask {
        Ask::Copied(self)
    }

    fn sent((): ()) -> io::Result<(CopiedHead, Vec<u8>)> {
        Ok((CopiedHead::Noted, Vec::new()))
    }

    fn received(head: CopiedHead, _: &mut Link) -> Result<(), Unanswered> {
        let CopiedHead::Noted = head;
        Ok(())
    }
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
    pub(crate) unread: Option<String>,
}

impl Entries {
    pub(crate) fn at(root: &Path) -> Result<Self, String> {
        let mut found = Self::default();
        walk(root, |entry| {
            match entry {
                Ok((path, meta)) if meta.is_dir() => found.dirs.push(path),
                Ok((path, meta)) if meta.is_file() => {
                    found.files.insert(path, Stamp::of(&meta));
                }
                Ok(_) => {}
                Err(error) => {
                    found.unread.get_or_insert(error);
                }
            }
            if found.dirs.len() + found.files.len() > RECORD_ENTRIES {
                return Err(format!(
                    "{} holds over {RECORD_ENTRIES} files and directories: pack the largest \
                     directories into archives and copy the record again",
                    root.display()
                ));
            }
            Ok(())
        })?;
        Ok(found)
    }
}

/// A JSON stream of the names and stamps found in the copy. A file left out is sent again.
pub(crate) fn held(root: &Path, limit: usize) -> Vec<u8> {
    let mut body = Vec::new();
    let mut left = RECORD_ENTRIES;
    let _ = walk(root, |entry| {
        left = left.checked_sub(1).ok_or(())?;
        if let Ok((path, meta)) = entry
            && meta.is_file()
        {
            let before = body.len();
            let pair = (path.as_os_str(), Stamp::of(&meta));
            serde_json::to_writer(&mut body, &pair).unwrap_or_else(|error| {
                unreachable!("a Unix name and two integers written into a Vec are JSON: {error}")
            });
            if body.len() > limit {
                body.truncate(before);
                return Err(());
            }
        }
        Ok(())
    });
    body
}

fn walk<E>(
    root: &Path,
    mut visit: impl FnMut(Result<(PathBuf, fs::Metadata), String>) -> Result<(), E>,
) -> Result<(), E> {
    let mut pending = vec![PathBuf::new()];
    while let Some(below) = pending.pop() {
        let dir = root.join(&below);
        let entries = match fs::read_dir(&dir).context(dir.display()) {
            Ok(entries) => entries,
            Err(error) => {
                visit(Err(error))?;
                continue;
            }
        };
        for entry in entries {
            let entry = entry.context(dir.display()).and_then(|entry| {
                let path = below.join(entry.file_name());
                let meta = fs::symlink_metadata(entry.path()).context(entry.path().display())?;
                if meta.is_dir() {
                    pending.push(path.clone());
                }
                Ok((path, meta))
            });
            visit(entry)?;
        }
    }
    Ok(())
}
