//! `reap`: what it removes of the runs that have ended, or are lost, and what it leaves.

use std::fs::{self, File};
use std::os::unix::fs::{MetadataExt as _, symlink};
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::Stdio;

use expect_test::expect;
use serde_json::json;
use stdx::process::Environment;

use super::{
    EC2, INFO, MODAL, MODAL_HELPER, PUMPKIN, Scratch, Step, check, docker_table, install,
    invocation, poll, ran, report, write_gone, write_on, write_record,
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

/// A lost run on `local`, of kind `docker`, `aws`, of kind `ec2`, and `gpus`, of kind `modal`,
/// which left a sandbox on each.
const SPREAD: &str = "20261108-120700-0a000008";

/// A lost run on `aws` and `gpus`, which left a sandbox on each, and which `cancel` reaps, whose
/// configuration names Modal's configuration file in its job's directory, gone with the job.
const CANCELLED: &str = "20261108-120800-0a000009";

/// A lost run on `aws`, whose configuration is gone.
const UNCONFIGURED: &str = "20261108-120900-0a00000a";

/// A lost run on `aws`, which the configuration that ran it no longer names.
const UNNAMED: &str = "20261108-121100-0a00000c";

/// A run that no record names, whose sandboxes on `aws` and `gpus` a reap leaves.
const OTHER: &str = "20261108-121000-0a00000b";

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the stand-ins' sandboxes, the steps and their snapshot"
)]
fn reaps_providers() {
    let scratch = Scratch::new("reaps_providers");
    let bin = scratch.0.join("bin");
    let installed = install(&scratch, ".pumpkin/installed", &MODAL_HELPER);
    let (calls, container) = (scratch.0.join("docker-calls"), "c".repeat(64));
    let docker = format!(
        r#"#!/bin/sh
# `--context=<name>` comes first.
shift
echo "$*" >> "{}"
case $1 in
    context) echo unix:///stand-in.sock ;;
    info) echo '{INFO}' ;;
    container) case "$*" in
        *label=pumpkin.run={SPREAD}*|*id={container}*) echo "{container} exited {SPREAD}" ;;
    esac ;;
    rm) ;;
    *) echo "the stand-in has no such command" >&2; exit 1 ;;
