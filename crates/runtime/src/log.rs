//! The run log, `log.jsonl` in a run's record, and its readers. `ARCHITECTURE.md` says what the log
//! holds and what it leaves to the record's other files.
//!
//! The log is one JSON object a line, appended as the run goes and never changed; a line counts
//! once its newline is there. A line has `seq`, its number from 0, and `at_ns`, when it was
//! written, which never decreases; and then either `note`, a sentence for the operator, or `event`,
//! an object whose `kind` names it, with `observed_ns`, when it happened, and `actions`, what the
//! runtime did about it, in order. A thread that observed an event hands it over to be written, so
//! its `observed_ns` may be before an earlier line's `at_ns`. Events that the runtime tells and the
//! core never decided, such as a sandbox's deletion, have no `observed_ns`. The first line is
//! `run_started`, observed at 0.
//!
//! Every `..._ns` is a string of decimal digits: nanoseconds since the run started, but the
//! `unix_ns` of `run_started`, since the Unix epoch by the driving machine's clock; `u64::MAX`
//! stands for anything longer. Grants appear by name, jobs, ops and leases by number, payloads by
//! size and digest, and a `caller` or `holder` as `agent` or `job-<n>`.
//!
//! A line that leads to an act, an event with actions or an `op_refused`, is synced with all before
//! it ahead of the act; any other lasts with the next sync. The run's end syncs the log before
//! `outcome.json`, so a crash cuts a lost run's log only after its last synced line.
//!
//! A reader takes a line only once it ends in a newline, skips an event or action `kind` it doesn't
//! know, lists a part it can't read as unread rather than failing, and takes as optional what an
//! older log lacks: `observed_ns`, `hard_end_ns`, `first_token_ns`, `model` and `kept`.

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
pub use usage::{Cost, PlaceUse, Priced, TokensCost, Used};
pub(super) use usage::{usage_of, used};

pub(super) struct Log<'h, H: Host> {
    path: PathBuf,
    file: File,
    seq: u64,
    host: &'h H,
    clock: H::Clock,
}

impl<'h, H: Host> Log<'h, H> {
    pub(super) fn create(host: &'h H, clock: H::Clock, dir: &Path) -> Result<Self, String> {
        let path = dir.join("log.jsonl");
        let file = OpenOptions::new()
            .append(true)
            .create_new(true)
            .open(&path)
            .context(path.display())?;
        // So that the log lasts as each record synced into it does.
        record::sync_dir(host, dir)?;
        Ok(Self {
            path,
            file,
            seq: 0,
            host,
            clock,
        })
    }

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

    pub(super) fn event(
        &mut self,
        workflow: &Workflow,
        observed: Duration,
        event: &Event,
        actions: &[Action],
    ) -> Result<(), String> {
        self.decided(workflow, observed, event_json(event), actions)
    }

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

    /// Writes the `model_answered` event of a request: its `caller`, `request` number, the
    /// `upstream` and `api` of the entry that took it, if one did, `upstream_status`, null if no
    /// response came, and `model`, `request_id`, `usage` and `failed` (with the `code` of its
    /// failure, if it gave one), each if there is one. `answer` is named by its `kind`: `forwarded`
    /// or `cut` short, for an upstream's response; `barred`, refused before an upstream; `refused`,
    /// in place of the upstream's response; the last two with `status` and `message`, never a body.
    /// `first_byte_ns` is when a response began to arrive, and `first_token_ns` when its output
    /// did; `kept` says what the exchange left in the record's `model/`.
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
        // A line whose actions the runtime carries out next lasts before it does.
        let leads = !actions.is_empty();
        self.observed(observed, event, &actions)?;
        if leads { self.sync() } else { Ok(()) }
    }

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

    fn told(&mut self, event: Value) -> Result<(), String> {
        let mut record = json!({ "actions": [] });
        record["event"] = event;
        self.append(record)
    }

    pub(super) fn note(&mut self, note: &str) -> Result<(), String> {
        self.append(json!({ "note": note }))
    }

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

pub(super) enum Entry {
    Note(String),
    /// A model request going upstream, at a time, as `model_request_started`; a request that the
    /// route refuses itself never goes upstream, and has none.
    ModelRequestStarted((Caller, u64), Duration),
    ModelAnswered((Caller, u64), Box<ModelAnswer>),
    /// The GPUs that a sandbox's `sandboxd` saw as it started, as `sandbox_gpus`: the sandbox's
    /// name in the run, and each GPU's model as `nvidia-smi` names it. A sandbox whose `sandboxd`
    /// saw none, or couldn't tell, has none.
    Gpus(Name, Vec<GpuModel>),
    /// A sandbox's delete, which ended it, as `sandbox_deleted`: its name in the run, `agent`,
    /// `job-<n>` or `lease-<n>`, or null for one that no start of the run recorded, which the run's
    /// end found left; and its provider and id. Every sandbox's end is one, on every path, a stop
    /// short of the core's end included, after which the log keeps the notes and deletes of the
    /// run's children, but no event of theirs.
    Deleted(Deleted),
}

pub(super) struct ModelAnswer {
    /// The name and API of the upstream entry that took the request, if one did, even if the route
    /// barred the request itself and never sent it upstream.
    pub(super) taken: Option<(Name, Api)>,
    pub(super) answer: Answer,
    pub(super) kept: Value,
    /// When the response's first bytes came and, if its stream's output began, when that did;
    /// `None` if no byte came.
    pub(super) first: Option<(Duration, Option<Duration>)>,
    /// When the answer ended.
    pub(super) ended: Duration,
}

/// A response's usage: `input_tokens` and `output_tokens`, and `cached_tokens`,
/// `cache_write_tokens` and `reasoning_tokens` if it reported them.
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

/// `duration` as the log gives times and durations: its whole nanoseconds, as decimal digits, at
/// most `u64::MAX`.
pub(super) fn nanos(duration: Duration) -> String {
    // A decimal string, since a reader that reads JSON numbers as doubles rounds integers past
    // 2^53, and a time since the Unix epoch in nanoseconds is past 2^53.
    u64::try_from(duration.as_nanos())
        .unwrap_or(u64::MAX)
        .to_string()
}

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

/// How a run finished, for `outcome.json`: `score`, null if it has none; `work_end`, which is
/// `deadline`, `finalized`, `agent_exited` with its `status`, or `agent_failed` with its `reason`;
/// and `verdict`, which is `no_submission`, `judged` or `no_result`, the last two with the
/// `submission` (`payload`, `finalized`) and the `result` (`score`, `returned`) or `failure`
/// (`timeout`, `error` or `invalid_result`) that judging gave. Each of them is named by its `kind`.
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
