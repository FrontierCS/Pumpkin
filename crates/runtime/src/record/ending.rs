//! A run's [`Ending`], and how a finished run's is read from its `outcome.json`.

use std::path::Path;
use std::{fs, io};

use schema::ir::Name;
use schema::sandbox::ExitStatus;
use serde::Deserialize as _;
use serde_json::Value;

use crate::log::{self, outcome};

/// How a run ended, as its record says. A variant's sentence reads after the run's id, as in
/// `<id> failed: <reason>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ending {
    /// It finished with the result that its final call returned. `values` are the values the
    /// result shows, each by its name, in their order; `after` says how the work phase ended:
    /// `after the work phase ended <how>`.
    Returned {
        values: Vec<(Name, Value)>,
        after: String,
    },
    /// It finished without a result, as the agent submitted nothing or its final call gave none:
    /// `finished without a result: <why>`.
    NoResult(String),
    /// It stopped without an outcome, as `Stopped` says in words: `failed: <reason>`.
    Failed(String),
    /// A signal stopped it, as `Stopped` says in words: `was interrupted by <signal>`.
    Interrupted(String),
    /// Its driver ended without an outcome.
    Lost,
}

pub(super) fn finished(outcome: &Value, dir: &Path) -> Result<Ending, String> {
    let read = outcome::Finished::deserialize(outcome).map_err(|error| {
        let path = dir.join("outcome.json");
        format!("{} doesn't read: {error}", path.display())
    })?;
    let outcome::Finished { work_end, verdict } = read;
    let ended = match work_end {
        outcome::WorkEnd::Deadline => "at its deadline".to_owned(),
        outcome::WorkEnd::Finalized => "when the agent finalized".to_owned(),
        outcome::WorkEnd::AgentExited { status } => match status {
            ExitStatus::Code(code) => format!("when the agent exited with status {code}"),
            ExitStatus::Signal(signal) => format!("when signal {signal} killed the agent"),
        },
        outcome::WorkEnd::AgentFailed { reason } => format!("when the agent failed: {reason}"),
    };
    let without = |why: String| format!("finished without a result: {why}");
    let ending = match verdict {
        outcome::Verdict::Returned {
            submission: _,
            result,
        } => Ending::Returned {
            values: result.values(),
            after: format!("after the work phase ended {ended}"),
        },
        outcome::Verdict::NoSubmission => Ending::NoResult(without(format!(
            "the agent submitted nothing, and the work phase ended {ended}"
        ))),
        outcome::Verdict::NoResult {
            submission: _,
            failure,
        } => {
            let failure = match failure {
                outcome::Failure::Timeout => "timeout".to_owned(),
                outcome::Failure::Error { reason } => format!("error: {reason}"),
            };
            let logs = match log::final_call(dir) {
                Ok(job) => logs(&dir.join(format!("jobs/{job}/log"))),
                Err(error) => format!("its commands' logs can't be found: {error}"),
            };
            Ending::NoResult(without(format!(
                "the final call gave none ({failure}), after the work phase ended {ended}; {logs}"
            )))
        }
    };
    Ok(ending)
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
