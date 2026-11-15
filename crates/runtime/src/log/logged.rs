//! The run log's records as its readers read them, in the forms that the run log writes.
//!
//! Each line is a [`Line`], its event an [`Event`] and its actions [`Action`]s, each with the
//! fields of its kind that a reader of the log reads, and no other, so a field that no reader
//! reads can't keep a line from reading.
//!
//! A line's frame, its event and each of its actions read on their own, each whole or not at
//! all. A kind of event or action that this build doesn't know reads as `Unknown`, and changes
//! nothing that a reader works out. A part of a known kind that lacks a field its kind has, or
//! holds one in another form, doesn't read: its readers leave it out and say so
//! ([`Unread`]), and never read it with defaults. A field that a writer may leave out is an
//! `Option`.

use std::time::Duration;

use provider::UnixTime;
use schema::sandbox::ExitStatus;
use schema::sandbox::op::{CommandEnd, TransferEnd};
use serde::de::{DeserializeOwned, Deserializer, Error as _};
use serde::{Deserialize, Serialize, Serializer, de};
use serde_json::{Number, Value};

use super::{Part, Unread};
use crate::sandbox::SandboxName;

/// A line of the run log, with those of its parts that read.
#[derive(Debug)]
pub(super) struct Line {
    /// When the log wrote it.
    pub(super) at_ns: Duration,
    /// When the runtime observed what it records, if the record says: when the core took its
    /// event, if the core decided on it.
    pub(super) observed_ns: Option<Duration>,
    /// Its event, but for a note, or for an event that doesn't read.
    pub(super) event: Option<Event>,
    /// The actions the core decided on its event, but those that don't read.
    pub(super) actions: Vec<Action>,
    pub(super) note: Option<String>,
}

/// A line's frame, with its event and each of its actions left as JSON, to read on their own.
#[derive(Deserialize)]
struct Frame {
    #[serde(deserialize_with = "nanos")]
    at_ns: Duration,
    #[serde(default, deserialize_with = "optional_nanos")]
    observed_ns: Option<Duration>,
    event: Option<Value>,
    #[serde(default)]
    actions: Vec<Value>,
    note: Option<String>,
}

impl Line {
    /// Reads line `number` of the log, `bytes`, and adds each part of it that doesn't read to
    /// `unread`; `None` if its frame doesn't read.
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
            note,
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
            note,
        })
    }

    /// When it happened, as the log's readers time it: when the runtime observed it, if the record
    /// says, and else when the log wrote it.
    pub(super) fn when(&self) -> Duration {
        self.observed_ns.unwrap_or(self.at_ns)
    }
}

/// An event of the run log, by its `kind`.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum Event {
    RunStarted {
        /// The run's start, by the driving machine's clock.
        #[serde(deserialize_with = "unix_nanos")]
        unix_ns: UnixTime,
        /// The run's hard end, since its start; a start without it reads as no start.
        #[serde(default, deserialize_with = "optional_nanos")]
        hard_end_ns: Option<Duration>,
    },
    AgentStarted,
    AgentExited {
        status: ExitStatus,
    },
    AgentFailed {
        reason: String,
    },
    OpArrived {
        request: Request,
    },
    OpRefused,
    JudgeStarted {
        job: u64,
    },
    JudgeExited {
        job: u64,
    },
    JudgeTimedOut {
        job: u64,
    },
    JudgeCollected {
        job: u64,
        #[serde(flatten)]
        collected: Collected,
    },
    JudgeFailed {
        job: u64,
        reason: String,
    },
    LeaseReady {
        lease: u64,
    },
    CommandEnded {
        lease: u64,
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
    /// A model request of `caller`'s, numbered `request` in the run, going upstream.
    ModelRequestStarted {
        caller: Caller,
        request: u64,
    },
    /// How a route answered a model request of `caller`'s, if the line names it, numbered
    /// `request` in the run, and when its stream's output began, if the line says.
    ModelAnswered {
        caller: Option<Caller>,
        request: u64,
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
        /// Each GPU's model, as `nvidia-smi` names it.
        gpus: Vec<String>,
    },
    /// A sandbox's delete; `sandbox` is `None` for one that no start of the run recorded.
    SandboxDeleted {
        sandbox: Option<SandboxName>,
    },
    #[serde(other)]
    Unknown,
}

