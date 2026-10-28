//! The checker, tested through its report on the LWE workflow (`lwe_structured_recovery`)
//! that `pumpkin-schema` tests parsing with, and on variants of it.

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]

use std::collections::HashSet;

use expect_test::{Expect, expect};
use pumpkin_schema::ir;
use pumpkin_schema::workflow::Workflow;
use serde_json::{Value, json};

const LWE: &str = include_str!("../../pumpkin-schema/tests/fixtures/lwe.json");

fn report(ir: Value) -> String {
    let ir: ir::Workflow = serde_json::from_value(ir).unwrap();
    let workflow = Workflow::parse(&ir).unwrap();
    pumpkin_checker::check(&workflow).to_string()
}

/// Checks the LWE workflow with `edit` applied, and compares its report with `expect`: the
/// verdict and the sections that differ from LWE's, or the whole report when none differ.
fn check(edit: impl FnOnce(&mut Value), expect: &Expect) {
    let mut ir: Value = serde_json::from_str(LWE).unwrap();
    let lwe = report(ir.clone());
    edit(&mut ir);
    let report = report(ir);
    let unchanged: HashSet<&str> = lwe.split("\n\n").collect();
    let mut sections = report.split("\n\n");
    let verdict = sections.next().into_iter();
    let changed: Vec<&str> = sections.filter(|s| !unchanged.contains(s)).collect();
    if changed.is_empty() {
        expect.assert_eq(&report);
    } else {
        expect.assert_eq(&verdict.chain(changed).collect::<Vec<_>>().join("\n\n"));
    }
}

