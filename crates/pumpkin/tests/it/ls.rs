//! `ls`, and the commands that a user learns beside it: `run` of a task's entry, which builds and
//! starts a run, printing its id, or with `--wait` its outcome; and `ls`, which lists the runs
//! in their states.

use std::fs::{self, File};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use expect_test::expect;
use serde_json::{Value, json};

use super::{
    Prices, Scratch, StandIn, Step, asked, check, cut, judged, judged_log, lease_places,
    lease_task, logged, masked, poll, ran, write_record, write_run,
};

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the steps and their snapshot"
)]
fn runs_a_task() {
    let scratch = Scratch::new("runs-a-task");
    let stand_in = StandIn::new(&scratch);
    stand_in.hold("create");
    let ls = || {
        ran(
            scratch.pumpkin(&["ls", "--runs", "runs"]),
            &scratch.0,
            false,
        )
    };
    let mut steps = vec![ran(
        stand_in.run("stand-in/task.star", &[]),
        &scratch.0,
        false,
    )];
    let id = fs::read_dir(scratch.0.join("runs"))
        .unwrap()
        .next()
        .unwrap();
    let id = id.unwrap().file_name().into_string().unwrap();
    // Once the agent's create holds, as a slow one does, the run is starting its agent.
    stand_in.held("create");
    steps.push(masked(&ls(), &id));
    stand_in.release("create");
    // Ended, so that `wait` has nothing to follow.
    let outcome = scratch.0.join("runs").join(&id).join("outcome.json");
    poll(|| outcome.exists().then_some(()));
    let wait = scratch.pumpkin(&["wait", &id, "--runs", "runs"]);
    steps.push(ran(wait, &scratch.0, false));
    steps.push(ls());
    steps.push(ran(
        stand_in.run("stand-in/task.star", &["--wait"]),
        &scratch.0,
        false,
    ));
    check(
        &scratch,
        steps.into_iter().map(Step::Ran),
        false,
        &[(&id, "<first>")],
        &expect![[r#"
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
            exit status: Some(0)
            pumpkin: run <id> records into <scratch>/runs/<id>
            pumpkin: run <id> started
            pumpkin: to follow it: pumpkin wait --runs <scratch>/runs <id>
            pumpkin: to stop it: pumpkin cancel --runs <scratch>/runs <id>
            outcome.json: none
            printed the id alone: true
            $ pumpkin ls --runs runs
            exit status: Some(0)
            printed:
              <first> starting        <t>  of 9m           -  example/task: the agent on local/container
              1 run: 1 running; every live run ends by <t> UTC
            $ pumpkin wait <first> --runs runs
            exit status: Some(3)
            pumpkin: run <first> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container, failed at <t>: the agent's sandbox didn't start: `docker create` failed with exit status: 1: the stand-in creates nothing: fix what Docker reports; the work phase ended at <t>
              sandboxes: 1 on local/container, <t> in all, none created
            pumpkin: run <first> finished without a result: the agent submitted nothing, and the work phase ended when the agent failed: the agent's sandbox didn't start: `docker create` failed with exit status: 1: the stand-in creates nothing: fix what Docker reports; its record is <scratch>/runs/<first>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star
            printed outcome.json: {"state":"finished","usage":{"model_requests":0,"ran_ns":"<ns>","sandboxes":[{"class":"container","count":1,"lasted_ns":"<ns>","provider":"local"}],"tokens":{"input":0,"output":0,"responses":0}},"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: `docker create` failed with exit status: 1: the stand-in creates nothing: fix what Docker reports"}}
            $ pumpkin ls --runs runs
            exit status: Some(0)
            printed:
              <first> finished         0s  -               -  example/task
              1 run: 1 finished (1 without a result)
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml --wait
            exit status: Some(3)
            pumpkin: run <id> records into <scratch>/runs/<id>
            pumpkin: run <id> started
            pumpkin: following run <id>, recorded in <scratch>/runs/<id>: ending `pumpkin run` does not stop it
            pumpkin:    <t> the work phase ended
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container, failed at <t>: the agent's sandbox didn't start: `docker create` failed with exit status: 1: the stand-in creates nothing: fix what Docker reports; the work phase ended at <t>
              sandboxes: 1 on local/container, <t> in all, none created
            pumpkin: run <id> finished without a result: the agent submitted nothing, and the work phase ended when the agent failed: the agent's sandbox didn't start: `docker create` failed with exit status: 1: the stand-in creates nothing: fix what Docker reports; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star
            record: driver.log, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json, sources/task/lib/train.star, sources/task/stand-in/task.star
            outcome.json: {"state":"finished","usage":{"model_requests":0,"ran_ns":"<ns>","sandboxes":[{"class":"container","count":1,"lasted_ns":"<ns>","provider":"local"}],"tokens":{"input":0,"output":0,"responses":0}},"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: `docker create` failed with exit status: 1: the stand-in creates nothing: fix what Docker reports"}}
            printed outcome.json: true"#]],
    );
}

