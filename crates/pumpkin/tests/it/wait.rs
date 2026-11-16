//! `wait`: what it prints and exits with for a record in each state.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::Stdio;

use expect_test::expect;
use schema::sandbox::RESULT_MAX_DEPTH;
use serde_json::{Value, json};

use super::{
    Prices, Scratch, Step, asked, check, cut, invocation, judged, judged_log, lease_places, logged,
    ran, report, write_record, write_run,
};

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn waits() {
    let scratch = Scratch::new("waits");
    let finished = |score: Value, status: Value| {
        let work_end = json!({ "kind": "agent_exited", "status": status });
        let verdict = json!({ "kind": "judged" });
        json!({ "state": "finished", "score": score, "verdict": verdict, "work_end": work_end })
    };
    let ended = |id, outcome| write_record(&scratch, id, true, Some(outcome));
    ended(
        "20261107-120000-1b8a6389",
        finished(json!(7.5), json!({ "code": 1 })),
    );
    ended(
        "20261107-120100-2c9b0001",
        finished(json!(null), json!({ "signal": 9 })),
    );
    ended(
        "20261107-120200-2c9b0002",
        json!({ "state": "failed", "reason": "r" }),
    );
    ended(
        "20261107-120300-3d0c0003",
        json!({ "state": "finished", "score": null, "verdict": { "kind": "graded" } }),
    );
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
        wait("1b8a"),
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
            pumpkin: run 20261107-120000-1b8a6389 finished with a score of 7.5, after the work phase ended when the agent exited with status 1; its record is <scratch>/runs/20261107-120000-1b8a6389
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            printed outcome.json: {"score":7.5,"state":"finished","verdict":{"kind":"judged"},"work_end":{"kind":"agent_exited","status":{"code":1}}}
            $ pumpkin wait 1b8a --runs runs
            exit status: Some(0)
            pumpkin: run 20261107-120000-1b8a6389 finished with a score of 7.5, after the work phase ended when the agent exited with status 1; its record is <scratch>/runs/20261107-120000-1b8a6389
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            printed outcome.json: {"score":7.5,"state":"finished","verdict":{"kind":"judged"},"work_end":{"kind":"agent_exited","status":{"code":1}}}
            $ pumpkin wait 20261107-1201 --runs runs
            exit status: Some(0)
            pumpkin: run 20261107-120100-2c9b0001 finished with a result, after the work phase ended when signal 9 killed the agent; its record is <scratch>/runs/20261107-120100-2c9b0001
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            printed outcome.json: {"score":null,"state":"finished","verdict":{"kind":"judged"},"work_end":{"kind":"agent_exited","status":{"signal":9}}}
            $ pumpkin wait runs/20261107-120200-2c9b0002
            exit status: Some(1)
            pumpkin: run 20261107-120200-2c9b0002 failed: r; its record is <scratch>/runs/20261107-120200-2c9b0002
            pumpkin: run 20261107-120200-2c9b0002 was made by pumpkin 0.0.0 (0f3a2b1c9 2026-01-02), and this is pumpkin <this>: to run it as it ran, run it with the pumpkin that made it
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            printed outcome.json: {"reason":"r","state":"failed"}
            $ pumpkin wait 2c9b --runs runs
            exit status: Some(1)
            pumpkin: `2c9b` matches 2 runs: name one by more of its id
              20261107-120100-2c9b0001
              20261107-120200-2c9b0002
            $ pumpkin wait 3d0c --runs runs
            exit status: Some(3)
            pumpkin: run 20261107-120300-3d0c0003 finished without a result: its verdict is of a kind this pumpkin doesn't know, after the work phase ended in a way this pumpkin doesn't know; its record is <scratch>/runs/20261107-120300-3d0c0003
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            printed outcome.json: {"score":null,"state":"finished","verdict":{"kind":"graded"}}
            $ pumpkin wait 20261107 --runs runs
            exit status: Some(1)
            pumpkin: `20261107` matches 4 runs: name one by more of its id
              20261107-120000-1b8a6389
              20261107-120100-2c9b0001
              20261107-120200-2c9b0002
              20261107-120300-3d0c0003
            $ pumpkin wait notes --runs runs
            exit status: Some(1)
            pumpkin: runs/notes has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
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
            pumpkin: run 20261107-120000-1b8a6389 finished with a score of 7.5, after the work phase ended when the agent exited with status 1; its record is <scratch>/runs/20261107-120000-1b8a6389
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            printed outcome.json: {"score":7.5,"state":"finished","verdict":{"kind":"judged"},"work_end":{"kind":"agent_exited","status":{"code":1}}}
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

#[test]
fn follows() {
    let scratch = Scratch::new("follows");
    let id = "20261107-120000-1b8a6389";
    write_record(&scratch, id, true, None);
    let driver = File::open(scratch.0.join(format!("runs/{id}/driver.pid"))).unwrap();
    driver.lock().unwrap();
    let mut command = scratch.pumpkin(&["wait", "1b8a", "--runs", "runs"]);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let invoked = invocation(&command);
    let mut child = command.spawn().unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut said = String::new();
    stderr.read_line(&mut said).unwrap();
    // After `wait`'s line that it follows the run, which comes before it blocks.
    let outcome = json!({ "state": "finished", "score": 7, "verdict": { "kind": "judged" } });
    scratch.write(&format!("runs/{id}/outcome.json"), &outcome);
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
            pumpkin: run 20261107-120000-1b8a6389 finished with a score of 7, after the work phase ended in a way this pumpkin doesn't know; its record is <scratch>/runs/20261107-120000-1b8a6389
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            printed outcome.json: {"score":7,"state":"finished","verdict":{"kind":"judged"}}"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn judge_stderr() {
    let scratch = Scratch::new("judge-stderr");
    let no_result = json!({
        "state": "finished",
        "score": null,
        "verdict": { "kind": "no_result", "failure": { "kind": "error", "reason": "r" } },
        "work_end": { "kind": "agent_exited", "status": { "code": 0 } },
    });
    // The final judge's job follows the agent's train job, whose standard error isn't shown.
    let shown = "20261108-120000-0000000a";
    write_record(&scratch, shown, true, Some(no_result.clone()));
    let start = |job: u64, grant: Value| {
        let start = json!({ "kind": "start_judge", "job": job, "grant": grant, "timeout_ns": "0" });
        json!({ "seq": job, "at_ns": "0", "actions": [start] })
    };
    let log = [start(0, json!("train")), start(1, Value::Null)];
    let lines: Vec<String> = log.iter().map(|line| format!("{line}\n")).collect();
    scratch.write(&format!("runs/{shown}/log.jsonl"), &lines.concat());
    scratch.write(
        &format!("runs/{shown}/jobs/0/log/command.stderr"),
        "train\n",
    );
    let stderr = b"Traceback:\n\tscore.py, line 1\n\x1b[31mno result\x1b[0m\r\n\xff\n";
    let path = scratch.0.join(format!("runs/{shown}/jobs/1/log"));
    fs::create_dir_all(&path).unwrap();
    fs::write(path.join("command.stderr"), stderr).unwrap();
    let final_job = start(0, Value::Null);
    let large = "20261108-120000-0000000b";
    write_record(&scratch, large, true, Some(no_result.clone()));
    scratch.write(
        &format!("runs/{large}/log.jsonl"),
        &format!("{final_job}\n"),
    );
    // One byte more than `pumpkin` shows.
    let stderr = "x".repeat((1 << 20) + 1);
    scratch.write(&format!("runs/{large}/jobs/0/log/command.stderr"), &stderr);
    let uncollected = "20261108-120000-0000000c";
    write_record(&scratch, uncollected, true, Some(no_result.clone()));
    scratch.write(
        &format!("runs/{uncollected}/log.jsonl"),
        &format!("{final_job}\n"),
    );
    let unlogged = "20261108-120000-0000000d";
    write_record(&scratch, unlogged, true, Some(no_result.clone()));
    let unjudged = "20261108-120000-0000000e";
    write_record(&scratch, unjudged, true, Some(no_result.clone()));
    scratch.write(
        &format!("runs/{unjudged}/log.jsonl"),
        &format!("{}\n", log[0]),
    );
    let ids = [shown, large, uncollected, unlogged, unjudged];
    let commands = ids.map(|id| scratch.pumpkin(&["wait", id, "--runs", "runs"]));
    check(
        &scratch,
        commands,
        false,
        &[],
        &expect![[r#"
            $ pumpkin wait 20261108-120000-0000000a --runs runs
            exit status: Some(3)
            pumpkin: the final judge's standard error, <scratch>/runs/20261108-120000-0000000a/jobs/1/log/command.stderr, holds:
              Traceback:
              	score.py, line 1
              \u{1b}[31mno result\u{1b}[0m\r
              �
            pumpkin: run 20261108-120000-0000000a finished without a result: the final judge gave none (error: r), after the work phase ended when the agent exited with status 0; its record is <scratch>/runs/20261108-120000-0000000a
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            printed outcome.json: {"score":null,"state":"finished","verdict":{"failure":{"kind":"error","reason":"r"},"kind":"no_result"},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            $ pumpkin wait 20261108-120000-0000000b --runs runs
            exit status: Some(3)
            pumpkin: the final judge's standard error, <scratch>/runs/20261108-120000-0000000b/jobs/0/log/command.stderr, holds 1048577 bytes, more than the 1048576 shown here: read it there
            pumpkin: run 20261108-120000-0000000b finished without a result: the final judge gave none (error: r), after the work phase ended when the agent exited with status 0; its record is <scratch>/runs/20261108-120000-0000000b
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            printed outcome.json: {"score":null,"state":"finished","verdict":{"failure":{"kind":"error","reason":"r"},"kind":"no_result"},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            $ pumpkin wait 20261108-120000-0000000c --runs runs
            exit status: Some(3)
            pumpkin: run 20261108-120000-0000000c finished without a result: the final judge gave none (error: r), after the work phase ended when the agent exited with status 0; its record is <scratch>/runs/20261108-120000-0000000c
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            printed outcome.json: {"score":null,"state":"finished","verdict":{"failure":{"kind":"error","reason":"r"},"kind":"no_result"},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            $ pumpkin wait 20261108-120000-0000000d --runs runs
            exit status: Some(3)
            pumpkin: the final judge's standard error can't be found: <scratch>/runs/20261108-120000-0000000d/log.jsonl doesn't exist: expected a run's log
            pumpkin: run 20261108-120000-0000000d finished without a result: the final judge gave none (error: r), after the work phase ended when the agent exited with status 0; its record is <scratch>/runs/20261108-120000-0000000d
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            printed outcome.json: {"score":null,"state":"finished","verdict":{"failure":{"kind":"error","reason":"r"},"kind":"no_result"},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            $ pumpkin wait 20261108-120000-0000000e --runs runs
            exit status: Some(3)
            pumpkin: the final judge's standard error can't be found: <scratch>/runs/20261108-120000-0000000e/log.jsonl names no final judge: expected a `start_judge` without a grant
            pumpkin: run 20261108-120000-0000000e finished without a result: the final judge gave none (error: r), after the work phase ended when the agent exited with status 0; its record is <scratch>/runs/20261108-120000-0000000e
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            printed outcome.json: {"score":null,"state":"finished","verdict":{"failure":{"kind":"error","reason":"r"},"kind":"no_result"},"work_end":{"kind":"agent_exited","status":{"code":0}}}"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn states() {
    let scratch = Scratch::new("states");
    let unscored = r#"{"state":"finished","score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"deadline"}}"#;
    let outcomes = [
        ("no outcome", None),
        (
            "scored",
            Some(r#"{"state":"finished","score":3,"verdict":{"kind":"judged"}}"#),
        ),
        ("unscored", Some(unscored)),
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
    let versions = [Some("v0"), Some("v9"), None];
    let (mut commands, mut shown, mut held) = (Vec::new(), Vec::new(), Vec::new());
    for (outcome, contents) in outcomes {
        for pid in pids {
            for version in versions {
                // A live record of this version, locked with no outcome, is `wait`'s to follow,
                // which `follows` covers; the matrix would block on it.
                if pid == "locked" && contents.is_none() && version == Some("v0") {
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
                    held.push(File::open(dir.join("driver.pid")).unwrap());
                    held.last().unwrap().lock().unwrap();
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
            $ pumpkin wait <no outcome, no driver.pid, v0> --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/<no outcome, no driver.pid, v0>/driver.pid: No such file or directory (os error 2)
            $ pumpkin wait <no outcome, no driver.pid, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<no outcome, no driver.pid, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <no outcome, no driver.pid, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<no outcome, no driver.pid, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <no outcome, free, v0> --runs runs
            exit status: Some(1)
            pumpkin: run <no outcome, free, v0> is lost: its driver ended without writing outcome.json, so nothing says how it ended; its record is <scratch>/runs/<no outcome, free, v0>
            pumpkin: to run it again: pumpkin run task.star
            pumpkin: to remove what it left: pumpkin cancel --runs <scratch>/runs <no outcome, free, v0>
            pumpkin: to remove what every run that has ended left: pumpkin reap --runs <scratch>/runs
            $ pumpkin wait <no outcome, free, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<no outcome, free, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <no outcome, free, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<no outcome, free, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <no outcome, locked, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<no outcome, locked, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <no outcome, locked, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<no outcome, locked, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <scored, no driver.pid, v0> --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/<scored, no driver.pid, v0>/driver.pid: No such file or directory (os error 2)
            $ pumpkin wait <scored, no driver.pid, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<scored, no driver.pid, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <scored, no driver.pid, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<scored, no driver.pid, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <scored, free, v0> --runs runs
            exit status: Some(0)
            pumpkin: run <scored, free, v0> finished with a score of 3, after the work phase ended in a way this pumpkin doesn't know; its record is <scratch>/runs/<scored, free, v0>
            pumpkin: to run it again: pumpkin run task.star
            printed outcome.json: {"score":3,"state":"finished","verdict":{"kind":"judged"}}
            $ pumpkin wait <scored, free, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<scored, free, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <scored, free, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<scored, free, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <scored, locked, v0> --runs runs
            exit status: Some(0)
            pumpkin: run <scored, locked, v0> finished with a score of 3, after the work phase ended in a way this pumpkin doesn't know; its record is <scratch>/runs/<scored, locked, v0>
            pumpkin: to run it again: pumpkin run task.star
            printed outcome.json: {"score":3,"state":"finished","verdict":{"kind":"judged"}}
            $ pumpkin wait <scored, locked, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<scored, locked, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <scored, locked, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<scored, locked, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <unscored, no driver.pid, v0> --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/<unscored, no driver.pid, v0>/driver.pid: No such file or directory (os error 2)
            $ pumpkin wait <unscored, no driver.pid, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unscored, no driver.pid, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <unscored, no driver.pid, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unscored, no driver.pid, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <unscored, free, v0> --runs runs
            exit status: Some(3)
            pumpkin: run <unscored, free, v0> finished without a result: the agent submitted nothing, and the work phase ended at its deadline; its record is <scratch>/runs/<unscored, free, v0>
            pumpkin: to run it again: pumpkin run task.star
            printed outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"deadline"}}
            $ pumpkin wait <unscored, free, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unscored, free, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <unscored, free, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unscored, free, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <unscored, locked, v0> --runs runs
            exit status: Some(3)
            pumpkin: run <unscored, locked, v0> finished without a result: the agent submitted nothing, and the work phase ended at its deadline; its record is <scratch>/runs/<unscored, locked, v0>
            pumpkin: to run it again: pumpkin run task.star
            printed outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"deadline"}}
            $ pumpkin wait <unscored, locked, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unscored, locked, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <unscored, locked, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unscored, locked, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <failed, no driver.pid, v0> --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/<failed, no driver.pid, v0>/driver.pid: No such file or directory (os error 2)
            $ pumpkin wait <failed, no driver.pid, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<failed, no driver.pid, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <failed, no driver.pid, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<failed, no driver.pid, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <failed, free, v0> --runs runs
            exit status: Some(1)
            pumpkin: run <failed, free, v0> failed: r; its record is <scratch>/runs/<failed, free, v0>
            pumpkin: to run it again: pumpkin run task.star
            printed outcome.json: {"reason":"r","state":"failed"}
            $ pumpkin wait <failed, free, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<failed, free, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <failed, free, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<failed, free, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <failed, locked, v0> --runs runs
            exit status: Some(1)
            pumpkin: run <failed, locked, v0> failed: r; its record is <scratch>/runs/<failed, locked, v0>
            pumpkin: to run it again: pumpkin run task.star
            printed outcome.json: {"reason":"r","state":"failed"}
            $ pumpkin wait <failed, locked, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<failed, locked, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <failed, locked, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<failed, locked, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <interrupted, no driver.pid, v0> --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/<interrupted, no driver.pid, v0>/driver.pid: No such file or directory (os error 2)
            $ pumpkin wait <interrupted, no driver.pid, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<interrupted, no driver.pid, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <interrupted, no driver.pid, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<interrupted, no driver.pid, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <interrupted, free, v0> --runs runs
            exit status: Some(1)
            pumpkin: run <interrupted, free, v0> was interrupted by SIGHUP; its record is <scratch>/runs/<interrupted, free, v0>
            pumpkin: to run it again: pumpkin run task.star
            printed outcome.json: {"signal":"SIGHUP","state":"interrupted"}
            $ pumpkin wait <interrupted, free, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<interrupted, free, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <interrupted, free, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<interrupted, free, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <interrupted, locked, v0> --runs runs
            exit status: Some(1)
            pumpkin: run <interrupted, locked, v0> was interrupted by SIGHUP; its record is <scratch>/runs/<interrupted, locked, v0>
            pumpkin: to run it again: pumpkin run task.star
            printed outcome.json: {"signal":"SIGHUP","state":"interrupted"}
            $ pumpkin wait <interrupted, locked, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<interrupted, locked, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <interrupted, locked, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<interrupted, locked, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <unknown signal, no driver.pid, v0> --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/<unknown signal, no driver.pid, v0>/driver.pid: No such file or directory (os error 2)
            $ pumpkin wait <unknown signal, no driver.pid, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown signal, no driver.pid, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <unknown signal, no driver.pid, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown signal, no driver.pid, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <unknown signal, free, v0> --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/<unknown signal, free, v0>/outcome.json's signal is not one pumpkin knows: expected `SIGINT`, `SIGTERM` or `SIGHUP`
            $ pumpkin wait <unknown signal, free, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown signal, free, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <unknown signal, free, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown signal, free, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <unknown signal, locked, v0> --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/<unknown signal, locked, v0>/outcome.json's signal is not one pumpkin knows: expected `SIGINT`, `SIGTERM` or `SIGHUP`
            $ pumpkin wait <unknown signal, locked, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown signal, locked, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <unknown signal, locked, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown signal, locked, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <unknown state, no driver.pid, v0> --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/<unknown state, no driver.pid, v0>/driver.pid: No such file or directory (os error 2)
            $ pumpkin wait <unknown state, no driver.pid, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown state, no driver.pid, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <unknown state, no driver.pid, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown state, no driver.pid, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <unknown state, free, v0> --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/<unknown state, free, v0>/outcome.json's state is not one pumpkin knows: expected `finished`, `failed` or `interrupted`
            $ pumpkin wait <unknown state, free, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown state, free, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <unknown state, free, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown state, free, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <unknown state, locked, v0> --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/<unknown state, locked, v0>/outcome.json's state is not one pumpkin knows: expected `finished`, `failed` or `interrupted`
            $ pumpkin wait <unknown state, locked, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown state, locked, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <unknown state, locked, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown state, locked, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <not JSON, no driver.pid, v0> --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/<not JSON, no driver.pid, v0>/driver.pid: No such file or directory (os error 2)
            $ pumpkin wait <not JSON, no driver.pid, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<not JSON, no driver.pid, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <not JSON, no driver.pid, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<not JSON, no driver.pid, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <not JSON, free, v0> --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/<not JSON, free, v0>/outcome.json: EOF while parsing an object at line 1 column 1
            $ pumpkin wait <not JSON, free, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<not JSON, free, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <not JSON, free, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<not JSON, free, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <not JSON, locked, v0> --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/<not JSON, locked, v0>/outcome.json: EOF while parsing an object at line 1 column 1
            $ pumpkin wait <not JSON, locked, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<not JSON, locked, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
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
    let scratch = Scratch::new("summarizes");
    // A run that went the whole way, priced but for its box's class, whose stopped judge call
    // left its standard error.
    let whole = "20261110-100000-0000000a";
    let outcome = judged(49.36, json!({ "raw_score": 49.36 }));
    let priced = lease_places(Prices::Both);
    write_run(
        &scratch,
        whole,
        asked(whole, 30),
        &judged_log(),
        Some(outcome),
        &priced,
    );
    scratch.write(
        &format!("runs/{whole}/jobs/2/log/command.stderr"),
        "Traceback\n",
    );
    // And one that ended at its deadline with nothing submitted, unpriced, whose route refused a
    // request of its agent's, and whose upstream answered others with a 200 whose stream states
    // a rate limit, one giving no code; its requests left behind more header names than the
    // summary holds.
    let late = "20261110-110000-0000000b";
    let answer = |more: Value| {
        let mut answer = json!({
            "kind": "model_answered", "caller": "agent",
            "answer": { "kind": "forwarded" }, "upstream_status": 200,
        });
        for (field, value) in more.as_object().unwrap() {
            answer[field] = value.clone();
        }
        answer
    };
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
    let mut log = vec![logged(5, json!({ "kind": "agent_started" }), json!([]))];
    for (request, (seconds, mut answer)) in (0_u64..).zip((10..).step_by(10).zip(answers)) {
        answer["request"] = json!(request);
        log.push(logged(seconds, answer, json!([])));
    }
    log.extend([
        logged(
            1805,
            json!({ "kind": "tick" }),
            json!([{ "kind": "tear_down_agent" }]),
        ),
        logged(
            1806,
            json!({ "kind": "tick" }),
            json!([{ "kind": "finish" }]),
        ),
    ]);
    let unscored = json!({
        "state": "finished", "score": null,
        "verdict": { "kind": "no_submission" }, "work_end": { "kind": "deadline" },
    });
    let unpriced = lease_places(Prices::Nothing);
    write_run(
        &scratch,
        late,
        asked(late, 90),
        &log,
        Some(unscored),
        &unpriced,
    );
    // And the first priced otherwise: its tokens alone, its sandboxes alone, and nothing.
    let others = [
        ("20261110-120000-0000000c", Prices::Tokens),
        ("20261110-120000-0000000d", Prices::Sandboxes),
        ("20261110-120000-0000000e", Prices::Nothing),
    ];
    for (id, prices) in others {
        let outcome = judged(49.36, json!({ "raw_score": 49.36 }));
        let places = lease_places(prices);
        write_run(
            &scratch,
            id,
            asked(id, 30),
            &judged_log(),
            Some(outcome),
            &places,
        );
    }
    // What it said, as these runs' times are their records'.
    let wait = |id: &str| {
        let mut wait = scratch.pumpkin(&["wait", "--runs", "runs", id]);
        let invoked = invocation(&wait);
        let output = wait.output().unwrap();
        let said = String::from_utf8(output.stderr).unwrap();
        let status = output.status.code();
        format!("{invoked}\nexit status: {status:?}\n{}", said.trim_end())
    };
    let mut steps: Vec<Step> = [whole, late].map(|id| Step::Ran(wait(id))).into();
    // Of those, the command and the lines that say a cost.
    for (id, _) in others {
        let said = wait(id);
        let mut lines = said.lines();
        let costs = lines
            .next()
            .into_iter()
            .chain(lines.filter(|line| line.contains("model:") || line.contains("cost:")));
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
                 0:00 agent on aws/judge, started at 0:05, exited 0 at 16:40; the work phase ended at 16:40
                 0:40 train judge job 0 on modal/a100_80 with unknown GPUs, started after 6s, ran 54s: 49.53
                 0:50   box lease 0 of job 0 on aws/box, 52s: charged 45s, 5s of it busy, 2 commands, the last `sh -c set -e\nprint 7` exited 0; closed as its call ended
                 2:00 gpu lease 1 on modal/a100_80 with NVIDIA A100-SXM4-80GB, 25s: charged 14s, 2s of it busy, 1 command, the last `nvidia-smi` exited 0; closed by its holder
                 2:00 cpu lease 2 on aws/judge, 14m45s: charged 14m10s, 30s of it busy, 1 command, the last `sleep 60` timed out; closed as the work phase ended
                 3:20 train judge job 1 on modal/a100_80, never started, ended after 1m: failed: the judge's sandbox didn't start: Modal refused the helper's `create`: ResourceExhaustedError: Insufficient capacity to create sandbox: fix what Modal reports
                 5:00 train judge job 2 on modal/a100_80 with NVIDIA A100-SXM4-80GB, started after 10s, ran 20s: failed: the call was stopped (stopped); its standard error jobs/2/log/command.stderr
                 6:40 train judge job 3 on modal/a100_80 with unknown GPUs, started after 5s, ran 10s: no score, returned {"exit_status":1,"stderr_tail":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx… (timed out)
                 8:20 gpu lease 3 on modal/a100_80 with NVIDIA A100 80GB PCIe (its role needs NVIDIA A100-SXM4-80GB), 1m10s: charged 1m, 1s of it busy, 1 command, the last `true` exited 0; expired
                16:46 final judge job 4 on modal/a100_80 with NVIDIA A100-SXM4-80GB, started after 4s, ran 1m30s: 49.36
                        model: 1 request, all answered 200; 1 failed upstream: rate_limit_exceeded; dropped request headers: x-stainless-lang, x-title; tokens 500 in, 50 out; model-b's tokens lack a price
              model: 4 requests, all answered 200; 1 failed upstream: rate_limit_exceeded; dropped request headers: x-stainless-lang, x-title; tokens 3500 in (2600 cached, 150 written to the cache), 350 out (40 reasoning), as 3 responses reported them; $0.66 at the models' prices, but for model-b's tokens, which lack a price, and 1 response's unreported tokens; of 3 good answers, by the median, 1.38s to the first token (of 2) and 5.00s to the end
              sandboxes: 1 on aws/box, 52s in all, leases charged 45s, 5s of it busy, 1 created, all deleted; 2 on aws/judge, 31m27s in all, leases charged 14m10s, 30s of it busy, $0.19 at $0.3672 an hour, 2 created, all deleted; 7 on modal/a100_80, 5m54s in all, leases charged 1m14s, 3s of it busy, $0.25 at $2.5 an hour, 6 created, all deleted
              cost: $1.10, $0.66 of it for tokens and $0.44 for sandboxes, and more without a price, and for 1 response's unreported tokens
            pumpkin: run 20261110-100000-0000000a finished with a score of 49.36, after the work phase ended when the agent exited with status 0; its record is <scratch>/runs/20261110-100000-0000000a
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            $ pumpkin wait --runs runs 20261110-110000-0000000b
            exit status: Some(3)
            pumpkin: run 20261110-110000-0000000b started 1m30s after it was asked for, and ran 30m06s:
                 0:00 agent on aws/judge, started at 0:05; the work phase ended at 30:05
              model: 5 requests, answered 4 200, 1 refused; 3 failed upstream: 1 with no code, 2 rate_limit_exceeded; dropped request headers: x-a, x-h00, x-h01, x-h02, x-h03, x-h04, x-h05, x-h06, x-h07, x-h08, x-h09, x-h10, x-h11, x-h12, x-h13, x-h14, x-h15, x-h16, x-h17, x-h18, x-h19, x-h20, x-h21, x-h22, x-h23, x-h24, x-h25, x-h26, x-h27, x-h28, x-h29, x-h30, and more; no response reported its tokens
              sandboxes: 1 on aws/judge, 30m06s in all, 1 created, all deleted
            pumpkin: run 20261110-110000-0000000b finished without a result: the agent submitted nothing, and the work phase ended at its deadline; its record is <scratch>/runs/20261110-110000-0000000b
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            $ pumpkin wait --runs runs 20261110-120000-0000000c
                        model: 1 request, all answered 200; 1 failed upstream: rate_limit_exceeded; dropped request headers: x-stainless-lang, x-title; tokens 500 in, 50 out; model-b's tokens lack a price
              model: 4 requests, all answered 200; 1 failed upstream: rate_limit_exceeded; dropped request headers: x-stainless-lang, x-title; tokens 3500 in (2600 cached, 150 written to the cache), 350 out (40 reasoning), as 3 responses reported them; $0.66 at the models' prices, but for model-b's tokens, which lack a price, and 1 response's unreported tokens; of 3 good answers, by the median, 1.38s to the first token (of 2) and 5.00s to the end
              cost: $0.66 for tokens, and more for sandboxes, which lack a price, and for 1 response's unreported tokens
            $ pumpkin wait --runs runs 20261110-120000-0000000d
                        model: 1 request, all answered 200; 1 failed upstream: rate_limit_exceeded; dropped request headers: x-stainless-lang, x-title; tokens 500 in, 50 out; model-b's tokens lack a price
              model: 4 requests, all answered 200; 1 failed upstream: rate_limit_exceeded; dropped request headers: x-stainless-lang, x-title; tokens 3500 in (2600 cached, 150 written to the cache), 350 out (40 reasoning), as 3 responses reported them; model-a's and model-b's tokens lack a price; of 3 good answers, by the median, 1.38s to the first token (of 2) and 5.00s to the end
              cost: $0.44 for sandboxes, and more for tokens, which lack a price
            $ pumpkin wait --runs runs 20261110-120000-0000000e
                        model: 1 request, all answered 200; 1 failed upstream: rate_limit_exceeded; dropped request headers: x-stainless-lang, x-title; tokens 500 in, 50 out
              model: 4 requests, all answered 200; 1 failed upstream: rate_limit_exceeded; dropped request headers: x-stainless-lang, x-title; tokens 3500 in (2600 cached, 150 written to the cache), 350 out (40 reasoning), as 3 responses reported them; of 3 good answers, by the median, 1.38s to the first token (of 2) and 5.00s to the end"#]],
    );
}

// `wait` of a live run says each milestone its log records, and once the part that doesn't read;
// the test ends the run once it has said the last, so `wait` reads the log again a poll later.
#[test]
fn follows_milestones() {
    let scratch = Scratch::new("follows-milestones");
    let id = "20261110-100000-0000000a";
    let priced = lease_places(Prices::Both);
    let mut log = judged_log();
    log.push(logged(
        1,
        json!({ "kind": "tick" }),
        json!([{ "kind": "close_lease" }]),
    ));
    let dir = write_run(&scratch, id, asked(id, 30), &log, None, &priced);
    let driver = File::open(dir.join("driver.pid")).unwrap();
    driver.lock().unwrap();
    let mut command = scratch.pumpkin(&["wait", id, "--runs", "runs"]);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let invoked = invocation(&command);
    let mut child = command.spawn().unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut said = String::new();
    while !said.contains("final judge job 4 ended") {
        if stderr.read_line(&mut said).unwrap() == 0 {
            break;
        }
    }
    let outcome = json!({ "state": "finished", "score": 49.36 });
    scratch.write(&format!("runs/{id}/outcome.json"), &outcome);
    drop(driver);
    stderr.read_to_string(&mut said).unwrap();
    child.wait().unwrap();
    // The summary, which follows, says the part too, on a line of its own.
    let said: Vec<&str> = (said.lines())
        .filter(|line| line.starts_with("pumpkin:  ") || line.contains("can't be read"))
        .collect();
    let step = Step::Ran(format!("{invoked}\n{}", said.join("\n")));
    check(
        &scratch,
        [step],
        false,
        &[],
        &expect![[r#"
        $ pumpkin wait 20261110-100000-0000000a --runs runs
        pumpkin: run 20261110-100000-0000000a: 1 part of its log can't be read by this pumpkin, the first at line 57, its action 1: missing field `lease`
        pumpkin:    0:05 the agent started on aws/judge
        pumpkin:    0:40 train judge job 0 asked on modal/a100_80
        pumpkin:    0:46 train judge job 0 started with unknown GPUs
        pumpkin:    0:50 box lease 0 of job 0 asked on aws/box
        pumpkin:    0:55 box lease 0 of job 0 ready
        pumpkin:    1:40 train judge job 0 ended: 49.53
        pumpkin:    1:42 box lease 0 of job 0 ended: closed as its call ended
        pumpkin:    2:00 gpu lease 1 asked on modal/a100_80
        pumpkin:    2:00 cpu lease 2 asked on aws/judge
        pumpkin:    2:06 gpu lease 1 ready with NVIDIA A100-SXM4-80GB
        pumpkin:    2:25 gpu lease 1 ended: closed by its holder
        pumpkin:    2:30 cpu lease 2 ready
        pumpkin:    3:20 train judge job 1 asked on modal/a100_80
        pumpkin:    4:20 train judge job 1 ended: failed: the judge's sandbox didn't start: Modal refused the helper's `create`: ResourceExhaustedError: Insufficient capacity to create sandbox: fix what Modal reports
        pumpkin:    5:00 train judge job 2 asked on modal/a100_80
        pumpkin:    5:10 train judge job 2 started with NVIDIA A100-SXM4-80GB
        pumpkin:    5:30 train judge job 2 ended: failed: the call was stopped
        pumpkin:    6:40 train judge job 3 asked on modal/a100_80
        pumpkin:    6:45 train judge job 3 started with unknown GPUs
        pumpkin:    6:55 train judge job 3 ended: no score
        pumpkin:    8:20 gpu lease 3 asked on modal/a100_80
        pumpkin:    8:25 gpu lease 3 ready with NVIDIA A100 80GB PCIe (its role needs NVIDIA A100-SXM4-80GB)
        pumpkin:    9:30 gpu lease 3 ended: expired
        pumpkin:   16:40 the work phase ended
        pumpkin:   16:45 cpu lease 2 ended: closed as the work phase ended
        pumpkin:   16:46 final judge job 4 asked on modal/a100_80
        pumpkin:   16:50 final judge job 4 started with NVIDIA A100-SXM4-80GB
        pumpkin:   18:20 final judge job 4 ended: 49.36
          1 part of its log can't be read by this pumpkin, the first at line 57, its action 1: missing field `lease`"#]],
    );
}

// A follower whose standard error has closed ends at the next line it says, though the run goes on.
#[test]
fn follower_without_its_client() {
    let scratch = Scratch::new("follower-without-its-client");
    let id = "20261110-100000-0000000b";
    let dir = write_run(
        &scratch,
        id,
        asked(id, 30),
        &[],
        None,
        &lease_places(Prices::Nothing),
    );
    fs::rename(scratch.0.join("runs"), scratch.0.join(".pumpkin/runs")).unwrap();
    let dir = scratch
        .0
        .join(".pumpkin/runs")
        .join(dir.file_name().unwrap());
    let driver = File::open(dir.join("driver.pid")).unwrap();
    driver.lock().unwrap();
    let mut command = scratch.pumpkin(&["controller", "follow", id]);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let invoked = invocation(&command);
    let mut child = command.spawn().unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut said = String::new();
    stderr.read_line(&mut said).unwrap();
    drop(stderr);
    let mut started = logged(5, json!({ "kind": "agent_started" }), json!([]));
    started["seq"] = json!(1);
    let mut log = fs::OpenOptions::new()
        .append(true)
        .open(dir.join("log.jsonl"))
        .unwrap();
    writeln!(log, "{started}").unwrap();
    let mut output = child.wait_with_output().unwrap();
    output.stderr = said.into_bytes();
    check(
        &scratch,
        [Step::Ran(report(invoked, &output, &scratch.0, false))],
        false,
        &[],
        &expect![[r#"
            $ pumpkin controller follow 20261110-100000-0000000b
            exit status: Some(1)
            pumpkin: following run 20261110-100000-0000000b: ending this command does not stop it"#]],
    );
}

#[test]
fn reads_the_deepest_result_and_a_log_cut_short() {
    let scratch = Scratch::new("reads-the-deepest-results");
    let mut steps = Vec::new();
    let (id, depth) = ("20261113-123335-0000000a", RESULT_MAX_DEPTH);
    let whole = (1..depth).fold(json!({ "raw_score": 6 }), |inner, _| json!({ "d": inner }));
    let returned = json!({ "raw_score": 6, "whole": whole });
    let result = json!({ "score": 6, "returned": returned });
    let collected = json!({ "kind": "judge_collected", "job": 0, "result": result });
    let finish = json!([{ "kind": "finish" }]);
    let final_judge = json!([{
        "kind": "start_judge", "job": 0, "grant": null, "timeout_ns": "60000000000",
    }]);
    let exit = json!({ "kind": "agent_exited", "status": { "code": 0 } });
    let log = [
        logged(0, json!({ "kind": "agent_started" }), json!([])),
        logged(1, exit, json!([{ "kind": "tear_down_agent" }])),
        logged(1, json!({ "kind": "tick" }), final_judge),
        logged(1, json!({ "kind": "judge_started", "job": 0 }), json!([])),
        logged(2, collected, finish),
    ];
    let outcome = Some(judged(6.0, returned));
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
    // What it said, without the outcome it printed, which holds the result.
    let mut wait = scratch.pumpkin(&["wait", "--runs", "runs", id]);
    let invoked = invocation(&wait);
    let output = wait.output().unwrap();
    let said = String::from_utf8(output.stderr).unwrap();
    let status = output.status.code();
    steps.push(Step::Ran(format!(
        "{invoked}\nexit status: {status:?}\n{}",
        said.trim_end()
    )));
    check(
        &scratch,
        steps,
        false,
        &[],
        &expect![[r#"
            $ pumpkin ls --runs runs 20261113-123335-0000000a
            exit status: Some(0)
            printed:
              20261113-123335-0000000a finished         2s  6               -  example/lease
              1 run: 1 finished
            $ pumpkin wait --runs runs 20261113-123335-0000000a
            exit status: Some(0)
            pumpkin: run 20261113-123335-0000000a started 0s after it was asked for, and ran 2s:
                 0:00 agent on aws/judge, started at 0:00, exited 0 at 0:01; the work phase ended at 0:01
                 0:01 final judge job 0 on modal/a100_80 with unknown GPUs, started after 0s, ran 1s: 6
              model: 0 requests
              sandboxes: 1 on aws/judge, 2s in all, 1 created, all deleted; 1 on modal/a100_80, 1s in all, 1 created, all deleted
            pumpkin: run 20261113-123335-0000000a finished with a score of 6, after the work phase ended when the agent exited with status 0; its record is <scratch>/runs/20261113-123335-0000000a
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star"#]],
    );
}

// A run that a signal stopped as its judge call's lease ran: its log ends with the stop, then
// the delete of each sandbox the core never ended, which the summary says.
#[test]
fn summarizes_an_interrupted_run() {
    let scratch = Scratch::new("summarizes-an-interrupted-run");
    let id = "20261113-123335-5ea1ea5f";
    let deleted = |seconds, sandbox: &str| {
        let event = json!({ "kind": "sandbox_deleted", "sandbox": sandbox });
        logged(seconds, event, json!([]))
    };
    // Up to the end of the lease's command.
    let mut log = cut(&judged_log(), 62);
    log.extend([
        json!({ "at_ns": "70000000000", "note": "the run was interrupted by SIGTERM" }),
        deleted(71, "lease-0"),
        deleted(72, "job-0"),
        deleted(80, "agent"),
    ]);
    let outcome = json!({ "signal": "SIGTERM", "state": "interrupted" });
    let priced = lease_places(Prices::Both);
    write_run(&scratch, id, asked(id, 0), &log, Some(outcome), &priced);
    let mut wait = scratch.pumpkin(&["wait", "--runs", "runs", id]);
    let invoked = invocation(&wait);
    let output = wait.output().unwrap();
    let said = String::from_utf8(output.stderr).unwrap();
    let status = output.status.code();
    let step = Step::Ran(format!(
        "{invoked}\nexit status: {status:?}\n{}",
        said.trim_end()
    ));
    check(
        &scratch,
        [step],
        false,
        &[],
        &expect![[r#"
            $ pumpkin wait --runs runs 20261113-123335-5ea1ea5f
            exit status: Some(1)
            pumpkin: run 20261113-123335-5ea1ea5f started 0s after it was asked for, and ran 1m20s:
                 0:00 agent on aws/judge, started at 0:05
                 0:40 train judge job 0 on modal/a100_80 with unknown GPUs, started after 6s, ran 26s: deleted as the run stopped
                 0:50   box lease 0 of job 0 on aws/box, 21s: charged 16s, 5s of it busy, 2 commands, the last `sh -c set -e\nprint 7` exited 0; deleted as the run stopped
              model: 3 requests, all answered 200; tokens 3000 in (2600 cached, 150 written to the cache), 300 out (40 reasoning), as 2 responses reported them; $0.66 at the models' prices, but for 1 response's unreported tokens; of 3 good answers, by the median, 1.38s to the first token (of 2) and 5.00s to the end
              sandboxes: 1 on aws/box, 21s in all, leases charged 16s, 5s of it busy, 1 created, all deleted; 1 on aws/judge, 1m20s in all, $0.01 at $0.3672 an hour, 1 created, all deleted; 1 on modal/a100_80, 32s in all, $0.02 at $2.5 an hour, 1 created, all deleted
              cost: $0.69, $0.66 of it for tokens and $0.03 for sandboxes, and more without a price, and for 1 response's unreported tokens
            pumpkin: run 20261113-123335-5ea1ea5f was interrupted by SIGTERM; its record is <scratch>/runs/20261113-123335-5ea1ea5f
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star"#]],
    );
}
