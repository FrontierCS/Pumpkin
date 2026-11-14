//! `reap`: what it removes of the runs that have ended, or are lost, and what it leaves.

use std::fs::{self, File};
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::process::Stdio;

use expect_test::expect;
use serde_json::json;

use super::{
    INFO, Scratch, Step, check, invocation, poll, ran, report, write_gone, write_on, write_record,
};

/// A lost run, whose turn the test holds as the first reap begins.
const TURN: &str = "20261108-115900-0a000000";

/// A lost run, which left its socket directory.
const LOST: &str = "20261108-120000-0a000001";

/// A lost run, which left nothing else.
const BARE: &str = "20261108-120100-0a000002";

/// A run that finished, and left nothing.
const FINISHED: &str = "20261108-120200-0a000003";

/// A live run, whose lock the test holds.
const LIVE: &str = "20261108-120300-0a000004";

/// A record made before records were locked.
const UNLOCKED: &str = "20261108-120400-0a000005";

/// A run that failed on a Docker context that is gone since.
const GONE: &str = "20261108-120500-0a000006";

/// A lost record whose name is no run tag.
const BAD_NAME: &str = "lost name";

/// A run that failed on an engine that can't enforce every limit, and left a container there.
const UNFIT: &str = "20261108-120600-0a000007";

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the records, the steps and their snapshot"
)]
fn reaps() {
    let scratch = Scratch::new("reaps");
    write_record(&scratch, TURN, true, None);
    let turn = scratch.0.join(format!("runs/{TURN}"));
    let held = File::open(&turn).unwrap();
    held.lock().unwrap();
    write_record(&scratch, LOST, true, None);
    write_record(&scratch, BARE, true, None);
    let finished = json!({ "state": "finished", "score": 1 });
    write_record(&scratch, FINISHED, true, Some(finished));
    write_record(&scratch, LIVE, true, None);
    let live = File::open(scratch.0.join(format!("runs/{LIVE}/driver.pid"))).unwrap();
    live.lock().unwrap();
    write_record(&scratch, UNLOCKED, false, None);
    let failed = json!({ "state": "failed", "reason": "r" });
    let path = write_gone(&scratch, GONE, Some(failed));
    write_record(&scratch, BAD_NAME, true, None);
    scratch.write("runs/notes/todo", "");
    scratch.write(&format!("runs/{LOST}/sockets/agent/s"), "");
    let reap = |runs: &str| {
        let mut reap = scratch.pumpkin(&["reap", "--runs", runs]);
        reap.env("PATH", &path);
        ran(reap, &scratch.0, false)
    };
    let lost = scratch.0.join(format!("runs/{LOST}/outcome.json"));
    let mut first = scratch.pumpkin(&["reap", "--runs", "runs"]);
    first
        .env("PATH", &path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let invoked = invocation(&first);
    let child = first.spawn().unwrap();
    poll(|| Some(()).filter(|()| locks(child.id(), &turn) == [true]));
    let pid = locks(child.id(), &turn.join("driver.pid"));
    let waited = format!(
        "while the test holds {TURN}'s turn, reap waits for it, holding {} locks on its \
         driver.pid, and {LOST} has an outcome.json: {}",
        pid.len(),
        lost.exists()
    );
    drop(held);
    let output = child.wait_with_output().unwrap();
    let mut steps = vec![waited, report(invoked, &output, &scratch.0, false)];
    let left = |id| {
        let outcome = scratch.0.join(format!("runs/{id}/outcome.json"));
        let socket = scratch.0.join(format!("runs/{id}/sockets"));
        format!(
            "{id}: outcome.json: {}; socket directory: {}",
            fs::read_to_string(outcome)
                .unwrap_or_else(|_| "none".to_owned())
                .trim(),
            socket.exists()
        )
    };
    steps.extend([LOST, LIVE, BAD_NAME].map(left));
    let wait = scratch.pumpkin(&["wait", LOST, "--runs", "runs"]);
    steps.push(ran(wait, &scratch.0, false));
    for unreaped in [UNLOCKED, BAD_NAME] {
        fs::remove_dir_all(scratch.0.join(format!("runs/{unreaped}"))).unwrap();
    }
    steps.push(reap("runs"));
    fs::remove_dir_all(scratch.0.join(format!("runs/{GONE}"))).unwrap();
    steps.push(reap("runs"));
    steps.push(reap("nowhere"));
    drop(live);
    check(
        &scratch,
        steps.into_iter().map(Step::Ran),
        false,
        &[],
        &expect![[r#"
            while the test holds 20261108-115900-0a000000's turn, reap waits for it, holding 0 locks on its driver.pid, and 20261108-120000-0a000001 has an outcome.json: false
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin reap --runs runs
            exit status: Some(1)
            pumpkin: run 20261108-115900-0a000000 was lost, as its driver ended without an outcome.json: it is now recorded as failed
            pumpkin: run 20261108-120000-0a000001 left its socket directory, now removed: <scratch>/runs/20261108-120000-0a000001/sockets
            pumpkin: run 20261108-120000-0a000001 was lost, as its driver ended without an outcome.json: it is now recorded as failed
            pumpkin: run 20261108-120100-0a000002 was lost, as its driver ended without an outcome.json: it is now recorded as failed
            pumpkin: <scratch>/runs/20261108-120400-0a000005/driver.pid: No such file or directory (os error 2)
            pumpkin: run 20261108-120500-0a000006's sandboxes on provider `local`, if it left any, can't be removed, as it can't be reached: the Docker context `gone` can't be read: `docker context inspect` failed with exit status: 1: context "gone": context not found: name one that `docker context ls` lists; once it can be, `pumpkin reap --runs <scratch>/runs` removes them, and if it is gone for good, remove the record, <scratch>/runs/20261108-120500-0a000006, to stop reaping it
            pumpkin: run lost name can't be reaped: the record <scratch>/runs/lost name is named by no run tag (the run tag `lost name` holds ` `: use only ASCII letters, digits, `_`, `.` and `-`): expected a record that `pumpkin run` made
            pumpkin: 2 runs can't be reaped, as said above: fix what each says, and reap again
            pumpkin: to reap again: pumpkin reap --runs <scratch>/runs
            printed:
              20261108-115900-0a000000
              20261108-120000-0a000001
              20261108-120100-0a000002
            20261108-120000-0a000001: outcome.json: {
              "reason": "its driver ended without an outcome",
              "state": "failed"
            }; socket directory: false
            20261108-120300-0a000004: outcome.json: none; socket directory: false
            lost name: outcome.json: none; socket directory: false
            $ pumpkin wait 20261108-120000-0a000001 --runs runs
            exit status: Some(1)
            pumpkin: run 20261108-120000-0a000001 failed: its driver ended without an outcome; its record is <scratch>/runs/20261108-120000-0a000001
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            printed outcome.json: {"reason":"its driver ended without an outcome","state":"failed"}
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin reap --runs runs
            exit status: Some(3)
            pumpkin: run 20261108-120500-0a000006's sandboxes on provider `local`, if it left any, can't be removed, as it can't be reached: the Docker context `gone` can't be read: `docker context inspect` failed with exit status: 1: context "gone": context not found: name one that `docker context ls` lists; once it can be, `pumpkin reap --runs <scratch>/runs` removes them, and if it is gone for good, remove the record, <scratch>/runs/20261108-120500-0a000006, to stop reaping it
            pumpkin: 1 run ran on a provider that can't be reached, as said above: what each left there may remain
            pumpkin: to reap again once it can be reached: pumpkin reap --runs <scratch>/runs
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin reap --runs runs
            exit status: Some(0)
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin reap --runs nowhere
            exit status: Some(1)
            pumpkin: nowhere: No such file or directory (os error 2): expected the directory that runs are recorded in, as `--runs` names it"#]],
    );
}

#[test]
fn reaps_unfit() {
    let scratch = Scratch::new("reaps_unfit");
    let info = INFO.replace(r#""SwapLimit":true"#, r#""SwapLimit":false"#);
    let (calls, container) = (scratch.0.join("calls"), "c".repeat(64));
    let docker = format!(
        r#"#!/bin/sh
# `--context=<name>` comes first.
shift
echo "$*" >> "{}"
case $1 in
    context) echo unix:///stand-in.sock ;;
    info) echo '{info}' ;;
    container) echo "{container} exited {UNFIT}" ;;
    rm) ;;
    *) echo "the stand-in has no such command" >&2; exit 1 ;;
esac
"#,
        calls.display()
    );
    let failed = json!({ "state": "failed", "reason": "r" });
    let path = write_on(&scratch, UNFIT, "default", Some(failed), &docker);
    let mut reap = scratch.pumpkin(&["reap", "--runs", "runs"]);
    reap.env("PATH", &path);
    let reaped = ran(reap, &scratch.0, false);
    let called = fs::read_to_string(calls).unwrap();
    check(
        &scratch,
        [reaped, format!("docker was called with:\n{called}")].map(Step::Ran),
        false,
        &[],
        &expect![[r#"
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin reap --runs runs
            exit status: Some(0)
            pumpkin: run 20261108-120600-0a000007 left 1 sandbox on provider `local`, now removed: cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc
            printed:
              20261108-120600-0a000007
            docker was called with:
            context inspect --format {{.Endpoints.docker.Host}} -- default
            info --format {{json .}}
            container ls --all --no-trunc --format {{.ID}} {{.State}} {{.Label "pumpkin.run"}} --filter label=pumpkin.run --filter label=pumpkin.run=20261108-120600-0a000007
            container ls --all --no-trunc --format {{.ID}} {{.State}} {{.Label "pumpkin.run"}} --filter label=pumpkin.run --filter id=cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc
            rm --force --volumes cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc
        "#]],
    );
}

/// The locks that the process `pid` holds on `path`, or waits for, as `/proc/locks` lists them,
/// each by its process and its file's device and inode: for each, whether it waits, as one
/// listed after `->` does.
fn locks(pid: u32, path: &Path) -> Vec<bool> {
    let inode = format!(":{}", fs::metadata(path).unwrap().ino());
    let locks = fs::read_to_string("/proc/locks").unwrap();
    let found = locks.lines().filter_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let (waits, rest) = match fields[..] {
            [_, "->", ref rest @ ..] => (true, rest),
            [_, ref rest @ ..] => (false, rest),
            [] => return None,
        };
        let [_, _, _, holder, file, ..] = rest[..] else {
            return None;
        };
        (holder == pid.to_string() && file.ends_with(&inode)).then_some(waits)
    });
    found.collect()
}
