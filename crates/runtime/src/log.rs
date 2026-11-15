//! The run log, `log.jsonl`, and the JSON forms of events, actions and outcomes.
//!
//! `log.jsonl` holds one JSON record per line, each timed and synced as [`Log`] writes it.
//! [`Progress`] reads it back, timing each record by its `o`, or else its `t`. A record is one of:
//!
//! - an event of the core's: `{"seq": n, "at_ns": "t", "observed_ns": "o", "event": e,
//!   "actions": [a, ...]}`, with the actions the core decided on it, which it took at `o`, when
//!   the runtime observed it: what the agent's session handed over as it reached the host, a
//!   judge call's events as its thread saw them, so `o` may be before an earlier record's `t`.
//!   The first, `seq` 0, is the run's start, `{"kind": "run_started", "unix_ns": "u",
//!   "hard_end_ns": "h"}`, observed at 0, `u` nanoseconds after the Unix epoch by the driving
//!   machine's clock, whose hard end is `h` after it;
//! - one that no core decides on: an `op_refused` ([`Log::refused`]) or an [`Entry`], `{"seq": n,
//!   "at_ns": "t", "event": e, "actions": []}`, with `"observed_ns": "o"` as above if the thread
//!   that observed it timed it; or a note, `{"seq": n, "at_ns": "t", "note": s}`.
//!
//! `seq` counts records from 0. Each event and action is an object whose `kind` is its name in
//! snake case, such as `{"kind": "judge_exited", "job": 1, "status": {"code": 0}}`. Grants appear
//! by their names in the workflow, jobs, ops and leases by their numbers, payloads by size and
//! digest, and times and durations in nanoseconds as decimal strings, where `u64::MAX` stands for
//! anything longer; a status's job, a number the agent gave, is a decimal string too. A `caller`
//! or `holder` is its sandbox's name in the run, `agent`, or `job-<n>` for job `n`'s call.
//! `start_judge` names the agent's grant of the call, or `null` for the score, `start_lease` the
//! lease's grant; a `transfer` gives its paths, and its `transfer_ended` its sizes, never what it
//! moved. A reply is the [`OpReply`](schema::sandbox::op::OpReply) sent.

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use decision::{
    Action, Caller, Event, Failure, JudgeResult, Outcome, PayloadRef, Request, Submission, Verdict,
    WorkEnd,
};
use gateway::RefusedOp;
use provider::UnixTime;
use route::{Answer, Usage};
use schema::ir::{Api, Gpu, GpuModel, Name};
use schema::sandbox::ExitStatus;
use schema::sandbox::op::{CommandEnd, TransferEnd};
use schema::workflow::{JudgeCall, Workflow};
use serde::Deserialize;
use serde_json::{Value, json};
use stdx::Context;

use super::sandbox::{Deleted, SandboxName};
use super::{Clock, Durable, Host, record};

mod logged;

pub use logged::Direction;
use logged::{Collected, Line};

/// The run log, open for appending: each record's `at_ns` is when the log wrote it, by the run's
/// clock, so no record's `at_ns` is before the one's before it; synced on the run's host, `H`.
///
/// A record that leads to an act, an event with actions or an `op_refused`, is synced with all
/// before it ahead of the act; any other is only written, to last with the next sync. An act that
/// no record leads to is recorded after it: a model exchange, a session's end that the gateway
/// makes, and what the run's end does after a failure of its own. The run's end syncs the log
/// before `outcome.json`, and a failed sync fails the run: so a crash cuts a lost run's log only
/// after its last synced record, and a finished run's only if a sync failed.
pub(super) struct Log<'h, H: Host> {
    path: PathBuf,
    file: File,
    seq: u64,
    host: &'h H,
    clock: H::Clock,
}

impl<'h, H: Host> Log<'h, H> {
    /// Creates the log, `log.jsonl`, in the record's directory `dir`, where it must not exist,
    /// and syncs `dir` on `host`, so that the log lasts as each record synced into it does; each
    /// record is timed by `clock`, the run's.
    pub(super) fn create(host: &'h H, clock: H::Clock, dir: &Path) -> Result<Self, String> {
        let path = dir.join("log.jsonl");
        let file = OpenOptions::new()
            .append(true)
            .create_new(true)
            .open(&path)
            .context(path.display())?;
        record::sync_dir(host, dir)?;
        Ok(Self {
            path,
            file,
            seq: 0,
            host,
            clock,
        })
    }

    /// Appends the record of the run's start, observed at 0, `unix` after the Unix epoch, whose
    /// hard end is `hard_end` after it, with the core's first `actions`.
    pub(super) fn start(
        &mut self,
        workflow: &Workflow,
        unix: UnixTime,
        hard_end: Duration,
        actions: &[Action],
    ) -> Result<(), String> {
        let event = json!({
            "kind": "run_started",
            "unix_ns": nanos(unix.since_epoch()),
            "hard_end_ns": nanos(hard_end),
        });
        self.decided(workflow, Duration::ZERO, event, actions)
    }

    /// Appends the record of `event`, observed at `observed`, the time the core took it at, and
    /// the `actions` it called for.
    pub(super) fn event(
        &mut self,
        workflow: &Workflow,
        observed: Duration,
        event: &Event,
        actions: &[Action],
    ) -> Result<(), String> {
        self.decided(workflow, observed, event_json(event), actions)
    }

    /// Appends the record of `refused`, an op of the agent's whose payload the gateway refused:
    /// `{"kind": "op_refused", "request": r, "message": m}`, where the request's payload is
    /// `{"size": s, "over": n}`, its size and the limit it was over, in bytes, without a digest,
    /// as the gateway kept none of its bytes, and the message is the refusal the agent is sent,
    /// unless a note after it says it wasn't.
    pub(super) fn refused(&mut self, refused: &RefusedOp) -> Result<(), String> {
        let mut request = json!(refused.call());
        let (size, over) = (refused.size(), refused.payload_limit().bytes());
        request["payload"] = json!({ "size": size, "over": over });
        let event = json!({
            "kind": "op_refused",
            "request": request,
            "message": refused.message(),
        });
        self.told(event)?;
        // The refusal is sent next, an act that no action lists.
        self.sync()
    }

