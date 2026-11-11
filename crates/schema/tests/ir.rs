//! The workflow IR and its parsing, tested through the crate's public API against a
//! hand-written workflow for the LWE task (`lwe_structured_recovery`) that follows the task's
//! policy. Its image digests, the agent's command, files and standard input, and the judges'
//! environment are placeholders.
//! Also the JSON of ops' replies, which `sandboxctl` prints, the in-sandbox contract, and the run
//! tags that providers find a run's sandboxes by.
//!
//! The workflow stays whole, as the IR's golden fixture: `lwe` compares a round trip of all of
//! it, and every other IR case is a change to it (`lwe_json`, `lwe_with`).

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]

use expect_test::{Expect, expect};
use schema::ir;
use schema::provider::RunTag;
use schema::sandbox::op::OpReply;
use schema::sandbox::{
    MAX_OP_PAYLOAD_BYTES, RelativePath, command_env, command_stderr, command_stdin, command_stdout,
    evidence_dir, pumpkin_dir, pumpkin_subdirs, result_file, slot_dir, slot_payload,
};
use schema::workflow::{Main, Workflow};
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

/// An IR, which parses, or fails to deserialize, or breaks rules, each at a place that resolves
/// in the IR's JSON.
impl Case for Value {
    fn outcome(self) -> String {
        match serde_json::from_value::<ir::Workflow>(self) {
            Err(error) => error.to_string(),
            Ok(ir) => match Workflow::parse(&ir) {
                Ok(_) => "parsed".to_owned(),
                Err(errors) => {
                    let json = serde_json::to_value(&ir).unwrap();
                    for error in errors.as_slice() {
                        assert!(error.at.resolve(&json).is_some(), "{error} resolves");
                    }
                    errors.to_string()
                }
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

/// A relative path below an absolute one, as a role's commands find a path of the in-sandbox
/// contract below `pumpkin_dir()`.
struct Joined(ir::AbsolutePath, RelativePath);

impl Case for Joined {
    fn outcome(self) -> String {
        self.0.join(&self.1).to_string()
    }
}

/// A command's environment as its IR gives it, and as the command runs with it.
struct CommandEnv(Value);

impl Case for CommandEnv {
    fn outcome(self) -> String {
        let env: ir::Env = serde_json::from_value(self.0.clone()).unwrap();
        let runs = serde_json::to_string(&command_env(&env)).unwrap();
        format!("{} runs as {runs}", self.0)
    }
}

/// A run's tag, which parses, or is refused.
struct Tag(&'static str);

impl Case for Tag {
    fn outcome(self) -> String {
        match RunTag::try_from(self.0.to_owned()) {
            Ok(tag) => format!("`{}` parsed", tag.as_str()),
            Err(error) => error,
        }
    }
}

#[test]
fn run_tags() {
    check(
        [
            Tag("20261105-122057-9802092b"),
            Tag("pk-4242-cleanup-other"),
            Tag("selftest.v2_a"),
            Tag(""),
            // `docker` trims a filter's value, so this tag would find another run's sandboxes.
            Tag("pk-run "),
            Tag("pk/run"),
            Tag("-pk"),
            Tag(".pk"),
        ],
        &expect![[r#"
            `20261105-122057-9802092b` parsed
            `pk-4242-cleanup-other` parsed
            `selftest.v2_a` parsed
            the run tag is empty: use only ASCII letters, digits, `_`, `.` and `-`, at least one
            the run tag `pk-run ` holds ` `: use only ASCII letters, digits, `_`, `.` and `-`
            the run tag `pk/run` holds `/`: use only ASCII letters, digits, `_`, `.` and `-`
            the run tag `-pk` starts with `-`: start it with an ASCII letter or digit
            the run tag `.pk` starts with `.`: start it with an ASCII letter or digit"#]],
    );
}

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
                r#"{"kind":"status","job":1,"state":"completed","returned":{"metrics":{"solved":3},"raw_score":3},"scored":true,"remaining":{"calls":99,"seconds":11993}}"#,
            ),
            Reply(
                r#"{"kind":"status","job":2,"state":"completed","returned":{"failure":"wrong answer","raw_score":null},"scored":false,"remaining":{"calls":98,"seconds":11990}}"#,
            ),
            Reply(
                r#"{"kind":"status","job":3,"state":"errored","failure":{"code":"judge_timeout","message":"the judge ran past its timeout"},"remaining":{"calls":97,"seconds":0}}"#,
            ),
            Reply(r#"{"kind":"submit","state":"accepted","remaining_seconds":60}"#),
            Reply(r#"{"kind":"refused","message":"`train` has no calls left: it allows 100"}"#),
            Reply(
                r#"{"kind":"status","job":1,"state":"running","returned":{},"remaining":{"calls":null,"seconds":null}}"#,
            ),
            Reply(
                r#"{"kind":"status","job":1,"state":"completed","remaining":{"calls":null,"seconds":null}}"#,
            ),
            Reply(
                r#"{"kind":"status","job":1,"state":"errored","returned":{},"failure":{"code":"judge_error","message":"the judge failed"},"remaining":{"calls":null,"seconds":null}}"#,
            ),
        ],
        &expect![[r#"
            Ops(OpsReply { remaining_seconds: 17940, resources: Resources { cpu_millis: 2000, memory_bytes: 4294967296 }, grants: [Judge(JudgeGrant { name: Name("train"), max_bytes: 4194304, calls: Some(100), cumulative_seconds: Some(12000), call_timeout_seconds: 600 }), Submit(SubmitGrant { name: Name("submit"), max_bytes: 4194304 })] })
            Judge(JudgeReply { job: 1, remaining: Remaining { calls: Some(99), seconds: Some(12000) } })
            Status(StatusReply { job: 1, state: Queued, remaining: Remaining { calls: Some(99), seconds: None } })
            Status(StatusReply { job: 1, state: Running, remaining: Remaining { calls: Some(99), seconds: Some(12000) } })
            Status(StatusReply { job: 1, state: Completed { returned: Returned({"metrics": Object {"solved": Number(3)}, "raw_score": Number(3)}), scored: true }, remaining: Remaining { calls: Some(99), seconds: Some(11993) } })
            Status(StatusReply { job: 2, state: Completed { returned: Returned({"failure": String("wrong answer"), "raw_score": Null}), scored: false }, remaining: Remaining { calls: Some(98), seconds: Some(11990) } })
            Status(StatusReply { job: 3, state: Errored { failure: Failure { code: JudgeTimeout, message: "the judge ran past its timeout" } }, remaining: Remaining { calls: Some(97), seconds: Some(0) } })
            Submit(SubmitReply { state: Accepted, remaining_seconds: 60 })
            Refused { message: "`train` has no calls left: it allows 100" }
            a job that hasn't ended has no result: give it none of `returned`, `scored` and `failure`
            a completed job has a result: give it `returned` and `scored`, and no `failure`
            an errored job has no result: give it `failure`, and neither `returned` nor `scored`"#]],
    );
}

#[test]
fn in_sandbox_contract() {
    let slot = ir::Name::try_from("payload".to_owned()).unwrap();
    let paths = pumpkin_subdirs().into_iter().chain([
        slot_dir(&slot),
        slot_payload(&slot),
        result_file(),
        evidence_dir(),
        command_stdout(),
        command_stderr(),
        command_stdin(),
    ]);
    let root = ir::AbsolutePath::try_from("/".to_owned()).unwrap();
    let paths = paths.map(|path| Joined(pumpkin_dir(), path));
    check(
        paths.chain([Joined(root, result_file())]),
        &expect![[r#"
            /pumpkin/in
            /pumpkin/out
            /pumpkin/home
            /pumpkin/log
            /pumpkin/in/payload
            /pumpkin/in/payload/raw
            /pumpkin/out/result.json
            /pumpkin/out/evidence
            /pumpkin/log/command.stdout
            /pumpkin/log/command.stderr
            /pumpkin/in/command.stdin
            /out/result.json"#]],
    );
    let envs = [
        json!([]),
        json!(["MODE=train"]),
        json!(["MODE=train", "HOME=/workspace"]),
    ];
    check(
        envs.map(CommandEnv),
        &expect![[r#"
        [] runs as ["HOME=/pumpkin/home"]
        ["MODE=train"] runs as ["HOME=/pumpkin/home","MODE=train"]
        ["MODE=train","HOME=/workspace"] runs as ["MODE=train","HOME=/workspace"]"#]],
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
            /roles/2/runtime/judge/slots/0/max_bytes: slot `payload`'s `max_bytes` is 67108865, over the op payload limit of 67108864: give at most that"#]],
    );
}

#[test]
fn agent_files() {
    let agent = "/roles/0/runtime/agent";
    let dir = |path: &str| json!({ "path": path, "mode": "0755" });
    let file = |path: &str| json!({ "path": path, "mode": "0644", "content": "" });
    let mut broken = lwe_json();
    let runtime = &mut broken["roles"][0]["runtime"]["agent"];
    runtime["dirs"] = json!([
        dir("/workspace/a"),
        dir("/workspace/a"),
        dir("/workspace/b/d")
    ]);
    runtime["files"] = json!([file("/workspace/a"), file("/workspace/b")]);
    let path = format!("{agent}/files/0/path");
    check(
        [
            lwe_with(&format!("{agent}/dirs/0/mode"), json!("0777")),
            lwe_with(&format!("{agent}/dirs/0/mode"), json!("0800")),
            lwe_with(&format!("{agent}/dirs/0/mode"), json!("4755")),
            lwe_with(&format!("{agent}/dirs/0/mode"), json!("755")),
            lwe_with(&path, json!("/pumpkinx/config.toml")),
            lwe_with(&path, json!("/")),
            lwe_with(&path, json!("/pumpkin/in/command.stdin")),
            lwe_with(&path, json!("/.pumpkin/bin/sandboxctl")),
            lwe_with(&path, json!("/harness")),
            broken,
        ],
        &expect![[r#"
            parsed
            `0800` is not a mode: write `0` and three octal digits, such as `0644`
            `4755` is not a mode: write `0` and three octal digits, such as `0644`
            `755` is not a mode: write `0` and three octal digits, such as `0644`
            parsed
            `/` holds the whole sandbox: name a path below it
            `/pumpkin/in/command.stdin` is in `/pumpkin`, which holds Pumpkin's own files: name a path outside it
            `/.pumpkin/bin/sandboxctl` is in `/.pumpkin`, which holds Pumpkin's own files: name a path outside it
            `/harness` is in `/harness`, which holds Pumpkin's own files: name a path outside it
            /roles/0/runtime/agent/dirs/1/path: `/workspace/a` is also the path of /roles/0/runtime/agent/dirs/0: put each path once
            /roles/0/runtime/agent/dirs/2/path: `/workspace/b/d` is below `/workspace/b`, the file at /roles/0/runtime/agent/files/1: put it in a directory
            /roles/0/runtime/agent/files/0/path: `/workspace/a` is also the path of /roles/0/runtime/agent/dirs/0: put each path once"#]],
    );
}

#[test]
fn broken_references() {
    let mut ir = lwe_json();
    let train_judge = ir["roles"][1].clone();
    ir["roles"].as_array_mut().unwrap().push(train_judge);
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
            /roles/3/name: `train_judge` also names /roles/1: give each a unique name
            /roles/2/runtime/judge/slots/1/name: `payload` also names /roles/2/runtime/judge/slots/0: give each a unique name
            /main/solo_then_judge/grants/2/judge/name: `train` also names /main/solo_then_judge/grants/0/judge: give each a unique name
            /main/solo_then_judge/grants/0/judge/role: no role is named `ghost`: name one listed in `roles`
            /main/solo_then_judge/grants/2/judge/slot: `train_judge` has no slot named `submission`: name one of its slots
            /main/solo_then_judge/score/role: `agent` is an agent: name a judge"#]],
    );
}

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
            /roles/2/egress/model: `final_judge` is a judge, and only the agent has a model route: remove it
            /roles/3/runtime: `helper` is a second agent, after `agent`: a workflow has exactly one
            /main/solo_then_judge/grants/0/judge/slot: `train_judge` has no slot named `submission`: name one of its slots
            /main/solo_then_judge/grants/3/judge/role: `train_judge` is named by judge grants `train` and `retrain`: name each judge from one grant
            /main/solo_then_judge/grants/2/submit: `resubmit` is a second submit grant, after `submit`: give the agent exactly one
            /main/solo_then_judge/score/role: `train_judge` is also the role of judge grant `train`: name a judge no grant names
            /roles: no role is an agent: a workflow has exactly one
            /main/solo_then_judge/grants: no submit grant: give the agent exactly one
            missing field `call_timeout_seconds`"#]],
    );
}
