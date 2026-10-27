//! The workflow IR and its parsing, tested through the crate's public API against a
//! hand-written workflow for the LWE task (`lwe_structured_recovery`) that follows the task's
//! policy. Its image digests, the agent's command and the judges' environment are placeholders.

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]

use expect_test::{Expect, expect};
use pumpkin_schema::ir::{
    self, AbsolutePath, Argv, Env, Image, Models, Name, Sha256Digest, WorkflowName,
};
use pumpkin_schema::workflow::{Holder, Main, RoleId, Workflow};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

const LWE: &str = include_str!("fixtures/lwe.json");

const HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn lwe_json() -> Value {
    serde_json::from_str(LWE).unwrap()
}

/// Deserializes and parses `ir`, and checks the rules it breaks against `expect`.
fn check(ir: Value, expect: &Expect) {
    let ir: ir::Workflow = serde_json::from_value(ir).unwrap();
    let actual = match Workflow::parse(&ir) {
        Ok(_) => "parsed".to_owned(),
        Err(errors) => errors.to_string(),
    };
    expect.assert_eq(&actual);
}

/// Deserializes a single value, and renders it with the outcome.
fn deserialize<T: DeserializeOwned>(value: &Value) -> String {
    match serde_json::from_value::<T>(value.clone()) {
        Ok(_) => format!("{value}: ok"),
        Err(error) => format!("{value}: {error}"),
    }
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
    let actual = [
        deserialize::<Name>(&json!("train_judge")),
        deserialize::<Name>(&json!("Train")),
        deserialize::<WorkflowName>(&json!("frontier-cs/lwe_structured_recovery")),
        deserialize::<WorkflowName>(&json!("lwe structured recovery")),
        deserialize::<Image>(&json!(format!("pumpkin/lwe-agent@sha256:{HEX}"))),
        deserialize::<Image>(&json!(format!("sha256:{HEX}"))),
        deserialize::<Image>(&json!("ubuntu:24.04")),
        deserialize::<Sha256Digest>(&json!("md5:abc")),
        deserialize::<AbsolutePath>(&json!("/opt/frontier")),
        deserialize::<AbsolutePath>(&json!("/pumpkin/../etc")),
        deserialize::<AbsolutePath>(&json!("workspace")),
        deserialize::<Argv>(&json!([])),
        deserialize::<Env>(&json!(["MODE=train"])),
        deserialize::<Env>(&json!(["1ST=value"])),
        deserialize::<Env>(&json!(["MODE=train", "MODE=eval"])),
        deserialize::<Models>(&json!([])),
        deserialize::<Models>(&json!(["gpt-6-astra", "gpt-6-astra"])),
    ];
    expect![[r#"
        "train_judge": ok
        "Train": `Train` is not a valid name: use 1 to 64 characters from a-z, 0-9, `_` and `-`, starting with a letter
        "frontier-cs/lwe_structured_recovery": ok
        "lwe structured recovery": `lwe structured recovery` is not a valid workflow name: use 1 to 128 characters from A-Z, a-z, 0-9, `.`, `_`, `-` and `/`
        "pumpkin/lwe-agent@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef": ok
        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef": ok
        "ubuntu:24.04": `ubuntu:24.04` is not a pinned image: use `<repository>@sha256:<hex>` or `sha256:<hex>`
        "md5:abc": `md5:abc` is not a `sha256:<hex>` digest
        "/opt/frontier": ok
        "/pumpkin/../etc": `/pumpkin/../etc` is not a normalized absolute path
        "workspace": `workspace` is not a normalized absolute path
        []: the command is empty: it needs a program
        ["MODE=train"]: ok
        ["1ST=value"]: `1ST` is not a valid environment variable name
        ["MODE=train","MODE=eval"]: `MODE` is set more than once
        []: no models: a model route allows at least one
        ["gpt-6-astra","gpt-6-astra"]: `gpt-6-astra` is listed more than once"#]].assert_eq(&actual.join("\n"));
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
        ir,
        &expect![[r#"
            roles[final_judge].runtime.judge.slots[payload]: the name is already used
            roles[root].name: `root` is reserved
            phases[work].live: `agent` is already listed
            phases[work].live: no role is named `ghost`
            phases[work].next: no phase is named `later`
            phases[final].next: the phase follows itself: it may not
            grants[train]: the name is already used
            grants[train].holder: no role is named `ghost`
            main.solo_then_judge.agent: `train_judge` is a judge, not an agent
            main.solo_then_judge.score: `submit` is not a judge grant"#]],
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
        ir,
        &expect![[r#"
            grants[train].holder: `agent` is not live in phase `final`
            grants[train].op.judge.role: `train_judge` is not live in phase `final`
            grants[train].op.judge.slot: `train_judge` has no slot named `submission`
            grants[submit].holder: the root holds a submit grant: only a role may
            grants[score].op.judge.role: `agent` is an agent, not a judge
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
        ir,
        &expect![[r#"
        main.solo_then_judge.work_phase: `work` is not the first phase, in which a run starts
        main.solo_then_judge.final_phase: the work phase `work` is not followed by `final`
        main.solo_then_judge.agent: `agent` is live in the final phase `final`, so it would survive into judging
        main.solo_then_judge.score: `score` judges with `train_judge`, which is train-mode
        main.solo_then_judge.submit: submissions of up to 8388608 bytes do not fit the score slot's 4194304 bytes"#]],
    );
}