    /// Appends how `caller`'s route answered its request `request`, observed at its end:
    /// `{"kind": "model_answered", "caller": c, "request": n, "upstream_status": u, "answer": a,
    /// "kept": k}`; `"upstream"` and `"api"` as [`ModelAnswer`] says; `"request_id"` if the
    /// upstream named one; `"usage"` if reported ([`Usage`]); `"failed": {"code": c}` if it
    /// stated a failure, without `"code"` if it gave none ([`route::Failure`]); and if a response
    /// came, `"first_byte_ns"` and `"first_token_ns"` as [`ModelAnswer`] says, where it has them.
    /// `u` is the upstream's status, or `null` if no response came, `a` the [`Answer`]'s kind, with
    /// a refusal's or a bar's status and message, but never a body, and `k` where the record keeps
    /// the exchange ([`Exchange::kept`](super::Exchange::kept)).
    #[expect(
        clippy::too_many_lines,
        reason = "one place for each field of the record"
    )]
    fn model_answered(
        &mut self,
        (caller, request): (Caller, u64),
        answered: ModelAnswer,
    ) -> Result<(), String> {
        let ModelAnswer {
            taken,
            answer,
            kept,
            first,
            ended,
        } = answered;
        let answer = &answer;
        let (upstream_status, request_id, (usage, failed), answer) = match answer {
            Answer::Forwarded {
                status,
                request_id,
                usage,
                failed,
            } => (
                Some(*status),
                request_id,
                (usage, failed),
                json!({ "kind": "forwarded" }),
            ),
            Answer::Cut {
                status,
                request_id,
                usage,
                failed,
            } => (
                Some(*status),
                request_id,
                (usage, failed),
                json!({ "kind": "cut" }),
            ),
            Answer::Barred { status, message } => {
                let barred = json!({ "kind": "barred", "status": status, "message": message });
                (None, &None, (&None, &None), barred)
            }
            Answer::Refused {
                upstream_status,
                request_id,
                status,
                message,
            } => {
                let refused = json!({ "kind": "refused", "status": status, "message": message });
                (*upstream_status, request_id, (&None, &None), refused)
            }
        };
        let mut event = json!({
            "kind": "model_answered",
            "caller": logged::Caller::from(caller),
            "request": request,
            "upstream_status": upstream_status,
            "answer": answer,
        });
        if let Some((upstream, api)) = taken {
            event["upstream"] = json!(upstream);
            event["api"] = json!(api);
        }
        // Where no response came, the bytes kept are the route's refusal's.
        if upstream_status.is_some()
            && let Some((first_byte, first_token)) = first
        {
            event["first_byte_ns"] = json!(nanos(first_byte));
            if let Some(first_token) = first_token {
                event["first_token_ns"] = json!(nanos(first_token));
            }
        }
        if let Some(request_id) = request_id {
            event["request_id"] = json!(request_id);
        }
        if let Some(usage) = usage {
            event["usage"] = usage_json(usage);
        }
        if let Some(failed) = failed {
            event["failed"] = match failed.code() {
                Some(code) => json!({ "code": code }),
                None => json!({}),
            };
        }
        event["kept"] = kept;
        self.observed(ended, event, &[])
    }

    pub(super) fn entry(&mut self, entry: Entry) -> Result<(), String> {
        match entry {
            Entry::Note(note) => self.note(&note),
            Entry::ModelRequestStarted((caller, request), started) => {
                let event = json!({
                    "kind": "model_request_started",
                    "caller": logged::Caller::from(caller),
                    "request": request,
                });
                self.observed(started, event, &[])
            }
            Entry::ModelAnswered(request, answered) => self.model_answered(request, answered),
            Entry::Gpus(sandbox, gpus) => {
                self.told(json!({ "kind": "sandbox_gpus", "sandbox": sandbox, "gpus": gpus }))
            }
            Entry::Deleted(deleted) => {
                let Deleted {
                    sandbox,
                    provider,
                    id,
                } = deleted;
                self.told(json!({
                    "kind": "sandbox_deleted",
                    "sandbox": sandbox,
                    "provider": provider,
                    "id": id.as_str(),
                }))
            }
        }
    }

    /// Appends the record of `event`, an event of the core's observed at `observed`, and the
    /// `actions` the core decided on, synced if it has any.
    fn decided(
        &mut self,
        workflow: &Workflow,
        observed: Duration,
        event: Value,
        actions: &[Action],
    ) -> Result<(), String> {
        let actions: Vec<Value> = actions
            .iter()
            .map(|action| action_json(workflow, action))
            .collect();
        let leads = !actions.is_empty();
        self.observed(observed, event, &actions)?;
        if leads { self.sync() } else { Ok(()) }
    }

    /// Appends the record of `event`, observed at `observed`, with `actions`.
    fn observed(
        &mut self,
        observed: Duration,
        event: Value,
        actions: &[Value],
    ) -> Result<(), String> {
        let mut record = json!({ "observed_ns": nanos(observed), "actions": actions });
        record["event"] = event;
        self.append(record)
    }

    /// Appends the record of `event`, which no core decides on, and so leads to no action.
    fn told(&mut self, event: Value) -> Result<(), String> {
        let mut record = json!({ "actions": [] });
        record["event"] = event;
        self.append(record)
    }

    /// Appends a note: `{"note": s}`.
    pub(super) fn note(&mut self, note: &str) -> Result<(), String> {
        self.append(json!({ "note": note }))
    }

    /// Appends `record`, numbered and timed as it is written.
    fn append(&mut self, mut record: Value) -> Result<(), String> {
        record["seq"] = self.seq.into();
        record["at_ns"] = nanos(self.clock.now()).into();
        let mut line = record.to_string();
        line.push('\n');
        self.file
            .write_all(line.as_bytes())
            .context(self.path.display())?;
        self.seq += 1;
        Ok(())
    }

    pub(super) fn sync(&self) -> Result<(), String> {
        (self.host)
            .sync(&self.file, Durable::Data)
            .context(self.path.display())
    }
}

/// A record of the run log that no core decides on, which the thread that observed it hands the
/// loop, and the loop writes as it takes it, at that time ([`Log`]).
pub(super) enum Entry {
    /// A note ([`Log::note`]).
    Note(String),
    /// A model request of the caller's, by its number, going upstream at a time:
    /// `{"kind": "model_request_started", "caller": c, "request": n}`, observed then. A request
    /// that the route refuses itself never goes upstream, and has none.
    ModelRequestStarted((Caller, u64), Duration),
    /// How a route answered a model request of the caller's, by its number
    /// ([`Log::model_answered`]).
    ModelAnswered((Caller, u64), ModelAnswer),
    /// The GPUs that a sandbox's `sandboxd` saw as it started, as its session says them:
    /// `{"kind": "sandbox_gpus", "sandbox": n, "gpus": [m, ...]}`, where `n` is its name within the
    /// run, and each `m` a GPU's model, as `nvidia-smi` names it. A sandbox whose `sandboxd` saw
    /// none, or couldn't tell, has none.
    Gpus(Name, Vec<GpuModel>),
    /// A sandbox's delete, which ended it: `{"kind": "sandbox_deleted", "sandbox": n, "provider":
    /// p, "id": i}`, where `n` is its name within the run, `agent`, `job-<n>` or `lease-<n>`, or
    /// `null` for one that no start of the run recorded, which the run's end found left. Every
    /// sandbox's end is one, on every path, a stop short of the core's end included, after which
    /// the log keeps the notes and deletes of the run's children, but no event of theirs.
    Deleted(Deleted),
}

/// How a route answered a model request, as its thread observed it: through the upstream and
/// API of the entry that took it, if one did, where the record keeps it, and when the answer's
/// first bytes came, its stream's output began ([`route::Keep::first_output`]), and it ended,
/// each by the run's clock.
pub(super) struct ModelAnswer {
    /// The upstream's name and the API of the entry that took the request, if one did, even if
    /// the route barred the request itself, and never sent it upstream.
    pub(super) taken: Option<(Name, Api)>,
    pub(super) answer: Answer,
    pub(super) kept: Value,
    pub(super) first: Option<(Duration, Option<Duration>)>,
    pub(super) ended: Duration,
}

/// `usage` in the run log's JSON form: its counts, without those the provider didn't give.
fn usage_json(usage: &Usage) -> Value {
    let Usage {
        input_tokens,
        output_tokens,
        cached_tokens,
        reasoning_tokens,
    } = usage;
    let mut json = json!({ "input_tokens": input_tokens, "output_tokens": output_tokens });
    if let Some(cached) = cached_tokens {
        json["cached_tokens"] = json!(cached);
    }
    if let Some(reasoning) = reasoning_tokens {
        json["reasoning_tokens"] = json!(reasoning);
    }
    json
}

/// `duration` in nanoseconds, as the log gives times and durations.
pub(super) fn nanos(duration: Duration) -> String {
    // A decimal string, since a reader that reads JSON numbers as doubles rounds integers past
    // 2^53, and a time since the Unix epoch in nanoseconds is past 2^53.
    u64::try_from(duration.as_nanos())
        .unwrap_or(u64::MAX)
        .to_string()
}

/// The JSON form of an event.
fn event_json(event: &Event) -> Value {
    match event {
        Event::AgentStarted => json!({ "kind": "agent_started" }),
        Event::AgentExited { status } => json!({ "kind": "agent_exited", "status": status }),
        Event::AgentFailed { reason } => json!({ "kind": "agent_failed", "reason": reason }),
        Event::OpArrived {
            op,
            caller,
            request,
        } => json!({
            "kind": "op_arrived",
            "op": op.0,
            "caller": logged::Caller::from(*caller),
            "request": request_json(request),
        }),
        Event::JudgeStarted { job } => json!({ "kind": "judge_started", "job": job.0 }),
        Event::JudgeExited { job, status } => {
            json!({ "kind": "judge_exited", "job": job.0, "status": status })
        }
        Event::JudgeTimedOut { job } => json!({ "kind": "judge_timed_out", "job": job.0 }),
        Event::JudgeCollected { job, result } => match result {
            Ok(result) => json!({
                "kind": "judge_collected",
                "job": job.0,
                "result": result_json(result),
            }),
            Err(reason) => json!({ "kind": "judge_collected", "job": job.0, "invalid": reason }),
        },
        Event::JudgeFailed { job, reason } => {
            json!({ "kind": "judge_failed", "job": job.0, "reason": reason })
        }
        Event::LeaseReady { lease } => json!({ "kind": "lease_ready", "lease": lease.0 }),
        Event::CommandEnded {
            lease,
            command,
            end,
            stdout,
            stderr,
        } => json!({
            "kind": "command_ended",
            "lease": lease.0,
            "command": command,
            "end": end,
            "stdout": stdout,
            "stderr": stderr,
        }),
        Event::TransferEnded {
            lease,
            transfer,
            end,
        } => json!({
            "kind": "transfer_ended",
            "lease": lease.0,
            "transfer": transfer,
            "end": end,
        }),
        Event::LeaseEnded { lease } => json!({ "kind": "lease_ended", "lease": lease.0 }),
        Event::LeaseFailed { lease, reason } => {
            json!({ "kind": "lease_failed", "lease": lease.0, "reason": reason })
        }
        Event::Tick => json!({ "kind": "tick" }),
    }
}

