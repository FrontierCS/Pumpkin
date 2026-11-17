//! A run's calls, as its log tells them.

use std::path::PathBuf;
use std::time::Duration;

use schema::ir::Name;
use serde_json::Value;

use super::progress::{ModelUse, Sandbox};

/// A call: the final judge's, which scores the selected submission, or the agent's call of a job
/// function.
#[derive(Clone, Debug)]
pub struct CallSpan {
    /// Its number in the run.
    pub job: u64,
    /// The job function it calls; `None` for the final judge.
    pub function: Option<String>,
    /// The role that runs it: the final judge, or the function's target.
    pub role: Option<String>,
    pub sandbox: Sandbox,
    /// How long the core gave it.
    pub timeout: Duration,
    /// Whether the core ordered it stopped.
    pub stopped: bool,
    /// Whether it ran past its timeout.
    pub timed_out: bool,
    /// How the call ended, collected or failed, at its sandbox's `gone`.
    pub end: Option<CallEnd>,
    /// Its judge's standard error in the record, if the judge wrote any when the readout was made.
    pub stderr: Option<PathBuf>,
    /// The directory in the record that holds its sandbox's `out/evidence`, if there is one: the
    /// final judge's evidence, or what a function's target wrote there.
    pub evidence: Option<PathBuf>,
    /// Its own model requests, which the run's (`Progress::model`) count too.
    pub model: ModelUse,
}

impl CallSpan {
    /// The span of job `job`, which calls `function`, started on `role`'s `sandbox`.
    pub(super) fn new(
        job: u64,
        function: Option<String>,
        role: Option<String>,
        sandbox: Sandbox,
        timeout: Duration,
    ) -> Self {
        Self {
            job,
            function,
            role,
            sandbox,
            timeout,
            stopped: false,
            timed_out: false,
            end: None,
            stderr: None,
            evidence: None,
            model: ModelUse::default(),
        }
    }

    pub(super) fn end(&mut self, at: Duration, end: CallEnd) {
        self.end = Some(end);
        self.sandbox.gone = Some(at);
    }
}

/// What came of a call: the values it gave, or why it gave none.
#[derive(Clone, Debug)]
pub enum CallEnd {
    /// The judge gave a result, which shows these values, each by its name, in the order its
    /// judge's spec names them; or the job function's body returned them, in its order.
    Result(Vec<(Name, Value)>),
    /// The judge's output was no result its spec reads, for this reason.
    Invalid(String),
    /// The run couldn't start the call or keep its sandbox, for this reason.
    Failed(String),
}
