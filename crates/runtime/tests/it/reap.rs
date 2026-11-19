//! [`Record::reap`] of records whose sandboxes the simulated provider no longer lists, as it has
//! freed each once it ended.

use std::cell::RefCell;
use std::fs;

use expect_test::expect;
use provider::{Listed, RunTag, SandboxId, Sweep, Undeleted};
use runtime::{Reaped, Record};
use schema::ir::Name;
use serde_json::{Value, json};
use test_support::TestDir;

use crate::run::{PROVIDER, compact};
use crate::sim::Sim;

/// A lost run, whose agent's sandbox and a lease's ended by themselves.
const ENDED: &str = "20261108-121200-0a00000d";

/// A run that finished, of a build whose verdicts this one doesn't read, whose agent's sandbox
/// ended by itself.
const OLDER_ENDED: &str = "20261108-121300-0a00000f";

/// The simulated provider's sweep, which notes the ids that each delete names.
struct Noting {
    sim: Sim,
    deletes: RefCell<Vec<String>>,
}

impl Sweep for Noting {
    fn delete(&self, run: &RunTag, ids: &[SandboxId]) -> Result<(), Undeleted> {
        let named: Vec<&str> = ids.iter().map(SandboxId::as_str).collect();
        self.deletes.borrow_mut().push(named.join(", "));
        self.sim.delete(run, ids)
    }

    fn list(&self, run: &RunTag) -> Result<Vec<Listed>, String> {
        self.sim.list(run)
    }
}

// A sandbox that its provider lists no more may have left what only its delete removes.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the records and what each reap left of them"
)]
fn reaps_ended() {
    let runs = TestDir::new();
    let older = json!({ "state": "finished", "verdict": { "kind": "graded" } });
    let records = [
        (ENDED, None, &[("agent", "a"), ("leases/0", "b")][..]),
        (OLDER_ENDED, Some(older), &[("agent", "d")]),
    ];
    for (id, outcome, sandboxes) in records {
        let dir = runs.path().join(id);
        fs::create_dir_all(&dir).unwrap();
        let record = json!({ "record_version": "pumpkin.record/v0", "args": [] });
        fs::write(dir.join("record.json"), record.to_string()).unwrap();
        fs::write(dir.join("driver.pid"), "4242\n").unwrap();
        if let Some(outcome) = outcome {
            fs::write(dir.join("outcome.json"), outcome.to_string()).unwrap();
        }
        for (sandbox, id) in sandboxes {
            let held = json!({ "provider": PROVIDER, "id": id, "report": {} });
            fs::create_dir_all(dir.join(sandbox)).unwrap();
            fs::write(dir.join(sandbox).join("sandbox.json"), held.to_string()).unwrap();
        }
    }
    let name = Name::try_from(PROVIDER.to_owned()).unwrap();
    let noting = Noting {
        sim: Sim::new(),
        deletes: RefCell::new(Vec::new()),
    };
    let mut lines = Vec::new();
    for reap in ["first", "second"] {
        for id in [ENDED, OLDER_ENDED] {
            let record = Record::open(&runs.path().join(id)).unwrap();
            let Reaped {
                sandboxes,
                ended,
                sockets,
                lost,
                failed,
            } = record.reap(&[(&name, &noting)]).unwrap();
            assert!(
                sandboxes == [(name.clone(), Vec::new())] && sockets.is_none() && failed.is_none()
            );
            let ended: Vec<&str> = ended.iter().map(|(_, id)| id.as_str()).collect();
            let deletes = noting.deletes.take();
            lines.push(format!(
                "the {reap} reap of {id}: ended {ended:?}; lost: {lost}; deletes: {deletes:?}"
            ));
        }
    }
    for id in [ENDED, OLDER_ENDED] {
        let dir = runs.path().join(id);
        let reaps = fs::read_to_string(dir.join("reaps.jsonl")).unwrap();
        let reaps = reaps.lines().map(|line| {
            let mut line: Value = serde_json::from_str(line).unwrap();
            line["at_ns"] = "<at>".into();
            format!("\n  {line}")
        });
        let outcome = fs::read_to_string(dir.join("outcome.json")).unwrap();
        lines.push(format!(
            "{id}: outcome.json: {}; reaps.jsonl:{}",
            compact(&outcome),
            reaps.collect::<String>()
        ));
    }
    expect![[r#"
        the first reap of 20261108-121200-0a00000d: ended ["a", "b"]; lost: true; deletes: ["a, b"]
        the first reap of 20261108-121300-0a00000f: ended ["d"]; lost: false; deletes: ["d"]
        the second reap of 20261108-121200-0a00000d: ended []; lost: false; deletes: []
        the second reap of 20261108-121300-0a00000f: ended []; lost: false; deletes: []
        20261108-121200-0a00000d: outcome.json: {"reason":"its driver ended without an outcome","state":"failed"}; reaps.jsonl:
          {"at_ns":"<at>","id":"a","provider":"simulated","reaped":"removed"}
          {"at_ns":"<at>","id":"b","provider":"simulated","reaped":"removed"}
        20261108-121300-0a00000f: outcome.json: {"state":"finished","verdict":{"kind":"graded"}}; reaps.jsonl:
          {"at_ns":"<at>","id":"d","provider":"simulated","reaped":"removed"}"#]]
    .assert_eq(&lines.join("\n"));
}
