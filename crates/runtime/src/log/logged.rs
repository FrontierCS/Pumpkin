//! The run log's lines as its readers read them, in the forms that the run log writes; the `log`
//! module says what a line holds.
//!
//! Each type has the fields of its kind that a reader of the log reads, and no other, so a field
//! that no reader reads can't keep a line from reading. A line's frame, its event and each of its
//! actions read on their own, each whole or not at all. A kind of event or action that this build
//! doesn't know reads as `Unknown`, and changes nothing that a reader works out. A field that a
//! writer may leave out is an `Option` or has a default; a part of a known kind that lacks any
//! other field of its kind, or holds one in another form, doesn't read: its readers leave it out
//! and say so ([`Unread`]), and never read it with defaults.

use std::fmt;
use std::num::NonZeroU64;
use std::time::Duration;

use provider::UnixTime;
use schema::ir::{Api, Name};
use schema::sandbox::ExitStatus;
use schema::sandbox::op::{CommandEnd, TransferEnd};
use serde::de::{DeserializeOwned, Deserializer, Error as _};
use serde::{Deserialize, Serialize, Serializer, de};
use serde_json::Value;

use super::{Part, Unread};
use crate::sandbox::SandboxName;

#[derive(Debug)]
pub(super) struct Line {
    pub(super) at_ns: Duration,
    pub(super) observed_ns: Option<Duration>,
    pub(super) event: Option<Event>,
    pub(super) actions: Vec<Action>,
}

#[derive(Deserialize)]
struct Frame {
    #[serde(deserialize_with = "nanos")]
    at_ns: Duration,
    #[serde(default, deserialize_with = "optional_nanos")]
    observed_ns: Option<Duration>,
    event: Option<Value>,
    #[serde(default)]
    actions: Vec<Value>,
}

impl Line {
    /// Reads the line `number`, counted from 1, that is `bytes`. `None` if it is no JSON object
    /// with an `at_ns`; otherwise the line, without the event or any action that couldn't be read,
    /// each of which is added to `unread`.
    pub(super) fn read(bytes: &[u8], number: u64, unread: &mut Vec<Unread>) -> Option<Self> {
        let frame: Frame = match serde_json::from_slice(bytes) {
            Ok(frame) => frame,
            Err(error) => {
                // serde's message ends where in its input it stopped, which is the line itself.
                let reason = error.to_string();
                let at = format!(" at line {} column {}", error.line(), error.column());
                let reason = match reason.strip_suffix(&at) {
                    Some(reason) => format!("{reason}, at column {}", error.column()),
                    None => reason,
                };
                unread.push(Unread {
                    line: number,
                    part: Part::Line,
                    reason,
                });
                return None;
            }
        };
        let Frame {
            at_ns,
            observed_ns,
            event,
            actions,
        } = frame;
        let event = event.and_then(|event| read_part(event, (number, Part::Event), unread));
        let actions = (actions.into_iter().enumerate())
            .filter_map(|(index, action)| read_part(action, (number, Part::Action(index)), unread))
            .collect();
        Some(Self {
            at_ns,
            observed_ns,
            event,
            actions,
        })
    }

