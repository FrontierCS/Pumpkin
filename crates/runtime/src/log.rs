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

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use decision::{
    Action, Caller, Direction, Event, Failure, JudgeResult, Outcome, PayloadRef, Request,
    Submission, Verdict, WorkEnd,
};
use gateway::RefusedOp;
use provider::UnixTime;
use route::{Answer, Usage};
use schema::ir::{Gpu, GpuModel, Name};
use schema::workflow::{JudgeCall, Workflow};
use serde::Deserialize;
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
    /// "answer": a}`, with `"request_id": i` if the upstream named its id for the request,
    /// `"usage"` if its response reported one ([`Usage`]), and `"failed": {"code": c}` if its
    /// response stated that it failed, without `"code"` if it gave none ([`route::Failure`]),
    /// where `u` is the upstream's status, or `null` if no response came, and `a` the
    /// [`Answer`]'s kind, with a refusal's or a bar's status and message, but never a body.
    fn model_answered(
        &mut self,
        (caller, request): (Caller, u64),
        answer: &Answer,
    ) -> Result<(), String> {
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
            "caller": caller_json(caller),
            "request": request,
            "upstream_status": upstream_status,
            "answer": answer,
        });
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

/// Where a role's sandboxes are: a provider of the run's, and a class it offers, as
/// `provider.json` names them.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Place {
    pub provider: String,
    pub class: String,
}

