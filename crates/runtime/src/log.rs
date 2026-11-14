//! The run log, `log.jsonl`, and the JSON forms of events, actions and outcomes.
//!
//! `log.jsonl` holds one JSON record per line, each timed and synced as [`Log`] writes it. A
//! record is one of:
//!
//! - an event of the core's: `{"seq": n, "at_ns": "t", "observed_ns": "o", "event": e,
//!   "actions": [a, ...]}`, with the actions the core decided on it, which it took at `o`, when
//!   the runtime observed it: what the agent's session handed over as it reached the host, a
//!   judge call's events as its thread saw them, so `o` may be before an earlier record's `t`.
//!   The first, `seq` 0, is the run's start, `{"kind": "run_started", "unix_ns": "u"}`,
//!   observed at 0, `u` nanoseconds after the Unix epoch by the driving machine's clock;
//! - one that no core decides on: an `op_refused` ([`Log::refused`]) or a `model_answered`
//!   ([`Log::model_answered`]), `{"seq": n, "at_ns": "t", "event": e, "actions": []}`, or a
//!   note, `{"seq": n, "at_ns": "t", "note": s}`, such as why the run failed.
//!
//! `seq` counts records from 0. Each event and action is an object whose `kind` is its name in
//! snake case, such as `{"kind": "judge_exited", "job": 1, "status": {"code": 0}}`. Grants appear
//! by their names in the workflow, jobs and ops by their numbers, payloads by size and digest,
//! and times and durations in nanoseconds as decimal strings, where `u64::MAX` stands for
//! anything longer. A status's job, a number the agent gave, is a decimal string too. A judge
//! call's `start_judge` names the agent's grant it was made under, or `null` for the pattern's
//! score. A reply is the [`OpReply`](schema::sandbox::op::OpReply) sent.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use decision::{
    Action, Event, Failure, JobId, JudgeResult, Outcome, PayloadRef, Request, Submission, Verdict,
    WorkEnd,
};
use gateway::RefusedOp;
use route::Answer;
use schema::workflow::{JudgeCall, Workflow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use stdx::Context;

use super::{Clock, Durable, Host, record};

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

    /// Appends the record of the run's start, observed at 0, `unix` after the Unix epoch, with the
    /// core's first `actions`.
    pub(super) fn start(
        &mut self,
        workflow: &Workflow,
        unix: Duration,
        actions: &[Action],
    ) -> Result<(), String> {
        let event = json!({ "kind": "run_started", "unix_ns": nanos(unix) });
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

    /// Appends the record of `answer`, how the route answered the agent's model request
    /// numbered `request`: `{"kind": "model_answered", "request": n, "upstream_status": u,
    /// "answer": a}`, with `"request_id": i` if the upstream named its id for the request, where
    /// `u` is the upstream's status, or `null` if no response came, and `a` the [`Answer`]'s kind,
    /// with a refusal's or a bar's status and message, but never a body.
    pub(super) fn model_answered(&mut self, request: u64, answer: &Answer) -> Result<(), String> {
        let (upstream_status, request_id, answer) = match answer {
            Answer::Forwarded { status, request_id } => {
                (Some(*status), request_id, json!({ "kind": "forwarded" }))
            }
            Answer::Cut { status, request_id } => {
                (Some(*status), request_id, json!({ "kind": "cut" }))
            }
            Answer::Barred { status, message } => {
                let barred = json!({ "kind": "barred", "status": status, "message": message });
                (None, &None, barred)
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
        self.told(event)
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
        let mut record = json!({ "observed_ns": nanos(observed), "actions": actions });
        record["event"] = event;
        self.append(record)?;
        if leads { self.sync() } else { Ok(()) }
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

    /// Makes every record written so far last.
    pub(super) fn sync(&self) -> Result<(), String> {
        (self.host)
            .sync(&self.file, Durable::Data)
            .context(self.path.display())
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
        Action::StartAgent => json!({ "kind": ActionKind::StartAgent }),
        Action::Reply { op, reply } => {
            json!({ "kind": ActionKind::Reply, "op": op.0, "reply": reply })
        }
        Action::StartJudge {
            job,
            call,
            payload,
            timeout,
        } => json!({
            "kind": ActionKind::StartJudge,
            "job": job.0,
            "grant": match call {
                JudgeCall::Grant(grant) => Some(workflow.judge_grant(*grant).name.as_str()),
                JudgeCall::Score => None,
            },
            "payload": payload_json(payload),
            "timeout_ns": nanos(*timeout),
        }),
        Action::StopJudge { job } => json!({ "kind": ActionKind::StopJudge, "job": job.0 }),
        Action::TearDownAgent => json!({ "kind": ActionKind::TearDownAgent }),
        Action::Finish { outcome } => {
            json!({ "kind": ActionKind::Finish, "outcome": outcome_json(outcome) })
        }
        Action::Abandon => json!({ "kind": ActionKind::Abandon }),
    }
}

/// The kind of an action, as the run log names it ([`action_json`]).
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ActionKind {
    StartAgent,
    Reply,
    StartJudge,
    StopJudge,
    TearDownAgent,
    Finish,
    Abandon,
}

/// A record of the run log, as [`final_judge`] reads it: its actions' kinds, jobs and grants.
// Structs, whose unread fields serde_json skips without nesting into them: a judge's result nested
// past its limit, which a `Value` or an internally tagged enum would refuse, doesn't stop the read.
#[derive(Deserialize)]
struct Logged {
    #[serde(default)]
    actions: Vec<LoggedAction>,
}

/// An action, as [`Logged`] reads it.
#[derive(Deserialize)]
struct LoggedAction {
    kind: ActionKind,
    job: Option<u64>,
    grant: Option<String>,
}

/// The final judge's job: the one that the run log of the record in `dir` names in its
/// `start_judge` without a grant.
///
/// # Errors
///
/// Fails if the log can't be read, has a line that is no record of it, or names no such job.
pub(super) fn final_judge(dir: &Path) -> Result<JobId, String> {
    let path = dir.join("log.jsonl");
    let log = File::open(&path).context(path.display())?;
    for (index, line) in BufReader::new(log).lines().enumerate() {
        let line = line.context(path.display())?;
        let logged: Logged = serde_json::from_str(&line).context(format_args!(
            "{}, line {}",
            path.display(),
            index + 1
        ))?;
        for action in logged.actions {
            if let LoggedAction {
                kind: ActionKind::StartJudge,
                job: Some(job),
                grant: None,
            } = action
            {
                return Ok(JobId(job));
            }
        }
    }
    Err(format!(
        "{} names no final judge: expected a `start_judge` without a grant",
        path.display()
    ))
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
