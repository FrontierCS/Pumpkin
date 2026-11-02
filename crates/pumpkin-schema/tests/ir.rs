//! The workflow IR and its parsing, tested through the crate's public API against a
//! hand-written workflow for the LWE task (`lwe_structured_recovery`) that follows the task's
//! policy. Its image digests, the agent's command and the judges' environment are placeholders.
//! Also the JSON of ops' replies, which `sandboxctl` prints.

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]

use expect_test::{Expect, expect};
use pumpkin_schema::ir;
use pumpkin_schema::sandbox::op::OpReply;
use pumpkin_schema::workflow::{Holder, Main, RoleId, Workflow};
use serde_json::{Value, json};

const LWE: &str = include_str!("fixtures/lwe.json");

const HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn lwe_json() -> Value {
    serde_json::from_str(LWE).unwrap()
}

/// The LWE workflow with `value` set at `pointer`, whose parent is an object.
fn lwe_with(pointer: &str, value: Value) -> Value {
    let mut ir = lwe_json();
    let (parent, key) = pointer.rsplit_once('/').unwrap();
    let parent = ir.pointer_mut(parent).unwrap().as_object_mut().unwrap();
    parent.insert(key.to_owned(), value);
    ir
}

/// Checks what becomes of each case against `expect`, a line each.
fn check<C: Case>(cases: impl IntoIterator<Item = C>, expect: &Expect) {
    let actual: Vec<String> = cases.into_iter().map(Case::outcome).collect();
    expect.assert_eq(&actual.join("\n"));
}

/// A case whose outcome [`check`] shows.
trait Case {
    fn outcome(self) -> String;
}

/// An IR, which parses, or fails to deserialize, or breaks rules.
impl Case for Value {
    fn outcome(self) -> String {
        match serde_json::from_value::<ir::Workflow>(self) {
            Err(error) => error.to_string(),
            Ok(ir) => match Workflow::parse(&ir) {
                Ok(_) => "parsed".to_owned(),
                Err(errors) => errors.to_string(),
            },
        }
    }
}

