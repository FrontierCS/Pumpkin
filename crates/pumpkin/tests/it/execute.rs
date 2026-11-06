//! Executing a prepared run through the library's entry point, [`Run::execute`], on the
//! simulated provider: what it returns, its record, its run log, and what it leaves behind.

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use expect_test::{Expect, expect};
use pumpkin::driver::core::lasts;
use pumpkin::driver::run::Run;
use pumpkin_provider::Provider;
use pumpkin_schema::ir;
use pumpkin_schema::provider::{Listed, RunTag};
use pumpkin_schema::workflow::{Main, Workflow};
use serde_json::{Value, json};

use crate::run::{Runs, compact, files};
use crate::sim::{LOST, MUTE, Sim, UNDELETABLE, UNPUBLISHED};

const LWE: &str = include_str!("../../../pumpkin-schema/tests/fixtures/lwe.json");

/// A `sandboxd` that dials.
const SANDBOXD: &str = "/sim/sandboxd";

/// The startup of a simulated provider whose tests wait for a start, or a teardown, to run out:
/// short, so that the wait takes seconds, but still far longer than a simulated sandbox takes
/// to start, so that a busy machine makes a start that succeeds wait longer, never fail.
const SHORT_STARTUP: u64 = 2;

/// Prepares `ir` as test `name` and executes it on `provider`, with `sandboxd`, and checks
/// what becomes of it: what `execute` returns, the record's files, the payloads it holds, its
/// `provider.json` and `outcome.json`, its log, when its last record came ([`when`]), whether
/// the run's socket directories are left, their directory's mode at each create, and what the
/// run's list finds once it has ended. Each record's time is shown only if it is 0 or isn't a
/// decimal string, the run's start only if it isn't within the call to `execute`, the time left
/// in a reply as `<s>`, and the socket directories as `<sockets>`.
fn check(name: &str, provider: &Sim, ir: &Value, sandboxd: &str, expect: &Expect) {
    let runs = Runs::new(name);
    let ir: ir::Workflow = serde_json::from_value(ir.clone()).unwrap();
    let run = Run::prepare(&ir, &runs.dir, SystemTime::now()).unwrap();
    let (record, tag) = (run.dir().to_owned(), RunTag::try_from(run.id().to_owned()));
    let sockets = std::env::temp_dir().join(format!("pumpkin-{}", run.id()));
    let before = unix_ns();
    let executed = run.execute(provider, Path::new(sandboxd));
    let started = before..=unix_ns();
    let mut lines = vec![match executed {
        Ok(outcome) => format!("finished, with score {:?}", outcome.score()),
        Err(reason) => format!("failed: {reason}"),
    }];
    lines.push(format!("record: {}", files(&record, &record).join(", ")));
    let payloads = fs::read_dir(record.join("payloads")).into_iter().flatten();
    let mut payloads: Vec<String> = payloads
        .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect();
    payloads.sort();
    lines.push(format!("payloads hold: {payloads:?}"));
    for name in ["provider.json", "outcome.json"] {
        let json = fs::read_to_string(record.join(name)).unwrap();
        lines.push(format!("{name}: {}", compact(&json)));
    }
    lines.push("log.jsonl:".to_owned());
    let mut last = 0;
    // A run that fails before it starts has no log.
    let log = fs::read_to_string(record.join("log.jsonl")).unwrap_or_default();
    for line in log.lines() {
        let mut logged: Value = serde_json::from_str(line).unwrap();
        last = last.max(nanos(&logged["at_ns"]).unwrap());
        if nanos(&logged["at_ns"]).is_some_and(|ns| ns > 0) {
            logged["at_ns"] = json!("<ns>");
        }
        if let Some(unix_ns) = logged.pointer_mut("/event/unix_ns")
            && nanos(unix_ns).is_some_and(|ns| started.contains(&ns))
        {
            *unix_ns = json!("<during execute>");
        }
        let actions = logged.get_mut("actions").and_then(Value::as_array_mut);
        for action in actions.into_iter().flatten() {
            if let Some(seconds) = action.pointer_mut("/reply/remaining_seconds") {
                *seconds = json!("<s>");
            }
        }
        lines.push(logged.to_string());
    }
    if !log.is_empty() {
        let last = when(&ir, provider, Duration::from_nanos(last));
        lines.push(format!("its last record came {last}"));
    }
    lines.push(format!("socket directories left: {}", sockets.exists()));
    let modes = provider.modes().into_iter().map(|mode| format!("{mode:o}"));
    let modes: Vec<String> = modes.collect();
    lines.push(format!("their directory's mode at each create: {modes:?}"));
    let listed = provider.list(&tag.unwrap()).unwrap();
    let listed: Vec<String> = (listed.into_iter())
        .map(|Listed { id }| id.as_str().to_owned())
        .collect();
    lines.push(format!("listed once it ended: {listed:?}"));
    let sockets = sockets.display().to_string();
    expect.assert_eq(&lines.join("\n").replace(&sockets, "<sockets>"));
}

