//! `wait`: what it prints and exits with for a record in each state.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::Stdio;

use expect_test::expect;
use schema::sandbox::RESULT_MAX_DEPTH;
use serde_json::{Value, json};

use super::{
    Prices, Scratch, Step, agent_answer, answered_log, asked, check, cut, invocation, lease_places,
    lease_task, lock_driver, logged, older_finished, payload, ran, report, returned, started_work,
    submission, whole_log, write_record, write_run,
};

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn waits() {
    let scratch = Scratch::new();
    let finished = |result: Value, status: Value| {
        let work_end = json!({ "kind": "agent_exited", "status": status });
        let verdict = json!({ "kind": "returned", "result": result });
        super::finished(&verdict, &work_end, &submission())
    };
    let ended = |id, outcome| write_record(&scratch, id, true, Some(outcome));
    ended(
        "20261107-120000-1b8a6389",
        finished(
            json!({ "values": [
                { "name": "score", "value": 7.5 },
                { "name": "status", "value": "passed" },
            ] }),
            json!({ "code": 1 }),
        ),
    );
    ended(
        "20261107-120100-2c9b0001",
        finished(json!({ "values": [] }), json!({ "signal": 9 })),
    );
    ended(
        "20261107-120200-2c9b0002",
        json!({ "state": "failed", "reason": "r" }),
    );
    ended("20261107-120300-3d0c0003", older_finished());
    // Made by another build.
    let made = scratch.0.join("runs/20261107-120200-2c9b0002/record.json");
    let mut record: Value = serde_json::from_slice(&fs::read(&made).unwrap()).unwrap();
    let commit = json!({ "hash": "0f3a2b1c9d8e", "date": "2026-01-02" });
    record["pumpkin"] = json!({ "version": "0.0.0", "commit": commit });
    fs::write(made, record.to_string()).unwrap();
    // Records whose build, or whose args, are not what a record keeps.
    for (id, field, value) in [
        ("20261108-120000-7b1a0007", "pumpkin", json!(7)),
        ("20261108-120100-8c2b0008", "args", json!("--runs /x")),
    ] {
        let outcome = json!({ "state": "failed", "reason": "r" });
        write_record(&scratch, id, true, Some(outcome));
        let path = scratch.0.join("runs").join(id).join("record.json");
        let mut record: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        record[field] = value;
        fs::write(path, record.to_string()).unwrap();
    }
    // A record in the making is not there yet, though it matches.
    write_record(&scratch, ".20261107-120600-2c9b0099.partial", true, None);
    scratch.write("runs/notes/todo", "");
    let mut no_home = scratch.pumpkin(&["wait", "runs/20261107-120000-1b8a6389"]);
    no_home.env_remove("HOME");
    let wait = |run| scratch.pumpkin(&["wait", run, "--runs", "runs"]);
    let commands = [
        wait("20261107-120000-1b8a6389"),
        scratch.pumpkin(&["wait", "--outcome", "1b8a", "--runs", "runs"]),
        wait("20261107-1201"),
        scratch.pumpkin(&["wait", "runs/20261107-120200-2c9b0002"]),
        wait("2c9b"),
        wait("3d0c"),
        wait("20261107"),
        wait("notes"),
        scratch.pumpkin(&["wait", "runs/notes"]),
        wait("7b1a"),
        wait("8c2b"),
        scratch.pumpkin(&["wait", "nowhere/x"]),
        no_home,
        scratch.pumpkin(&["wait", "6a", "--runs", "nowhere"]),
        wait("6a"),
        wait(""),
    ];
    let this = format!("this is pumpkin {}", runtime::BUILD);
    check(
        &scratch,
        commands,
        false,
        &[(&this, "this is pumpkin <this>")],
        &expect![[r#"
            $ pumpkin wait 20261107-120000-1b8a6389 --runs runs
            exit status: Some(0)
            pumpkin: run 20261107-120000-1b8a6389 finished with a result (score=7.5 status="passed"); its record is <scratch>/runs/20261107-120000-1b8a6389
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            $ pumpkin wait --outcome 1b8a --runs runs
            exit status: Some(0)
            pumpkin: run 20261107-120000-1b8a6389 finished with a result (score=7.5 status="passed"); its record is <scratch>/runs/20261107-120000-1b8a6389
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            printed outcome.json: {"agents":[{"role":"solver","submission":{"finalized":true,"payload":{"digest":"sha256:44778d82365e4af681c40d5f0eef5cf6f5899d3f0ac335050a7ed6779cf3f674","size":3}},"work_end":{"kind":"agent_exited","status":{"code":1}}}],"state":"finished","verdict":{"kind":"returned","result":{"values":[{"name":"score","value":7.5},{"name":"status","value":"passed"}]}}}
            $ pumpkin wait 20261107-1201 --runs runs
            exit status: Some(0)
            pumpkin: run 20261107-120100-2c9b0001 finished with a result; its record is <scratch>/runs/20261107-120100-2c9b0001
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            $ pumpkin wait runs/20261107-120200-2c9b0002
            exit status: Some(1)
            pumpkin: run 20261107-120200-2c9b0002 failed: r; its record is <scratch>/runs/20261107-120200-2c9b0002
            pumpkin: run 20261107-120200-2c9b0002 was made by pumpkin 0.0.0 (0f3a2b1c9 2026-01-02), and this is pumpkin <this>: to run it as it ran, run it with the pumpkin that made it
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            $ pumpkin wait 2c9b --runs runs
            exit status: Some(1)
            pumpkin: `2c9b` matches 2 runs: name one by more of its id
              20261107-120100-2c9b0001
              20261107-120200-2c9b0002
            $ pumpkin wait 3d0c --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/20261107-120300-3d0c0003/outcome.json doesn't read: unknown variant `graded`, expected `returned` or `no_result`
            $ pumpkin wait 20261107 --runs runs
            exit status: Some(1)
            pumpkin: `20261107` matches 4 runs: name one by more of its id
              20261107-120000-1b8a6389
              20261107-120100-2c9b0001
              20261107-120200-2c9b0002
              20261107-120300-3d0c0003
            $ pumpkin wait notes --runs runs
            exit status: Some(1)
            pumpkin: no run in runs matches `notes`: name an id, a unique prefix of an id or of its random digits, or a record's path
            $ pumpkin wait runs/notes
            exit status: Some(1)
            pumpkin: runs/notes has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait 7b1a --runs runs
            exit status: Some(1)
            pumpkin: runs/20261108-120000-7b1a0007/record.json holds pumpkin 7: expected the build that made it, {"version": v, "commit": c}
            $ pumpkin wait 8c2b --runs runs
            exit status: Some(1)
            pumpkin: runs/20261108-120100-8c2b0008/record.json holds args "--runs /x": expected an array of strings, the words that started the run
            $ pumpkin wait nowhere/x
            exit status: Some(1)
            pumpkin: nowhere/x is not a directory: expected the directory of a run's record
            $ env --unset=HOME pumpkin wait runs/20261107-120000-1b8a6389
            exit status: Some(0)
            pumpkin: run 20261107-120000-1b8a6389 finished with a result (score=7.5 status="passed"); its record is <scratch>/runs/20261107-120000-1b8a6389
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            $ pumpkin wait 6a --runs nowhere
            exit status: Some(1)
            pumpkin: nowhere: No such file or directory (os error 2): expected the directory that runs are recorded in, as `--runs` names it
            $ pumpkin wait 6a --runs runs
            exit status: Some(1)
            pumpkin: no run in runs matches `6a`: name an id, a unique prefix of an id or of its random digits, or a record's path
            $ pumpkin wait '' --runs runs
            exit status: Some(1)
            pumpkin: the run is empty: name an id, a prefix of one, or a record's path"#]],
    );
}

// `whole_log()` with the model requests of job 2, a stopped call of `train`, that spend the $0.10
// of `over_budget_task()`'s budget at $0.45, and then one that the budget bars.
fn over_budget() -> Vec<Value> {
    let mut log = whole_log();
    let started = |request: u64| json!({ "kind": "model_request_started", "caller": "job-2", "request": request });
    let spent = json!({
        "kind": "model_answered", "caller": "job-2", "request": 4, "model": "model-a",
        "api": "responses", "answer": { "kind": "forwarded" }, "upstream_status": 200,
        "usage": { "input_tokens": 1000, "output_tokens": 100 },
    });
    let message = "the model budget of the role's entry for `model-a`, $0.10 at the configured \
                   prices, is exhausted: make no more requests for its models";
    let barred = json!({
        "kind": "model_answered", "caller": "job-2", "request": 5, "api": "responses",
        "answer": { "kind": "barred", "bar": "budget", "status": 400, "message": message },
        "upstream_status": null,
    });
    let call_started = json!({ "kind": "call_started", "job": 2 });
    let after = 1
        + (log.iter())
            .position(|record| record["event"] == call_started)
            .unwrap();
    log.splice(
        after..after,
        [
            logged(312, started(4), json!([])),
            logged(314, spent, json!([])),
            logged(315, started(5), json!([])),
            logged(316, barred, json!([])),
        ],
    );
    for record in &mut log {
        let event = &mut record["event"];
        if event["kind"] == "call_failed" && event["job"] == 2 {
            event["model_requests_over_budget"] = json!(1);
        }
    }
    log
}

// `lease_task()` whose trainer has a model route for `model-a` alone, with a budget of $0.10.
fn over_budget_task() -> Value {
    let mut task = lease_task();
    task["roles"][1]["runtime"]["target"]["egress"] = json!({ "model": [
        { "api": "responses", "models": ["model-a"], "budget": { "microusd": 100_000 } },
    ] });
    task
}

// Writes run `id`'s `outcome.json` as its driver does, beside it then renamed over it: a `wait`
// that follows the run reads the file once it exists, and would read a plain write in part.
fn end_run(scratch: &Scratch, id: &str, outcome: &Value) {
    let partial = format!("runs/{id}/.outcome.json.partial");
    scratch.write(&partial, outcome);
    let outcome = scratch.0.join(format!("runs/{id}/outcome.json"));
    fs::rename(scratch.0.join(partial), outcome).unwrap();
}

#[test]
fn follows() {
    let scratch = Scratch::new();
    let id = "20261107-120000-1b8a6389";
    write_record(&scratch, id, true, None);
    let driver = lock_driver(&scratch.0.join(format!("runs/{id}")));
    let mut command = scratch.pumpkin(&["wait", "1b8a", "--runs", "runs"]);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let invoked = invocation(&command);
    let mut child = command.spawn().unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut said = String::new();
    stderr.read_line(&mut said).unwrap();
    // After `wait`'s line that it follows the run, which comes before it blocks.
    let outcome = returned(7.0);
    end_run(&scratch, id, &outcome);
    drop(driver);
    stderr.read_to_string(&mut said).unwrap();
    let mut output = child.wait_with_output().unwrap();
    output.stderr = said.into_bytes();
    let text = report(invoked, &output, &scratch.0, false);
    check(
        &scratch,
        [Step::Ran(text)],
        false,
        &[],
        &expect![[r#"
            $ pumpkin wait 1b8a --runs runs
            exit status: Some(0)
            pumpkin: following run 20261107-120000-1b8a6389, recorded in <scratch>/runs/20261107-120000-1b8a6389: ending `pumpkin wait` does not stop it
            pumpkin: run 20261107-120000-1b8a6389 finished with a result (score=7 metrics={"solved":3}); its record is <scratch>/runs/20261107-120000-1b8a6389
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn states() {
    let scratch = Scratch::new();
    let unsubmitted = super::unsubmitted().to_string();
    let returned = returned(7.0).to_string();
    let outcomes = [
        ("no outcome", None),
        ("returned", Some(returned.as_str())),
        ("unsubmitted", Some(unsubmitted.as_str())),
        ("failed", Some(r#"{"state":"failed","reason":"r"}"#)),
        (
            "interrupted",
            Some(r#"{"state":"interrupted","signal":"SIGHUP"}"#),
        ),
        (
            "unknown signal",
            Some(r#"{"state":"interrupted","signal":"SIGUSR1"}"#),
        ),
        ("unknown state", Some(r#"{"state":"paused"}"#)),
        ("not JSON", Some("{")),
    ];
    let pids = ["no driver.pid", "free", "locked"];
    let versions = [Some("v1"), Some("v9"), None];
    let (mut commands, mut shown, mut held) = (Vec::new(), Vec::new(), Vec::new());
    for (outcome, contents) in outcomes {
        for pid in pids {
            for version in versions {
                // A live record of this version, locked with no outcome, is `wait`'s to follow,
                // which `follows` covers; the matrix would block on it.
                if pid == "locked" && contents.is_none() && version == Some("v1") {
                    continue;
                }
                let id = format!("20261107-120000-{:08x}", shown.len());
                let dir = scratch.0.join("runs").join(&id);
                fs::create_dir_all(&dir).unwrap();
                if let Some(version) = version {
                    let version = format!("pumpkin.record/{version}");
                    let version = json!({ "record_version": version, "args": ["task.star"] });
                    fs::write(dir.join("record.json"), version.to_string()).unwrap();
                }
                if pid != "no driver.pid" {
                    fs::write(dir.join("driver.pid"), "4242\n").unwrap();
                }
                if pid == "locked" {
                    held.push(lock_driver(&dir));
                }
                if let Some(contents) = contents {
                    fs::write(dir.join("outcome.json"), contents).unwrap();
                }
                let version = version.unwrap_or("no record.json");
                let label = format!("<{outcome}, {pid}, {version}>");
                commands.push(scratch.pumpkin(&["wait", &id, "--runs", "runs"]));
                shown.push((id, label));
            }
        }
    }
    let shown: Vec<(&str, &str)> = (shown.iter())
        .map(|(id, label)| (&id[..], &label[..]))
        .collect();
    check(
        &scratch,
        commands,
        false,
        &shown,
        &expect![[r#"
            $ pumpkin wait <no outcome, no driver.pid, v1> --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/<no outcome, no driver.pid, v1>/driver.pid isn't there: expected the record of a run to have it
            $ pumpkin wait <no outcome, no driver.pid, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<no outcome, no driver.pid, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v1", which this `pumpkin` reads
            $ pumpkin wait <no outcome, no driver.pid, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<no outcome, no driver.pid, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <no outcome, free, v1> --runs runs
            exit status: Some(1)
            pumpkin: run <no outcome, free, v1> is lost: its driver ended without writing outcome.json, so nothing says how it ended; its record is <scratch>/runs/<no outcome, free, v1>
            pumpkin: to run it again: pumpkin run task.star
            pumpkin: to remove what it left: pumpkin cancel --runs <scratch>/runs <no outcome, free, v1>
            pumpkin: to remove what every run that has ended left: pumpkin reap --runs <scratch>/runs
            $ pumpkin wait <no outcome, free, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<no outcome, free, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v1", which this `pumpkin` reads
            $ pumpkin wait <no outcome, free, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<no outcome, free, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <no outcome, locked, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<no outcome, locked, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v1", which this `pumpkin` reads
            $ pumpkin wait <no outcome, locked, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<no outcome, locked, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <returned, no driver.pid, v1> --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/<returned, no driver.pid, v1>/driver.pid isn't there: expected the record of a run to have it
            $ pumpkin wait <returned, no driver.pid, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<returned, no driver.pid, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v1", which this `pumpkin` reads
            $ pumpkin wait <returned, no driver.pid, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<returned, no driver.pid, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <returned, free, v1> --runs runs
            exit status: Some(0)
            pumpkin: run <returned, free, v1> finished with a result (score=7 metrics={"solved":3}); its record is <scratch>/runs/<returned, free, v1>
            pumpkin: to run it again: pumpkin run task.star
            $ pumpkin wait <returned, free, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<returned, free, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v1", which this `pumpkin` reads
            $ pumpkin wait <returned, free, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<returned, free, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <returned, locked, v1> --runs runs
            exit status: Some(0)
            pumpkin: run <returned, locked, v1> finished with a result (score=7 metrics={"solved":3}); its record is <scratch>/runs/<returned, locked, v1>
            pumpkin: to run it again: pumpkin run task.star
            $ pumpkin wait <returned, locked, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<returned, locked, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v1", which this `pumpkin` reads
            $ pumpkin wait <returned, locked, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<returned, locked, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <unsubmitted, no driver.pid, v1> --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/<unsubmitted, no driver.pid, v1>/driver.pid isn't there: expected the record of a run to have it
            $ pumpkin wait <unsubmitted, no driver.pid, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unsubmitted, no driver.pid, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v1", which this `pumpkin` reads
            $ pumpkin wait <unsubmitted, no driver.pid, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unsubmitted, no driver.pid, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <unsubmitted, free, v1> --runs runs
            exit status: Some(3)
            pumpkin: run <unsubmitted, free, v1> finished without a result: the final call gave none (error: solver submitted nothing); its commands' logs can't be found: <scratch>/runs/<unsubmitted, free, v1>/log.jsonl doesn't exist: expected a run's log; its record is <scratch>/runs/<unsubmitted, free, v1>
            pumpkin: to run it again: pumpkin run task.star
            $ pumpkin wait <unsubmitted, free, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unsubmitted, free, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v1", which this `pumpkin` reads
            $ pumpkin wait <unsubmitted, free, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unsubmitted, free, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <unsubmitted, locked, v1> --runs runs
            exit status: Some(3)
            pumpkin: run <unsubmitted, locked, v1> finished without a result: the final call gave none (error: solver submitted nothing); its commands' logs can't be found: <scratch>/runs/<unsubmitted, locked, v1>/log.jsonl doesn't exist: expected a run's log; its record is <scratch>/runs/<unsubmitted, locked, v1>
            pumpkin: to run it again: pumpkin run task.star
            $ pumpkin wait <unsubmitted, locked, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unsubmitted, locked, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v1", which this `pumpkin` reads
            $ pumpkin wait <unsubmitted, locked, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unsubmitted, locked, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <failed, no driver.pid, v1> --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/<failed, no driver.pid, v1>/driver.pid isn't there: expected the record of a run to have it
            $ pumpkin wait <failed, no driver.pid, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<failed, no driver.pid, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v1", which this `pumpkin` reads
            $ pumpkin wait <failed, no driver.pid, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<failed, no driver.pid, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <failed, free, v1> --runs runs
            exit status: Some(1)
            pumpkin: run <failed, free, v1> failed: r; its record is <scratch>/runs/<failed, free, v1>
            pumpkin: to run it again: pumpkin run task.star
            $ pumpkin wait <failed, free, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<failed, free, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v1", which this `pumpkin` reads
            $ pumpkin wait <failed, free, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<failed, free, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <failed, locked, v1> --runs runs
            exit status: Some(1)
            pumpkin: run <failed, locked, v1> failed: r; its record is <scratch>/runs/<failed, locked, v1>
            pumpkin: to run it again: pumpkin run task.star
            $ pumpkin wait <failed, locked, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<failed, locked, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v1", which this `pumpkin` reads
            $ pumpkin wait <failed, locked, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<failed, locked, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <interrupted, no driver.pid, v1> --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/<interrupted, no driver.pid, v1>/driver.pid isn't there: expected the record of a run to have it
            $ pumpkin wait <interrupted, no driver.pid, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<interrupted, no driver.pid, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v1", which this `pumpkin` reads
            $ pumpkin wait <interrupted, no driver.pid, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<interrupted, no driver.pid, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <interrupted, free, v1> --runs runs
            exit status: Some(1)
            pumpkin: run <interrupted, free, v1> was interrupted by SIGHUP; its record is <scratch>/runs/<interrupted, free, v1>
            pumpkin: to run it again: pumpkin run task.star
            $ pumpkin wait <interrupted, free, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<interrupted, free, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v1", which this `pumpkin` reads
            $ pumpkin wait <interrupted, free, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<interrupted, free, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <interrupted, locked, v1> --runs runs
            exit status: Some(1)
            pumpkin: run <interrupted, locked, v1> was interrupted by SIGHUP; its record is <scratch>/runs/<interrupted, locked, v1>
            pumpkin: to run it again: pumpkin run task.star
            $ pumpkin wait <interrupted, locked, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<interrupted, locked, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v1", which this `pumpkin` reads
            $ pumpkin wait <interrupted, locked, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<interrupted, locked, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <unknown signal, no driver.pid, v1> --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/<unknown signal, no driver.pid, v1>/driver.pid isn't there: expected the record of a run to have it
            $ pumpkin wait <unknown signal, no driver.pid, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown signal, no driver.pid, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v1", which this `pumpkin` reads
            $ pumpkin wait <unknown signal, no driver.pid, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown signal, no driver.pid, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <unknown signal, free, v1> --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/<unknown signal, free, v1>/outcome.json doesn't read: unknown variant `SIGUSR1`, expected one of `SIGINT`, `SIGTERM`, `SIGHUP`
            $ pumpkin wait <unknown signal, free, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown signal, free, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v1", which this `pumpkin` reads
            $ pumpkin wait <unknown signal, free, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown signal, free, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <unknown signal, locked, v1> --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/<unknown signal, locked, v1>/outcome.json doesn't read: unknown variant `SIGUSR1`, expected one of `SIGINT`, `SIGTERM`, `SIGHUP`
            $ pumpkin wait <unknown signal, locked, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown signal, locked, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v1", which this `pumpkin` reads
            $ pumpkin wait <unknown signal, locked, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown signal, locked, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <unknown state, no driver.pid, v1> --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/<unknown state, no driver.pid, v1>/driver.pid isn't there: expected the record of a run to have it
            $ pumpkin wait <unknown state, no driver.pid, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown state, no driver.pid, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v1", which this `pumpkin` reads
            $ pumpkin wait <unknown state, no driver.pid, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown state, no driver.pid, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <unknown state, free, v1> --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/<unknown state, free, v1>/outcome.json doesn't read: unknown variant `paused`, expected one of `finished`, `failed`, `interrupted`
            $ pumpkin wait <unknown state, free, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown state, free, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v1", which this `pumpkin` reads
            $ pumpkin wait <unknown state, free, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown state, free, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <unknown state, locked, v1> --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/<unknown state, locked, v1>/outcome.json doesn't read: unknown variant `paused`, expected one of `finished`, `failed`, `interrupted`
            $ pumpkin wait <unknown state, locked, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown state, locked, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v1", which this `pumpkin` reads
            $ pumpkin wait <unknown state, locked, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown state, locked, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <not JSON, no driver.pid, v1> --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/<not JSON, no driver.pid, v1>/driver.pid isn't there: expected the record of a run to have it
            $ pumpkin wait <not JSON, no driver.pid, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<not JSON, no driver.pid, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v1", which this `pumpkin` reads
            $ pumpkin wait <not JSON, no driver.pid, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<not JSON, no driver.pid, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <not JSON, free, v1> --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/<not JSON, free, v1>/outcome.json: EOF while parsing an object at line 1 column 1
            $ pumpkin wait <not JSON, free, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<not JSON, free, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v1", which this `pumpkin` reads
            $ pumpkin wait <not JSON, free, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<not JSON, free, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <not JSON, locked, v1> --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/<not JSON, locked, v1>/outcome.json: EOF while parsing an object at line 1 column 1
            $ pumpkin wait <not JSON, locked, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<not JSON, locked, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v1", which this `pumpkin` reads
            $ pumpkin wait <not JSON, locked, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<not JSON, locked, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the steps and their snapshot"
)]
fn summarizes() {
    let scratch = Scratch::new();
    // A run that went the whole way, priced but for its box's class, whose train call was stopped,
    // with model requests over budget. It ran as a controller's job.
    let whole = "20261110-100000-0000000a";
    let mut outcome = returned(49.36);
    outcome["driver"] = json!({
        "cpu_ns": "41200000000", "children_cpu_ns": "3100000000", "peak_memory_bytes": 222_298_112,
        "job": { "unit": format!("pumpkin-job-{whole}.service"), "cpu_ns": "46000000000",
            "peak_memory_bytes": 723_517_440, "peak_tasks_count": 1204 },
    });
    let priced = lease_places(Prices::Both);
    write_run(
        &scratch,
        whole,
        asked(whole, 30),
        &over_budget(),
        Some(outcome),
        &priced,
    );
    // With a model budget for the agent, spent.
    let mut budgeted = over_budget_task();
    budgeted["roles"][0]["runtime"]["agent"]["egress"]["model"][0]["budget"] =
        json!({ "microusd": 500_000 });
    scratch.write(&format!("runs/{whole}/ir.json"), &budgeted);
    // And one that ended at its deadline with nothing submitted, unpriced, whose route refused a
    // request of its agent's, and whose upstream answered others with a 200 whose stream states
    // a rate limit, one giving no code; its requests left behind more header names than the
    // summary holds.
    let late = "20261110-110000-0000000b";
    let answer = |more: Value| agent_answer(200, more);
    let limited = || answer(json!({ "failed": { "code": "rate_limit_exceeded" } }));
    let refused = json!({
        "answer": { "kind": "refused", "status": 502, "message": "m" }, "upstream_status": null,
    });
    let many: Vec<String> = (0..32).map(|index| format!("x-h{index:02}")).collect();
    let mut again = limited();
    again["kept"] = json!({ "request": { "dropped_headers": ["x-a", "x-h00"] } });
    let answers = [
        answer(json!({ "kept": { "request": { "dropped_headers": many } } })),
        answer(refused),
        again,
        answer(json!({ "failed": {} })),
        limited(),
    ];
    let mut log = answered_log(answers);
    log.extend([
        logged(
            1805,
            json!({ "kind": "tick" }),
            json!([{ "kind": "tear_down_agent", "caller": "agent-0" }]),
        ),
        logged(
            1806,
            json!({ "kind": "tick" }),
            json!([{ "kind": "finish" }]),
        ),
    ]);
    // Its driver couldn't read what its children used.
    let mut unjudged = super::unsubmitted();
    unjudged["driver"] = json!({
        "cpu_ns": "850000000", "children_cpu_ns": null, "peak_memory_bytes": 13_002_342,
    });
    let unpriced = lease_places(Prices::Nothing);
    write_run(
        &scratch,
        late,
        asked(late, 90),
        &log,
        Some(unjudged),
        &unpriced,
    );
    // And the first priced otherwise: its tokens alone, its sandboxes alone, and nothing. The
    // first has a budget on its agent's entry for `model-a` through the Responses API, which
    // counts neither a priced answer for `model-b` nor one for `model-a` through Chat Completions.
    let others = [
        ("20261110-120000-0000000c", Prices::Tokens),
        ("20261110-120000-0000000d", Prices::Sandboxes),
        ("20261110-120000-0000000e", Prices::Nothing),
    ];
    let mut logs = others.map(|_| whole_log());
    let mut places = others.map(|(_, prices)| lease_places(prices));
    let usd_per_mtok =
        json!({ "input": 300.0, "cached": 30.0, "cache_write": 375.0, "output": 1500.0 });
    places[0]["routes"]["scorer"][0]["models"]["model-b"]["usd_per_mtok"] = usd_per_mtok;
    let outside = |request: u64, model: &str, api: &str| {
        let started =
            json!({ "kind": "model_request_started", "caller": "agent-0", "request": request });
        let answered = json!({
            "kind": "model_answered", "caller": "agent-0", "request": request, "model": model,
            "api": api, "answer": { "kind": "forwarded" }, "upstream_status": 200,
            "usage": { "input_tokens": 1000, "output_tokens": 100 },
        });
        [started, answered]
    };
    // Before the first train call, as the agent's.
    let at = (logs[0].iter())
        .position(|line| line["event"]["kind"] == "op_arrived")
        .unwrap();
    let [started_b, answered_b] = outside(9, "model-b", "responses");
    let [started_a, answered_a] = outside(10, "model-a", "chat");
    logs[0].splice(
        at..at,
        [
            logged(32, started_b, json!([])),
            logged(34, answered_b, json!([])),
            logged(35, started_a, json!([])),
            logged(37, answered_a, json!([])),
        ],
    );
    let call_answered = logs[0].iter().position(|record| {
        let event = &record["event"];
        event["kind"] == "model_answered" && event["caller"] == "job-4"
    });
    let started = json!({ "kind": "model_request_started", "caller": "job-4", "request": 4 });
    let abandoned = json!({
        "kind": "model_answered", "caller": "job-4", "request": 4, "model": "model-b",
        "api": "responses", "answer": { "kind": "abandoned" }, "upstream_status": null,
    });
    let after = call_answered.unwrap() + 1;
    logs[0].splice(
        after..after,
        [
            logged(1030, started, json!([])),
            logged(1099, abandoned, json!([])),
        ],
    );
    for (((id, _), log), places) in others.iter().zip(logs).zip(places) {
        let mut outcome = returned(49.36);
        // The last keeps what its driver used in a form that no build writes. Its summary says so,
        // rather than leave the figures out unsaid, or fail whole.
        if *id == others[2].0 {
            outcome["driver"] = json!({ "cpu_ns": 850_000_000 });
        }
        write_run(&scratch, id, asked(id, 30), &log, Some(outcome), &places);
    }
    let mut budgeted = lease_task();
    budgeted["roles"][0]["runtime"]["agent"]["egress"]["model"] = json!([
        { "api": "responses", "models": ["model-a"], "budget": { "microusd": 1_000_000 } },
        { "api": "responses", "models": ["model-b"] },
        { "api": "chat", "models": ["model-a"] },
    ]);
    scratch.write(&format!("runs/{}/ir.json", others[0].0), &budgeted);
    let wait = |id: &str| waited(&scratch, id);
    let mut steps: Vec<Step> = [whole, late].map(|id| Step::Ran(wait(id))).into();
    // Of those, the command and the lines that say a cost, a budget or what the driver used.
    for (id, _) in others {
        let said = wait(id);
        let mut lines = said.lines();
        let shown = |line: &&str| {
            ["model:", "budget ", "cost:", "driver"]
                .iter()
                .any(|word| line.contains(word))
        };
        let costs = lines.next().into_iter().chain(lines.filter(shown));
        steps.push(Step::Ran(costs.collect::<Vec<&str>>().join("\n")));
    }
    check(
        &scratch,
        steps,
        false,
        &[],
        &expect![[r#"
            $ pumpkin wait --runs runs 20261110-100000-0000000a
            exit status: Some(0)
            pumpkin: run 20261110-100000-0000000a started 30s after it was asked for, and ran 18m20s:
                 0:00 solver on aws/judge, started at 0:05, exited 0 at 16:40; its work ended at 16:40
                        model: 3 requests, all answered 200; tokens 3000 in (2600 cached, 150 written to the cache), 300 out (40 reasoning), as 2 responses reported them; $0.66 at the models' prices, but for 1 response's unreported tokens; of 3 good answers, by the median, 1.38s to the first token (of 2) and 5.00s to the end
                 0:40 train job 0 of agent 0 on modal/a100_80 with unknown GPUs, started after 6s, ran 54s: score=49.53
                 0:50   box lease 0 of job 0 on aws/box, 52s: charged 45s, 5s of it busy, 2 commands, the last `sh -c set -e\nprint 7` exited 0; closed as its call ended
                 2:00 gpu lease 1 of agent 0 on modal/a100_80 with NVIDIA A100-SXM4-80GB, 25s: charged 14s, 2s of it busy, 1 command, the last `nvidia-smi` exited 0; closed by its holder
                 2:00 cpu lease 2 of agent 0 on aws/judge, 14m45s: charged 14m10s, 30s of it busy, 1 command, the last `sleep 60` timed out; closed as the work phase ended
                 3:20 train job 1 of agent 0 on modal/a100_80, never started, ended after 1m: failed: the job's sandbox didn't start: Modal refused the helper's `create`: ResourceExhaustedError: Insufficient capacity to create sandbox: fix what Modal reports
                 5:00 train job 2 of agent 0 on modal/a100_80 with NVIDIA A100-SXM4-80GB, started after 10s, ran 20s: failed: the call was stopped (stopped); 1 model request barred by trainer's model budget
                        model: 2 requests, answered 1 200, 1 barred; tokens 1000 in, 100 out, as 1 response reported them; $0.45 at the models' prices; of 1 good answer, by the median, 2.00s to the end
                 6:40 train job 3 of agent 0 on modal/a100_80 with unknown GPUs, started after 5s, ran 10s: a result (timed out)
                 8:20 gpu lease 3 of agent 0 on modal/a100_80 with NVIDIA A100 80GB PCIe (its role needs NVIDIA A100-SXM4-80GB), 1m10s: charged 1m, 1s of it busy, 1 command, the last `true` exited 0; expired
                16:46 final call job 4 on modal/a100_80 with NVIDIA A100-SXM4-80GB, started after 4s, ran 1m30s: score=49.36 metrics={"solved":3}
                        model: 1 request, all answered 200; 1 failed upstream: rate_limit_exceeded; dropped request headers: x-stainless-lang, x-title; tokens 500 in, 50 out; model-b's tokens lack a price
              model: 6 requests, answered 5 200, 1 barred; 1 failed upstream: rate_limit_exceeded; dropped request headers: x-stainless-lang, x-title; tokens 4500 in (2600 cached, 150 written to the cache), 450 out (40 reasoning), as 4 responses reported them; $1.11 at the models' prices, but for model-b's tokens, which lack a price, and 1 response's unreported tokens; of 4 good answers, by the median, 1.38s to the first token (of 2) and 3.50s to the end
              budget solver (model-a, model-b): at least $0.66 of $0.50, exhausted
              budget trainer (model-a): $0.45 of $0.10, exhausted
              sandboxes: 1 on aws/box, 52s in all, leases charged 45s, 5s of it busy, 1 created, all deleted; 2 on aws/judge, 31m27s in all, leases charged 14m10s, 30s of it busy, $0.19 at $0.3672 an hour, 2 created, all deleted; 7 on modal/a100_80, 6m09s in all, leases charged 1m14s, 3s of it busy, $0.26 at $2.5 an hour, 6 created, all deleted
              driver: CPU 41.20s, children's CPU 3.10s, peak memory 212.0 MiB; job: CPU 46.00s, peak memory 690.0 MiB, peak tasks 1204
              cost: $1.56, $1.11 of it for tokens and $0.45 for sandboxes, and more without a price, and for 1 response's unreported tokens
            pumpkin: run 20261110-100000-0000000a finished with a result (score=49.36 metrics={"solved":3}); its record is <scratch>/runs/20261110-100000-0000000a
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            $ pumpkin wait --runs runs 20261110-110000-0000000b
            exit status: Some(3)
            pumpkin: run 20261110-110000-0000000b started 1m30s after it was asked for, and ran 30m06s:
                 0:00 solver on aws/judge, started at 0:05; its work ended at 30:05
                        model: 5 requests, answered 4 200, 1 refused; 3 failed upstream: 1 with no code, 2 rate_limit_exceeded; dropped request headers: x-a, x-h00, x-h01, x-h02, x-h03, x-h04, x-h05, x-h06, x-h07, x-h08, x-h09, x-h10, x-h11, x-h12, x-h13, x-h14, x-h15, x-h16, x-h17, x-h18, x-h19, x-h20, x-h21, x-h22, x-h23, x-h24, x-h25, x-h26, x-h27, x-h28, x-h29, x-h30, and more; no response reported its tokens
              model: 5 requests, answered 4 200, 1 refused; 3 failed upstream: 1 with no code, 2 rate_limit_exceeded; dropped request headers: x-a, x-h00, x-h01, x-h02, x-h03, x-h04, x-h05, x-h06, x-h07, x-h08, x-h09, x-h10, x-h11, x-h12, x-h13, x-h14, x-h15, x-h16, x-h17, x-h18, x-h19, x-h20, x-h21, x-h22, x-h23, x-h24, x-h25, x-h26, x-h27, x-h28, x-h29, x-h30, and more; no response reported its tokens
              sandboxes: 1 on aws/judge, 30m06s in all, 1 created, all deleted
              driver: CPU 0.85s, children's CPU unread, peak memory 12.3 MiB
            pumpkin: run 20261110-110000-0000000b finished without a result: the final call gave none (error: solver submitted nothing); its commands' logs can't be found: <scratch>/runs/20261110-110000-0000000b/log.jsonl names no final call: expected a `start_final_call`; its record is <scratch>/runs/20261110-110000-0000000b
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            $ pumpkin wait --runs runs 20261110-120000-0000000c
                        model: 5 requests, all answered 200; tokens 5000 in (2600 cached, 150 written to the cache), 500 out (40 reasoning), as 4 responses reported them; $1.56 at the models' prices, but for 1 response's unreported tokens; of 5 good answers, by the median, 1.38s to the first token (of 2) and 2.00s to the end
                        model: 2 requests, answered 1 200, 1 abandoned; 1 failed upstream: rate_limit_exceeded; dropped request headers: x-stainless-lang, x-title; tokens 500 in, 50 out, as 1 response reported them; $0.23 at the models' prices, but for 1 response's unreported tokens
              model: 7 requests, answered 6 200, 1 abandoned; 1 failed upstream: rate_limit_exceeded; dropped request headers: x-stainless-lang, x-title; tokens 5500 in (2600 cached, 150 written to the cache), 550 out (40 reasoning), as 5 responses reported them; $1.78 at the models' prices, but for 2 responses' unreported tokens; of 5 good answers, by the median, 1.38s to the first token (of 2) and 2.00s to the end
              budget solver (model-a): at least $0.66 of $1.00
              cost: $1.78 for tokens, and more for sandboxes, which lack a price, and for 2 responses' unreported tokens
            $ pumpkin wait --runs runs 20261110-120000-0000000d
                        model: 3 requests, all answered 200; tokens 3000 in (2600 cached, 150 written to the cache), 300 out (40 reasoning), as 2 responses reported them; model-a's tokens lack a price; of 3 good answers, by the median, 1.38s to the first token (of 2) and 5.00s to the end
                        model: 1 request, all answered 200; 1 failed upstream: rate_limit_exceeded; dropped request headers: x-stainless-lang, x-title; tokens 500 in, 50 out; model-b's tokens lack a price
              model: 4 requests, all answered 200; 1 failed upstream: rate_limit_exceeded; dropped request headers: x-stainless-lang, x-title; tokens 3500 in (2600 cached, 150 written to the cache), 350 out (40 reasoning), as 3 responses reported them; model-a's and model-b's tokens lack a price; of 3 good answers, by the median, 1.38s to the first token (of 2) and 5.00s to the end
              cost: $0.45 for sandboxes, and more for tokens, which lack a price
            $ pumpkin wait --runs runs 20261110-120000-0000000e
                        model: 3 requests, all answered 200; tokens 3000 in (2600 cached, 150 written to the cache), 300 out (40 reasoning), as 2 responses reported them; of 3 good answers, by the median, 1.38s to the first token (of 2) and 5.00s to the end
                        model: 1 request, all answered 200; 1 failed upstream: rate_limit_exceeded; dropped request headers: x-stainless-lang, x-title; tokens 500 in, 50 out
              model: 4 requests, all answered 200; 1 failed upstream: rate_limit_exceeded; dropped request headers: x-stainless-lang, x-title; tokens 3500 in (2600 cached, 150 written to the cache), 350 out (40 reasoning), as 3 responses reported them; of 3 good answers, by the median, 1.38s to the first token (of 2) and 5.00s to the end
              what its driver used can't be read: <scratch>/runs/20261110-120000-0000000e/outcome.json: invalid type: integer `850000000`, expected a string"#]],
    );
}

#[test]
fn says_sizes_in_binary_units() {
    let scratch = Scratch::new();
    // A driver's peak memory and its job's, at the edges of each binary unit, and 1,500,000 bytes,
    // which are 1.5 in decimal megabytes.
    let peaks = [
        (1023, 1024),
        (1_048_575, 1_048_576),
        (1_500_000, 1_073_741_823),
    ];
    let mut steps = Vec::new();
    for (index, (driver, job)) in peaks.into_iter().enumerate() {
        let id = format!("20261110-130000-0000000{index}");
        let mut outcome = returned(1.0);
        outcome["driver"] = json!({
            "cpu_ns": "1", "children_cpu_ns": null, "peak_memory_bytes": driver,
            "job": { "unit": "pumpkin-job.service", "cpu_ns": "1", "peak_memory_bytes": job,
                "peak_tasks_count": 1 },
        });
        let places = lease_places(Prices::Nothing);
        write_run(
            &scratch,
            &id,
            asked(&id, 30),
            &whole_log(),
            Some(outcome),
            &places,
        );
        let said = waited(&scratch, &id);
        let driver = said.lines().filter(|line| line.contains("driver:"));
        steps.push(Step::Ran(driver.map(str::trim_start).collect()));
    }
    check(
        &scratch,
        steps,
        false,
        &[],
        &expect![[r#"
            driver: CPU 0.00s, children's CPU unread, peak memory 1023 bytes; job: CPU 0.00s, peak memory 1.0 KiB, peak tasks 1
            driver: CPU 0.00s, children's CPU unread, peak memory 1023.9 KiB; job: CPU 0.00s, peak memory 1.0 MiB, peak tasks 1
            driver: CPU 0.00s, children's CPU unread, peak memory 1.4 MiB; job: CPU 0.00s, peak memory 1023.9 MiB, peak tasks 1"#]],
    );
}

/// Runs `wait` on run `id` of `scratch`'s `runs` and shows its invocation, its exit status and
/// what it said, without the outcome it printed.
///
/// Unlike [`ran`], it leaves each time as said, as the times of a record are fixed.
fn waited(scratch: &Scratch, id: &str) -> String {
    let mut wait = scratch.pumpkin(&["wait", "--runs", "runs", id]);
    let invoked = invocation(&wait);
    let output = wait.output().unwrap();
    let said = String::from_utf8(output.stderr).unwrap();
    let status = output.status.code();
    format!("{invoked}\nexit status: {status:?}\n{}", said.trim_end())
}

fn followed(scratch: &Scratch, id: &str, last: &str) -> Step {
    let driver = lock_driver(&scratch.0.join("runs").join(id));
    let mut command = scratch.pumpkin(&["wait", id, "--runs", "runs"]);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let invoked = invocation(&command);
    let mut child = command.spawn().unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut said = String::new();
    while !said.contains(last) {
        if stderr.read_line(&mut said).unwrap() == 0 {
            break;
        }
    }
    let outcome = returned(49.36);
    end_run(scratch, id, &outcome);
    drop(driver);
    stderr.read_to_string(&mut said).unwrap();
    child.wait().unwrap();
    // The summary, which follows, says the part too, on a line of its own.
    let said: Vec<&str> = (said.lines())
        .filter(|line| line.starts_with("pumpkin:  ") || line.contains("can't be read"))
        .collect();
    Step::Ran(format!("{invoked}\n{}", said.join("\n")))
}

#[test]
fn follows_an_agent_that_asks_at_startup() {
    let scratch = Scratch::new();
    let id = "20261110-100000-0000000a";
    let mut log = Vec::from(started_work(2));
    let call = json!({
        "kind": "start_call", "caller": "agent-0", "job": 0, "grant": "train", "payload": payload(), "timeout_ns": "600000000000",
    });
    let lease = json!({ "kind": "start_lease", "grant": "gpu", "holder": "agent-0", "lease": 0 });
    // The reader stamps both arrivals just before the loop reports startup across a second's edge.
    for (request, action, observed) in [
        (
            json!({ "kind": "call", "grant": "train" }),
            call,
            "1999950938",
        ),
        (
            json!({ "kind": "lease_open", "grant": "gpu" }),
            lease,
            "1999999999",
        ),
    ] {
        let mut line = logged(
            2,
            json!({ "kind": "op_arrived", "request": request }),
            json!([action]),
        );
        line["observed_ns"] = json!(observed);
        log.push(line);
    }
    write_run(
        &scratch,
        id,
        asked(id, 30),
        &log,
        None,
        &lease_places(Prices::Nothing),
    );
    let step = followed(&scratch, id, "gpu lease 0 of agent 0 asked");
    check(
        &scratch,
        [step],
        false,
        &[],
        &expect![[r#"
            $ pumpkin wait 20261110-100000-0000000a --runs runs
            pumpkin:    0:01 train job 0 of agent 0 asked on modal/a100_80
            pumpkin:    0:01 gpu lease 0 of agent 0 asked on modal/a100_80
            pumpkin:    0:02 solver started on aws/judge"#]],
    );
}

// `wait` of a live run says each milestone its log records, and once the part that doesn't read;
// the test ends the run once it has said the last, so `wait` reads the log again a poll later.
#[test]
fn follows_milestones() {
    let scratch = Scratch::new();
    let id = "20261110-100000-0000000a";
    let priced = lease_places(Prices::Both);
    let mut log = over_budget();
    log.push(logged(
        1,
        json!({ "kind": "tick" }),
        json!([{ "kind": "close_lease" }]),
    ));
    write_run(&scratch, id, asked(id, 30), &log, None, &priced);
    scratch.write(&format!("runs/{id}/ir.json"), &over_budget_task());
    let step = followed(&scratch, id, "final call job 4 ended");
    check(
        &scratch,
        [step],
        false,
        &[],
        &expect![[r#"
            $ pumpkin wait 20261110-100000-0000000a --runs runs
            pumpkin: run 20261110-100000-0000000a: 1 part of its log can't be read by this pumpkin, the first at line 74, its action 1: missing field `lease`
            pumpkin:    0:05 solver started on aws/judge
            pumpkin:    0:40 train job 0 of agent 0 asked on modal/a100_80
            pumpkin:    0:46 train job 0 of agent 0 started with unknown GPUs
            pumpkin:    0:50 box lease 0 of job 0 asked on aws/box
            pumpkin:    0:55 box lease 0 of job 0 ready
            pumpkin:    1:40 train job 0 of agent 0 ended: score=49.53
            pumpkin:    1:42 box lease 0 of job 0 ended: closed as its call ended
            pumpkin:    2:00 gpu lease 1 of agent 0 asked on modal/a100_80
            pumpkin:    2:00 cpu lease 2 of agent 0 asked on aws/judge
            pumpkin:    2:06 gpu lease 1 of agent 0 ready with NVIDIA A100-SXM4-80GB
            pumpkin:    2:25 gpu lease 1 of agent 0 ended: closed by its holder
            pumpkin:    2:30 cpu lease 2 of agent 0 ready
            pumpkin:    3:20 train job 1 of agent 0 asked on modal/a100_80
            pumpkin:    4:20 train job 1 of agent 0 ended: failed: the job's sandbox didn't start: Modal refused the helper's `create`: ResourceExhaustedError: Insufficient capacity to create sandbox: fix what Modal reports
            pumpkin:    5:00 train job 2 of agent 0 asked on modal/a100_80
            pumpkin:    5:10 train job 2 of agent 0 started with NVIDIA A100-SXM4-80GB
            pumpkin:    5:30 train job 2 of agent 0 ended: failed: the call was stopped; 1 model request barred by trainer's model budget
            pumpkin:    6:40 train job 3 of agent 0 asked on modal/a100_80
            pumpkin:    6:45 train job 3 of agent 0 started with unknown GPUs
            pumpkin:    6:55 train job 3 of agent 0 ended: a result
            pumpkin:    8:20 gpu lease 3 of agent 0 asked on modal/a100_80
            pumpkin:    8:25 gpu lease 3 of agent 0 ready with NVIDIA A100 80GB PCIe (its role needs NVIDIA A100-SXM4-80GB)
            pumpkin:    9:30 gpu lease 3 of agent 0 ended: expired
            pumpkin:   16:40 the work phase ended
            pumpkin:   16:45 cpu lease 2 of agent 0 ended: closed as the work phase ended
            pumpkin:   16:46 final call job 4 asked on modal/a100_80
            pumpkin:   16:50 final call job 4 started with NVIDIA A100-SXM4-80GB
            pumpkin:   18:20 final call job 4 ended: score=49.36 metrics={"solved":3}
              1 part of its log can't be read by this pumpkin, the first at line 74, its action 1: missing field `lease`"#]],
    );
}

// A run whose final call gave no result says where the call's commands' logs are, as its log
// names the call's job; or why it can't.
#[test]
#[expect(clippy::too_many_lines, reason = "most of it is the snapshot")]
fn final_call_logs() {
    let scratch = Scratch::new();
    let no_result = |failure: Value| {
        super::finished(
            &json!({"kind":"no_result","failure":failure}),
            &json!({"kind":"agent_exited","status":{"code":0}}),
            &submission(),
        )
    };
    let error = |reason: &str| json!({ "kind": "error", "reason": reason });
    let start = |job: u64, start: Value| json!({ "seq": job, "at_ns": "0", "actions": [start] });
    let call = json!({
        "kind": "start_call", "caller": "agent-0", "job": 0, "grant": "train", "payload": payload(), "timeout_ns": "0",
    });
    let final_call = json!({ "kind": "start_final_call", "job": 1, "selections": [{"role":"solver","selection":payload()}], "timeout_ns": "0" });
    let log = [start(0, call), start(1, final_call)];
    let lines: Vec<String> = log.iter().map(|line| format!("{line}\n")).collect();
    // A run whose final call, job 1, failed with `failure`, after running a command if `ran`.
    let logged = |id: &str, failure: Value, ran: bool| {
        write_record(&scratch, id, true, Some(no_result(failure)));
        scratch.write(&format!("runs/{id}/log.jsonl"), &lines.concat());
        if ran {
            scratch.write(&format!("runs/{id}/jobs/1/log/0/command.stdout"), "");
        }
    };
    let failed = "20261108-120000-0000000a";
    logged(failed, error("the body's command failed"), true);
    let timed_out = "20261108-120000-0000000d";
    logged(timed_out, json!({ "kind": "timeout" }), true);
    let unstarted = "20261108-120000-0000000e";
    logged(
        unstarted,
        error("the job's sandbox didn't start: no capacity"),
        false,
    );
    let early = "20261108-120000-0000000f";
    logged(
        early,
        error("final.star:2:16: getting /pumpkin/out/result.json failed"),
        false,
    );
    let unlogged = "20261108-120000-0000000b";
    write_record(&scratch, unlogged, true, Some(no_result(error("r"))));
    let uncalled = "20261108-120000-0000000c";
    write_record(&scratch, uncalled, true, Some(no_result(error("r"))));
    scratch.write(
        &format!("runs/{uncalled}/log.jsonl"),
        &format!("{}\n", log[0]),
    );
    let ids = [failed, timed_out, unstarted, early, unlogged, uncalled];
    let commands = ids.map(|id| scratch.pumpkin(&["wait", id, "--runs", "runs"]));
    check(
        &scratch,
        commands,
        false,
        &[],
        &expect![[r#"
            $ pumpkin wait 20261108-120000-0000000a --runs runs
            exit status: Some(3)
            pumpkin: run 20261108-120000-0000000a finished without a result: the final call gave none (error: the body's command failed); its commands' logs are in <scratch>/runs/20261108-120000-0000000a/jobs/1/log; its record is <scratch>/runs/20261108-120000-0000000a
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            $ pumpkin wait 20261108-120000-0000000d --runs runs
            exit status: Some(3)
            pumpkin: run 20261108-120000-0000000d finished without a result: the final call gave none (timeout); its commands' logs are in <scratch>/runs/20261108-120000-0000000d/jobs/1/log; its record is <scratch>/runs/20261108-120000-0000000d
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            $ pumpkin wait 20261108-120000-0000000e --runs runs
            exit status: Some(3)
            pumpkin: run 20261108-120000-0000000e finished without a result: the final call gave none (error: the job's sandbox didn't start: no capacity); it ran no command; its record is <scratch>/runs/20261108-120000-0000000e
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            $ pumpkin wait 20261108-120000-0000000f --runs runs
            exit status: Some(3)
            pumpkin: run 20261108-120000-0000000f finished without a result: the final call gave none (error: final.star:2:16: getting /pumpkin/out/result.json failed); it ran no command; its record is <scratch>/runs/20261108-120000-0000000f
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            $ pumpkin wait 20261108-120000-0000000b --runs runs
            exit status: Some(3)
            pumpkin: run 20261108-120000-0000000b finished without a result: the final call gave none (error: r); its commands' logs can't be found: <scratch>/runs/20261108-120000-0000000b/log.jsonl doesn't exist: expected a run's log; its record is <scratch>/runs/20261108-120000-0000000b
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            $ pumpkin wait 20261108-120000-0000000c --runs runs
            exit status: Some(3)
            pumpkin: run 20261108-120000-0000000c finished without a result: the final call gave none (error: r); its commands' logs can't be found: <scratch>/runs/20261108-120000-0000000c/log.jsonl names no final call: expected a `start_final_call`; its record is <scratch>/runs/20261108-120000-0000000c
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star"#]],
    );
}

#[test]
fn reads_a_deep_result_and_a_log_cut_short() {
    let scratch = Scratch::new();
    let mut steps = Vec::new();
    // A final call's return of the most depth that a body's return may have, its dict included.
    let (id, depth) = ("20261113-123335-0000000a", RESULT_MAX_DEPTH);
    let whole = (2..depth).fold(json!({ "score": 6 }), |inner, _| json!({ "d": inner }));
    let values = json!([{ "name": "whole", "value": whole }]);
    let call_returned = json!({ "kind": "call_returned", "job": 1, "returned": values });
    let finish = json!([{ "kind": "finish" }]);
    let final_call = json!([{
        "kind": "start_final_call", "job": 1, "selections": [{"role":"solver","selection":payload()}], "timeout_ns": "60000000000",
    }]);
    let mut log = Vec::from(started_work(0));
    log.extend(deep_call(depth));
    log.extend([
        super::agent_exited(1),
        logged(1, json!({ "kind": "tick" }), final_call),
        logged(1, json!({ "kind": "call_started", "job": 1 }), json!([])),
        logged(2, call_returned, finish),
    ]);
    let mut outcome = returned(6.0);
    outcome["verdict"]["result"] = json!({ "values": values });
    let outcome = Some(outcome);
    let dir = write_run(
        &scratch,
        id,
        asked(id, 0),
        &log,
        outcome,
        &lease_places(Prices::Nothing),
    );
    let mut log = File::options()
        .append(true)
        .open(dir.join("log.jsonl"))
        .unwrap();
    log.write_all(br#"{"at_ns":"14"#).unwrap();
    let ls = scratch.pumpkin(&["ls", "--runs", "runs", id]);
    steps.push(Step::Ran(ran(ls, &scratch.0, false)));
    steps.push(Step::Ran(waited(&scratch, id)));
    check(
        &scratch,
        steps,
        false,
        &[],
        &expect![[r#"
            $ pumpkin ls --runs runs 20261113-123335-0000000a
            exit status: Some(0)
            printed:
              ended
                20261113-123335-0000000a  example/lease  finished      2s  -  +1
              +n counts the values of a result left out: to see them: pumpkin wait --outcome --runs <scratch>/runs <run>

              1 run: 1 finished
            $ pumpkin wait --runs runs 20261113-123335-0000000a
            exit status: Some(0)
            pumpkin: run 20261113-123335-0000000a started 0s after it was asked for, and ran 2s:
                 0:00 solver on aws/judge, started at 0:00, exited 0 at 0:01; its work ended at 0:01
                        model: 0 requests
                 0:00 train job 0 of agent 0 on modal/a100_80 with unknown GPUs, started after 0s, ran 1s: deep=[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[]]]]]]]]]]]]]]]]…
                 0:01 final call job 1 on modal/a100_80 with unknown GPUs, started after 0s, ran 1s: whole={"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d"…
              model: 0 requests
              sandboxes: 1 on aws/judge, 2s in all, 1 created, all deleted; 2 on modal/a100_80, 2s in all, 2 created, all deleted
            pumpkin: run 20261113-123335-0000000a finished with a result (whole={"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d":{"d"…); its record is <scratch>/runs/20261113-123335-0000000a
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star"#]],
    );
}

/// The log of call 0 of `train`, from 0:00 to 0:01, whose body's return, its dict included, nests
/// `depth` deep.
fn deep_call(depth: usize) -> [Value; 5] {
    let deep = (2..depth).fold(json!([]), |inner, _| json!([inner]));
    let call = json!({ "kind": "op_arrived", "request": { "kind": "call", "grant": "train" } });
    let start = json!([{
        "kind": "start_call", "caller": "agent-0", "job": 0, "grant": "train", "payload": payload(),
        "timeout_ns": "60000000000",
    }]);
    let returned = json!({
        "kind": "call_returned", "job": 0, "returned": [{ "name": "deep", "value": deep }],
    });
    [
        logged(0, call, start),
        logged(0, json!({ "kind": "call_started", "job": 0 }), json!([])),
        logged(1, returned, json!([])),
        logged(
            1,
            json!({ "kind": "sandbox_deleted", "sandbox": "job-0" }),
            json!([]),
        ),
        logged(1, json!({ "kind": "call_torn_down", "job": 0 }), json!([])),
    ]
}

// A run that a signal stopped as its train call's lease ran: its log ends with the stop, then
// the delete of each sandbox the core never ended, which the summary says.
#[test]
fn summarizes_an_interrupted_run() {
    let scratch = Scratch::new();
    let id = "20261113-123335-5ea1ea5f";
    let deleted = |seconds, sandbox: &str| {
        let event = json!({ "kind": "sandbox_deleted", "sandbox": sandbox });
        logged(seconds, event, json!([]))
    };
    // Up to the end of the lease's command.
    let mut log = cut(&whole_log(), 62);
    log.extend([
        json!({ "at_ns": "70000000000", "note": "the run was interrupted by SIGTERM" }),
        deleted(71, "lease-0"),
        deleted(72, "job-0"),
        deleted(80, "agent-0"),
    ]);
    let outcome = json!({ "signal": "SIGTERM", "state": "interrupted" });
    let priced = lease_places(Prices::Both);
    write_run(&scratch, id, asked(id, 0), &log, Some(outcome), &priced);
    let step = Step::Ran(waited(&scratch, id));
    check(
        &scratch,
        [step],
        false,
        &[],
        &expect![[r#"
            $ pumpkin wait --runs runs 20261113-123335-5ea1ea5f
            exit status: Some(1)
            pumpkin: run 20261113-123335-5ea1ea5f started 0s after it was asked for, and ran 1m20s:
                 0:00 solver on aws/judge, started at 0:05
                        model: 3 requests, all answered 200; tokens 3000 in (2600 cached, 150 written to the cache), 300 out (40 reasoning), as 2 responses reported them; $0.66 at the models' prices, but for 1 response's unreported tokens; of 3 good answers, by the median, 1.38s to the first token (of 2) and 5.00s to the end
                 0:40 train job 0 of agent 0 on modal/a100_80 with unknown GPUs, started after 6s, ran 26s: deleted as the run stopped
                 0:50   box lease 0 of job 0 on aws/box, 21s: charged 16s, 5s of it busy, 2 commands, the last `sh -c set -e\nprint 7` exited 0; deleted as the run stopped
              model: 3 requests, all answered 200; tokens 3000 in (2600 cached, 150 written to the cache), 300 out (40 reasoning), as 2 responses reported them; $0.66 at the models' prices, but for 1 response's unreported tokens; of 3 good answers, by the median, 1.38s to the first token (of 2) and 5.00s to the end
              sandboxes: 1 on aws/box, 21s in all, leases charged 16s, 5s of it busy, 1 created, all deleted; 1 on aws/judge, 1m20s in all, $0.01 at $0.3672 an hour, 1 created, all deleted; 1 on modal/a100_80, 32s in all, $0.02 at $2.5 an hour, 1 created, all deleted
              cost: $0.69, $0.66 of it for tokens and $0.03 for sandboxes, and more without a price, and for 1 response's unreported tokens
            pumpkin: run 20261113-123335-5ea1ea5f was interrupted by SIGTERM; its record is <scratch>/runs/20261113-123335-5ea1ea5f
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star"#]],
    );
}

// `whole_log()` as builds before `call_torn_down` wrote it, which `pumpkin wait` reads as before.
#[test]
fn summarizes_a_record_in_the_old_order() {
    let scratch = Scratch::new();
    let id = "20261110-100000-0000000a";
    let log = in_the_old_order(&whole_log());
    let priced = lease_places(Prices::Both);
    write_run(
        &scratch,
        id,
        asked(id, 30),
        &log,
        Some(returned(49.36)),
        &priced,
    );
    let step = Step::Ran(waited(&scratch, id));
    check(
        &scratch,
        [step],
        false,
        &[],
        &expect![[r#"
        $ pumpkin wait --runs runs 20261110-100000-0000000a
        exit status: Some(0)
        pumpkin: run 20261110-100000-0000000a started 30s after it was asked for, and ran 18m20s:
             0:00 solver on aws/judge, started at 0:05, exited 0 at 16:40; its work ended at 16:40
                    model: 3 requests, all answered 200; tokens 3000 in (2600 cached, 150 written to the cache), 300 out (40 reasoning), as 2 responses reported them; $0.66 at the models' prices, but for 1 response's unreported tokens; of 3 good answers, by the median, 1.38s to the first token (of 2) and 5.00s to the end
             0:40 train job 0 of agent 0 on modal/a100_80 with unknown GPUs, started after 6s, ran 54s: score=49.53
             0:50   box lease 0 of job 0 on aws/box, 52s: charged 45s, 5s of it busy, 2 commands, the last `sh -c set -e\nprint 7` exited 0; closed as its call ended
             2:00 gpu lease 1 of agent 0 on modal/a100_80 with NVIDIA A100-SXM4-80GB, 25s: charged 14s, 2s of it busy, 1 command, the last `nvidia-smi` exited 0; closed by its holder
             2:00 cpu lease 2 of agent 0 on aws/judge, 14m45s: charged 14m10s, 30s of it busy, 1 command, the last `sleep 60` timed out; closed as the work phase ended
             3:20 train job 1 of agent 0 on modal/a100_80, never started, ended after 1m: failed: the job's sandbox didn't start: Modal refused the helper's `create`: ResourceExhaustedError: Insufficient capacity to create sandbox: fix what Modal reports
             5:00 train job 2 of agent 0 on modal/a100_80 with NVIDIA A100-SXM4-80GB, started after 10s, ran 20s: failed: the call was stopped (stopped)
             6:40 train job 3 of agent 0 on modal/a100_80 with unknown GPUs, started after 5s, ran 10s: a result (timed out)
             8:20 gpu lease 3 of agent 0 on modal/a100_80 with NVIDIA A100 80GB PCIe (its role needs NVIDIA A100-SXM4-80GB), 1m10s: charged 1m, 1s of it busy, 1 command, the last `true` exited 0; expired
            16:46 final call job 4 on modal/a100_80 with NVIDIA A100-SXM4-80GB, started after 4s, ran 1m30s: score=49.36 metrics={"solved":3}
                    model: 1 request, all answered 200; 1 failed upstream: rate_limit_exceeded; dropped request headers: x-stainless-lang, x-title; tokens 500 in, 50 out; model-b's tokens lack a price
          model: 4 requests, all answered 200; 1 failed upstream: rate_limit_exceeded; dropped request headers: x-stainless-lang, x-title; tokens 3500 in (2600 cached, 150 written to the cache), 350 out (40 reasoning), as 3 responses reported them; $0.66 at the models' prices, but for model-b's tokens, which lack a price, and 1 response's unreported tokens; of 3 good answers, by the median, 1.38s to the first token (of 2) and 5.00s to the end
          sandboxes: 1 on aws/box, 52s in all, leases charged 45s, 5s of it busy, 1 created, all deleted; 2 on aws/judge, 31m27s in all, leases charged 14m10s, 30s of it busy, $0.19 at $0.3672 an hour, 2 created, all deleted; 7 on modal/a100_80, 5m54s in all, leases charged 1m14s, 3s of it busy, $0.25 at $2.5 an hour, 6 created, all deleted
          cost: $1.10, $0.66 of it for tokens and $0.44 for sandboxes, and more without a price, and for 1 response's unreported tokens
        pumpkin: run 20261110-100000-0000000a finished with a result (score=49.36 metrics={"solved":3}); its record is <scratch>/runs/20261110-100000-0000000a
        pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star"#]],
    );
}

/// `whole`, a log, as builds before `call_torn_down` wrote it: each call's `sandbox_deleted` comes
/// just before its end, at its end's time, and the end carries its teardown's actions too.
fn in_the_old_order(whole: &[Value]) -> Vec<Value> {
    let job = |record: &Value, kind: &str| {
        (record["event"]["kind"] == kind).then(|| record["event"]["job"].clone())
    };
    let mut log: Vec<Value> = Vec::new();
    for record in whole {
        if job(record, "call_torn_down").is_some() {
            let actions = record["actions"].as_array().unwrap().clone();
            let end = log.iter_mut().rev().find(|end| {
                job(end, "call_returned").or_else(|| job(end, "call_failed"))
                    == job(record, "call_torn_down")
            });
            end.unwrap()["actions"]
                .as_array_mut()
                .unwrap()
                .extend(actions);
            continue;
        }
        let sandbox = record["event"]["sandbox"].as_str().unwrap_or_default();
        if record["event"]["kind"] == "sandbox_deleted" && sandbox.starts_with("job-") {
            continue;
        }
        let ended = job(record, "call_returned").or_else(|| job(record, "call_failed"));
        let deleted = ended.and_then(|ended| {
            let sandbox = format!("job-{ended}");
            whole.iter().find(|deleted| {
                deleted["event"]["kind"] == "sandbox_deleted"
                    && deleted["event"]["sandbox"] == sandbox
            })
        });
        if let Some(deleted) = deleted {
            let mut deleted = deleted.clone();
            deleted["at_ns"] = record["at_ns"].clone();
            log.push(deleted);
        }
        log.push(record.clone());
    }
    log
}
