//! How a run ended, as its `outcome.json` says, read in full or as far as its state alone.

use std::path::Path;
use std::{fs, io};

use schema::ir::Name;
use serde::Deserialize;
use serde_json::Value;

use crate::Stopped;
use crate::log::{self, outcome};
use crate::read_error::{ReadError, form, read_optional};

/// Whether a run finished, failed or was interrupted, as its `outcome.json` says, and nothing of
/// how.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum State {
    Finished,
    Failed,
    Interrupted,
}

impl State {
    /// The state that the `outcome.json` of the record `dir` gives; `None` while it has none.
    /// Reads nothing of the file but `state`, so a record whose ending this build doesn't read
    /// still gives its state.
    pub(super) fn read(dir: &Path) -> Result<Option<Self>, ReadError> {
        let path = dir.join("outcome.json");
        let Some(outcome) = read_outcome(&path)? else {
            return Ok(None);
        };
        let state = Self::deserialize(&outcome["state"]).map_err(unread(&path))?;
        Ok(Some(state))
    }
}

/// How a run ended, as its record says. A variant's sentence reads after the run's id, as in
/// `<id> failed: <reason>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ending {
    /// It finished with the result that its final call returned. `values` are the values the
    /// result shows, each by its name, in their order.
    Returned { values: Vec<(Name, Value)> },
    /// It finished without a result because the final call gave none:
    /// `finished without a result: <why>`.
    NoResult(String),
    /// It stopped without an outcome, as the `Stopped`'s words say.
    Stopped(Stopped),
    /// Its driver ended without an outcome.
    Lost,
}

/// The `outcome.json` at `path`, parsed; `None` while there is none.
pub(super) fn read_outcome(path: &Path) -> Result<Option<Value>, ReadError> {
    let Some(bytes) = read_optional(path)? else {
        return Ok(None);
    };
    let outcome = serde_json::from_slice(&bytes).map_err(form(path.display()))?;
    Ok(Some(outcome))
}

/// How the run whose record is `dir` ended, as its `outcome.json` says; `None` while it has none.
pub(super) fn read(dir: &Path) -> Result<Option<Ending>, ReadError> {
    let Some(ended) = ended(dir)? else {
        return Ok(None);
    };
    let ending = match ended {
        outcome::Ended::Finished(outcome) => match outcome.verdict {
            outcome::Verdict::Returned { result } => Ending::Returned {
                values: result.values(),
            },
            outcome::Verdict::NoResult { failure } => failed_final(failure, dir),
        },
        outcome::Ended::Failed { reason } => Ending::Stopped(Stopped::Failed(reason)),
        outcome::Ended::Interrupted { signal } => Ending::Stopped(Stopped::Interrupted(signal)),
    };
    Ok(Some(ending))
}

fn ended(dir: &Path) -> Result<Option<outcome::Ended>, ReadError> {
    let path = dir.join("outcome.json");
    read_outcome(&path)?
        .map(|value| outcome::Ended::deserialize(value).map_err(unread(&path)))
        .transpose()
}

fn unread(path: &Path) -> impl Fn(serde_json::Error) -> ReadError {
    |error| ReadError::Form(format!("{} doesn't read: {error}", path.display()))
}

fn failed_final(failure: outcome::Failure, dir: &Path) -> Ending {
    let failure = match failure {
        outcome::Failure::Timeout => "timeout".to_owned(),
        outcome::Failure::Error { reason } => format!("error: {reason}"),
    };
    let logs = match log::final_call(dir) {
        Ok(job) => logs(&dir.join(format!("jobs/{job}/log"))),
        Err(error) => format!("its commands' logs can't be found: {error}"),
    };
    Ending::NoResult(format!(
        "finished without a result: the final call gave none ({failure}); {logs}"
    ))
}

/// Where the final call's commands' logs are, `logs`, as its record has them: a call that ran no
/// command, as one whose target never started, has none.
fn logs(logs: &Path) -> String {
    match fs::symlink_metadata(logs) {
        Ok(_) => format!("its commands' logs are in {}", logs.display()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => "it ran no command".to_owned(),
        Err(error) => format!("its commands' logs can't be read: {error}"),
    }
}