/// When `last`, a time since a run of `ir` on `provider` started, came: before the agent's
/// teardown was to end, at most the hard end ([`lasts`]) less the final judge's startup and
/// timeout; before the hard end; or after it.
fn when(ir: &ir::Workflow, provider: &Sim, last: Duration) -> &'static str {
    let workflow = Workflow::parse(ir).unwrap();
    let Main::SoloThenJudge(pattern) = workflow.main();
    let startup = provider.ready().capabilities.startup_seconds.get();
    let startup = Duration::from_secs(startup);
    let hard_end = lasts(&workflow, startup);
    let teardown_end = hard_end.saturating_sub(startup + pattern.score.timeout);
    if last < teardown_end {
        "before the agent's teardown was to end"
    } else if last < hard_end {
        "once the agent's teardown was to end, before the hard end"
    } else {
        "once the hard end had passed"
    }
}

/// The fixture's workflow, without the agent's model route, which the driver refuses, and with
/// `argv` as the agent's command.
fn workflow(argv: &[&str]) -> Value {
    let mut lwe: Value = serde_json::from_str(LWE).unwrap();
    let agent = &mut lwe["roles"][0];
    agent.as_object_mut().unwrap().remove("egress");
    agent["runtime"]["agent"]["command"]["argv"] = json!(argv);
    lwe
}

/// The workflow of the runs whose agents make ops: [`workflow`]'s, with `argv` as the agent's
/// command and `deadline` as the pattern's deadline, in seconds, room in the train judge's slot
/// for 8 bytes and in the score's for 16, the most an op of the agent's may carry, and a call
/// timeout of 1 s, shorter than the time left in any run that makes its calls soon after it
/// starts, so that it is each call's timeout.
fn ops_workflow(argv: &[&str], deadline: u64) -> Value {
    let mut ir = workflow(argv);
    ir["main"]["solo_then_judge"]["grants"][0]["judge"]["call_timeout_seconds"] = json!(1);
    ir["roles"][1]["runtime"]["judge"]["slots"][0]["max_bytes"] = json!(8);
    ir["roles"][2]["runtime"]["judge"]["slots"][0]["max_bytes"] = json!(16);
    ir["main"]["solo_then_judge"]["deadline_seconds"] = json!(deadline);
    ir
}

/// A run whose agent's command exits: its exit ends the work phase without a submission, and
/// the agent's sandbox is collected and deleted, so the run finishes without a score.
#[test]
fn exits() {
    // A deadline far later than the exit, so that an exit the run misses ends the work phase.
    let mut ir = workflow(&["exit", "3"]);
    ir["main"]["solo_then_judge"]["deadline_seconds"] = json!(30);
    check(
        "exits",
        &Sim::new(),
        &ir,
        SANDBOXD,
        &expect![[r#"
            finished, with score None
            record: agent/log/collected, agent/out/collected, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            payloads hold: []
            provider.json: {"capabilities":{"startup_seconds":30},"report":{"provider":"sim"}}
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":3}}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"kind":"run_started","unix_ns":"<during execute>"},"seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"seq":1}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":3}}}}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":3}},"seq":2}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":3}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: []"#]],
    );
}