/// What a run's record says of it so far, as [`Record::progress`](crate::Record::progress)
/// reads its log, with the bounds of its workflow and of its roles' classes. Each time is since
/// the run started, as the log gives times.
#[derive(Debug, Default)]
pub struct Progress {
    /// When the run started, since the Unix epoch, by the driving machine's clock.
    pub started: Duration,
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

/// A sandbox of the run, whatever started it, from the core's start of it, at `asked`.
#[derive(Debug, Default)]
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
#[derive(Debug, Default)]
pub struct AgentSpan {
    pub sandbox: Sandbox,
    /// Whether its role has a model route.
    pub routed: bool,
    /// When it exited or failed, and how.
    pub end: Option<(Duration, AgentEnd)>,
}

/// How the agent ended: its command's exit, with its status as the log gives it, or its failure.
#[derive(Debug)]
pub enum AgentEnd {
    Exited(Value),
    Failed(String),
}

/// A judge call, job `job`.
#[derive(Debug)]
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
#[derive(Debug)]
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
    pub last: Option<LeaseCommand>,
    /// Why it failed, if it did.
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

/// A command or a transfer a lease ran, and how it ended, as the log gives it, once it has.
#[derive(Debug)]
pub struct LeaseCommand {
    pub work: Work,
    pub end: Option<Value>,
}

/// What a lease ran: a command's `argv`, or a transfer, `put` into the runner, `replace` for a
/// put that replaces its destination, or `get` out of it, from a path to a path.
#[derive(Debug)]
pub enum Work {
    Command(Vec<String>),
    Transfer {
        direction: String,
        from: String,
        to: String,
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

    /// Ends its busy time at `at`, if it is busy, or at its charge's end, if that came first.
    fn rest(&mut self, at: Duration) {
        if let Some(since) = self.working.take() {
            self.busy += self.until(at).saturating_sub(since);
        }
    }
}

/// How a judge call ended: with a result, which has a score, the JSON number, or not, when it
/// is what the judge returned, as JSON; with a result that was invalid; or with a failure.
#[derive(Debug)]
pub enum CallEnd {
    Scored(String),
    Unscored(String),
    Invalid(String),
    Failed(String),
}

/// The run's model requests, of every role with a route, as its routes answered them.
#[derive(Debug, Default)]
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
    /// When the last good one was answered: forwarded whole, with a 2xx status, and its stream
    /// stating no failure.
    pub last_good: Option<Duration>,
    /// The tokens of those whose responses reported their usage, summed.
    pub tokens: Tokens,
}

/// Tokens summed over the responses that reported them: `reported` is how many did, and each
/// `Option` count is `None` if none of those gave it.
#[derive(Debug, Default)]
pub struct Tokens {
    pub reported: u64,
    pub input: u64,
    pub output: u64,
    pub cached: Option<u64>,
    pub reasoning: Option<u64>,
}

/// Takes each record of the run log of the record in `dir` in turn, by `take`; false if there is
/// no log. A last line without its newline, one being written or that a crash cut short, is left
/// out.
///
/// # Errors
///
/// Fails if the log can't be read, or a whole line of it isn't JSON, which no log that a run
/// wrote holds: a judge's result nests no deeper than a line can hold
/// ([`RESULT_MAX_DEPTH`](schema::sandbox::RESULT_MAX_DEPTH)).
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

/// Reads what the record in `dir` says of its run so far ([`Progress`]); `None` if its run log
/// doesn't record the run's start yet.
///
/// # Errors
///
/// Fails if a file can't be read, or as [`records`] does.
pub(super) fn progress(dir: &Path) -> Result<Option<Progress>, String> {
    let ir = json_file(&dir.join("ir.json"))?;
    let provided = json_file(&dir.join("provider.json"))?;
    let roles = Roles::of(&ir, &provided);
    let mut reading = Reading::default();
    if !records(dir, |record| reading.take(record, &roles))? {
        return Ok(None);
    }
    // The run makes its log, then logs its start in it.
    let Some((started, hard_end)) = reading.started else {
        return Ok(None);
    };
    let mut progress = reading.progress;
    progress.started = started;
    progress.hard_end = hard_end;
    progress.agent.sandbox = Sandbox {
        started: progress.agent.sandbox.started,
        gone: progress.agent.sandbox.gone,
        deleted: deleted(&dir.join("agent"))?,
        gpus: progress.agent.sandbox.gpus.take(),
        ..roles.sandbox(Some(&roles.agent_role), Duration::ZERO)
    };
    progress.agent.routed = roles.routed;
    progress.deadline = roles.deadline;
    progress.prices.clone_from(&roles.prices);
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
    Ok(Some(progress))
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

/// A log as far as it has been read.
#[derive(Default)]
struct Reading {
    /// When the run started, since the Unix epoch, and its hard end, once its start is read.
    started: Option<(Duration, Duration)>,
    progress: Progress,
}

impl Reading {
    /// Takes in `record`, a line of the log of a run whose roles are `roles`.
    fn take(&mut self, record: &Value, roles: &Roles) {
        let written = time(&record["at_ns"]).unwrap_or_default();
        self.progress.last = self.progress.last.max(written);
        let at = when(record);
        self.event(at, &record["event"]);
        let event = &record["event"];
        let actions = record["actions"].as_array().into_iter().flatten();
        let closed = if event["request"]["kind"] == "lease_close" {
            Closed::Asked
        } else if actions
            .clone()
            .any(|action| action["kind"] == "tear_down_agent")
        {
            Closed::WorkEnded
        } else {
            Closed::Expired
        };
        // A judge call's end closes the leases it holds.
        let kind = event["kind"].as_str().unwrap_or_default();
        let ended = (kind.starts_with("judge_"))
            .then(|| event["job"].as_u64())
            .flatten();
        let progress = &mut self.progress;
        if let Some(note) = record["note"].as_str()
            && note.starts_with("collected from the agent's sandbox")
        {
            progress.agent.sandbox.gone = Some(at);
        }
        for action in actions {
            progress.action(at, action, (closed, ended), roles);
            // A holder may ask, at open, for a shorter life than its grant's.
            let asked = event["request"]["instance_seconds"].as_u64();
            if let (Some(seconds), "start_lease") = (asked, action["kind"].as_str().unwrap_or(""))
                && let Some(lease) = lease(progress, action)
            {
                let asked = Duration::from_secs(seconds);
                lease.ready_for = Some(lease.ready_for.map_or(asked, |most| most.min(asked)));
            }
        }
    }

    /// Takes in `event`, observed at `at`.
    #[expect(clippy::too_many_lines, reason = "one arm per kind of event")]
    fn event(&mut self, at: Duration, event: &Value) {
        let progress = &mut self.progress;
        match event["kind"].as_str() {
            Some("run_started") => {
                self.started = time(&event["unix_ns"]).zip(time(&event["hard_end_ns"]));
            }
            Some("agent_started") => progress.agent.sandbox.started = Some(at),
            Some("agent_exited") => {
                progress.agent.end = Some((at, AgentEnd::Exited(event["status"].clone())));
            }
            Some("agent_failed") => {
                let reason = event["reason"].as_str().unwrap_or_default().to_owned();
                progress.agent.end = Some((at, AgentEnd::Failed(reason)));
                progress.agent.sandbox.gone.get_or_insert(at);
            }
            Some("judge_started") => {
                if let Some(call) = job(progress, event) {
                    call.sandbox.started = Some(at);
                }
            }
            Some("judge_timed_out") => {
                if let Some(call) = job(progress, event) {
                    call.timed_out = true;
                }
            }
            Some("judge_collected") => {
                let end = match (&event["result"], &event["invalid"]) {
                    (result, _) if result["score"].is_number() => {
                        CallEnd::Scored(result["score"].to_string())
                    }
                    (result, _) if result.is_object() => {
                        CallEnd::Unscored(result["returned"].to_string())
                    }
                    (_, invalid) => CallEnd::Invalid(invalid.as_str().unwrap_or("").to_owned()),
                };
                if let Some(call) = job(progress, event) {
                    call.end(at, end);
                }
            }
            Some("judge_failed") => {
                let reason = event["reason"].as_str().unwrap_or_default().to_owned();
                if let Some(call) = job(progress, event) {
                    call.end(at, CallEnd::Failed(reason));
                }
            }
            Some("lease_ready") => {
                if let Some(lease) = lease(progress, event) {
                    lease.sandbox.started = Some(at);
                }
            }
            Some("command_ended" | "transfer_ended") => {
                let Some(lease) = lease(progress, event) else {
                    return;
                };
                if let Some(last) = &mut lease.last {
                    last.end = Some(event["end"].clone());
                    lease.rest(at);
                }
            }
            Some(kind @ ("lease_ended" | "lease_failed")) => {
                if let Some(lease) = lease(progress, event) {
                    lease.sandbox.gone = Some(at);
                    let reason = event["reason"].as_str().unwrap_or_default();
                    lease.failed = (kind == "lease_failed").then(|| reason.to_owned());
                }
            }
            Some("model_answered") => model_answered(progress, at, event),
            Some("sandbox_gpus") => {
                if let Some(sandbox) = progress.named(event["sandbox"].as_str()) {
                    let gpus = event["gpus"].as_array().into_iter().flatten();
                    let gpus = gpus.map(|gpu| gpu.as_str().unwrap_or_default().to_owned());
                    sandbox.gpus = Some(gpus.collect());
                }
            }
            Some("sandbox_deleted") => {
                if let Some(sandbox) = progress.named(event["sandbox"].as_str()) {
                    sandbox.gone.get_or_insert(at);
                }
            }
            _ => {}
        }
    }
}

impl CallSpan {
    fn end(&mut self, at: Duration, end: CallEnd) {
        self.end = Some(end);
        self.sandbox.gone = Some(at);
    }
}

impl Progress {
    /// The sandbox that `name` names within the run, `agent`, `job-<n>` or `lease-<n>`, if the
    /// core started it.
    fn named(&mut self, name: Option<&str>) -> Option<&mut Sandbox> {
        let name = name?;
        if name == "agent" {
            return Some(&mut self.agent.sandbox);
        }
        if let Some(job) = name.strip_prefix("job-") {
            let job: u64 = job.parse().ok()?;
            let call = self.calls.iter_mut().find(|call| call.job == job)?;
            return Some(&mut call.sandbox);
        }
        let lease: u64 = name.strip_prefix("lease-")?.parse().ok()?;
        let leased = self.leases.iter_mut().find(|span| span.lease == lease)?;
        Some(&mut leased.sandbox)
    }

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

/// Takes in `answered`, a `model_answered` event at `at`: the run's, and its caller's, if a judge
/// call made it.
fn model_answered(progress: &mut Progress, at: Duration, answered: &Value) {
    progress.model.take(at, answered);
    let caller = answered["caller"].as_str();
    let job = caller.and_then(|caller| caller.strip_prefix("job-")?.parse().ok());
    if let Some(call) = progress.calls.iter_mut().find(|call| Some(call.job) == job) {
        call.model.take(at, answered);
    }
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

/// The call of the job that `record`, an event or an action, names, if the core started it.
fn job<'p>(progress: &'p mut Progress, record: &Value) -> Option<&'p mut CallSpan> {
    let job = record["job"].as_u64()?;
    progress.calls.iter_mut().find(|call| call.job == job)
}

/// Notes that the lease that `action` names ran `work`, which is its last, from `at`.
fn lease_ran(progress: &mut Progress, at: Duration, action: &Value, work: Work) {
    if let Some(lease) = lease(progress, action) {
        lease.commands += 1;
        lease.last = Some(LeaseCommand { work, end: None });
        lease.working = Some(at);
    }
}

/// The lease that `record`, an event or an action, names, if the core opened it.
fn lease<'p>(progress: &'p mut Progress, record: &Value) -> Option<&'p mut LeaseSpan> {
    let lease = record["lease"].as_u64()?;
    progress.leases.iter_mut().find(|span| span.lease == lease)
}

