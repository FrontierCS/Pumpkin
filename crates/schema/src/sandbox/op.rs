//! Ops: the calls a caller in a sandbox makes to the session's owner, and the owner's replies.
//!
//! A caller makes an op with `sandboxctl`, which prints the reply's JSON form on one line, which
//! this module is the reference for; `sandboxctl --help` gives the commands' arguments.
//!
//! | Command | Call | Reply |
//! | --- | --- | --- |
//! | `sandboxctl ops` | [`OpCall::Ops`] | [`OpsReply`] |
//! | `sandboxctl call <function> <file>` | [`OpCall::Call`] | [`CallReply`] |
//! | `sandboxctl status <job>` | [`OpCall::Status`] | [`StatusReply`] |
//! | `sandboxctl call <session>.open [--seconds <n>]` | [`SessionOp::Open`] | [`LeaseReply`] |
//! | `sandboxctl call <session>.run <lease> [--timeout <s>] -- <argv>…` | [`SessionOp::Run`] | [`LeaseReply`] |
//! | `sandboxctl call <session>.put <lease> <tree> <to> [--replace]` | [`SessionOp::Put`] | [`LeaseReply`] |
//! | `sandboxctl call <session>.get <lease> <from> <to>` | [`SessionOp::Get`] | [`LeaseReply`] |
//! | `sandboxctl call <session>.status <lease>` | [`SessionOp::Status`] | [`LeaseStatusReply`] |
//! | `sandboxctl call <session>.close <lease>` | [`SessionOp::Close`] | [`LeaseReply`] |
//!
//! A payload is read from its file when its caller's turn comes, so leave the file be until the
//! call returns.
//!
//! A reply is an object whose `kind` is its [`OpReply`]'s variant, `refused` answering any op. Its
//! fields are its type's, under the same names; a limit or a quota that doesn't exist is `null`. A
//! status has `returned` once its job has given a result, and `failure` only if it gave none; once
//! its job has ended, it has `model_requests_over_budget` only if that is 1 or more.

use std::collections::BTreeMap;
use std::num::{NonZeroU32, NonZeroU64};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{ExitStatus, frame};
use crate::ir::{AbsolutePath, Argument, Argv, Name, Resources};

/// The most bytes of why a transfer failed, and of why a runner failed, that a lease's status
/// gives ([`LeaseStatusReply`]).
pub const REASON_BYTES: usize = 3 << 10;

/// The most bytes that the end of a command's standard output, and of its standard error, take in
/// a lease's status as JSON writes them, escapes included ([`output_tail`]).
// A lease's status holds a runner's failure, and a command's two output tails or a transfer's
// failure. JSON writes each byte of a reason in at most 6, as `\u0000`, and the rest of the frame's
// message is under 4 KiB, so the tails take what the frame leaves: 64 KiB - 4 KiB - 6 * 3 KiB =
// 2 * 21 KiB. A transfer's failure takes no more than the two tails would.
pub const OUTPUT_TAIL_BYTES: usize = 21 << 10;

const _: () = assert!(6 * REASON_BYTES <= 2 * OUTPUT_TAIL_BYTES);
const _: () =
    assert!((4 << 10) + 6 * REASON_BYTES + 2 * OUTPUT_TAIL_BYTES <= frame::MAX_MESSAGE_BYTES);

/// The longest end of `output`, a command's, whose JSON string takes at most
/// [`OUTPUT_TAIL_BYTES`], as UTF-8 with each invalid sequence replaced.
pub fn output_tail(output: &[u8]) -> String {
    // Each byte takes at least one in JSON, so no more than this many can fit.
    let output = &output[output.len().saturating_sub(OUTPUT_TAIL_BYTES)..];
    let text = String::from_utf8_lossy(output);
    let mut taken = 0;
    let mut start = text.len();
    for (index, char) in text.char_indices().rev() {
        taken += match char {
            '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
            '\0'..='\u{1f}' => 6,
            other => other.len_utf8(),
        };
        if taken > OUTPUT_TAIL_BYTES {
            break;
        }
        start = index;
    }
    text[start..].to_owned()
}

/// What an op asks of the session's owner, which decides whether the sandbox may ask it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OpCall {
    /// Asks for the caller's limits: the time left to work, its resources and the grants it holds.
    Ops,
    /// Calls the function of the grant named `grant` that the caller's role holds, with the op's
    /// payload, and `arguments`, the texts of its caller's arguments by name.
    Call {
        grant: Name,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        arguments: BTreeMap<Name, String>,
    },
    /// Asks how job `job`, which an earlier call of a job function started, is doing.
    Status { job: u64 },
    /// Calls `op`, one of the functions of the session named `grant` that the caller's role holds.
    Session { grant: Name, op: SessionOp },
}