/// A run whose agent's `sandboxd` ends the session once the agent's command has started: the
/// agent fails, and its sandbox is deleted.
#[test]
fn crashes() {
    // A deadline far later than the session's end, so that an end the run misses ends the work
    // phase.
    let mut ir = workflow(&["crash"]);
    ir["main"]["solo_then_judge"]["deadline_seconds"] = json!(30);
    check(
        "crashes",
        &Sim::new(),
        &ir,
        SANDBOXD,
        &expect![[r#"
            finished, with score None
            record: agent/, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            payloads hold: []
            provider.json: {"capabilities":{"startup_seconds":30},"report":{"provider":"sim"}}
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"kind":"run_started","unix_ns":"<during execute>"},"seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"seq":1}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"}}}],"at_ns":"<ns>","event":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"},"seq":2}
            {"at_ns":"<ns>","note":"freezing the agent's sandbox failed: the sandbox ended the session","seq":3}
            {"at_ns":"<ns>","note":"the agent's out is incomplete: collecting /pumpkin/out failed: the sandbox ended the session","seq":4}
            {"at_ns":"<ns>","note":"the agent's log is incomplete: collecting /pumpkin/log failed: the sandbox ended the session","seq":5}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, nothing; log, nothing","seq":6}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: []"#]],
    );
}

/// A run whose agent's sandbox stalls once its command has started, and whose command never
/// exits: the deadline ends the work phase, and freezing the sandbox runs out at the
/// teardown's end, which leaves the final judge its startup and timeout before the hard end;
/// what couldn't be collected is noted, and the sandbox is deleted.
#[test]
fn stalls() {
    let mut ir = workflow(&["stall"]);
    let pattern = &mut ir["main"]["solo_then_judge"];
    pattern["deadline_seconds"] = json!(1);
    pattern["finalize_grace_seconds"] = json!(0);
    check(
        "stalls",
        &Sim::with_startup(SHORT_STARTUP),
        &ir,
        SANDBOXD,
        &expect![[r#"
            finished, with score None
            record: agent/, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            payloads hold: []
            provider.json: {"capabilities":{"startup_seconds":2},"report":{"provider":"sim"}}
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"deadline"}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"kind":"run_started","unix_ns":"<during execute>"},"seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"seq":1}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"deadline"}}}],"at_ns":"<ns>","event":{"kind":"tick"},"seq":2}
            {"at_ns":"<ns>","note":"freezing the agent's sandbox failed: the sandbox didn't answer before the request's deadline: expected its answer while the deadline lasts","seq":3}
            {"at_ns":"<ns>","note":"the agent's out is incomplete: collecting /pumpkin/out failed: the sandbox didn't answer before the request's deadline: expected its answer while the deadline lasts","seq":4}
            {"at_ns":"<ns>","note":"the agent's log is incomplete: collecting /pumpkin/log failed: the sandbox didn't answer before the request's deadline: expected its answer while the deadline lasts","seq":5}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, nothing; log, nothing","seq":6}
            its last record came once the agent's teardown was to end, before the hard end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: []"#]],
    );
}

/// A run whose agent's `sandboxd` never dials: its start runs out at the provider's startup,
/// and the sandbox is deleted, with its console quoted in the agent's failure. The start
/// outlasts the deadline, so the core hears of the failure before the deadline's tick, which
/// ends the work phase as the failure is taken.
#[test]
fn unstarted() {
    let mut ir = workflow(&["exit", "0"]);
    ir["main"]["solo_then_judge"]["deadline_seconds"] = json!(1);
    check(
        "unstarted",
        &Sim::with_startup(SHORT_STARTUP),
        &ir,
        MUTE,
        &expect![[r#"
            finished, with score None
            record: ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            payloads hold: []
            provider.json: {"capabilities":{"startup_seconds":2},"report":{"provider":"sim"}}
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"deadline"}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"kind":"run_started","unix_ns":"<during execute>"},"seq":0}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"deadline"}}}],"at_ns":"<ns>","event":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: sandbox sim-1: no sandbox connected to <sockets>/agent/s by the deadline: expected its sandbox to connect while the deadline lasts; its console ends: sandboxd: a simulated one, which never dials"},"seq":1}
            {"at_ns":"<ns>","note":"the agent's sandbox never started: nothing was collected","seq":2}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: []"#]],
    );
}

