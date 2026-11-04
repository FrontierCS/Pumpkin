//! The workflow IR and its parsing, tested through the crate's public API against a
//! hand-written workflow for the LWE task (`lwe_structured_recovery`) that follows the task's
//! policy. Its image digests, the agent's command and the judges' environment are placeholders.
//! Also the JSON of ops' replies, which `sandboxctl` prints.
//!
//! The workflow stays whole, as the IR's golden fixture: `lwe` compares a round trip of all of
//! it, and every other IR case is a change to it (`lwe_json`, `lwe_with`).

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]

use expect_test::{Expect, expect};
use pumpkin_schema::ir;
use pumpkin_schema::sandbox::MAX_OP_PAYLOAD_BYTES;
use pumpkin_schema::sandbox::op::OpReply;
use pumpkin_schema::workflow::{Main, Workflow};
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
                r#"{"kind":"ops","remaining_seconds":17940,"resources":{"cpu_millis":2000,"memory_bytes":4294967296},"grants":[{"kind":"judge","name":"train","max_bytes":4194304,"calls":100,"cumulative_seconds":12000,"call_timeout_seconds":600},{"kind":"submit","name":"submit","max_bytes":4194304}]}"#,
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
            Ops(OpsReply { remaining_seconds: 17940, resources: Resources { cpu_millis: 2000, memory_bytes: 4294967296 }, grants: [Judge(JudgeGrant { name: Name("train"), max_bytes: 4194304, calls: Some(100), cumulative_seconds: Some(12000), call_timeout_seconds: 600 }), Submit(SubmitGrant { name: Name("submit"), max_bytes: 4194304 })] })
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
    assert_eq!(workflow.agent().sandbox.name.as_str(), "agent");
    assert_eq!(workflow.submit_grant().name.as_str(), "submit");
    let judge = workflow.judge(pattern.score.slot.judge());
    assert_eq!(judge.sandbox.name.as_str(), "final_judge");
}