/// One of a session's functions, over the session's leases, each of which is named by the number
/// that the session's `open` returned.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionOp {
    /// Opens a lease: a runner, which starts in the background, and lasts `instance_seconds` from
    /// when it is ready, if given, which is at most the session's own; without one, the session's.
    Open {
        instance_seconds: Option<NonZeroU64>,
    },
    /// Runs `argv` in lease `lease`'s runner, once it is ready and runs no other command, killing
    /// it once it has run for `timeout_seconds`, if given, which is at most what the lease has
    /// left; without one, it may run until the lease ends.
    Run {
        lease: u64,
        argv: Argv,
        timeout_seconds: Option<NonZeroU64>,
    },
    /// Copies the tree in the caller's directory `tree` to `to` in lease `lease`'s runner, once it
    /// is ready and runs nothing else: the tree's root becomes `to`, which must not exist, or, if
    /// `replace`, which is removed first, whatever it is.
    Put {
        lease: u64,
        tree: AbsolutePath,
        to: AbsolutePath,
        replace: bool,
    },
    /// Copies the tree in lease `lease`'s runner's directory `from` to `to` in the caller's
    /// sandbox, as [`SessionOp::Put`] copies one the other way.
    Get {
        lease: u64,
        from: AbsolutePath,
        to: AbsolutePath,
    },
    /// Asks how lease `lease` is doing.
    Status { lease: u64 },
    /// Closes lease `lease`, ending its runner with whatever runs in it.
    Close { lease: u64 },
}

impl OpCall {
    /// Whether the call takes a payload ([`FromSandbox::Op`](super::FromSandbox::Op)): a function's
    /// call does, and no other call.
    pub fn takes_payload(&self) -> bool {
        match self {
            Self::Call { .. } => true,
            Self::Ops | Self::Status { .. } | Self::Session { .. } => false,
        }
    }
}