esac
"#,
        calls.display()
    );
    let path = write_on(&scratch, SPREAD, "default", None, &docker);
    for (command, stand_in) in [("aws", test_support::aws()), ("uv", test_support::uv())] {
        symlink(stand_in, bin.join(command)).unwrap();
    }
    let config = scratch.0.join("pumpkin.toml");
    scratch.write(
        "pumpkin.toml",
        &format!("{}\n{EC2}\n{MODAL}", docker_table("default")),
    );
    let gone = scratch.0.join("job/modal.toml");
    let modal = MODAL.replace(
        "[providers.gpus.modal]\n",
        &format!("[providers.gpus.modal]\nconfig = \"{}\"\n", gone.display()),
    );
    let job = scratch.0.join("job.toml");
    scratch.write(
        "job.toml",
        &format!("{}\n{EC2}\n{modal}", docker_table("default")),
    );
    scratch.write("sent/modal.toml", "[pumpkin]\ntoken_id = \"ak-x\"\n");
    let sent = scratch.0.join("sent/modal.toml");
    scratch.write(
        "bare.toml",
        &format!(
            "{}\n{}",
            docker_table("default"),
            "[providers.local.docker]\n"
        ),
    );
    // Each run's record, which names the configuration that ran it, and what each of its
    // providers reported, of which a reap reads only a Docker engine's context.
    let records = [
        (SPREAD, &["local", "aws", "gpus"][..], config.clone()),
        (CANCELLED, &["aws", "gpus"], job),
        (UNCONFIGURED, &["aws"], scratch.0.join("gone.toml")),
        (UNNAMED, &["aws"], scratch.0.join("bare.toml")),
    ];
    for (id, providers, config) in records {
        write_record(&scratch, id, true, None);
        let args = [
            "--runs".to_owned(),
            scratch.0.join("runs").display().to_string(),
            "--config".to_owned(),
            config.display().to_string(),
        ];
        let record = json!({ "record_version": "pumpkin.record/v0", "args": args });
        scratch.write(&format!("runs/{id}/record.json"), &record);
        let reports = providers.iter().map(|&name| {
            let report = match name {
                "local" => json!({ "context": "default" }),
                _ => json!({ "region": "us-west-1" }),
            };
            (name.to_owned(), json!({ "report": report }))
        });
        let provided = json!({ "providers": reports.collect::<serde_json::Map<_, _>>() });
        scratch.write(&format!("runs/{id}/provider.json"), &provided);
    }
    // The sandboxes that each record holds: on each provider, one the provider lists, which the
    // reap removes; one on `aws` that its list misses, as one in another region would be; and
    // one on `gpus` that the run deleted.
    let held = [
        (SPREAD, "agent", "local", "c".repeat(64), false),
        (
            SPREAD,
            "jobs/0",
            "aws",
            "i-00000000000000038".to_owned(),
            false,
        ),
        (SPREAD, "jobs/1", "gpus", "sb-8".to_owned(), false),
        (
            SPREAD,
            "jobs/2",
            "aws",
            "i-0000000000000000f".to_owned(),
            false,
        ),
        (SPREAD, "jobs/3", "gpus", "sb-7".to_owned(), true),
        (
            CANCELLED,
            "jobs/0",
            "aws",
            "i-00000000000000039".to_owned(),
            false,
        ),
    ];
    for (run, dir, provider, id, deleted) in held {
        let mut sandbox = json!({ "provider": provider, "id": id, "report": {} });
        if deleted {
            sandbox["delete"] = json!({ "state": "deleted" });
        }
        scratch.write(&format!("runs/{run}/{dir}/sandbox.json"), &sandbox);
    }
    // An instance of each run's on the stand-in `aws`, running.
    let instances = [SPREAD, CANCELLED, OTHER].map(|run| {
        let id = format!("i-{:017x}", run.as_bytes()[run.len() - 1]);
        scratch.write(&format!("bin/instances/{id}/state"), "running\n");
        scratch.write(&format!("bin/instances/{id}/run"), run);
        id
    });
    // A sandbox of each run's in the stand-in for Modal's SDK, in Pumpkin's App, each a process
    // of the test's, in a process group of its own, which a delete kills.
    let sandboxes = [SPREAD, CANCELLED, OTHER].map(|run| {
        let id = format!("sb-{}", &run[run.len() - 1..]);
        let process = stdx::process::command("cat", Environment::Inherited)
            .stdin(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        scratch.write("bin/apps/pumpkin-images", "");
        let dir = format!("bin/sandboxes/{id}");
        scratch.write(&format!("{dir}/app"), "pumpkin-images");
        let tags = json!({ "pumpkin-run": run, "pumpkin-role": "train_judge" });
        scratch.write(&format!("{dir}/tags.json"), &tags);
        scratch.write(&format!("{dir}/request.json"), &json!({ "digest": null }));
        scratch.write(&format!("{dir}/pid"), &process.id());
        (id, process)
    });
    let command = |program: &Path, args: &[&str]| {
        let mut command = scratch.program(program, args);
        command.args(["--runs", "runs"]).env("PATH", &path);
        ran(command, &scratch.0, false)
    };
    let sent = sent.to_str().unwrap();
    let mut steps = vec![
        command(&installed, &["cancel", CANCELLED]),
        command(&installed, &["cancel", "--modal-config", sent, CANCELLED]),
        command(&installed, &["reap"]),
    ];
    let states = instances.iter().map(|id| {
        let state = fs::read_to_string(bin.join(format!("instances/{id}/state"))).unwrap();
        format!("{id}: {}", state.trim())
    });
    steps.push(format!(
        "the stand-in aws's instances:\n{}",
        states.collect::<Vec<_>>().join("\n")
    ));
    let ended = sandboxes.map(|(id, mut process)| {
        let returncode = bin.join(format!("sandboxes/{id}/returncode"));
        let ended = fs::read_to_string(returncode).unwrap_or_else(|_| "none".to_owned());
        // Its standard input ends, so that it ends, if a delete didn't kill it.
        drop(process.stdin.take());
        format!("{id}: returncode {ended}, {}", process.wait().unwrap())
    });
    steps.push(format!(
        "the stand-in SDK's sandboxes:\n{}",
        ended.join("\n")
    ));
    for run in [SPREAD, CANCELLED] {
        let reaps = fs::read_to_string(scratch.0.join(format!("runs/{run}/reaps.jsonl"))).unwrap();
        let reaps = reaps.lines().map(|line| {
            let mut line: serde_json::Value = serde_json::from_str(line).unwrap();
            line["at_ns"] = json!("<at>");
            line.to_string()
        });
        steps.push(format!(
            "{run}'s reaps.jsonl:\n{}",
            reaps.collect::<Vec<_>>().join("\n")
        ));
    }
    let called = fs::read_to_string(&calls).unwrap();
    steps.push(format!("docker was called with:\n{}", called.trim_end()));
    steps.push(command(Path::new(PUMPKIN), &["reap"]));
    let built = Path::new(PUMPKIN).parent().unwrap().to_str().unwrap();
    check(
        &scratch,
        steps.into_iter().map(Step::Ran),
        false,
        &[(built, "<built>")],
        &expect![[r#"
            $ PATH=<scratch>/bin:/usr/bin:/bin <scratch>/.pumpkin/installed/pumpkin cancel 20261108-120800-0a000009 --runs runs
            exit status: Some(3)
            pumpkin: run 20261108-120800-0a000009's sandboxes on provider `gpus`, if it left any, can't be removed, as it can't be reached: Modal refused the helper's `ready`: AuthError: Modal profile 'pumpkin' was not found in <scratch>/job/modal.toml. Run `modal profile list` to see available profiles: fix what Modal reports; once it can be, `pumpkin reap --runs <scratch>/runs` removes them, and if it is gone for good, remove the record, <scratch>/runs/20261108-120800-0a000009, to stop reaping it
            pumpkin: run 20261108-120800-0a000009 left 1 sandbox on provider `aws`, now removed: i-00000000000000039
            pumpkin: run 20261108-120800-0a000009 was lost, as its driver ended without an outcome.json: it is now recorded as failed
            pumpkin: run 20261108-120800-0a000009 failed: its driver ended without an outcome; its record is <scratch>/runs/20261108-120800-0a000009
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/job.toml
            pumpkin: to remove what it left once its provider can be reached: pumpkin reap --runs <scratch>/runs
            printed outcome.json: {"reason":"its driver ended without an outcome","state":"failed"}
            $ PATH=<scratch>/bin:/usr/bin:/bin <scratch>/.pumpkin/installed/pumpkin cancel --modal-config <scratch>/sent/modal.toml 20261108-120800-0a000009 --runs runs
            exit status: Some(0)
            pumpkin: run 20261108-120800-0a000009 left 1 sandbox on provider `gpus`, now removed: sb-9
            pumpkin: run 20261108-120800-0a000009 failed: its driver ended without an outcome; its record is <scratch>/runs/20261108-120800-0a000009
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/job.toml
            printed outcome.json: {"reason":"its driver ended without an outcome","state":"failed"}
            $ PATH=<scratch>/bin:/usr/bin:/bin <scratch>/.pumpkin/installed/pumpkin reap --runs runs
            exit status: Some(3)
            pumpkin: run 20261108-120700-0a000008's sandbox i-0000000000000000f on provider `aws`, which its record holds undeleted, isn't listed there now: it has ended since, or the provider is reached elsewhere than the run reached it, as when the configuration that ran the run, or the Docker context, has moved to another region, profile or engine since; if so, put it back as it was when the run ran, and reap again: pumpkin reap --runs <scratch>/runs
            pumpkin: run 20261108-120700-0a000008 left 1 sandbox on provider `aws`, now removed: i-00000000000000038
            pumpkin: run 20261108-120700-0a000008 left 1 sandbox on provider `gpus`, now removed: sb-8
            pumpkin: run 20261108-120700-0a000008 left 1 sandbox on provider `local`, now removed: cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc
            pumpkin: run 20261108-120700-0a000008 was lost, as its driver ended without an outcome.json: it is now recorded as failed
            pumpkin: run 20261108-120800-0a000009's sandboxes on provider `gpus`, if it left any, can't be removed, as it can't be reached: Modal refused the helper's `ready`: AuthError: Modal profile 'pumpkin' was not found in <scratch>/job/modal.toml. Run `modal profile list` to see available profiles: fix what Modal reports; once it can be, `pumpkin reap --runs <scratch>/runs` removes them, and if it is gone for good, remove the record, <scratch>/runs/20261108-120800-0a000009, to stop reaping it
            pumpkin: run 20261108-120900-0a00000a's sandboxes on provider `aws`, if it left any, can't be removed, as it can't be reached: the configuration that ran it, <scratch>/gone.toml, can't be read (No such file or directory (os error 2)): put it back there, as it was when the run ran; once it can be, `pumpkin reap --runs <scratch>/runs` removes them, and if it is gone for good, remove the record, <scratch>/runs/20261108-120900-0a00000a, to stop reaping it
            pumpkin: run 20261108-120900-0a00000a was lost, as its driver ended without an outcome.json: it is now recorded as failed
            pumpkin: run 20261108-121100-0a00000c's sandboxes on provider `aws`, if it left any, can't be removed, as it can't be reached: the configuration that ran it, <scratch>/bare.toml, names no provider `aws`: put its `[providers.aws]` table back there, as it was when the run ran; once it can be, `pumpkin reap --runs <scratch>/runs` removes them, and if it is gone for good, remove the record, <scratch>/runs/20261108-121100-0a00000c, to stop reaping it
            pumpkin: run 20261108-121100-0a00000c was lost, as its driver ended without an outcome.json: it is now recorded as failed
            pumpkin: 3 runs ran on a provider that can't be reached, as said above: what each left there may remain
            pumpkin: to reap again once it can be reached: pumpkin reap --runs <scratch>/runs
            printed:
              20261108-120700-0a000008
              20261108-120900-0a00000a
              20261108-121100-0a00000c
            the stand-in aws's instances:
            i-00000000000000038: terminated
            i-00000000000000039: terminated
            i-00000000000000062: running
            the stand-in SDK's sandboxes:
            sb-8: returncode -9, signal: 9 (SIGKILL)
            sb-9: returncode -9, signal: 9 (SIGKILL)
            sb-b: returncode none, exit status: 0
            20261108-120700-0a000008's reaps.jsonl:
            {"at_ns":"<at>","id":"i-00000000000000038","provider":"aws","reaped":"removed"}
            {"at_ns":"<at>","id":"sb-8","provider":"gpus","reaped":"removed"}
            {"at_ns":"<at>","id":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","provider":"local","reaped":"removed"}
            {"at_ns":"<at>","id":"i-0000000000000000f","provider":"aws","reaped":"gone"}
            20261108-120800-0a000009's reaps.jsonl:
            {"at_ns":"<at>","id":"i-00000000000000039","provider":"aws","reaped":"removed"}
            {"at_ns":"<at>","id":"sb-9","provider":"gpus","reaped":"removed"}
            docker was called with:
            context inspect --format {{.Endpoints.docker.Host}} -- default
            info --format {{json .}}
            container ls --all --no-trunc --format {{.ID}} {{.State}} {{.Label "pumpkin.run"}} --filter label=pumpkin.run --filter label=pumpkin.run=20261108-120700-0a000008
            container ls --all --no-trunc --format {{.ID}} {{.State}} {{.Label "pumpkin.run"}} --filter label=pumpkin.run --filter id=cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc
            rm --force --volumes cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc
            $ PATH=<scratch>/bin:/usr/bin:/bin <built>/pumpkin reap --runs runs
            exit status: Some(3)
            pumpkin: run 20261108-120700-0a000008's sandboxes on provider `gpus`, if it left any, can't be removed, as it can't be reached: <built>/pumpkin-modal.py doesn't exist: expected Modal's helper and its lock beside `pumpkin`, as `cargo xtask install` installs them; once it can be, `pumpkin reap --runs <scratch>/runs` removes them, and if it is gone for good, remove the record, <scratch>/runs/20261108-120700-0a000008, to stop reaping it
            pumpkin: run 20261108-120700-0a000008's sandbox i-0000000000000000f on provider `aws`, which its record holds undeleted, isn't listed there now: it has ended since, or the provider is reached elsewhere than the run reached it, as when the configuration that ran the run, or the Docker context, has moved to another region, profile or engine since; if so, put it back as it was when the run ran, and reap again: pumpkin reap --runs <scratch>/runs
            pumpkin: run 20261108-120700-0a000008 left 1 sandbox on provider `local`, now removed: cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc
            pumpkin: run 20261108-120800-0a000009's sandboxes on provider `gpus`, if it left any, can't be removed, as it can't be reached: <built>/pumpkin-modal.py doesn't exist: expected Modal's helper and its lock beside `pumpkin`, as `cargo xtask install` installs them; once it can be, `pumpkin reap --runs <scratch>/runs` removes them, and if it is gone for good, remove the record, <scratch>/runs/20261108-120800-0a000009, to stop reaping it
            pumpkin: run 20261108-120900-0a00000a's sandboxes on provider `aws`, if it left any, can't be removed, as it can't be reached: the configuration that ran it, <scratch>/gone.toml, can't be read (No such file or directory (os error 2)): put it back there, as it was when the run ran; once it can be, `pumpkin reap --runs <scratch>/runs` removes them, and if it is gone for good, remove the record, <scratch>/runs/20261108-120900-0a00000a, to stop reaping it
            pumpkin: run 20261108-121100-0a00000c's sandboxes on provider `aws`, if it left any, can't be removed, as it can't be reached: the configuration that ran it, <scratch>/bare.toml, names no provider `aws`: put its `[providers.aws]` table back there, as it was when the run ran; once it can be, `pumpkin reap --runs <scratch>/runs` removes them, and if it is gone for good, remove the record, <scratch>/runs/20261108-121100-0a00000c, to stop reaping it
            pumpkin: 4 runs ran on a provider that can't be reached, as said above: what each left there may remain
            pumpkin: to reap again once it can be reached: pumpkin reap --runs <scratch>/runs
            printed:
              20261108-120700-0a000008"#]],
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
