//! The run log, `log.jsonl`, and the JSON forms of events, actions and outcomes.
//!
//! `log.jsonl` holds one JSON record per line, each written and synced before what it records
//! is acted on, but for a `model_answered`: the loop writes it once it takes it, which may be
//! after the answer it records was sent, though before what the agent does next. A record is
//! one of:
//!
//! - an event: `{"seq": n, "at_ns": "t", "event": e, "actions": [a, ...]}`, which the runtime
//!   observed `t` nanoseconds after the run started, with the actions the core decided on. A
//!   judge call's events, and its notes, are at the times its thread observed them, which may
//!   come before an earlier record's, as the loop takes them later. The
//!   first, `seq` 0, is the run's start, `{"kind": "run_started", "unix_ns": "u"}`, at `u`
//!   nanoseconds after the Unix epoch by the driving machine's clock, with the core's first
//!   actions. Like it, an op the gateway refused, since its payload was over the agent's limit,
//!   is no event of the core's, which neither decides on it nor learns of it, so it has no
//!   actions: `{"kind": "op_refused", "request": r, "message": m}`, where the request's payload
//!   is `{"size": s, "over": n}`, its size and the limit it was over, in bytes, without a
//!   digest, since the gateway kept none of its bytes, and the message is the refusal the agent
//!   is sent, unless a note after it says it wasn't. So is how the route answered a model
//!   request of the agent's, at the time the request's thread observed it, as a judge call's
//!   events are: `{"kind": "model_answered", "request": n, "upstream_status": u, "answer": a}`,
//!   with `"request_id": i` besides if the upstream's response named its id for the request, in
//!   `x-request-id` or `x-oai-request-id`,
//!   where `u` is the status of the upstream's response, or `null` if none came, and `a` is
//!   `{"kind": "forwarded"}` for that response whole, `{"kind": "cut"}` for it cut short, or
//!   `{"kind": "refused", "status": s, "message": m}` for the route's refusal sent in its place,
//!   without either's body;
//! - a note: `{"seq": n, "at_ns": "t", "note": s}`, something the runtime observed that the
//!   core doesn't decide on, such as why the run failed.
//!
//! `seq` counts records from 0. Each event and action is an object whose `kind` is its name in
//! snake case, such as `{"kind": "judge_exited", "job": 1, "status": {"code": 0}}`. Grants appear
//! by their names in the workflow, jobs and ops by their numbers, payloads by size and digest, and
//! times and durations in nanoseconds, the core's resolution, as decimal strings, where `u64::MAX`
//! (about 584 years) stands for anything longer. A status's job, a number the agent gave, is a
//! decimal string too. A judge call's `start_judge` names the agent's grant it was made under,
//! or `null` for the pattern's score. A reply is the
//! [`OpReply`](pumpkin_schema::sandbox::op::OpReply) sent.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use pumpkin_decision::{
    Action, Event, Failure, JudgeResult, Outcome, PayloadRef, Request, Submission, Verdict, WorkEnd,
};
use pumpkin_gateway::RefusedOp;
use pumpkin_schema::workflow::{JudgeCall, Workflow};
use serde_json::{Value, json};
use stdx::Context;

use super::record;
use crate::model_route::Answer;

/// The run log, open for appending.
#[derive(Debug)]
pub(super) struct Log {
    path: PathBuf,
    file: File,
    seq: u64,
}

impl Log {
    /// Creates the log, `log.jsonl`, in the record's directory `dir`, where it must not exist,
    /// and syncs `dir`, so that the log lasts as each record synced into it does.
    pub(super) fn create(dir: &Path) -> Result<Self, String> {
        let path = dir.join("log.jsonl");
        let file = OpenOptions::new()
            .append(true)
            .create_new(true)
            .open(&path)
            .context(path.display())?;
        record::sync_dir(dir)?;
        Ok(Self { path, file, seq: 0 })
    }

    /// Appends the record of the run's start, at time 0, `unix` after the Unix epoch, with the
    /// core's first `actions`.
    pub(super) fn start(
        &mut self,
        workflow: &Workflow,
        unix: Duration,
        actions: &[Action],
    ) -> Result<(), String> {
        let event = json!({ "kind": "run_started", "unix_ns": nanos(unix) });
        self.record(workflow, Duration::ZERO, event, actions)
    }