impl Event {
    /// The request of an op's arrival.
    pub(super) fn request(&self) -> Option<&Request> {
        match self {
            Self::OpArrived { request } => Some(request),
            Self::RunStarted { .. }
            | Self::AgentStarted
            | Self::AgentExited { .. }
            | Self::AgentFailed { .. }
            | Self::OpRefused
            | Self::JudgeStarted { .. }
            | Self::JudgeExited { .. }
            | Self::JudgeTimedOut { .. }
            | Self::JudgeCollected { .. }
            | Self::JudgeFailed { .. }
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

    /// The job of a judge call's event: its start, exit, timeout, collection or failure.
    pub(super) fn judge_job(&self) -> Option<u64> {
        match self {
            Self::JudgeStarted { job }
            | Self::JudgeExited { job }
            | Self::JudgeTimedOut { job }
            | Self::JudgeCollected { job, .. }
            | Self::JudgeFailed { job, .. } => Some(*job),
            Self::RunStarted { .. }
            | Self::AgentStarted
            | Self::AgentExited { .. }
            | Self::AgentFailed { .. }
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

/// An op's request, by its `kind`, as far as a lease's readers read it: a lease's open, with
/// the life its holder asked for, if it did, and its close.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum Request {
    LeaseOpen {
        instance_seconds: Option<u64>,
    },
    LeaseClose,
    /// Any other op.
    #[serde(other)]
    Other,
}

/// What a judge call's collection found: a result, with its score, the JSON number, if it has
/// one, and what the judge returned; or why what it found is no result.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Collected {
    Result(JudgeResult),
    Invalid(String),
}

/// A judge's result: its score, the JSON number, if it has one, and what the judge returned.
#[derive(Debug, Deserialize)]
pub(super) struct JudgeResult {
    pub(super) score: Option<Number>,
    pub(super) returned: Value,
}

/// How a route answered a model request.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum Answer {
    /// Its upstream's response, forwarded whole.
    Forwarded,
    /// Its upstream's response, cut short.
    Cut,
    Barred,
    Refused,
}

/// Where the record keeps an exchange, of which a reader reads what its request left behind.
#[derive(Debug, Default, Deserialize)]
pub(super) struct Kept {
    #[serde(default)]
    pub(super) request: KeptRequest,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct KeptRequest {
    /// The names of the request's headers that the route left behind; none in a log written
    /// before the route named them.
    #[serde(default)]
    pub(super) dropped_headers: Vec<String>,
}

/// An answer's statement that it failed, with the code it gave, if any.
#[derive(Debug, Deserialize)]
pub(super) struct Failed {
    pub(super) code: Option<String>,
}

/// The tokens a response reported, but those it didn't give.
#[derive(Debug, Deserialize)]
#[expect(clippy::struct_field_names, reason = "the run log's names for them")]
pub(super) struct Usage {
    pub(super) input_tokens: u64,
    pub(super) output_tokens: u64,
    pub(super) cached_tokens: Option<u64>,
    pub(super) reasoning_tokens: Option<u64>,
}

/// An action of the run log, by its `kind`.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum Action {
    StartAgent,
    Reply,
    /// A judge call's start: under the agent's `grant`, or `None` for the score.
    StartJudge {
        job: u64,
        grant: Option<String>,
        #[serde(deserialize_with = "nanos")]
        timeout_ns: Duration,
    },
    StopJudge {
        job: u64,
    },
    /// A lease's open, held by `holder`, or by the agent if the line names none.
    StartLease {
        lease: u64,
        holder: Option<Caller>,
        grant: String,
    },
    RunCommand {
        lease: u64,
        argv: Vec<String>,
    },
    Transfer {
        lease: u64,
        direction: Direction,
        from: String,
        to: String,
    },
    CloseLease {
        lease: u64,
    },
    TearDownAgent,
    Finish,
    Abandon,
    #[serde(other)]
    Unknown,
}

impl Action {
    /// The job of the score's judge call, if this starts it: a judge call's start without a
    /// grant.
    pub(super) fn score_job(&self) -> Option<u64> {
        match self {
            Self::StartJudge {
                job, grant: None, ..
            } => Some(*job),
            Self::StartJudge { grant: Some(_), .. }
            | Self::StartAgent
            | Self::Reply
            | Self::StopJudge { .. }
            | Self::StartLease { .. }
            | Self::RunCommand { .. }
            | Self::Transfer { .. }
            | Self::CloseLease { .. }
            | Self::TearDownAgent
            | Self::Finish
            | Self::Abandon
            | Self::Unknown => None,
        }
    }
}

/// Which way a lease's transfer moves a tree: `put` into the runner, `replace` for a put that
/// replaces its destination, or `get` out of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Put,
    Replace,
    Get,
}

/// An op's caller, or a lease's holder: the agent, or job `n`'s judge call, `agent` or
/// `job-<n>` in the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Caller {
    Agent,
    Job(u64),
}

impl From<decision::Caller> for Caller {
    fn from(caller: decision::Caller) -> Self {
        match caller {
            decision::Caller::Agent => Self::Agent,
            decision::Caller::Job(job) => Self::Job(job.0),
        }
    }
}

impl From<Caller> for SandboxName {
    fn from(caller: Caller) -> Self {
        match caller {
            Caller::Agent => Self::Agent,
            Caller::Job(job) => Self::Job(job),
        }
    }
}

impl Serialize for Caller {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&SandboxName::from(*self))
    }
}

impl<'de> Deserialize<'de> for Caller {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match SandboxName::deserialize(deserializer)? {
            SandboxName::Agent => Ok(Self::Agent),
            SandboxName::Job(job) => Ok(Self::Job(job)),
            SandboxName::Lease(lease) => Err(D::Error::custom(format_args!(
                "lease-{lease} is no caller: expected `agent` or `job-<n>`"
            ))),
        }
    }
}

/// `value`, part `part` of line `line`, as a `T`; `None` if it doesn't read, which is added to
/// `unread`.
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

/// A time or duration in nanoseconds, as a decimal string.
fn nanos<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
    let text = String::deserialize(deserializer)?;
    let nanos = text.parse().map_err(|_| {
        D::Error::invalid_value(de::Unexpected::Str(&text), &"nanoseconds, a decimal string")
    })?;
    Ok(Duration::from_nanos(nanos))
}

/// A time or duration in nanoseconds, as [`nanos`] reads one, or `null`.
fn optional_nanos<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Duration>, D::Error> {
    #[derive(Deserialize)]
    struct Nanos(#[serde(deserialize_with = "nanos")] Duration);
    let nanos = Option::<Nanos>::deserialize(deserializer)?;
    Ok(nanos.map(|Nanos(nanos)| nanos))
}

/// A time by the machine's clock, in nanoseconds since the Unix epoch, as [`nanos`] reads one.
fn unix_nanos<'de, D: Deserializer<'de>>(deserializer: D) -> Result<UnixTime, D::Error> {
    nanos(deserializer).map(UnixTime::after_epoch)
}