/// A run whose agent's create fails, leaving behind a sandbox it can't name: the agent fails,
/// and the run's end deletes the sandbox, which the run's list still finds.
#[test]
fn lost() {
    check(
        "lost",
        &Sim::new(),
        &workflow(&["exit", "0"]),
        LOST,
        &expect![[r#"
            finished, with score None
            record: ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            payloads hold: []
            provider.json: {"capabilities":{"startup_seconds":30},"report":{"provider":"sim"}}
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: the simulated create failed, and can't name what it made: expected the run's list to find it"}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"kind":"run_started","unix_ns":"<during execute>"},"seq":0}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: the simulated create failed, and can't name what it made: expected the run's list to find it"}}}],"at_ns":"<ns>","event":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: the simulated create failed, and can't name what it made: expected the run's list to find it"},"seq":1}
            {"at_ns":"<ns>","note":"the agent's sandbox never started: nothing was collected","seq":2}
            {"at_ns":"<ns>","note":"deleted the sandboxes the run left: sim-1","seq":3}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: []"#]],
    );
}

/// A run whose agent's sandbox can't be deleted: its teardown fails the run, whose end tries to
/// delete it again and fails too, so the run fails with both reasons.
#[test]
fn undeletable() {
    check(
        "undeletable",
        &Sim::new(),
        &workflow(&["exit", "0"]),
        UNDELETABLE,
        &expect![[r#"
            failed: deleting sandbox sim-1 failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`
            record: agent/log/collected, agent/out/collected, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            payloads hold: []
            provider.json: {"capabilities":{"startup_seconds":30},"report":{"provider":"sim"}}
            outcome.json: {"reason":"deleting sandbox sim-1 failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`","state":"failed"}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"kind":"run_started","unix_ns":"<during execute>"},"seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"seq":1}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"seq":2}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":3}
            {"at_ns":"<ns>","note":"the run failed: deleting sandbox sim-1 failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`","seq":4}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: ["sim-1"]"#]],
    );
}

