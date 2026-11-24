//! The run log, `log.jsonl` in a run's record, and its readers. The log holds each fact once. It
//! leaves a command's output, an op's payload, a model exchange's bodies and the outcome to the
//! record's other files.
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
//! size and digest, and a `caller` or `holder` as `agent-<role index>` or `job-<n>`.
//!
//! The runtime syncs a line and all preceding lines before acting on it, including delivering a
//! runner receipt to a body. Other lines last with the next sync. The run's end syncs the log
//! before `outcome.json`, so a crash cuts a lost run's log only after its last synced line.
//!
//! A reader takes a line only once it ends in a newline, skips an event or action `kind` it doesn't
//! know, and counts any part it can't read so the operator can find it.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::time::Duration;

use decision::{
    Action, Caller, Effect, Evaluated, Event, JobId, Outcome, PayloadRef, Request, Unevaluated,
};
use gateway::RefusedOp;
use provider::UnixTime;
use route::{Answer, Bar, Usage};
use schema::ir::{AbsolutePath, Api, Argv, CompanionName, GpuModel, Name};
use schema::sandbox::op::{CommandEnd, OpReply, SessionOp};
use schema::workflow::{AgentId, TargetId, Workflow};
use serde::Deserialize;
use serde_json::{Value, json};
use stdx::Context;
use stdx::time::Clock as _;

use super::sandbox::Deleted;
use super::{Durable, Host, Stopped, record};

mod call;
mod fold;
mod logged;
pub(super) mod outcome;
mod progress;
mod roles;
mod usage;