#[test]
fn checked_values() {
    let agent = "/roles/0";
    let command = "/roles/0/runtime/agent/command";
    let result = "/roles/1/runtime/judge/result";
    check(
        [
            lwe_with("/name", json!("lwe structured recovery")),
            lwe_with(&format!("{agent}/name"), json!("Agent")),
            lwe_with(&format!("{agent}/name"), json!("bad\nname")),
            lwe_with(&format!("{agent}/image"), json!(format!("sha256:{HEX}"))),
            lwe_with(&format!("{agent}/image"), json!("ubuntu:24.04")),
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
            lwe_with(&format!("{result}/score"), json!("raw_score")),
            lwe_with(&format!("{result}/score"), json!("/raw~2score")),
            lwe_with(&format!("{result}/returns"), json!(["/metrics", ""])),
            lwe_with(&format!("{result}/returns"), json!(["/metrics/"])),
            lwe_with(&format!("{result}/returns"), json!(["/x/a~1b", "/a~1b"])),
            lwe_with(
                "/roles/2/runtime/judge/slots/0/max_bytes",
                json!(MAX_OP_PAYLOAD_BYTES + 1),
            ),
        ],
        &expect![[r#"
            `lwe structured recovery` is not a valid workflow name: use 1 to 128 characters from A-Z, a-z, 0-9, `.`, `_`, `-` and `/`
            `Agent` is not a valid name: use 1 to 64 characters from a-z, 0-9, `_` and `-`, starting with a letter
            `bad\nname` is not a valid name: use 1 to 64 characters from a-z, 0-9, `_` and `-`, starting with a letter
            parsed
            `ubuntu:24.04` is not a pinned image: use `<repository>@sha256:<hex>` or `sha256:<hex>`
            `workspace` is not a normalized absolute path: start with `/`, and use no empty, `.` or `..` components and no NUL byte
            `/workspace/` is not a normalized absolute path: start with `/`, and use no empty, `.` or `..` components and no NUL byte
            the command is empty: it needs a program
            `MODE` is not an environment entry: write `NAME=value`
            `1ST` is not a valid environment variable name: use A-Z, a-z, 0-9 and `_`, not starting with a digit
            `MODE` is set more than once: set each name once
            no models: a model route allows at least one
            `gpt-6-astra` is listed more than once: list each model once
            `raw_score` is not a JSON pointer: write `/` before each token, `~0` for `~` and `~1` for `/`, or nothing for the whole document
            `/raw~2score` is not a JSON pointer: write `/` before each token, `~0` for `~` and `~1` for `/`, or nothing for the whole document
            the empty pointer names the whole result, with no key: point to a field
            `/metrics/` would return the empty key: point to a field with a name
            `/x/a~1b` and `/a~1b` would both return `a/b`: return each key once
            roles[final_judge].runtime.judge.slots[payload].max_bytes: slot `payload`'s `max_bytes` is 67108865, over the op payload limit of 67108864: give at most that"#]],
    );
}

#[test]
fn broken_references() {
    let mut ir = lwe_json();
    let payload = ir["roles"][2]["runtime"]["judge"]["slots"][0].clone();
    ir["roles"][2]["runtime"]["judge"]["slots"]
        .as_array_mut()
        .unwrap()
        .push(payload);
    let pattern = &mut ir["main"]["solo_then_judge"];
    let train = pattern["grants"][0].clone();
    pattern["grants"].as_array_mut().unwrap().push(train);
    pattern["grants"][0]["judge"]["role"] = json!("ghost");
    pattern["grants"][2]["judge"]["slot"] = json!("submission");
    pattern["score"]["role"] = json!("agent");
    check(
        [ir],
        &expect![[r#"
            roles[final_judge].runtime.judge.slots[1].name: `payload` also names roles[final_judge].runtime.judge.slots[0]: give each a unique name
            main.solo_then_judge.grants[2].name: `train` also names main.solo_then_judge.grants[0]: give each a unique name
            main.solo_then_judge.grants[0].judge.role: no role is named `ghost`: name one listed in `roles`
            main.solo_then_judge.grants[2].judge.slot: `train_judge` has no slot named `submission`: name one of its slots
            main.solo_then_judge.score.role: `agent` is an agent: name a judge"#]],
    );
}

/// A workflow has one agent, which alone has a model route and holds one submit grant, a judge
/// grant names a judge no other grant names, and the score judges with a judge no grant names,
/// even a grant that fails to parse.
#[test]
fn broken_pattern() {
    let mut ir = lwe_json();
    let mut helper = ir["roles"][0].clone();
    helper["name"] = json!("helper");
    ir["roles"].as_array_mut().unwrap().push(helper);
    ir["roles"][2]["egress"] = ir["roles"][0]["egress"].clone();
    let pattern = &mut ir["main"]["solo_then_judge"];
    let submit = json!({ "submit": { "name": "resubmit" } });
    let mut retrain = pattern["grants"][0].clone();
    retrain["judge"]["name"] = json!("retrain");
    pattern["grants"]
        .as_array_mut()
        .unwrap()
        .extend([submit, retrain]);
    pattern["grants"][0]["judge"]["slot"] = json!("submission");
    pattern["score"]["role"] = json!("train_judge");
    let mut none = lwe_json();
    none["roles"].as_array_mut().unwrap().remove(0);
    none["main"]["solo_then_judge"]["grants"]
        .as_array_mut()
        .unwrap()
        .remove(1);
    let mut untimed = lwe_json();
    untimed["main"]["solo_then_judge"]["grants"][0]["judge"]
        .as_object_mut()
        .unwrap()
        .remove("call_timeout_seconds");
    check(
        [ir, none, untimed],
        &expect![[r#"
            roles[final_judge].egress.model: `final_judge` is a judge, and only the agent has a model route: remove it
            roles[helper].runtime: `helper` is a second agent, after `agent`: a workflow has exactly one
            main.solo_then_judge.grants[train].judge.slot: `train_judge` has no slot named `submission`: name one of its slots
            main.solo_then_judge.grants[retrain].judge.role: `train_judge` is named by judge grants `train` and `retrain`: name each judge from one grant
            main.solo_then_judge.grants[resubmit].submit: `resubmit` is a second submit grant, after `submit`: give the agent exactly one
            main.solo_then_judge.score.role: `train_judge` is also the role of judge grant `train`: name a judge no grant names
            roles: no role is an agent: a workflow has exactly one
            main.solo_then_judge.grants: no submit grant: give the agent exactly one
            missing field `call_timeout_seconds`"#]],
    );
}
