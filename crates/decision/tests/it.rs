//! The driver's decision core, fed event scripts on [`workflow`], or on variants of it. Each
//! test shows what the core did after each event, and when it next wants a tick whenever that
//! changes. Each role is placed in a class whose sandboxes may take [`STARTUP`] to start, or
//! [`SLOW_STARTUP`] for a role that needs the CPU model `slow`, which sets the run's hard end.
//!
//! Each line gives milliseconds since the start and an event:
//!
//! - `work_started`, `agent_exited <code>`, `agent_failed <reason>`;
//! - `ops`, `call <function> <bytes> [<name>=<value>]…`, `now <function> <bytes> <effect>…|failed|
//!   unread` and `status <job>`, each an op from the agent, its payload named by its number;
//! - `<session>.open [<seconds>]`, `.run <lease> <seconds>|- <argv>...`, `.put <lease> <tree> <to>
//!   [replace]`, `.get <lease> <from> <to>`, `.status <lease>` and `.close <lease>`, each an op
//!   from the agent, `-` for a command without a timeout;
//! - an event after `@<role>`, from that agent, or an op after `@<job>`, from that call;
//! - `agent_torn_down`, `shared_failed <target> <reason>` and `shared_torn_down <target>`;
//! - `call_started <job>`, `call_timed_out <job>`, `call_returned <job> [+<n>] [<name>=<value>]…`,
//!   the body's return in its order, `call_failed <job> [+<n>] <reason>`, with `+<n>` if the route
//!   barred `n` of the call's model requests over budget, and `call_torn_down <job>`;
//! - `lease_ready <lease>`, `command_ended <lease> <command> <code>|timed_out|unstarted
//!   [<stdout>]`, `transfer_ended <lease> <transfer> moved <entries> <bytes>|failed <reason>`,
//!   `lease_ended <lease>`, `lease_failed <lease> <reason>`;
//! - `tick`;
//! - `model`, no event, but the question whether the agent's model requests are allowed then,
//!   or, after `@<job>`, that job's call's;
//! - `locked`, whether the agent ended its work with a call.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test helper fails the test that calls it"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::num::NonZeroU64;
use std::time::Duration;

use decision::{
    Action, CallResult, Caller, Core, Direction, Effect, Evaluated, Event, JobId, LeaseId, OpId,
    Outcome, PayloadRef, Request, Submission, Unevaluated,
};
use expect_test::{Expect, expect};
use schema::ir::{
    self, AbsolutePath, ArgumentValue, Argv, Container, CpuModel, Name, Sha256Digest,
};
use schema::sandbox::op::{CommandEnd, OpReply, REASON_BYTES, SessionOp, TransferEnd};
use schema::sandbox::{ExitStatus, RESULT_MAX_BYTES};
use schema::workflow::{Class, ContainerId, Offers, Workflow};
use serde_json::{Value, json};
use test_support::workflow;

/// How long a sandbox may take to start in class `fast`.
const STARTUP: Duration = Duration::from_secs(10);

/// How long a sandbox may take to start in class `slow`.
const SLOW_STARTUP: Duration = Duration::from_secs(30);

/// Runs `script` on [`workflow`] with `edit` applied, and compares what the core did
/// with `expect`.
fn check(edit: impl FnOnce(&mut Value), script: &str, expect: &Expect) {
    let mut ir = workflow();
    edit(&mut ir);
    let ir: ir::Workflow = serde_json::from_value(ir).unwrap();
    let offered = offered();
    let (placed, _) = Workflow::parse(&ir)
        .unwrap()
        .place(offered, |classes| classes)
        .unwrap();
    let mut run = Run {
        workflow: placed.workflow(),
        ops: 0,
        caller: Caller::Agent(placed.workflow().agents().next().unwrap().id),
        tick_due: None,
        transcript: String::new(),
    };
    let (mut core, actions) = Core::start(&placed);
    run.note("start", &actions, core.tick_due());
    for line in script
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let (at, event) = line.split_once(' ').unwrap();
        let at = Duration::from_millis(at.parse().unwrap());
        let (caller, event) = match event.strip_prefix('@') {
            Some(named) => {
                let (name, event) = named.split_once(' ').unwrap();
                (run.named_caller(name), event)
            }
            None => (run.default_caller(), event),
        };
        run.caller = caller;
        match event {
            "model" => {
                let allowed = if core.model_requests_allowed(caller, at) {
                    "allowed"
                } else {
                    "refused"
                };
                writeln!(run.transcript, "{line}\n  model requests {allowed}").unwrap();
            }
            "locked" => {
                let Caller::Agent(agent) = caller else {
                    panic!("only agents submit payloads")
                };
                writeln!(
                    run.transcript,
                    "{line}\n  submission locked: {}",
                    core.submission_locked(agent)
                )
                .unwrap();
            }
            event => {
                let event = run.event(event);
                let asked = match &event {
                    Event::OpArrived { op, .. } => Some(*op),
                    _ => None,
                };
                let actions = core.handle(at, event);
                assert_replies(line, asked, &actions);
                assert_lives(line, &actions);
                run.note(line, &actions, core.tick_due());
            }
        }
    }
    expect.assert_eq(&run.transcript);
}

fn assert_replies(line: &str, asked: Option<OpId>, actions: &[Action]) {
    let replied: Vec<OpId> = (actions.iter())
        .filter_map(|action| match action {
            Action::Reply { op, .. } => Some(*op),
            _ => None,
        })
        .collect();
    assert_eq!(
        replied,
        Vec::from_iter(asked),
        "`{line}`: an op gets exactly one reply, and no other event gets one"
    );
}

// Each lease that `actions` start lasts the life that its open's reply gives as its time left. The
// core counts that life from the lease's ready.
fn assert_lives(line: &str, actions: &[Action]) {
    let left = (actions.iter()).find_map(|action| match action {
        Action::Reply {
            reply: OpReply::Lease(reply),
            ..
        } => Some(Duration::from_secs(reply.remaining.instance_seconds)),
        _ => None,
    });
    for action in actions {
        if let Action::StartLease { life, .. } = action {
            assert_eq!(
                Some(*life),
                left,
                "`{line}`: a lease starts with a life other than the time its open's reply gives"
            );
        }
    }
}

/// The classes of one provider, each of which offers what every role of the fixture needs:
/// `fast`, and `slow`, which states the CPU model `slow`, so that only a role that needs that
/// model is placed in it.
fn offered() -> BTreeMap<Name, BTreeMap<Name, Class>> {
    let name = |name: &str| Name::try_from(name.to_owned()).unwrap();
    let class = |class: &str, startup: Duration, cpu_model: Option<CpuModel>| {
        let offers = Offers {
            cpu_millis: NonZeroU64::new(64_000).unwrap(),
            memory_bytes: NonZeroU64::new(1 << 40).unwrap(),
            gpu: None,
            cpu_features: BTreeSet::new(),
            cpu_model,
            companions: true,
        };
        let declared = Class {
            offers,
            startup_seconds: NonZeroU64::new(startup.as_secs()).unwrap(),
            usd_per_hour: None,
        };
        (name(class), declared)
    };
    let slow = CpuModel::try_from("slow".to_owned()).unwrap();
    let classes = [
        class("fast", STARTUP, None),
        class("slow", SLOW_STARTUP, Some(slow)),
    ];
    BTreeMap::from([(name("provider"), BTreeMap::from(classes))])
}

struct Run<'w> {
    workflow: &'w Workflow,
    ops: u64,
    /// Who makes the line's op: the agent, unless the line names a job's call.
    caller: Caller,
    /// When the core last said it next wants a tick, once it has.
    tick_due: Option<Duration>,
    transcript: String,
}

/// A job function's call's event, as the script's `words` give it.
fn call_event(words: &[&str]) -> Event {
    let job = JobId(words[1].parse().unwrap());
    let rest = words.get(2).copied().unwrap_or_default();
    let (model_requests_over_budget, rest) = match rest.strip_prefix('+') {
        Some(counted) => {
            let (count, rest) = counted.split_once(' ').unwrap_or((counted, ""));
            (Some(count.parse().unwrap()), rest)
        }
        None => (None, rest),
    };
    match words[0] {
        "call_started" => Event::CallStarted { job },
        "call_timed_out" => Event::CallTimedOut { job },
        "call_torn_down" => Event::CallTornDown { job },
        "call_returned" => {
            let returned = (rest.split(' '))
                .filter(|field| !field.is_empty())
                .map(|field| {
                    let (name, value) = field.split_once('=').unwrap();
                    let name = serde_json::from_value(json!(name)).unwrap();
                    (name, serde_json::from_str(value).unwrap())
                })
                .collect();
            let result = CallResult::returned_by(returned).unwrap();
            Event::CallReturned {
                job,
                result,
                model_requests_over_budget,
            }
        }
        "call_failed" => Event::CallFailed {
            job,
            reason: rest.to_owned(),
            model_requests_over_budget,
        },
        other => panic!("no call event is named `{other}`"),
    }
}

impl Run<'_> {
    fn named_caller(&self, name: &str) -> Caller {
        if let Ok(number) = name.parse() {
            Caller::Job(JobId(number))
        } else {
            let agent = self
                .workflow
                .agents()
                .find(|agent| self.workflow.sandbox(agent.id.role()).name.as_str() == name)
                .unwrap();
            Caller::Agent(agent.id)
        }
    }

    fn default_caller(&self) -> Caller {
        Caller::Agent(self.workflow.agents().next().unwrap().id)
    }

    fn shared_target(&self, name: &str) -> schema::workflow::TargetId {
        self.workflow
            .work_targets()
            .find(|id| self.workflow.sandbox(id.role()).name.as_str() == name)
            .unwrap()
    }

    fn event(&mut self, event: &str) -> Event {
        let words: Vec<&str> = event.splitn(3, ' ').collect();
        let code = |word: &str| ExitStatus::Code(word.parse().unwrap());
        if let Some((grant, op)) = words[0].split_once('.') {
            let grant = Name::try_from(grant.to_owned()).unwrap();
            let op = session_op(op, &words);
            return self.op(Request::Session { grant, op });
        }
        let agent = || match self.caller {
            Caller::Agent(agent) => agent,
            Caller::Job(_) => panic!("a job cannot send an agent event"),
        };
        match words[0] {
            "shared_failed" | "shared_torn_down" => {
                let target = self.shared_target(words[1]);
                if words[0] == "shared_failed" {
                    Event::SharedFailed {
                        target,
                        reason: words[2].to_owned(),
                    }
                } else {
                    Event::SharedTornDown { target }
                }
            }
            "work_started" => Event::WorkStarted,
            "agent_torn_down" => Event::AgentTornDown { agent: agent() },
            "agent_exited" => Event::AgentExited {
                agent: agent(),
                status: code(words[1]),
            },
            "agent_failed" => Event::AgentFailed {
                agent: agent(),
                reason: words[1..].join(" "),
            },
            "ops" => self.op(Request::Ops),
            "call" => {
                let words: Vec<&str> = words[1..].iter().flat_map(|w| w.split(' ')).collect();
                let (function, payload) = self.call(&words[..2]);
                let arguments = (words[2..].iter())
                    .map(|word| {
                        let (name, text) = word.split_once('=').unwrap();
                        (Name::try_from(name.to_owned()).unwrap(), text.to_owned())
                    })
                    .collect();
                self.op(Request::Call {
                    function,
                    payload,
                    arguments,
                })
            }
            "now" => self.now(&words[1..]),
            "status" => self.op(Request::Status {
                job: words[1].parse().unwrap(),
            }),
            kind if kind.starts_with("call_") => call_event(&words),
            kind if kind.starts_with("lease_") || kind.ends_with("_ended") => lease_event(&words),
            "tick" => Event::Tick,
            other => panic!("no event is named `{other}`"),
        }
    }

    /// The function and payload of a call: `<function> <bytes>`. The payload is named by the
    /// number the op will have.
    fn call(&self, words: &[&str]) -> (Name, PayloadRef) {
        let words: Vec<&str> = words.iter().flat_map(|w| w.split(' ')).collect();
        let payload = PayloadRef {
            size: words[1].parse().unwrap(),
            digest: Sha256Digest::try_from(format!("sha256:{:064x}", self.ops + 1)).unwrap(),
        };
        let grant = Name::try_from(words[0].to_owned()).unwrap();
        (grant, payload)
    }

    fn now(&mut self, words: &[&str]) -> Event {
        let words: Vec<&str> = words.iter().flat_map(|w| w.split(' ')).collect();
        let (function, payload) = self.call(&words[..2]);
        let effects = words[2..].iter().map(|word| match *word {
            "set_submission" => Ok(Effect::SetSubmission),
            "end_work" => Ok(Effect::EndWork),
            "failed" => Err(Unevaluated::Failed("the body failed".to_owned())),
            "unread" => Err(Unevaluated::Unread(
                "`--arg finalize` didn't read".to_owned(),
            )),
            other => panic!("no effect {other}"),
        });
        let effects: Result<Vec<Effect>, Unevaluated> = effects.collect();
        let evaluated = effects.map(|effects| Evaluated {
            effects,
            returned: CallResult::returned_by(vec![(Name::from_static("state"), "done".into())])
                .unwrap(),
        });
        self.op(Request::Now {
            function,
            payload,
            evaluated,
        })
    }

    fn op(&mut self, request: Request) -> Event {
        self.ops += 1;
        Event::OpArrived {
            op: OpId(self.ops),
            caller: self.caller,
            request,
        }
    }

    fn note(&mut self, event: &str, actions: &[Action], tick_due: Duration) {
        writeln!(self.transcript, "{event}").unwrap();
        for action in actions {
            let action = self.action(action);
            writeln!(self.transcript, "  {action}").unwrap();
        }
        if self.tick_due != Some(tick_due) {
            self.tick_due = Some(tick_due);
            // In milliseconds, as the script gives times, and the nanoseconds past them, if any.
            let due = match tick_due.subsec_nanos() % 1_000_000 {
                0 => format!("{}", tick_due.as_millis()),
                nanos => format!("{}.{nanos:06}", tick_due.as_millis()),
            };
            writeln!(self.transcript, "  (tick due at {due})").unwrap();
        }
    }

    #[expect(clippy::too_many_lines, reason = "one arm per action")]
    fn action(&self, action: &Action) -> String {
        match action {
            Action::StartAgents => "start agents".to_owned(),
            Action::Reply {
                op: OpId(op),
                reply:
                    reply @ (OpReply::Ops(_)
                    | OpReply::Call(_)
                    | OpReply::Status(_)
                    | OpReply::Lease(_)
                    | OpReply::LeaseStatus(_)),
            } => format!(
                "reply to op {op}: {}",
                serde_json::to_string(reply).unwrap()
            ),
            Action::Reply {
                op: OpId(op),
                reply: OpReply::Refused { message, busy },
            } => {
                let busy = if *busy { " (busy)" } else { "" };
                format!("refuse op {op}{busy}: {message}")
            }
            Action::StartFinalCall {
                job,
                selections,
                timeout,
            } => {
                let selections = selections
                    .iter()
                    .map(|(agent, selection)| {
                        let name = &self.workflow.sandbox(agent.role()).name;
                        let selection = selection
                            .as_ref()
                            .map_or_else(|| "missing".to_owned(), payload_name);
                        format!("{name}={selection}")
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "start call {} of final on {selections}, with a timeout of {timeout:?}",
                    job.0
                )
            }
            Action::StartCall {
                job,
                function,
                payload,
                arguments,
                timeout,
            } => {
                let name = &self.workflow.job_function(*function).name;
                let payload = payload_name(payload);
                let given: String = (arguments.iter())
                    .map(|(name, value)| match value {
                        ArgumentValue::Str(text) => format!(", {name}={text:?}"),
                        ArgumentValue::Int(int) => format!(", {name}={int}"),
                        ArgumentValue::Bool(bool) => format!(", {name}={bool}"),
                    })
                    .collect();
                format!(
                    "start call {} of {name} on {payload}{given}, with a timeout of {timeout:?}",
                    job.0
                )
            }
            Action::StopCall { job } => format!("stop call {}", job.0),
            Action::StartLease {
                lease,
                holder,
                grant,
                instance,
                start_by,
                life: _,
            } => format!(
                "open lease {} of {}{}, as instance {instance}, started by {}",
                lease.0,
                self.workflow.session(*grant).name,
                holder_text(*holder),
                start_by.as_millis()
            ),
            Action::RunCommand {
                lease,
                command,
                container,
                argv,
                timeout,
            } => format!(
                "run command {command} of lease {}{}, {}, with a timeout of {timeout:?}",
                lease.0,
                in_container(*container),
                json!(argv)
            ),
            Action::Transfer {
                lease,
                holder,
                transfer,
                container,
                direction,
                max_bytes,
            } => transfer_text(
                (*lease, *container, *holder),
                *transfer,
                direction,
                *max_bytes,
            ),
            Action::CloseLease { lease } => format!("close lease {}", lease.0),
            Action::TearDownShared { target } => format!(
                "tear down shared {}",
                self.workflow.sandbox(target.role()).name
            ),
            Action::TearDownAgent { agent } => {
                format!("tear down {}", self.workflow.sandbox(agent.role()).name)
            }
            Action::Finish { outcome } => {
                format!("finish: {}", outcome_text(outcome, self.workflow))
            }
            Action::Abandon => "abandon the run at its hard end".to_owned(),
        }
    }
}

/// Where in a lease's runner an action runs, as the action says it: nothing for `main`.
fn in_container(container: ContainerId) -> String {
    match container {
        ContainerId::Main => String::new(),
        ContainerId::Companion(id) => format!(" in companion {}", id.index()),
    }
}