pub use call::{CallEnd, CallOf, CallSpan, CallStart};
pub use fold::{LogFold, StartedOver, TakenLine};
pub(super) use fold::{final_call, lines, progress};
pub use logged::{CallerName, Direction};
pub(super) use progress::places;
pub use progress::{
    AgentEnd, AgentSpan, Closed, LeaseSpan, ModelUse, Part, Place, Progress, Readout, Sandbox,
    SharedSpan, Tokens, Unread, Work,
};
pub(super) use roles::ir_in;
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
        self.decided(workflow, Duration::ZERO, event, actions, false)
    }

    pub(super) fn event(
        &mut self,
        workflow: &Workflow,
        observed: Duration,
        event: &Event,
        actions: &[Action],
        delivers_receipt: bool,
    ) -> Result<(), String> {
        self.decided(
            workflow,
            observed,
            event_json(event),
            actions,
            delivers_receipt,
        )
    }

    pub(super) fn refused(&mut self, caller: Caller, refused: &RefusedOp) -> Result<(), String> {
        let mut request = json!(refused.call());
        let (size, over) = (refused.size(), refused.payload_limit().bytes());
        request["payload"] = json!({ "size": size, "over": over });
        let event = json!({
            "kind": "op_refused",
            "caller": logged::CallerName::from(caller),
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
    /// or `cut` short, for an upstream's response; `barred` before an upstream, by its `bar`
    /// (`rules`, `ended` or `budget`), or `refused` in place of the upstream's response, each with
    /// `status` and `message`, never a body; `abandoned`, given up on as the route ended, once
    /// handed to the upstream, with its `model`. `first_byte_ns` is when a response began to
    /// arrive, `first_token_ns` when its output did, and `kept` what the exchange left in `model/`.
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
            Answer::Barred {
                bar,
                status,
                message,
            } => {
                let bar = match bar {
                    Bar::Rules => "rules",
                    Bar::Ended => "ended",
                    Bar::Budget => "budget",
                };
                let barred = json!({
                    "kind": "barred",
                    "bar": bar,
                    "status": status,
                    "message": message,
                });
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
            Answer::Abandoned { model } => {
                let abandoned = json!({ "kind": "abandoned" });
                (Some(model), None, &None, (&None, &None), abandoned)
            }
        };
        let mut event = json!({
            "kind": "model_answered",
            "caller": logged::CallerName::from(caller),
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
            Entry::AgentCollected(id) => self.told(agent_event("agent_collected", id)),
            Entry::AgentLaunched(id, at) => {
                self.observed(at, agent_event("agent_started", id), &[])
            }
            Entry::SharedStarted { target, at } => self.observed(
                at,
                json!({
                    "kind": "shared_started", "shared": target.role().index(),
                }),
                &[],
            ),
            Entry::ModelRequestStarted((caller, request), started) => {
                let event = json!({
                    "kind": "model_request_started",
                    "caller": logged::CallerName::from(caller),
                    "request": request,
                });
                self.observed(started, event, &[])
            }
            Entry::ModelAnswered(request, answered) => self.model_answered(request, *answered),
            Entry::Gpus(sandbox, gpus) => {
                self.told(json!({ "kind": "sandbox_gpus", "sandbox": sandbox, "gpus": gpus }))
            }
            Entry::Target(job, targeted, at) => {
                let job = job.0;
                let event = match targeted {
                    Targeted::Put { path, put } => json!({
                        "kind": "target_put", "job": job, "path": path,
                        "digest": put.digest, "bytes": put.size,
                    }),
                    Targeted::Run {
                        command,
                        argv,
                        stdin,
                    } => {
                        let mut event = json!({
                            "kind": "target_run", "job": job, "command": command, "argv": argv,
                        });
                        if let Some(stdin) = stdin {
                            event["stdin"] = payload_json(&stdin);
                        }
                        event
                    }
                    Targeted::Ended { command, end } => json!({
                        "kind": "command_ended", "job": job, "command": command, "end": end,
                    }),
                    Targeted::Get { path, got } => json!({
                        "kind": "target_get", "job": job, "path": path,
                        "digest": got.digest, "bytes": got.size,
                    }),
                };
                self.observed(at, event, &[])
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
        delivers_receipt: bool,
    ) -> Result<(), String> {
        let actions: Vec<Value> = actions
            .iter()
            .map(|action| action_json(workflow, action))
            .collect();
        // A line whose actions the runtime carries out next lasts before it does.
        let leads = delivers_receipt || !actions.is_empty();
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
    /// The agent's sandbox frozen and what it left collected, as `agent_collected`: the run is
    /// done with it.
    AgentCollected(AgentId),
    AgentLaunched(AgentId, Duration),
    SharedStarted {
        target: TargetId,
        at: Duration,
    },
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
    /// What a job function's body asked of its call's target, job `job`'s, at a time.
    Target(JobId, Targeted, Duration),
}

/// What a body asked of its target, as the log's `target_put`, `target_run`, `command_ended` and
/// `target_get` hold it.
pub(super) enum Targeted {
    /// The call's payload is now at `path` in the target; `put` names it by its size and digest.
    Put { path: AbsolutePath, put: PayloadRef },
    /// A command has started, the body's `command`th, whose output the job's `log/<command>`
    /// holds once it exits.
    Run {
        command: u64,
        argv: Argv,
        stdin: Option<PayloadRef>,
    },
    /// The body's `command`th command exited, as `end` says; one that its call's end cut short
    /// has none.
    Ended { command: u64, end: CommandEnd },
    /// The body got the file at `path`, which `got` names by its size and digest.
    Get { path: AbsolutePath, got: PayloadRef },
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

fn agent_event(kind: &str, agent: AgentId) -> Value {
    json!({"kind": kind, "caller": logged::CallerName::Agent(agent.role().index())})
}

#[expect(
    clippy::too_many_lines,
    reason = "one arm per decision event, as in action_json"
)]
fn event_json(event: &Event) -> Value {
    match event {
        Event::WorkStarted => json!({ "kind": "work_started" }),
        Event::AgentTornDown { agent } => agent_event("agent_torn_down", *agent),
        Event::AgentExited { agent, status } => {
            let mut event = agent_event("agent_exited", *agent);
            event["status"] = json!(status);
            event
        }
        Event::AgentFailed { agent, reason } => {
            let mut event = agent_event("agent_failed", *agent);
            event["reason"] = json!(reason);
            event
        }
        Event::SharedFailed { target, reason } => json!({
            "kind": "shared_failed", "shared": target.role().index(), "reason": reason,
        }),
        Event::SharedTornDown { target } => {
            json!({ "kind": "shared_torn_down", "shared": target.role().index() })
        }
        Event::OpArrived {
            op,
            caller,
            request,
        } => json!({
            "kind": "op_arrived",
            "op": op.0,
            "caller": logged::CallerName::from(*caller),
            "request": request_json(request),
        }),
        Event::CallStarted { job } => json!({ "kind": "call_started", "job": job.0 }),
        Event::CallTimedOut { job } => json!({ "kind": "call_timed_out", "job": job.0 }),
        Event::CallTornDown { job } => json!({ "kind": "call_torn_down", "job": job.0 }),
        Event::CallReturned {
            job,
            result,
            model_requests_over_budget,
        } => with_budget_count(
            json!({
                "kind": "call_returned", "job": job.0, "returned": shown_json(result.values()),
            }),
            *model_requests_over_budget,
        ),
        Event::CallFailed {
            job,
            reason,
            model_requests_over_budget,
        } => with_budget_count(
            json!({ "kind": "call_failed", "job": job.0, "reason": reason }),
            *model_requests_over_budget,
        ),
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

// Gives a call's end `event` its count of model requests over budget, if it has one.
fn with_budget_count(mut event: Value, model_requests_over_budget: Option<NonZeroU64>) -> Value {
    if let Some(count) = model_requests_over_budget {
        event["model_requests_over_budget"] = json!(count);
    }
    event
}

// A now function's evaluation: its effects and what it returned, or why it gave neither.
fn evaluated_json(evaluated: &Result<Evaluated, Unevaluated>) -> Value {
    match evaluated {
        Ok(Evaluated { effects, returned }) => {
            let effects: Vec<&str> = (effects.iter())
                .map(|effect| match effect {
                    Effect::SetSubmission => "set_submission",
                    Effect::EndWork => "end_work",
                })
                .collect();
            json!({ "effects": effects, "returned": shown_json(returned.values()) })
        }
        Err(Unevaluated::Unread(words)) => json!({ "unread": words }),
        Err(Unevaluated::Failed(reason)) => json!({ "failed": reason }),
    }
}

fn request_json(request: &Request) -> Value {
    match request {
        Request::Ops => json!({ "kind": "ops" }),
        Request::Call {
            function,
            payload,
            arguments,
        } => {
            let mut call = json!({
                "kind": "call",
                "grant": function.as_str(),
                "payload": payload_json(payload),
            });
            if !arguments.is_empty() {
                call["arguments"] = json!(arguments);
            }
            call
        }
        Request::Now {
            function,
            payload,
            evaluated,
        } => json!({
            "kind": "now",
            "grant": function.as_str(),
            "payload": payload_json(payload),
            "evaluated": evaluated_json(evaluated),
        }),
        // A decimal string, as a time is, since the agent may give any number, past 2^53 too.
        Request::Status { job } => json!({ "kind": "status", "job": job.to_string() }),
        Request::Session { grant, op } => session_json(grant, op),
    }
}

fn session_json(grant: &Name, op: &SessionOp) -> Value {
    let grant = grant.as_str();
    match op {
        SessionOp::Open { instance_seconds } => json!({
            "kind": "lease_open",
            "grant": grant,
            "instance_seconds": instance_seconds,
        }),
        SessionOp::Run {
            lease,
            argv,
            timeout_seconds,
            container,
        } => in_container(
            json!({
                "kind": "lease_run",
                "grant": grant,
                "lease": lease,
                "argv": argv,
                "timeout_seconds": timeout_seconds,
            }),
            container.as_ref(),
        ),
        SessionOp::Put {
            lease,
            tree,
            to,
            replace,
            container,
        } => in_container(
            json!({
                "kind": "lease_put",
                "grant": grant,
                "lease": lease,
                "tree": tree,
                "to": to,
                "replace": replace,
            }),
            container.as_ref(),
        ),
        SessionOp::Get {
            lease,
            from,
            to,
            container,
        } => in_container(
            json!({
                "kind": "lease_get",
                "grant": grant,
                "lease": lease,
                "from": from,
                "to": to,
            }),
            container.as_ref(),
        ),
        SessionOp::Status { lease } => {
            json!({ "kind": "lease_status", "grant": grant, "lease": lease })
        }
        SessionOp::Close { lease } => {
            json!({ "kind": "lease_close", "grant": grant, "lease": lease })
        }
    }
}

/// `op`, a lease's op, with the runner's companion that it names, if it names one.
fn in_container(mut op: Value, container: Option<&CompanionName>) -> Value {
    if let Some(container) = container {
        op["container"] = json!(container);
    }
    op
}

#[expect(clippy::too_many_lines, reason = "one arm per action")]
fn action_json(workflow: &Workflow, action: &Action) -> Value {
    match action {
        Action::StartAgents => json!({ "kind": "start_agents" }),
        Action::Reply { op: _, reply } => {
            // A poll's reply says how a job or a lease stood, which the job's and the lease's own
            // events in the log say too.
            let reply = match reply {
                OpReply::Status(_) => json!({ "kind": "status" }),
                OpReply::LeaseStatus(_) => json!({ "kind": "lease_status" }),
                OpReply::Ops(_)
                | OpReply::Call(_)
                | OpReply::Lease(_)
                | OpReply::Refused { .. } => json!(reply),
            };
            json!({ "kind": "reply", "reply": reply })
        }
        Action::StartCall {
            job,
            function,
            payload,
            // The call's request holds them, as its caller gave them.
            arguments: _,
            timeout,
        } => {
            let grant = workflow.job_function(*function);
            json!({
                "kind": "start_call", "job": job.0, "grant": grant.name,
                "caller": logged::CallerName::Agent(grant.holder.role().index()),
                "payload": payload_json(payload), "timeout_ns": nanos(*timeout),
            })
        }
        Action::StartFinalCall {
            job,
            selections,
            timeout,
        } => {
            let selections: Vec<_> = selections.iter().map(|(agent, selected)| {
                json!({"role": workflow.sandbox(agent.role()).name, "selection": selected.as_ref().map(payload_json)})
            }).collect();
            json!({"kind":"start_final_call", "job":job.0, "selections":selections, "timeout_ns":nanos(*timeout)})
        }
        Action::StopCall { job } => json!({ "kind": "stop_call", "job": job.0 }),
        Action::StartLease {
            lease,
            holder,
            grant,
            instance,
            start_by,
        } => json!({
            "kind": "start_lease",
            "lease": lease.0,
            "holder": logged::CallerName::from(*holder),
            "grant": workflow.session(*grant).name.as_str(),
            "instance": instance,
            "start_by_ns": nanos(*start_by),
        }),
        Action::RunCommand {
            lease,
            command,
            argv: _,
            timeout,
            // The op that made the action says it.
            container: _,
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
            // The op that made the action says it.
            container: _,
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
        Action::TearDownAgent { agent } => agent_event("tear_down_agent", *agent),
        // `outcome.json` holds the outcome.
        Action::TearDownShared { target } => {
            json!({ "kind": "tear_down_shared", "shared": target.role().index() })
        }
        Action::Finish { outcome: _ } => json!({ "kind": "finish" }),
        Action::Abandon => json!({ "kind": "abandon" }),
    }
}

/// How a run ended, for `outcome.json`, in the form of [`outcome::Ended`].
pub(super) fn outcome_json(workflow: &Workflow, ended: &Result<Outcome, Stopped>) -> Value {
    json!(outcome::Ended::of(workflow, ended))
}

/// `values` in order, each as `{"name": …, "value": …}`.
fn shown_json(values: &[(Name, Value)]) -> Value {
    let values = values.iter();
    let values: Vec<Value> = values
        .map(|(name, value)| json!({ "name": name, "value": value }))
        .collect();
    json!(values)
}

/// The values that `returned`, a body's return as the log's `call_returned` holds it,
/// gives, each by its name, in order.
///
/// # Errors
///
/// Fails if `returned` is not an array of objects that each hold a `name`, a [`Name`], and a
/// `value`.
pub fn returned_values(returned: &Value) -> Result<Vec<(Name, Value)>, serde_json::Error> {
    let values = Vec::<logged::Shown>::deserialize(returned)?;
    Ok(values
        .into_iter()
        .map(|shown| (shown.name, shown.value))
        .collect())
}

fn payload_json(payload: &PayloadRef) -> Value {
    let PayloadRef { size, digest } = payload;
    json!({ "size": size, "digest": digest.as_str() })
}