    /// When its event happened: its `observed_ns`, or else when it was written.
    pub(super) fn when(&self) -> Duration {
        self.observed_ns.unwrap_or(self.at_ns)
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum Event {
    RunStarted {
        #[serde(deserialize_with = "unix_nanos")]
        unix_ns: UnixTime,
        #[serde(default, deserialize_with = "optional_nanos")]
        hard_end_ns: Option<Duration>,
    },
    WorkStarted,
    AgentStarted {
        #[serde(deserialize_with = "agent")]
        caller: CallerName,
    },
    AgentExited {
        #[serde(deserialize_with = "agent")]
        caller: CallerName,
        status: ExitStatus,
    },
    AgentFailed {
        #[serde(deserialize_with = "agent")]
        caller: CallerName,
        reason: String,
    },
    AgentCollected {
        #[serde(deserialize_with = "agent")]
        caller: CallerName,
    },
    OpArrived {
        request: Request,
    },
    OpRefused,
    CallStarted {
        job: u64,
    },
    CallTimedOut {
        job: u64,
    },
    CallReturned {
        job: u64,
        returned: Vec<Shown>,
        model_requests_over_budget: Option<NonZeroU64>,
    },
    CallFailed {
        job: u64,
        reason: String,
        model_requests_over_budget: Option<NonZeroU64>,
    },
    LeaseReady {
        lease: u64,
    },
    CommandEnded {
        #[serde(flatten)]
        of: CommandOf,
        end: CommandEnd,
    },
    TransferEnded {
        lease: u64,
        end: TransferEnd,
    },
    LeaseEnded {
        lease: u64,
    },
    LeaseFailed {
        lease: u64,
        reason: String,
    },
    Tick,
    ModelRequestStarted {
        caller: CallerName,
        request: u64,
    },
    ModelAnswered {
        caller: Option<CallerName>,
        request: u64,
        api: Option<Api>,
        model: Option<String>,
        #[serde(default, deserialize_with = "optional_nanos")]
        first_token_ns: Option<Duration>,
        upstream_status: Option<u16>,
        answer: Answer,
        failed: Option<Failed>,
        usage: Option<Usage>,
        #[serde(default)]
        kept: Kept,
    },
    SandboxGpus {
        sandbox: SandboxName,
        gpus: Vec<String>,
    },
    SandboxDeleted {
        sandbox: Option<SandboxName>,
    },
    #[serde(other)]
    Unknown,
}

impl Event {
    pub(super) fn request(&self) -> Option<&Request> {
        match self {
            Self::OpArrived { request } => Some(request),
            Self::RunStarted { .. }
            | Self::WorkStarted
            | Self::AgentStarted { .. }
            | Self::AgentExited { .. }
            | Self::AgentFailed { .. }
            | Self::AgentCollected { .. }
            | Self::OpRefused
            | Self::CallStarted { .. }
            | Self::CallTimedOut { .. }
            | Self::CallReturned { .. }
            | Self::CallFailed { .. }
            | Self::LeaseReady { .. }
            | Self::CommandEnded { .. }
            | Self::TransferEnded { .. }
            | Self::LeaseEnded { .. }
            | Self::LeaseFailed { .. }
            | Self::Tick
            | Self::ModelRequestStarted { .. }
            | Self::ModelAnswered { .. }
            | Self::SandboxGpus { .. }
            | Self::SandboxDeleted { .. }
            | Self::Unknown => None,
        }
    }

    pub(super) fn job(&self) -> Option<u64> {
        match self {
            Self::CallStarted { job }
            | Self::CallTimedOut { job }
            | Self::CallReturned { job, .. }
            | Self::CallFailed { job, .. } => Some(*job),
            Self::RunStarted { .. }
            | Self::WorkStarted
            | Self::AgentStarted { .. }
            | Self::AgentExited { .. }
            | Self::AgentFailed { .. }
            | Self::AgentCollected { .. }
            | Self::OpArrived { .. }
            | Self::OpRefused
            | Self::LeaseReady { .. }
            | Self::CommandEnded { .. }
            | Self::TransferEnded { .. }
            | Self::LeaseEnded { .. }
            | Self::LeaseFailed { .. }
            | Self::Tick
            | Self::ModelRequestStarted { .. }
            | Self::ModelAnswered { .. }
            | Self::SandboxGpus { .. }
            | Self::SandboxDeleted { .. }
            | Self::Unknown => None,
        }
    }
}

/// Whose command a `command_ended` ended: a lease's, or a job function's call's.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(super) enum CommandOf {
    Lease {
        lease: u64,
    },
    Job {
        #[expect(dead_code, reason = "a call's commands show in no line of their own")]
        job: u64,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum Request {
    LeaseOpen {
        instance_seconds: Option<u64>,
    },
    LeaseRun {
        argv: Vec<String>,
    },
    LeasePut {
        tree: String,
        to: String,
        replace: bool,
    },
    LeaseGet {
        from: String,
        to: String,
    },
    LeaseClose,
    #[serde(other)]
    Other,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct Shown {
    pub(super) name: Name,
    pub(super) value: Value,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum Answer {
    Forwarded,
    Cut,
    Barred,
    Refused,
    Abandoned,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct Kept {
    #[serde(default)]
    pub(super) request: KeptRequest,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct KeptRequest {
    // None in a log written before the route named them.
    #[serde(default)]
    pub(super) dropped_headers: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct Failed {
    pub(super) code: Option<String>,
}

#[derive(Debug, Deserialize)]
#[expect(clippy::struct_field_names, reason = "the run log's names for them")]
pub(super) struct Usage {
    pub(super) input_tokens: u64,
    pub(super) output_tokens: u64,
    pub(super) cached_tokens: Option<u64>,
    pub(super) cache_write_tokens: Option<u64>,
    pub(super) reasoning_tokens: Option<u64>,
}

impl From<&Usage> for route::Usage {
    fn from(usage: &Usage) -> Self {
        let Usage {
            input_tokens,
            output_tokens,
            cached_tokens,
            cache_write_tokens,
            reasoning_tokens,
        } = *usage;
        Self {
            input_tokens,
            output_tokens,
            cached_tokens,
            cache_write_tokens,
            reasoning_tokens,
        }
    }
}

/// The payload that an action gives a call.
#[derive(Debug, Deserialize)]
pub(super) struct Payload {
    pub(super) size: u64,
}

#[derive(Debug, Deserialize)]
pub(super) struct AgentSelection {
    pub(super) selection: Option<Payload>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum Action {
    StartAgents,
    Reply,
    StartCall {
        #[serde(deserialize_with = "agent")]
        caller: CallerName,
        job: u64,
        grant: String,
        payload: Payload,
        #[serde(deserialize_with = "nanos")]
        timeout_ns: Duration,
    },
    StartFinalCall {
        job: u64,
        selections: Vec<AgentSelection>,
        #[serde(deserialize_with = "nanos")]
        timeout_ns: Duration,
    },
    StopCall {
        job: u64,
    },
    StartLease {
        lease: u64,
        holder: CallerName,
        grant: String,
    },
    RunCommand {
        lease: u64,
    },
    Transfer {
        lease: u64,
    },
    CloseLease {
        lease: u64,
    },
    TearDownAgent {
        #[serde(deserialize_with = "agent")]
        caller: CallerName,
    },
    Finish,
    Abandon,
    #[serde(other)]
    Unknown,
}

impl Action {
    pub(super) fn final_call(&self) -> Option<u64> {
        match self {
            Self::StartFinalCall { job, .. } => Some(*job),
            Self::StartAgents
            | Self::Reply
            | Self::StartCall { .. }
            | Self::StopCall { .. }
            | Self::StartLease { .. }
            | Self::RunCommand { .. }
            | Self::Transfer { .. }
            | Self::CloseLease { .. }
            | Self::TearDownAgent { .. }
            | Self::Finish
            | Self::Abandon
            | Self::Unknown => None,
        }
    }
}

/// What a transfer does, as the log's `transfer` action names it: `put` copies the holder's tree to
/// the runner, where nothing is, `replace` does so over what is, and `get` copies the runner's tree
/// to the holder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Put,
    Replace,
    Get,
}

/// A caller in the record: its agent's declared role index, or a job's number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CallerName {
    Agent(usize),
    Job(u64),
}

impl From<decision::Caller> for CallerName {
    fn from(caller: decision::Caller) -> Self {
        match caller {
            decision::Caller::Agent(id) => Self::Agent(id.role().index()),
            decision::Caller::Job(job) => Self::Job(job.0),
        }
    }
}

impl From<CallerName> for SandboxName {
    fn from(caller: CallerName) -> Self {
        match caller {
            CallerName::Agent(role) => Self::Agent(role),
            CallerName::Job(job) => Self::Job(job),
        }
    }
}

impl fmt::Display for CallerName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        SandboxName::from(*self).fmt(f)
    }
}

impl Serialize for CallerName {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for CallerName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match SandboxName::deserialize(deserializer)? {
            SandboxName::Agent(role) => Ok(Self::Agent(role)),
            SandboxName::Job(job) => Ok(Self::Job(job)),
            SandboxName::Lease(lease) => Err(D::Error::custom(format_args!(
                "lease-{lease} is no caller: expected `agent-<n>` or `job-<n>`"
            ))),
        }
    }
}

fn agent<'de, D: Deserializer<'de>>(deserializer: D) -> Result<CallerName, D::Error> {
    let caller = CallerName::deserialize(deserializer)?;
    match caller {
        CallerName::Agent(_) => Ok(caller),
        CallerName::Job(_) => Err(D::Error::custom("expected an agent caller")),
    }
}

fn read_part<T: DeserializeOwned>(
    value: Value,
    (line, part): (u64, Part),
    unread: &mut Vec<Unread>,
) -> Option<T> {
    match T::deserialize(value) {
        Ok(read) => Some(read),
        Err(error) => {
            let reason = error.to_string();
            unread.push(Unread { line, part, reason });
            None
        }
    }
}

/// Reads a time written as the log writes every `..._ns`: a string of decimal digits.
pub(super) fn nanos<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
    let text = String::deserialize(deserializer)?;
    let nanos = text.parse().map_err(|_| {
        D::Error::invalid_value(de::Unexpected::Str(&text), &"nanoseconds, a decimal string")
    })?;
    Ok(Duration::from_nanos(nanos))
}

fn optional_nanos<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Duration>, D::Error> {
    #[derive(Deserialize)]
    struct Nanos(#[serde(deserialize_with = "nanos")] Duration);
    let nanos = Option::<Nanos>::deserialize(deserializer)?;
    Ok(nanos.map(|Nanos(nanos)| nanos))
}

fn unix_nanos<'de, D: Deserializer<'de>>(deserializer: D) -> Result<UnixTime, D::Error> {
    nanos(deserializer).map(UnixTime::after_epoch)
}