fn request_json(request: &Request) -> Value {
    match request {
        Request::Ops => json!({ "kind": "ops" }),
        Request::Judge { grant, payload } => json!({
            "kind": "judge",
            "grant": grant.as_str(),
            "payload": payload_json(payload),
        }),
        Request::Submit {
            grant,
            finalize,
            payload,
        } => json!({
            "kind": "submit",
            "grant": grant.as_str(),
            "finalize": finalize,
            "payload": payload_json(payload),
        }),
        // A decimal string, as a time is, since the agent may give any number, past 2^53 too.
        Request::Status { job } => json!({ "kind": "status", "job": job.to_string() }),
        Request::LeaseOpen {
            grant,
            instance_seconds,
        } => json!({
            "kind": "lease_open",
            "grant": grant.as_str(),
            "instance_seconds": instance_seconds,
        }),
        Request::LeaseRun {
            lease,
            argv,
            timeout_seconds,
        } => json!({
            "kind": "lease_run",
            "lease": lease,
            "argv": argv,
            "timeout_seconds": timeout_seconds,
        }),
        Request::LeasePut {
            lease,
            tree,
            to,
            replace,
        } => json!({
            "kind": "lease_put",
            "lease": lease,
            "tree": tree,
            "to": to,
            "replace": replace,
        }),
        Request::LeaseGet { lease, from, to } => json!({
            "kind": "lease_get",
            "lease": lease,
            "from": from,
            "to": to,
        }),
        Request::LeaseStatus { lease } => json!({ "kind": "lease_status", "lease": lease }),
        Request::LeaseClose { lease } => json!({ "kind": "lease_close", "lease": lease }),
    }
}

/// The JSON form of an action.
#[expect(clippy::too_many_lines, reason = "one arm per action")]
fn action_json(workflow: &Workflow, action: &Action) -> Value {
    match action {
        Action::StartAgent => json!({ "kind": "start_agent" }),
        Action::Reply { op, reply } => {
            json!({ "kind": "reply", "op": op.0, "reply": reply })
        }
        Action::StartJudge {
            job,
            call,
            payload,
            timeout,
        } => json!({
            "kind": "start_judge",
            "job": job.0,
            "grant": match call {
                JudgeCall::Grant(grant) => Some(workflow.judge_grant(*grant).name.as_str()),
                JudgeCall::Score => None,
            },
            "payload": payload_json(payload),
            "timeout_ns": nanos(*timeout),
        }),
        Action::StopJudge { job } => json!({ "kind": "stop_judge", "job": job.0 }),
        Action::StartLease {
            lease,
            holder,
            grant,
            instance,
            start_by,
        } => json!({
            "kind": "start_lease",
            "lease": lease.0,
            "holder": logged::Caller::from(*holder),
            "grant": workflow.lease_grant(*grant).name.as_str(),
            "instance": instance,
            "start_by_ns": nanos(*start_by),
        }),
        Action::RunCommand {
            lease,
            command,
            argv,
            timeout,
        } => json!({
            "kind": "run_command",
            "lease": lease.0,
            "command": command,
            "argv": argv,
            "timeout_ns": timeout.map(nanos),
        }),
        Action::Transfer {
            lease,
            holder: _,
            transfer,
            direction,
            max_bytes,
        } => {
            let (direction, from, to) = match direction {
                decision::Direction::Put { tree, to, replace } if *replace => {
                    (Direction::Replace, tree, to)
                }
                decision::Direction::Put { tree, to, .. } => (Direction::Put, tree, to),
                decision::Direction::Get { from, to } => (Direction::Get, from, to),
            };
            json!({
                "kind": "transfer",
                "lease": lease.0,
                "transfer": transfer,
                "direction": direction,
                "from": from,
                "to": to,
                "max_bytes": max_bytes,
            })
        }
        Action::CloseLease { lease } => json!({ "kind": "close_lease", "lease": lease.0 }),
        Action::TearDownAgent => json!({ "kind": "tear_down_agent" }),
        Action::Finish { outcome } => {
            json!({ "kind": "finish", "outcome": outcome_json(outcome) })
        }
        Action::Abandon => json!({ "kind": "abandon" }),
    }
}

/// The JSON form of an outcome: `{"score": s, "work_end": w, "verdict": v}`, where the score
/// is `null` if there is none.
pub(super) fn outcome_json(outcome: &Outcome) -> Value {
    let Outcome { work_end, verdict } = outcome;
    let work_end = match work_end {
        WorkEnd::Deadline => json!({ "kind": "deadline" }),
        WorkEnd::Finalized => json!({ "kind": "finalized" }),
        WorkEnd::AgentExited(status) => json!({ "kind": "agent_exited", "status": status }),
        WorkEnd::AgentFailed(reason) => json!({ "kind": "agent_failed", "reason": reason }),
    };
    let verdict = match verdict {
        Verdict::NoSubmission => json!({ "kind": "no_submission" }),
        Verdict::Judged { submission, result } => json!({
            "kind": "judged",
            "submission": submission_json(submission),
            "result": result_json(result),
        }),
        Verdict::NoResult {
            submission,
            failure,
        } => json!({
            "kind": "no_result",
            "submission": submission_json(submission),
            "failure": failure_json(failure),
        }),
    };
    json!({ "score": outcome.score(), "work_end": work_end, "verdict": verdict })
}

fn submission_json(submission: &Submission) -> Value {
    let Submission { payload, finalized } = submission;
    json!({ "payload": payload_json(payload), "finalized": finalized })
}

fn result_json(result: &JudgeResult) -> Value {
    json!({
        "score": result.score(),
        "returned": result.returned(),
    })
}

fn failure_json(failure: &Failure) -> Value {
    match failure {
        Failure::Timeout => json!({ "kind": "timeout" }),
        Failure::Error(reason) => json!({ "kind": "error", "reason": reason }),
        Failure::InvalidResult(reason) => json!({ "kind": "invalid_result", "reason": reason }),
    }
}

fn payload_json(payload: &PayloadRef) -> Value {
    let PayloadRef { size, digest } = payload;
    json!({ "size": size, "digest": digest.as_str() })
}

/// Where a role's sandboxes are: a provider of the run's, and a class it offers, as
/// `provider.json` names them.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Place {
    pub provider: String,
    pub class: String,
}

/// What a run's log, as far as this build can read it ([`Readout`]), says of the run so far,
/// with the bounds of its workflow and of its roles' classes.
///
/// Each time is since the run started, as the log gives times.
#[derive(Debug)]
pub struct Progress {
    /// When the run started, by the driving machine's clock.
    pub started: UnixTime,
    /// When the core abandons the run if it is still going.
    pub hard_end: Duration,
    /// The work phase's deadline, counted from the agent's start.
    pub deadline: Option<Duration>,
    /// When the log wrote its last record.
    pub last: Duration,
    pub agent: AgentSpan,
    /// Each judge call, in the order the core started them.
    pub calls: Vec<CallSpan>,
    /// Each lease, in the order the core opened them.
    pub leases: Vec<LeaseSpan>,
    /// The model requests of every role's route.
    pub model: ModelUse,
    /// When the work phase ended, as the core tore the agent down, if it has.
    pub work_end: Option<Duration>,
    /// When the run ended, by the core's finish or abandon, if it has.
    pub end: Option<Duration>,
    /// The price of each class of the run's providers that declared one, in dollars an hour, by
    /// its place, as `provider.json` records it.
    pub prices: BTreeMap<Place, f64>,
}

/// What [`Record::progress`](crate::Record::progress), or a [`LogFold`], reads of a run's log.
///
/// The run's progress, once the log records its start with its hard end, and each part of the
/// log that this build can't read, in the log's order, which the progress leaves out.
#[derive(Debug)]
pub struct Readout {
    pub progress: Option<Progress>,
    pub unread: Vec<Unread>,
}

/// A part of a run log's line that this build can't read: it isn't JSON, or it is of a kind
/// this build knows but lacks a field of that kind's, or holds one in another form.
#[derive(Clone, Debug)]
pub struct Unread {
    /// Its line's number in the log, counting from 1.
    pub line: u64,
    pub part: Part,
    /// Why it doesn't read.
    pub reason: String,
}

/// Which part of a line doesn't read: the line, so none of it reads; its event; or its action
/// at an index, from 0, among those it holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    Line,
    Event,
    Action(usize),
}

/// Where it is, and why it doesn't read: `line 4: <reason>`, `line 4, its event: <reason>` or,
/// for the action at index 1, `line 4, its action 2: <reason>`.
impl fmt::Display for Unread {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { line, part, reason } = self;
        match part {
            Part::Line => write!(f, "line {line}: {reason}"),
            Part::Event => write!(f, "line {line}, its event: {reason}"),
            Part::Action(index) => write!(f, "line {line}, its action {}: {reason}", index + 1),
        }
    }
}

