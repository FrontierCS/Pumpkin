//! Executing a prepared run through the library's entry point, [`Run::execute`], on the
//! simulated provider: what it returns, its record, and its run log.

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]

use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use expect_test::{Expect, expect};
use pumpkin::driver::run::Run;
use pumpkin_schema::ir;
use serde_json::{Value, json};

use crate::run::{Runs, compact, files};
use crate::sim::Sim;

const LWE: &str = include_str!("../../../pumpkin-schema/tests/fixtures/lwe.json");

/// Prepares `ir` and executes it on `provider`, and checks what becomes of it: what `execute`
/// returns, the record's files, its `provider.json` and `outcome.json`, and its log, with each
/// record's time shown only if it is 0 or isn't a decimal string, and the run's start only if it
/// isn't within the call to `execute`.
fn check(provider: &Sim, ir: &Value, expect: &Expect) {
    let runs = Runs::new("execute");
    let ir: ir::Workflow = serde_json::from_value(ir.clone()).unwrap();
    let run = Run::prepare(&ir, &runs.dir, SystemTime::now()).unwrap();
    let record = run.dir().to_owned();
    let before = unix_ns();
    let executed = run.execute(provider);
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
    for line in fs::read_to_string(record.join("log.jsonl"))
        .unwrap()
        .lines()
    {
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
    expect.assert_eq(&lines.join("\n"));
}

/// A run whose agent doesn't start: the agent's failure ends the work phase without a
/// submission, so the run finishes without a score.
#[test]
fn unstarted() {
    let mut lwe: Value = serde_json::from_str(LWE).unwrap();
    lwe["roles"][0].as_object_mut().unwrap().remove("egress");
    check(
        &Sim::new(),
        &lwe,
        &expect![[r#"
            finished, with score None
            record: ir.json, log.jsonl, outcome.json, provider.json, record.json
            provider.json: {"capabilities":{"startup_seconds":30},"report":{"provider":"sim"}}
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"this runtime starts no sandbox yet"}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"kind":"run_started","unix_ns":"<during execute>"},"seq":0}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"this runtime starts no sandbox yet"}}}],"at_ns":"<ns>","event":{"kind":"agent_failed","reason":"this runtime starts no sandbox yet"},"seq":1}
            {"at_ns":"<ns>","note":"the agent's sandbox never started: nothing was collected","seq":2}"#]],
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
