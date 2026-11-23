use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};
use std::time::Duration;

use docker::Reached;
use expect_test::expect;
use provider::{RunTag, Sweep as _};
use serde_json::Value;
use test_support::{docker_context, sandboxd};

use super::{
    Scratch, Step, check, containers, engine, files, invocation, machine, poll, ran, record,
    report, sandboxes, signal, with_room, workflow,
};

struct Cleanup<'a> {
    containers: &'a Reached,
    runs: PathBuf,
}

// Kills the drivers and removes the containers that a failed test would leave on the engine.
impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        let Ok(entries) = fs::read_dir(&self.runs) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let Ok(id) = entry.file_name().into_string() else {
                continue;
            };
            if let Some(driver) = live_driver(&entry.path()) {
                let _ = signal("KILL", driver);
            }
            let Ok(tag) = RunTag::try_from(id) else {
                continue;
            };
            if let Ok(listed) = self.containers.list(&tag) {
                let ids: Vec<_> = listed.into_iter().map(|listed| listed.id).collect();
                let _ = self.containers.delete(&tag, &ids);
            }
        }
    }
}

fn unlocked(dir: &Path) {
    // A driver's main thread shows as ended, a zombie, while another thread may still be exiting,
    // holding the descriptors and so the lock.
    poll(|| Some(()).filter(|()| live_driver(dir).is_none()));
}

fn live_driver(dir: &Path) -> Option<i64> {
    let pid = File::open(dir.join("driver.pid")).ok()?;
    // A live driver holds an exclusive lock on its pid file; a stale file is unlocked.
    let held = matches!(pid.try_lock_shared(), Err(TryLockError::WouldBlock));
    let text = fs::read_to_string(dir.join("driver.pid")).ok()?;
    text.trim().parse().ok().filter(|_| held)
}

