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
//! `start_judge` names the agent's grant of the call, `null` for the score, `start_lease` the
//! lease's; the op's request gives a command's argv and a transfer's paths, never what moved.
//! A reply, to the line's op, is the [`OpReply`] sent, a `status` or `lease_status` by kind alone.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use decision::{
    Action, Caller, Event, Failure, JudgeResult, Outcome, PayloadRef, Request, Submission, Verdict,
    WorkEnd,
};
use gateway::RefusedOp;
use provider::UnixTime;
use route::{Answer, Usage};
use schema::ir::{Api, GpuModel, Name};
use schema::sandbox::op::OpReply;
use schema::workflow::{JudgeCall, Workflow};
use serde_json::{Value, json};
use stdx::Context;

use super::sandbox::Deleted;
use super::{Clock, Durable, Host, record};

mod fold;
mod logged;
mod progress;
mod usage;

pub use fold::{LogFold, StartedOver, TakenLine};
pub(super) use fold::{final_judge, lines, progress};
pub use logged::Direction;
pub(super) use progress::places;
pub use progress::{
    AgentEnd, AgentSpan, CallEnd, CallSpan, Closed, LeaseSpan, ModelUse, Part, Place, Progress,
    Readout, Sandbox, Tokens, Unread, Work,
};
pub use usage::{Cost, PlaceUse, Priced, Used};
pub(super) use usage::{usage_of, used};

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

    /// Appends how `caller`'s route answered its request `request`, observed at its end: `{"kind":
    /// "model_answered", "caller": c, "request": n, "upstream_status": u, "answer": a, "kept": k}`;
    /// `"upstream"` and `"api"` as [`ModelAnswer`] says, `"model"` as [`Answer`] does;
    /// `"request_id"` if the upstream named one; `"usage"` if reported ([`Usage`]); `"failed":
    /// {"code": c}` if it stated a failure, without `"code"` if it gave none ([`route::Failure`]);
    /// and if a response came, `"first_byte_ns"` and `"first_token_ns"` as [`ModelAnswer`] says,
    /// where it has them. `u` is the upstream's status, or `null` if no response came, `a` the
    /// [`Answer`]'s kind, with a refusal's or a bar's status and message, but never a body, and `k`
    /// where the record keeps the exchange ([`Exchange::kept`](super::Exchange::kept)).
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
        let (model, upstream_status, request_id, (usage, failed), answer) = match answer {
            Answer::Forwarded {
                model,
                status,
                request_id,
                usage,
                failed,
            } => (
                Some(model),
                Some(*status),
                request_id,
                (usage, failed),
                json!({ "kind": "forwarded" }),
            ),
            Answer::Cut {
                model,
                status,
                request_id,
                usage,
                failed,
            } => (
                Some(model),
                Some(*status),
                request_id,
                (usage, failed),
                json!({ "kind": "cut" }),
            ),
            Answer::Barred { status, message } => {
                let barred = json!({ "kind": "barred", "status": status, "message": message });
                (None, None, &None, (&None, &None), barred)
            }
            Answer::Refused {
                upstream_status,
                request_id,
                status,
                message,
            } => {
                let refused = json!({ "kind": "refused", "status": status, "message": message });
                (None, *upstream_status, request_id, (&None, &None), refused)
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
        if let Some(model) = model {
            event["model"] = json!(model);
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
            Entry::ModelAnswered(request, answered) => self.model_answered(request, *answered),
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
    ModelAnswered((Caller, u64), Box<ModelAnswer>),
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
        cache_write_tokens,
        reasoning_tokens,
    } = usage;
    let mut json = json!({ "input_tokens": input_tokens, "output_tokens": output_tokens });
    if let Some(cached) = cached_tokens {
        json["cached_tokens"] = json!(cached);
    }
    if let Some(written) = cache_write_tokens {
        json["cache_write_tokens"] = json!(written);
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
            // The command's output files in the record hold the same bytes.
            stdout: _,
            stderr: _,
        } => json!({
            "kind": "command_ended",
            "lease": lease.0,
            "command": command,
            "end": end,
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
        Action::Reply { op: _, reply } => {
            // A poll's reply says how a job or a lease stood, which the job's and the lease's own
            // events in the log say too.
            let reply = match reply {
                OpReply::Status(_) => json!({ "kind": "status" }),
                OpReply::LeaseStatus(_) => json!({ "kind": "lease_status" }),
                OpReply::Ops(_)
                | OpReply::Judge(_)
                | OpReply::Submit(_)
                | OpReply::Lease(_)
                | OpReply::Refused { .. } => json!(reply),
            };
            json!({ "kind": "reply", "reply": reply })
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
            argv: _,
            timeout,
        } => json!({
            "kind": "run_command",
            "lease": lease.0,
            "command": command,
            "timeout_ns": timeout.map(nanos),
        }),
        Action::Transfer {
            lease,
            holder: _,
            transfer,
            direction,
            max_bytes,
        } => {
            let direction = match direction {
                decision::Direction::Put { replace: true, .. } => Direction::Replace,
                decision::Direction::Put { replace: false, .. } => Direction::Put,
                decision::Direction::Get { .. } => Direction::Get,
            };
            json!({
                "kind": "transfer",
                "lease": lease.0,
                "transfer": transfer,
                "direction": direction,
                "max_bytes": max_bytes,
            })
        }
        Action::CloseLease { lease } => json!({ "kind": "close_lease", "lease": lease.0 }),
        Action::TearDownAgent => json!({ "kind": "tear_down_agent" }),
        // `outcome.json` holds the outcome.
        Action::Finish { outcome: _ } => json!({ "kind": "finish" }),
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