/// A sandbox of the run, whatever started it, from the core's start of it, at `asked`.
#[derive(Clone, Debug, Default)]
pub struct Sandbox {
    pub place: Option<Place>,
    /// How long its class may take to start it.
    pub startup: Option<Duration>,
    /// The GPUs its role needs, if it needs any.
    pub needs: Option<Gpu>,
    /// The GPUs its `sandboxd` saw as it started, each by its model as `nvidia-smi` names it, as
    /// the log's `sandbox_gpus` says, if it does.
    pub gpus: Option<Vec<String>>,
    pub asked: Duration,
    /// When its command started, or its runner was ready.
    pub started: Option<Duration>,
    /// When the run was done with it: once it had collected what the agent's work phase left, or
    /// the agent failed; once a judge call or a lease ended; else once it was deleted, as one
    /// is when the run stops short of the core's end.
    pub gone: Option<Duration>,
    /// Whether the run deleted it, as its `sandbox.json` in the record says, if it is there.
    pub deleted: Option<bool>,
}

/// The agent's sandbox, from the core's start of it, at time 0.
#[derive(Clone, Debug, Default)]
pub struct AgentSpan {
    pub sandbox: Sandbox,
    /// Whether its role has a model route.
    pub routed: bool,
    /// When it exited or failed, and how.
    pub end: Option<(Duration, AgentEnd)>,
}

/// How the agent ended: its command's exit, with its status, or its failure.
#[derive(Clone, Debug)]
pub enum AgentEnd {
    Exited(ExitStatus),
    Failed(String),
}

/// A judge call, job `job`.
#[derive(Clone, Debug)]
pub struct CallSpan {
    pub job: u64,
    /// The grant of the agent's that it was made under, or `None` for the workflow's score.
    pub grant: Option<String>,
    /// The judge role it called, if the record names it.
    pub role: Option<String>,
    pub sandbox: Sandbox,
    /// How long its command may run.
    pub timeout: Duration,
    /// Whether the core stopped it, or its command was killed at its timeout.
    pub stopped: bool,
    pub timed_out: bool,
    /// How the call ended, collected or failed, at its sandbox's `gone`.
    pub end: Option<CallEnd>,
    /// Its command's standard error in the record, if it holds any bytes.
    pub stderr: Option<PathBuf>,
    /// Its evidence directory in the record, if it collected one.
    pub evidence: Option<PathBuf>,
    /// Its own model requests, which the run's ([`Progress::model`]) count too.
    pub model: ModelUse,
}

/// A lease, `lease`, opened under the grant `grant`.
#[derive(Clone, Debug)]
pub struct LeaseSpan {
    pub lease: u64,
    pub grant: String,
    /// The job whose judge call holds it, or `None` for the agent.
    pub holder: Option<u64>,
    /// Its runner's sandbox.
    pub sandbox: Sandbox,
    /// How long its runner may be ready, as its grant says, or its holder asked at its open.
    pub ready_for: Option<Duration>,
    /// When the core closed it, and why.
    pub closed: Option<(Duration, Closed)>,
    /// How many commands and transfers it ran, and the last.
    pub commands: u64,
    pub last: Option<Work>,
    pub failed: Option<String>,
    /// How long it was busy, until the command or transfer that runs, if one does, began.
    busy: Duration,
    /// When the command or transfer that runs began, until it ended.
    working: Option<Duration>,
}

/// Why the core closed a lease: its holder asked it to, the agent's work ended, the judge call
/// that held it ended, or the lease's time, or its grant's, ran out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Closed {
    Asked,
    WorkEnded,
    CallEnded,
    Expired,
}

/// What a lease ran, and how it ended, once it has: a command, its `argv`, or a transfer of a
/// tree, from a path to a path.
#[derive(Clone, Debug)]
pub enum Work {
    Command {
        argv: Vec<String>,
        end: Option<CommandEnd>,
    },
    Transfer {
        direction: Direction,
        from: String,
        to: String,
        end: Option<TransferEnd>,
    },
}

impl LeaseSpan {
    /// How long its runner was charged for until `now`: from when it was ready until the core
    /// closed the lease, or its sandbox was gone.
    pub fn charged(&self, now: Duration) -> Duration {
        let Some(ready) = self.sandbox.started else {
            return Duration::ZERO;
        };
        self.until(now).saturating_sub(ready)
    }

    /// How much of the time it was charged for until `now` ([`LeaseSpan::charged`]) a command or
    /// transfer of its ran: from when the core accepted it until it ended, or the core closed
    /// the lease, or its sandbox was gone, whichever came first.
    pub fn busy(&self, now: Duration) -> Duration {
        let working = (self.working).map_or(Duration::ZERO, |since| {
            self.until(now).saturating_sub(since)
        });
        self.busy + working
    }

    /// When its runner's charge ends, as of `now`: when the core closed the lease, or else its
    /// sandbox was gone, or else `now`.
    fn until(&self, now: Duration) -> Duration {
        let closed = self.closed.map(|(at, _)| at);
        closed.or(self.sandbox.gone).unwrap_or(now)
    }

    /// Notes that its last command, if it ran one, ended at `at`, as `end` says.
    fn command_ended(&mut self, at: Duration, end: CommandEnd) {
        if let Some(Work::Command { end: last, .. }) = &mut self.last {
            *last = Some(end);
            self.rest(at);
        }
    }

    /// Notes that its last transfer, if it ran one, ended at `at`, as `end` says.
    fn transfer_ended(&mut self, at: Duration, end: TransferEnd) {
        if let Some(Work::Transfer { end: last, .. }) = &mut self.last {
            *last = Some(end);
            self.rest(at);
        }
    }

    /// Ends its busy time at `at`, if it is busy, or at its charge's end, if that came first.
    fn rest(&mut self, at: Duration) {
        if let Some(since) = self.working.take() {
            self.busy += self.until(at).saturating_sub(since);
        }
    }
}

/// How a judge call ended: with a result, which has a score, the JSON number, or not, when it
/// is what the judge returned, as JSON; with a result that was invalid; or with a failure.
#[derive(Clone, Debug)]
pub enum CallEnd {
    Scored(String),
    Unscored(String),
    Invalid(String),
    Failed(String),
}

/// The run's model requests, of every role with a route, as its routes answered them.
#[derive(Clone, Debug, Default)]
pub struct ModelUse {
    pub requests: u64,
    /// How many were answered each way: the upstream's status for a response forwarded whole,
    /// `<status> cut` for one cut short, and `refused` or `barred` for the route's refusal.
    pub answers: BTreeMap<String, u64>,
    /// How many of them stated that they failed ([`route::Failure`]), by the code each gave, or
    /// `None` for one that gave none.
    pub failed: BTreeMap<Option<String>, u64>,
    /// How many were not forwarded whole with a 2xx status: cut short, refused or barred by the
    /// route, or forwarded with another status.
    pub not_2xx: u64,
    /// When the good one that ended last was answered, a good one being forwarded whole, with a
    /// 2xx status, and its stream stating no failure.
    pub last_good: Option<Duration>,
    /// Each that went upstream and has no answer yet, by its number, with when it went.
    pub in_flight: BTreeMap<u64, Duration>,
    /// How long each good one whose start the log records took, from going upstream to its
    /// answer's end, in the order they were answered.
    pub took: Vec<Duration>,
    /// How long each of those whose stream's output began took to begin it.
    pub first_tokens: Vec<Duration>,
    /// The tokens of those whose responses reported their usage, summed.
    pub tokens: Tokens,
}

/// Tokens summed over the responses that reported them: `reported` is how many did, and each
/// `Option` count is `None` if none of those gave it.
#[derive(Clone, Debug, Default)]
pub struct Tokens {
    pub reported: u64,
    pub input: u64,
    pub output: u64,
    pub cached: Option<u64>,
    pub reasoning: Option<u64>,
}

/// Takes each line of the run log of the record in `dir` whose frame reads, as a [`Line`] of
/// the parts of it that read, in turn, by `take`, and returns the parts that don't; `None` if
/// there is no log. A last line without its newline is left out ([`take_lines`]).
///
/// # Errors
///
/// Fails if the log can't be read.
fn records(dir: &Path, take: impl FnMut(Line)) -> Result<Option<Vec<Unread>>, String> {
    let path = dir.join("log.jsonl");
    let Some(log) = open(&path)? else {
        return Ok(None);
    };
    let mut unread = Vec::new();
    take_lines(log, &path, &mut Cursor::default(), &mut unread, take)?;
    Ok(Some(unread))
}