/// An op's reply as JSON, which reads as the reply shown and writes back as itself, or fails to
/// read.
struct Reply(&'static str);

impl Case for Reply {
    fn outcome(self) -> String {
        match serde_json::from_str::<OpReply>(self.0) {
            Err(error) => error.to_string(),
            Ok(reply) => match serde_json::to_string(&reply).unwrap() {
                json if json == self.0 => format!("{reply:?}"),
                json => format!("{reply:?}, written back as {json}"),
            },
        }
    }
}

/// Each reply's JSON form, a status's in each of its job's states; and statuses whose fields
/// don't match their states, which are refused.
#[test]
fn op_replies() {
    check(
        [
            Reply(
                r#"{"kind":"ops","remaining_seconds":17940,"resources":{"cpu_millis":2000,"memory_bytes":4294967296},"grants":[{"kind":"judge","name":"train","max_bytes":4194304,"calls":100,"cumulative_seconds":12000,"concurrency":1,"call_timeout_seconds":600},{"kind":"submit","name":"submit","max_bytes":4194304}]}"#,
            ),
            Reply(r#"{"kind":"judge","job":1,"remaining":{"calls":99,"seconds":12000}}"#),
            Reply(
                r#"{"kind":"status","job":1,"state":"queued","remaining":{"calls":99,"seconds":null}}"#,
            ),
            Reply(
                r#"{"kind":"status","job":1,"state":"running","remaining":{"calls":99,"seconds":12000}}"#,
            ),
            Reply(
                r#"{"kind":"status","job":1,"state":"passed","returned":{"metrics":{"solved":3},"raw_score":3},"remaining":{"calls":99,"seconds":11993}}"#,
            ),
            Reply(
                r#"{"kind":"status","job":2,"state":"failed","returned":{"failure":"wrong answer","raw_score":null},"remaining":{"calls":98,"seconds":11990}}"#,
            ),
            Reply(
                r#"{"kind":"status","job":3,"state":"failed","failure":{"code":"judge_timeout","message":"the judge ran past its timeout"},"remaining":{"calls":97,"seconds":0}}"#,
            ),
            Reply(r#"{"kind":"submit","state":"accepted","remaining_seconds":60}"#),
            Reply(r#"{"kind":"refused","message":"`train` has no calls left: it allows 100"}"#),
            Reply(
                r#"{"kind":"status","job":1,"state":"running","returned":{},"remaining":{"calls":null,"seconds":null}}"#,
            ),
            Reply(
                r#"{"kind":"status","job":1,"state":"passed","remaining":{"calls":null,"seconds":null}}"#,
            ),
            Reply(
                r#"{"kind":"status","job":1,"state":"failed","returned":{},"failure":{"code":"judge_error","message":"the judge failed"},"remaining":{"calls":null,"seconds":null}}"#,
            ),
        ],
        &expect![[r#"
            Ops(OpsReply { remaining_seconds: 17940, resources: Resources { cpu_millis: 2000, memory_bytes: 4294967296 }, grants: [Judge(JudgeGrant { name: Name("train"), max_bytes: 4194304, calls: Some(100), cumulative_seconds: Some(12000), concurrency: 1, call_timeout_seconds: 600 }), Submit(SubmitGrant { name: Name("submit"), max_bytes: 4194304 })] })
            Judge(JudgeReply { job: 1, remaining: Remaining { calls: Some(99), seconds: Some(12000) } })
            Status(StatusReply { job: 1, state: Queued, remaining: Remaining { calls: Some(99), seconds: None } })
            Status(StatusReply { job: 1, state: Running, remaining: Remaining { calls: Some(99), seconds: Some(12000) } })
            Status(StatusReply { job: 1, state: Passed { returned: Returned({"metrics": Object {"solved": Number(3)}, "raw_score": Number(3)}) }, remaining: Remaining { calls: Some(99), seconds: Some(11993) } })
            Status(StatusReply { job: 2, state: Failed(Judged { returned: Returned({"failure": String("wrong answer"), "raw_score": Null}) }), remaining: Remaining { calls: Some(98), seconds: Some(11990) } })
            Status(StatusReply { job: 3, state: Failed(NoResult { failure: Failure { code: JudgeTimeout, message: "the judge ran past its timeout" } }), remaining: Remaining { calls: Some(97), seconds: Some(0) } })
            Submit(SubmitReply { state: Accepted, remaining_seconds: 60 })
            Refused { message: "`train` has no calls left: it allows 100" }
            a job that hasn't finished has no result: give it neither `returned` nor `failure`
            a passed job has a result: give it `returned`, and no `failure`
            a failed job has a result or a failure: give it exactly one of `returned` and `failure`"#]],
    );
}

#[test]
fn lwe() {
    let ir: ir::Workflow = serde_json::from_str(LWE).unwrap();
    // Whatever writes the IR serializes these types, so each must read back what it wrote.
    assert_eq!(
        serde_json::to_value(&ir).unwrap(),
        lwe_json(),
        "a round trip"
    );
    let workflow = Workflow::parse(&ir).unwrap();
    let Main::SoloThenJudge(pattern) = workflow.main();
    assert_eq!(workflow.agent(pattern.agent).sandbox.name.as_str(), "agent");
    assert_eq!(pattern.work_phase, workflow.initial_phase());
    let submit = workflow.submit_grant(pattern.submit);
    assert_eq!(submit.holder, RoleId::Agent(pattern.agent));
    let score = workflow.judge_grant(pattern.score);
    assert_eq!(score.holder, Holder::Root);
    let judge = workflow.judge(score.judge());
    assert_eq!(judge.sandbox.name.as_str(), "final_judge");
}

#[test]
fn checked_values() {
    let agent = "/roles/0";
    let command = "/roles/0/runtime/agent/command";
    let asset = json!({ "name": "data", "path": "/data", "digest": "md5:abc", "mode": "any" });
    check(
        [
            lwe_with("/name", json!("lwe structured recovery")),
            lwe_with(&format!("{agent}/name"), json!("Agent")),
            lwe_with(&format!("{agent}/name"), json!("bad\nname")),
            lwe_with(&format!("{agent}/image"), json!(format!("sha256:{HEX}"))),
            lwe_with(&format!("{agent}/image"), json!("ubuntu:24.04")),
            lwe_with(&format!("{agent}/assets"), json!([asset])),
            lwe_with(&format!("{command}/cwd"), json!("workspace")),
            lwe_with(&format!("{command}/cwd"), json!("/workspace/")),
            lwe_with(&format!("{command}/argv"), json!([])),
            lwe_with(&format!("{command}/env"), json!(["MODE"])),
            lwe_with(&format!("{command}/env"), json!(["1ST=value"])),
            lwe_with(
                &format!("{command}/env"),
                json!(["MODE=train", "MODE=eval"]),
            ),
            lwe_with(&format!("{agent}/egress/model/models"), json!([])),
            lwe_with(
                &format!("{agent}/egress/model/models"),
                json!(["gpt-6-astra", "gpt-6-astra"]),
            ),
        ],
        &expect![[r#"
            `lwe structured recovery` is not a valid workflow name: use 1 to 128 characters from A-Z, a-z, 0-9, `.`, `_`, `-` and `/`
            `Agent` is not a valid name: use 1 to 64 characters from a-z, 0-9, `_` and `-`, starting with a letter
            `bad\nname` is not a valid name: use 1 to 64 characters from a-z, 0-9, `_` and `-`, starting with a letter
            parsed
            `ubuntu:24.04` is not a pinned image: use `<repository>@sha256:<hex>` or `sha256:<hex>`
            `md5:abc` is not a digest: use `sha256:` and 64 lowercase hexadecimal digits
            `workspace` is not a normalized absolute path: start with `/`, and use no empty, `.` or `..` components and no NUL byte
            `/workspace/` is not a normalized absolute path: start with `/`, and use no empty, `.` or `..` components and no NUL byte
            the command is empty: it needs a program
            `MODE` is not an environment entry: write `NAME=value`
            `1ST` is not a valid environment variable name: use A-Z, a-z, 0-9 and `_`, not starting with a digit
            `MODE` is set more than once: set each name once
            no models: a model route allows at least one
            `gpt-6-astra` is listed more than once: list each model once"#]],
    );
}

#[test]
fn broken_references() {
    let mut ir = lwe_json();
    let mut root = ir["roles"][1].clone();
    root["name"] = json!("root");
    ir["roles"].as_array_mut().unwrap().push(root);
    let payload = ir["roles"][2]["runtime"]["judge"]["slots"][0].clone();
    ir["roles"][2]["runtime"]["judge"]["slots"]
        .as_array_mut()
        .unwrap()
        .push(payload);
    let train = ir["grants"][0].clone();
    ir["grants"].as_array_mut().unwrap().push(train);
    ir["grants"][0]["holder"] = json!({ "role": "ghost" });
    ir["phases"][0]["live"] = json!(["agent", "agent", "train_judge", "ghost"]);
    ir["phases"][0]["next"] = json!(["final", "later"]);
    ir["phases"][1]["next"] = json!(["final"]);
    ir["main"]["solo_then_judge"]["agent"] = json!("train_judge");
    ir["main"]["solo_then_judge"]["score"] = json!("submit");
    check(
        [ir],
        &expect![[r#"
            roles[final_judge].runtime.judge.slots[1].name: `payload` also names roles[final_judge].runtime.judge.slots[0]: give each a unique name
            roles[root].name: `root` is reserved: use another name
            phases[work].live: `agent` is already listed: list each once
            phases[work].live: no role is named `ghost`: name one listed in `roles`
            phases[work].next: no phase is named `later`: name one listed in `phases`
            phases[final].next: the phase follows itself: it may not
            grants[3].name: `train` also names grants[0]: give each a unique name
            grants[0].holder: no role is named `ghost`: name one listed in `roles`
            main.solo_then_judge.agent: `train_judge` is a judge: name an agent
            main.solo_then_judge.score: `submit` is a submit grant: name a judge grant"#]],
    );
}

#[test]
fn broken_grants() {
    let mut ir = lwe_json();
    ir["grants"][0]["phases"] = json!(["work", "final"]);
    ir["grants"][0]["op"]["judge"]["slot"] = json!("submission");
    ir["grants"][1]["holder"] = json!("root");
    ir["grants"][2]["op"]["judge"]["role"] = json!("agent");
    ir["grants"].as_array_mut().unwrap().push(json!({
        "name": "idle",
        "holder": { "role": "agent" },
        "phases": [],
        "op": { "submit": { "max_bytes": 1 } }
    }));
    check(
        [ir],
        &expect![[r#"
            grants[train].holder: `agent` is not live in phase `final`: a grant applies only in phases where its roles are live
            grants[train].op.judge.role: `train_judge` is not live in phase `final`: a grant applies only in phases where its roles are live
            grants[train].op.judge.slot: `train_judge` has no slot named `submission`: name one of its slots
            grants[submit].holder: the root holds a submit grant: only a role may
            grants[score].op.judge.role: `agent` is an agent: name a judge
            grants[idle].phases: no phases: a grant applies in at least one"#]],
    );
}

#[test]
fn broken_pattern() {
    let mut ir = lwe_json();
    // The final phase comes first and keeps the agent, the work phase leads nowhere, a
    // train-mode judge scores, and submissions outgrow the score slot.
    ir["phases"] = json!([
        { "name": "final", "live": ["final_judge", "train_judge", "agent"] },
        { "name": "work", "live": ["agent", "train_judge"] }
    ]);
    ir["grants"][2]["op"]["judge"]["role"] = json!("train_judge");
    ir["grants"][1]["op"]["submit"]["max_bytes"] = json!(8_388_608);
    check(
        [ir],
        &expect![[r#"
            main.solo_then_judge.work_phase: `work` is not the first phase: name the phase a run starts in
            main.solo_then_judge.final_phase: the work phase `work` is not followed by `final`: list `final` in its `next`
            main.solo_then_judge.agent: `agent` is live in the final phase `final`, so it would survive into judging: drop it from the phase's `live`
            main.solo_then_judge.score: `score` judges with `train_judge`, which is train-mode: name a final-mode judge
            main.solo_then_judge.submit: submissions of up to 8388608 bytes do not fit the score slot's 4194304 bytes: raise the slot's `max_bytes` or lower the submit grant's"#]],
    );
}
