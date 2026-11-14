//! `wait`: what it prints and exits with for a record in each state.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::process::Stdio;

use expect_test::expect;
use serde_json::{Value, json};

use super::{Scratch, Step, check, invocation, report};

/// Writes the record `id` in `scratch`'s `runs`, as `pumpkin run` leaves it, with `driver.pid`
/// unless `pid` is false, and `outcome` as its `outcome.json`, if it has one.
fn write_record(scratch: &Scratch, id: &str, pid: bool, outcome: Option<Value>) {
    scratch.write(
        &format!("runs/{id}/record.json"),
        &json!({ "record_version": "pumpkin.record/v0" }),
    );
    if pid {
        scratch.write(&format!("runs/{id}/driver.pid"), "4242\n");
    }
    if let Some(outcome) = outcome {
        scratch.write(&format!("runs/{id}/outcome.json"), &outcome);
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn waits() {
    let scratch = Scratch::new("waits");
    let finished = |score: Value, status: Value| {
        let work_end = json!({ "kind": "agent_exited", "status": status });
        json!({ "state": "finished", "score": score, "work_end": work_end })
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
        json!({ "state": "failed", "reason": "f" }),
    );
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
        wait("20261107"),
        wait("notes"),
        scratch.pumpkin(&["wait", "runs/notes"]),
        scratch.pumpkin(&["wait", "nowhere/x"]),
        no_home,
        scratch.pumpkin(&["wait", "6a", "--runs", "nowhere"]),
        wait("6a"),
        wait(""),
    ];
    check(
        &scratch,
        commands,
        false,
        &[],
        &expect![[r#"
            $ pumpkin wait 20261107-120000-1b8a6389 --runs runs
            exit status: Some(0)
            pumpkin: run 20261107-120000-1b8a6389 finished with a score of 7.5, after the work phase ended when the agent exited with status 1; its record is <scratch>/runs/20261107-120000-1b8a6389
            printed outcome.json: {"score":7.5,"state":"finished","work_end":{"kind":"agent_exited","status":{"code":1}}}
            $ pumpkin wait 1b8a --runs runs
            exit status: Some(0)
            pumpkin: run 20261107-120000-1b8a6389 finished with a score of 7.5, after the work phase ended when the agent exited with status 1; its record is <scratch>/runs/20261107-120000-1b8a6389
            printed outcome.json: {"score":7.5,"state":"finished","work_end":{"kind":"agent_exited","status":{"code":1}}}
            $ pumpkin wait 20261107-1201 --runs runs
            exit status: Some(3)
            pumpkin: run 20261107-120100-2c9b0001 finished without a score: the verdict has no score, after the work phase ended when signal 9 killed the agent; its record is <scratch>/runs/20261107-120100-2c9b0001
            printed outcome.json: {"score":null,"state":"finished","work_end":{"kind":"agent_exited","status":{"signal":9}}}
            $ pumpkin wait runs/20261107-120200-2c9b0002
            exit status: Some(1)
            pumpkin: run 20261107-120200-2c9b0002 failed: r; its record is <scratch>/runs/20261107-120200-2c9b0002
            printed outcome.json: {"reason":"r","state":"failed"}
            $ pumpkin wait 2c9b --runs runs
            exit status: Some(1)
            pumpkin: `2c9b` matches 2 runs: name one by more of its id
              20261107-120100-2c9b0001
              20261107-120200-2c9b0002
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
            $ pumpkin wait nowhere/x
            exit status: Some(1)
            pumpkin: nowhere/x is not a directory: expected the directory of a run's record
            $ env --unset=HOME pumpkin wait runs/20261107-120000-1b8a6389
            exit status: Some(0)
            pumpkin: run 20261107-120000-1b8a6389 finished with a score of 7.5, after the work phase ended when the agent exited with status 1; its record is <scratch>/runs/20261107-120000-1b8a6389
            printed outcome.json: {"score":7.5,"state":"finished","work_end":{"kind":"agent_exited","status":{"code":1}}}
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
    let outcome = json!({ "state": "finished", "score": 7 });
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
            printed outcome.json: {"score":7,"state":"finished"}"#]],
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
    let log = [
        json!({ "seq": 0, "actions": [{ "kind": "start_judge", "job": 0, "grant": "train" }] }),
        json!({ "seq": 1, "actions": [{ "kind": "start_judge", "job": 1, "grant": null }] }),
    ];
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
    let final_job =
        json!({ "seq": 0, "actions": [{ "kind": "start_judge", "job": 0, "grant": null }] });
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
    // A judge's result nested past the depth to which serde_json reads a `Value`, in the event
    // and in a reply on the line that starts the final judge.
    let deep = "20261108-120000-0000000f";
    write_record(&scratch, deep, true, Some(no_result));
    let returned = format!(r#"{{"details":{}{}}}"#, "[".repeat(200), "]".repeat(200));
    let start = &log[1]["actions"][0];
    let collected = format!(
        r#"{{"seq":1,"event":{{"kind":"judge_collected","job":0,"result":{{"score":1,"returned":{returned}}}}},"actions":[{{"kind":"reply","op":0,"reply":{{"returned":{returned}}}}},{start}]}}"#
    );
    assert!(serde_json::from_str::<Value>(&collected).is_err());
    scratch.write(
        &format!("runs/{deep}/log.jsonl"),
        &format!("{}\n{collected}\n", log[0]),
    );
    scratch.write(&format!("runs/{deep}/jobs/1/log/command.stderr"), "found\n");
    let ids = [shown, large, uncollected, unlogged, unjudged, deep];
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
            pumpkin: run 20261108-120000-0000000a finished without a score: the final judge returned no result (error: r), after the work phase ended when the agent exited with status 0; its record is <scratch>/runs/20261108-120000-0000000a
            printed outcome.json: {"score":null,"state":"finished","verdict":{"failure":{"kind":"error","reason":"r"},"kind":"no_result"},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            $ pumpkin wait 20261108-120000-0000000b --runs runs
            exit status: Some(3)
            pumpkin: the final judge's standard error, <scratch>/runs/20261108-120000-0000000b/jobs/0/log/command.stderr, holds 1048577 bytes, more than the 1048576 shown here: read it there
            pumpkin: run 20261108-120000-0000000b finished without a score: the final judge returned no result (error: r), after the work phase ended when the agent exited with status 0; its record is <scratch>/runs/20261108-120000-0000000b
            printed outcome.json: {"score":null,"state":"finished","verdict":{"failure":{"kind":"error","reason":"r"},"kind":"no_result"},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            $ pumpkin wait 20261108-120000-0000000c --runs runs
            exit status: Some(3)
            pumpkin: run 20261108-120000-0000000c finished without a score: the final judge returned no result (error: r), after the work phase ended when the agent exited with status 0; its record is <scratch>/runs/20261108-120000-0000000c
            printed outcome.json: {"score":null,"state":"finished","verdict":{"failure":{"kind":"error","reason":"r"},"kind":"no_result"},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            $ pumpkin wait 20261108-120000-0000000d --runs runs
            exit status: Some(3)
            pumpkin: the final judge's standard error can't be found: <scratch>/runs/20261108-120000-0000000d/log.jsonl: No such file or directory (os error 2)
            pumpkin: run 20261108-120000-0000000d finished without a score: the final judge returned no result (error: r), after the work phase ended when the agent exited with status 0; its record is <scratch>/runs/20261108-120000-0000000d
            printed outcome.json: {"score":null,"state":"finished","verdict":{"failure":{"kind":"error","reason":"r"},"kind":"no_result"},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            $ pumpkin wait 20261108-120000-0000000e --runs runs
            exit status: Some(3)
            pumpkin: the final judge's standard error can't be found: <scratch>/runs/20261108-120000-0000000e/log.jsonl names no final judge: expected a `start_judge` without a grant
            pumpkin: run 20261108-120000-0000000e finished without a score: the final judge returned no result (error: r), after the work phase ended when the agent exited with status 0; its record is <scratch>/runs/20261108-120000-0000000e
            printed outcome.json: {"score":null,"state":"finished","verdict":{"failure":{"kind":"error","reason":"r"},"kind":"no_result"},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            $ pumpkin wait 20261108-120000-0000000f --runs runs
            exit status: Some(3)
            pumpkin: the final judge's standard error, <scratch>/runs/20261108-120000-0000000f/jobs/1/log/command.stderr, holds:
              found
            pumpkin: run 20261108-120000-0000000f finished without a score: the final judge returned no result (error: r), after the work phase ended when the agent exited with status 0; its record is <scratch>/runs/20261108-120000-0000000f
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
        ("scored", Some(r#"{"state":"finished","score":3}"#)),
        ("unscored", Some(unscored)),
        ("failed", Some(r#"{"state":"failed","reason":"r"}"#)),
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
                    let version = json!({ "record_version": format!("pumpkin.record/{version}") });
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
            pumpkin: <scratch>/runs/<no outcome, no driver.pid, v0> has no driver.pid: it was made before records were locked, which `wait` needs: name a record that this version of `pumpkin run` made
            $ pumpkin wait <no outcome, no driver.pid, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<no outcome, no driver.pid, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <no outcome, no driver.pid, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<no outcome, no driver.pid, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <no outcome, free, v0> --runs runs
            exit status: Some(1)
            pumpkin: run <no outcome, free, v0> is lost: its driver ended without writing outcome.json, so nothing says how it ended: remove what it left, which is its containers, labelled `pumpkin.run=<no outcome, free, v0>`, and its socket directory, `pumpkin-<no outcome, free, v0>` in the temporary directory; its record is <scratch>/runs/<no outcome, free, v0>
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
            pumpkin: <scratch>/runs/<scored, no driver.pid, v0> has no driver.pid: it was made before records were locked, which `wait` needs: name a record that this version of `pumpkin run` made
            $ pumpkin wait <scored, no driver.pid, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<scored, no driver.pid, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <scored, no driver.pid, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<scored, no driver.pid, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <scored, free, v0> --runs runs
            exit status: Some(0)
            pumpkin: run <scored, free, v0> finished with a score of 3, after the work phase ended in a way this pumpkin doesn't know; its record is <scratch>/runs/<scored, free, v0>
            printed outcome.json: {"score":3,"state":"finished"}
            $ pumpkin wait <scored, free, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<scored, free, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <scored, free, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<scored, free, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <scored, locked, v0> --runs runs
            exit status: Some(0)
            pumpkin: run <scored, locked, v0> finished with a score of 3, after the work phase ended in a way this pumpkin doesn't know; its record is <scratch>/runs/<scored, locked, v0>
            printed outcome.json: {"score":3,"state":"finished"}
            $ pumpkin wait <scored, locked, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<scored, locked, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <scored, locked, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<scored, locked, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <unscored, no driver.pid, v0> --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/<unscored, no driver.pid, v0> has no driver.pid: it was made before records were locked, which `wait` needs: name a record that this version of `pumpkin run` made
            $ pumpkin wait <unscored, no driver.pid, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unscored, no driver.pid, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <unscored, no driver.pid, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unscored, no driver.pid, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <unscored, free, v0> --runs runs
            exit status: Some(3)
            pumpkin: run <unscored, free, v0> finished without a score: the agent submitted nothing, and the work phase ended at its deadline; its record is <scratch>/runs/<unscored, free, v0>
            printed outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"deadline"}}
            $ pumpkin wait <unscored, free, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unscored, free, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <unscored, free, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unscored, free, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <unscored, locked, v0> --runs runs
            exit status: Some(3)
            pumpkin: run <unscored, locked, v0> finished without a score: the agent submitted nothing, and the work phase ended at its deadline; its record is <scratch>/runs/<unscored, locked, v0>
            printed outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"deadline"}}
            $ pumpkin wait <unscored, locked, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unscored, locked, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <unscored, locked, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unscored, locked, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <failed, no driver.pid, v0> --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/<failed, no driver.pid, v0> has no driver.pid: it was made before records were locked, which `wait` needs: name a record that this version of `pumpkin run` made
            $ pumpkin wait <failed, no driver.pid, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<failed, no driver.pid, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <failed, no driver.pid, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<failed, no driver.pid, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <failed, free, v0> --runs runs
            exit status: Some(1)
            pumpkin: run <failed, free, v0> failed: r; its record is <scratch>/runs/<failed, free, v0>
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
            printed outcome.json: {"reason":"r","state":"failed"}
            $ pumpkin wait <failed, locked, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<failed, locked, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <failed, locked, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<failed, locked, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <unknown state, no driver.pid, v0> --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/<unknown state, no driver.pid, v0> has no driver.pid: it was made before records were locked, which `wait` needs: name a record that this version of `pumpkin run` made
            $ pumpkin wait <unknown state, no driver.pid, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown state, no driver.pid, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <unknown state, no driver.pid, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown state, no driver.pid, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <unknown state, free, v0> --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/<unknown state, free, v0>/outcome.json's state is not one pumpkin knows: expected `finished` or `failed`
            $ pumpkin wait <unknown state, free, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown state, free, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <unknown state, free, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown state, free, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <unknown state, locked, v0> --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/<unknown state, locked, v0>/outcome.json's state is not one pumpkin knows: expected `finished` or `failed`
            $ pumpkin wait <unknown state, locked, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown state, locked, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <unknown state, locked, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown state, locked, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <not JSON, no driver.pid, v0> --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/<not JSON, no driver.pid, v0> has no driver.pid: it was made before records were locked, which `wait` needs: name a record that this version of `pumpkin run` made
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