/// The run log at `path`, open to read; `None` if there is none.
fn open(path: &Path) -> Result<Option<File>, String> {
    match File::open(path) {
        Ok(log) => Ok(Some(log)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context(path.display()),
    }
}

/// How far a read of a run log went: up to the end of the last whole line it took.
#[derive(Clone, Copy, Default)]
struct Cursor {
    /// The bytes before that end.
    size: u64,
    /// The lines before it, so the number of the last.
    lines: u64,
}

/// Takes each whole line of `log`, the run log at `path`, past `cursor`, whose frame reads, as
/// a [`Line`] of the parts of it that read, in turn, by `take`, adds the parts that don't to
/// `unread`, and moves `cursor` past each line as it takes it, so that a failed read leaves it
/// past the lines taken before. A last line without its newline, one being written or that a
/// crash cut short, is left to a later read.
fn take_lines(
    log: File,
    path: &Path,
    cursor: &mut Cursor,
    unread: &mut Vec<Unread>,
    mut take: impl FnMut(Line),
) -> Result<(), String> {
    let mut log = BufReader::new(log);
    log.seek(SeekFrom::Start(cursor.size))
        .context(path.display())?;
    let mut line = Vec::new();
    loop {
        line.clear();
        log.read_until(b'\n', &mut line).context(path.display())?;
        let Some(whole) = line.strip_suffix(b"\n") else {
            return Ok(());
        };
        cursor.size += line.len() as u64;
        cursor.lines += 1;
        if let Some(record) = Line::read(whole, cursor.lines, unread) {
            take(record);
        }
    }
}

/// The final judge's job: the one that the run log of the record in `dir` names in its
/// `start_judge` without a grant.
///
/// # Errors
///
/// Fails if the log can't be read, or doesn't name such a job.
pub(super) fn final_judge(dir: &Path) -> Result<u64, String> {
    let mut job = None;
    let unread = records(dir, |line| {
        let started = line.actions.iter().find_map(logged::Action::score_job);
        job = job.or(started);
    })?;
    let path = dir.join("log.jsonl");
    match (unread, job) {
        (_, Some(job)) => Ok(job),
        (None, None) => Err(format!(
            "{} doesn't exist: expected a run's log",
            path.display()
        )),
        (Some(unread), None) => {
            let mut error = format!(
                "{} names no final judge: expected a `start_judge` without a grant",
                path.display()
            );
            if let Some(first) = unread.first() {
                let _ = write!(
                    error,
                    ", among the parts that this pumpkin can read; {} of its parts can't be read, \
                     the first at {first}",
                    unread.len(),
                );
            }
            Err(error)
        }
    }
}

/// Reads what the record in `dir` says of its run so far ([`Readout`]).
///
/// # Errors
///
/// Fails if a file of the record can't be read, or, once the log exists, its `ir.json` or
/// `provider.json` isn't JSON.
pub(super) fn progress(dir: &Path) -> Result<Readout, String> {
    let mut fold = LogFold::new(dir);
    let None = fold.take()? else {
        unreachable!("a fold that has taken nothing never starts over");
    };
    let LogFold {
        dir,
        roles,
        reading,
        unread,
        file: _,
        taken: _,
    } = fold;
    readout(&dir, roles.as_ref(), reading, unread)
}

/// A run's log, as far as it was read, kept to be read on as the run appends to it.
///
/// Each [`take`](Self::take) reads only what the log gained, and a [`readout`](Self::readout)
/// says what all it took says, as a read of those bytes whole would.
///
/// Modelled on sapling's indexedlog `FoldState`, which resumes a fold over an append-only log
/// from the offset it reached, and starts over when `self.offset > log.disk_buf.len() as u64 ||
/// self.epoch != log.meta.epoch`, a log's epoch here being its file's device and inode:
/// <https://github.com/meta/sapling/blob/8a502b24d6777264898ad36acad97209e9ab1978/eden/scm/lib/indexedlog/src/log/fold.rs#L158-L216>.
pub struct LogFold {
    dir: PathBuf,
    /// What `ir.json` and `provider.json` say, read as the log is first found: a run writes
    /// each of them whole, once, before it creates its log.
    roles: Option<Roles>,
    reading: Reading,
    unread: Vec<Unread>,
    /// The device and inode of the log file taken, once it is found.
    file: Option<(u64, u64)>,
    taken: Cursor,
}

/// Why a [`LogFold`] started over: its log is no longer the one it took some of.
///
/// It found no log at the log's path, a log shorter than it had taken, or another file, by its
/// device and inode, none of which a run's log, that only grows, ever is.
#[derive(Debug)]
pub struct StartedOver {
    /// The bytes it had taken.
    taken: u64,
    found: Found,
}

#[derive(Debug)]
enum Found {
    Gone,
    /// A log of this many bytes.
    Shorter(u64),
    Other,
}

/// `its log is 120 bytes, shorter than the 4096 bytes read of it before: reading it again from
/// its start`.
impl fmt::Display for StartedOver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { taken, found } = self;
        match found {
            Found::Gone => write!(
                f,
                "its log is gone, after {taken} bytes were read of it: reading the next log \
                 there from its start"
            ),
            Found::Shorter(size) => write!(
                f,
                "its log is {size} bytes, shorter than the {taken} bytes read of it before: \
                 reading it again from its start"
            ),
            Found::Other => write!(
                f,
                "its log is another file than the one {taken} bytes were read of before: \
                 reading it from its start"
            ),
        }
    }
}

impl LogFold {
    /// The log of the record in `dir`, none of it taken.
    pub(super) fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_owned(),
            roles: None,
            reading: Reading::default(),
            unread: Vec::new(),
            file: None,
            taken: Cursor::default(),
        }
    }

    /// Takes each whole line that the log gained since the last take; a last line without its
    /// newline waits for a later one. A failed take keeps the lines it took, and the next goes
    /// on after them. Once some of the log is taken, if it is gone, shorter, or another file,
    /// the fold starts over, takes the log from its start, and says so.
    ///
    /// # Errors
    ///
    /// Fails if a file of the record can't be read, or, once the log exists, its `ir.json` or
    /// `provider.json` isn't JSON.
    pub fn take(&mut self) -> Result<Option<StartedOver>, String> {
        let path = self.dir.join("log.jsonl");
        let log = open(&path)?;
        let found = match &log {
            Some(log) => {
                let meta = log.metadata().context(path.display())?;
                Some(((meta.dev(), meta.ino()), meta.len()))
            }
            None => None,
        };
        let taken = self.taken.size;
        let started_over = if taken == 0 {
            None
        } else {
            match found {
                None => Some(Found::Gone),
                Some((file, _)) if self.file != Some(file) => Some(Found::Other),
                Some((_, size)) => (size < taken).then_some(Found::Shorter(size)),
            }
        }
        .map(|found| StartedOver { taken, found });
        if started_over.is_some() {
            *self = Self::new(&self.dir);
        }
        let (Some(log), Some((file, _))) = (log, found) else {
            return Ok(started_over);
        };
        self.file = Some(file);
        let roles = if let Some(roles) = self.roles.take() {
            roles
        } else {
            let ir = json_file(&self.dir.join("ir.json"))?;
            let provided = json_file(&self.dir.join("provider.json"))?;
            Roles::of(&ir, &provided)
        };
        let roles = self.roles.insert(roles);
        let reading = &mut self.reading;
        take_lines(log, &path, &mut self.taken, &mut self.unread, |line| {
            reading.take(&line, roles);
        })?;
        Ok(started_over)
    }

    /// What the lines taken say of the run ([`Readout`]), with what the record says now of
    /// each sandbox that they name. More can be taken after.
    ///
    /// # Errors
    ///
    /// Fails if a sandbox's `sandbox.json` in the record can't be read, or isn't JSON.
    pub fn readout(&self) -> Result<Readout, String> {
        readout(
            &self.dir,
            self.roles.as_ref(),
            self.reading.clone(),
            self.unread.clone(),
        )
    }
}