#[test]
fn lists() {
    let scratch = Scratch::new("lists");
    let task = |id: &str, name: &str| {
        scratch.write(&format!("runs/{id}/ir.json"), &json!({ "name": name }));
    };
    let ended = |id: &str, name: &str, outcome| {
        write_record(&scratch, id, true, Some(outcome));
        task(id, name);
    };
    ended(
        "20261107-120000-1b8a6389",
        "examples/hello",
        judged(7.0, json!({})),
    );
    ended(
        "20261107-120100-2c9b0001",
        "examples/hello",
        json!({ "state": "finished", "verdict": { "kind": "judged" } }),
    );
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
    let mut fresh = scratch.pumpkin(&["ls"]);
    fresh.env("HOME", scratch.0.join("home"));
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
            pumpkin: <scratch>/runs/20261107-120600-7130006/driver.pid: No such file or directory (os error 2)
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
            $ HOME=<scratch>/home pumpkin ls
            exit status: Some(0)"#]],
    );
}

#[test]
fn lists_the_last_day() {
    let scratch = Scratch::new("lists-the-last-day");
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
          1 run: 1 failed; 2 runs that ended over a day ago aren't listed: to list them: pumpkin ls --runs <scratch>/runs 2026110
        $ pumpkin ls --runs runs 20261106
        exit status: Some(0)
        printed:
          20261106-120000-1b8a0001 failed            -  -               -  -
          1 run: 1 failed
        $ pumpkin ls --runs runs
        exit status: Some(0)
        printed:
          no run is live, or ended within a day; 2 runs that ended over a day ago aren't listed: to list them: pumpkin ls --runs <scratch>/runs 2026110"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn lists_phases() {
    let scratch = Scratch::new("lists-phases");
    let log = judged_log();
    // A run that went the whole way, priced but for its box's class, and one that ended at its
    // deadline with nothing submitted, unpriced.
    let whole = "20261110-100000-0000000a";
    let outcome = judged(49.36, json!({ "raw_score": 49.36 }));
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
    let unjudged = json!({
        "state": "finished",
        "verdict": { "kind": "no_submission" }, "work_end": { "kind": "deadline" },
    });
    let late_log = [
        logged(5, json!({ "kind": "agent_started" }), json!([])),
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
    ];
    let unpriced = lease_places(Prices::Nothing);
    write_run(
        &scratch,
        late,
        asked(late, 90),
        &late_log,
        Some(unjudged),
        &unpriced,
    );
    // And the first priced otherwise: its tokens alone, its sandboxes alone, and nothing.
    for (id, prices) in [
        ("20261110-120000-0000000c", Prices::Tokens),
        ("20261110-120000-0000000d", Prices::Sandboxes),
        ("20261110-120000-0000000e", Prices::Nothing),
    ] {
        let outcome = judged(49.36, json!({ "raw_score": 49.36 }));
        let places = lease_places(prices);
        write_run(&scratch, id, asked(id, 30), &log, Some(outcome), &places);
    }
    // Live, cut at the second named, its last record written 2 seconds ago: as it starts its
    // agent, as the agent has made model requests, as it has called a judge, and as the final
    // judge runs; and an agent's that was written long ago, past its deadline.
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
            "kind": "model_answered", "caller": "agent",
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
        let mut log = vec![logged(5, json!({ "kind": "agent_started" }), json!([]))];
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
              20261111-090000-0000000a starting        <t>  of 2m       $0.00  example/lease: the agent on aws/judge
              20261111-090000-0000000b agent           <t>  of 30m      $0.66  example/lease: 2 model requests, the last answered <t> ago; model request 2 in flight for <t>
              20261111-090000-0000000c agent           <t>  of 30m     $0.68+  example/lease: 3 model requests, the last answered <t> ago
                                       job 0           <t>  of 10m06s          train call on modal/a100_80 with unknown GPUs
                                       lease 0         <t>  of 1m              box of job 0 on aws/box, ready, 5s busy; `sh -c set -e\nprint 7` exited 0
              20261111-090000-0000000d final judge     <t>  of 15m04s  $1.04+  example/lease: job 4 on modal/a100_80 with NVIDIA A100-SXM4-80GB; model request 3 in flight for <t>
              20261111-090000-0000000e agent           <t>  of 30m      $0.66  example/lease: 2 model requests, the last answered <t> ago; model request 2 in flight for <t>; it is past its bound, which is a bug: to stop it: pumpkin cancel --runs <scratch>/runs 20261111-090000-0000000e
              20261111-090000-0000000f publishing      <t>                  -  example/lease
              20261111-090000-0000000g agent           <t>  of 30m      $0.00  example/lease: 3 model requests, 2 failed upstream: rate_limit_exceeded; the last good one answered <t> ago
              20261111-090000-0000000h agent           <t>  of 30m      $0.00  example/lease: 3 model requests, 3 failed upstream: 1 with no code, 2 rate_limit_exceeded; no good one answered yet
              20261111-090000-0000000i agent           <t>  of 30m      $0.00  example/lease: 2 model requests, answered 1 200, 1 429; the last good one answered <t> ago
              20261111-090000-0000000j agent           <t>  of 30m      $0.00  example/lease: 2 model requests, answered 1 200, 1 200 cut; the last good one answered <t> ago
              11 runs: 10 running, 1 finished; $4.15 so far, and more without a price, and for 3 responses' unreported tokens; every live run ends by <t> UTC"#]],
    );
}

