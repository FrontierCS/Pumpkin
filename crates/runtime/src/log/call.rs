//! A run's calls, as its log tells them.

use std::num::NonZeroU64;
use std::time::Duration;

use schema::ir::Name;
use serde::{Deserialize as _, Serialize};
use serde_json::Value;

use super::logged::Action;
use super::progress::{ModelUse, Sandbox};

/// A call: the final call, which judges the selection, or the agent's call of a job function.
#[derive(Clone, Debug)]
pub struct CallSpan {
    /// Its number in the run.
    pub job: u64,
    pub of: CallOf,
    /// The role that runs it: the function's target, or the final call's.
    pub role: Option<String>,
    pub sandbox: Sandbox,
    /// How long the core gave it.
    pub timeout: Duration,
    /// Whether the core ordered it stopped.
    pub stopped: bool,
    /// Whether it ran past its timeout.
    pub timed_out: bool,
    /// How the call ended, collected or failed, at its sandbox's `gone`, with how many of its
    /// model requests a spent model budget barred, as its end said: none if it barred none.
    pub end: Option<(CallEnd, Option<NonZeroU64>)>,
    /// Its own model requests, which the run's (`Progress::model`) count too.
    pub model: ModelUse,
}

impl CallSpan {
    /// The span of job `job`, which calls `of`, started on `role`'s `sandbox`.
    pub(super) fn new(
        job: u64,
        of: CallOf,
        role: Option<String>,
        sandbox: Sandbox,
        timeout: Duration,
    ) -> Self {
        Self {
            job,
            of,
            role,
            sandbox,
            timeout,
            stopped: false,
            timed_out: false,
            end: None,
            model: ModelUse::default(),
        }
    }

    pub(super) fn end(
        &mut self,
        at: Duration,
        end: CallEnd,
        model_requests_over_budget: Option<NonZeroU64>,
    ) {
        self.end = Some((end, model_requests_over_budget));
        self.sandbox.gone = Some(at);
    }
}

/// Whose body a call calls, written `{"function": <name>}` or `"final"`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CallOf {
    /// The agent's job function of this name.
    Function(String),
    /// The final call, which judges the selection.
    Final,
}

/// A call's start, as an action of the log's says it.
#[derive(Clone, Debug)]
pub struct CallStart {
    pub job: u64,
    pub of: CallOf,
    /// The size of the call's payload.
    pub payload_bytes: u64,
}

impl CallStart {
    /// The call's start that `action`, an action of a line of the log's, is, if it reads as one.
    pub fn read(action: &Value) -> Option<Self> {
        let start = |job, of, payload: super::logged::Payload| Self {
            job,
            of,
            payload_bytes: payload.size,
        };
        match Action::deserialize(action).ok()? {
            Action::StartCall {
                job,
                grant,
                payload,
                timeout_ns: _,
            } => Some(start(job, CallOf::Function(grant), payload)),
            Action::StartFinalCall {
                job,
                payload,
                timeout_ns: _,
            } => Some(start(job, CallOf::Final, payload)),
            Action::StartAgent
            | Action::Reply
            | Action::StopCall { .. }
            | Action::StartLease { .. }
            | Action::RunCommand { .. }
            | Action::Transfer { .. }
            | Action::CloseLease { .. }
            | Action::TearDownAgent
            | Action::Finish
            | Action::Abandon
            | Action::Unknown => None,
        }
    }
}

/// What came of a call: the values it gave, or why it gave none.
#[derive(Clone, Debug)]
pub enum CallEnd {
    /// Its body returned these values, each by its name, in its order.
    Result(Vec<(Name, Value)>),
    /// The run couldn't start the call or keep its sandbox, for this reason.
    Failed(String),
}
