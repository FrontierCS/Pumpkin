//! The driver's decision core, fed event scripts on the LWE workflow that `pumpkin-schema`
//! tests parsing with, or on variants of it. Each test shows what the core did after each
//! event, and when it next wants a tick whenever that changes.
//!
//! A script has one event per line: the time in milliseconds since the run started, then one
//! of these:
//!
//! - `agent_exited <code>`, `agent_failed <reason>`;
//! - `ops`, `judge <grant> <bytes>`, `submit <grant> <bytes> [final]` and `status <job>`,
//!   each an op from the agent, whose payload is named by the op's number;
//! - `judge_started <job>`, `judge_exited <job>`, `judge_timed_out <job>`,
//!   `collected <job> <result.json>`, `judge_failed <job> <reason>`;
//! - `tick`.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test helper fails the test that calls it"
)]

use std::collections::HashMap;
use std::fmt::Write as _;
use std::time::Duration;

use expect_test::{Expect, expect};
use pumpkin::driver::core::{
    Action, Core, Event, JobId, JudgeResult, OpId, Outcome, PayloadRef, Request, Submission,
    Verdict, WorkEnd,
};
use pumpkin_schema::ir::{self, Name, Sha256Digest};
use pumpkin_schema::sandbox::ExitStatus;
use pumpkin_schema::sandbox::op::OpReply;
use pumpkin_schema::workflow::{JudgeCall, Workflow};
use serde_json::{Value, json};

const LWE: &str = include_str!("../../../pumpkin-schema/tests/fixtures/lwe.json");

/// Runs `script` on the LWE workflow with `edit` applied, and compares what the core did
/// with `expect`.
fn check(edit: impl FnOnce(&mut Value), script: &str, expect: &Expect) {
    let mut ir: Value = serde_json::from_str(LWE).unwrap();
    edit(&mut ir);
    let ir: ir::Workflow = serde_json::from_value(ir).unwrap();
    let workflow = Workflow::parse(&ir).unwrap();
    let mut run = Run {
        workflow: &workflow,
        ops: 0,
        jobs: HashMap::new(),
        tick_due: None,
        transcript: String::new(),
    };
    let (mut core, actions) = Core::start(&workflow);
    run.note("start", &actions, core.tick_due());
    for line in script
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let (at, event) = line.split_once(' ').unwrap();
        let event = run.event(event);
        let actions = core.handle(Duration::from_millis(at.parse().unwrap()), event);
        run.note(line, &actions, core.tick_due());
    }
    expect.assert_eq(&run.transcript);
}

/// What a script needs to know as it runs.
struct Run<'w> {
    workflow: &'w Workflow,
    /// The ops made so far.
    ops: u64,
    /// What each job was made under, for its result's spec.
    jobs: HashMap<JobId, JudgeCall>,
    /// When the core last said it next wants a tick.
    tick_due: Option<Duration>,
    transcript: String,
}

