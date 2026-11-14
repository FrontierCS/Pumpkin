//! The run log, `log.jsonl`, and the JSON forms of events, actions and outcomes.
//!
//! `log.jsonl` holds one JSON record per line, each timed and synced as [`Log`] writes it. A
//! record is one of:
//!
//! - an event of the core's: `{"seq": n, "at_ns": "t", "observed_ns": "o", "event": e,
//!   "actions": [a, ...]}`, with the actions the core decided on it, which it took at `o`, when
//!   the runtime observed it: what the agent's session handed over as it reached the host, a
//!   judge call's events as its thread saw them, so `o` may be before an earlier record's `t`.
//!   The first, `seq` 0, is the run's start, `{"kind": "run_started", "unix_ns": "u",
//!   "hard_end_ns": "h"}`, observed at 0, `u` nanoseconds after the Unix epoch by the driving
//!   machine's clock, whose hard end is `h` after it;
//! - one that no core decides on: an `op_refused` ([`Log::refused`]) or an [`Entry`], `{"seq": n,
//!   "at_ns": "t", "event": e, "actions": []}`, or a note, `{"seq": n, "at_ns": "t", "note": s}`,
//!   such as why the run failed.
//!
//! `seq` counts records from 0. Each event and action is an object whose `kind` is its name in
//! snake case, such as `{"kind": "judge_exited", "job": 1, "status": {"code": 0}}`. Grants appear
//! by their names in the workflow, jobs, ops and leases by their numbers, payloads by size and
//! digest, and times and durations in nanoseconds as decimal strings, where `u64::MAX` stands for
//! anything longer; a status's job, a number the agent gave, is a decimal string too. A `caller`
//! or `holder` is as [`caller_json`] gives it. `start_judge` names the agent's grant of the call,
//! or `null` for the score, `start_lease` the lease's grant; a `transfer` gives its paths, and
//! its `transfer_ended` its sizes, never what it moved. A reply is the
//! [`OpReply`](schema::sandbox::op::OpReply) sent.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use decision::{
    Action, Caller, Direction, Event, Failure, JudgeResult, Outcome, PayloadRef, Request,
    Submission, Verdict, WorkEnd,
};
use gateway::RefusedOp;
use provider::UnixTime;
use route::Answer;
use schema::ir::{GpuModel, Name};
use schema::workflow::{JudgeCall, Workflow};
use serde_json::{Value, json};
use stdx::Context;

use super::sandbox::Deleted;
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

    /// Appends the record of `answer`, how `caller`'s route answered its model request numbered
    /// `request`: `{"kind": "model_answered", "caller": c, "request": n, "upstream_status": u,
    /// "answer": a}`, with `"request_id": i` if the upstream named its id for the request, where
    /// `u` is the upstream's status, or `null` if no response came, and `a` the [`Answer`]'s kind,
    /// with a refusal's or a bar's status and message, but never a body.
    fn model_answered(
        &mut self,
        (caller, request): (Caller, u64),
        answer: &Answer,
    ) -> Result<(), String> {
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
            "caller": caller_json(caller),
            "request": request,
            "upstream_status": upstream_status,
            "answer": answer,
        });
        if let Some(request_id) = request_id {
            event["request_id"] = json!(request_id);
        }
        self.told(event)
    }

    /// Appends `entry`, as [`Entry`] says.
    pub(super) fn entry(&mut self, entry: Entry) -> Result<(), String> {
        match entry {
            Entry::Note(note) => self.note(&note),
            Entry::ModelAnswered(request, answer) => self.model_answered(request, &answer),
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

/// A record of the run log that no core decides on, which the thread that observed it hands the
/// loop, and the loop writes as it takes it, at that time ([`Log`]).
pub(super) enum Entry {
    /// A note ([`Log::note`]).
    Note(String),
    /// How a route answered a model request of the caller's, by its number
    /// ([`Log::model_answered`]).
    ModelAnswered((Caller, u64), Answer),
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
            "caller": caller_json(*caller),
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

/// The JSON form of an op's caller, or a lease's holder: its sandbox's name in the run, `agent`,
/// or `job-<n>` for job `n`'s call.
fn caller_json(caller: Caller) -> String {
    match caller {
        Caller::Agent => "agent".to_owned(),
        Caller::Job(job) => format!("job-{}", job.0),
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
            "holder": caller_json(*holder),
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
                Direction::Put { tree, to, replace } if *replace => ("replace", tree, to),
                Direction::Put { tree, to, .. } => ("put", tree, to),
                Direction::Get { from, to } => ("get", from, to),
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

/// Takes each record of the run log of the record in `dir` in turn, by `take`; false if there is
/// no log. A last line without its newline, one being written or that a crash cut short, is left
/// out.
///
/// # Errors
///
/// Fails if the log can't be read, or a whole line of it isn't JSON, which no log that a run
/// wrote holds.
fn records(dir: &Path, mut take: impl FnMut(&Value)) -> Result<bool, String> {
    let path = dir.join("log.jsonl");
    let log = match File::open(&path) {
        Ok(log) => log,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context(path.display()),
    };
    let mut log = BufReader::new(log);
    let mut line = Vec::new();
    for number in 1.. {
        line.clear();
        log.read_until(b'\n', &mut line).context(path.display())?;
        let Some(whole) = line.strip_suffix(b"\n") else {
            break;
        };
        let record: Value = serde_json::from_slice(whole)
            .context(format_args!("{}, line {number}", path.display()))?;
        take(&record);
    }
    Ok(true)
}

/// The final judge's job: the one that the run log of the record in `dir` names in its
/// `start_judge` without a grant.
///
/// # Errors
///
/// Fails as [`records`] does, or if the log names no such job.
pub(super) fn final_judge(dir: &Path) -> Result<u64, String> {
    let mut job = None;
    let logged = records(dir, |record| {
        let actions = record["actions"].as_array().into_iter().flatten();
        let mut started = actions.filter(|action| action["kind"] == "start_judge");
        let started = started.find(|action| action["grant"].is_null());
        job = job.or_else(|| started?["job"].as_u64());
    })?;
    let path = dir.join("log.jsonl");
    match (logged, job) {
        (_, Some(job)) => Ok(job),
        (false, None) => Err(format!(
            "{} doesn't exist: expected a run's log",
            path.display()
        )),
        (true, None) => Err(format!(
            "{} names no final judge: expected a `start_judge` without a grant",
            path.display()
        )),
    }
}