/// The owner's answer to an op: the reply for the op's kind, or a refusal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OpReply {
    Ops(OpsReply),
    Call(CallReply),
    Status(StatusReply),
    Lease(LeaseReply),
    LeaseStatus(LeaseStatusReply),
    /// The owner refused the op, and did nothing, for the reason given.
    Refused {
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// The reply to [`OpCall::Ops`]: the time left to work, the caller's resources and the grants it
/// holds.
pub struct OpsReply {
    /// The time left to work, in whole seconds rounded down: from when the op reached its owner, or
    /// later, until its caller's work ends.
    pub remaining_seconds: u64,
    /// The caller's CPU and memory limits: its role's.
    pub resources: Resources,
    pub grants: Vec<Grant>,
}

/// The reply to [`OpCall::Call`], by the kind of the function called.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CallReply {
    /// A job function's: the job that runs the call, which `status` follows, and what the
    /// function has left of its quotas.
    Job(JobReply),
    /// A now function's.
    Now(NowReply),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// The reply to a now function's call: what its body returned, once its effects are applied.
pub struct NowReply {
    pub returned: Returned,
    /// The time left to work, in whole seconds rounded down: from when the op reached its owner, or
    /// later, until the deadline, or, once the call has ended the work, until the end of the grace
    /// to wind down, if that comes first.
    pub remaining_seconds: u64,
}

/// A grant that a caller holds, and its limits. Its `kind` says which call takes it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Grant {
    Job(JobFunction),
    Session(SessionGrant),
    Now(NowFunction),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// A job function, which [`OpCall::Call`] calls; see [`ir::JobFunction`](crate::ir::JobFunction)
/// for its limits.
pub struct JobFunction {
    pub name: Name,
    pub payload_max_bytes: NonZeroU64,
    pub calls: Option<NonZeroU64>,
    pub cumulative_seconds: Option<NonZeroU64>,
    pub call_timeout_seconds: NonZeroU64,
    /// The parameters that the caller gives, by name, each as `--arg <name>=<value>`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub arguments: BTreeMap<Name, Argument>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// A session, whose functions [`OpCall::Session`] calls; see [`ir::Session`](crate::ir::Session)
/// for its limits.
pub struct SessionGrant {
    pub name: Name,
    pub instances: NonZeroU32,
    pub instance_seconds: NonZeroU64,
    pub cumulative_seconds: Option<NonZeroU64>,
    pub commands: Option<NonZeroU64>,
    pub transfer_bytes: NonZeroU64,
    /// What it has left, as a lease opened now would have its own time whole.
    pub remaining: LeaseRemaining,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// A now function that [`OpCall::Call`] calls; see [`ir::NowFunction`](crate::ir::NowFunction).
pub struct NowFunction {
    pub name: Name,
    /// The most bytes a payload may hold.
    pub payload_max_bytes: NonZeroU64,
    /// The parameters that the caller gives, by name, each as `--arg <name>=<value>`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub arguments: BTreeMap<Name, Argument>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// A job function's reply to [`OpCall::Call`]: the job that runs the call, queued, and what the
/// function has left now that the call counts against it.
pub struct JobReply {
    pub job: u64,
    pub remaining: Remaining,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// What a job function has left of its quotas.
pub struct Remaining {
    /// The calls left, or `None` if the function doesn't limit them.
    pub calls: Option<u64>,
    /// The call time left, in whole seconds rounded down, or `None` if the function doesn't limit
    /// it.
    pub seconds: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "StatusJson", into = "StatusJson")]
/// The reply to [`OpCall::Status`]: how a job is doing, and what its function has left.
pub struct StatusReply {
    pub job: u64,
    pub state: JobState,
    pub remaining: Remaining,
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// Where a job is, with what came of it once it has ended. Its JSON form is the status's `state`,
/// with the fields of its variant beside it.
///
/// An ended job's `model_requests_over_budget` is how many of its target's model requests the
/// route had barred, by the time the call ended, for a spent
/// [`ModelBudget`](crate::ir::ModelBudget): a request barred after the end is not counted.
pub enum JobState {
    /// Accepted; its body hasn't started.
    Queued,
    /// Its body is running, or has ended and its target is being cleaned up.
    Running,
    /// The body returned.
    Completed {
        returned: Returned,
        model_requests_over_budget: Option<NonZeroU64>,
    },
    /// The body didn't return.
    Errored {
        failure: Failure,
        model_requests_over_budget: Option<NonZeroU64>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
/// What a body returned, by name, each with its value.
pub struct Returned(Map<String, Value>);

impl FromIterator<(String, Value)> for Returned {
    fn from_iter<I: IntoIterator<Item = (String, Value)>>(fields: I) -> Self {
        Self(fields.into_iter().collect())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Why a job gave no result.
pub struct Failure {
    pub code: FailureCode,
    /// What happened, in a sentence.
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCode {
    /// The call's body ran past its timeout.
    CallTimeout,
    /// The call's body failed before it returned.
    CallError,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// The reply to [`SessionOp::Open`], [`SessionOp::Run`], [`SessionOp::Put`], [`SessionOp::Get`]
/// and [`SessionOp::Close`]: the lease, and what it and its session have left now that the op
/// counts against them.
pub struct LeaseReply {
    pub lease: u64,
    pub remaining: LeaseRemaining,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// What a lease and its session have left of their limits, each time in whole seconds rounded down,
/// and `None` for a limit the session doesn't set.
pub struct LeaseRemaining {
    /// How many more of the session's leases may be open now.
    pub instances: u32,
    /// The lease's own time left: all of it until its runner is ready, none once it has ended.
    pub instance_seconds: u64,
    /// The session's time left, which each of its leases open at once spends.
    pub cumulative_seconds: Option<u64>,
    /// The session's commands left, a transfer counting as one.
    pub commands: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// The reply to [`SessionOp::Status`]: how a lease is doing, its last command or transfer, and
/// what it and its session have left.
///
/// It carries the ends of its commands' output, so it goes to the lease's holder alone.
pub struct LeaseStatusReply {
    pub lease: u64,
    pub state: LeaseState,
    /// The time since it was opened, in whole seconds rounded down, its runner's start included.
    pub elapsed_seconds: u64,
    /// Of the time it has been charged for, from its runner's ready until its close, how long a
    /// command or transfer of its ran, in whole seconds rounded down.
    pub busy_seconds: u64,
    /// The rest of the time it has been charged for, likewise.
    pub idle_seconds: u64,
    /// Why its runner failed, once it has, as the run and its provider said it, of at most
    /// [`REASON_BYTES`].
    pub failure: Option<String>,
    /// The last command or transfer it ran, once it has run one.
    pub last: Option<Last>,
    pub remaining: LeaseRemaining,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
/// What a lease ran last: a command or a transfer, whose `kind` says which.
pub enum Last {
    Command(CommandStatus),
    Transfer(TransferStatus),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// How a lease is doing.
pub enum LeaseState {
    /// Its runner is starting.
    Starting,
    /// Its runner is ready, and runs none of its commands.
    Ready,
    /// Its runner runs a command, or collects what the command wrote, or a tree moves to or from
    /// it.
    Running,
    /// It was closed, or its time ran out, and its runner is being removed, until which it holds
    /// its instance of its session's `instances`; then it is `closed` or `expired`.
    Closing,
    /// It was closed, as its holder asked or as its holder's work ended.
    Closed,
    /// Its time, or its session's, ran out.
    Expired,
    /// Its runner failed to start, as when its provider had no room, or failed as it ran: no fault
    /// of its holder's, which may open another.
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// A command that a lease ran, and how it ended.
pub struct CommandStatus {
    /// Its number among the lease's commands and transfers, from 0.
    pub command: u64,
    /// How it ended; `None` while it runs, and for one that ran when its lease ended.
    pub end: Option<CommandEnd>,
    /// The end of what it wrote to its standard output, as [`output_tail`] gives it; empty until it
    /// has ended.
    pub stdout: String,
    /// The same of its standard error.
    pub stderr: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// A transfer of a tree to or from a lease's runner, and how it ended.
pub struct TransferStatus {
    /// Its number among the lease's commands and transfers, from 0.
    pub transfer: u64,
    /// How it ended; `None` while it runs, and for one that ran when its lease ended.
    pub end: Option<TransferEnd>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Whether a transfer moved its whole tree, or why it didn't.
pub enum TransferEnd {
    /// The whole tree arrived at its destination: this many entries, directories included, and
    /// bytes of files.
    Moved { entries: u64, bytes: u64 },
    /// It failed, or was cut short by its lease's end, for this reason, of at most [`REASON_BYTES`]
    /// in a status: what of the tree arrived before then stays at its destination.
    Failed(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// How a command ran to its end: it exited, ran out of time, or never started.
pub enum CommandEnd {
    /// It exited, or a signal killed it.
    Exited(ExitStatus),
    /// It ran past its timeout, and was killed.
    TimedOut,
    /// It didn't start, for the reason its standard error's end gives.
    Unstarted,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusJson {
    job: u64,
    state: StateName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    returned: Option<Returned>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    failure: Option<Failure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model_requests_over_budget: Option<NonZeroU64>,
    remaining: Remaining,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StateName {
    Queued,
    Running,
    Completed,
    Errored,
}

impl TryFrom<StatusJson> for StatusReply {
    type Error = &'static str;

    fn try_from(json: StatusJson) -> Result<Self, Self::Error> {
        let StatusJson {
            job,
            state,
            returned,
            failure,
            model_requests_over_budget,
            remaining,
        } = json;
        let state = match (state, returned, failure, model_requests_over_budget) {
            (StateName::Queued, None, None, None) => JobState::Queued,
            (StateName::Running, None, None, None) => JobState::Running,
            (StateName::Completed, Some(returned), None, model_requests_over_budget) => {
                JobState::Completed {
                    returned,
                    model_requests_over_budget,
                }
            }
            (StateName::Errored, None, Some(failure), model_requests_over_budget) => {
                JobState::Errored {
                    failure,
                    model_requests_over_budget,
                }
            }
            (StateName::Queued | StateName::Running, None, None, Some(_)) => {
                return Err(
                    "a job that hasn't ended has no count of model requests over \
                            budget yet: give it no `model_requests_over_budget`",
                );
            }
            (StateName::Queued | StateName::Running, _, _, _) => {
                return Err("a job that hasn't ended has no result: give it neither \
                            `returned` nor `failure`");
            }
            (StateName::Completed, _, _, _) => {
                return Err("a completed job has a result: give it `returned`, and no `failure`");
            }
            (StateName::Errored, _, _, _) => {
                return Err("an errored job has no result: give it `failure`, and no `returned`");
            }
        };
        Ok(Self {
            job,
            state,
            remaining,
        })
    }
}

impl From<StatusReply> for StatusJson {
    fn from(reply: StatusReply) -> Self {
        let StatusReply {
            job,
            state,
            remaining,
        } = reply;
        let (state, returned, failure, model_requests_over_budget) = match state {
            JobState::Queued => (StateName::Queued, None, None, None),
            JobState::Running => (StateName::Running, None, None, None),
            JobState::Completed {
                returned,
                model_requests_over_budget,
            } => (
                StateName::Completed,
                Some(returned),
                None,
                model_requests_over_budget,
            ),
            JobState::Errored {
                failure,
                model_requests_over_budget,
            } => (
                StateName::Errored,
                None,
                Some(failure),
                model_requests_over_budget,
            ),
        };
        Self {
            job,
            state,
            returned,
            failure,
            model_requests_over_budget,
            remaining,
        }
    }
}