#[test]
fn lists_leases() {
    let scratch = Scratch::new("lists-leases");
    let log = judged_log();
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
              20261114-090000-00000001 agent           <t>  of 30m     $0.68+  example/lease: 3 model requests, the last answered <t> ago
                                       job 0           <t>  of 10m06s          train call on modal/a100_80 with unknown GPUs
                                       lease 0         <t>  of 1m              box of job 0 on aws/box, ready, 1s busy; put /pumpkin/in/payload to /work/in moved 1 entry, 1 byte
              20261114-090000-00000002 agent           <t>  of 30m     $0.68+  example/lease: 3 model requests, the last answered <t> ago
                                       job 0           <t>  of 10m06s          train call on modal/a100_80 with unknown GPUs
                                       lease 0         <t>  of 1m              box of job 0 on aws/box, ready, 5s busy; `sh -c set -e\nprint 7` exited 0
              20261114-090000-00000003 agent           <t>  of 30m     $0.71+  example/lease: 3 model requests, the last answered <t> ago
                                       lease 0         <t>                     box of job 0 on aws/box, closed as its call ended; its sandbox isn't gone yet; `sh -c set -e\nprint 7` exited 0
              20261114-090000-00000004 agent           <t>  of 30m     $0.71+  example/lease: 3 model requests, the last answered <t> ago
                                       lease 1         <t>  of 30m             gpu on modal/a100_80, starting
                                       lease 2         <t>  of 2m              cpu on aws/judge, starting
              20261114-090000-00000005 agent           <t>  of 30m     $0.72+  example/lease: 3 model requests, the last answered <t> ago
                                       lease 1         <t>  of 1m              gpu on modal/a100_80 with NVIDIA A100-SXM4-80GB, ready, 2s busy; `nvidia-smi` exited 0
                                       lease 2         <t>  of 2m              cpu on aws/judge, starting
              20261114-090000-00000006 agent           <t>  of 30m     $0.73+  example/lease: 3 model requests, the last answered <t> ago
                                       lease 1         <t>                     gpu on modal/a100_80 with NVIDIA A100-SXM4-80GB, closed by its holder; its sandbox isn't gone yet; `nvidia-smi` exited 0
                                       lease 2         <t>  of 2m              cpu on aws/judge, starting
              20261114-090000-00000007 agent           <t>  of 30m     $0.74+  example/lease: 3 model requests, the last answered <t> ago
                                       lease 2         <t>  of 30m             cpu on aws/judge, ready, 2s busy; `sleep 60` running
              20261114-090000-00000008 agent           <t>  of 30m     $0.94+  example/lease: 3 model requests, the last answered <t> ago
                                       lease 2         <t>  of 30m             cpu on aws/judge, ready, 30s busy; `sleep 60` timed out
                                       lease 3         <t>                     gpu on modal/a100_80 with NVIDIA A100 80GB PCIe (its role needs NVIDIA A100-SXM4-80GB), expired; its sandbox isn't gone yet; `true` exited 0
              20261114-090000-00000009 ending          <t>  of 1h43m   $1.03+  example/lease: the end of lease 2
                                       lease 2         <t>                     cpu on aws/judge, closed as the work phase ended; its sandbox isn't gone yet; `sleep 60` timed out
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
    let scratch = Scratch::new("says-unread-lines");
    let id = "20261110-110000-0000000b";
    let log = [
        logged(5, json!({ "kind": "agent_started" }), json!([])),
        logged(10, json!({ "kind": "later" }), json!([])),
        logged(20, json!({ "kind": "agent_exited" }), json!([])),
        logged(
            30,
            json!({ "kind": "model_answered", "caller": "agent", "upstream_status": "200",
                "answer": { "kind": "forwarded" } }),
            json!([]),
        ),
        logged(
            40,
            json!({ "kind": "tick" }),
            json!([{ "kind": "tear_down_agent" }, { "kind": "close_lease" }, { "kind": "finish" }]),
        ),
    ];
    let unjudged = json!({
        "state": "finished",
        "verdict": { "kind": "no_submission" }, "work_end": { "kind": "deadline" },
    });
    let dir = write_run(
        &scratch,
        id,
        asked(id, 90),
        &log,
        Some(unjudged),
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
            pumpkin: run 20261110-110000-0000000b: 4 parts of its log can't be read by this pumpkin, the first at line 4, its event: missing field `status`
            pumpkin: run 20261110-110000-0000000c: 4 parts of its log can't be read by this pumpkin, the first at line 1, its event: missing field `unix_ns`
            printed:
              20261110-110000-0000000b finished        40s  -               -  example/lease
              20261110-110000-0000000c failed            -  -               -  example/lease
              2 runs: 1 finished (1 without a result), 1 failed
            $ pumpkin wait --runs runs 20261110-110000-0000000b
            exit status: Some(3)
            pumpkin: run 20261110-110000-0000000b started <t> after it was asked for, and ran <t>:
                 <t> agent on aws/judge, started at <t>; the work phase ended at <t>
              model: 0 requests
              sandboxes: 1 on aws/judge, <t> in all, 1 created, all deleted
              4 parts of its log can't be read by this pumpkin, the first at line 4, its event: missing field `status`
            pumpkin: run 20261110-110000-0000000b finished without a result: the agent submitted nothing, and the work phase ended at its deadline; its record is <scratch>/runs/20261110-110000-0000000b
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            printed outcome.json: {"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"deadline"}}
            $ pumpkin wait --runs runs 20261110-110000-0000000c
            exit status: Some(1)
            pumpkin: run 20261110-110000-0000000c: 4 parts of its log can't be read by this pumpkin, the first at line 1, its event: missing field `unix_ns`
            pumpkin: run 20261110-110000-0000000c failed: the agent's sandbox didn't start; its record is <scratch>/runs/20261110-110000-0000000c
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs <scratch>/task.star
            printed outcome.json: {"reason":"the agent's sandbox didn't start","state":"failed"}"#]],
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
