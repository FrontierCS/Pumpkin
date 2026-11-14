//! The driver's decision core, fed event scripts on [`WORKFLOW`], or on variants of it. Each
//! test shows what the core did after each event, and when it next wants a tick whenever that
//! changes. Each role is placed in a class whose sandboxes may take [`STARTUP`] to start, or, if
//! it asks for more memory than that class gives, [`SLOW_STARTUP`], which sets the run's hard
//! end.
//!
//! A script has one event per line: the time in milliseconds since the run started, then one
//! of these:
//!
//! - `agent_started`, `agent_exited <code>`, `agent_failed <reason>`;
//! - `ops`, `judge <grant> <bytes>`, `submit <grant> <bytes> [final]` and `status <job>`,
//!   each an op from the agent, whose payload is named by the op's number;
//! - `judge_started <job>`, `judge_exited <job> [<code>]`, `judge_timed_out <job>`,
//!   `collected <job> <result.json>`, `judge_failed <job> <reason>`;
//! - `tick`;
//! - `model`, no event, but the question whether the agent's model requests are allowed then.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test helper fails the test that calls it"
)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::num::NonZeroU64;
use std::time::Duration;

use decision::{
    Action, Core, Event, JobId, JudgeResult, OpId, Outcome, PayloadRef, Request, Submission,
    Verdict, WorkEnd,
};
use expect_test::{Expect, expect};
use schema::ir::{self, Name, Resources, Sha256Digest};
use schema::sandbox::ExitStatus;
use schema::sandbox::op::OpReply;
use schema::workflow::{Class, JudgeCall, Workflow};
use serde_json::{Value, json};
use test_support::WORKFLOW;

/// How long a sandbox may take to start in the class that every role of [`WORKFLOW`] fits.
const STARTUP: Duration = Duration::from_secs(10);

/// How long a sandbox may take to start in the class that gives a role more memory.
const SLOW_STARTUP: Duration = Duration::from_secs(30);

/// The memory that the slow class gives a role.
const SLOW_MEMORY_BYTES: u64 = 2 << 40;

/// Runs `script` on [`WORKFLOW`] with `edit` applied, and compares what the core did
/// with `expect`.
fn check(edit: impl FnOnce(&mut Value), script: &str, expect: &Expect) {
    let mut ir: Value = serde_json::from_str(WORKFLOW).unwrap();
    edit(&mut ir);
    let ir: ir::Workflow = serde_json::from_value(ir).unwrap();
    let placed = Workflow::parse(&ir).unwrap().place(&classes()).unwrap();
    let mut run = Run {
        workflow: placed.workflow(),
        ops: 0,
        jobs: BTreeMap::new(),
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
        if let Some(at) = line.strip_suffix(" model") {
            let at = Duration::from_millis(at.parse().unwrap());
            let allowed = if core.model_requests_allowed(at) {
                "allowed"
            } else {
                "refused"
            };
            writeln!(run.transcript, "{line}\n  model requests {allowed}").unwrap();
        } else {
            let (at, event) = line.split_once(' ').unwrap();
            let event = run.event(event);
            let actions = core.handle(Duration::from_millis(at.parse().unwrap()), event);
            run.note(line, &actions, core.tick_due());
        }
    }
    expect.assert_eq(&run.transcript);
}

/// Two classes: one whose starts take [`STARTUP`], which every role of [`WORKFLOW`] fits, and one
/// whose starts take [`SLOW_STARTUP`], which gives a role [`SLOW_MEMORY_BYTES`].
fn classes() -> BTreeMap<Name, Class> {
    let class = |name: &str, memory_bytes, startup: Duration| {
        let class = Class {
            resources: Resources {
                cpu_millis: NonZeroU64::new(64_000).unwrap(),
                memory_bytes: NonZeroU64::new(memory_bytes).unwrap(),
            },
            startup_seconds: NonZeroU64::new(startup.as_secs()).unwrap(),
        };
        (Name::try_from(name.to_owned()).unwrap(), class)
    };
    BTreeMap::from([
        class("fast", 1 << 40, STARTUP),
        class("slow", SLOW_MEMORY_BYTES, SLOW_STARTUP),
    ])
}

/// What a script needs to know as it runs.
struct Run<'w> {
    workflow: &'w Workflow,
    /// The ops made so far.
    ops: u64,
    /// What each job was made under, for its result's spec.
    jobs: BTreeMap<JobId, JudgeCall>,
    /// When the core last said it next wants a tick, once it has.
    tick_due: Option<Duration>,
    transcript: String,
}