impl Progress {
    /// Takes in `action`, which the core decided on at `at`, in a run whose roles are `roles`;
    /// a lease it closes is `closed`.
    fn action(
        &mut self,
        at: Duration,
        action: &Value,
        (closed, ended): (Closed, Option<u64>),
        roles: &Roles,
    ) {
        let progress = self;
        match action["kind"].as_str() {
            Some("start_judge") => {
                let grant = action["grant"].as_str().map(str::to_owned);
                let role = match &grant {
                    Some(grant) => {
                        let key = (roles.agent_role.clone(), grant.clone());
                        roles.grants.get(&key).cloned().unwrap_or_default().role
                    }
                    None => roles.score_role.clone(),
                };
                progress.calls.push(CallSpan {
                    job: action["job"].as_u64().unwrap_or_default(),
                    grant,
                    sandbox: roles.sandbox(role.as_deref(), at),
                    role,
                    timeout: time(&action["timeout_ns"]).unwrap_or_default(),
                    stopped: false,
                    timed_out: false,
                    end: None,
                    stderr: None,
                    evidence: None,
                    model: ModelUse::default(),
                });
            }
            Some("stop_judge") => {
                if let Some(call) = job(progress, action) {
                    call.stopped = true;
                }
            }
            Some("start_lease") => progress.open_lease(at, action, roles),
            Some("run_command") => {
                let argv = action["argv"].as_array().into_iter().flatten();
                let argv = argv.map(|arg| arg.as_str().unwrap_or_default().to_owned());
                lease_ran(progress, at, action, Work::Command(argv.collect()));
            }
            Some("transfer") => {
                let text = |key: &str| action[key].as_str().unwrap_or_default().to_owned();
                let transfer = Work::Transfer {
                    direction: text("direction"),
                    from: text("from"),
                    to: text("to"),
                };
                lease_ran(progress, at, action, transfer);
            }
            Some("close_lease") => {
                if let Some(lease) = lease(progress, action) {
                    let closed = match lease.holder {
                        Some(holder) if ended == Some(holder) => Closed::CallEnded,
                        _ => closed,
                    };
                    lease.closed.get_or_insert((at, closed));
                }
            }
            Some("tear_down_agent") => {
                progress.work_end.get_or_insert(at);
            }
            Some("finish" | "abandon") => progress.end = Some(at),
            _ => {}
        }
    }