    /// Appends the record of `event`, observed at `at`, and the `actions` it called for.
    pub(super) fn event(
        &mut self,
        workflow: &Workflow,
        at: Duration,
        event: &Event,
        actions: &[Action],
    ) -> Result<(), String> {
        self.record(workflow, at, event_json(event), actions)
    }

    /// Appends the record of `refused`, an op of the agent's whose payload the gateway refused,
    /// observed at `at`: an `op_refused`, which has no actions, since the core doesn't decide on
    /// it.
    pub(super) fn refused(
        &mut self,
        workflow: &Workflow,
        at: Duration,
        refused: &RefusedOp,
    ) -> Result<(), String> {
        let mut request = json!(refused.call());
        let (size, over) = (refused.size(), refused.payload_limit().bytes());
        request["payload"] = json!({ "size": size, "over": over });
        let event = json!({
            "kind": "op_refused",
            "request": request,
            "message": refused.message(),
        });
        self.record(workflow, at, event, &[])
    }

    /// Appends the record of `answer`, how the route answered the agent's model request
    /// numbered `request`, observed at `at`: a `model_answered`, which has no actions, since the
    /// core doesn't decide on it.
    pub(super) fn model_answered(
        &mut self,
        workflow: &Workflow,
        at: Duration,
        request: u64,
        answer: &Answer,
    ) -> Result<(), String> {
        let (upstream_status, request_id, answer) = match answer {
            Answer::Forwarded { status, request_id } => {
                (Some(*status), request_id, json!({ "kind": "forwarded" }))
            }
            Answer::Cut { status, request_id } => {
                (Some(*status), request_id, json!({ "kind": "cut" }))
            }
            Answer::Refused {
                upstream_status,
                request_id,
                status,
                message,
            } => {
                let refused = json!({ "kind": "refused", "status": status, "message": message });
                (*upstream_status, request_id, refused)
            }
        };
        let mut event = json!({
            "kind": "model_answered",
            "request": request,
            "upstream_status": upstream_status,
            "answer": answer,
        });
        if let Some(request_id) = request_id {
            event["request_id"] = json!(request_id);
        }
        self.record(workflow, at, event, &[])
    }

    fn record(
        &mut self,
        workflow: &Workflow,
        at: Duration,
        event: Value,
        actions: &[Action],
    ) -> Result<(), String> {
        let actions: Vec<Value> = actions
            .iter()
            .map(|action| action_json(workflow, action))
            .collect();
        let mut record = json!({ "actions": actions, "at_ns": nanos(at) });
        record["event"] = event;
        self.append(record)
    }

    /// Appends a note.
    pub(super) fn note(&mut self, at: Duration, note: &str) -> Result<(), String> {
        self.append(json!({ "note": note, "at_ns": nanos(at) }))
    }

    fn append(&mut self, mut record: Value) -> Result<(), String> {
        record["seq"] = self.seq.into();
        let mut line = record.to_string();
        line.push('\n');
        self.file
            .write_all(line.as_bytes())
            .context(self.path.display())?;
        self.file.sync_data().context(self.path.display())?;
        self.seq += 1;
        Ok(())
    }
}

/// `duration` in nanoseconds, as the log gives times and durations.
fn nanos(duration: Duration) -> String {
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
        Event::OpArrived { op, request } => json!({
            "kind": "op_arrived",
            "op": op.0,
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
    }
}

/// The JSON form of an action.
fn action_json(workflow: &Workflow, action: &Action) -> Value {
    match action {
        Action::StartAgent => json!({ "kind": "start_agent" }),
        Action::Reply { op, reply } => json!({ "kind": "reply", "op": op.0, "reply": reply }),
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
        Action::TearDownAgent => json!({ "kind": "tear_down_agent" }),
        Action::Finish { outcome } => json!({ "kind": "finish", "outcome": outcome_json(outcome) }),
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
        "passed": result.passed(),
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