impl Run<'_> {
    fn event(&mut self, event: &str) -> Event {
        let words: Vec<&str> = event.splitn(3, ' ').collect();
        let job = || JobId(words[1].parse().unwrap());
        let code = |word: &str| ExitStatus::Code(word.parse().unwrap());
        match words[0] {
            "agent_started" => Event::AgentStarted,
            "agent_exited" => Event::AgentExited {
                status: code(words[1]),
            },
            "agent_failed" => Event::AgentFailed {
                reason: words[1..].join(" "),
            },
            "ops" => self.op(Request::Ops),
            "judge" => {
                let (grant, payload, _) = self.call(&words[1..]);
                self.op(Request::Judge { grant, payload })
            }
            "submit" => {
                let (grant, payload, finalize) = self.call(&words[1..]);
                let request = Request::Submit {
                    grant,
                    finalize,
                    payload,
                };
                self.op(request)
            }
            "status" => self.op(Request::Status {
                job: words[1].parse().unwrap(),
            }),
            "judge_started" => Event::JudgeStarted { job: job() },
            // A judge's exit code is 0 unless the script gives one.
            "judge_exited" => Event::JudgeExited {
                job: job(),
                status: words.get(2).map_or(ExitStatus::Code(0), |word| code(word)),
            },
            "judge_timed_out" => Event::JudgeTimedOut { job: job() },
            "collected" => {
                let slot = self.workflow.call_slot(self.jobs[&job()]);
                let spec = &self.workflow.judge(slot.judge()).result;
                let result = JudgeResult::parse(spec, words[2].as_bytes());
                Event::JudgeCollected { job: job(), result }
            }
            "judge_failed" => Event::JudgeFailed {
                job: job(),
                reason: words[2].to_owned(),
            },
            "tick" => Event::Tick,
            other => panic!("no event is named `{other}`"),
        }
    }

    /// The grant, payload and finalize of a judge call or a submission: `<grant> <bytes>
    /// [final]`. The payload is named by the number the op will have.
    fn call(&self, words: &[&str]) -> (Name, PayloadRef, bool) {
        let words: Vec<&str> = words.iter().flat_map(|w| w.split(' ')).collect();
        let payload = PayloadRef {
            size: words[1].parse().unwrap(),
            digest: Sha256Digest::try_from(format!("sha256:{:064x}", self.ops + 1)).unwrap(),
        };
        let grant = Name::try_from(words[0].to_owned()).unwrap();
        (grant, payload, words.get(2) == Some(&"final"))
    }

    fn op(&mut self, request: Request) -> Event {
        self.ops += 1;
        Event::OpArrived {
            op: OpId(self.ops),
            request,
        }
    }

    /// Notes an event, the actions it called for, and when the next tick is due if that
    /// changed.
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

    fn action(&mut self, action: &Action) -> String {
        match action {
            Action::StartAgent => "start the agent".to_owned(),
            Action::Reply {
                op: OpId(op),
                reply:
                    reply @ (OpReply::Ops(_)
                    | OpReply::Judge(_)
                    | OpReply::Submit(_)
                    | OpReply::Status(_)),
            } => format!(
                "reply to op {op}: {}",
                serde_json::to_string(reply).unwrap()
            ),
            Action::Reply {
                op: OpId(op),
                reply: OpReply::Refused { message },
            } => format!("refuse op {op}: {message}"),
            Action::StartJudge {
                job,
                call,
                payload,
                timeout,
            } => {
                self.jobs.insert(*job, *call);
                let call = match call {
                    JudgeCall::Grant(grant) => self.workflow.judge_grant(*grant).name.as_str(),
                    JudgeCall::Score => "score",
                };
                let payload = payload_name(payload);
                format!(
                    "start job {} of {call} on {payload}, with a timeout of {timeout:?}",
                    job.0
                )
            }
            Action::StopJudge { job } => format!("stop job {}", job.0),
            Action::TearDownAgent => "tear down the agent".to_owned(),
            Action::Finish { outcome } => format!("finish: {}", outcome_text(outcome)),
            Action::Abandon => "abandon the run at its hard end".to_owned(),
        }
    }
}

/// A payload by the number of the op that sent it, which its test digest holds.
fn payload_name(payload: &PayloadRef) -> String {
    let digest = payload.digest.as_str().trim_start_matches("sha256:");
    let op = u64::from_str_radix(digest, 16).unwrap();
    format!("op {op}'s payload of {} bytes", payload.size)
}

fn outcome_text(outcome: &Outcome) -> String {
    let work_end = match &outcome.work_end {
        WorkEnd::Deadline => "the deadline".to_owned(),
        WorkEnd::Finalized => "a finalize".to_owned(),
        WorkEnd::AgentExited(status) => format!("the agent's exit, {status:?}"),
        WorkEnd::AgentFailed(reason) => format!("the agent's failure, {reason}"),
    };
    let score = match outcome.score() {
        Some(score) => format!("score {score}"),
        None => "no score".to_owned(),
    };
    let verdict = match &outcome.verdict {
        Verdict::NoSubmission => "no submission".to_owned(),
        Verdict::Judged { submission, result } => {
            let returned = json!(result.returned());
            format!(
                "{} judged, returning {returned}",
                submission_text(submission)
            )
        }
        Verdict::NoResult {
            submission,
            failure,
        } => format!("{} got no result, {failure:?}", submission_text(submission)),
    };
    format!("{score}; work ended at {work_end}; {verdict}")
}