/// What `reading`, of the log of the record in `dir`, with `roles` and `unread`, says of the
/// run ([`LogFold::readout`]).
fn readout(
    dir: &Path,
    roles: Option<&Roles>,
    reading: Reading,
    unread: Vec<Unread>,
) -> Result<Readout, String> {
    let (Some(roles), Some((started, hard_end))) = (roles, reading.started) else {
        return Ok(Readout {
            progress: None,
            unread,
        });
    };
    let Reading {
        started: _,
        last,
        mut agent,
        calls,
        leases,
        model,
        work_end,
        end,
    } = reading;
    agent.sandbox = Sandbox {
        started: agent.sandbox.started,
        gone: agent.sandbox.gone,
        deleted: deleted(&dir.join("agent"))?,
        gpus: agent.sandbox.gpus.take(),
        ..roles.sandbox(Some(&roles.agent_role), Duration::ZERO)
    };
    agent.routed = roles.routed;
    let mut progress = Progress {
        started,
        hard_end,
        deadline: roles.deadline,
        last,
        agent,
        calls,
        leases,
        model,
        work_end,
        end,
        prices: roles.prices.clone(),
    };
    // Each sandbox's `sandbox.json` is rewritten as the run deletes it, and a judge call's
    // standard error and evidence are collected as it ends, so they are read now.
    for lease in &mut progress.leases {
        lease.sandbox.deleted = deleted(&dir.join(format!("leases/{}", lease.lease)))?;
    }
    for call in &mut progress.calls {
        let job = dir.join(format!("jobs/{}", call.job));
        call.sandbox.deleted = deleted(&job)?;
        let stderr = job.join(schema::sandbox::command_stderr().as_str());
        let held = fs::metadata(&stderr).is_ok_and(|stderr| stderr.len() > 0);
        call.stderr = held.then_some(stderr);
        let evidence = job.join(schema::sandbox::evidence_dir().as_str());
        call.evidence = evidence.is_dir().then_some(evidence);
    }
    Ok(Readout {
        progress: Some(progress),
        unread,
    })
}

/// The JSON in `path`, or `null` if there is no such file.
fn json_file(path: &Path) -> Result<Value, String> {
    match record::read_optional(path)? {
        Some(bytes) => serde_json::from_slice(&bytes).context(path.display()),
        None => Ok(Value::Null),
    }
}

/// Whether the sandbox whose record is in `dir` was deleted, as its `sandbox.json` says, if it
/// is there.
fn deleted(dir: &Path) -> Result<Option<bool>, String> {
    let sandbox = json_file(&dir.join("sandbox.json"))?;
    Ok((!sandbox.is_null()).then(|| sandbox["delete"]["state"] == "deleted"))
}

/// What the record's `ir.json` and `provider.json` say of where the run's roles are, what GPUs
/// they need, and their bounds.
#[derive(Default)]
struct Roles {
    /// The agent's role's name.
    agent_role: String,
    /// Whether the agent's role has a model route.
    routed: bool,
    /// Each judge or lease grant, by the name of the role that holds it and its own name.
    grants: BTreeMap<(String, String), Grant>,
    /// The score's judge role's name.
    score_role: Option<String>,
    /// Each role's place, by its name.
    places: BTreeMap<String, Place>,
    /// The GPUs of each role that needs any, by its name.
    gpus: BTreeMap<String, Gpu>,
    deadline: Option<Duration>,
    /// Each class's startup, by its place.
    startups: BTreeMap<Place, Duration>,
    /// Each class's price, in dollars an hour, by its place, if its provider declared one.
    prices: BTreeMap<Place, f64>,
}

impl Roles {
    fn of(ir: &Value, provided: &Value) -> Self {
        let roles_placed = provided["roles"].as_object().into_iter().flatten();
        let places = roles_placed.filter_map(|(role, placed)| {
            let place = Place {
                provider: placed["provider"].as_str()?.to_owned(),
                class: placed["class"].as_str()?.to_owned(),
            };
            Some((role.clone(), place))
        });
        let roles = ir["roles"].as_array().into_iter().flatten();
        let gpus = roles.clone().filter_map(|role| {
            let gpu = Gpu::deserialize(&role["needs"]["gpu"]).ok()?;
            Some((role["name"].as_str()?.to_owned(), gpu))
        });
        let agent = (roles.clone()).find(|role| !role["runtime"]["agent"].is_null());
        let agent = agent.unwrap_or(&Value::Null);
        let agent_role = agent["name"].as_str().unwrap_or_default();
        let held = roles.map(|role| (role["name"].as_str().unwrap_or_default(), &role["grants"]));
        let grants = held.flat_map(|(holder, grants)| {
            let grants = grants.as_array().into_iter().flatten();
            grants.filter_map(move |grant| {
                // `{"judge": g}`, `{"lease": g}` or `{"submit": g}`.
                let (_, grant) = grant.as_object()?.iter().next()?;
                let role = grant["role"].as_str().map(str::to_owned);
                let granted = Grant {
                    role,
                    ready_for: grant["instance_seconds"].as_u64().map(Duration::from_secs),
                };
                Some((
                    (holder.to_owned(), grant["name"].as_str()?.to_owned()),
                    granted,
                ))
            })
        });
        let score = ir["score"]["role"].as_str();
        let (mut startups, mut prices) = (BTreeMap::new(), BTreeMap::new());
        let providers = provided["providers"].as_object().into_iter().flatten();
        for (provider, ready) in providers {
            let classes = ready["classes"].as_object().into_iter().flatten();
            for (class, offered) in classes {
                let place = Place {
                    provider: provider.clone(),
                    class: class.clone(),
                };
                if let Some(price) = offered["usd_per_hour"].as_f64() {
                    prices.insert(place.clone(), price);
                }
                if let Some(seconds) = offered["startup_seconds"].as_u64() {
                    startups.insert(place, Duration::from_secs(seconds));
                }
            }
        }
        Self {
            agent_role: agent_role.to_owned(),
            routed: !agent["egress"]["model"].is_null(),
            grants: grants.collect(),
            score_role: score.map(str::to_owned),
            places: places.collect(),
            gpus: gpus.collect(),
            deadline: (agent["runtime"]["agent"]["deadline_seconds"].as_u64())
                .map(Duration::from_secs),
            startups,
            prices,
        }
    }

    /// A sandbox of `role`, which the core started at `asked`, in its role's place, with its
    /// class's startup and the GPUs its role needs.
    fn sandbox(&self, role: Option<&str>, asked: Duration) -> Sandbox {
        let place = role.and_then(|role| self.places.get(role)).cloned();
        Sandbox {
            startup: (place.as_ref()).and_then(|place| self.startups.get(place).copied()),
            place,
            needs: role.and_then(|role| self.gpus.get(role)).cloned(),
            asked,
            ..Sandbox::default()
        }
    }
}

/// A judge or lease grant: the role it names, and how long a lease's runner may be ready.
#[derive(Clone, Default)]
struct Grant {
    role: Option<String>,
    ready_for: Option<Duration>,
}

/// A log as far as it has been read: what its lines say, each as [`Progress`]'s field of its
/// name holds it.
#[derive(Clone, Default)]
struct Reading {
    /// When the run started, and its hard end, once its start is read.
    started: Option<(UnixTime, Duration)>,
    last: Duration,
    agent: AgentSpan,
    calls: Vec<CallSpan>,
    leases: Vec<LeaseSpan>,
    model: ModelUse,
    work_end: Option<Duration>,
    end: Option<Duration>,
}

impl Reading {
    /// Takes in `line`, a line of the log of a run whose roles are `roles`.
    fn take(&mut self, line: &Line, roles: &Roles) {
        self.last = self.last.max(line.at_ns);
        let at = line.when();
        let event = line.event.as_ref();
        if let Some(event) = event {
            self.event(at, event);
        }
        let request = event.and_then(logged::Event::request);
        let closed = if matches!(request, Some(logged::Request::LeaseClose)) {
            Closed::Asked
        } else if (line.actions.iter())
            .any(|action| matches!(action, logged::Action::TearDownAgent))
        {
            Closed::WorkEnded
        } else {
            Closed::Expired
        };
        // A judge call's end closes the leases it holds.
        let ended = event.and_then(logged::Event::judge_job);
        // A holder may ask, at open, for a shorter life than its grant's.
        let asked = match request {
            Some(logged::Request::LeaseOpen { instance_seconds }) => {
                instance_seconds.map(Duration::from_secs)
            }
            Some(logged::Request::LeaseClose | logged::Request::Other) | None => None,
        };
        if let Some(note) = &line.note
            && note.starts_with("collected from the agent's sandbox")
        {
            self.agent.sandbox.gone = Some(at);
        }
        for action in &line.actions {
            self.action(at, action, (closed, ended), roles);
            if let (Some(asked), logged::Action::StartLease { lease, .. }) = (asked, action)
                && let Some(lease) = self.lease(*lease)
            {
                lease.ready_for = Some(lease.ready_for.map_or(asked, |most| most.min(asked)));
            }
        }
    }