/// Whose a lease is, as an action says it: nothing for the agent's.
fn holder_text(holder: Caller) -> String {
    match holder {
        Caller::Agent(_) => String::new(),
        Caller::Job(job) => format!(" for job {}", job.0),
    }
}

fn transfer_text(
    (lease, container, holder): (LeaseId, ContainerId, Caller),
    transfer: u64,
    direction: &Direction,
    max: NonZeroU64,
) -> String {
    let (verb, from, to) = match direction {
        Direction::Put {
            tree,
            to,
            replace: false,
        } => ("put", tree, to),
        Direction::Put {
            tree,
            to,
            replace: true,
        } => ("replace with", tree, to),
        Direction::Get { from, to } => ("get", from, to),
    };
    format!(
        "{verb} {from} to {to} as transfer {transfer} of lease {}{}{}, of at most {max} bytes",
        lease.0,
        in_container(container),
        holder_text(holder)
    )
}

fn lease_event(words: &[&str]) -> Event {
    let lease = || LeaseId(words[1].parse().unwrap());
    let code = |word: &str| ExitStatus::Code(word.parse().unwrap());
    match words[0] {
        "transfer_ended" => {
            let mut ended = words[2].splitn(3, ' ');
            let transfer = ended.next().unwrap().parse().unwrap();
            let end = match (ended.next().unwrap(), ended.next().unwrap()) {
                ("moved", sizes) => {
                    let (entries, bytes) = sizes.split_once(' ').unwrap();
                    TransferEnd::Moved {
                        entries: entries.parse().unwrap(),
                        bytes: bytes.parse().unwrap(),
                    }
                }
                ("failed", reason) => TransferEnd::Failed(reason.to_owned()),
                (other, _) => panic!("no transfer ends `{other}`"),
            };
            Event::TransferEnded {
                lease: lease(),
                transfer,
                end,
            }
        }
        "lease_ready" => Event::LeaseReady { lease: lease() },
        "command_ended" => {
            let mut ended = words[2].splitn(3, ' ');
            let command = ended.next().unwrap().parse().unwrap();
            let end = match ended.next().unwrap() {
                "timed_out" => CommandEnd::TimedOut,
                "unstarted" => CommandEnd::Unstarted,
                exited => CommandEnd::Exited(code(exited)),
            };
            Event::CommandEnded {
                lease: lease(),
                command,
                end,
                stdout: ended.next().unwrap_or_default().to_owned(),
                stderr: String::new(),
            }
        }
        "lease_ended" => Event::LeaseEnded { lease: lease() },
        "lease_failed" => Event::LeaseFailed {
            lease: lease(),
            reason: words[2].to_owned(),
        },
        other => panic!("no lease event is named `{other}`"),
    }
}

/// The op `op` of a session, whose arguments are `words` after the first. A command or a copy in a
/// container other than `main` names its lease as `<lease>:<container>`.
fn session_op(op: &str, words: &[&str]) -> SessionOp {
    let named = words.get(1).copied().unwrap_or_default();
    let (lease, container) = named.split_once(':').unwrap_or((named, "main"));
    let lease = || lease.parse().unwrap();
    let container = || Container::try_from(container.to_owned()).unwrap();
    match op {
        "open" => SessionOp::Open {
            instance_seconds: words.get(1).map(|seconds| seconds.parse().unwrap()),
        },
        "run" => {
            let (timeout, argv) = words[2].split_once(' ').unwrap();
            let argv = argv.split(' ').map(str::to_owned).collect::<Vec<_>>();
            SessionOp::Run {
                lease: lease(),
                container: container(),
                argv: Argv::try_from(argv).unwrap(),
                timeout_seconds: timeout.parse().ok(),
            }
        }
        "put" | "get" => {
            let (from, to) = words[2].split_once(' ').unwrap();
            let (to, replace) = to.split_once(' ').unwrap_or((to, ""));
            let (from, to) = (path(from), path(to));
            if op == "put" {
                SessionOp::Put {
                    lease: lease(),
                    container: container(),
                    tree: from,
                    to,
                    replace: replace == "replace",
                }
            } else {
                SessionOp::Get {
                    lease: lease(),
                    container: container(),
                    from,
                    to,
                }
            }
        }
        "status" => SessionOp::Status { lease: lease() },
        "close" => SessionOp::Close { lease: lease() },
        other => panic!("no session op is named `{other}`"),
    }
}

fn path(path: &str) -> AbsolutePath {
    AbsolutePath::try_from(path.to_owned()).unwrap()
}

/// A payload by the number of the op that sent it, which its test digest holds.
fn payload_name(payload: &PayloadRef) -> String {
    let digest = payload.digest.hex();
    let op = u64::from_str_radix(digest, 16).unwrap();
    format!("op {op}'s payload of {} bytes", payload.size)
}

fn outcome_text(outcome: &Outcome, workflow: &Workflow) -> String {
    let agents = outcome
        .agents
        .iter()
        .map(|agent| {
            let name = &workflow.sandbox(agent.agent.role()).name;
            let selection = agent
                .submission
                .as_ref()
                .map_or_else(|| "missing".to_owned(), submission_text);
            format!("{name}: {:?}, {selection}", agent.work_end)
        })
        .collect::<Vec<_>>()
        .join("\n  ");
    let result = match &outcome.result {
        Ok(result) => {
            let values = result
                .values()
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>();
            format!("returned [{}]", values.join(" "))
        }
        Err(failure) => format!("failed: {failure:?}"),
    };
    format!("{agents}\n  final {result}")
}

fn submission_text(submission: &Submission) -> String {
    let kind = if submission.finalized {
        "finalized"
    } else {
        "stored"
    };
    format!("{kind} {}", payload_name(&submission.payload))
}