fn submission_text(submission: &Submission) -> String {
    let kind = if submission.finalized {
        "finalized"
    } else {
        "stored"
    };
    format!(
        "the {kind} submission, {},",
        payload_name(&submission.payload)
    )
}

#[test]
fn whole_run() {
    check(
        |_| {},
        r#"
        0 agent_started
        1000 ops
        2000 judge train 10240
        2100 status 0
        2500 judge_started 0
        3000 status 0
        9500 judge_exited 0
        9600 collected 0 {"raw_score": 0, "metrics": {"solved": 0}, "summary": "for the operator"}
        10000 status 0
        11000 submit submit 10240
        12000 submit submit 10240 final
        12100 agent_exited 0
        13500 judge_started 1
        20000 judge_exited 1
        20100 collected 1 {"raw_score": 0, "metrics": {"solved": 0}}
        "#,
        &expect![[r#"
            start
              start the agent
              (tick due at 18010000)
            0 agent_started
              (tick due at 18000000)
            1000 ops
              reply to op 1: {"kind":"ops","remaining_seconds":17999,"resources":{"cpu_millis":8000,"memory_bytes":34359738368},"grants":[{"kind":"judge","name":"train","max_bytes":4194304,"calls":100,"cumulative_seconds":12000,"call_timeout_seconds":12000},{"kind":"submit","name":"submit","max_bytes":4194304}]}
            2000 judge train 10240
              reply to op 2: {"kind":"judge","job":0,"remaining":{"calls":99,"seconds":12000}}
              start job 0 of train on op 2's payload of 10240 bytes, with a timeout of 12000s
            2100 status 0
              reply to op 3: {"kind":"status","job":0,"state":"queued","remaining":{"calls":99,"seconds":12000}}
            2500 judge_started 0
            3000 status 0
              reply to op 4: {"kind":"status","job":0,"state":"running","remaining":{"calls":99,"seconds":12000}}
            9500 judge_exited 0
            9600 collected 0 {"raw_score": 0, "metrics": {"solved": 0}, "summary": "for the operator"}
            10000 status 0
              reply to op 5: {"kind":"status","job":0,"state":"completed","returned":{"metrics":{"solved":0},"raw_score":0},"scored":true,"remaining":{"calls":99,"seconds":11993}}
            11000 submit submit 10240
              reply to op 6: {"kind":"submit","state":"stored","remaining_seconds":17989}
            12000 submit submit 10240 final
              reply to op 7: {"kind":"submit","state":"accepted","remaining_seconds":60}
              (tick due at 72000)
            12100 agent_exited 0
              tear down the agent
              start job 1 of score on op 7's payload of 10240 bytes, with a timeout of 120s
              (tick due at 18210000)
            13500 judge_started 1
            20000 judge_exited 1
            20100 collected 1 {"raw_score": 0, "metrics": {"solved": 0}}
              finish: score 0; work ended at the agent's exit, Code(0); the finalized submission, op 7's payload of 10240 bytes, judged, returning {"metrics":{"solved":0},"raw_score":0}
        "#]],
    );
}

#[test]
fn quotas() {
    check(
        |ir| {
            let train = &mut ir["roles"][0]["grants"][0]["judge"];
            train["cumulative_seconds"] = json!(10);
            train["call_timeout_seconds"] = json!(6);
        },
        r#"
        0 agent_started
        1000 judge train 4194305
        1000 judge nothing 10
        1000 submit train 10
        1000 judge submit 10
        1000 submit submit 4194305
        1000 judge train 10
        2000 judge_started 0
        6000 judge_exited 0
        6100 collected 0 {"raw_score": null, "failure": {"code": "wrong_answer"}, "summary": "for the operator"}
        6200 status 0
        7000 judge train 10
        7500 judge_started 1
        13600 judge_timed_out 1
        13700 collected 1 {"raw_score": 5}
        14000 status 1
        14100 judge train 10
        14200 submit submit 0
        "#,
        &expect![[r#"
            start
              start the agent
              (tick due at 18010000)
            0 agent_started
              (tick due at 18000000)
            1000 judge train 4194305
              refuse op 1: the payload is 4194305 bytes, over `train`'s limit of 4194304: send a smaller one
            1000 judge nothing 10
              refuse op 2: you hold no grant named `nothing`: call one that `ops` lists
            1000 submit train 10
              refuse op 3: `train` is a judge grant, not a submit grant: judge with it instead
            1000 judge submit 10
              refuse op 4: `submit` is a submit grant, not a judge grant: submit to it instead
            1000 submit submit 4194305
              refuse op 5: the payload is 4194305 bytes, over `submit`'s limit of 4194304: send a smaller one
            1000 judge train 10
              reply to op 6: {"kind":"judge","job":0,"remaining":{"calls":99,"seconds":10}}
              start job 0 of train on op 6's payload of 10 bytes, with a timeout of 6s
            2000 judge_started 0
            6000 judge_exited 0
            6100 collected 0 {"raw_score": null, "failure": {"code": "wrong_answer"}, "summary": "for the operator"}
            6200 status 0
              reply to op 7: {"kind":"status","job":0,"state":"completed","returned":{"failure":{"code":"wrong_answer"},"raw_score":null},"scored":false,"remaining":{"calls":99,"seconds":6}}
            7000 judge train 10
              reply to op 8: {"kind":"judge","job":1,"remaining":{"calls":98,"seconds":6}}
              start job 1 of train on op 8's payload of 10 bytes, with a timeout of 6s
            7500 judge_started 1
            13600 judge_timed_out 1
            13700 collected 1 {"raw_score": 5}
            14000 status 1
              reply to op 9: {"kind":"status","job":1,"state":"errored","failure":{"code":"judge_timeout","message":"the judge ran past its timeout"},"remaining":{"calls":98,"seconds":0}}
            14100 judge train 10
              refuse op 10: `train` has no judge time left of the 10 s it allows: make no more calls to it
            14200 submit submit 0
              refuse op 11: the payload is empty: send `submit` at least one byte
        "#]],
    );
}

#[test]
fn one_call_at_a_time() {
    check(
        |ir| ir["roles"][0]["grants"][0]["judge"]["calls"] = json!(2),
        r#"
        0 agent_started
        1000 judge train 10
        1100 judge train 10
        1200 status 0
        2000 judge_started 0
        3000 judge_exited 0
        3100 judge train 10
        3200 collected 0 {}
        3300 status 0
        3400 judge train 10
        4000 judge_failed 1 the judge's sandbox did not connect
        4100 judge train 10
        4200 status 1
        "#,
        &expect![[r#"
            start
              start the agent
              (tick due at 18010000)
            0 agent_started
              (tick due at 18000000)
            1000 judge train 10
              reply to op 1: {"kind":"judge","job":0,"remaining":{"calls":1,"seconds":12000}}
              start job 0 of train on op 1's payload of 10 bytes, with a timeout of 12000s
            1100 judge train 10
              refuse op 2: `train` runs one call at a time, and already has one queued or running: call again once it has finished; this call is not charged
            1200 status 0
              reply to op 3: {"kind":"status","job":0,"state":"queued","remaining":{"calls":1,"seconds":12000}}
            2000 judge_started 0
            3000 judge_exited 0
            3100 judge train 10
              refuse op 4: `train` runs one call at a time, and already has one queued or running: call again once it has finished; this call is not charged
            3200 collected 0 {}
            3300 status 0
              reply to op 5: {"kind":"status","job":0,"state":"completed","returned":{},"scored":false,"remaining":{"calls":1,"seconds":11999}}
            3400 judge train 10
              reply to op 6: {"kind":"judge","job":1,"remaining":{"calls":0,"seconds":11999}}
              start job 1 of train on op 6's payload of 10 bytes, with a timeout of 11999s
            4000 judge_failed 1 the judge's sandbox did not connect
            4100 judge train 10
              refuse op 7: `train` has no calls left of the 2 it allows: make no more calls to it
            4200 status 1
              reply to op 8: {"kind":"status","job":1,"state":"errored","failure":{"code":"judge_error","message":"the judge failed before giving a result"},"remaining":{"calls":0,"seconds":11999}}
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
        |ir| ir["roles"][0]["runtime"]["agent"]["deadline_seconds"] = json!(60),
        r#"
        0 agent_started
        50000 submit submit 10
        55000 judge train 10
        55500 judge_started 0
        59999 tick
        59999 model
        60000 model
        60000 tick
        60100 status 0
        60200 agent_exited 137
        60400 judge_failed 0 stopped
        60500 judge_started 1
        180500 judge_timed_out 1
        180600 collected 1 {"raw_score": 7}
        "#,
        &expect![[r#"
            start
              start the agent
              (tick due at 70000)
            0 agent_started
              (tick due at 60000)
            50000 submit submit 10
              reply to op 1: {"kind":"submit","state":"stored","remaining_seconds":10}
            55000 judge train 10
              reply to op 2: {"kind":"judge","job":0,"remaining":{"calls":99,"seconds":12000}}
              start job 0 of train on op 2's payload of 10 bytes, with a timeout of 5s
            55500 judge_started 0
            59999 tick
            59999 model
              model requests allowed
            60000 model
              model requests refused
            60000 tick
              stop job 0
              tear down the agent
              (tick due at 270000)
            60100 status 0
              refuse op 3: the work phase has ended: make no more ops
            60200 agent_exited 137
            60400 judge_failed 0 stopped
              start job 1 of score on op 1's payload of 10 bytes, with a timeout of 120s
            60500 judge_started 1
            180500 judge_timed_out 1
            180600 collected 1 {"raw_score": 7}
              finish: no score; work ended at the deadline; the stored submission, op 1's payload of 10 bytes, got no result, Timeout
        "#]],
    );
    check(
        |ir| ir["roles"][0]["runtime"]["agent"]["deadline_seconds"] = json!(60),
        r"
        0 agent_started
        50000 submit submit 10
        59999 judge train 10
        60000 judge_failed 0 this runtime runs no judge call yet
        ",
        &expect![[r#"
            start
              start the agent
              (tick due at 70000)
            0 agent_started
              (tick due at 60000)
            50000 submit submit 10
              reply to op 1: {"kind":"submit","state":"stored","remaining_seconds":10}
            59999 judge train 10
              reply to op 2: {"kind":"judge","job":0,"remaining":{"calls":99,"seconds":12000}}
              start job 0 of train on op 2's payload of 10 bytes, with a timeout of 1ms
            60000 judge_failed 0 this runtime runs no judge call yet
              stop job 0
              tear down the agent
              start job 1 of score on op 1's payload of 10 bytes, with a timeout of 120s
              (tick due at 270000)
        "#]],
    );
}

#[test]
fn deadline_from_the_agent_start() {
    let deadline =
        |ir: &mut Value| ir["roles"][0]["runtime"]["agent"]["deadline_seconds"] = json!(60);
    check(
        deadline,
        r"
        8000 agent_started
        8000 ops
        9000 agent_started
        67999 tick
        68000 tick
        ",
        &expect![[r#"
            start
              start the agent
              (tick due at 70000)
            8000 agent_started
              (tick due at 68000)
            8000 ops
              reply to op 1: {"kind":"ops","remaining_seconds":60,"resources":{"cpu_millis":8000,"memory_bytes":34359738368},"grants":[{"kind":"judge","name":"train","max_bytes":4194304,"calls":100,"cumulative_seconds":12000,"call_timeout_seconds":12000},{"kind":"submit","name":"submit","max_bytes":4194304}]}
            9000 agent_started
            67999 tick
            68000 tick
              tear down the agent
              finish: no score; work ended at the deadline; no submission
              (tick due at 270000)
        "#]],
    );
    check(
        deadline,
        r"
        12000 agent_started
        12000 ops
        70000 tick
        ",
        &expect![[r#"
            start
              start the agent
              (tick due at 70000)
            12000 agent_started
            12000 ops
              reply to op 1: {"kind":"ops","remaining_seconds":58,"resources":{"cpu_millis":8000,"memory_bytes":34359738368},"grants":[{"kind":"judge","name":"train","max_bytes":4194304,"calls":100,"cumulative_seconds":12000,"call_timeout_seconds":12000},{"kind":"submit","name":"submit","max_bytes":4194304}]}
            70000 tick
              tear down the agent
              finish: no score; work ended at the deadline; no submission
              (tick due at 270000)
        "#]],
    );
    check(
        deadline,
        r"
        70000 tick
        ",
        &expect![[r#"
            start
              start the agent
              (tick due at 70000)
            70000 tick
              tear down the agent
              finish: no score; work ended at the deadline; no submission
              (tick due at 270000)
        "#]],
    );
}

#[test]
fn hard_end_covers_the_run() {
    // Each step takes as long as it may: the agent starts at its startup and works to the
    // deadline, its teardown takes the grace and its startup again, and the final judge its own.
    check(
        |ir| ir["roles"][0]["runtime"]["agent"]["deadline_seconds"] = json!(60),
        r#"
        10000 agent_started
        69999 submit submit 10
        70000 tick
        150000 judge_started 0
        269999 judge_exited 0
        269999 collected 0 {"raw_score": 7}
        "#,
        &expect![[r#"
            start
              start the agent
              (tick due at 70000)
            10000 agent_started
            69999 submit submit 10
              reply to op 1: {"kind":"submit","state":"stored","remaining_seconds":0}
            70000 tick
              tear down the agent
              start job 0 of score on op 1's payload of 10 bytes, with a timeout of 120s
              (tick due at 270000)
            150000 judge_started 0
            269999 judge_exited 0
            269999 collected 0 {"raw_score": 7}
              finish: score 7; work ended at the deadline; the stored submission, op 1's payload of 10 bytes, judged, returning {"raw_score":7}
        "#]],
    );
}

#[test]
fn agent_ends() {
    check(
        |_| {},
        r#"
        0 agent_started
        1000 judge train 10
        1500 judge_started 0
        3000 judge_exited 0
        3100 submit submit 10
        3200 agent_exited 0
        3400 collected 0 {"raw_score": 1}
        3500 judge_started 1
        4000 judge_failed 0 its report came again
        5000 judge_exited 1
        5100 collected 1 {"raw_score": "high"}
        "#,
        &expect![[r#"
            start
              start the agent
              (tick due at 18010000)
            0 agent_started
              (tick due at 18000000)
            1000 judge train 10
              reply to op 1: {"kind":"judge","job":0,"remaining":{"calls":99,"seconds":12000}}
              start job 0 of train on op 1's payload of 10 bytes, with a timeout of 12000s
            1500 judge_started 0
            3000 judge_exited 0
            3100 submit submit 10
              reply to op 2: {"kind":"submit","state":"stored","remaining_seconds":17996}
            3200 agent_exited 0
              tear down the agent
              (tick due at 18210000)
            3400 collected 0 {"raw_score": 1}
              start job 1 of score on op 2's payload of 10 bytes, with a timeout of 120s
            3500 judge_started 1
            4000 judge_failed 0 its report came again
            5000 judge_exited 1
            5100 collected 1 {"raw_score": "high"}
              finish: no score; work ended at the agent's exit, Code(0); the stored submission, op 2's payload of 10 bytes, got no result, InvalidResult("the score at `/raw_score` is not a number: write a number there, or null")
        "#]],
    );
    check(
        |_| {},
        r"
        30000 agent_failed the agent's sandbox did not connect
        ",
        &expect![[r#"
            start
              start the agent
              (tick due at 18010000)
            30000 agent_failed the agent's sandbox did not connect
              tear down the agent
              finish: no score; work ended at the agent's failure, the agent's sandbox did not connect; no submission
              (tick due at 18210000)
        "#]],
    );
}

#[test]
fn hard_end() {
    let deadline =
        |ir: &mut Value| ir["roles"][0]["runtime"]["agent"]["deadline_seconds"] = json!(60);
    check(
        deadline,
        r"
        0 agent_started
        1000 judge train 10
        1500 judge_started 0
        270000 ops
        270100 judge_exited 0
        ",
        &expect![[r#"
            start
              start the agent
              (tick due at 70000)
            0 agent_started
              (tick due at 60000)
            1000 judge train 10
              reply to op 1: {"kind":"judge","job":0,"remaining":{"calls":99,"seconds":12000}}
              start job 0 of train on op 1's payload of 10 bytes, with a timeout of 59s
            1500 judge_started 0
            270000 ops
              abandon the run at its hard end
              refuse op 2: the run has passed its hard end: make no more ops
              (tick due at 270000)
            270100 judge_exited 0
        "#]],
    );
    check(
        deadline,
        r"
        0 agent_started
        1000 submit submit 10 final
        270000 tick
        ",
        &expect![[r#"
            start
              start the agent
              (tick due at 70000)
            0 agent_started
              (tick due at 60000)
            1000 submit submit 10 final
              reply to op 1: {"kind":"submit","state":"accepted","remaining_seconds":59}
            270000 tick
              abandon the run at its hard end
              (tick due at 270000)
        "#]],
    );
}

#[test]
fn hard_end_after_work() {
    check(
        |ir| ir["roles"][0]["runtime"]["agent"]["deadline_seconds"] = json!(60),
        r"
        0 agent_started
        1000 judge train 10
        1500 judge_started 0
        60000 tick
        269999 tick
        270000 tick
        ",
        &expect![[r#"
            start
              start the agent
              (tick due at 70000)
            0 agent_started
              (tick due at 60000)
            1000 judge train 10
              reply to op 1: {"kind":"judge","job":0,"remaining":{"calls":99,"seconds":12000}}
              start job 0 of train on op 1's payload of 10 bytes, with a timeout of 59s
            1500 judge_started 0
            60000 tick
              stop job 0
              tear down the agent
              (tick due at 270000)
            269999 tick
            270000 tick
              abandon the run at its hard end
        "#]],
    );
    check(
        |ir| {
            let agent = &mut ir["roles"][0]["runtime"]["agent"];
            agent["deadline_seconds"] = json!(60);
            agent["finalize_grace_seconds"] = json!(30);
        },
        r#"
        0 agent_started
        1000 submit submit 10
        2000 agent_exited 0
        2500 judge_started 0
        122500 judge_timed_out 0
        239999 tick
        240000 collected 0 {"raw_score": 7}
        "#,
        &expect![[r#"
            start
              start the agent
              (tick due at 70000)
            0 agent_started
              (tick due at 60000)
            1000 submit submit 10
              reply to op 1: {"kind":"submit","state":"stored","remaining_seconds":59}
            2000 agent_exited 0
              tear down the agent
              start job 0 of score on op 1's payload of 10 bytes, with a timeout of 120s
              (tick due at 240000)
            2500 judge_started 0
            122500 judge_timed_out 0
            239999 tick
            240000 collected 0 {"raw_score": 7}
              abandon the run at its hard end
        "#]],
    );
}

#[test]
fn hard_end_bounds() {
    check(
        |ir| ir["roles"][0]["runtime"]["agent"]["deadline_seconds"] = json!(60),
        r"
        0 agent_started
        1000 agent_exited 0
        270000 tick
        ",
        &expect![[r#"
            start
              start the agent
              (tick due at 70000)
            0 agent_started
              (tick due at 60000)
            1000 agent_exited 0
              tear down the agent
              finish: no score; work ended at the agent's exit, Code(0); no submission
              (tick due at 270000)
            270000 tick
        "#]],
    );
    check(
        |ir| ir["roles"][0]["runtime"]["agent"]["deadline_seconds"] = json!(5),
        r"
        0 agent_started
        1000 submit submit 10
        5000 tick
        214999 tick
        215000 tick
        ",
        &expect![[r#"
            start
              start the agent
              (tick due at 15000)
            0 agent_started
              (tick due at 5000)
            1000 submit submit 10
              reply to op 1: {"kind":"submit","state":"stored","remaining_seconds":4}
            5000 tick
              tear down the agent
              start job 0 of score on op 1's payload of 10 bytes, with a timeout of 120s
              (tick due at 215000)
            214999 tick
            215000 tick
              abandon the run at its hard end
        "#]],
    );
    check(
        |ir| ir["roles"][0]["runtime"]["agent"]["deadline_seconds"] = json!(u64::MAX),
        r"
        0 agent_started
        1000 submit submit 10
        2000 agent_exited 0
        ",
        &expect![[r#"
            start
              start the agent
              (tick due at 18446744073709551615999.999999)
            0 agent_started
              (tick due at 18446744073709551615000)
            1000 submit submit 10
              reply to op 1: {"kind":"submit","state":"stored","remaining_seconds":18446744073709551614}
            2000 agent_exited 0
              tear down the agent
              start job 0 of score on op 1's payload of 10 bytes, with a timeout of 120s
              (tick due at 18446744073709551615999.999999)
        "#]],
    );
}

#[test]
fn hard_end_startups() {
    // In the slow class, the agent's startup counts twice.
    check(
        |ir| {
            ir["roles"][0]["runtime"]["agent"]["deadline_seconds"] = json!(5);
            ir["roles"][0]["needs"]["memory_bytes"] = json!(SLOW_MEMORY_BYTES);
        },
        r"
        0 agent_started
        1000 submit submit 10
        5000 tick
        254999 tick
        255000 tick
        ",
        &expect![[r#"
            start
              start the agent
              (tick due at 35000)
            0 agent_started
              (tick due at 5000)
            1000 submit submit 10
              reply to op 1: {"kind":"submit","state":"stored","remaining_seconds":4}
            5000 tick
              tear down the agent
              start job 0 of score on op 1's payload of 10 bytes, with a timeout of 120s
              (tick due at 255000)
            254999 tick
            255000 tick
              abandon the run at its hard end
        "#]],
    );
    // In the slow class, the final judge's startup counts once.
    check(
        |ir| {
            ir["roles"][0]["runtime"]["agent"]["deadline_seconds"] = json!(60);
            ir["roles"][2]["needs"]["memory_bytes"] = json!(SLOW_MEMORY_BYTES);
        },
        r"
        0 agent_started
        1000 submit submit 10
        60000 tick
        289999 tick
        290000 tick
        ",
        &expect![[r#"
            start
              start the agent
              (tick due at 70000)
            0 agent_started
              (tick due at 60000)
            1000 submit submit 10
              reply to op 1: {"kind":"submit","state":"stored","remaining_seconds":59}
            60000 tick
              tear down the agent
              start job 0 of score on op 1's payload of 10 bytes, with a timeout of 120s
              (tick due at 290000)
            289999 tick
            290000 tick
              abandon the run at its hard end
        "#]],
    );
}

#[test]
fn reports_for_the_log() {
    check(
        |_| {},
        r#"
        0 agent_started
        1000 judge train 10
        1500 judge_started 0
        3000 judge_exited 0 1
        3100 collected 0 {"raw_score": 1}
        3200 status 0
        "#,
        &expect![[r#"
            start
              start the agent
              (tick due at 18010000)
            0 agent_started
              (tick due at 18000000)
            1000 judge train 10
              reply to op 1: {"kind":"judge","job":0,"remaining":{"calls":99,"seconds":12000}}
              start job 0 of train on op 1's payload of 10 bytes, with a timeout of 12000s
            1500 judge_started 0
            3000 judge_exited 0 1
            3100 collected 0 {"raw_score": 1}
            3200 status 0
              reply to op 2: {"kind":"status","job":0,"state":"completed","returned":{"raw_score":1},"scored":true,"remaining":{"calls":99,"seconds":11998}}
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
        0 agent_started
        1000 judge train 10
        1500 judge_started 0
        2000 submit submit 10 final
        2100 ops
        2200 status 0
        2300 submit submit 10
        61999 tick
        61999 model
        62000 model
        62000 tick
        62200 judge_failed 0 stopped
        ",
        &expect![[r#"
            start
              start the agent
              (tick due at 18010000)
            0 agent_started
              (tick due at 18000000)
            1000 judge train 10
              reply to op 1: {"kind":"judge","job":0,"remaining":{"calls":99,"seconds":12000}}
              start job 0 of train on op 1's payload of 10 bytes, with a timeout of 12000s
            1500 judge_started 0
            2000 submit submit 10 final
              reply to op 2: {"kind":"submit","state":"accepted","remaining_seconds":60}
              (tick due at 62000)
            2100 ops
              refuse op 3: a final submission was accepted: make no more ops
            2200 status 0
              refuse op 4: a final submission was accepted: make no more ops
            2300 submit submit 10
              refuse op 5: a final submission was accepted: make no more ops
            61999 tick
            61999 model
              model requests allowed
            62000 model
              model requests refused
            62000 tick
              stop job 0
              tear down the agent
              (tick due at 18210000)
            62200 judge_failed 0 stopped
              start job 1 of score on op 2's payload of 10 bytes, with a timeout of 120s
        "#]],
    );
    check(
        |ir| ir["roles"][0]["runtime"]["agent"]["finalize_grace_seconds"] = json!(0),
        r"
        0 agent_started
        2000 submit submit 10 final
        2000 model
        2100 ops
        ",
        &expect![[r#"
            start
              start the agent
              (tick due at 18010000)
            0 agent_started
              (tick due at 18000000)
            2000 submit submit 10 final
              reply to op 1: {"kind":"submit","state":"accepted","remaining_seconds":0}
              tear down the agent
              start job 0 of score on op 1's payload of 10 bytes, with a timeout of 120s
              (tick due at 18150000)
            2000 model
              model requests refused
            2100 ops
              refuse op 2: a final submission was accepted: make no more ops
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
        |ir| ir["roles"][0]["runtime"]["agent"]["deadline_seconds"] = json!(30),
        r"
        0 agent_started
        10000 submit submit 10 final
        29999 model
        30000 model
        ",
        &expect![[r#"
            start
              start the agent
              (tick due at 40000)
            0 agent_started
              (tick due at 30000)
            10000 submit submit 10 final
              reply to op 1: {"kind":"submit","state":"accepted","remaining_seconds":20}
            29999 model
              model requests allowed
            30000 model
              model requests refused
        "#]],
    );
    let grace = |ir: &mut Value| {
        let agent = &mut ir["roles"][0]["runtime"]["agent"];
        agent["deadline_seconds"] = json!(30);
        agent["finalize_grace_seconds"] = json!(10);
    };
    check(
        grace,
        r#"
        0 agent_started
        19950 submit submit 10 final
        30100 tick
        30150 ops
        30200 judge_started 0
        30300 judge_exited 0
        30400 collected 0 {"raw_score": 1}
        "#,
        &expect![[r#"
            start
              start the agent
              (tick due at 40000)
            0 agent_started
              (tick due at 30000)
            19950 submit submit 10 final
              reply to op 1: {"kind":"submit","state":"accepted","remaining_seconds":10}
              (tick due at 29950)
            30100 tick
              tear down the agent
              start job 0 of score on op 1's payload of 10 bytes, with a timeout of 120s
              (tick due at 190000)
            30150 ops
              refuse op 2: a final submission was accepted: make no more ops
            30200 judge_started 0
            30300 judge_exited 0
            30400 collected 0 {"raw_score": 1}
              finish: score 1; work ended at a finalize; the finalized submission, op 1's payload of 10 bytes, judged, returning {"raw_score":1}
        "#]],
    );
    check(
        grace,
        r"
        0 agent_started
        20000 submit submit 10 final
        30000 tick
        30000 ops
        ",
        &expect![[r#"
            start
              start the agent
              (tick due at 40000)
            0 agent_started
              (tick due at 30000)
            20000 submit submit 10 final
              reply to op 1: {"kind":"submit","state":"accepted","remaining_seconds":10}
            30000 tick
              tear down the agent
              start job 0 of score on op 1's payload of 10 bytes, with a timeout of 120s
              (tick due at 190000)
            30000 ops
              refuse op 2: the work phase has ended: make no more ops
        "#]],
    );
}

#[test]
fn unknown_jobs() {
    check(
        |_| {},
        r"
        0 agent_started
        1000 judge train 10
        1400 status 2
        1500 status 1
        ",
        &expect![[r#"
            start
              start the agent
              (tick due at 18010000)
            0 agent_started
              (tick due at 18000000)
            1000 judge train 10
              reply to op 1: {"kind":"judge","job":0,"remaining":{"calls":99,"seconds":12000}}
              start job 0 of train on op 1's payload of 10 bytes, with a timeout of 12000s
            1400 status 2
              refuse op 2: you have no job 2: ask about a job that one of your calls returned
            1500 status 1
              refuse op 3: you have no job 1: ask about a job that one of your calls returned
        "#]],
    );
}