    /// Takes in `event`, observed at `at`.
    #[expect(clippy::too_many_lines, reason = "one arm per kind of event")]
    fn event(&mut self, at: Duration, event: &logged::Event) {
        match event {
            logged::Event::RunStarted {
                unix_ns,
                hard_end_ns,
            } => self.started = hard_end_ns.map(|hard_end| (*unix_ns, hard_end)),
            logged::Event::AgentStarted => self.agent.sandbox.started = Some(at),
            logged::Event::AgentExited { status } => {
                self.agent.end = Some((at, AgentEnd::Exited(*status)));
            }
            logged::Event::AgentFailed { reason } => {
                self.agent.end = Some((at, AgentEnd::Failed(reason.clone())));
                self.agent.sandbox.gone.get_or_insert(at);
            }
            logged::Event::JudgeStarted { job } => {
                if let Some(call) = self.call(*job) {
                    call.sandbox.started = Some(at);
                }
            }
            logged::Event::JudgeTimedOut { job } => {
                if let Some(call) = self.call(*job) {
                    call.timed_out = true;
                }
            }
            logged::Event::JudgeCollected { job, collected } => {
                let end = match collected {
                    Collected::Result(result) => match &result.score {
                        Some(score) => CallEnd::Scored(score.to_string()),
                        None => CallEnd::Unscored(result.returned.to_string()),
                    },
                    Collected::Invalid(invalid) => CallEnd::Invalid(invalid.clone()),
                };
                if let Some(call) = self.call(*job) {
                    call.end(at, end);
                }
            }
            logged::Event::JudgeFailed { job, reason } => {
                if let Some(call) = self.call(*job) {
                    call.end(at, CallEnd::Failed(reason.clone()));
                }
            }
            logged::Event::LeaseReady { lease } => {
                if let Some(lease) = self.lease(*lease) {
                    lease.sandbox.started = Some(at);
                }
            }
            logged::Event::CommandEnded { lease, end } => {
                if let Some(lease) = self.lease(*lease) {
                    lease.command_ended(at, *end);
                }
            }
            logged::Event::TransferEnded { lease, end } => {
                if let Some(lease) = self.lease(*lease) {
                    lease.transfer_ended(at, end.clone());
                }
            }
            logged::Event::LeaseEnded { lease } => {
                if let Some(lease) = self.lease(*lease) {
                    lease.sandbox.gone = Some(at);
                    lease.failed = None;
                }
            }
            logged::Event::LeaseFailed { lease, reason } => {
                if let Some(lease) = self.lease(*lease) {
                    lease.sandbox.gone = Some(at);
                    lease.failed = Some(reason.clone());
                }
            }
            logged::Event::ModelRequestStarted { caller, request } => {
                self.model.in_flight.insert(*request, at);
                if let logged::Caller::Job(job) = caller
                    && let Some(call) = self.call(*job)
                {
                    call.model.in_flight.insert(*request, at);
                }
            }
            logged::Event::ModelAnswered {
                caller,
                request,
                first_token_ns,
                upstream_status,
                answer,
                failed,
                usage,
            } => {
                let answered = Answered {
                    request: *request,
                    first_token: *first_token_ns,
                    upstream_status: *upstream_status,
                    answer,
                    failed: failed.as_ref(),
                    usage: usage.as_ref(),
                };
                self.model.take(at, &answered);
                // A judge call's own, which the run's count too.
                if let Some(logged::Caller::Job(job)) = caller
                    && let Some(call) = self.call(*job)
                {
                    call.model.take(at, &answered);
                }
            }
            logged::Event::SandboxGpus { sandbox, gpus } => {
                if let Some(sandbox) = self.named(*sandbox) {
                    sandbox.gpus = Some(gpus.clone());
                }
            }
            logged::Event::SandboxDeleted { sandbox } => {
                if let Some(sandbox) = sandbox.and_then(|sandbox| self.named(sandbox)) {
                    sandbox.gone.get_or_insert(at);
                }
            }
            logged::Event::JudgeExited { .. }
            | logged::Event::OpArrived { .. }
            | logged::Event::OpRefused
            | logged::Event::Tick
            | logged::Event::Unknown => {}
        }
    }
}

impl CallSpan {
    fn end(&mut self, at: Duration, end: CallEnd) {
        self.end = Some(end);
        self.sandbox.gone = Some(at);
    }
}

impl Reading {
    /// The sandbox that `name` names within the run, if the core started it.
    fn named(&mut self, name: SandboxName) -> Option<&mut Sandbox> {
        match name {
            SandboxName::Agent => Some(&mut self.agent.sandbox),
            SandboxName::Job(job) => self.call(job).map(|call| &mut call.sandbox),
            SandboxName::Lease(lease) => self.lease(lease).map(|lease| &mut lease.sandbox),
        }
    }

    /// Judge call `job`, if the core started it.
    fn call(&mut self, job: u64) -> Option<&mut CallSpan> {
        self.calls.iter_mut().find(|call| call.job == job)
    }

    /// Lease `lease`, if the core opened it.
    fn lease(&mut self, lease: u64) -> Option<&mut LeaseSpan> {
        self.leases.iter_mut().find(|span| span.lease == lease)
    }
}

impl Progress {
    /// Each sandbox of the run: the agent's, each judge call's, and each lease's.
    fn each_sandbox(&self) -> impl Iterator<Item = &Sandbox> {
        let calls = self.calls.iter().map(|call| &call.sandbox);
        let leases = self.leases.iter().map(|lease| &lease.sandbox);
        std::iter::once(&self.agent.sandbox)
            .chain(calls)
            .chain(leases)
    }

    /// Each place the run's sandboxes were in, with how many, how long they lasted in all, each
    /// from the core's start of it until the run was done with it, or the run's end, or else the
    /// log's last record, how long its leases' runners were charged for ([`LeaseSpan::charged`]),
    /// and busy ([`LeaseSpan::busy`]),
    /// and how many the record says were created, and deleted.
    pub fn sandboxes(&self) -> BTreeMap<Place, PlaceUse> {
        let now = self.end.unwrap_or(self.last);
        let mut sandboxes: BTreeMap<Place, PlaceUse> = BTreeMap::new();
        for sandbox in self.each_sandbox() {
            let Some(place) = &sandbox.place else {
                continue;
            };
            let used = sandboxes.entry(place.clone()).or_insert_with(|| PlaceUse {
                count: 0,
                lasted: Duration::ZERO,
                charged: None,
                created: 0,
                deleted: 0,
                usd_per_hour: self.prices.get(place).copied(),
            });
            used.count += 1;
            used.lasted += sandbox.gone.unwrap_or(now).saturating_sub(sandbox.asked);
            used.created += u64::from(sandbox.deleted.is_some());
            used.deleted += u64::from(sandbox.deleted == Some(true));
        }
        for lease in &self.leases {
            let used = (lease.sandbox.place.as_ref()).and_then(|place| sandboxes.get_mut(place));
            if let Some(used) = used {
                let (charged, busy) = used.charged.get_or_insert_default();
                *charged += lease.charged(now);
                *busy += lease.busy(now);
            }
        }
        sandboxes
    }

    /// What the run's sandboxes cost, in dollars, at their classes' prices, and whether some
    /// lasted in a class without one, which the sum leaves out.
    pub fn usd(&self) -> (f64, bool) {
        let sandboxes = self.sandboxes();
        // From 0, as `sum` of no `f64` is -0.
        let usd = (sandboxes.values().filter_map(PlaceUse::usd)).fold(0.0, |usd, each| usd + each);
        (
            usd,
            sandboxes.values().any(|used| used.usd_per_hour.is_none()),
        )
    }
}

/// The sandboxes a run had in a place.
///
/// How many, how long they lasted in all, how many the record says were created, and deleted,
/// and the price of the place's class, in dollars an hour, if its provider declared one.
#[derive(Debug)]
pub struct PlaceUse {
    pub count: u64,
    pub lasted: Duration,
    /// How long the runners of the leases among them were charged for, if there are any, and how
    /// much of that their commands and transfers ran ([`LeaseSpan::busy`]).
    pub charged: Option<(Duration, Duration)>,
    pub created: u64,
    pub deleted: u64,
    pub usd_per_hour: Option<f64>,
}

impl PlaceUse {
    /// What they cost, in dollars, at the price, if there is one.
    pub fn usd(&self) -> Option<f64> {
        Some(self.usd_per_hour? * self.lasted.as_secs_f64() / 3600.0)
    }
}