#[test]
fn lwe() {
    check(
        |_| {},
        &expect![[r#"
            frontier-cs/lwe_structured_recovery: passes

            Rules
              holds: final-mode assets only on final-mode judges, declared or in their images
              holds: no information path from a final-mode judge to an untrusted principal live in the same or a later phase

            Assumed by frontier-cs/lwe_structured_recovery
              train_judge is trusted: its own code acts as its author intends
              final_judge is trusted: its own code acts as its author intends
              train_judge.payload is a data slot: train_judge parses it without running it
              final_judge.payload is a data slot: final_judge parses it without running it
              agent declares no assets: its image holds no final-mode data
              train_judge declares no assets: its image holds no final-mode data
              final_judge declares no assets: its image holds no final-mode data

            Assumed by Pumpkin
              agent's model route to gpt-6-astra is closed: one pinned upstream, no onward fetching, and no state shared across requests

            Principals
              agent: an untrusted agent, live in work
              train_judge: a trusted train-mode judge, live in work
              final_judge: a trusted final-mode judge, live in final
              root: the pattern, solo_then_judge, trusted and live in every phase
              operator: trusted, receives the outcome
              model provider: serves model routes

            Flows, where r@p is r's instances in phase p, and a code flow gives control of its receiver to its sender and to whatever reaches the sender
              in work:
                agent@work -> train_judge@work (data): train's payload, into slot payload
                train_judge@work -> agent@work (code): train's results
                agent@work -> root@work (data): submit's submissions
                root@work -> agent@work (code): submit's receipts
                agent@work -> model provider (data): model requests
                agent@work -> agent@work (code): model responses, which carry only what it sent
                then final, survived by root
              in final:
                root@final -> final_judge@final (data): score's payload, into slot payload
                final_judge@final -> root@final (data): score's results
                root@final -> operator (data): the outcome

            Authority, Auth(R): what each node reaches, controls, and can use itself or through the nodes it controls
              agent@work:
                reaches train_judge@work, root@work, final_judge@final, root@final, operator, model provider
                controls nothing
                can use train, submit, model route
              train_judge@work:
                reaches agent@work, root@work, final_judge@final, root@final, operator, model provider
                controls agent@work
                can use agent@work's train, submit, model route
              root@work:
                reaches agent@work, train_judge@work, final_judge@final, root@final, operator, model provider
                controls agent@work
                can use agent@work's train, submit, model route
              final_judge@final:
                reaches root@final, operator
                controls nothing
                can use nothing
              root@final:
                reaches final_judge@final, operator
                controls nothing
                can use score

            Untrusted code: the nodes that run an untrusted principal's code, and whose
              agent@work runs code from agent@work

            Trust domains: the nodes that control each other
              none: no two nodes control each other"#]],
    );
}

/// Both judges run what the agent submits. The train judge's results return to the agent, so
/// the two control each other; the final judge runs the agent's code a phase later, through
/// the root, without controlling the agent back.
#[test]
fn code_slots() {
    check(
        |ir| {
            ir["roles"][1]["runtime"]["judge"]["slots"][0]["kind"] = json!("code");
            ir["roles"][2]["runtime"]["judge"]["slots"][0]["kind"] = json!("code");
        },
        &expect![[r#"
            frontier-cs/lwe_structured_recovery: passes

            Assumed by frontier-cs/lwe_structured_recovery
              train_judge is trusted: its own code acts as its author intends
              final_judge is trusted: its own code acts as its author intends
              agent declares no assets: its image holds no final-mode data
              train_judge declares no assets: its image holds no final-mode data
              final_judge declares no assets: its image holds no final-mode data
              train's results from train_judge@work rest on the task's own isolation inside train_judge's sandbox, which runs code from agent@work: that code cannot forge them
              score's results from final_judge@final rest on the task's own isolation inside final_judge's sandbox, which runs code from agent@work: that code cannot forge them

            Flows, where r@p is r's instances in phase p, and a code flow gives control of its receiver to its sender and to whatever reaches the sender
              in work:
                agent@work -> train_judge@work (code): train's payload, into slot payload
                train_judge@work -> agent@work (code): train's results
                agent@work -> root@work (data): submit's submissions
                root@work -> agent@work (code): submit's receipts
                agent@work -> model provider (data): model requests
                agent@work -> agent@work (code): model responses, which carry only what it sent
                then final, survived by root
              in final:
                root@final -> final_judge@final (code): score's payload, into slot payload
                final_judge@final -> root@final (data): score's results
                root@final -> operator (data): the outcome

            Authority, Auth(R): what each node reaches, controls, and can use itself or through the nodes it controls
              agent@work:
                reaches train_judge@work, root@work, final_judge@final, root@final, operator, model provider
                controls train_judge@work, final_judge@final
                can use train, submit, model route
              train_judge@work:
                reaches agent@work, root@work, final_judge@final, root@final, operator, model provider
                controls agent@work, final_judge@final
                can use agent@work's train, submit, model route
              root@work:
                reaches agent@work, train_judge@work, final_judge@final, root@final, operator, model provider
                controls agent@work, train_judge@work, final_judge@final
                can use agent@work's train, submit, model route
              final_judge@final:
                reaches root@final, operator
                controls nothing
                can use nothing
              root@final:
                reaches final_judge@final, operator
                controls final_judge@final
                can use score

            Untrusted code: the nodes that run an untrusted principal's code, and whose
              agent@work runs code from agent@work
              train_judge@work runs code from agent@work
              final_judge@final runs code from agent@work

            Trust domains: the nodes that control each other
              agent@work, train_judge@work"#]],
    );
}

/// The final judge declares a final-mode asset, as it may. The train judge declares it too,
/// and runs the final judge's image besides.
#[test]
fn final_mode_assets() {
    check(
        |ir| {
            let eval = json!([{
                "name": "eval",
                "path": "/opt/frontier/lwe/datasets/eval.json",
                "digest": "sha256:3333333333333333333333333333333333333333333333333333333333333333",
                "mode": "final"
            }]);
            ir["roles"][1]["assets"] = eval.clone();
            ir["roles"][2]["assets"] = eval;
        },
        &expect![[r#"
            frontier-cs/lwe_structured_recovery: fails

            Rules
              broken: final-mode assets only on final-mode judges, declared or in their images
                train_judge declares the final-mode asset eval but is a train-mode judge: move the asset to a final-mode judge
                train_judge runs the image of final_judge, which declares final-mode assets, but is a train-mode judge: give it an image without them
              holds: no information path from a final-mode judge to an untrusted principal live in the same or a later phase

            Assumed by frontier-cs/lwe_structured_recovery
              train_judge is trusted: its own code acts as its author intends
              final_judge is trusted: its own code acts as its author intends
              train_judge.payload is a data slot: train_judge parses it without running it
              final_judge.payload is a data slot: final_judge parses it without running it
              agent declares no assets: its image holds no final-mode data
              train_judge declares eval (final-mode, at /opt/frontier/lwe/datasets/eval.json): its image holds no other final-mode data
              final_judge declares eval (final-mode, at /opt/frontier/lwe/datasets/eval.json): its image holds no other final-mode data"#]],
    );
}

/// The agent judges its work with a final-mode judge.
#[test]
fn final_mode_results_reach_agent() {
    check(
        |ir| ir["roles"][1]["runtime"]["judge"]["mode"] = json!("final"),
        &expect![[r#"
            frontier-cs/lwe_structured_recovery: fails

            Rules
              holds: final-mode assets only on final-mode judges, declared or in their images
              broken: no information path from a final-mode judge to an untrusted principal live in the same or a later phase
                final-mode train_judge reaches untrusted agent by train_judge@work -> agent@work: cut this path

            Principals
              agent: an untrusted agent, live in work
              train_judge: a trusted final-mode judge, live in work
              final_judge: a trusted final-mode judge, live in final
              root: the pattern, solo_then_judge, trusted and live in every phase
              operator: trusted, receives the outcome
              model provider: serves model routes"#]],
    );
}

/// The train judge is declared untrusted, so nothing vouches that it parses its data slot
/// without running it, and its results are only as good as its own code.
#[test]
fn untrusted_train_judge() {
    check(
        |ir| ir["roles"][1]["runtime"]["judge"]["trust"] = json!("untrusted"),
        &expect![[r#"
            frontier-cs/lwe_structured_recovery: passes

            Assumed by frontier-cs/lwe_structured_recovery
              final_judge is trusted: its own code acts as its author intends
              final_judge.payload is a data slot: final_judge parses it without running it
              agent declares no assets: its image holds no final-mode data
              train_judge declares no assets: its image holds no final-mode data
              final_judge declares no assets: its image holds no final-mode data
              train's results from train_judge@work come from untrusted train_judge's own code: the workflow takes them as they are

            Principals
              agent: an untrusted agent, live in work
              train_judge: an untrusted train-mode judge, live in work
              final_judge: a trusted final-mode judge, live in final
              root: the pattern, solo_then_judge, trusted and live in every phase
              operator: trusted, receives the outcome
              model provider: serves model routes

            Flows, where r@p is r's instances in phase p, and a code flow gives control of its receiver to its sender and to whatever reaches the sender
              in work:
                agent@work -> train_judge@work (code): train's payload, into slot payload
                train_judge@work -> agent@work (code): train's results
                agent@work -> root@work (data): submit's submissions
                root@work -> agent@work (code): submit's receipts
                agent@work -> model provider (data): model requests
                agent@work -> agent@work (code): model responses, which carry only what it sent
                then final, survived by root
              in final:
                root@final -> final_judge@final (data): score's payload, into slot payload
                final_judge@final -> root@final (data): score's results
                root@final -> operator (data): the outcome

            Authority, Auth(R): what each node reaches, controls, and can use itself or through the nodes it controls
              agent@work:
                reaches train_judge@work, root@work, final_judge@final, root@final, operator, model provider
                controls train_judge@work
                can use train, submit, model route
              train_judge@work:
                reaches agent@work, root@work, final_judge@final, root@final, operator, model provider
                controls agent@work
                can use agent@work's train, submit, model route
              root@work:
                reaches agent@work, train_judge@work, final_judge@final, root@final, operator, model provider
                controls agent@work, train_judge@work
                can use agent@work's train, submit, model route
              final_judge@final:
                reaches root@final, operator
                controls nothing
                can use nothing
              root@final:
                reaches final_judge@final, operator
                controls nothing
                can use score

            Untrusted code: the nodes that run an untrusted principal's code, and whose
              agent@work runs code from agent@work, train_judge@work
              train_judge@work runs code from agent@work, train_judge@work

            Trust domains: the nodes that control each other
              agent@work, train_judge@work"#]],
    );
}