impl Run<'_> {
    fn event(&mut self, event: &str) -> Event {
        let words: Vec<&str> = event.splitn(3, ' ').collect();
        let job = || JobId(words[1].parse().unwrap());
        let code = |word: &str| ExitStatus::Code(word.parse().unwrap());
        match words[0] {
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
            "judge_exited" => Event::JudgeExited { job: job() },
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
    fn note(&mut self, event: &str, actions: &[Action], tick_due: Option<Duration>) {
        writeln!(self.transcript, "{event}").unwrap();
        for action in actions {
            let action = self.action(action);
            writeln!(self.transcript, "  {action}").unwrap();
        }
        if tick_due != self.tick_due {
            self.tick_due = tick_due;
            let due = tick_due.map_or("no tick due".to_owned(), |due| {
                format!("tick due at {}", due.as_millis())
            });
            writeln!(self.transcript, "  ({due})").unwrap();
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
            let state = if result.passed() { "passed" } else { "failed" };
            let returned = json!(result.returned());
            format!(
                "{} {state}, returning {returned}",
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
fn lwe() {
    check(
        |_| {},
        r#"
        1000 ops
        2000 judge train 10240
        2100 status 1
        2500 judge_started 1
        3000 status 1
        9500 judge_exited 1
        9600 collected 1 {"status": "passed", "raw_score": 0, "metrics": {"solved": 0}, "summary": "for the operator"}
        10000 status 1
        11000 submit submit 10240
        12000 submit submit 10240 final
        12100 agent_exited 0
        13500 judge_started 2
        20000 judge_exited 2
        20100 collected 2 {"status": "passed", "raw_score": 0, "metrics": {"solved": 0}}
        "#,
        &expect![[r#"
            start
              start the agent
              (tick due at 18000000)
            1000 ops
              reply to op 1: {"kind":"ops","remaining_seconds":17999,"resources":{"cpu_millis":8000,"memory_bytes":34359738368},"grants":[{"kind":"judge","name":"train","max_bytes":4194304,"calls":100,"cumulative_seconds":12000,"call_timeout_seconds":12000},{"kind":"submit","name":"submit","max_bytes":4194304}]}
            2000 judge train 10240
              reply to op 2: {"kind":"judge","job":1,"remaining":{"calls":99,"seconds":12000}}
              start job 1 of train on op 2's payload of 10240 bytes, with a timeout of 12000s
            2100 status 1
              reply to op 3: {"kind":"status","job":1,"state":"queued","remaining":{"calls":99,"seconds":12000}}
            2500 judge_started 1
            3000 status 1
              reply to op 4: {"kind":"status","job":1,"state":"running","remaining":{"calls":99,"seconds":12000}}
            9500 judge_exited 1
            9600 collected 1 {"status": "passed", "raw_score": 0, "metrics": {"solved": 0}, "summary": "for the operator"}
            10000 status 1
              reply to op 5: {"kind":"status","job":1,"state":"passed","returned":{"metrics":{"solved":0},"raw_score":0},"remaining":{"calls":99,"seconds":11993}}
            11000 submit submit 10240
              reply to op 6: {"kind":"submit","state":"stored","remaining_seconds":17989}
            12000 submit submit 10240 final
              reply to op 7: {"kind":"submit","state":"accepted","remaining_seconds":60}
              (tick due at 72000)
            12100 agent_exited 0
              tear down the agent
              start job 2 of score on op 7's payload of 10240 bytes, with a timeout of 120s
              (no tick due)
            13500 judge_started 2
            20000 judge_exited 2
            20100 collected 2 {"status": "passed", "raw_score": 0, "metrics": {"solved": 0}}
              finish: score 0; work ended at the agent's exit, Code(0); the finalized submission, op 7's payload of 10240 bytes, passed, returning {"metrics":{"solved":0},"raw_score":0}
        "#]],
    );
}

/// A train call's timeout is the smaller of its call timeout and the cumulative time left; a
/// call costs the time its command ran, timed out or not.
#[test]
fn quotas() {
    check(
        |ir| {
            let train = &mut ir["main"]["solo_then_judge"]["grants"][0]["judge"];
            train["cumulative_seconds"] = json!(10);
            train["call_timeout_seconds"] = json!(6);
        },
        r#"
        1000 judge train 4194305
        1000 judge nothing 10
        1000 submit train 10
        1000 judge submit 10
        1000 submit submit 4194305
        1000 judge train 10
        2000 judge_started 1
        6000 judge_exited 1
        6100 collected 1 {"status": "failed", "raw_score": null, "failure": {"code": "wrong_answer"}, "summary": "for the operator"}
        6200 status 1
        7000 judge train 10
        7500 judge_started 2
        13600 judge_timed_out 2
        13700 collected 2 {"status": "passed", "raw_score": 5}
        14000 status 2
        14100 judge train 10
        14200 submit submit 0
        "#,
        &expect![[r#"
            start
              start the agent
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
              reply to op 6: {"kind":"judge","job":1,"remaining":{"calls":99,"seconds":10}}
              start job 1 of train on op 6's payload of 10 bytes, with a timeout of 6s
            2000 judge_started 1
            6000 judge_exited 1
            6100 collected 1 {"status": "failed", "raw_score": null, "failure": {"code": "wrong_answer"}, "summary": "for the operator"}
            6200 status 1
              reply to op 7: {"kind":"status","job":1,"state":"failed","returned":{"failure":{"code":"wrong_answer"},"raw_score":null},"remaining":{"calls":99,"seconds":6}}
            7000 judge train 10
              reply to op 8: {"kind":"judge","job":2,"remaining":{"calls":98,"seconds":6}}
              start job 2 of train on op 8's payload of 10 bytes, with a timeout of 6s
            7500 judge_started 2
            13600 judge_timed_out 2
            13700 collected 2 {"status": "passed", "raw_score": 5}
            14000 status 2
              reply to op 9: {"kind":"status","job":2,"state":"failed","failure":{"code":"judge_timeout","message":"the judge ran past its timeout"},"remaining":{"calls":98,"seconds":0}}
            14100 judge train 10
              refuse op 10: `train` has no judge time left of the 10 s it allows: make no more calls to it
            14200 submit submit 0
              refuse op 11: the payload is empty: send `submit` at least one byte
        "#]],
    );
}

/// A grant runs one call at a time: a call made while another is queued or running is refused
/// and not charged. A call counts once accepted, even if its job fails before its command
/// starts.
#[test]
fn one_call_at_a_time() {
    check(
        |ir| ir["main"]["solo_then_judge"]["grants"][0]["judge"]["calls"] = json!(2),
        r#"
        1000 judge train 10
        1100 judge train 10
        1200 status 1
        2000 judge_started 1
        3000 judge_exited 1
        3100 judge train 10
        3200 collected 1 {"status": "passed"}
        3300 status 1
        3400 judge train 10
        4000 judge_failed 2 the judge's sandbox did not connect
        4100 judge train 10
        4200 status 2
        "#,
        &expect![[r#"
            start
              start the agent
              (tick due at 18000000)
            1000 judge train 10
              reply to op 1: {"kind":"judge","job":1,"remaining":{"calls":1,"seconds":12000}}
              start job 1 of train on op 1's payload of 10 bytes, with a timeout of 12000s
            1100 judge train 10
              refuse op 2: `train` runs one call at a time, and already has one queued or running: call again once it has finished; this call is not charged
            1200 status 1
              reply to op 3: {"kind":"status","job":1,"state":"queued","remaining":{"calls":1,"seconds":12000}}
            2000 judge_started 1
            3000 judge_exited 1
            3100 judge train 10
              refuse op 4: `train` runs one call at a time, and already has one queued or running: call again once it has finished; this call is not charged
            3200 collected 1 {"status": "passed"}
            3300 status 1
              reply to op 5: {"kind":"status","job":1,"state":"failed","failure":{"code":"invalid_result","message":"the judge wrote no valid result"},"remaining":{"calls":1,"seconds":11999}}
            3400 judge train 10
              reply to op 6: {"kind":"judge","job":2,"remaining":{"calls":0,"seconds":11999}}
              start job 2 of train on op 6's payload of 10 bytes, with a timeout of 11999s
            4000 judge_failed 2 the judge's sandbox did not connect
            4100 judge train 10
              refuse op 7: `train` has no calls left of the 2 it allows: make no more calls to it
            4200 status 2
              reply to op 8: {"kind":"status","job":2,"state":"failed","failure":{"code":"judge_error","message":"the judge failed before giving a result"},"remaining":{"calls":0,"seconds":11999}}
        "#]],
    );
}

/// The deadline ends the work phase, and bounds a train call's timeout before it. The final
/// judge waits until the agent and its stopped job are gone, and a final judge that times out
/// leaves no score.
#[test]
fn deadline() {
    check(
        |ir| ir["main"]["solo_then_judge"]["deadline_seconds"] = json!(60),
        r#"
        50000 submit submit 10
        55000 judge train 10
        55500 judge_started 1
        59999 tick
        60000 tick
        60100 status 1
        60200 agent_exited 137
        60400 judge_failed 1 stopped
        60500 judge_started 2
        180500 judge_timed_out 2
        180600 collected 2 {"status": "passed", "raw_score": 7}
        "#,
        &expect![[r#"
            start
              start the agent
              (tick due at 60000)
            50000 submit submit 10
              reply to op 1: {"kind":"submit","state":"stored","remaining_seconds":10}
            55000 judge train 10
              reply to op 2: {"kind":"judge","job":1,"remaining":{"calls":99,"seconds":12000}}
              start job 1 of train on op 2's payload of 10 bytes, with a timeout of 5s
            55500 judge_started 1
            59999 tick
            60000 tick
              stop job 1
              tear down the agent
              (no tick due)
            60100 status 1
              refuse op 3: the work phase has ended: make no more ops
            60200 agent_exited 137
            60400 judge_failed 1 stopped
              start job 2 of score on op 1's payload of 10 bytes, with a timeout of 120s
            60500 judge_started 2
            180500 judge_timed_out 2
            180600 collected 2 {"status": "passed", "raw_score": 7}
              finish: no score; work ended at the deadline; the stored submission, op 1's payload of 10 bytes, got no result, Timeout
        "#]],
    );
}

/// The agent's exit and its sandbox's failure end the work phase. A job whose command has
/// already ended isn't stopped, and the stored submission is judged once it's gone. A failed
/// result with a score is invalid, and leaves none.
#[test]
fn agent_ends() {
    check(
        |_| {},
        r#"
        1000 judge train 10
        1500 judge_started 1
        3000 judge_exited 1
        3100 submit submit 10
        3200 agent_exited 0
        3400 collected 1 {"status": "passed", "raw_score": 1}
        3500 judge_started 2
        5000 judge_exited 2
        5100 collected 2 {"status": "failed", "raw_score": 7.5, "failure": "too slow"}
        "#,
        &expect![[r#"
            start
              start the agent
              (tick due at 18000000)
            1000 judge train 10
              reply to op 1: {"kind":"judge","job":1,"remaining":{"calls":99,"seconds":12000}}
              start job 1 of train on op 1's payload of 10 bytes, with a timeout of 12000s
            1500 judge_started 1
            3000 judge_exited 1
            3100 submit submit 10
              reply to op 2: {"kind":"submit","state":"stored","remaining_seconds":17996}
            3200 agent_exited 0
              tear down the agent
              (no tick due)
            3400 collected 1 {"status": "passed", "raw_score": 1}
              start job 2 of score on op 2's payload of 10 bytes, with a timeout of 120s
            3500 judge_started 2
            5000 judge_exited 2
            5100 collected 2 {"status": "failed", "raw_score": 7.5, "failure": "too slow"}
              finish: no score; work ended at the agent's exit, Code(0); the stored submission, op 2's payload of 10 bytes, got no result, InvalidResult("the result failed, yet has a score at `/raw_score`: leave it out, or write null")
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
              (tick due at 18000000)
            30000 agent_failed the agent's sandbox did not connect
              tear down the agent
              finish: no score; work ended at the agent's failure, the agent's sandbox did not connect; no submission
              (no tick due)
        "#]],
    );
}

/// Once a finalize is accepted, every op is refused. The agent keeps running for the grace,
/// and its job too, until the grace runs out and the work phase ends without it. With no
/// grace, the work phase ends at the finalize.
#[test]
fn finalize_lock() {
    check(
        |_| {},
        r"
        1000 judge train 10
        1500 judge_started 1
        2000 submit submit 10 final
        2100 ops
        2200 status 1
        2300 submit submit 10
        61999 tick
        62000 tick
        62200 judge_failed 1 stopped
        ",
        &expect![[r#"
            start
              start the agent
              (tick due at 18000000)
            1000 judge train 10
              reply to op 1: {"kind":"judge","job":1,"remaining":{"calls":99,"seconds":12000}}
              start job 1 of train on op 1's payload of 10 bytes, with a timeout of 12000s
            1500 judge_started 1
            2000 submit submit 10 final
              reply to op 2: {"kind":"submit","state":"accepted","remaining_seconds":60}
              (tick due at 62000)
            2100 ops
              refuse op 3: a final submission was accepted: make no more ops
            2200 status 1
              refuse op 4: a final submission was accepted: make no more ops
            2300 submit submit 10
              refuse op 5: a final submission was accepted: make no more ops
            61999 tick
            62000 tick
              stop job 1
              tear down the agent
              (no tick due)
            62200 judge_failed 1 stopped
              start job 2 of score on op 2's payload of 10 bytes, with a timeout of 120s
        "#]],
    );
    check(
        |ir| ir["main"]["solo_then_judge"]["finalize_grace_seconds"] = json!(0),
        r"
        2000 submit submit 10 final
        2100 ops
        ",
        &expect![[r#"
            start
              start the agent
              (tick due at 18000000)
            2000 submit submit 10 final
              reply to op 1: {"kind":"submit","state":"accepted","remaining_seconds":0}
              tear down the agent
              start job 1 of score on op 1's payload of 10 bytes, with a timeout of 120s
              (no tick due)
            2100 ops
              refuse op 2: a final submission was accepted: make no more ops
        "#]],
    );
}

/// A finalize's reply counts the time left to the end of its grace, or to the deadline if
/// that comes first.
#[test]
fn finalize_near_deadline() {
    check(
        |ir| ir["main"]["solo_then_judge"]["deadline_seconds"] = json!(30),
        r"
        10000 submit submit 10 final
        ",
        &expect![[r#"
            start
              start the agent
              (tick due at 30000)
            10000 submit submit 10 final
              reply to op 1: {"kind":"submit","state":"accepted","remaining_seconds":20}
        "#]],
    );
}

/// A status names a job that one of the agent's calls returned.
#[test]
fn unknown_jobs() {
    check(
        |_| {},
        r"
        1000 judge train 10
        1400 status 2
        1500 status 0
        ",
        &expect![[r#"
            start
              start the agent
              (tick due at 18000000)
            1000 judge train 10
              reply to op 1: {"kind":"judge","job":1,"remaining":{"calls":99,"seconds":12000}}
              start job 1 of train on op 1's payload of 10 bytes, with a timeout of 12000s
            1400 status 2
              refuse op 2: you have no job 2: ask about a job that one of your calls returned
            1500 status 0
              refuse op 3: you have no job 0: ask about a job that one of your calls returned
        "#]],
    );
}