/// A run whose `sandboxd` the provider can't publish: the run fails before it starts, and
/// nothing is created.
#[test]
fn unpublished() {
    check(
        "unpublished",
        &Sim::new(),
        &workflow(&["exit", "0"]),
        UNPUBLISHED,
        &expect![[r#"
            failed: publishing what the run's sandboxes run failed: /sim/unpublished can't be published by the simulated provider: name another path
            record: ir.json, outcome.json, provider.json, record.json
            payloads hold: []
            provider.json: {"capabilities":{"startup_seconds":30},"report":{"provider":"sim"}}
            outcome.json: {"reason":"publishing what the run's sandboxes run failed: /sim/unpublished can't be published by the simulated provider: name another path","state":"failed"}
            log.jsonl:
            socket directories left: false
            their directory's mode at each create: []
            listed once it ended: []"#]],
    );
}

/// A run whose agent makes ops: `ops`; `status` of a job it doesn't have; a judge call to a
/// grant it doesn't hold; one with a payload over the grant's slot, which the core refuses; one
/// over the most an op may carry, which the gateway refuses, without the core; one the core
/// accepts, whose call fails, as this runtime runs no judge call; `status` of that job; and a
/// final submission, whose score fails too, so that the run finishes without a score. Every
/// reply reaches the agent, which makes each op once the one before it is answered, and exits
/// once the last is; the record keeps the accepted payloads alone.
#[test]
fn ops() {
    let argv = [
        "ops",
        "ops",
        "status 0",
        "judge nope 6",
        "judge train 9",
        "judge train 17",
        "judge train 6",
        "status 0",
        "finalize submit 6",
    ];
    // A deadline far later than the agent's last op, so that a reply the agent misses ends the
    // work phase.
    check(
        "ops",
        &Sim::new(),
        &ops_workflow(&argv, 30),
        SANDBOXD,
        &expect![[r#"
            finished, with score None
            record: agent/log/collected, agent/out/collected, ir.json, log.jsonl, outcome.json, payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd, provider.json, record.json
            payloads hold: ["xxxxxx"]
            provider.json: {"capabilities":{"startup_seconds":30},"report":{"provider":"sim"}}
            outcome.json: {"score":null,"state":"finished","verdict":{"failure":{"kind":"error","reason":"this runtime runs no judge call yet"},"kind":"no_result","submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"kind":"run_started","unix_ns":"<during execute>"},"seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"grants":[{"call_timeout_seconds":1,"calls":100,"cumulative_seconds":12000,"kind":"judge","max_bytes":8,"name":"train"},{"kind":"submit","max_bytes":16,"name":"submit"}],"kind":"ops","remaining_seconds":"<s>","resources":{"cpu_millis":8000,"memory_bytes":34359738368}}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":0,"request":{"kind":"ops"}},"seq":2}
            {"actions":[{"kind":"reply","op":1,"reply":{"kind":"refused","message":"you have no job 0: ask about a job that one of your calls returned"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":1,"request":{"job":"0","kind":"status"}},"seq":3}
            {"actions":[{"kind":"reply","op":2,"reply":{"kind":"refused","message":"you hold no grant named `nope`: call one that `ops` lists"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":2,"request":{"grant":"nope","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"seq":4}
            {"actions":[{"kind":"reply","op":3,"reply":{"kind":"refused","message":"the payload is 9 bytes, over `train`'s limit of 8: send a smaller one"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":3,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:a73add1aecea03a894ca65d99bc5e91d21b58cdb728343bbcb6cf6cf38b809f5","size":9}}},"seq":5}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"op_refused","message":"the payload is 17 bytes, more than the limit of 16: send at most 16","request":{"grant":"train","kind":"judge","payload":{"over":16,"size":17}}},"seq":6}
            {"actions":[{"kind":"reply","op":4,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":12000}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":4,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"seq":7}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_failed","reason":"this runtime runs no judge call yet"},"seq":8}
            {"actions":[{"kind":"reply","op":5,"reply":{"failure":{"code":"judge_error","message":"the judge failed before giving a result"},"job":0,"kind":"status","remaining":{"calls":99,"seconds":12000},"state":"failed"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":5,"request":{"job":"0","kind":"status"}},"seq":9}
            {"actions":[{"kind":"reply","op":6,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"accepted"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":6,"request":{"finalize":true,"grant":"submit","kind":"submit","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"seq":10}
            {"actions":[{"kind":"tear_down_agent"},{"grant":null,"job":1,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"120000000000"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"seq":11}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":12}
            {"actions":[{"kind":"finish","outcome":{"score":null,"verdict":{"failure":{"kind":"error","reason":"this runtime runs no judge call yet"},"kind":"no_result","submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"job":1,"kind":"judge_failed","reason":"this runtime runs no judge call yet"},"seq":13}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: []"#]],
    );
}

/// A run whose agent's `sandboxd` ends the session while the agent's judge call is in flight,
/// its payload over the score's slot but within the judge's, the larger, so within the most an
/// op may carry: the op arrived whole, so the core takes it, and accepts it, before the
/// session's end, which fails the agent; and the reply can't be sent, which the run log notes.
#[test]
fn crashes_with_an_op_in_flight() {
    let mut ir = ops_workflow(&["ops", "judge train 12", "crash"], 30);
    ir["roles"][1]["runtime"]["judge"]["slots"][0]["max_bytes"] = json!(16);
    ir["roles"][2]["runtime"]["judge"]["slots"][0]["max_bytes"] = json!(8);
    check(
        "crashes-with-an-op",
        &Sim::new(),
        &ir,
        SANDBOXD,
        &expect![[r#"
            finished, with score None
            record: agent/, ir.json, log.jsonl, outcome.json, payloads/59ffe12a70df15109e0345955e3230a978f31ebc28d8fe3e42d306afb28b8e81, provider.json, record.json
            payloads hold: ["xxxxxxxxxxxx"]
            provider.json: {"capabilities":{"startup_seconds":30},"report":{"provider":"sim"}}
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"kind":"run_started","unix_ns":"<during execute>"},"seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":12000}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:59ffe12a70df15109e0345955e3230a978f31ebc28d8fe3e42d306afb28b8e81","size":12},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:59ffe12a70df15109e0345955e3230a978f31ebc28d8fe3e42d306afb28b8e81","size":12}}},"seq":2}
            {"at_ns":"<ns>","note":"op 0's reply failed: the sandbox ended the session","seq":3}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_failed","reason":"this runtime runs no judge call yet"},"seq":4}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"}}}],"at_ns":"<ns>","event":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"},"seq":5}
            {"at_ns":"<ns>","note":"freezing the agent's sandbox failed: the sandbox ended the session","seq":6}
            {"at_ns":"<ns>","note":"the agent's out is incomplete: collecting /pumpkin/out failed: the sandbox ended the session","seq":7}
            {"at_ns":"<ns>","note":"the agent's log is incomplete: collecting /pumpkin/log failed: the sandbox ended the session","seq":8}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, nothing; log, nothing","seq":9}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: []"#]],
    );
}

/// A run whose agent waits 2 s once its first `ops` is answered before it makes its second,
/// timed as the loop takes it, so that its reply leaves at least 2 s less than the first's,
/// however long the agent took to start, which the agent checks, exiting with 1 if not. It then
/// exits, and its sandbox makes a judge call after the exit, as a process the agent left would:
/// the call arrives once the core has finished the run, so the core refuses it, and the refusal
/// can't be sent, which the run log notes.
#[test]
fn late_ops() {
    let argv = [
        "ops",
        "ops",
        "sleep 2",
        "ops at least 2 less",
        "exit",
        "judge train 6",
    ];
    check(
        "late-ops",
        &Sim::new(),
        &ops_workflow(&argv, 30),
        SANDBOXD,
        &expect![[r#"
            finished, with score None
            record: agent/log/collected, agent/out/collected, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            payloads hold: []
            provider.json: {"capabilities":{"startup_seconds":30},"report":{"provider":"sim"}}
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"kind":"run_started","unix_ns":"<during execute>"},"seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"grants":[{"call_timeout_seconds":1,"calls":100,"cumulative_seconds":12000,"kind":"judge","max_bytes":8,"name":"train"},{"kind":"submit","max_bytes":16,"name":"submit"}],"kind":"ops","remaining_seconds":"<s>","resources":{"cpu_millis":8000,"memory_bytes":34359738368}}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":0,"request":{"kind":"ops"}},"seq":2}
            {"actions":[{"kind":"reply","op":1,"reply":{"grants":[{"call_timeout_seconds":1,"calls":100,"cumulative_seconds":12000,"kind":"judge","max_bytes":8,"name":"train"},{"kind":"submit","max_bytes":16,"name":"submit"}],"kind":"ops","remaining_seconds":"<s>","resources":{"cpu_millis":8000,"memory_bytes":34359738368}}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":1,"request":{"kind":"ops"}},"seq":3}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"seq":4}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":5}
            {"actions":[{"kind":"reply","op":2,"reply":{"kind":"refused","message":"the work phase has ended: make no more ops"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":2,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"seq":6}
            {"at_ns":"<ns>","note":"op 2's reply wasn't sent: the agent's sandbox is gone","seq":7}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: []"#]],
    );
}

/// Nanoseconds since the Unix epoch.
fn unix_ns() -> u64 {
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    u64::try_from(since.as_nanos()).unwrap()
}

/// A time or duration of the log's, a decimal string of nanoseconds, as a number.
fn nanos(value: &Value) -> Option<u64> {
    value.as_str()?.parse().ok()
}