    /// Takes in `action`, a `start_lease` the core decided on at `at`, in a run whose roles are
    /// `roles`.
    fn open_lease(&mut self, at: Duration, action: &Value, roles: &Roles) {
        let grant = action["grant"].as_str().unwrap_or_default().to_owned();
        // `agent`, or `job-<n>`.
        let holder = action["holder"]
            .as_str()
            .and_then(|holder| holder.strip_prefix("job-")?.parse::<u64>().ok());
        let call = holder.and_then(|holder| self.calls.iter().find(|call| call.job == holder));
        let role = match call {
            Some(call) => call.role.clone().unwrap_or_default(),
            None => roles.agent_role.clone(),
        };
        let granted = roles.grants.get(&(role, grant.clone())).cloned();
        let Grant { role, ready_for } = granted.unwrap_or_default();
        self.leases.push(LeaseSpan {
            lease: action["lease"].as_u64().unwrap_or_default(),
            grant,
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
}

impl ModelUse {
    /// Takes in `answered`, a `model_answered` event at `at`.
    fn take(&mut self, at: Duration, answered: &Value) {
        self.requests += 1;
        let status = &answered["upstream_status"];
        let failed = &answered["failed"];
        if failed.is_object() {
            let code = failed["code"].as_str().map(str::to_owned);
            *self.failed.entry(code).or_default() += 1;
        }
        let whole_2xx = answered["answer"]["kind"] == "forwarded"
            && status
                .as_u64()
                .is_some_and(|status| (200..300).contains(&status));
        if !whole_2xx {
            self.not_2xx += 1;
        } else if !failed.is_object() {
            self.last_good = Some(at);
        }
        let answer = match answered["answer"]["kind"].as_str() {
            Some("forwarded") => status.to_string(),
            Some("cut") => format!("{status} cut"),
            Some(other) => other.to_owned(),
            None => "unknown".to_owned(),
        };
        *self.answers.entry(answer).or_default() += 1;
        let usage = &answered["usage"];
        if usage.is_object() {
            let tokens = &mut self.tokens;
            tokens.reported += 1;
            tokens.input += usage["input_tokens"].as_u64().unwrap_or_default();
            tokens.output += usage["output_tokens"].as_u64().unwrap_or_default();
            if let Some(cached) = usage["cached_tokens"].as_u64() {
                *tokens.cached.get_or_insert(0) += cached;
            }
            if let Some(reasoning) = usage["reasoning_tokens"].as_u64() {
                *tokens.reasoning.get_or_insert(0) += reasoning;
            }
        }
    }
}

/// When `record` happened, as the log's readers time it: when the core took its event, its
/// `observed_ns`, if it has one, and else when the log wrote it, its `at_ns`.
fn when(record: &Value) -> Duration {
    let when = time(&record["observed_ns"]).or_else(|| time(&record["at_ns"]));
    when.unwrap_or_default()
}

/// A time or duration that the log gives in nanoseconds, as a decimal string.
fn time(value: &Value) -> Option<Duration> {
    value.as_str()?.parse().ok().map(Duration::from_nanos)
}
