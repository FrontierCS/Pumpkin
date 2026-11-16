//! A run's [`Ending`], and how a finished run's is read from its `outcome.json`.

use std::path::{Path, PathBuf};

use schema::ir::Name;
use schema::sandbox::command_stderr;
use serde_json::Value;

use crate::log;

/// How a run ended, as its record says. A variant's sentence reads after the run's id, as in
/// `<id> failed: <reason>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ending {
    /// It finished with a result. `values` are the values the result shows, each by its name, in
    /// their order; `after` says how the work phase ended: `after the work phase ended <how>`.
    Judged {
        values: Vec<(Name, Value)>,
        after: String,
    },
    /// It finished without a result, as the agent submitted nothing or its verdict is of a kind
    /// this pumpkin doesn't know: `finished without a result: <why>`.
    Unjudged(String),
    /// It finished without a result as its final judge gave none: `finished without a result:
    /// <why>`. `judge_stderr` is where the record holds that judge's standard error, which may not
    /// have been collected, or why the run log doesn't say.
    NoResult {
        why: String,
        judge_stderr: Result<PathBuf, String>,
    },
    /// It stopped without an outcome, as `Stopped` says in words: `failed: <reason>`.
    Failed(String),
    /// A signal stopped it, as `Stopped` says in words: `was interrupted by <signal>`.
    Interrupted(String),
    /// Its driver ended without an outcome.
    Lost,
}

pub(super) fn finished(outcome: &Value, dir: &Path) -> Result<Ending, String> {
    let work_end = &outcome["work_end"];
    let status = &work_end["status"];
    let ended = match work_end["kind"].as_str() {
        Some("deadline") => "at its deadline".to_owned(),
        Some("finalized") => "when the agent finalized".to_owned(),
        Some("agent_exited") => match (status["code"].as_i64(), status["signal"].as_i64()) {
            (Some(code), _) => format!("when the agent exited with status {code}"),
            (None, Some(signal)) => format!("when signal {signal} killed the agent"),
            (None, None) => "when the agent exited".to_owned(),
        },
        Some("agent_failed") => {
            let reason = work_end["reason"]
                .as_str()
                .unwrap_or("no reason is recorded");
            format!("when the agent failed: {reason}")
        }
        Some(_) | None => "in a way this pumpkin doesn't know".to_owned(),
    };
    let without = |why: String| format!("finished without a result: {why}");
    match outcome["verdict"]["kind"].as_str() {
        Some("judged") => {
            let values = log::result_values(&outcome["verdict"]["result"]).map_err(|error| {
                let path = dir.join("outcome.json");
                format!("{}'s values don't read: {error}", path.display())
            })?;
            Ok(Ending::Judged {
                values,
                after: format!("after the work phase ended {ended}"),
            })
        }
        Some("no_submission") => Ok(Ending::Unjudged(without(format!(
            "the agent submitted nothing, and the work phase ended {ended}"
        )))),
        Some("no_result") => {
            let failure = &outcome["verdict"]["failure"];
            let kind = failure["kind"].as_str().unwrap_or("of an unknown kind");
            let detail = failure["reason"]
                .as_str()
                .map_or_else(String::new, |r| format!(": {r}"));
            Ok(Ending::NoResult {
                why: without(format!(
                    "the final judge gave none ({kind}{detail}), after the work phase ended \
                     {ended}"
                )),
                judge_stderr: final_judge_stderr(dir),
            })
        }
        Some(_) | None => Ok(Ending::Unjudged(without(format!(
            "its verdict is of a kind this pumpkin doesn't know, after the work phase ended {ended}"
        )))),
    }
}

fn final_judge_stderr(dir: &Path) -> Result<PathBuf, String> {
    let job = dir.join(format!("jobs/{}", log::final_judge(dir)?));
    Ok(job.join(command_stderr().as_str()))
}
