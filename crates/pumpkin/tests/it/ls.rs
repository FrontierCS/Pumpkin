//! `ls`, which lists the runs in their states.

use std::fs::{self, File, Permissions};
use std::os::unix::fs::PermissionsExt as _;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use expect_test::expect;
use serde_json::{Value, json};

use super::{
    Prices, Scratch, Step, asked, check, cut, lease_places, lease_task, logged, masked, ran,
    returned, whole_log, write_record, write_run,
};

#[test]
fn launcher_workspace() {
    let scratch = Scratch::new();
    let id = "20261107-120000-1b8a6389";
    write_record(&scratch, id, true, Some(returned(7.0)));
    let workspace = scratch.0.join("consumer/.pumpkin");
    fs::create_dir_all(&workspace).unwrap();
    fs::rename(scratch.0.join("runs"), workspace.join("runs")).unwrap();
    let mut command = scratch.program(super::PUMPKIN, &["ls"]);
    command.env("PUMPKIN_WORKSPACE", &workspace);
    let output = command.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8(output.stdout).unwrap().contains(id));
}

#[test]
fn lists() {
    let scratch = Scratch::new();
    let task = |id: &str, name: &str| {
        scratch.write(&format!("runs/{id}/ir.json"), &json!({ "name": name }));
    };
    let ended = |id: &str, name: &str, outcome| {
        write_record(&scratch, id, true, Some(outcome));
        task(id, name);
    };
    ended("20261107-120000-1b8a6389", "examples/hello", returned(7.0));
    let mut showing_none = returned(7.0);
    showing_none["verdict"]["result"]["values"] = json!([]);
    ended("20261107-120100-2c9b0001", "examples/hello", showing_none);
    ended(
        "20261107-120200-3d0c0002",
        "examples/codex",
        json!({ "state": "failed", "reason": "r" }),
    );
    ended(
        "20261107-120300-4e1d0003",
        "examples/codex",
        json!({ "state": "interrupted", "signal": "SIGINT" }),
    );
    let live = "20261107-120400-5f2e0004";
    write_record(&scratch, live, true, None);
    task(live, "examples/hello");
    let driver = File::open(scratch.0.join(format!("runs/{live}/driver.pid"))).unwrap();
    driver.lock().unwrap();
    write_record(&scratch, "20261107-120500-602f0005", true, None);
    write_record(&scratch, "20261107-120600-7130006", false, None);
    let ls = |runs: &str| Step::from(scratch.pumpkin(&["ls", "--runs", runs]));
    // Nothing is listed for a default runs directory that isn't there yet.
    let fresh = scratch.pumpkin(&["ls"]);
    let listed = masked(
        &ran(
            scratch.pumpkin(&["ls", "--runs", "runs"]),
            &scratch.0,
            false,
        ),
        live,
    );
    check(
        &scratch,
        [Step::Ran(listed), ls("nowhere"), Step::from(fresh)],
        false,
        &[],
        &expect![[r#"
            $ pumpkin ls --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/20261107-120600-7130006/driver.pid isn't there: expected the record of a run to have it
            pumpkin: 1 run can't be listed, as said above
            printed:
              20261107-120000-1b8a6389 finished          -  -               -  examples/hello: score=7 metrics={"solved":3}
              20261107-120100-2c9b0001 finished          -  -               -  examples/hello
              20261107-120200-3d0c0002 failed            -  -               -  examples/codex
              20261107-120300-4e1d0003 interrupted       -  -               -  examples/codex
              20261107-120400-5f2e0004 publishing      <t>                  -  examples/hello
              20261107-120500-602f0005 lost              -  -               -  -
              6 runs: 1 running, 2 finished, 1 failed, 1 interrupted, 1 lost
            $ pumpkin ls --runs nowhere
            exit status: Some(1)
            pumpkin: nowhere: No such file or directory (os error 2): expected the directory that runs are recorded in, as `--runs` names it
            $ pumpkin ls
            exit status: Some(0)"#]],
    );
}

#[test]
fn lists_other_builds_unread() {
    let scratch = Scratch::new();
    let made = |id: &str, build: Value| {
        let record = json!({ "record_version": "pumpkin.record/v1", "pumpkin": build, "args": [] });
        scratch.write(&format!("runs/{id}/record.json"), &record);
        scratch.write(&format!("runs/{id}/driver.pid"), "4242\n");
    };
    let other =
        json!({ "version": "9.9.9", "commit": { "hash": "0f3a2b1c9d8e", "date": "2026-11-08" } });
    write_record(
        &scratch,
        "20261107-120000-1b8a6389",
        true,
        Some(returned(7.0)),
    );
    // An outcome that this build doesn't know, from a build that does.
    made("20261107-120100-2c9b0001", other.clone());
    scratch.write(
        "runs/20261107-120100-2c9b0001/outcome.json",
        &json!({ "state": "judged" }),
    );
    // A record version that is not this build's.
    scratch.write(
        "runs/20261107-120200-3d0c0002/record.json",
        &json!({ "record_version": "pumpkin.record/v9", "pumpkin": other, "args": [] }),
    );
    // An older record that doesn't read: it names no build.
    made("20261107-120300-4e1d0003", Value::Null);
    scratch.write(
        "runs/20261107-120300-4e1d0003/outcome.json",
        &json!({ "state": "judged" }),
    );
    let ls = || {
        Step::Ran(ran(
            scratch.pumpkin(&["ls", "--runs", "runs"]),
            &scratch.0,
            false,
        ))
    };
    let unread = ls();
    // Another build's record that the kernel won't let this build read says nothing of its form:
    // it is an error, not an unread row.
    let denied = "20261107-120400-5f2e0004";
    made(denied, other);
    let outcome = format!("runs/{denied}/outcome.json");
    scratch.write(&outcome, &json!({ "state": "failed", "reason": "r" }));
    fs::set_permissions(scratch.0.join(outcome), Permissions::from_mode(0o000)).unwrap();
    check(
        &scratch,
        [unread, ls()],
        false,
        &[],
        &expect![[r#"
            $ pumpkin ls --runs runs
            exit status: Some(0)
            printed:
              20261107-120000-1b8a6389 finished          -  -               -  -: score=7 metrics={"solved":3}
              20261107-120100-2c9b0001 unread            -                  -  made by pumpkin 9.9.9 (0f3a2b1c9 2026-11-08)
              20261107-120200-3d0c0002 unread            -                  -  made by pumpkin 9.9.9 (0f3a2b1c9 2026-11-08)
              20261107-120300-4e1d0003 unread            -                  -  made by an older pumpkin
              4 runs: 1 finished, 3 unread
            $ pumpkin ls --runs runs
            exit status: Some(1)
            pumpkin: <scratch>/runs/20261107-120400-5f2e0004/outcome.json: Permission denied (os error 13)
            pumpkin: 1 run can't be listed, as said above
            printed:
              20261107-120000-1b8a6389 finished          -  -               -  -: score=7 metrics={"solved":3}
              20261107-120100-2c9b0001 unread            -                  -  made by pumpkin 9.9.9 (0f3a2b1c9 2026-11-08)
              20261107-120200-3d0c0002 unread            -                  -  made by pumpkin 9.9.9 (0f3a2b1c9 2026-11-08)
              20261107-120300-4e1d0003 unread            -                  -  made by an older pumpkin
              4 runs: 1 finished, 3 unread"#]],
    );
}

#[test]
fn lists_the_last_day() {
    let scratch = Scratch::new();
    let ended = |id: &str, days: u64| {
        write_record(
            &scratch,
            id,
            true,
            Some(json!({ "state": "failed", "reason": "r" })),
        );
        let outcome = scratch.0.join(format!("runs/{id}/outcome.json"));
        let at = SystemTime::now() - Duration::from_hours(24 * days);
        let outcome = File::options().write(true).open(outcome).unwrap();
        outcome.set_modified(at).unwrap();
    };
    ended("20261106-120000-1b8a0001", 2);
    ended("20261107-110000-1b8a0002", 3);
    ended("20261107-120000-1b8a0003", 0);
    // Another build's run that this build can't read, which ended over a day ago too.
    let unread = "20261106-130000-2c9b0004";
    let other = json!({ "version": "9.9.9", "commit": null });
    let record = json!({ "record_version": "pumpkin.record/v1", "pumpkin": other, "args": [] });
    scratch.write(&format!("runs/{unread}/record.json"), &record);
    scratch.write(&format!("runs/{unread}/driver.pid"), "4242\n");
    scratch.write(
        &format!("runs/{unread}/outcome.json"),
        &json!({ "state": "judged" }),
    );
    let outcome = scratch.0.join(format!("runs/{unread}/outcome.json"));
    let outcome = File::options().write(true).open(outcome).unwrap();
    outcome
        .set_modified(SystemTime::now() - Duration::from_hours(48))
        .unwrap();
    let ls = |args: &[&str]| {
        let mut ls = scratch.pumpkin(&["ls", "--runs", "runs"]);
        ls.args(args);
        Step::Ran(ran(ls, &scratch.0, false))
    };
    let mut steps = vec![ls(&[]), ls(&["20261106"])];
    // Only runs that ended over a day ago.
    fs::remove_dir_all(scratch.0.join("runs/20261107-120000-1b8a0003")).unwrap();
    steps.push(ls(&[]));
    check(
        &scratch,
        steps,
        false,
        &[],
        &expect![[r#"
            $ pumpkin ls --runs runs
            exit status: Some(0)
            printed:
              20261107-120000-1b8a0003 failed            -  -               -  -
              1 run: 1 failed; 3 runs that ended over a day ago aren't listed: to list them: pumpkin ls --runs <scratch>/runs 2026110
            $ pumpkin ls --runs runs 20261106
            exit status: Some(0)
            printed:
              20261106-120000-1b8a0001 failed            -  -               -  -
              20261106-130000-2c9b0004 unread            -                  -  made by pumpkin 9.9.9
              2 runs: 1 failed, 1 unread
            $ pumpkin ls --runs runs
            exit status: Some(0)
            printed:
              no run is live, or ended within a day; 3 runs that ended over a day ago aren't listed: to list them: pumpkin ls --runs <scratch>/runs 2026110"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn lists_phases() {
    let scratch = Scratch::new();
    let log = whole_log();
    // A run that went the whole way, priced but for its box's class, and one that ended at its
    // deadline with nothing submitted, unpriced.
    let whole = "20261110-100000-0000000a";
    let outcome = returned(49.36);
    let priced = lease_places(Prices::Both);
    write_run(
        &scratch,
        whole,
        asked(whole, 30),
        &log,
        Some(outcome),
        &priced,
    );
    let late = "20261110-110000-0000000b";
    let unsubmitted = super::unsubmitted();
    let late_log = [
        logged(5, json!({"kind":"work_started"}), json!([])),
        logged(
            5,
            json!({"kind":"agent_started","caller":"agent-0"}),
            json!([]),
        ),
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
    ];
    let unpriced = lease_places(Prices::Nothing);
    write_run(
        &scratch,
        late,
        asked(late, 90),
        &late_log,
        Some(unsubmitted),
        &unpriced,
    );
    // And the first priced otherwise: its tokens alone, its sandboxes alone, and nothing.
    for (id, prices) in [
        ("20261110-120000-0000000c", Prices::Tokens),
        ("20261110-120000-0000000d", Prices::Sandboxes),
        ("20261110-120000-0000000e", Prices::Nothing),
    ] {
        let outcome = returned(49.36);
        let places = lease_places(prices);
        write_run(&scratch, id, asked(id, 30), &log, Some(outcome), &places);
    }
    // Live, cut at the second named, its last record written 2 seconds ago: as it starts its
    // agent, as the agent has made model requests, as it has called a job function, and as the
    // final call runs; and an agent's that was written long ago, past its deadline.
    let ago = Duration::from_secs(2);
    let mut held: Vec<File> = [
        ("20261111-090000-0000000a", 0, ago),
        ("20261111-090000-0000000b", 30, ago),
        ("20261111-090000-0000000c", 62, ago),
        ("20261111-090000-0000000d", 1012, ago),
        ("20261111-090000-0000000e", 30, Duration::from_hours(1)),
    ]
    .into_iter()
    .map(|(id, seconds, ago)| live(&scratch, id, &cut(&log, seconds), ago))
    .collect();
    // As its upstream answered its agent's requests otherwise: with a 200 whose stream states a
    // rate limit, or a failure with no code; with a 429; or cut short.
    let answer = |status: u64, more: Value| {
        let mut answer = json!({
            "kind": "model_answered", "caller": "agent-0",
            "answer": { "kind": "forwarded" }, "upstream_status": status,
        });
        for (field, value) in more.as_object().unwrap() {
            answer[field] = value.clone();
        }
        answer
    };
    let good = || answer(200, json!({}));
    let limited = || answer(200, json!({ "failed": { "code": "rate_limit_exceeded" } }));
    let answered = [
        (
            "20261111-090000-0000000g",
            vec![good(), limited(), limited()],
        ),
        (
            "20261111-090000-0000000h",
            vec![limited(), answer(200, json!({ "failed": {} })), limited()],
        ),
        (
            "20261111-090000-0000000i",
            vec![good(), answer(429, json!({}))],
        ),
        (
            "20261111-090000-0000000j",
            vec![good(), answer(200, json!({ "answer": { "kind": "cut" } }))],
        ),
    ];
    for (id, answers) in answered {
        let mut log = vec![
            logged(5, json!({"kind":"work_started"}), json!([])),
            logged(
                5,
                json!({"kind":"agent_started","caller":"agent-0"}),
                json!([]),
            ),
        ];
        for (request, (seconds, mut answer)) in (0_u64..).zip((10..).step_by(10).zip(answers)) {
            answer["request"] = json!(request);
            log.push(logged(seconds, answer, json!([])));
        }
        held.push(live(&scratch, id, &log, ago));
    }
    // And one whose images are still being published, with no log yet.
    let publishing = "20261111-090000-0000000f";
    write_record(&scratch, publishing, true, None);
    scratch.write(&format!("runs/{publishing}/ir.json"), &lease_task());
    let driver = File::open(scratch.0.join(format!("runs/{publishing}/driver.pid"))).unwrap();
    driver.lock().unwrap();
    held.push(driver);
    let ls = |prefixes: &[&str]| {
        let mut args = vec!["ls", "--runs", "runs"];
        args.extend(prefixes);
        masked(&ran(scratch.pumpkin(&args), &scratch.0, false), "20261111-")
    };
    check(
        &scratch,
        [
            Step::Ran(ls(&["20261110"])),
            Step::Ran(ls(&["20261110-10", "20261111"])),
        ],
        false,
        &[],
        &expect![[r#"
            $ pumpkin ls --runs runs 20261110
            exit status: Some(0)
            printed:
              20261110-100000-0000000a finished     18m20s  -          $1.10+  example/lease: score=49.36 metrics={"solved":3}
              20261110-110000-0000000b finished     30m06s  -               -  example/lease
              20261110-120000-0000000c finished     18m20s  -          $0.66+  example/lease: score=49.36 metrics={"solved":3}
              20261110-120000-0000000d finished     18m20s  -          $0.44+  example/lease: score=49.36 metrics={"solved":3}
              20261110-120000-0000000e finished     18m20s  -               -  example/lease: score=49.36 metrics={"solved":3}
              5 runs: 5 finished (1 without a result); $2.20 so far, and more without a price, and for 2 responses' unreported tokens
            $ pumpkin ls --runs runs 20261110-10 20261111
            exit status: Some(0)
            printed:
              20261110-100000-0000000a finished     18m20s  -          $1.10+  example/lease: score=49.36 metrics={"solved":3}
              20261111-090000-0000000a starting        <t>  of 2m       $0.00  example/lease: 1 agent
                                       starting        <t>  of 2m              solver on aws/judge
              20261111-090000-0000000b work            <t>  of 30m      $0.66  example/lease: 1 agent working; model request 2 in flight for <t>
                                       solver          <t>  of 30m             2 model requests, the last answered <t> ago; model request 2 in flight for <t>
              20261111-090000-0000000c work            <t>  of 30m     $0.68+  example/lease: 1 agent working
                                       solver          <t>  of 30m             3 model requests, the last answered <t> ago
                                       job 0           <t>  of 10m06s          train call on modal/a100_80 with unknown GPUs for agent 0
                                       lease 0         <t>  of 1m              box of job 0 on aws/box, ready, 5s busy; `sh -c set -e\nprint 7` exited 0
              20261111-090000-0000000d final call      <t>  of 15m04s  $1.04+  example/lease: job 4 on modal/a100_80 with NVIDIA A100-SXM4-80GB; model request 3 in flight for <t>
              20261111-090000-0000000e work            <t>  of 30m      $0.66  example/lease: 1 agent working; model request 2 in flight for <t>; it is past its bound, which is a bug: to stop it: pumpkin cancel --runs <scratch>/runs 20261111-090000-0000000e
                                       solver          <t>  of 30m             2 model requests, the last answered <t> ago; model request 2 in flight for <t>; it is past its bound, which is a bug: to stop it: pumpkin cancel --runs <scratch>/runs 20261111-090000-0000000e
              20261111-090000-0000000f publishing      <t>                  -  example/lease
              20261111-090000-0000000g work            <t>  of 30m      $0.00  example/lease: 1 agent working
                                       solver          <t>  of 30m             3 model requests, 2 failed upstream: rate_limit_exceeded; the last good one answered <t> ago
              20261111-090000-0000000h work            <t>  of 30m      $0.00  example/lease: 1 agent working
                                       solver          <t>  of 30m             3 model requests, 3 failed upstream: 1 with no code, 2 rate_limit_exceeded; no good one answered yet
              20261111-090000-0000000i work            <t>  of 30m      $0.00  example/lease: 1 agent working
                                       solver          <t>  of 30m             2 model requests, answered 1 200, 1 429; the last good one answered <t> ago
              20261111-090000-0000000j work            <t>  of 30m      $0.00  example/lease: 1 agent working
                                       solver          <t>  of 30m             2 model requests, answered 1 200, 1 200 cut; the last good one answered <t> ago
              11 runs: 10 running, 1 finished; $4.15 so far, and more without a price, and for 3 responses' unreported tokens; every live run ends by <t> UTC"#]],
    );
}

#[test]
fn lists_leases() {
    let scratch = Scratch::new();
    let log = whole_log();
    // Live, cut at the second named, its last record written 2 seconds ago: as its train call's
    // box had a file put, ran a script, and was closed as its call ended; as the agent's runners
    // start, are ready, one running a command, and are closed by the agent or expire; and as the
    // run waits for the last after its work phase ended.
    let held: Vec<File> = [57, 62, 101, 120, 132, 142, 160, 566, 1002]
        .into_iter()
        .zip(1_u64..)
        .map(|(seconds, n)| {
            let id = format!("20261114-090000-{n:08x}");
            live(&scratch, &id, &cut(&log, seconds), Duration::from_secs(2))
        })
        .collect();
    let ls = masked(
        &ran(
            scratch.pumpkin(&["ls", "--runs", "runs"]),
            &scratch.0,
            false,
        ),
        "20261114-",
    );
    drop(held);
    check(
        &scratch,
        [Step::Ran(ls)],
        false,
        &[],
        &expect![[r#"
            $ pumpkin ls --runs runs
            exit status: Some(0)
            printed:
              20261114-090000-00000001 work            <t>  of 30m     $0.68+  example/lease: 1 agent working
                                       solver          <t>  of 30m             3 model requests, the last answered <t> ago
                                       job 0           <t>  of 10m06s          train call on modal/a100_80 with unknown GPUs for agent 0
                                       lease 0         <t>  of 1m              box of job 0 on aws/box, ready, 1s busy; put /pumpkin/in/payload to /work/in moved 1 entry, 1 byte
              20261114-090000-00000002 work            <t>  of 30m     $0.68+  example/lease: 1 agent working
                                       solver          <t>  of 30m             3 model requests, the last answered <t> ago
                                       job 0           <t>  of 10m06s          train call on modal/a100_80 with unknown GPUs for agent 0
                                       lease 0         <t>  of 1m              box of job 0 on aws/box, ready, 5s busy; `sh -c set -e\nprint 7` exited 0
              20261114-090000-00000003 work            <t>  of 30m     $0.71+  example/lease: 1 agent working
                                       solver          <t>  of 30m             3 model requests, the last answered <t> ago
                                       lease 0         <t>                     box of job 0 on aws/box, closed as its call ended; its sandbox isn't gone yet; `sh -c set -e\nprint 7` exited 0
              20261114-090000-00000004 work            <t>  of 30m     $0.71+  example/lease: 1 agent working
                                       solver          <t>  of 30m             3 model requests, the last answered <t> ago
                                       lease 1         <t>  of 30m             gpu of agent 0 on modal/a100_80, starting
                                       lease 2         <t>  of 2m              cpu of agent 0 on aws/judge, starting
              20261114-090000-00000005 work            <t>  of 30m     $0.72+  example/lease: 1 agent working
                                       solver          <t>  of 30m             3 model requests, the last answered <t> ago
                                       lease 1         <t>  of 1m              gpu of agent 0 on modal/a100_80 with NVIDIA A100-SXM4-80GB, ready, 2s busy; `nvidia-smi` exited 0
                                       lease 2         <t>  of 2m              cpu of agent 0 on aws/judge, starting
              20261114-090000-00000006 work            <t>  of 30m     $0.73+  example/lease: 1 agent working
                                       solver          <t>  of 30m             3 model requests, the last answered <t> ago
                                       lease 1         <t>                     gpu of agent 0 on modal/a100_80 with NVIDIA A100-SXM4-80GB, closed by its holder; its sandbox isn't gone yet; `nvidia-smi` exited 0
                                       lease 2         <t>  of 2m              cpu of agent 0 on aws/judge, starting
              20261114-090000-00000007 work            <t>  of 30m     $0.74+  example/lease: 1 agent working
                                       solver          <t>  of 30m             3 model requests, the last answered <t> ago
                                       lease 2         <t>  of 30m             cpu of agent 0 on aws/judge, ready, 2s busy; `sleep 60` running
              20261114-090000-00000008 work            <t>  of 30m     $0.94+  example/lease: 1 agent working
                                       solver          <t>  of 30m             3 model requests, the last answered <t> ago
                                       lease 2         <t>  of 30m             cpu of agent 0 on aws/judge, ready, 30s busy; `sleep 60` timed out
                                       lease 3         <t>                     gpu of agent 0 on modal/a100_80 with NVIDIA A100 80GB PCIe (its role needs NVIDIA A100-SXM4-80GB), expired; its sandbox isn't gone yet; `true` exited 0
              20261114-090000-00000009 ending          <t>  of 1h43m   $1.03+  example/lease: the end of lease 2
                                       lease 2         <t>                     cpu of agent 0 on aws/judge, closed as the work phase ended; its sandbox isn't gone yet; `sleep 60` timed out
              9 runs: 9 running; $6.95 so far, and more without a price, and for 9 responses' unreported tokens; every live run ends by <t> UTC"#]],
    );
}

// A log's parts that don't read are left out and said by `ls` and the summary, and the rest of
// their lines count; a run whose start doesn't read is listed, with no progress.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn says_unread_lines() {
    let scratch = Scratch::new();
    let id = "20261110-110000-0000000b";
    let log = [
        logged(5, json!({"kind":"work_started"}), json!([])),
        logged(
            5,
            json!({"kind":"agent_started","caller":"agent-0"}),
            json!([]),
        ),
        logged(10, json!({ "kind": "later" }), json!([])),
        logged(
            20,
            json!({ "kind": "agent_exited", "caller":"agent-0" }),
            json!([]),
        ),
        logged(
            30,
            json!({ "kind": "model_answered", "caller": "agent-0", "upstream_status": "200",
                "answer": { "kind": "forwarded" } }),
            json!([]),
        ),
        logged(
            40,
            json!({ "kind": "tick" }),
            json!([{ "kind": "tear_down_agent", "caller": "agent-0" }, { "kind": "close_lease" }, { "kind": "finish" }]),
        ),
    ];
    let unsubmitted = super::unsubmitted();
    let dir = write_run(
        &scratch,
        id,
        asked(id, 90),
        &log,
        Some(unsubmitted),
        &lease_places(Prices::Nothing),
    );
    let mut written = fs::read_to_string(dir.join("log.jsonl")).unwrap();
    written.push_str("{\"seq\":\n");
    fs::write(dir.join("log.jsonl"), written).unwrap();
    let unstarted = "20261110-110000-0000000c";
    let dir = write_run(
        &scratch,
        unstarted,
        asked(unstarted, 90),
        &log,
        Some(json!({ "state": "failed", "reason": "the agent's sandbox didn't start" })),
        &lease_places(Prices::Nothing),
    );
    let written = fs::read_to_string(dir.join("log.jsonl")).unwrap();
    let written = written.replacen("\"unix_ns\"", "\"unix\"", 1);
    fs::write(dir.join("log.jsonl"), written).unwrap();
    let steps = [
        &["ls", "--runs", "runs"][..],
        &["wait", "--runs", "runs", id],
        &["wait", "--runs", "runs", unstarted],
    ]
    .map(|args| Step::Ran(ran(scratch.pumpkin(args), &scratch.0, false)));
    check(
        &scratch,
        steps,
        false,
        &[],
        &expect![[r#"
            $ pumpkin ls --runs runs
            exit status: Some(0)
            pumpkin: run 20261110-110000-0000000b: 4 parts of its log can't be read by this pumpkin, the first at line 5, its event: missing field `status`
            pumpkin: run 20261110-110000-0000000c: 4 parts of its log can't be read by this pumpkin, the first at line 1, its event: missing field `unix_ns`
            printed:
              20261110-110000-0000000b finished        40s  -               -  example/lease
              20261110-110000-0000000c failed            -  -               -  example/lease
              2 runs: 1 finished (1 without a result), 1 failed
            $ pumpkin wait --runs runs 20261110-110000-0000000b
            exit status: Some(3)
            pumpkin: run 20261110-110000-0000000b started <t> after it was asked for, and ran <t>:
                 <t> solver on aws/judge, started at <t>; its work ended at <t>
                        model: 0 requests
              model: 0 requests
              sandboxes: 1 on aws/judge, <t> in all, no hourly rate, 1 created, all deleted
              estimated cost: unavailable (no priced usage)
              4 parts of its log can't be read by this pumpkin, the first at line 5, its event: missing field `status`
            pumpkin: run 20261110-110000-0000000b finished without a result: the final call gave none (error: solver submitted nothing); its commands' logs can't be found: <scratch>/runs/20261110-110000-0000000b/log.jsonl names no final call: expected a `start_final_call`, among the parts that this pumpkin can read; 4 of its parts can't be read, the first at line 5, its event: missing field `status`; its record is <scratch>/runs/20261110-110000-0000000b
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            $ pumpkin wait --runs runs 20261110-110000-0000000c
            exit status: Some(1)
            pumpkin: run 20261110-110000-0000000c: 4 parts of its log can't be read by this pumpkin, the first at line 1, its event: missing field `unix_ns`
            pumpkin: run 20261110-110000-0000000c failed: the agent's sandbox didn't start; its record is <scratch>/runs/20261110-110000-0000000c
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star"#]],
    );
}

/// Writes the record `id` in `scratch`'s `runs` of a live run of [`lease_task`], priced, whose
/// log holds `log`, its last record written `ago`; returns `driver.pid`, locked as its driver
/// does.
fn live(scratch: &Scratch, id: &str, log: &[Value], ago: Duration) -> File {
    let last = log
        .last()
        .map_or(Some("0"), |record| record["at_ns"].as_str());
    let last = Duration::from_nanos(last.unwrap().parse().unwrap());
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let dir = write_run(
        scratch,
        id,
        since.checked_sub(last + ago).unwrap(),
        log,
        None,
        &lease_places(Prices::Both),
    );
    let driver = File::open(dir.join("driver.pid")).unwrap();
    driver.lock().unwrap();
    driver
}
