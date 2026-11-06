//! Executing a prepared run through the library's entry point, [`Run::execute`], on the
//! simulated provider: what it returns, its record, its run log, and what it leaves behind.

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]

use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use expect_test::{Expect, expect};
use pumpkin::driver::run::Run;
use pumpkin_provider::Provider;
use pumpkin_schema::ir;
use pumpkin_schema::provider::{Listed, RunTag};
use serde_json::{Value, json};

use crate::run::{Runs, compact, files};
use crate::sim::{LOST, MUTE, Sim, UNDELETABLE, UNPUBLISHED};

const LWE: &str = include_str!("../../../pumpkin-schema/tests/fixtures/lwe.json");

/// A `sandboxd` that dials.
const SANDBOXD: &str = "/sim/sandboxd";

/// The startup of a simulated provider whose tests wait for a start to run out: short, so that
/// the wait takes seconds, but still far longer than a simulated sandbox takes to start, so that
/// a busy machine makes a start that succeeds wait longer, never fail.
const SHORT_STARTUP: u64 = 2;

/// Prepares `ir` as test `name` and executes it on `provider`, with `sandboxd`, and checks
/// what becomes of it: what `execute` returns, the record's files, its `provider.json` and
/// `outcome.json`, its log, whether the run's socket directories are left, their directory's
/// mode at each create, and what the run's list finds once it has ended. Each record's time is
/// shown only if it is 0 or isn't a decimal string, the run's start only if it isn't within the
/// call to `execute`, and the socket directories as `<sockets>`.
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
    for name in ["provider.json", "outcome.json"] {
        let json = fs::read_to_string(record.join(name)).unwrap();
        lines.push(format!("{name}: {}", compact(&json)));
    }
    lines.push("log.jsonl:".to_owned());
    // A run that fails before it starts has no log.
    let log = fs::read_to_string(record.join("log.jsonl")).unwrap_or_default();
    for line in log.lines() {
        let mut logged: Value = serde_json::from_str(line).unwrap();
        if nanos(&logged["at_ns"]).is_some_and(|ns| ns > 0) {
            logged["at_ns"] = json!("<ns>");
        }
        if let Some(unix_ns) = logged.pointer_mut("/event/unix_ns")
            && nanos(unix_ns).is_some_and(|ns| started.contains(&ns))
        {
            *unix_ns = json!("<during execute>");
        }
        lines.push(logged.to_string());
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

/// The fixture's workflow, without the agent's model route, which the driver refuses, and with
/// `argv` as the agent's command.
fn workflow(argv: &[&str]) -> Value {
    let mut lwe: Value = serde_json::from_str(LWE).unwrap();
    let agent = &mut lwe["roles"][0];
    agent.as_object_mut().unwrap().remove("egress");
    agent["runtime"]["agent"]["command"]["argv"] = json!(argv);
    lwe
}

/// A run whose agent's command exits: its exit ends the work phase without a submission, and
/// the agent's sandbox is deleted, so the run finishes without a score.
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
            record: ir.json, log.jsonl, outcome.json, provider.json, record.json
            provider.json: {"capabilities":{"startup_seconds":30},"report":{"provider":"sim"}}
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":3}}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"kind":"run_started","unix_ns":"<during execute>"},"seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"seq":1}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":3}}}}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":3}},"seq":2}
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
            record: ir.json, log.jsonl, outcome.json, provider.json, record.json
            provider.json: {"capabilities":{"startup_seconds":30},"report":{"provider":"sim"}}
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"kind":"run_started","unix_ns":"<during execute>"},"seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"seq":1}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"}}}],"at_ns":"<ns>","event":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"},"seq":2}
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
            record: ir.json, log.jsonl, outcome.json, provider.json, record.json
            provider.json: {"capabilities":{"startup_seconds":2},"report":{"provider":"sim"}}
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"deadline"}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"kind":"run_started","unix_ns":"<during execute>"},"seq":0}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"deadline"}}}],"at_ns":"<ns>","event":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: sandbox sim-1: no sandbox connected to <sockets>/agent/s by the deadline: expected its sandbox to connect while the deadline lasts; its console ends: sandboxd: a simulated one, which never dials"},"seq":1}
            {"at_ns":"<ns>","note":"the agent's sandbox never started: nothing was collected","seq":2}
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
            record: ir.json, log.jsonl, outcome.json, provider.json, record.json
            provider.json: {"capabilities":{"startup_seconds":30},"report":{"provider":"sim"}}
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: the simulated create failed, and can't name what it made: expected the run's list to find it"}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"kind":"run_started","unix_ns":"<during execute>"},"seq":0}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: the simulated create failed, and can't name what it made: expected the run's list to find it"}}}],"at_ns":"<ns>","event":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: the simulated create failed, and can't name what it made: expected the run's list to find it"},"seq":1}
            {"at_ns":"<ns>","note":"the agent's sandbox never started: nothing was collected","seq":2}
            {"at_ns":"<ns>","note":"deleted the sandboxes the run left: sim-1","seq":3}
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
            record: ir.json, log.jsonl, outcome.json, provider.json, record.json
            provider.json: {"capabilities":{"startup_seconds":30},"report":{"provider":"sim"}}
            outcome.json: {"reason":"deleting sandbox sim-1 failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`","state":"failed"}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"kind":"run_started","unix_ns":"<during execute>"},"seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"seq":1}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"seq":2}
            {"at_ns":"<ns>","note":"the run failed: deleting sandbox sim-1 failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`","seq":3}
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
            provider.json: {"capabilities":{"startup_seconds":30},"report":{"provider":"sim"}}
            outcome.json: {"reason":"publishing what the run's sandboxes run failed: /sim/unpublished can't be published by the simulated provider: name another path","state":"failed"}
            log.jsonl:
            socket directories left: false
            their directory's mode at each create: []
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