// An emulator such as Rosetta, as Lima registers it, gives the driver the executable's path as its
// `argv[0]`, as this command does.
#[test]
fn known_by_its_command() {
    let scratch = Scratch::new();
    let id = "20261123-120000-0a000001";
    let driver = scratch.pumpkin(&["driver", "1", id, "operator", "--", "run", "task.star"]);
    check(
        &scratch,
        [driver],
        false,
        &[],
        &expect![[r#"
            $ pumpkin driver 1 20261123-120000-0a000001 operator -- run task.star
            exit status: Some(1)
            pumpkin: `run` ended before its driver asked to end with it"#]],
    );
}

#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the run's steps and their snapshot"
)]
fn lost_on_docker() {
    let scratch = Scratch::new();
    let sandboxd = sandboxd().into_os_string().into_string().unwrap();
    let context = docker_context();
    let engine = engine(&scratch, &context);
    let containers = containers(&context);
    scratch.write_task("sleeps", &workflow("", &["sleep", "300"]));
    scratch.write("pumpkin.toml", &machine(&context));
    let _cleanup = Cleanup {
        containers: &containers,
        runs: scratch.0.join("runs"),
    };
    let temp = scratch.0.join("tmp");
    fs::create_dir(&temp).unwrap();
    let mut run = scratch.pumpkin(&["run", "sleeps/task.star", "--sandboxd", &sandboxd]);
    run.args(["--runs", "runs", "--config", "pumpkin.toml"]);
    run.env("TMPDIR", &temp);
    let invoked = invocation(&run);
    let output = with_room(&mut run);
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    let (id, dir) = record(&stderr).unwrap();
    let tag = RunTag::try_from(id.to_owned()).unwrap();
    let record = scratch.0.join(dir);
    let created = record.join("agents/agent/sandbox.json");
    poll(|| Some(()).filter(|()| created.exists()));
    let listed = containers.list(&tag).unwrap();
    let agent = listed[0].id.as_str().to_owned();
    let driver = live_driver(&record).unwrap();
    assert!(signal("KILL", driver), "the driver is gone");
    unlocked(&record);
    let rootful = !engine.report().security.rootless;
    let mut killed = vec![report(invoked, &output, &scratch.0, rootful)];
    killed.extend(sandboxes(&record));
    // Names only: the agent's socket goes once the agent connects, which the kill may precede.
    let sockets = fs::read_dir(scratch.0.join(dir).join("sockets")).unwrap();
    let mut sockets: Vec<_> = sockets.map(|entry| entry.unwrap().file_name()).collect();
    sockets.sort();
    killed.push(format!("its socket directories: {sockets:?}"));
    killed.push(format!("its temporary directory holds: {:?}", files(&temp)));
    let mut steps = vec![killed.join("\n")];
    let pumpkin = |args: &[&str]| ran(scratch.pumpkin(args), &scratch.0, false);
    steps.push(pumpkin(&["wait", id, "--runs", "runs"]));
    steps.push(pumpkin(&["cancel", id, "--runs", "runs"]));
    let ir = fs::read_to_string(record.join("ir.json")).unwrap();
    let ir: serde_json::Value = serde_json::from_str(&ir).unwrap();
    let image = ir["roles"][0]["image"].as_str().unwrap();
    let mut create = test_support::docker();
    create.args([
        "create",
        "--label",
        &format!("pumpkin.run={id}"),
        image,
        "true",
    ]);
    let created = create.output().unwrap();
    assert!(created.status.success(), "{created:?}");
    let unstarted = String::from_utf8(created.stdout).unwrap().trim().to_owned();
    steps.push(pumpkin(&["reap", "--runs", "runs"]));
    let left = containers.list(&tag).unwrap().len();
    steps.push(format!("containers left: {left}"));
    let shown = [
        (&sandboxd[..], "<sandboxd>"),
        (id, "<id>"),
        (&agent[..], "<the agent's container>"),
        (&unstarted[..], "<the unstarted container>"),
    ];
    check(
        &scratch,
        steps.into_iter().map(Step::Ran),
        false,
        &shown,
        &expect![[r#"
            $ TMPDIR=<scratch>/tmp pumpkin run sleeps/task.star --sandboxd <sandboxd> --runs runs --config pumpkin.toml
            exit status: Some(0)
            pumpkin: run <id> records into <scratch>/runs/<id>
            pumpkin: run <id> started
            pumpkin: to follow it: pumpkin wait --runs <scratch>/runs <id>
            pumpkin: to stop it: pumpkin cancel --runs <scratch>/runs <id>
            outcome.json: none
            printed the id alone: true
            agent `agent`'s sandbox, agents/agent/sandbox.json: {"id":"<container>","provider":"local"}
            its socket directories: ["agent-0"]
            its temporary directory holds: []
            $ pumpkin wait <id> --runs runs
            exit status: Some(1)
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container, started at <t>
              sandboxes: 1 on local/container, <t> in all, 1 created, 0 deleted
            pumpkin: run <id> is lost: its driver ended without writing outcome.json, so nothing says how it ended; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <sandboxd> <scratch>/sleeps/task.star
            pumpkin: to remove what it left: pumpkin cancel --runs <scratch>/runs <id>
            pumpkin: to remove what every run that has ended left: pumpkin reap --runs <scratch>/runs
            $ pumpkin cancel <id> --runs runs
            exit status: Some(0)
            pumpkin: run <id> left 1 sandbox on provider `local`, now removed: <the agent's container>
            pumpkin: run <id> left its socket directory, now removed: <scratch>/runs/<id>/sockets
            pumpkin: run <id> was lost, as its driver ended without an outcome.json: it is now recorded as failed
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container, started at <t>
              sandboxes: 1 on local/container, <t> in all, 1 created, 0 deleted
            pumpkin: run <id> failed: its driver ended without an outcome; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <sandboxd> <scratch>/sleeps/task.star
            $ pumpkin reap --runs runs
            exit status: Some(0)
            pumpkin: run <id> left 1 sandbox on provider `local`, now removed: <the unstarted container>
            printed:
              <id>
            containers left: 0"#]],
    );
}

#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
fn signaled_on_docker() {
    let scratch = Scratch::new();
    let sandboxd = sandboxd().to_str().unwrap().to_owned();
    let context = docker_context();
    let containers = containers(&context);
    let ir = workflow("", &["sleep", "300"]);
    let deadline = ir["deadline_seconds"].as_u64();
    let deadline = Duration::from_secs(deadline.unwrap());
    scratch.write_task("sleeps", &ir);
    scratch.write("pumpkin.toml", &machine(&context));
    let _cleanup = Cleanup {
        containers: &containers,
        runs: scratch.0.join("runs"),
    };
    let mut run = scratch.pumpkin(&["run", "sleeps/task.star", "--sandboxd", &sandboxd]);
    run.args(["--runs", "runs", "--config", "pumpkin.toml"]);
    let output = with_room(&mut run);
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    let (id, dir) = record(&stderr).unwrap();
    let record = scratch.0.join(dir);
    let log = || fs::read_to_string(record.join("log.jsonl")).unwrap_or_default();
    poll(|| Some(()).filter(|()| log().contains(r#""kind":"agent_started""#)));
    let driver = live_driver(&record).unwrap();
    assert!(signal("TERM", driver), "the driver is gone");
    // So that `wait` finds the run ended, rather than follow it while the driver ends.
    unlocked(&record);
    let wait = ran(
        scratch.pumpkin(&["wait", id, "--runs", "runs"]),
        &scratch.0,
        false,
    );
    // The sandboxes' deletes come after the stop's note, in records that hold none.
    let log = log();
    let records = log
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap());
    let last = records
        .rev()
        .find(|record| record["note"].is_string())
        .unwrap();
    let at = Duration::from_nanos(last["at_ns"].as_str().unwrap().parse().unwrap());
    let tag = RunTag::try_from(id.to_owned()).unwrap();
    let sockets = record.join("sockets");
    let lines = [
        wait,
        format!("the run log's last note: {}", last["note"]),
        format!("it came before the deadline: {}", at < deadline),
        format!("containers left: {}", containers.list(&tag).unwrap().len()),
        format!("socket directory left: {}", sockets.exists()),
    ];
    let shown = [(id, "<id>"), (&sandboxd[..], "<sandboxd>")];
    check(
        &scratch,
        lines.map(Step::Ran),
        false,
        &shown,
        &expect![[r#"
            $ pumpkin wait <id> --runs runs
            exit status: Some(1)
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container, started at <t>
              sandboxes: 1 on local/container, <t> in all, 1 created, all deleted
            pumpkin: run <id> was interrupted by SIGTERM; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <sandboxd> <scratch>/sleeps/task.star
            the run log's last note: "the run was interrupted by SIGTERM"
            it came before the deadline: true
            containers left: 0
            socket directory left: false"#]],
    );
}