/// What a run used, as its `outcome.json` records it: `{"ran_ns": r, "model_requests": m,
/// "tokens": t, "calls": [c, ...], "sandboxes": [s, ...], "usd": d}`: when its log last
/// wrote, the model requests its routes answered and the tokens they reported, `{"responses": n,
/// "input": i, "output": o}` with `"cached"` and `"reasoning"` where given; each judge call's
/// share, if it made model requests, `{"job": j, "grant": g, "model_requests": m, "tokens": t}`;
/// each place's sandboxes, `{"provider": p, "class": c, "count": n, "lasted_ns": l}`, with
/// `"charged_ns"` and `"busy_ns"` for leases' ([`PlaceUse::charged`]), `"undeleted"`, and
/// `"usd_per_hour"` and `"usd"` for a priced class ([`Progress::sandboxes`]); and their cost.
pub(super) fn usage_of(progress: &Progress) -> Value {
    let sandboxes = progress.sandboxes().into_iter().map(|(place, used)| {
        let mut sandboxes = json!({
            "provider": place.provider,
            "class": place.class,
            "count": used.count,
            "lasted_ns": nanos(used.lasted),
        });
        if let Some((charged, busy)) = used.charged {
            sandboxes["charged_ns"] = json!(nanos(charged));
            sandboxes["busy_ns"] = json!(nanos(busy));
        }
        if used.deleted < used.created {
            sandboxes["undeleted"] = json!(used.created - used.deleted);
        }
        if let (Some(price), Some(usd)) = (used.usd_per_hour, used.usd()) {
            sandboxes["usd_per_hour"] = json!(price);
            sandboxes["usd"] = json!(usd);
        }
        sandboxes
    });
    let mut usage = json!({
        "ran_ns": nanos(progress.last),
        "model_requests": progress.model.requests,
        "tokens": tokens_json(&progress.model.tokens),
        "sandboxes": sandboxes.collect::<Vec<Value>>(),
    });
    let calls = progress.calls.iter().filter(|call| call.model.requests > 0);
    let calls: Vec<Value> = calls
        .map(|call| {
            json!({
                "job": call.job,
                "grant": call.grant,
                "model_requests": call.model.requests,
                "tokens": tokens_json(&call.model.tokens),
            })
        })
        .collect();
    if !calls.is_empty() {
        usage["calls"] = json!(calls);
    }
    if !progress.prices.is_empty() {
        usage["usd"] = json!(progress.usd().0);
    }
    usage
}

/// The JSON form of `tokens`, as [`usage_of`] gives it.
fn tokens_json(tokens: &Tokens) -> Value {
    let Tokens {
        reported,
        input,
        output,
        cached,
        reasoning,
    } = tokens;
    let mut json = json!({ "responses": reported, "input": input, "output": output });
    if let Some(cached) = cached {
        json["cached"] = json!(cached);
    }
    if let Some(reasoning) = reasoning {
        json["reasoning"] = json!(reasoning);
    }
    json
}

impl Reading {
    /// Takes in `action`, which the core decided on at `at`, in a run whose roles are `roles`;
    /// a lease it closes is `closed`, but one that the judge call of job `ended`, which ended,
    /// holds.
    #[expect(clippy::too_many_lines, reason = "one arm per kind of action")]
    fn action(
        &mut self,
        at: Duration,
        action: &logged::Action,
        (closed, ended): (Closed, Option<u64>),
        roles: &Roles,
    ) {
        match action {
            logged::Action::StartJudge {
                job,
                grant,
                timeout_ns,
            } => {
                let role = match grant {
                    Some(grant) => {
                        let key = (roles.agent_role.clone(), grant.clone());
                        roles.grants.get(&key).cloned().unwrap_or_default().role
                    }
                    None => roles.score_role.clone(),
                };
                self.calls.push(CallSpan {
                    job: *job,
                    grant: grant.clone(),
                    sandbox: roles.sandbox(role.as_deref(), at),
                    role,
                    timeout: *timeout_ns,
                    stopped: false,
                    timed_out: false,
                    end: None,
                    stderr: None,
                    evidence: None,
                    model: ModelUse::default(),
                });
            }
            logged::Action::StopJudge { job } => {
                if let Some(call) = self.call(*job) {
                    call.stopped = true;
                }
            }
            logged::Action::StartLease {
                lease,
                holder,
                grant,
            } => self.open_lease(at, *lease, *holder, grant, roles),
            logged::Action::RunCommand { lease, argv } => {
                let command = Work::Command {
                    argv: argv.clone(),
                    end: None,
                };
                self.lease_ran(at, *lease, command);
            }
            logged::Action::Transfer {
                lease,
                direction,
                from,
                to,
            } => {
                let transfer = Work::Transfer {
                    direction: *direction,
                    from: from.clone(),
                    to: to.clone(),
                    end: None,
                };
                self.lease_ran(at, *lease, transfer);
            }
            logged::Action::CloseLease { lease } => {
                if let Some(lease) = self.lease(*lease) {
                    let closed = match lease.holder {
                        Some(holder) if ended == Some(holder) => Closed::CallEnded,
                        _ => closed,
                    };
                    lease.closed.get_or_insert((at, closed));
                }
            }
            logged::Action::TearDownAgent => {
                self.work_end.get_or_insert(at);
            }
            logged::Action::Finish | logged::Action::Abandon => self.end = Some(at),
            logged::Action::StartAgent | logged::Action::Reply | logged::Action::Unknown => {}
        }
    }

    /// Takes in the open of lease `lease`, under `grant`, held by `holder`, which the core
    /// decided on at `at`, in a run whose roles are `roles`.
    fn open_lease(
        &mut self,
        at: Duration,
        lease: u64,
        holder: Option<logged::Caller>,
        grant: &str,
        roles: &Roles,
    ) {
        let holder = match holder {
            Some(logged::Caller::Job(job)) => Some(job),
            Some(logged::Caller::Agent) | None => None,
        };
        let call = holder.and_then(|holder| self.calls.iter().find(|call| call.job == holder));
        let role = match call {
            Some(call) => call.role.clone().unwrap_or_default(),
            None => roles.agent_role.clone(),
        };
        let granted = roles.grants.get(&(role, grant.to_owned())).cloned();
        let Grant { role, ready_for } = granted.unwrap_or_default();
        self.leases.push(LeaseSpan {
            lease,
            grant: grant.to_owned(),
            holder,
            sandbox: roles.sandbox(role.as_deref(), at),
            ready_for,
            closed: None,
            commands: 0,
            last: None,
            failed: None,
            busy: Duration::ZERO,
            working: None,
        });
    }

    /// Notes that lease `lease` ran `work`, which is its last, from `at`, if the core opened it.
    fn lease_ran(&mut self, at: Duration, lease: u64, work: Work) {
        if let Some(lease) = self.lease(lease) {
            lease.commands += 1;
            lease.last = Some(work);
            lease.working = Some(at);
        }
    }
}

/// A model request's answer, as its `model_answered` says: its number, when its stream's output
/// began, if it did, its upstream's status, if a response came, how the route answered it, the
/// code of the failure it stated, if it stated one, and the tokens it reported, if it did.
struct Answered<'e> {
    request: u64,
    first_token: Option<Duration>,
    upstream_status: Option<u16>,
    answer: &'e logged::Answer,
    failed: Option<&'e logged::Failed>,
    usage: Option<&'e logged::Usage>,
}

impl ModelUse {
    /// Takes in `answered`, a model request answered at `at`.
    fn take(&mut self, at: Duration, answered: &Answered<'_>) {
        let Answered {
            request,
            first_token,
            upstream_status,
            answer,
            failed,
            usage,
        } = answered;
        self.requests += 1;
        let started = self.in_flight.remove(request);
        if let Some(logged::Failed { code }) = failed {
            *self.failed.entry(code.clone()).or_default() += 1;
        }
        let whole_2xx = matches!(answer, logged::Answer::Forwarded)
            && upstream_status.is_some_and(|status| (200..300).contains(&status));
        if !whole_2xx {
            self.not_2xx += 1;
        } else if failed.is_none() {
            // Threads write their answers as they end, which needn't be in the order they ended.
            self.last_good = self.last_good.max(Some(at));
            if let Some(started) = started {
                self.took.push(at.saturating_sub(started));
                if let Some(first_token) = first_token {
                    self.first_tokens.push(first_token.saturating_sub(started));
                }
            }
        }
        // As the log gives the status: `null` for none.
        let status = upstream_status.map_or_else(|| "null".to_owned(), |status| status.to_string());
        let answer = match answer {
            logged::Answer::Forwarded => status,
            logged::Answer::Cut => format!("{status} cut"),
            logged::Answer::Barred => "barred".to_owned(),
            logged::Answer::Refused => "refused".to_owned(),
        };
        *self.answers.entry(answer).or_default() += 1;
        if let Some(usage) = usage {
            let tokens = &mut self.tokens;
            tokens.reported += 1;
            tokens.input += usage.input_tokens;
            tokens.output += usage.output_tokens;
            if let Some(cached) = usage.cached_tokens {
                *tokens.cached.get_or_insert(0) += cached;
            }
            if let Some(reasoning) = usage.reasoning_tokens {
                *tokens.reasoning.get_or_insert(0) += reasoning;
            }
        }
    }
}