#[test]
fn whole_run() {
    check(
        |_| {},
        r#"
        0 work_started
        1000 ops
        2000 call train 10240
        2100 status 0
        2500 call_started 0
        3000 status 0
        9600 call_returned 0 raw_score=0 metrics={"solved":0}
        9600 call_torn_down 0
        10000 status 0
        11000 now submit 10240 set_submission
        12000 now submit 10240 set_submission end_work
        12100 agent_exited 0
        12100 agent_torn_down
        13500 call_started 1
        20100 call_returned 1 score=0 metrics={"solved":0}
        20100 call_torn_down 1
        "#,
        &expect![[r#"
            start
              start agents
              (tick due at 18010000)
            0 work_started
              (tick due at 18000000)
            1000 ops
              reply to op 1: {"kind":"ops","role":"agent","remaining_seconds":17999,"resources":{"cpu_millis":8000,"memory_bytes":34359738368},"grants":[{"kind":"job","name":"train","payload_max_bytes":4194304,"calls":100,"cumulative_seconds":12000,"call_timeout_seconds":12000},{"kind":"now","name":"submit","payload_max_bytes":4194304,"arguments":{"finalize":{"type":"bool","default":false}}}]}
            2000 call train 10240
              reply to op 2: {"kind":"call","job":0,"remaining":{"calls":99,"seconds":12000}}
              start call 0 of train on op 2's payload of 10240 bytes, with a timeout of 12000s
            2100 status 0
              reply to op 3: {"kind":"status","job":0,"state":"queued","remaining":{"calls":99,"seconds":12000}}
            2500 call_started 0
            3000 status 0
              reply to op 4: {"kind":"status","job":0,"state":"running","remaining":{"calls":99,"seconds":12000}}
            9600 call_returned 0 raw_score=0 metrics={"solved":0}
            9600 call_torn_down 0
            10000 status 0
              reply to op 5: {"kind":"status","job":0,"state":"completed","returned":{"metrics":{"solved":0},"raw_score":0},"remaining":{"calls":99,"seconds":11992}}
            11000 now submit 10240 set_submission
              reply to op 6: {"kind":"call","returned":{"state":"done"},"remaining_seconds":17989}
            12000 now submit 10240 set_submission end_work
              reply to op 7: {"kind":"call","returned":{"state":"done"},"remaining_seconds":60}
              (tick due at 72000)
            12100 agent_exited 0
              tear down agent
              (tick due at 18210000)
            12100 agent_torn_down
              start call 1 of final on agent=op 7's payload of 10240 bytes, with a timeout of 120s
            13500 call_started 1
            20100 call_returned 1 score=0 metrics={"solved":0}
            20100 call_torn_down 1
              finish: agent: AgentExited(Code(0)), finalized op 7's payload of 10240 bytes
              final returned [score=0 metrics={"solved":0}]
        "#]],
    );
}

// A call's caller hears its result, and is charged its time, as its end is reported; its function
// stays busy, and the final call and the run's finish wait, until its sandbox is torn down.
#[test]
fn result_before_teardown() {
    check(
        |_| {},
        r#"
        0 work_started
        1000 call train 10
        1500 call_started 0
        2000 call_returned 0 raw_score=1
        2100 status 0
        2200 call train 10
        30000 call_torn_down 0
        30100 call train 10
        30200 call_started 1
        30300 call_returned 1 raw_score=2
        30400 now submit 10 set_submission end_work
        30500 agent_exited 0
        30500 agent_torn_down
        31000 call_torn_down 1
        31500 call_started 2
        32000 call_returned 2 score=2
        60000 call_torn_down 2
        "#,
        &expect![[r#"
            start
              start agents
              (tick due at 18010000)
            0 work_started
              (tick due at 18000000)
            1000 call train 10
              reply to op 1: {"kind":"call","job":0,"remaining":{"calls":99,"seconds":12000}}
              start call 0 of train on op 1's payload of 10 bytes, with a timeout of 12000s
            1500 call_started 0
            2000 call_returned 0 raw_score=1
            2100 status 0
              reply to op 2: {"kind":"status","job":0,"state":"completed","returned":{"raw_score":1},"remaining":{"calls":99,"seconds":11999}}
            2200 call train 10
              refuse op 3 (busy): `train` runs one call at a time, and already has one queued, running or releasing its sandbox: call again once it has finished; this call is not charged
            30000 call_torn_down 0
            30100 call train 10
              reply to op 4: {"kind":"call","job":1,"remaining":{"calls":98,"seconds":11999}}
              start call 1 of train on op 4's payload of 10 bytes, with a timeout of 11999.5s
            30200 call_started 1
            30300 call_returned 1 raw_score=2
            30400 now submit 10 set_submission end_work
              reply to op 5: {"kind":"call","returned":{"state":"done"},"remaining_seconds":60}
              (tick due at 90400)
            30500 agent_exited 0
              tear down agent
              (tick due at 18210000)
            30500 agent_torn_down
            31000 call_torn_down 1
              start call 2 of final on agent=op 5's payload of 10 bytes, with a timeout of 120s
            31500 call_started 2
            32000 call_returned 2 score=2
            60000 call_torn_down 2
              finish: agent: AgentExited(Code(0)), finalized op 5's payload of 10 bytes
              final returned [score=2]
        "#]],
    );
}

#[test]
fn quotas() {
    check(
        |ir| {
            let train = &mut ir["roles"][0]["runtime"]["agent"]["grants"][0]["job"];
            train["cumulative_seconds"] = json!(10);
            train["call_timeout_seconds"] = json!(6);
        },
        r#"
        0 work_started
        1000 call train 4194305
        1000 call nothing 10
        1000 now submit 4194305 set_submission
        1000 call train 10
        2000 call_started 0
        6100 call_returned 0 +1 raw_score=null failure={"code":"wrong_answer"}
        6100 call_torn_down 0
        6200 status 0
        7000 call train 10
        7500 call_started 1
        13600 call_timed_out 1
        13700 call_failed 1 +2 the call's timeout passed
        13700 call_torn_down 1
        14000 status 1
        14100 call train 10
        14200 now submit 0 set_submission
        "#,
        &expect![[r#"
            start
              start agents
              (tick due at 18010000)
            0 work_started
              (tick due at 18000000)
            1000 call train 4194305
              refuse op 1: the payload is 4194305 bytes, over `train`'s limit of 4194304: send a smaller one
            1000 call nothing 10
              refuse op 2: you hold no grant named `nothing`: call one that `ops` lists
            1000 now submit 4194305 set_submission
              refuse op 3: the payload is 4194305 bytes, over `submit`'s limit of 4194304: send a smaller one
            1000 call train 10
              reply to op 4: {"kind":"call","job":0,"remaining":{"calls":99,"seconds":10}}
              start call 0 of train on op 4's payload of 10 bytes, with a timeout of 6s
            2000 call_started 0
            6100 call_returned 0 +1 raw_score=null failure={"code":"wrong_answer"}
            6100 call_torn_down 0
            6200 status 0
              reply to op 5: {"kind":"status","job":0,"state":"completed","returned":{"failure":{"code":"wrong_answer"},"raw_score":null},"model_requests_over_budget":1,"remaining":{"calls":99,"seconds":5}}
            7000 call train 10
              reply to op 6: {"kind":"call","job":1,"remaining":{"calls":98,"seconds":5}}
              start call 1 of train on op 6's payload of 10 bytes, with a timeout of 5.9s
            7500 call_started 1
            13600 call_timed_out 1
            13700 call_failed 1 +2 the call's timeout passed
            13700 call_torn_down 1
            14000 status 1
              reply to op 7: {"kind":"status","job":1,"state":"errored","failure":{"code":"call_timeout","message":"the call ran past its timeout"},"model_requests_over_budget":2,"remaining":{"calls":98,"seconds":0}}
            14100 call train 10
              refuse op 8: `train` has no call time left of the 10 s it allows: make no more calls to it
            14200 now submit 0 set_submission
              reply to op 9: {"kind":"call","returned":{"state":"done"},"remaining_seconds":17985}
        "#]],
    );
}

#[test]
fn job_functions() {
    check(
        |ir| {
            let mut verifier = ir["roles"][1].clone();
            verifier["name"] = json!("verifier");
            verifier["runtime"] = json!({ "target": { "cwd": "/pumpkin/home" } });
            ir["roles"].as_array_mut().unwrap().push(verifier);
            let arguments =
                json!({ "n": { "type": "int" }, "mode": { "type": "str", "default": "fast" } });
            let function = json!({ "name": "check", "job": {
                "target": "verifier",
                "body": { "file": "//lib/verify.star", "def": "check", "arguments": arguments },
                "payload_max_bytes": 20, "calls": 2, "call_timeout_seconds": 60,
            } });
            ir["roles"][0]["runtime"]["agent"]["grants"]
                .as_array_mut()
                .unwrap()
                .push(function);
        },
        r#"
        0 work_started
        1000 ops
        1050 call check 10
        1060 call check 10 n=x
        1070 call check 10 n=3 other=1
        1100 call check 10 n=3
        1500 status 0
        2000 call_failed 0 the target's sandbox did not connect
        2000 call_torn_down 0
        2050 status 0
        2100 call check 10 n=-1 mode=slow
        2300 call_started 1
        2600 call_returned 1 lines=1 bytes=6
        2600 call_torn_down 1
        2700 status 1
        "#,
        &expect![[r#"
            start
              start agents
              (tick due at 18010000)
            0 work_started
              (tick due at 18000000)
            1000 ops
              reply to op 1: {"kind":"ops","role":"agent","remaining_seconds":17999,"resources":{"cpu_millis":8000,"memory_bytes":34359738368},"grants":[{"kind":"job","name":"train","payload_max_bytes":4194304,"calls":100,"cumulative_seconds":12000,"call_timeout_seconds":12000},{"kind":"job","name":"check","payload_max_bytes":20,"calls":2,"cumulative_seconds":null,"call_timeout_seconds":60,"arguments":{"mode":{"type":"str","default":"fast"},"n":{"type":"int"}}},{"kind":"now","name":"submit","payload_max_bytes":4194304,"arguments":{"finalize":{"type":"bool","default":false}}}]}
            1050 call check 10
              refuse op 2: `--arg n` is required, as `check` gives it no default: give it as `--arg n=<value>`
            1060 call check 10 n=x
              refuse op 3: `--arg n` is `"x"`, which `int` can't read: give it decimal digits, after `-` or `+` or neither, from -2^63 to 2^63 - 1
            1070 call check 10 n=3 other=1
              refuse op 4: `--arg other` is given, and `check` takes no such argument: give only those that `ops` lists
            1100 call check 10 n=3
              reply to op 5: {"kind":"call","job":0,"remaining":{"calls":1,"seconds":null}}
              start call 0 of check on op 5's payload of 10 bytes, mode="fast", n=3, with a timeout of 60s
            1500 status 0
              reply to op 6: {"kind":"status","job":0,"state":"queued","remaining":{"calls":1,"seconds":null}}
            2000 call_failed 0 the target's sandbox did not connect
            2000 call_torn_down 0
            2050 status 0
              reply to op 7: {"kind":"status","job":0,"state":"errored","failure":{"code":"call_error","message":"the call's body failed: the run's record says why"},"remaining":{"calls":2,"seconds":null}}
            2100 call check 10 n=-1 mode=slow
              reply to op 8: {"kind":"call","job":1,"remaining":{"calls":1,"seconds":null}}
              start call 1 of check on op 8's payload of 10 bytes, mode="slow", n=-1, with a timeout of 60s
            2300 call_started 1
            2600 call_returned 1 lines=1 bytes=6
            2600 call_torn_down 1
            2700 status 1
              reply to op 9: {"kind":"status","job":1,"state":"completed","returned":{"bytes":6,"lines":1},"remaining":{"calls":1,"seconds":null}}
        "#]],
    );
}

#[test]
fn one_call_at_a_time() {
    check(
        |ir| ir["roles"][0]["runtime"]["agent"]["grants"][0]["job"]["calls"] = json!(2),
        r#"
        0 work_started
        1000 call train 10
        1100 call train 10
        1200 status 0
        2000 call_started 0
        3100 call train 10
        3200 call_returned 0
        3200 call_torn_down 0
        3300 status 0
        3400 call train 10
        4000 call_failed 1 the target's sandbox did not connect
        4000 call_torn_down 1
        4100 call train 10
        4200 status 1
        "#,
        &expect![[r#"
            start
              start agents
              (tick due at 18010000)
            0 work_started
              (tick due at 18000000)
            1000 call train 10
              reply to op 1: {"kind":"call","job":0,"remaining":{"calls":1,"seconds":12000}}
              start call 0 of train on op 1's payload of 10 bytes, with a timeout of 12000s
            1100 call train 10
              refuse op 2 (busy): `train` runs one call at a time, and already has one queued, running or releasing its sandbox: call again once it has finished; this call is not charged
            1200 status 0
              reply to op 3: {"kind":"status","job":0,"state":"queued","remaining":{"calls":1,"seconds":12000}}
            2000 call_started 0
            3100 call train 10
              refuse op 4 (busy): `train` runs one call at a time, and already has one queued, running or releasing its sandbox: call again once it has finished; this call is not charged
            3200 call_returned 0
            3200 call_torn_down 0
            3300 status 0
              reply to op 5: {"kind":"status","job":0,"state":"completed","returned":{},"remaining":{"calls":1,"seconds":11998}}
            3400 call train 10
              reply to op 6: {"kind":"call","job":1,"remaining":{"calls":0,"seconds":11998}}
              start call 1 of train on op 6's payload of 10 bytes, with a timeout of 11998.8s
            4000 call_failed 1 the target's sandbox did not connect
            4000 call_torn_down 1
            4100 call train 10
              reply to op 7: {"kind":"call","job":2,"remaining":{"calls":0,"seconds":11998}}
              start call 2 of train on op 7's payload of 10 bytes, with a timeout of 11998.8s
            4200 status 1
              reply to op 8: {"kind":"status","job":1,"state":"errored","failure":{"code":"call_error","message":"the call's body failed: the run's record says why"},"remaining":{"calls":0,"seconds":11998}}
        "#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn deadline() {
    check(
        |ir| ir["deadline_seconds"] = json!(60),
        r#"
        0 work_started
        50000 now submit 10 set_submission
        55000 call train 10
        55500 call_started 0
        59999 tick
        59999 model
        60000 model
        60000 tick
        60000 agent_torn_down
        60100 status 0
        60200 agent_exited 137
        60400 call_failed 0 stopped
        60400 call_torn_down 0
        60500 call_started 1
        180500 call_timed_out 1
        180600 call_returned 1 score=7
        180600 call_torn_down 1
        "#,
        &expect![[r#"
            start
              start agents
              (tick due at 70000)
            0 work_started
              (tick due at 60000)
            50000 now submit 10 set_submission
              reply to op 1: {"kind":"call","returned":{"state":"done"},"remaining_seconds":10}
            55000 call train 10
              reply to op 2: {"kind":"call","job":0,"remaining":{"calls":99,"seconds":12000}}
              start call 0 of train on op 2's payload of 10 bytes, with a timeout of 5s
            55500 call_started 0
            59999 tick
            59999 model
              model requests allowed
            60000 model
              model requests refused
            60000 tick
              stop call 0
              tear down agent
              (tick due at 270000)
            60000 agent_torn_down
            60100 status 0
              refuse op 3: your work has ended: make no more ops
            60200 agent_exited 137
            60400 call_failed 0 stopped
            60400 call_torn_down 0
              start call 1 of final on agent=op 1's payload of 10 bytes, with a timeout of 120s
            60500 call_started 1
            180500 call_timed_out 1
            180600 call_returned 1 score=7
            180600 call_torn_down 1
              finish: agent: Deadline, stored op 1's payload of 10 bytes
              final failed: Timeout
        "#]],
    );
    check(
        |ir| ir["deadline_seconds"] = json!(60),
        r"
        0 work_started
        50000 now submit 10 set_submission
        59999 call train 10
        60000 call_failed 0 the target's sandbox didn't start
        60000 call_torn_down 0
        60000 agent_torn_down
        ",
        &expect![[r#"
            start
              start agents
              (tick due at 70000)
            0 work_started
              (tick due at 60000)
            50000 now submit 10 set_submission
              reply to op 1: {"kind":"call","returned":{"state":"done"},"remaining_seconds":10}
            59999 call train 10
              reply to op 2: {"kind":"call","job":0,"remaining":{"calls":99,"seconds":12000}}
              start call 0 of train on op 2's payload of 10 bytes, with a timeout of 1ms
            60000 call_failed 0 the target's sandbox didn't start
              stop call 0
              tear down agent
              (tick due at 270000)
            60000 call_torn_down 0
            60000 agent_torn_down
              start call 1 of final on agent=op 1's payload of 10 bytes, with a timeout of 120s
        "#]],
    );
}

#[test]
fn deadline_from_work_start() {
    let deadline = |ir: &mut Value| ir["deadline_seconds"] = json!(60);
    check(
        deadline,
        r"
        8000 work_started
        8000 ops
        9000 work_started
        67999 tick
        68000 tick
        68000 agent_torn_down
        ",
        &expect![[r#"
            start
              start agents
              (tick due at 70000)
            8000 work_started
              (tick due at 68000)
            8000 ops
              reply to op 1: {"kind":"ops","role":"agent","remaining_seconds":60,"resources":{"cpu_millis":8000,"memory_bytes":34359738368},"grants":[{"kind":"job","name":"train","payload_max_bytes":4194304,"calls":100,"cumulative_seconds":12000,"call_timeout_seconds":12000},{"kind":"now","name":"submit","payload_max_bytes":4194304,"arguments":{"finalize":{"type":"bool","default":false}}}]}
            9000 work_started
            67999 tick
            68000 tick
              tear down agent
              (tick due at 270000)
            68000 agent_torn_down
              start call 0 of final on agent=missing, with a timeout of 120s
        "#]],
    );
    check(
        deadline,
        r"
        12000 work_started
        12000 ops
        70000 tick
        70000 agent_torn_down
        ",
        &expect![[r#"
            start
              start agents
              (tick due at 70000)
            12000 work_started
            12000 ops
              reply to op 1: {"kind":"ops","role":"agent","remaining_seconds":58,"resources":{"cpu_millis":8000,"memory_bytes":34359738368},"grants":[{"kind":"job","name":"train","payload_max_bytes":4194304,"calls":100,"cumulative_seconds":12000,"call_timeout_seconds":12000},{"kind":"now","name":"submit","payload_max_bytes":4194304,"arguments":{"finalize":{"type":"bool","default":false}}}]}
            70000 tick
              tear down agent
              (tick due at 270000)
            70000 agent_torn_down
              start call 0 of final on agent=missing, with a timeout of 120s
        "#]],
    );
    check(
        deadline,
        r"
        70000 tick
        70000 agent_torn_down
        ",
        &expect![[r#"
            start
              start agents
              (tick due at 70000)
            70000 tick
              tear down agent
              (tick due at 270000)
            70000 agent_torn_down
              start call 0 of final on agent=missing, with a timeout of 120s
        "#]],
    );
}

#[test]
fn hard_end_covers_the_run() {
    // Each step takes as long as it may: the agent starts at its startup and works to the deadline,
    // its teardown takes the grace and its startup again, and the final call's target its own.
    check(
        |ir| ir["deadline_seconds"] = json!(60),
        r#"
        10000 work_started
        69999 now submit 10 set_submission
        70000 tick
        140000 agent_torn_down
        150000 call_started 0
        269999 call_returned 0 score=7
        269999 call_torn_down 0
        "#,
        &expect![[r#"
            start
              start agents
              (tick due at 70000)
            10000 work_started
            69999 now submit 10 set_submission
              reply to op 1: {"kind":"call","returned":{"state":"done"},"remaining_seconds":0}
            70000 tick
              tear down agent
              (tick due at 270000)
            140000 agent_torn_down
              start call 0 of final on agent=op 1's payload of 10 bytes, with a timeout of 120s
            150000 call_started 0
            269999 call_returned 0 score=7
            269999 call_torn_down 0
              finish: agent: Deadline, stored op 1's payload of 10 bytes
              final returned [score=7]
        "#]],
    );
}

#[test]
fn agent_ends() {
    check(
        |_| {},
        r#"
        0 work_started
        1000 call train 10
        1500 call_started 0
        3100 now submit 10 set_submission
        3200 agent_exited 0
        3200 agent_torn_down
        3400 call_returned 0 raw_score=1
        3400 call_torn_down 0
        3500 call_started 1
        4000 call_failed 0 its report came again
        5100 call_returned 1 score="high"
        5100 call_torn_down 1
        "#,
        &expect![[r#"
            start
              start agents
              (tick due at 18010000)
            0 work_started
              (tick due at 18000000)
            1000 call train 10
              reply to op 1: {"kind":"call","job":0,"remaining":{"calls":99,"seconds":12000}}
              start call 0 of train on op 1's payload of 10 bytes, with a timeout of 12000s
            1500 call_started 0
            3100 now submit 10 set_submission
              reply to op 2: {"kind":"call","returned":{"state":"done"},"remaining_seconds":17996}
            3200 agent_exited 0
              stop call 0
              tear down agent
              (tick due at 18210000)
            3200 agent_torn_down
            3400 call_returned 0 raw_score=1
            3400 call_torn_down 0
              start call 1 of final on agent=op 2's payload of 10 bytes, with a timeout of 120s
            3500 call_started 1
            4000 call_failed 0 its report came again
            5100 call_returned 1 score="high"
            5100 call_torn_down 1
              finish: agent: AgentExited(Code(0)), stored op 2's payload of 10 bytes
              final returned [score="high"]
        "#]],
    );
    check(
        |_| {},
        r"
        30000 agent_failed the agent's sandbox did not connect
        30000 agent_torn_down
        ",
        &expect![[r#"
            start
              start agents
              (tick due at 18010000)
            30000 agent_failed the agent's sandbox did not connect
              tear down agent
              (tick due at 18210000)
            30000 agent_torn_down
              start call 0 of final on agent=missing, with a timeout of 120s
        "#]],
    );
}

#[test]
fn hard_end() {
    let deadline = |ir: &mut Value| ir["deadline_seconds"] = json!(60);
    check(
        deadline,
        r"
        0 work_started
        1000 call train 10
        1500 call_started 0
        270000 ops
        ",
        &expect![[r#"
            start
              start agents
              (tick due at 70000)
            0 work_started
              (tick due at 60000)
            1000 call train 10
              reply to op 1: {"kind":"call","job":0,"remaining":{"calls":99,"seconds":12000}}
              start call 0 of train on op 1's payload of 10 bytes, with a timeout of 59s
            1500 call_started 0
            270000 ops
              abandon the run at its hard end
              refuse op 2: the run has passed its hard end: make no more ops
              (tick due at 270000)
        "#]],
    );
    check(
        deadline,
        r"
        0 work_started
        1000 now submit 10 set_submission end_work
        270000 tick
        ",
        &expect![[r#"
            start
              start agents
              (tick due at 70000)
            0 work_started
              (tick due at 60000)
            1000 now submit 10 set_submission end_work
              reply to op 1: {"kind":"call","returned":{"state":"done"},"remaining_seconds":59}
            270000 tick
              abandon the run at its hard end
              (tick due at 270000)
        "#]],
    );
}

#[test]
fn shared_failure_after_abandon_leaves_the_call_to_teardown() {
    check(
        |ir| {
            ir["deadline_seconds"] = json!(60);
            ir["roles"][1]["runtime"] = json!({"target": {"lifetime": "work_phase", "cwd": "/"}});
        },
        r"
        0 work_started
        1000 call train 10
        270000 tick
        270001 shared_failed trainer disconnected
        ",
        &expect![[r#"
            start
              start agents
              (tick due at 70000)
            0 work_started
              (tick due at 60000)
            1000 call train 10
              reply to op 1: {"kind":"call","job":0,"remaining":{"calls":99,"seconds":12000}}
              start call 0 of train on op 1's payload of 10 bytes, with a timeout of 59s
            270000 tick
              abandon the run at its hard end
              (tick due at 270000)
            270001 shared_failed trainer disconnected
        "#]],
    );
}

#[test]
fn hard_end_after_work() {
    check(
        |ir| ir["deadline_seconds"] = json!(60),
        r"
        0 work_started
        1000 call train 10
        1500 call_started 0
        60000 tick
        60000 agent_torn_down
        269999 tick
        270000 tick
        ",
        &expect![[r#"
            start
              start agents
              (tick due at 70000)
            0 work_started
              (tick due at 60000)
            1000 call train 10
              reply to op 1: {"kind":"call","job":0,"remaining":{"calls":99,"seconds":12000}}
              start call 0 of train on op 1's payload of 10 bytes, with a timeout of 59s
            1500 call_started 0
            60000 tick
              stop call 0
              tear down agent
              (tick due at 270000)
            60000 agent_torn_down
            269999 tick
            270000 tick
              abandon the run at its hard end
        "#]],
    );
    check(
        |ir| {
            ir["deadline_seconds"] = json!(60);
            ir["roles"][0]["runtime"]["agent"]["finalize_grace_seconds"] = json!(30);
        },
        r#"
        0 work_started
        1000 now submit 10 set_submission
        2000 agent_exited 0
        2000 agent_torn_down
        2500 call_started 0
        122500 call_timed_out 0
        239999 tick
        240000 call_returned 0 score=7
        240000 call_torn_down 0
        "#,
        &expect![[r#"
            start
              start agents
              (tick due at 70000)
            0 work_started
              (tick due at 60000)
            1000 now submit 10 set_submission
              reply to op 1: {"kind":"call","returned":{"state":"done"},"remaining_seconds":59}
            2000 agent_exited 0
              tear down agent
              (tick due at 240000)
            2000 agent_torn_down
              start call 0 of final on agent=op 1's payload of 10 bytes, with a timeout of 120s
            2500 call_started 0
            122500 call_timed_out 0
            239999 tick
            240000 call_returned 0 score=7
              abandon the run at its hard end
            240000 call_torn_down 0
        "#]],
    );
}

#[test]
#[expect(clippy::too_many_lines, reason = "most of it is the transcript")]
fn hard_end_bounds() {
    check(
        |ir| ir["deadline_seconds"] = json!(60),
        r"
        0 work_started
        1000 agent_exited 0
        1000 agent_torn_down
        1500 call_returned 0
        1500 call_torn_down 0
        270000 tick
        ",
        &expect![[r#"
            start
              start agents
              (tick due at 70000)
            0 work_started
              (tick due at 60000)
            1000 agent_exited 0
              tear down agent
              (tick due at 270000)
            1000 agent_torn_down
              start call 0 of final on agent=missing, with a timeout of 120s
            1500 call_returned 0
            1500 call_torn_down 0
              finish: agent: AgentExited(Code(0)), missing
              final returned []
            270000 tick
        "#]],
    );
    check(
        |ir| ir["deadline_seconds"] = json!(5),
        r"
        0 work_started
        1000 now submit 10 set_submission
        5000 tick
        5000 agent_torn_down
        214999 tick
        215000 tick
        ",
        &expect![[r#"
            start
              start agents
              (tick due at 15000)
            0 work_started
              (tick due at 5000)
            1000 now submit 10 set_submission
              reply to op 1: {"kind":"call","returned":{"state":"done"},"remaining_seconds":4}
            5000 tick
              tear down agent
              (tick due at 215000)
            5000 agent_torn_down
              start call 0 of final on agent=op 1's payload of 10 bytes, with a timeout of 120s
            214999 tick
            215000 tick
              abandon the run at its hard end
        "#]],
    );
    check(
        |ir| ir["deadline_seconds"] = json!(u64::MAX),
        r"
        0 work_started
        1000 now submit 10 set_submission
        2000 agent_exited 0
        2000 agent_torn_down
        ",
        &expect![[r#"
            start
              start agents
              (tick due at 18446744073709551615999.999999)
            0 work_started
              (tick due at 18446744073709551615000)
            1000 now submit 10 set_submission
              reply to op 1: {"kind":"call","returned":{"state":"done"},"remaining_seconds":18446744073709551614}
            2000 agent_exited 0
              tear down agent
              (tick due at 18446744073709551615999.999999)
            2000 agent_torn_down
              start call 0 of final on agent=op 1's payload of 10 bytes, with a timeout of 120s
        "#]],
    );
}

#[test]
fn hard_end_startups() {
    // In the slow class, the agent's startup counts twice.
    check(
        |ir| {
            ir["deadline_seconds"] = json!(5);
            ir["roles"][0]["needs"]["cpu_model"] = json!("slow");
        },
        r"
        0 work_started
        1000 now submit 10 set_submission
        5000 tick
        5000 agent_torn_down
        254999 tick
        255000 tick
        ",
        &expect![[r#"
            start
              start agents
              (tick due at 35000)
            0 work_started
              (tick due at 5000)
            1000 now submit 10 set_submission
              reply to op 1: {"kind":"call","returned":{"state":"done"},"remaining_seconds":4}
            5000 tick
              tear down agent
              (tick due at 255000)
            5000 agent_torn_down
              start call 0 of final on agent=op 1's payload of 10 bytes, with a timeout of 120s
            254999 tick
            255000 tick
              abandon the run at its hard end
        "#]],
    );
    // In the slow class, the final call's target's startup counts once.
    check(
        |ir| {
            ir["deadline_seconds"] = json!(60);
            ir["roles"][2]["needs"]["cpu_model"] = json!("slow");
        },
        r"
        0 work_started
        1000 now submit 10 set_submission
        60000 tick
        60000 agent_torn_down
        289999 tick
        290000 tick
        ",
        &expect![[r#"
            start
              start agents
              (tick due at 70000)
            0 work_started
              (tick due at 60000)
            1000 now submit 10 set_submission
              reply to op 1: {"kind":"call","returned":{"state":"done"},"remaining_seconds":59}
            60000 tick
              tear down agent
              (tick due at 290000)
            60000 agent_torn_down
              start call 0 of final on agent=op 1's payload of 10 bytes, with a timeout of 120s
            289999 tick
            290000 tick
              abandon the run at its hard end
        "#]],
    );
}

#[test]
fn exact_scores() {
    check(
        |_| {},
        r#"
        0 work_started
        1000 call train 10
        1500 call_started 0
        2100 call_returned 0 raw_score=22.833062975103598
        2100 call_torn_down 0
        2200 status 0
        3000 now submit 10 set_submission end_work
        3100 agent_exited 0
        3100 agent_torn_down
        3500 call_started 1
        4100 call_returned 1 score=22.830584316865718
        4100 call_torn_down 1
        "#,
        &expect![[r#"
            start
              start agents
              (tick due at 18010000)
            0 work_started
              (tick due at 18000000)
            1000 call train 10
              reply to op 1: {"kind":"call","job":0,"remaining":{"calls":99,"seconds":12000}}
              start call 0 of train on op 1's payload of 10 bytes, with a timeout of 12000s
            1500 call_started 0
            2100 call_returned 0 raw_score=22.833062975103598
            2100 call_torn_down 0
            2200 status 0
              reply to op 2: {"kind":"status","job":0,"state":"completed","returned":{"raw_score":22.833062975103598},"remaining":{"calls":99,"seconds":11999}}
            3000 now submit 10 set_submission end_work
              reply to op 3: {"kind":"call","returned":{"state":"done"},"remaining_seconds":60}
              (tick due at 63000)
            3100 agent_exited 0
              tear down agent
              (tick due at 18210000)
            3100 agent_torn_down
              start call 1 of final on agent=op 3's payload of 10 bytes, with a timeout of 120s
            3500 call_started 1
            4100 call_returned 1 score=22.830584316865718
            4100 call_torn_down 1
              finish: agent: AgentExited(Code(0)), finalized op 3's payload of 10 bytes
              final returned [score=22.830584316865718]
        "#]],
    );
}

// A body's return is a result as long as its JSON, its dict included, takes at most
// `RESULT_MAX_BYTES`.
#[test]
fn return_bytes() {
    // A return whose JSON, `{"text":"x…"}`, takes `bytes`.
    let taking = |bytes: u64| {
        let text = bytes - u64::try_from(r#"{"text":""}"#.len()).unwrap();
        let text = "x".repeat(usize::try_from(text).unwrap());
        vec![(Name::from_static("text"), json!(text))]
    };
    let returned = [RESULT_MAX_BYTES, RESULT_MAX_BYTES + 1].map(|bytes| {
        match CallResult::returned_by(taking(bytes)) {
            Ok(result) => format!("{bytes} bytes: returned {} value", result.values().len()),
            Err(reason) => format!("{bytes} bytes: {reason}"),
        }
    });
    expect![[r#"
        1048576 bytes: returned 1 value
        1048577 bytes: the body's return takes 1048577 bytes as JSON, over the limit of 1048576: return at most that"#]].assert_eq(&returned.join("\n"));
}

#[test]
fn now_calls() {
    check(
        |ir| ir["final"]["payload_max_bytes"] = 10.into(),
        r"
        0 work_started
        1100 now submit 11 set_submission
        1200 now submit 10 failed
        1300 now submit 10 unread
        1400 now submit 10 set_submission end_work set_submission
        1500 now submit 10
        1600 locked
        1700 now submit 0 end_work
        1700 locked
        1800 ops
        1810 agent_exited 0
        1810 locked
        1820 agent_torn_down
        1850 call_returned 0
        1850 call_torn_down 0
        ",
        &expect![[r#"
            start
              start agents
              (tick due at 18010000)
            0 work_started
              (tick due at 18000000)
            1100 now submit 11 set_submission
              refuse op 1: the payload is 11 bytes, over the final call's limit of 10: send a smaller one
            1200 now submit 10 failed
              refuse op 2: the call's body failed: the run's record says why
            1300 now submit 10 unread
              refuse op 3: `--arg finalize` didn't read
            1400 now submit 10 set_submission end_work set_submission
              refuse op 4: `run.end_work()` ended the work, and the call has more effects after it: end the work last
            1500 now submit 10
              reply to op 5: {"kind":"call","returned":{"state":"done"},"remaining_seconds":17998}
            1600 locked
              submission locked: false
            1700 now submit 0 end_work
              reply to op 6: {"kind":"call","returned":{"state":"done"},"remaining_seconds":60}
              (tick due at 61700)
            1700 locked
              submission locked: true
            1800 ops
              refuse op 7: your work has ended: make no more ops
            1810 agent_exited 0
              tear down agent
              (tick due at 18210000)
            1810 locked
              submission locked: true
            1820 agent_torn_down
              start call 0 of final on agent=missing, with a timeout of 120s
            1850 call_returned 0
            1850 call_torn_down 0
              finish: agent: AgentExited(Code(0)), missing
              final returned []
        "#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn finalize_lock() {
    check(
        |_| {},
        r"
        0 work_started
        1000 call train 10
        1500 call_started 0
        2000 now submit 10 set_submission end_work
        2100 ops
        2200 status 0
        2300 now submit 10 set_submission
        61999 tick
        61999 model
        62000 model
        62000 tick
        62000 agent_torn_down
        62200 call_failed 0 stopped
        62200 call_torn_down 0
        ",
        &expect![[r#"
            start
              start agents
              (tick due at 18010000)
            0 work_started
              (tick due at 18000000)
            1000 call train 10
              reply to op 1: {"kind":"call","job":0,"remaining":{"calls":99,"seconds":12000}}
              start call 0 of train on op 1's payload of 10 bytes, with a timeout of 12000s
            1500 call_started 0
            2000 now submit 10 set_submission end_work
              reply to op 2: {"kind":"call","returned":{"state":"done"},"remaining_seconds":60}
              (tick due at 62000)
            2100 ops
              refuse op 3: your work has ended: make no more ops
            2200 status 0
              refuse op 4: your work has ended: make no more ops
            2300 now submit 10 set_submission
              refuse op 5: your work has ended: make no more ops
            61999 tick
            61999 model
              model requests allowed
            62000 model
              model requests refused
            62000 tick
              stop call 0
              tear down agent
              (tick due at 18210000)
            62000 agent_torn_down
            62200 call_failed 0 stopped
            62200 call_torn_down 0
              start call 1 of final on agent=op 2's payload of 10 bytes, with a timeout of 120s
        "#]],
    );
    check(
        |ir| ir["roles"][0]["runtime"]["agent"]["finalize_grace_seconds"] = json!(0),
        r"
        0 work_started
        2000 now submit 10 set_submission end_work
        2000 agent_torn_down
        2000 model
        2100 ops
        ",
        &expect![[r#"
            start
              start agents
              (tick due at 18010000)
            0 work_started
              (tick due at 18000000)
            2000 now submit 10 set_submission end_work
              reply to op 1: {"kind":"call","returned":{"state":"done"},"remaining_seconds":0}
              tear down agent
              (tick due at 18150000)
            2000 agent_torn_down
              start call 0 of final on agent=op 1's payload of 10 bytes, with a timeout of 120s
            2000 model
              model requests refused
            2100 ops
              refuse op 2: your work has ended: make no more ops
        "#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn finalize_near_deadline() {
    check(
        |ir| ir["deadline_seconds"] = json!(30),
        r"
        0 work_started
        10000 now submit 10 set_submission end_work
        29999 model
        30000 model
        ",
        &expect![[r#"
            start
              start agents
              (tick due at 40000)
            0 work_started
              (tick due at 30000)
            10000 now submit 10 set_submission end_work
              reply to op 1: {"kind":"call","returned":{"state":"done"},"remaining_seconds":20}
            29999 model
              model requests allowed
            30000 model
              model requests refused
        "#]],
    );
    let grace = |ir: &mut Value| {
        ir["deadline_seconds"] = json!(30);
        ir["roles"][0]["runtime"]["agent"]["finalize_grace_seconds"] = json!(10);
    };
    check(
        grace,
        r#"
        0 work_started
        19950 now submit 10 set_submission end_work
        30100 tick
        30100 agent_torn_down
        30150 ops
        30200 call_started 0
        30400 call_returned 0 score=1
        30400 call_torn_down 0
        "#,
        &expect![[r#"
            start
              start agents
              (tick due at 40000)
            0 work_started
              (tick due at 30000)
            19950 now submit 10 set_submission end_work
              reply to op 1: {"kind":"call","returned":{"state":"done"},"remaining_seconds":10}
              (tick due at 29950)
            30100 tick
              tear down agent
              (tick due at 190000)
            30100 agent_torn_down
              start call 0 of final on agent=op 1's payload of 10 bytes, with a timeout of 120s
            30150 ops
              refuse op 2: your work has ended: make no more ops
            30200 call_started 0
            30400 call_returned 0 score=1
            30400 call_torn_down 0
              finish: agent: Finalized, finalized op 1's payload of 10 bytes
              final returned [score=1]
        "#]],
    );
    check(
        grace,
        r"
        0 work_started
        20000 now submit 10 set_submission end_work
        30000 tick
        30000 agent_torn_down
        30000 ops
        ",
        &expect![[r#"
            start
              start agents
              (tick due at 40000)
            0 work_started
              (tick due at 30000)
            20000 now submit 10 set_submission end_work
              reply to op 1: {"kind":"call","returned":{"state":"done"},"remaining_seconds":10}
            30000 tick
              tear down agent
              (tick due at 190000)
            30000 agent_torn_down
              start call 0 of final on agent=op 1's payload of 10 bytes, with a timeout of 120s
            30000 ops
              refuse op 2: your work has ended: make no more ops
        "#]],
    );
}

#[test]
fn unknown_jobs() {
    check(
        |_| {},
        r"
        0 work_started
        1000 call train 10
        1400 status 2
        ",
        &expect![[r#"
            start
              start agents
              (tick due at 18010000)
            0 work_started
              (tick due at 18000000)
            1000 call train 10
              reply to op 1: {"kind":"call","job":0,"remaining":{"calls":99,"seconds":12000}}
              start call 0 of train on op 1's payload of 10 bytes, with a timeout of 12000s
            1400 status 2
              refuse op 2: you have no job 2: ask about a job that one of your calls returned
        "#]],
    );
}

/// [`workflow`] with a runner, `gpu_runner`, of an image of its own, that the agent leases under
/// grant `gpu`: 2 leases at once, each for 900 s, 1200 s in all, 3 commands in all, and transfers
/// of 4096 bytes each.
fn leasing(ir: &mut Value) {
    let mut runner = ir["roles"][1].clone();
    runner["name"] = json!("gpu_runner");
    runner["image"] = json!(format!("sha256:{}", "3".repeat(64)));
    runner["runtime"] = json!({ "runner": { "cwd": "/work" } });
    ir["roles"].as_array_mut().unwrap().push(runner);
    let grant = json!({ "name": "gpu", "session": {
        "runner": "gpu_runner", "instances": 2, "instance_seconds": 900,
        "cumulative_seconds": 1200, "commands": 3, "transfer_bytes": 4096,
    } });
    ir["roles"][0]["runtime"]["agent"]["grants"]
        .as_array_mut()
        .unwrap()
        .insert(1, grant);
}

#[test]
#[expect(clippy::too_many_lines, reason = "most of it is the transcript")]
fn leases() {
    check(
        leasing,
        r"
        0 work_started
        1000 ops
        2000 gpu.open
        2100 gpu.status 0
        2200 gpu.run 0 - nvidia-smi
        3000 lease_ready 0
        4000 gpu.run 0 - nvidia-smi -L
        4100 gpu.run 0 - true
        4200 gpu.status 0
        6000 command_ended 0 0 0 GPU 0
        7000 gpu.status 0
        8000 gpu.run 0 899 echo it's
        8100 gpu.run 0 894 sleep 1000
        9000 command_ended 0 1 timed_out
        9100 command_ended 0 0 1
        10000 gpu.close 0
        10100 gpu.status 0
        10200 gpu.close 0
        10300 gpu.run 0 - true
        11000 lease_ended 0
        11100 gpu.status 0
        11200 gpu.get 0:scheduler /work/out /got
        ",
        &expect![[r#"
            start
              start agents
              (tick due at 18010000)
            0 work_started
              (tick due at 18000000)
            1000 ops
              reply to op 1: {"kind":"ops","role":"agent","remaining_seconds":17999,"resources":{"cpu_millis":8000,"memory_bytes":34359738368},"grants":[{"kind":"job","name":"train","payload_max_bytes":4194304,"calls":100,"cumulative_seconds":12000,"call_timeout_seconds":12000},{"kind":"session","name":"gpu","instances":2,"instance_seconds":900,"cumulative_seconds":1200,"commands":3,"transfer_bytes":4096,"remaining":{"instances":2,"instance_seconds":900,"cumulative_seconds":1200,"commands":3}},{"kind":"now","name":"submit","payload_max_bytes":4194304,"arguments":{"finalize":{"type":"bool","default":false}}}]}
            2000 gpu.open
              reply to op 2: {"kind":"lease","lease":0,"remaining":{"instances":1,"instance_seconds":900,"cumulative_seconds":1200,"commands":3}}
              open lease 0 of gpu, as instance 0, started by 18000000
            2100 gpu.status 0
              reply to op 3: {"kind":"lease_status","lease":0,"state":"starting","elapsed_seconds":0,"busy_seconds":0,"idle_seconds":0,"failure":null,"last":null,"remaining":{"instances":1,"instance_seconds":900,"cumulative_seconds":1200,"commands":3}}
            2200 gpu.run 0 - nvidia-smi
              refuse op 4: lease 0 is still starting: run a command once `gpu.status` says it is ready
            3000 lease_ready 0
              (tick due at 903000)
            4000 gpu.run 0 - nvidia-smi -L
              reply to op 5: {"kind":"lease","lease":0,"remaining":{"instances":1,"instance_seconds":899,"cumulative_seconds":1199,"commands":2}}
              run command 0 of lease 0, ["nvidia-smi","-L"], with a timeout of None
            4100 gpu.run 0 - true
              refuse op 6: lease 0 runs command 0, one at a time: run a command once `gpu.status` says it has ended
            4200 gpu.status 0
              reply to op 7: {"kind":"lease_status","lease":0,"state":"running","elapsed_seconds":2,"busy_seconds":0,"idle_seconds":1,"failure":null,"last":{"kind":"command","command":0,"end":null,"stdout":"","stderr":""},"remaining":{"instances":1,"instance_seconds":898,"cumulative_seconds":1198,"commands":2}}
            6000 command_ended 0 0 0 GPU 0
            7000 gpu.status 0
              reply to op 8: {"kind":"lease_status","lease":0,"state":"ready","elapsed_seconds":5,"busy_seconds":2,"idle_seconds":2,"failure":null,"last":{"kind":"command","command":0,"end":{"exited":{"code":0}},"stdout":"GPU 0","stderr":""},"remaining":{"instances":1,"instance_seconds":896,"cumulative_seconds":1196,"commands":2}}
            8000 gpu.run 0 899 echo it's
              refuse op 9: the timeout of 899 s is longer than lease 0 has left, 895 s: run it again as `sandboxctl call gpu.run 0 --timeout 895 -- echo 'it'\''s'`
            8100 gpu.run 0 894 sleep 1000
              reply to op 10: {"kind":"lease","lease":0,"remaining":{"instances":1,"instance_seconds":894,"cumulative_seconds":1194,"commands":1}}
              run command 1 of lease 0, ["sleep","1000"], with a timeout of Some(894s)
            9000 command_ended 0 1 timed_out
            9100 command_ended 0 0 1
            10000 gpu.close 0
              reply to op 11: {"kind":"lease","lease":0,"remaining":{"instances":1,"instance_seconds":0,"cumulative_seconds":1193,"commands":1}}
              close lease 0
              (tick due at 18000000)
            10100 gpu.status 0
              reply to op 12: {"kind":"lease_status","lease":0,"state":"closing","elapsed_seconds":8,"busy_seconds":2,"idle_seconds":4,"failure":null,"last":{"kind":"command","command":1,"end":"timed_out","stdout":"","stderr":""},"remaining":{"instances":1,"instance_seconds":0,"cumulative_seconds":1193,"commands":1}}
            10200 gpu.close 0
              reply to op 13: {"kind":"lease","lease":0,"remaining":{"instances":1,"instance_seconds":0,"cumulative_seconds":1193,"commands":1}}
            10300 gpu.run 0 - true
              refuse op 14: lease 0 has ended: open another
            11000 lease_ended 0
            11100 gpu.status 0
              reply to op 15: {"kind":"lease_status","lease":0,"state":"closed","elapsed_seconds":9,"busy_seconds":2,"idle_seconds":4,"failure":null,"last":{"kind":"command","command":1,"end":"timed_out","stdout":"","stderr":""},"remaining":{"instances":2,"instance_seconds":0,"cumulative_seconds":1193,"commands":1}}
            11200 gpu.get 0:scheduler /work/out /got
              refuse op 16: lease 0 holds no container `scheduler`: name `main`, the one container that it holds
        "#]],
    );
}

/// [`leasing`], its runner holding the companions that `names` names, each of an image of its own.
fn accompanied(names: &[&str]) -> impl FnOnce(&mut Value) {
    move |ir| {
        leasing(ir);
        let runner = ir["roles"].as_array().unwrap().len() - 1;
        let companions: Vec<Value> = (names.iter().zip(4..))
            .map(|(name, digit)| {
                json!({
                    "name": name,
                    "image": format!("sha256:{}", digit.to_string().repeat(64)),
                    "user": ir["roles"][runner]["user"],
                    "cwd": "/work",
                })
            })
            .collect();
        ir["roles"][runner]["runtime"]["runner"]["companions"] = json!(companions);
    }
}

// A lease runs one command or copy at a time across its containers, each in a container it holds.
#[test]
fn lease_containers() {
    check(
        accompanied(&["scheduler", "swe-000"]),
        r"
        0 work_started
        2000 gpu.open
        2100 gpu.run 0:ghost - true
        3000 lease_ready 0
        3100 gpu.run 0:scheduler - serve
        3200 gpu.get 0:swe-000 /work/out /got
        3300 command_ended 0 0 0
        3400 gpu.get 0:swe-000 /work/out /got
        3500 transfer_ended 0 1 moved 1 5
        3600 gpu.run 0:scheduler 900 echo it's
        ",
        &expect![[r#"
            start
              start agents
              (tick due at 18010000)
            0 work_started
              (tick due at 18000000)
            2000 gpu.open
              reply to op 1: {"kind":"lease","lease":0,"remaining":{"instances":1,"instance_seconds":900,"cumulative_seconds":1200,"commands":3}}
              open lease 0 of gpu, as instance 0, started by 18000000
            2100 gpu.run 0:ghost - true
              refuse op 2: lease 0 holds no container `ghost`: name `main` or one of its companions, `scheduler` or `swe-000`
            3000 lease_ready 0
              (tick due at 903000)
            3100 gpu.run 0:scheduler - serve
              reply to op 3: {"kind":"lease","lease":0,"remaining":{"instances":1,"instance_seconds":899,"cumulative_seconds":1199,"commands":2}}
              run command 0 of lease 0 in companion 0, ["serve"], with a timeout of None
            3200 gpu.get 0:swe-000 /work/out /got
              refuse op 4: lease 0 runs command 0, one at a time: get a tree once `gpu.status` says it has ended
            3300 command_ended 0 0 0
            3400 gpu.get 0:swe-000 /work/out /got
              reply to op 5: {"kind":"lease","lease":0,"remaining":{"instances":1,"instance_seconds":899,"cumulative_seconds":1199,"commands":1}}
              get /work/out to /got as transfer 1 of lease 0 in companion 1, of at most 4096 bytes
            3500 transfer_ended 0 1 moved 1 5
            3600 gpu.run 0:scheduler 900 echo it's
              refuse op 6: the timeout of 900 s is longer than lease 0 has left, 899 s: run it again as `sandboxctl call gpu.run 0 --container scheduler --timeout 899 -- echo 'it'\''s'`
        "#]],
    );
}

#[test]
#[expect(clippy::too_many_lines, reason = "most of it is the transcript")]
fn lease_quotas() {
    check(
        leasing,
        r"
        0 work_started
        1000 gpu.open
        1100 gpu.open
        1200 gpu.open
        1300 gpu.status 2
        1400 gpu.run 7 - true
        1500 gpu.close 9
        1600 call gpu 10
        1800 train.open
        1900 submit.open
        2000 nothing.open
        3000 gpu.close 0
        3500 lease_ready 0
        4000 lease_ended 0
        4100 gpu.open
        5000 lease_ready 1
        5100 gpu.run 1 - a
        5200 command_ended 1 0 unstarted
        5300 gpu.run 1 - b
        5400 command_ended 1 1 1
        5500 gpu.run 1 - c
        5600 command_ended 1 2 0
        5700 gpu.run 1 - d
        5800 gpu.status 1
        ",
        &expect![[r#"
            start
              start agents
              (tick due at 18010000)
            0 work_started
              (tick due at 18000000)
            1000 gpu.open
              reply to op 1: {"kind":"lease","lease":0,"remaining":{"instances":1,"instance_seconds":900,"cumulative_seconds":1200,"commands":3}}
              open lease 0 of gpu, as instance 0, started by 18000000
            1100 gpu.open
              reply to op 2: {"kind":"lease","lease":1,"remaining":{"instances":0,"instance_seconds":900,"cumulative_seconds":1200,"commands":3}}
              open lease 1 of gpu, as instance 1, started by 18000000
            1200 gpu.open
              refuse op 3: `gpu` has 2 leases open, as many as it allows at once: open another once `gpu.status` says one has ended
            1300 gpu.status 2
              refuse op 4: you have no lease 2: name one that `gpu.open` returned
            1400 gpu.run 7 - true
              refuse op 5: you have no lease 7: name one that `gpu.open` returned
            1500 gpu.close 9
              refuse op 6: you have no lease 9: name one that `gpu.open` returned
            1600 call gpu 10
              refuse op 7: `gpu` is a session, not a function: call its functions, as `gpu.open`, instead
            1800 train.open
              refuse op 8: `train` is a job function, not a session: call it instead
            1900 submit.open
              refuse op 9: `submit` is a now function, not a session: call it instead
            2000 nothing.open
              refuse op 10: you hold no grant named `nothing`: call one that `ops` lists
            3000 gpu.close 0
              reply to op 11: {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":0,"cumulative_seconds":1200,"commands":3}}
              close lease 0
            3500 lease_ready 0
            4000 lease_ended 0
            4100 gpu.open
              reply to op 12: {"kind":"lease","lease":2,"remaining":{"instances":0,"instance_seconds":900,"cumulative_seconds":1200,"commands":3}}
              open lease 2 of gpu, as instance 0, started by 18000000
            5000 lease_ready 1
              (tick due at 905000)
            5100 gpu.run 1 - a
              reply to op 13: {"kind":"lease","lease":1,"remaining":{"instances":0,"instance_seconds":899,"cumulative_seconds":1199,"commands":2}}
              run command 0 of lease 1, ["a"], with a timeout of None
            5200 command_ended 1 0 unstarted
            5300 gpu.run 1 - b
              reply to op 14: {"kind":"lease","lease":1,"remaining":{"instances":0,"instance_seconds":899,"cumulative_seconds":1199,"commands":1}}
              run command 1 of lease 1, ["b"], with a timeout of None
            5400 command_ended 1 1 1
            5500 gpu.run 1 - c
              reply to op 15: {"kind":"lease","lease":1,"remaining":{"instances":0,"instance_seconds":899,"cumulative_seconds":1199,"commands":0}}
              run command 2 of lease 1, ["c"], with a timeout of None
            5600 command_ended 1 2 0
            5700 gpu.run 1 - d
              refuse op 16: `gpu` has no commands left of the 3 it allows, a transfer counting as one: run and move no more
            5800 gpu.status 1
              reply to op 17: {"kind":"lease_status","lease":1,"state":"ready","elapsed_seconds":4,"busy_seconds":0,"idle_seconds":0,"failure":null,"last":{"kind":"command","command":2,"end":{"exited":{"code":0}},"stdout":"","stderr":""},"remaining":{"instances":0,"instance_seconds":899,"cumulative_seconds":1199,"commands":0}}
        "#]],
    );
}

#[test]
fn lease_own_time() {
    check(
        leasing,
        r"
        0 work_started
        1000 gpu.open
        1100 gpu.open
        1200 lease_failed 1 the runner's sandbox didn't start
        2000 lease_ready 0
        3000 gpu.open
        4000 lease_ready 2
        5000 lease_failed 2 the runner's session ended
        5100 gpu.status 2
        901500 gpu.run 0 1 true
        901999 tick
        902000 tick
        902100 gpu.status 0
        ",
        &expect![[r#"
            start
              start agents
              (tick due at 18010000)
            0 work_started
              (tick due at 18000000)
            1000 gpu.open
              reply to op 1: {"kind":"lease","lease":0,"remaining":{"instances":1,"instance_seconds":900,"cumulative_seconds":1200,"commands":3}}
              open lease 0 of gpu, as instance 0, started by 18000000
            1100 gpu.open
              reply to op 2: {"kind":"lease","lease":1,"remaining":{"instances":0,"instance_seconds":900,"cumulative_seconds":1200,"commands":3}}
              open lease 1 of gpu, as instance 1, started by 18000000
            1200 lease_failed 1 the runner's sandbox didn't start
            2000 lease_ready 0
              (tick due at 902000)
            3000 gpu.open
              reply to op 3: {"kind":"lease","lease":2,"remaining":{"instances":0,"instance_seconds":900,"cumulative_seconds":1199,"commands":3}}
              open lease 2 of gpu, as instance 1, started by 18000000
            4000 lease_ready 2
              (tick due at 603000)
            5000 lease_failed 2 the runner's session ended
              (tick due at 902000)
            5100 gpu.status 2
              reply to op 4: {"kind":"lease_status","lease":2,"state":"failed","elapsed_seconds":2,"busy_seconds":0,"idle_seconds":1,"failure":"the runner's session ended","last":null,"remaining":{"instances":1,"instance_seconds":0,"cumulative_seconds":1195,"commands":3}}
            901500 gpu.run 0 1 true
              refuse op 5: the timeout of 1 s is longer than lease 0 has left, 0 s: run it again as `sandboxctl call gpu.run 0 -- true`
            901999 tick
            902000 tick
              close lease 0
              (tick due at 18000000)
            902100 gpu.status 0
              reply to op 6: {"kind":"lease_status","lease":0,"state":"closing","elapsed_seconds":901,"busy_seconds":0,"idle_seconds":900,"failure":null,"last":null,"remaining":{"instances":1,"instance_seconds":0,"cumulative_seconds":299,"commands":3}}
        "#]],
    );
}

#[test]
fn lease_asked_time() {
    check(
        leasing,
        r"
        0 work_started
        1000 gpu.open 901
        1100 gpu.open 300
        1200 gpu.open 900
        2000 lease_ready 0
        2100 gpu.run 0 301 true
        2200 gpu.status 0
        301999 tick
        302000 tick
        ",
        &expect![[r#"
            start
              start agents
              (tick due at 18010000)
            0 work_started
              (tick due at 18000000)
            1000 gpu.open 901
              refuse op 1: the time of 901 s is longer than `gpu` lets a lease last, 900 s: open it again as `sandboxctl call gpu.open --seconds 900`
            1100 gpu.open 300
              reply to op 2: {"kind":"lease","lease":0,"remaining":{"instances":1,"instance_seconds":300,"cumulative_seconds":1200,"commands":3}}
              open lease 0 of gpu, as instance 0, started by 18000000
            1200 gpu.open 900
              reply to op 3: {"kind":"lease","lease":1,"remaining":{"instances":0,"instance_seconds":900,"cumulative_seconds":1200,"commands":3}}
              open lease 1 of gpu, as instance 1, started by 18000000
            2000 lease_ready 0
              (tick due at 302000)
            2100 gpu.run 0 301 true
              refuse op 4: the timeout of 301 s is longer than lease 0 has left, 299 s: run it again as `sandboxctl call gpu.run 0 --timeout 299 -- true`
            2200 gpu.status 0
              reply to op 5: {"kind":"lease_status","lease":0,"state":"ready","elapsed_seconds":1,"busy_seconds":0,"idle_seconds":0,"failure":null,"last":null,"remaining":{"instances":0,"instance_seconds":299,"cumulative_seconds":1199,"commands":3}}
            301999 tick
            302000 tick
              close lease 0
              (tick due at 18000000)
        "#]],
    );
}

#[test]
fn lease_times() {
    check(
        leasing,
        r"
        0 work_started
        1000 gpu.open
        2000 lease_ready 0
        3300 gpu.open
        4000 lease_ready 1
        602999 tick
        603000 tick
        603100 gpu.status 0
        603200 gpu.status 1
        604000 lease_ended 0
        604100 lease_ended 1
        604200 gpu.open
        ",
        &expect![[r#"
            start
              start agents
              (tick due at 18010000)
            0 work_started
              (tick due at 18000000)
            1000 gpu.open
              reply to op 1: {"kind":"lease","lease":0,"remaining":{"instances":1,"instance_seconds":900,"cumulative_seconds":1200,"commands":3}}
              open lease 0 of gpu, as instance 0, started by 18000000
            2000 lease_ready 0
              (tick due at 902000)
            3300 gpu.open
              reply to op 2: {"kind":"lease","lease":1,"remaining":{"instances":0,"instance_seconds":900,"cumulative_seconds":1198,"commands":3}}
              open lease 1 of gpu, as instance 1, started by 18000000
            4000 lease_ready 1
              (tick due at 603000)
            602999 tick
            603000 tick
              close lease 0
              close lease 1
              (tick due at 18000000)
            603100 gpu.status 0
              reply to op 3: {"kind":"lease_status","lease":0,"state":"closing","elapsed_seconds":602,"busy_seconds":0,"idle_seconds":601,"failure":null,"last":null,"remaining":{"instances":0,"instance_seconds":0,"cumulative_seconds":0,"commands":3}}
            603200 gpu.status 1
              reply to op 4: {"kind":"lease_status","lease":1,"state":"closing","elapsed_seconds":599,"busy_seconds":0,"idle_seconds":599,"failure":null,"last":null,"remaining":{"instances":0,"instance_seconds":0,"cumulative_seconds":0,"commands":3}}
            604000 lease_ended 0
            604100 lease_ended 1
            604200 gpu.open
              refuse op 5: `gpu` has no time left of the 1200 s it allows: open no more leases of it
        "#]],
    );
}

#[test]
fn leases_end_with_work() {
    check(
        leasing,
        r"
        0 work_started
        1000 gpu.open
        1100 gpu.open
        2000 lease_ready 0
        3000 gpu.run 0 - train
        4000 now submit 10 set_submission
        5000 agent_exited 0
        5000 agent_torn_down
        6000 lease_ended 1
        7000 lease_failed 0 the runner's session ended
        7100 gpu.status 0
        8000 call_started 0
        ",
        &expect![[r#"
            start
              start agents
              (tick due at 18010000)
            0 work_started
              (tick due at 18000000)
            1000 gpu.open
              reply to op 1: {"kind":"lease","lease":0,"remaining":{"instances":1,"instance_seconds":900,"cumulative_seconds":1200,"commands":3}}
              open lease 0 of gpu, as instance 0, started by 18000000
            1100 gpu.open
              reply to op 2: {"kind":"lease","lease":1,"remaining":{"instances":0,"instance_seconds":900,"cumulative_seconds":1200,"commands":3}}
              open lease 1 of gpu, as instance 1, started by 18000000
            2000 lease_ready 0
              (tick due at 902000)
            3000 gpu.run 0 - train
              reply to op 3: {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":899,"cumulative_seconds":1199,"commands":2}}
              run command 0 of lease 0, ["train"], with a timeout of None
            4000 now submit 10 set_submission
              reply to op 4: {"kind":"call","returned":{"state":"done"},"remaining_seconds":17996}
            5000 agent_exited 0
              close lease 0
              close lease 1
              tear down agent
              (tick due at 18210000)
            5000 agent_torn_down
            6000 lease_ended 1
            7000 lease_failed 0 the runner's session ended
              start call 0 of final on agent=op 4's payload of 10 bytes, with a timeout of 120s
            7100 gpu.status 0
              refuse op 5: your work has ended: make no more ops
            8000 call_started 0
        "#]],
    );
}

#[test]
fn lease_shares() {
    check(
        |ir| {
            leasing(ir);
            ir["roles"][0]["runtime"]["agent"]["grants"][1]["session"]["instances"] = json!(3);
        },
        r"
        0 work_started
        1000 gpu.open
        1100 gpu.open
        1200 gpu.open
        2000 lease_ready 0
        3000 lease_ready 1
        4001 lease_ready 2
        403000 tick
        403001 tick
        ",
        &expect![[r#"
            start
              start agents
              (tick due at 18010000)
            0 work_started
              (tick due at 18000000)
            1000 gpu.open
              reply to op 1: {"kind":"lease","lease":0,"remaining":{"instances":2,"instance_seconds":900,"cumulative_seconds":1200,"commands":3}}
              open lease 0 of gpu, as instance 0, started by 18000000
            1100 gpu.open
              reply to op 2: {"kind":"lease","lease":1,"remaining":{"instances":1,"instance_seconds":900,"cumulative_seconds":1200,"commands":3}}
              open lease 1 of gpu, as instance 1, started by 18000000
            1200 gpu.open
              reply to op 3: {"kind":"lease","lease":2,"remaining":{"instances":0,"instance_seconds":900,"cumulative_seconds":1200,"commands":3}}
              open lease 2 of gpu, as instance 2, started by 18000000
            2000 lease_ready 0
              (tick due at 902000)
            3000 lease_ready 1
              (tick due at 602500)
            4001 lease_ready 2
              (tick due at 403000.333334)
            403000 tick
            403001 tick
              close lease 0
              close lease 1
              close lease 2
              (tick due at 18000000)
        "#]],
    );
}

#[test]
fn sessions_apart() {
    check(
        |ir| {
            leasing(ir);
            let mut runner = ir["roles"][3].clone();
            runner["name"] = json!("cpu_runner");
            ir["roles"].as_array_mut().unwrap().push(runner);
            let mut cpu = ir["roles"][0]["runtime"]["agent"]["grants"][1].clone();
            cpu["name"] = json!("cpu");
            cpu["session"]["runner"] = json!("cpu_runner");
            cpu["session"]["instances"] = json!(1);
            ir["roles"][0]["runtime"]["agent"]["grants"]
                .as_array_mut()
                .unwrap()
                .insert(2, cpu);
        },
        r"
        0 work_started
        1000 gpu.open
        1100 cpu.open
        1200 gpu.open
        1300 cpu.close 0
        ",
        &expect![[r#"
            start
              start agents
              (tick due at 18010000)
            0 work_started
              (tick due at 18000000)
            1000 gpu.open
              reply to op 1: {"kind":"lease","lease":0,"remaining":{"instances":1,"instance_seconds":900,"cumulative_seconds":1200,"commands":3}}
              open lease 0 of gpu, as instance 0, started by 18000000
            1100 cpu.open
              reply to op 2: {"kind":"lease","lease":1,"remaining":{"instances":0,"instance_seconds":900,"cumulative_seconds":1200,"commands":3}}
              open lease 1 of cpu, as instance 0, started by 18000000
            1200 gpu.open
              reply to op 3: {"kind":"lease","lease":2,"remaining":{"instances":0,"instance_seconds":900,"cumulative_seconds":1200,"commands":3}}
              open lease 2 of gpu, as instance 1, started by 18000000
            1300 cpu.close 0
              refuse op 4: lease 0 is `gpu`'s, not `cpu`'s: call `gpu`'s functions for it
        "#]],
    );
}

#[test]
fn lease_start_bounds() {
    check(
        holders_leasing,
        r"
        0 work_started
        1000 gpu.open
        1100 gpu.close 0
        1200 gpu.status 0
        1300 lease_failed 0 the runner's sandbox didn't start: it was given up
        1400 gpu.status 0
        2000 call train 10
        3000 call_started 0
        3100 @0 box.open
        53000 call_timed_out 0
        53000 lease_failed 1 the runner's sandbox didn't start: it was given up
        53100 call_failed 0 the call's timeout passed
        53100 call_torn_down 0
        54000 gpu.open
        18000000 tick
        18000000 agent_torn_down
        18000000 lease_failed 2 the runner's sandbox didn't start: it was given up
        ",
        &expect![[r#"
            start
              start agents
              (tick due at 18010000)
            0 work_started
              (tick due at 18000000)
            1000 gpu.open
              reply to op 1: {"kind":"lease","lease":0,"remaining":{"instances":1,"instance_seconds":900,"cumulative_seconds":1200,"commands":3}}
              open lease 0 of gpu, as instance 0, started by 18000000
            1100 gpu.close 0
              reply to op 2: {"kind":"lease","lease":0,"remaining":{"instances":1,"instance_seconds":0,"cumulative_seconds":1200,"commands":3}}
              close lease 0
            1200 gpu.status 0
              reply to op 3: {"kind":"lease_status","lease":0,"state":"closing","elapsed_seconds":0,"busy_seconds":0,"idle_seconds":0,"failure":null,"last":null,"remaining":{"instances":1,"instance_seconds":0,"cumulative_seconds":1200,"commands":3}}
            1300 lease_failed 0 the runner's sandbox didn't start: it was given up
            1400 gpu.status 0
              reply to op 4: {"kind":"lease_status","lease":0,"state":"closed","elapsed_seconds":0,"busy_seconds":0,"idle_seconds":0,"failure":null,"last":null,"remaining":{"instances":2,"instance_seconds":0,"cumulative_seconds":1200,"commands":3}}
            2000 call train 10
              reply to op 5: {"kind":"call","job":0,"remaining":{"calls":99,"seconds":12000}}
              start call 0 of train on op 5's payload of 10 bytes, with a timeout of 50s
            3000 call_started 0
            3100 @0 box.open
              reply to op 6: {"kind":"lease","lease":1,"remaining":{"instances":0,"instance_seconds":100,"cumulative_seconds":null,"commands":2}}
              open lease 1 of box for job 0, as instance 0, started by 53000
            53000 call_timed_out 0
              close lease 1
            53000 lease_failed 1 the runner's sandbox didn't start: it was given up
            53100 call_failed 0 the call's timeout passed
            53100 call_torn_down 0
            54000 gpu.open
              reply to op 7: {"kind":"lease","lease":2,"remaining":{"instances":1,"instance_seconds":900,"cumulative_seconds":1200,"commands":3}}
              open lease 2 of gpu, as instance 0, started by 18000000
            18000000 tick
              close lease 2
              tear down agent
              (tick due at 18210000)
            18000000 agent_torn_down
            18000000 lease_failed 2 the runner's sandbox didn't start: it was given up
              start call 1 of final on agent=missing, with a timeout of 120s
        "#]],
    );
}

#[test]
fn lease_busy() {
    check(
        leasing,
        r"
        0 work_started
        1000 gpu.open
        11000 lease_ready 0
        21000 gpu.run 0 - make
        31000 gpu.status 0
        41000 command_ended 0 0 0
        51000 gpu.status 0
        61000 gpu.put 0 /workspace/repo /work/repo
        71000 gpu.close 0
        81000 transfer_ended 0 1 moved 1 1
        91000 lease_ended 0
        92000 gpu.status 0
        93000 gpu.open
        94000 lease_ready 1
        95000 gpu.run 1 - make
        96000 lease_failed 1 the runner's session ended
        97000 gpu.status 1
        ",
        &expect![[r#"
            start
              start agents
              (tick due at 18010000)
            0 work_started
              (tick due at 18000000)
            1000 gpu.open
              reply to op 1: {"kind":"lease","lease":0,"remaining":{"instances":1,"instance_seconds":900,"cumulative_seconds":1200,"commands":3}}
              open lease 0 of gpu, as instance 0, started by 18000000
            11000 lease_ready 0
              (tick due at 911000)
            21000 gpu.run 0 - make
              reply to op 2: {"kind":"lease","lease":0,"remaining":{"instances":1,"instance_seconds":890,"cumulative_seconds":1190,"commands":2}}
              run command 0 of lease 0, ["make"], with a timeout of None
            31000 gpu.status 0
              reply to op 3: {"kind":"lease_status","lease":0,"state":"running","elapsed_seconds":30,"busy_seconds":10,"idle_seconds":10,"failure":null,"last":{"kind":"command","command":0,"end":null,"stdout":"","stderr":""},"remaining":{"instances":1,"instance_seconds":880,"cumulative_seconds":1180,"commands":2}}
            41000 command_ended 0 0 0
            51000 gpu.status 0
              reply to op 4: {"kind":"lease_status","lease":0,"state":"ready","elapsed_seconds":50,"busy_seconds":20,"idle_seconds":20,"failure":null,"last":{"kind":"command","command":0,"end":{"exited":{"code":0}},"stdout":"","stderr":""},"remaining":{"instances":1,"instance_seconds":860,"cumulative_seconds":1160,"commands":2}}
            61000 gpu.put 0 /workspace/repo /work/repo
              reply to op 5: {"kind":"lease","lease":0,"remaining":{"instances":1,"instance_seconds":850,"cumulative_seconds":1150,"commands":1}}
              put /workspace/repo to /work/repo as transfer 1 of lease 0, of at most 4096 bytes
            71000 gpu.close 0
              reply to op 6: {"kind":"lease","lease":0,"remaining":{"instances":1,"instance_seconds":0,"cumulative_seconds":1140,"commands":1}}
              close lease 0
              (tick due at 18000000)
            81000 transfer_ended 0 1 moved 1 1
            91000 lease_ended 0
            92000 gpu.status 0
              reply to op 7: {"kind":"lease_status","lease":0,"state":"closed","elapsed_seconds":91,"busy_seconds":30,"idle_seconds":30,"failure":null,"last":{"kind":"transfer","transfer":1,"end":{"moved":{"entries":1,"bytes":1}}},"remaining":{"instances":2,"instance_seconds":0,"cumulative_seconds":1140,"commands":1}}
            93000 gpu.open
              reply to op 8: {"kind":"lease","lease":1,"remaining":{"instances":1,"instance_seconds":900,"cumulative_seconds":1140,"commands":1}}
              open lease 1 of gpu, as instance 0, started by 18000000
            94000 lease_ready 1
              (tick due at 994000)
            95000 gpu.run 1 - make
              reply to op 9: {"kind":"lease","lease":1,"remaining":{"instances":1,"instance_seconds":899,"cumulative_seconds":1139,"commands":0}}
              run command 0 of lease 1, ["make"], with a timeout of None
            96000 lease_failed 1 the runner's session ended
              (tick due at 18000000)
            97000 gpu.status 1
              reply to op 10: {"kind":"lease_status","lease":1,"state":"failed","elapsed_seconds":4,"busy_seconds":1,"idle_seconds":1,"failure":"the runner's session ended","last":{"kind":"command","command":0,"end":null,"stdout":"","stderr":""},"remaining":{"instances":2,"instance_seconds":0,"cumulative_seconds":1138,"commands":0}}
        "#]],
    );
}

#[test]
fn lease_failure() {
    let reason = format!("a{}", "é".repeat(REASON_BYTES));
    check(
        leasing,
        &format!(
            "
            0 work_started
            1000 gpu.open
            2000 lease_failed 0 {reason}
            2100 gpu.status 0
            "
        ),
        &expect![[r#"
            start
              start agents
              (tick due at 18010000)
            0 work_started
              (tick due at 18000000)
            1000 gpu.open
              reply to op 1: {"kind":"lease","lease":0,"remaining":{"instances":1,"instance_seconds":900,"cumulative_seconds":1200,"commands":3}}
              open lease 0 of gpu, as instance 0, started by 18000000
            2000 lease_failed 0 aéééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééé
            2100 gpu.status 0
              reply to op 2: {"kind":"lease_status","lease":0,"state":"failed","elapsed_seconds":1,"busy_seconds":0,"idle_seconds":0,"failure":"aééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééé","last":null,"remaining":{"instances":2,"instance_seconds":0,"cumulative_seconds":1200,"commands":3}}
        "#]],
    );
}

#[test]
#[expect(clippy::too_many_lines, reason = "most of it is the transcript")]
fn lease_transfers() {
    let reason = format!("{}bc", "a".repeat(REASON_BYTES));
    check(
        leasing,
        &format!(
            "
            0 work_started
            1000 gpu.open
            1100 gpu.put 0 /workspace/repo /work/repo
            2000 lease_ready 0
            3000 gpu.put 0 /workspace/repo /work/repo replace
            3100 gpu.run 0 - make
            3200 gpu.get 0 /work/out /workspace/out
            3300 gpu.status 0
            3400 command_ended 0 0 0
            4000 transfer_ended 0 0 moved 3 2048
            4100 gpu.status 0
            5000 gpu.run 0 - make
            5100 transfer_ended 0 1 moved 1 1
            5200 gpu.status 0
            6000 command_ended 0 1 0
            6100 gpu.get 0 /work/out /workspace/out
            6200 gpu.close 0
            6300 gpu.status 0
            7000 transfer_ended 0 2 failed {reason}
            7100 gpu.status 0
            7200 gpu.put 0 /a /b
            7300 gpu.open
            8000 lease_ready 1
            8100 gpu.put 1 /a /b
            "
        ),
        &expect![[r#"
            start
              start agents
              (tick due at 18010000)
            0 work_started
              (tick due at 18000000)
            1000 gpu.open
              reply to op 1: {"kind":"lease","lease":0,"remaining":{"instances":1,"instance_seconds":900,"cumulative_seconds":1200,"commands":3}}
              open lease 0 of gpu, as instance 0, started by 18000000
            1100 gpu.put 0 /workspace/repo /work/repo
              refuse op 2: lease 0 is still starting: put a tree once `gpu.status` says it is ready
            2000 lease_ready 0
              (tick due at 902000)
            3000 gpu.put 0 /workspace/repo /work/repo replace
              reply to op 3: {"kind":"lease","lease":0,"remaining":{"instances":1,"instance_seconds":899,"cumulative_seconds":1199,"commands":2}}
              replace with /workspace/repo to /work/repo as transfer 0 of lease 0, of at most 4096 bytes
            3100 gpu.run 0 - make
              refuse op 4: lease 0 runs transfer 0, one at a time: run a command once `gpu.status` says it has ended
            3200 gpu.get 0 /work/out /workspace/out
              refuse op 5: lease 0 runs transfer 0, one at a time: get a tree once `gpu.status` says it has ended
            3300 gpu.status 0
              reply to op 6: {"kind":"lease_status","lease":0,"state":"running","elapsed_seconds":2,"busy_seconds":0,"idle_seconds":1,"failure":null,"last":{"kind":"transfer","transfer":0,"end":null},"remaining":{"instances":1,"instance_seconds":898,"cumulative_seconds":1198,"commands":2}}
            3400 command_ended 0 0 0
            4000 transfer_ended 0 0 moved 3 2048
            4100 gpu.status 0
              reply to op 7: {"kind":"lease_status","lease":0,"state":"ready","elapsed_seconds":3,"busy_seconds":1,"idle_seconds":1,"failure":null,"last":{"kind":"transfer","transfer":0,"end":{"moved":{"entries":3,"bytes":2048}}},"remaining":{"instances":1,"instance_seconds":897,"cumulative_seconds":1197,"commands":2}}
            5000 gpu.run 0 - make
              reply to op 8: {"kind":"lease","lease":0,"remaining":{"instances":1,"instance_seconds":897,"cumulative_seconds":1197,"commands":1}}
              run command 1 of lease 0, ["make"], with a timeout of None
            5100 transfer_ended 0 1 moved 1 1
            5200 gpu.status 0
              reply to op 9: {"kind":"lease_status","lease":0,"state":"running","elapsed_seconds":4,"busy_seconds":1,"idle_seconds":2,"failure":null,"last":{"kind":"command","command":1,"end":null,"stdout":"","stderr":""},"remaining":{"instances":1,"instance_seconds":896,"cumulative_seconds":1196,"commands":1}}
            6000 command_ended 0 1 0
            6100 gpu.get 0 /work/out /workspace/out
              reply to op 10: {"kind":"lease","lease":0,"remaining":{"instances":1,"instance_seconds":895,"cumulative_seconds":1195,"commands":0}}
              get /work/out to /workspace/out as transfer 2 of lease 0, of at most 4096 bytes
            6200 gpu.close 0
              reply to op 11: {"kind":"lease","lease":0,"remaining":{"instances":1,"instance_seconds":0,"cumulative_seconds":1195,"commands":0}}
              close lease 0
              (tick due at 18000000)
            6300 gpu.status 0
              reply to op 12: {"kind":"lease_status","lease":0,"state":"closing","elapsed_seconds":5,"busy_seconds":2,"idle_seconds":2,"failure":null,"last":{"kind":"transfer","transfer":2,"end":null},"remaining":{"instances":1,"instance_seconds":0,"cumulative_seconds":1195,"commands":0}}
            7000 transfer_ended 0 2 failed aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaabc
            7100 gpu.status 0
              reply to op 13: {"kind":"lease_status","lease":0,"state":"closing","elapsed_seconds":6,"busy_seconds":2,"idle_seconds":2,"failure":null,"last":{"kind":"transfer","transfer":2,"end":{"failed":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}},"remaining":{"instances":1,"instance_seconds":0,"cumulative_seconds":1195,"commands":0}}
            7200 gpu.put 0 /a /b
              refuse op 14: lease 0 has ended: open another
            7300 gpu.open
              reply to op 15: {"kind":"lease","lease":1,"remaining":{"instances":0,"instance_seconds":900,"cumulative_seconds":1195,"commands":0}}
              open lease 1 of gpu, as instance 1, started by 18000000
            8000 lease_ready 1
              (tick due at 908000)
            8100 gpu.put 1 /a /b
              refuse op 16: `gpu` has no commands left of the 3 it allows, a transfer counting as one: run and move no more
        "#]],
    );
}

#[test]
fn lease_slots() {
    check(
        leasing,
        r"
        0 work_started
        1000 gpu.open
        1100 gpu.open
        2000 lease_ready 0
        3000 gpu.run 0 - train
        4000 gpu.close 0
        4100 gpu.status 0
        4200 gpu.open
        4300 ops
        5000 lease_ended 0
        5100 gpu.status 0
        5200 ops
        5300 gpu.open
        ",
        &expect![[r#"
            start
              start agents
              (tick due at 18010000)
            0 work_started
              (tick due at 18000000)
            1000 gpu.open
              reply to op 1: {"kind":"lease","lease":0,"remaining":{"instances":1,"instance_seconds":900,"cumulative_seconds":1200,"commands":3}}
              open lease 0 of gpu, as instance 0, started by 18000000
            1100 gpu.open
              reply to op 2: {"kind":"lease","lease":1,"remaining":{"instances":0,"instance_seconds":900,"cumulative_seconds":1200,"commands":3}}
              open lease 1 of gpu, as instance 1, started by 18000000
            2000 lease_ready 0
              (tick due at 902000)
            3000 gpu.run 0 - train
              reply to op 3: {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":899,"cumulative_seconds":1199,"commands":2}}
              run command 0 of lease 0, ["train"], with a timeout of None
            4000 gpu.close 0
              reply to op 4: {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":0,"cumulative_seconds":1198,"commands":2}}
              close lease 0
              (tick due at 18000000)
            4100 gpu.status 0
              reply to op 5: {"kind":"lease_status","lease":0,"state":"closing","elapsed_seconds":3,"busy_seconds":1,"idle_seconds":1,"failure":null,"last":{"kind":"command","command":0,"end":null,"stdout":"","stderr":""},"remaining":{"instances":0,"instance_seconds":0,"cumulative_seconds":1198,"commands":2}}
            4200 gpu.open
              refuse op 6: `gpu` has 2 leases open, as many as it allows at once, 1 of them `closing`, which holds its instance until its runner is gone: open another once `gpu.status` says one has ended
            4300 ops
              reply to op 7: {"kind":"ops","role":"agent","remaining_seconds":17995,"resources":{"cpu_millis":8000,"memory_bytes":34359738368},"grants":[{"kind":"job","name":"train","payload_max_bytes":4194304,"calls":100,"cumulative_seconds":12000,"call_timeout_seconds":12000},{"kind":"session","name":"gpu","instances":2,"instance_seconds":900,"cumulative_seconds":1200,"commands":3,"transfer_bytes":4096,"remaining":{"instances":0,"instance_seconds":900,"cumulative_seconds":1198,"commands":2}},{"kind":"now","name":"submit","payload_max_bytes":4194304,"arguments":{"finalize":{"type":"bool","default":false}}}]}
            5000 lease_ended 0
            5100 gpu.status 0
              reply to op 8: {"kind":"lease_status","lease":0,"state":"closed","elapsed_seconds":4,"busy_seconds":1,"idle_seconds":1,"failure":null,"last":{"kind":"command","command":0,"end":null,"stdout":"","stderr":""},"remaining":{"instances":1,"instance_seconds":0,"cumulative_seconds":1198,"commands":2}}
            5200 ops
              reply to op 9: {"kind":"ops","role":"agent","remaining_seconds":17994,"resources":{"cpu_millis":8000,"memory_bytes":34359738368},"grants":[{"kind":"job","name":"train","payload_max_bytes":4194304,"calls":100,"cumulative_seconds":12000,"call_timeout_seconds":12000},{"kind":"session","name":"gpu","instances":2,"instance_seconds":900,"cumulative_seconds":1200,"commands":3,"transfer_bytes":4096,"remaining":{"instances":1,"instance_seconds":900,"cumulative_seconds":1198,"commands":2}},{"kind":"now","name":"submit","payload_max_bytes":4194304,"arguments":{"finalize":{"type":"bool","default":false}}}]}
            5300 gpu.open
              reply to op 10: {"kind":"lease","lease":2,"remaining":{"instances":0,"instance_seconds":900,"cumulative_seconds":1198,"commands":2}}
              open lease 2 of gpu, as instance 0, started by 18000000
        "#]],
    );
}

/// [`leasing`]'s workflow, whose trainer and final call's target each lease a runner of their own
/// under a session named `box`: `trainer` `train_box`, 1 at a time, each for 100 s, and 2 commands
/// in all, and `final_judge` `final_box`, 1 at a time, each for 30 s; and whose train calls time
/// out after 50 s.
fn holders_leasing(ir: &mut Value) {
    leasing(ir);
    ir["roles"][0]["runtime"]["agent"]["grants"][0]["job"]["call_timeout_seconds"] = json!(50);
    let boxes = [("train_box", 1, "4", 100), ("final_box", 2, "5", 30)];
    for (name, holder, image, seconds) in boxes {
        let mut runner = ir["roles"][3].clone();
        runner["name"] = json!(name);
        runner["image"] = json!(format!("sha256:{}", image.repeat(64)));
        ir["roles"].as_array_mut().unwrap().push(runner);
        let mut grant = json!({ "name": "box", "session": {
            "runner": name, "instances": 1, "instance_seconds": seconds,
            "transfer_bytes": 4096,
        } });
        if holder == 1 {
            grant["help"] = json!("Compile the draft in a fresh runner.");
            grant["session"]["commands"] = json!(2);
        }
        ir["roles"][holder]["runtime"]["target"]["sessions"] = json!([grant]);
    }
}

#[test]
#[expect(clippy::too_many_lines, reason = "most of it is the transcript")]
fn call_model_routes() {
    check(
        |ir| {
            let train = &mut ir["roles"][0]["runtime"]["agent"]["grants"][0]["job"];
            train["call_timeout_seconds"] = json!(10);
        },
        r#"
        0 work_started
        1000 call train 10
        1500 @0 model
        2000 call_started 0
        2000 @0 model
        2100 @1 model
        11999 @0 model
        12000 @0 model
        12000 model
        12000 @0 model
        12100 call_returned 0 raw_score=1
        12100 call_torn_down 0
        12200 call train 10
        13000 call_started 1
        13100 @1 model
        14000 call_timed_out 1
        14000 @1 model
        14100 call_failed 1 the call's timeout passed
        14100 call_torn_down 1
        14200 call train 10
        15000 call_started 2
        15100 now submit 10 set_submission
        15200 agent_exited 0
        15200 agent_torn_down
        15200 @2 model
        15200 model
        15300 call_failed 2 the call was stopped
        15300 call_torn_down 2
        16000 call_started 3
        16000 @3 model
        16000 model
        17000 @3 model
        "#,
        &expect![[r#"
            start
              start agents
              (tick due at 18010000)
            0 work_started
              (tick due at 18000000)
            1000 call train 10
              reply to op 1: {"kind":"call","job":0,"remaining":{"calls":99,"seconds":12000}}
              start call 0 of train on op 1's payload of 10 bytes, with a timeout of 10s
            1500 @0 model
              model requests refused
            2000 call_started 0
            2000 @0 model
              model requests allowed
            2100 @1 model
              model requests refused
            11999 @0 model
              model requests allowed
            12000 @0 model
              model requests refused
            12000 model
              model requests allowed
            12000 @0 model
              model requests refused
            12100 call_returned 0 raw_score=1
            12100 call_torn_down 0
            12200 call train 10
              reply to op 2: {"kind":"call","job":1,"remaining":{"calls":98,"seconds":11989}}
              start call 1 of train on op 2's payload of 10 bytes, with a timeout of 10s
            13000 call_started 1
            13100 @1 model
              model requests allowed
            14000 call_timed_out 1
            14000 @1 model
              model requests refused
            14100 call_failed 1 the call's timeout passed
            14100 call_torn_down 1
            14200 call train 10
              reply to op 3: {"kind":"call","job":2,"remaining":{"calls":97,"seconds":11988}}
              start call 2 of train on op 3's payload of 10 bytes, with a timeout of 10s
            15000 call_started 2
            15100 now submit 10 set_submission
              reply to op 4: {"kind":"call","returned":{"state":"done"},"remaining_seconds":17984}
            15200 agent_exited 0
              stop call 2
              tear down agent
              (tick due at 18210000)
            15200 agent_torn_down
            15200 @2 model
              model requests refused
            15200 model
              model requests refused
            15300 call_failed 2 the call was stopped
            15300 call_torn_down 2
              start call 3 of final on agent=op 4's payload of 10 bytes, with a timeout of 120s
            16000 call_started 3
            16000 @3 model
              model requests allowed
            16000 model
              model requests refused
            17000 @3 model
              model requests allowed
        "#]],
    );
}

#[test]
#[expect(clippy::too_many_lines, reason = "most of it is the transcript")]
fn target_leases() {
    check(
        holders_leasing,
        r#"
        0 work_started
        1000 call train 10
        1100 @0 ops
        2000 call_started 0
        2100 @0 ops
        2200 @0 box.open
        2300 @0 gpu.open
        2400 @0 status 0
        2500 @0 now submit 10 set_submission
        2600 @0 call train 10
        2700 gpu.status 0
        2800 gpu.open
        2900 @0 box.status 1
        3000 @0 box.run 1 - true
        3100 @0 box.close 1
        3200 @7 ops
        4000 lease_ready 0
        4100 @0 box.run 0 - check
        4200 command_ended 0 0 0 7
        4300 @0 box.status 0
        4400 @0 box.put 0 /pumpkin/in /work/in
        4500 transfer_ended 0 1 moved 2 10
        4600 gpu.get 0 /work/in /in
        4700 @0 box.get 0 /work/in /in
        "#,
        &expect![[r#"
            start
              start agents
              (tick due at 18010000)
            0 work_started
              (tick due at 18000000)
            1000 call train 10
              reply to op 1: {"kind":"call","job":0,"remaining":{"calls":99,"seconds":12000}}
              start call 0 of train on op 1's payload of 10 bytes, with a timeout of 50s
            1100 @0 ops
              refuse op 2: the call has ended: make no more ops
            2000 call_started 0
            2100 @0 ops
              reply to op 3: {"kind":"ops","role":"trainer","remaining_seconds":49,"resources":{"cpu_millis":8000,"memory_bytes":2147483648},"grants":[{"kind":"session","help":"Compile the draft in a fresh runner.","name":"box","instances":1,"instance_seconds":100,"cumulative_seconds":null,"commands":2,"transfer_bytes":4096,"remaining":{"instances":1,"instance_seconds":100,"cumulative_seconds":null,"commands":2}}]}
            2200 @0 box.open
              reply to op 4: {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":100,"cumulative_seconds":null,"commands":2}}
              open lease 0 of box for job 0, as instance 0, started by 52000
            2300 @0 gpu.open
              refuse op 5: you hold no grant named `gpu`: call one that `ops` lists
            2400 @0 status 0
              refuse op 6: you have no job 0: ask about a job that one of your calls returned
            2500 @0 now submit 10 set_submission
              refuse op 7: you hold no grant named `submit`: call one that `ops` lists
            2600 @0 call train 10
              refuse op 8: you hold no grant named `train`: call one that `ops` lists
            2700 gpu.status 0
              refuse op 9: you have no lease 0: name one that `gpu.open` returned
            2800 gpu.open
              reply to op 10: {"kind":"lease","lease":1,"remaining":{"instances":1,"instance_seconds":900,"cumulative_seconds":1200,"commands":3}}
              open lease 1 of gpu, as instance 0, started by 18000000
            2900 @0 box.status 1
              refuse op 11: you have no lease 1: name one that `box.open` returned
            3000 @0 box.run 1 - true
              refuse op 12: you have no lease 1: name one that `box.open` returned
            3100 @0 box.close 1
              refuse op 13: you have no lease 1: name one that `box.open` returned
            3200 @7 ops
              refuse op 14: the call has ended: make no more ops
            4000 lease_ready 0
              (tick due at 104000)
            4100 @0 box.run 0 - check
              reply to op 15: {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":99,"cumulative_seconds":null,"commands":1}}
              run command 0 of lease 0, ["check"], with a timeout of None
            4200 command_ended 0 0 0 7
            4300 @0 box.status 0
              reply to op 16: {"kind":"lease_status","lease":0,"state":"ready","elapsed_seconds":2,"busy_seconds":0,"idle_seconds":0,"failure":null,"last":{"kind":"command","command":0,"end":{"exited":{"code":0}},"stdout":"7","stderr":""},"remaining":{"instances":0,"instance_seconds":99,"cumulative_seconds":null,"commands":1}}
            4400 @0 box.put 0 /pumpkin/in /work/in
              reply to op 17: {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":99,"cumulative_seconds":null,"commands":0}}
              put /pumpkin/in to /work/in as transfer 1 of lease 0 for job 0, of at most 4096 bytes
            4500 transfer_ended 0 1 moved 2 10
            4600 gpu.get 0 /work/in /in
              refuse op 18: you have no lease 0: name one that `gpu.open` returned
            4700 @0 box.get 0 /work/in /in
              refuse op 19: `box` has no commands left of the 2 it allows, a transfer counting as one: run and move no more
        "#]],
    );
}

#[test]
#[expect(clippy::too_many_lines, reason = "most of it is the transcript")]
fn call_leases() {
    check(
        holders_leasing,
        r#"
        0 work_started
        1000 call train 10
        2000 call_started 0
        2100 @0 box.open
        2200 @0 box.open
        3000 lease_ready 0
        3100 @0 box.run 0 49 a
        3200 @0 box.run 0 47 a
        3300 command_ended 0 0 0
        3400 @0 box.run 0 - b
        3500 command_ended 0 1 0
        3600 @0 box.run 0 - c
        4000 call_returned 0 raw_score=1
        4000 call_torn_down 0
        4100 @0 box.status 0
        4300 status 0
        4400 call train 10
        5000 lease_ended 0
        5100 status 0
        5200 call train 10
        6000 call_started 1
        6100 @1 box.open
        6200 lease_ready 1
        6300 @1 box.run 1 - a
        7000 call_failed 1 the target's session ended
        7000 call_torn_down 1
        8000 lease_ended 1
        8100 call train 10
        9000 call_started 2
        9100 @2 box.open
        9200 lease_ready 2
        9250 now submit 10 set_submission
        9300 agent_exited 0
        9300 agent_torn_down
        9400 call_failed 2 the call was stopped
        9400 call_torn_down 2
        9500 lease_ended 2
        "#,
        &expect![[r#"
            start
              start agents
              (tick due at 18010000)
            0 work_started
              (tick due at 18000000)
            1000 call train 10
              reply to op 1: {"kind":"call","job":0,"remaining":{"calls":99,"seconds":12000}}
              start call 0 of train on op 1's payload of 10 bytes, with a timeout of 50s
            2000 call_started 0
            2100 @0 box.open
              reply to op 2: {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":100,"cumulative_seconds":null,"commands":2}}
              open lease 0 of box for job 0, as instance 0, started by 52000
            2200 @0 box.open
              refuse op 3: `box` has 1 leases open, as many as it allows at once: open another once `box.status` says one has ended
            3000 lease_ready 0
              (tick due at 103000)
            3100 @0 box.run 0 49 a
              refuse op 4: the timeout of 49 s is longer than lease 0 has left, 48 s: run it again as `sandboxctl call box.run 0 --timeout 48 -- a`
            3200 @0 box.run 0 47 a
              reply to op 5: {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":99,"cumulative_seconds":null,"commands":1}}
              run command 0 of lease 0, ["a"], with a timeout of Some(47s)
            3300 command_ended 0 0 0
            3400 @0 box.run 0 - b
              reply to op 6: {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":99,"cumulative_seconds":null,"commands":0}}
              run command 1 of lease 0, ["b"], with a timeout of None
            3500 command_ended 0 1 0
            3600 @0 box.run 0 - c
              refuse op 7: `box` has no commands left of the 2 it allows, a transfer counting as one: run and move no more
            4000 call_returned 0 raw_score=1
              close lease 0
              (tick due at 18000000)
            4000 call_torn_down 0
            4100 @0 box.status 0
              refuse op 8: the call has ended: make no more ops
            4300 status 0
              reply to op 9: {"kind":"status","job":0,"state":"completed","returned":{"raw_score":1},"remaining":{"calls":99,"seconds":11998}}
            4400 call train 10
              refuse op 10 (busy): `train` runs one call at a time, and already has one queued, running or releasing its sandbox: call again once it has finished; this call is not charged
            5000 lease_ended 0
            5100 status 0
              reply to op 11: {"kind":"status","job":0,"state":"completed","returned":{"raw_score":1},"remaining":{"calls":99,"seconds":11998}}
            5200 call train 10
              reply to op 12: {"kind":"call","job":1,"remaining":{"calls":98,"seconds":11998}}
              start call 1 of train on op 12's payload of 10 bytes, with a timeout of 50s
            6000 call_started 1
            6100 @1 box.open
              reply to op 13: {"kind":"lease","lease":1,"remaining":{"instances":0,"instance_seconds":100,"cumulative_seconds":null,"commands":2}}
              open lease 1 of box for job 1, as instance 0, started by 56000
            6200 lease_ready 1
              (tick due at 106200)
            6300 @1 box.run 1 - a
              reply to op 14: {"kind":"lease","lease":1,"remaining":{"instances":0,"instance_seconds":99,"cumulative_seconds":null,"commands":1}}
              run command 0 of lease 1, ["a"], with a timeout of None
            7000 call_failed 1 the target's session ended
              close lease 1
              (tick due at 18000000)
            7000 call_torn_down 1
            8000 lease_ended 1
            8100 call train 10
              reply to op 15: {"kind":"call","job":2,"remaining":{"calls":97,"seconds":11997}}
              start call 2 of train on op 15's payload of 10 bytes, with a timeout of 50s
            9000 call_started 2
            9100 @2 box.open
              reply to op 16: {"kind":"lease","lease":2,"remaining":{"instances":0,"instance_seconds":100,"cumulative_seconds":null,"commands":2}}
              open lease 2 of box for job 2, as instance 0, started by 59000
            9200 lease_ready 2
              (tick due at 109200)
            9250 now submit 10 set_submission
              reply to op 17: {"kind":"call","returned":{"state":"done"},"remaining_seconds":17990}
            9300 agent_exited 0
              stop call 2
              close lease 2
              tear down agent
              (tick due at 18210000)
            9300 agent_torn_down
            9400 call_failed 2 the call was stopped
            9400 call_torn_down 2
            9500 lease_ended 2
              start call 3 of final on agent=op 17's payload of 10 bytes, with a timeout of 120s
        "#]],
    );
}

#[test]
fn final_call_leases() {
    check(
        holders_leasing,
        r#"
        0 work_started
        1000 now submit 10 set_submission
        2000 agent_exited 0
        2000 agent_torn_down
        3000 call_started 0
        3100 @0 ops
        3200 @0 box.open
        4000 lease_ready 0
        4100 @0 box.run 0 - check
        34000 tick
        34100 @0 box.open
        34200 lease_ended 0
        34300 @0 box.open
        35100 call_returned 0 score=7
        35100 call_torn_down 0
        36000 lease_ended 1
        "#,
        &expect![[r#"
            start
              start agents
              (tick due at 18010000)
            0 work_started
              (tick due at 18000000)
            1000 now submit 10 set_submission
              reply to op 1: {"kind":"call","returned":{"state":"done"},"remaining_seconds":17999}
            2000 agent_exited 0
              tear down agent
              (tick due at 18210000)
            2000 agent_torn_down
              start call 0 of final on agent=op 1's payload of 10 bytes, with a timeout of 120s
            3000 call_started 0
            3100 @0 ops
              reply to op 2: {"kind":"ops","role":"final_judge","remaining_seconds":119,"resources":{"cpu_millis":8000,"memory_bytes":2147483648},"grants":[{"kind":"session","name":"box","instances":1,"instance_seconds":30,"cumulative_seconds":null,"commands":null,"transfer_bytes":4096,"remaining":{"instances":1,"instance_seconds":30,"cumulative_seconds":null,"commands":null}}]}
            3200 @0 box.open
              reply to op 3: {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":30,"cumulative_seconds":null,"commands":null}}
              open lease 0 of box for job 0, as instance 0, started by 123000
            4000 lease_ready 0
              (tick due at 34000)
            4100 @0 box.run 0 - check
              reply to op 4: {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":29,"cumulative_seconds":null,"commands":null}}
              run command 0 of lease 0, ["check"], with a timeout of None
            34000 tick
              close lease 0
              (tick due at 18210000)
            34100 @0 box.open
              refuse op 5: `box` has 1 leases open, as many as it allows at once, 1 of them `closing`, which holds its instance until its runner is gone: open another once `box.status` says one has ended
            34200 lease_ended 0
            34300 @0 box.open
              reply to op 6: {"kind":"lease","lease":1,"remaining":{"instances":0,"instance_seconds":30,"cumulative_seconds":null,"commands":null}}
              open lease 1 of box for job 0, as instance 0, started by 123000
            35100 call_returned 0 score=7
              close lease 1
            35100 call_torn_down 0
            36000 lease_ended 1
              finish: agent: AgentExited(Code(0)), stored op 1's payload of 10 bytes
              final returned [score=7]
        "#]],
    );
}

#[test]
fn work_target_drains_the_previous_jobs_lease_before_reusing_its_slot() {
    check(
        |ir| {
            two_agents(ir);
            ir["roles"][0]["runtime"]["agent"]["grants"][0]["job"]["call_timeout_seconds"] =
                json!(1);
            ir["roles"][1]["runtime"]["target"]["lifetime"] = json!("work_phase");
        },
        r#"
        0 work_started
        100 @zoe call train 1
        110 call_started 0
        120 @0 box.open
        130 lease_ready 0
        1110 call_timed_out 0
        1120 call_failed 0 the call's timeout passed
        1120 call_torn_down 0
        1130 @bob call train 1
        1140 lease_ended 0
        1150 @bob call train 1
        1160 call_started 1
        1170 @1 box.status 0
        1180 @1 box.open
        "#,
        &expect![[r#"
            start
              start agents
              (tick due at 30000)
            0 work_started
              (tick due at 20000)
            100 @zoe call train 1
              reply to op 1: {"kind":"call","job":0,"remaining":{"calls":99,"seconds":12000}}
              start call 0 of train on op 1's payload of 1 bytes, with a timeout of 1s
            110 call_started 0
            120 @0 box.open
              reply to op 2: {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":100,"cumulative_seconds":null,"commands":2}}
              open lease 0 of box for job 0, as instance 0, started by 1110
            130 lease_ready 0
            1110 call_timed_out 0
              close lease 0
            1120 call_failed 0 the call's timeout passed
            1120 call_torn_down 0
            1130 @bob call train 1
              refuse op 3 (busy): `train`'s shared sandbox is busy: retry after its current call ends; this call is not charged
            1140 lease_ended 0
            1150 @bob call train 1
              reply to op 4: {"kind":"call","job":1,"remaining":{"calls":99,"seconds":12000}}
              start call 1 of train on op 4's payload of 1 bytes, with a timeout of 18.85s
            1160 call_started 1
            1170 @1 box.status 0
              refuse op 5: you have no lease 0: name one that `box.open` returned
            1180 @1 box.open
              reply to op 6: {"kind":"lease","lease":1,"remaining":{"instances":0,"instance_seconds":100,"cumulative_seconds":null,"commands":2}}
              open lease 1 of box for job 1, as instance 0, started by 20000
        "#]],
    );
}

#[test]
fn abandoned_final_call_leases() {
    check(
        |ir| {
            holders_leasing(ir);
            ir["deadline_seconds"] = json!(1);
            ir["roles"][0]["runtime"]["agent"]["finalize_grace_seconds"] = json!(0);
            ir["roles"][2]["runtime"]["target"]["sessions"][0]["session"]["instances"] = json!(2);
        },
        r"
        10000 work_started
        10500 now submit 10 set_submission
        10999 agent_exited 0
        10999 agent_torn_down
        30999 call_started 0
        120000 @0 box.open
        121000 lease_ready 0
        150900 @0 box.open
        151000 tick
        151001 lease_ready 1
        151002 @0 box.status 1
        151003 call_returned 0 score=7
        151003 call_torn_down 0
        151004 lease_ended 0
        151005 lease_ended 1
        ",
        &expect![[r#"
            start
              start agents
              (tick due at 11000)
            10000 work_started
            10500 now submit 10 set_submission
              reply to op 1: {"kind":"call","returned":{"state":"done"},"remaining_seconds":0}
            10999 agent_exited 0
              tear down agent
              (tick due at 151000)
            10999 agent_torn_down
              start call 0 of final on agent=op 1's payload of 10 bytes, with a timeout of 120s
            30999 call_started 0
            120000 @0 box.open
              reply to op 2: {"kind":"lease","lease":0,"remaining":{"instances":1,"instance_seconds":30,"cumulative_seconds":null,"commands":null}}
              open lease 0 of box for job 0, as instance 0, started by 150999
            121000 lease_ready 0
            150900 @0 box.open
              reply to op 3: {"kind":"lease","lease":1,"remaining":{"instances":0,"instance_seconds":30,"cumulative_seconds":null,"commands":null}}
              open lease 1 of box for job 0, as instance 1, started by 150999
            151000 tick
              abandon the run at its hard end
            151001 lease_ready 1
            151002 @0 box.status 1
              refuse op 4: the run has passed its hard end: make no more ops
            151003 call_returned 0 score=7
            151003 call_torn_down 0
            151004 lease_ended 0
            151005 lease_ended 1
        "#]],
    );
}

fn two_agents(ir: &mut Value) {
    holders_leasing(ir);
    ir["deadline_seconds"] = json!(20);
    let zoe = &mut ir["roles"][0];
    zoe["name"] = json!("zoe");
    zoe["runtime"]["agent"]["finalize_grace_seconds"] = json!(3);
    let mut bob = zoe.clone();
    bob["name"] = json!("bob");
    bob["runtime"]["agent"]["finalize_grace_seconds"] = json!(10);
    ir["roles"].as_array_mut().unwrap().push(bob);
}

#[test]
#[expect(clippy::too_many_lines, reason = "most of it is the transcript")]
fn agents_work_until_their_own_end() {
    check(
        two_agents,
        r#"
        100 work_started
        200 @zoe call train 12
        201 @bob call train 22
        204 call_started 0
        205 call_started 1
        210 @0 box.open
        211 @1 box.open
        220 @zoe gpu.open
        225 @bob status 0
        226 @bob gpu.close 2
        230 @zoe now submit 12 set_submission end_work
        235 @zoe now submit 15 set_submission
        300 @zoe model
        3230 @0 model
        3230 @1 model
        3230 tick
        3240 @zoe model
        3250 @bob model
        3260 @bob status 1
        3200 @zoe agent_exited 0
        3300 @bob now submit 0 set_submission
        3400 @bob agent_exited 0
        3500 @bob agent_torn_down
        3600 call_failed 0 stopped
        3600 call_torn_down 0
        3601 call_failed 1 stopped
        3601 call_torn_down 1
        3700 lease_ended 0
        3701 lease_ended 1
        3702 lease_ended 2
        3800 @zoe agent_torn_down
        3900 call_returned 2 score=1
        3900 call_torn_down 2
    "#,
        &expect![[r#"
            start
              start agents
              (tick due at 30000)
            100 work_started
              (tick due at 20100)
            200 @zoe call train 12
              reply to op 1: {"kind":"call","job":0,"remaining":{"calls":99,"seconds":12000}}
              start call 0 of train on op 1's payload of 12 bytes, with a timeout of 19.9s
            201 @bob call train 22
              reply to op 2: {"kind":"call","job":1,"remaining":{"calls":99,"seconds":12000}}
              start call 1 of train on op 2's payload of 22 bytes, with a timeout of 19.899s
            204 call_started 0
            205 call_started 1
            210 @0 box.open
              reply to op 3: {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":100,"cumulative_seconds":null,"commands":2}}
              open lease 0 of box for job 0, as instance 0, started by 20100
            211 @1 box.open
              reply to op 4: {"kind":"lease","lease":1,"remaining":{"instances":0,"instance_seconds":100,"cumulative_seconds":null,"commands":2}}
              open lease 1 of box for job 1, as instance 0, started by 20100
            220 @zoe gpu.open
              reply to op 5: {"kind":"lease","lease":2,"remaining":{"instances":1,"instance_seconds":900,"cumulative_seconds":1200,"commands":3}}
              open lease 2 of gpu, as instance 0, started by 20100
            225 @bob status 0
              refuse op 6: you have no job 0: ask about a job that one of your calls returned
            226 @bob gpu.close 2
              refuse op 7: you have no lease 2: name one that `gpu.open` returned
            230 @zoe now submit 12 set_submission end_work
              reply to op 8: {"kind":"call","returned":{"state":"done"},"remaining_seconds":3}
              (tick due at 3230)
            235 @zoe now submit 15 set_submission
              refuse op 9: your work has ended: make no more ops
            300 @zoe model
              model requests allowed
            3230 @0 model
              model requests refused
            3230 @1 model
              model requests allowed
            3230 tick
              stop call 0
              close lease 0
              close lease 2
              tear down zoe
              (tick due at 20100)
            3240 @zoe model
              model requests refused
            3250 @bob model
              model requests allowed
            3260 @bob status 1
              reply to op 10: {"kind":"status","job":1,"state":"running","remaining":{"calls":99,"seconds":12000}}
            3200 @zoe agent_exited 0
            3300 @bob now submit 0 set_submission
              reply to op 11: {"kind":"call","returned":{"state":"done"},"remaining_seconds":16}
            3400 @bob agent_exited 0
              stop call 1
              close lease 1
              tear down bob
              (tick due at 180000)
            3500 @bob agent_torn_down
            3600 call_failed 0 stopped
            3600 call_torn_down 0
            3601 call_failed 1 stopped
            3601 call_torn_down 1
            3700 lease_ended 0
            3701 lease_ended 1
            3702 lease_ended 2
            3800 @zoe agent_torn_down
              start call 2 of final on zoe=op 8's payload of 12 bytes, bob=op 11's payload of 0 bytes, with a timeout of 120s
            3900 call_returned 2 score=1
            3900 call_torn_down 2
              finish: zoe: Finalized, finalized op 8's payload of 12 bytes
              bob: AgentExited(Code(0)), stored op 11's payload of 0 bytes
              final returned [score=1]
        "#]],
    );
}

#[test]
fn common_deadline_wins_a_tie() {
    check(
        |ir| {
            two_agents(ir);
            let bob = ir["roles"].as_array_mut().unwrap().last_mut().unwrap();
            bob["needs"]["cpu_model"] = json!("slow");
        },
        r#"
        35000 work_started
        46000 @zoe now submit 2 set_submission
        47000 @zoe now submit 0 end_work
        49999 @bob model
        50000 @zoe agent_exited 0
        49999 @bob now submit 1 set_submission
        50100 @zoe agent_torn_down
        50200 @bob agent_torn_down
        50500 call_failed 0 judge failed
        50500 call_torn_down 0
    "#,
        &expect![[r#"
            start
              start agents
              (tick due at 50000)
            35000 work_started
            46000 @zoe now submit 2 set_submission
              reply to op 1: {"kind":"call","returned":{"state":"done"},"remaining_seconds":4}
            47000 @zoe now submit 0 end_work
              reply to op 2: {"kind":"call","returned":{"state":"done"},"remaining_seconds":3}
            49999 @bob model
              model requests allowed
            50000 @zoe agent_exited 0
              tear down zoe
              tear down bob
              (tick due at 220000)
            49999 @bob now submit 1 set_submission
              refuse op 3: your work has ended: make no more ops
            50100 @zoe agent_torn_down
            50200 @bob agent_torn_down
              start call 0 of final on zoe=op 1's payload of 2 bytes, bob=missing, with a timeout of 120s
            50500 call_failed 0 judge failed
            50500 call_torn_down 0
              finish: zoe: Deadline, finalized op 1's payload of 2 bytes
              bob: Deadline, missing
              final failed: Error("judge failed")
        "#]],
    );
}

#[test]
fn agents_without_submissions_are_judged() {
    check(
        two_agents,
        r#"
        50 @bob agent_failed could not start
        60 @bob agent_torn_down
        100 work_started
        150 @zoe agent_exited 0
        150 @bob agent_exited 0
        200 @zoe agent_torn_down
        250 call_returned 0 score=null
        250 call_torn_down 0
    "#,
        &expect![[r#"
            start
              start agents
              (tick due at 30000)
            50 @bob agent_failed could not start
              tear down bob
            60 @bob agent_torn_down
            100 work_started
              (tick due at 20100)
            150 @zoe agent_exited 0
              tear down zoe
              (tick due at 180000)
            150 @bob agent_exited 0
            200 @zoe agent_torn_down
              start call 0 of final on zoe=missing, bob=missing, with a timeout of 120s
            250 call_returned 0 score=null
            250 call_torn_down 0
              finish: zoe: AgentExited(Code(0)), missing
              bob: AgentFailed("could not start"), missing
              final returned [score=null]
        "#]],
    );
}

#[test]
#[expect(clippy::too_many_lines, reason = "most of it is the transcript")]
fn shared_calls_serialize_across_grants_and_drain_before_judging() {
    check(
        |ir| {
            two_agents(ir);
            let roles = ir["roles"].as_array_mut().unwrap();
            let mut board = roles[1].clone();
            board["name"] = json!("board");
            board["runtime"] = json!({"target":{"lifetime":"work_phase","cwd":"/"}});
            for role in &mut *roles {
                if let Some(grants) = role["runtime"]
                    .get_mut("agent")
                    .and_then(|agent| agent["grants"].as_array_mut())
                {
                    let mut board_call = grants
                        .iter()
                        .find(|grant| grant["name"] == "train")
                        .unwrap()
                        .clone();
                    board_call["name"] = json!("board");
                    board_call["job"]["target"] = json!("board");
                    grants.push(board_call);
                }
            }
            roles.push(board);
        },
        r#"
        100 work_started
        200 @zoe call board 3
        210 @bob call board 7
        220 call_started 0
        230 call_returned 0 count=1
        232 @zoe call board 3
        235 call_torn_down 0
        240 @bob call board 7
        250 shared_failed board exited
        255 @bob call board 1
        260 call_failed 1 lost
        260 call_torn_down 1
        270 @zoe call board 1
        300 @zoe agent_exited 0
        310 @bob agent_exited 0
        320 @bob agent_torn_down
        330 @zoe agent_torn_down
        340 shared_torn_down board
        350 call_returned 2 score=0
        350 call_torn_down 2
    "#,
        &expect![[r#"
            start
              start agents
              (tick due at 30000)
            100 work_started
              (tick due at 20100)
            200 @zoe call board 3
              reply to op 1: {"kind":"call","job":0,"remaining":{"calls":99,"seconds":12000}}
              start call 0 of board on op 1's payload of 3 bytes, with a timeout of 19.9s
            210 @bob call board 7
              refuse op 2 (busy): `board`'s shared sandbox is busy: retry after its current call ends; this call is not charged
            220 call_started 0
            230 call_returned 0 count=1
            232 @zoe call board 3
              refuse op 3 (busy): `board` runs one call at a time, and already has one queued, running or releasing its sandbox: call again once it has finished; this call is not charged
            235 call_torn_down 0
            240 @bob call board 7
              reply to op 4: {"kind":"call","job":1,"remaining":{"calls":99,"seconds":12000}}
              start call 1 of board on op 4's payload of 7 bytes, with a timeout of 19.86s
            250 shared_failed board exited
              stop call 1
            255 @bob call board 1
              refuse op 5: `board` cannot run because its shared sandbox is unavailable
            260 call_failed 1 lost
            260 call_torn_down 1
            270 @zoe call board 1
              refuse op 6: `board` cannot run because its shared sandbox is unavailable
            300 @zoe agent_exited 0
              tear down zoe
            310 @bob agent_exited 0
              tear down bob
              (tick due at 180000)
            320 @bob agent_torn_down
            330 @zoe agent_torn_down
              tear down shared board
            340 shared_torn_down board
              start call 2 of final on zoe=missing, bob=missing, with a timeout of 120s
            350 call_returned 2 score=0
            350 call_torn_down 2
              finish: zoe: AgentExited(Code(0)), missing
              bob: AgentExited(Code(0)), missing
              final returned [score=0]
        "#]],
    );
}
