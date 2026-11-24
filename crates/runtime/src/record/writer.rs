//! Who made a record, read from its `record.json` without its `record_version` being checked.

use std::path::Path;
use std::{fs, io};

use serde_json::Value;

use super::{BUILD, write_build};
use crate::read_error::{ReadError, failed, form};

fn shown(build: &Value) -> Option<String> {
    let version = build["version"].as_str()?;
    let commit = match &build["commit"] {
        Value::Null => None,
        commit => Some((commit["hash"].as_str()?, commit["date"].as_str()?)),
    };
    let mut shown = String::new();
    write_build(&mut shown, version, commit).ok()?;
    Some(shown)
}

/// Who made a record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Writer {
    /// This build, [`BUILD`].
    This,
    /// Another build, shown as a [`Build`](super::Build) is.
    Other(String),
    /// The record names no build.
    Older,
}

/// Who made the record in `dir`, read without checking its `record_version`.
///
/// Fails with [`ReadError::Failed`] if `record.json` can't be read, and with [`ReadError::Form`]
/// if it isn't JSON, or names a build in no form that a build writes.
pub fn writer(dir: &Path) -> Result<Writer, ReadError> {
    let path = dir.join("record.json");
    writer_of(&path, &read_record_json(dir, &path)?)
}

pub(super) fn writer_of(path: &Path, record: &Value) -> Result<Writer, ReadError> {
    match &record["pumpkin"] {
        Value::Null => Ok(Writer::Older),
        build if *build == BUILD.json() => Ok(Writer::This),
        build => shown(build).map(Writer::Other).ok_or_else(|| {
            ReadError::Form(format!(
                "{} holds pumpkin {build}: expected the build that made it, \
                 {{\"version\": v, \"commit\": c}}",
                path.display()
            ))
        }),
    }
}

pub(super) fn read_record_json(dir: &Path, path: &Path) -> Result<Value, ReadError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound && !dir.is_dir() => {
            return Err(ReadError::Failed(format!(
                "{} is not a directory: expected the directory of a run's record",
                dir.display()
            )));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(ReadError::Failed(format!(
                "{} has no record.json, so it is no run's record: expected the directory \
                 of one that `pumpkin run` made",
                dir.display()
            )));
        }
        Err(error) => return Err(failed(path.display())(error)),
    };
    serde_json::from_slice(&bytes).map_err(form(path.display()))
}
