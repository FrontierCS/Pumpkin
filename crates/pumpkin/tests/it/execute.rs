//! Executing a prepared run through the library's entry point, [`Run::execute`], on the
//! simulated provider, and on Docker where its sandbox makes ops through `sandboxctl`: what it
//! returns, its record, its run log, and what it leaves behind. The test on Docker is ignored,
//! as the Docker provider's are ([`docker`](crate::docker)).

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]

use std::ops::RangeInclusive;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{fs, iter};

use expect_test::{Expect, expect};
use pumpkin::driver::core::lasts;
use pumpkin::driver::run::Run;
use pumpkin_docker::Engine;
use pumpkin_provider::Provider;
use pumpkin_schema::ir;
use pumpkin_schema::provider::{Listed, RunTag};
use pumpkin_schema::sandbox::{SANDBOXCTL_PATH, command_stdout};
use pumpkin_schema::workflow::{Main, Workflow};
use serde_json::{Value, json};

use crate::docker;
use crate::run::{Runs, compact, files};
use crate::sim::{self, LINGERING, LOST, MUTE, SLOW, Sim, UNDELETABLE, UNPUBLISHED};

const LWE: &str = include_str!("../../../pumpkin-schema/tests/fixtures/lwe.json");

/// A `sandboxd` that dials.
const SANDBOXD: &str = "/sim/sandboxd";

/// The startup of a simulated provider whose tests wait for a start, or a teardown, to run out:
/// short, so that the wait takes seconds, but still far longer than a simulated sandbox takes
/// to start, so that a busy machine makes a start that succeeds wait longer, never fail.
const SHORT_STARTUP: u64 = 2;

/// A provider these tests execute runs on, and what its platform shows of a run that the
/// provider's calls don't.
trait Platform: Provider {
    fn shown(&self) -> Vec<String>;
}

impl Platform for Sim {
    /// The mode of the directory that held each create's socket directory.
    fn shown(&self) -> Vec<String> {
        let modes = self.modes().into_iter().map(|mode| format!("{mode:o}"));
        let modes: Vec<String> = modes.collect();
        vec![format!("their directory's mode at each create: {modes:?}")]
    }
}

impl Platform for Engine {
    fn shown(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Prepares `ir` as test `name` and executes it on `provider`, with `sandboxd`, and checks
/// what becomes of it: what `execute` returns, the record's files, the payloads it holds,
/// whether its `provider.json` is what the provider declared, its `outcome.json`, its log
/// ([`logged`]), when its last record came ([`when`]), whether `execute` returned once the hard
/// end had passed, if it did, the standard output of the agent and of each judge call, if
/// collected, whether the run's socket directories are left, what the platform shows
/// ([`Platform`]), and what the run's list finds once it has ended. The socket directories show
/// as `<sockets>`.
fn check(name: &str, provider: &dyn Platform, ir: &Value, sandboxd: &str, expect: &Expect) {
    let runs = Runs::new(name);
    let ir: ir::Workflow = serde_json::from_value(ir.clone()).unwrap();
    let run = Run::prepare(&ir, &runs.dir, SystemTime::now()).unwrap();
    let (record, tag) = (run.dir().to_owned(), RunTag::try_from(run.id().to_owned()));
    let sockets = std::env::temp_dir().join(format!("pumpkin-{}", run.id()));
    let before = unix_ns();
    let executed = run.execute(provider, Path::new(sandboxd));
    let started = before..=unix_ns();
    let mut lines = vec![match executed {
        Ok(outcome) => format!("finished, with score {:?}", outcome.score()),
        Err(reason) => format!("failed: {reason}"),
    }];
    lines.push(format!("record: {}", files(&record, &record).join(", ")));
    let payloads = fs::read_dir(record.join("payloads")).into_iter().flatten();
    let mut payloads: Vec<String> = payloads
        .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect();
    payloads.sort();
    lines.push(format!("payloads hold: {payloads:?}"));
    let read = |name| fs::read_to_string(record.join(name)).unwrap();
    let declared: Value = serde_json::from_str(&read("provider.json")).unwrap();
    let declared = declared == json!(provider.ready());
    lines.push(format!(
        "provider.json is what the provider declared: {declared}"
    ));
    lines.push(format!("outcome.json: {}", compact(&read("outcome.json"))));
    lines.push("log.jsonl:".to_owned());
    let (log, last) = logged(&record, &started);
    lines.extend(log);
    if let Some(last) = last {
        let last = when(&ir, provider, last);
        lines.push(format!("its last record came {last}"));
    }
    // By the whole call's time, which the run's clock starts within.
    if Duration::from_nanos(started.end() - started.start()) >= hard_end(&ir, provider) {
        lines.push("execute returned once the hard end had passed".to_owned());
    }
    lines.extend(standard_outputs(&record));
    lines.push(format!("socket directories left: {}", sockets.exists()));
    lines.extend(provider.shown());
    let listed = provider.list(&tag.unwrap()).unwrap();
    let listed: Vec<String> = (listed.into_iter())
        .map(|Listed { id }| id.as_str().to_owned())
        .collect();
    lines.push(format!("listed once it ended: {listed:?}"));
    let sockets = sockets.display().to_string();
    expect.assert_eq(&lines.join("\n").replace(&sockets, "<sockets>"));
}

/// The records of the run log in `record`, a run's, one per line, and the time of its last
/// record, if it has any. It leaves out the `status` ops answered, alone, while their job was
/// queued or running, whose number depends on timing, and numbers the rest, and their ops, in
/// turn. Each record's time is shown only if it is 0 or isn't a decimal string, the run's start
/// only if it isn't within `started`, the call to `execute`, and the time left in a reply as
/// `<s>`.
fn logged(record: &Path, started: &RangeInclusive<u64>) -> (Vec<String>, Option<Duration>) {
    let (mut lines, mut last, mut polls) = (Vec::new(), None, 0);
    // A run that fails before it starts has no log.
    let log = fs::read_to_string(record.join("log.jsonl")).unwrap_or_default();
    for (seq, line) in log.lines().enumerate() {
        let mut logged: Value = serde_json::from_str(line).unwrap();
        assert_eq!(logged["seq"], json!(seq), "{logged}");
        let at = Duration::from_nanos(nanos(&logged["at_ns"]).unwrap());
        last = last.max(Some(at));
        let actions = logged["actions"].as_array().map_or(0, Vec::len);
        let state = logged.pointer("/actions/0/reply/state");
        if logged["event"]["request"]["kind"] == "status"
            && actions == 1
            && state.is_some_and(|state| state == "queued" || state == "running")
        {
            polls += 1;
            continue;
        }
        logged["seq"] = json!(lines.len());
        let ops = iter::once("/event/op".to_owned())
            .chain((0..actions).map(|action| format!("/actions/{action}/op")));
        for op in ops {
            if let Some(op) = logged.pointer_mut(&op) {
                *op = json!(op.as_u64().unwrap() - polls);
            }
        }
        if !at.is_zero() {
            logged["at_ns"] = json!("<ns>");
        }
        if let Some(unix_ns) = logged.pointer_mut("/event/unix_ns")
            && nanos(unix_ns).is_some_and(|ns| started.contains(&ns))
        {
            *unix_ns = json!("<during execute>");
        }
        let actions = logged.get_mut("actions").and_then(Value::as_array_mut);
        for action in actions.into_iter().flatten() {
            if let Some(seconds) = action.pointer_mut("/reply/remaining_seconds") {
                *seconds = json!("<s>");
            }
        }
        lines.push(logged.to_string());
    }
    (lines, last)
}

/// The standard output that `record`, a run's, holds of the agent's command and of each judge
/// call's, in the order of the jobs' numbers.
fn standard_outputs(record: &Path) -> Vec<String> {
    let jobs = fs::read_dir(record.join("jobs")).into_iter().flatten();
    let job = |entry: fs::DirEntry| entry.file_name().into_string().unwrap().parse().unwrap();
    let mut jobs: Vec<u64> = jobs.map(|entry| job(entry.unwrap())).collect();
    jobs.sort_unstable();
    let jobs = (jobs.into_iter()).map(|job| (format!("job {job}'s"), format!("jobs/{job}")));
    let agent = ("the agent's".to_owned(), "agent".to_owned());
    let mut lines = Vec::new();
    for (whose, dir) in iter::once(agent).chain(jobs) {
        let stdout = record.join(dir).join(command_stdout().as_str());
        if let Ok(stdout) = fs::read_to_string(stdout) {
            lines.push(format!("{whose} standard output:"));
            lines.extend(stdout.lines().map(str::to_owned));
        }
    }
    lines
}

/// When `last`, a time since a run of `ir` on `provider` started, came: before the agent's
/// teardown was to end, at most the hard end ([`lasts`]) less the final judge's startup and
/// timeout; before the hard end; or after it.
fn when(ir: &ir::Workflow, provider: &dyn Provider, last: Duration) -> &'static str {
    let workflow = Workflow::parse(ir).unwrap();
    let Main::SoloThenJudge(pattern) = workflow.main();
    let startup = provider.ready().capabilities.startup_seconds.get();
    let hard_end = hard_end(ir, provider);
    let teardown_end =
        hard_end.saturating_sub(Duration::from_secs(startup) + pattern.score.timeout);
    if last < teardown_end {
        "before the agent's teardown was to end"
    } else if last < hard_end {
        "once the agent's teardown was to end, before the hard end"
    } else {
        "once the hard end had passed"
    }
}

/// The hard end ([`lasts`]) of a run of `ir` on `provider`.
fn hard_end(ir: &ir::Workflow, provider: &dyn Provider) -> Duration {
    let startup = provider.ready().capabilities.startup_seconds.get();
    lasts(&Workflow::parse(ir).unwrap(), Duration::from_secs(startup))
}

/// The fixture's workflow, without the agent's model route, which the driver refuses, and with
/// `argv` as the agent's command.
fn workflow(argv: &[&str]) -> Value {
    let mut lwe: Value = serde_json::from_str(LWE).unwrap();
    let agent = &mut lwe["roles"][0];
    agent.as_object_mut().unwrap().remove("egress");
    agent["runtime"]["agent"]["command"]["argv"] = json!(argv);
    lwe
}

/// The workflow of the runs whose agents make ops: [`workflow`]'s, with `argv` as the agent's
/// command and `deadline` as the pattern's deadline, in seconds; both judges judging the payload
/// in their slot as the simulated provider's `judge` command does; room in the train judge's
/// slot for 8 bytes and in the score's for 16, the most an op of the agent's may carry; and a
/// call timeout of 1 s, shorter than the time left in any run that makes its calls soon after it
/// starts, without a cumulative limit, so that it is each call's timeout, and what a reply has
/// left depends on no command's time.
fn ops_workflow(argv: &[&str], deadline: u64) -> Value {
    let mut ir = workflow(argv);
    let grant = &mut ir["main"]["solo_then_judge"]["grants"][0]["judge"];
    grant["call_timeout_seconds"] = json!(1);
    grant.as_object_mut().unwrap().remove("cumulative_seconds");
    for role in [1, 2] {
        let judge = &mut ir["roles"][role]["runtime"]["judge"];
        judge["command"]["argv"] = json!(["judge", "/pumpkin/in/payload/raw"]);
    }
    ir["roles"][1]["runtime"]["judge"]["slots"][0]["max_bytes"] = json!(8);
    ir["roles"][2]["runtime"]["judge"]["slots"][0]["max_bytes"] = json!(16);
    ir["main"]["solo_then_judge"]["deadline_seconds"] = json!(deadline);
    ir
}

/// [`ops_workflow`]'s workflow, with the run's other shares of its hard end as short as they
/// go: no finalize grace, and a score timeout of 1 s, so that a run that waits for its
/// teardown's end or its hard end waits seconds.
fn short_workflow(argv: &[&str], deadline: u64) -> Value {
    let mut ir = ops_workflow(argv, deadline);
    let pattern = &mut ir["main"]["solo_then_judge"];
    pattern["finalize_grace_seconds"] = json!(0);
    pattern["score"]["timeout_seconds"] = json!(1);
    ir
}

/// A run whose agent's command exits: its exit ends the work phase without a submission, and
/// the agent's sandbox is collected and deleted, so the run finishes without a score.
#[test]
fn exits() {
    // A deadline far later than the exit, so that an exit the run misses ends the work phase.
    let mut ir = workflow(&["exit", "3"]);
    ir["main"]["solo_then_judge"]["deadline_seconds"] = json!(30);
    check(
        "exits",
        &Sim::new(),
        &ir,
        SANDBOXD,
        &expect![[r#"
            finished, with score None
            record: agent/log/collected, agent/out/collected, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            payloads hold: []
            provider.json is what the provider declared: true
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":3}}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"kind":"run_started","unix_ns":"<during execute>"},"seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"seq":1}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":3}}}}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":3}},"seq":2}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":3}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: []"#]],
    );
}

/// A run whose agent's `sandboxd` ends the session once the agent's command has started: the
/// agent fails, and its sandbox is deleted.
#[test]
fn crashes() {
    // A deadline far later than the session's end, so that an end the run misses ends the work
    // phase.
    let mut ir = workflow(&["crash"]);
    ir["main"]["solo_then_judge"]["deadline_seconds"] = json!(30);
    check(
        "crashes",
        &Sim::new(),
        &ir,
        SANDBOXD,
        &expect![[r#"
            finished, with score None
            record: agent/, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            payloads hold: []
            provider.json is what the provider declared: true
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"kind":"run_started","unix_ns":"<during execute>"},"seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"seq":1}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"}}}],"at_ns":"<ns>","event":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"},"seq":2}
            {"at_ns":"<ns>","note":"freezing the agent's sandbox failed: the sandbox ended the session","seq":3}
            {"at_ns":"<ns>","note":"the agent's out is incomplete: collecting /pumpkin/out failed: the sandbox ended the session","seq":4}
            {"at_ns":"<ns>","note":"the agent's log is incomplete: collecting /pumpkin/log failed: the sandbox ended the session","seq":5}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, nothing; log, nothing","seq":6}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: []"#]],
    );
}

/// A run whose agent's sandbox stalls once its command has started, and whose command never
/// exits: the deadline ends the work phase, and freezing the sandbox runs out at the
/// teardown's end, which leaves the final judge its startup and timeout before the hard end;
/// what couldn't be collected is noted, and the sandbox is deleted.
#[test]
fn stalls() {
    let mut ir = workflow(&["stall"]);
    let pattern = &mut ir["main"]["solo_then_judge"];
    pattern["deadline_seconds"] = json!(1);
    pattern["finalize_grace_seconds"] = json!(0);
    check(
        "stalls",
        &Sim::with_startup(SHORT_STARTUP),
        &ir,
        SANDBOXD,
        &expect![[r#"
            finished, with score None
            record: agent/, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            payloads hold: []
            provider.json is what the provider declared: true
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"deadline"}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"kind":"run_started","unix_ns":"<during execute>"},"seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"seq":1}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"deadline"}}}],"at_ns":"<ns>","event":{"kind":"tick"},"seq":2}
            {"at_ns":"<ns>","note":"freezing the agent's sandbox failed: the sandbox didn't answer before the request's deadline: expected its answer while the deadline lasts","seq":3}
            {"at_ns":"<ns>","note":"the agent's out is incomplete: collecting /pumpkin/out failed: the sandbox didn't answer before the request's deadline: expected its answer while the deadline lasts","seq":4}
            {"at_ns":"<ns>","note":"the agent's log is incomplete: collecting /pumpkin/log failed: the sandbox didn't answer before the request's deadline: expected its answer while the deadline lasts","seq":5}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, nothing; log, nothing","seq":6}
            its last record came once the agent's teardown was to end, before the hard end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: []"#]],
    );
}

/// A run whose agent's `sandboxd` never dials: its start runs out at the provider's startup,
/// and the sandbox is deleted, with its console quoted in the agent's failure. The start
/// outlasts the deadline, so the core hears of the failure before the deadline's tick, which
/// ends the work phase as the failure is taken.
#[test]
fn unstarted() {
    let mut ir = workflow(&["exit", "0"]);
    ir["main"]["solo_then_judge"]["deadline_seconds"] = json!(1);
    check(
        "unstarted",
        &Sim::with_startup(SHORT_STARTUP),
        &ir,
        MUTE,
        &expect![[r#"
            finished, with score None
            record: ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            payloads hold: []
            provider.json is what the provider declared: true
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"deadline"}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"kind":"run_started","unix_ns":"<during execute>"},"seq":0}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"deadline"}}}],"at_ns":"<ns>","event":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: sandbox sim-1: no sandbox connected to <sockets>/agent/s by the deadline: expected its sandbox to connect while the deadline lasts; its console ends: sandboxd: a simulated one, which never dials"},"seq":1}
            {"at_ns":"<ns>","note":"the agent's sandbox never started: nothing was collected","seq":2}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: []"#]],
    );
}

/// A run whose agent's create fails, leaving behind a sandbox it can't name: the agent fails,
/// and the run's end deletes the sandbox, which the run's list still finds.
#[test]
fn lost() {
    check(
        "lost",
        &Sim::new(),
        &workflow(&["exit", "0"]),
        LOST,
        &expect![[r#"
            finished, with score None
            record: ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            payloads hold: []
            provider.json is what the provider declared: true
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: the simulated create failed, and can't name what it made: expected the run's list to find it"}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"kind":"run_started","unix_ns":"<during execute>"},"seq":0}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: the simulated create failed, and can't name what it made: expected the run's list to find it"}}}],"at_ns":"<ns>","event":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: the simulated create failed, and can't name what it made: expected the run's list to find it"},"seq":1}
            {"at_ns":"<ns>","note":"the agent's sandbox never started: nothing was collected","seq":2}
            {"at_ns":"<ns>","note":"deleted the sandboxes the run left: sim-1","seq":3}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: []"#]],
    );
}

/// A run whose agent's sandbox can't be deleted: its teardown fails the run, whose end tries to
/// delete it again and fails too, so the run fails with both reasons.
#[test]
fn undeletable() {
    check(
        "undeletable",
        &Sim::new(),
        &workflow(&["exit", "0"]),
        UNDELETABLE,
        &expect![[r#"
            failed: deleting sandbox sim-1 failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`
            record: agent/log/collected, agent/out/collected, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            payloads hold: []
            provider.json is what the provider declared: true
            outcome.json: {"reason":"deleting sandbox sim-1 failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`","state":"failed"}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"kind":"run_started","unix_ns":"<during execute>"},"seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"seq":1}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"seq":2}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":3}
            {"at_ns":"<ns>","note":"the run failed: deleting sandbox sim-1 failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left failed: sim-1 can't be deleted by the simulated provider: create one with another `sandboxd`","seq":4}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: ["sim-1"]"#]],
    );
}

/// A run whose `sandboxd` the provider can't publish: the run fails before it starts, and
/// nothing is created.
#[test]
fn unpublished() {
    check(
        "unpublished",
        &Sim::new(),
        &workflow(&["exit", "0"]),
        UNPUBLISHED,
        &expect![[r#"
            failed: publishing what the run's sandboxes run failed: /sim/unpublished can't be published by the simulated provider: name another path
            record: ir.json, outcome.json, provider.json, record.json
            payloads hold: []
            provider.json is what the provider declared: true
            outcome.json: {"reason":"publishing what the run's sandboxes run failed: /sim/unpublished can't be published by the simulated provider: name another path","state":"failed"}
            log.jsonl:
            socket directories left: false
            their directory's mode at each create: []
            listed once it ended: []"#]],
    );
}

/// A run whose agent makes ops: `ops`; `status` of a job it doesn't have; a judge call to a
/// grant it doesn't hold; one with a payload over the grant's slot, which the core refuses; one
/// over the most an op may carry, which the gateway refuses, without the core; one the core
/// accepts, whose judge passes it; one whose judge runs past its call's timeout; and a final
/// submission, which the final judge scores. The agent awaits each call's end by `status`, and
/// every reply reaches it, since it makes each op once the one before it is answered, and exits
/// once the last is; the record keeps the accepted payloads alone, and what each call's sandbox
/// held.
#[test]
fn ops() {
    let argv = [
        "ops",
        "ops",
        "status 0",
        "judge nope 6",
        "judge train 9",
        "judge train 17",
        "judge train 6",
        "await 0 passed",
        "judge train slow",
        "await 1 failed",
        "finalize submit 6",
    ];
    // A deadline far later than the agent's last op, so that a reply the agent misses ends the
    // work phase.
    check(
        "ops",
        &Sim::new(),
        &ops_workflow(&argv, 30),
        SANDBOXD,
        &expect![[r#"
            finished, with score Some(Number(6))
            record: agent/log/collected, agent/out/collected, ir.json, jobs/0/log/collected, jobs/0/out/collected, jobs/0/out/result.json, jobs/1/log/collected, jobs/1/out/collected, jobs/2/log/collected, jobs/2/out/collected, jobs/2/out/result.json, log.jsonl, outcome.json, payloads/5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6, payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd, provider.json, record.json
            payloads hold: ["slow", "xxxxxx"]
            provider.json is what the provider declared: true
            outcome.json: {"score":6,"state":"finished","verdict":{"kind":"judged","result":{"passed":true,"returned":{"raw_score":6},"score":6},"submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"kind":"run_started","unix_ns":"<during execute>"},"seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"grants":[{"call_timeout_seconds":1,"calls":100,"cumulative_seconds":null,"kind":"judge","max_bytes":8,"name":"train"},{"kind":"submit","max_bytes":16,"name":"submit"}],"kind":"ops","remaining_seconds":"<s>","resources":{"cpu_millis":8000,"memory_bytes":34359738368}}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":0,"request":{"kind":"ops"}},"seq":2}
            {"actions":[{"kind":"reply","op":1,"reply":{"kind":"refused","message":"you have no job 0: ask about a job that one of your calls returned"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":1,"request":{"job":"0","kind":"status"}},"seq":3}
            {"actions":[{"kind":"reply","op":2,"reply":{"kind":"refused","message":"you hold no grant named `nope`: call one that `ops` lists"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":2,"request":{"grant":"nope","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"seq":4}
            {"actions":[{"kind":"reply","op":3,"reply":{"kind":"refused","message":"the payload is 9 bytes, over `train`'s limit of 8: send a smaller one"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":3,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:a73add1aecea03a894ca65d99bc5e91d21b58cdb728343bbcb6cf6cf38b809f5","size":9}}},"seq":5}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"op_refused","message":"the payload is 17 bytes, more than the limit of 16: send at most 16","request":{"grant":"train","kind":"judge","payload":{"over":16,"size":17}}},"seq":6}
            {"actions":[{"kind":"reply","op":4,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":4,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"seq":7}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"seq":8}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_exited","status":{"code":0}},"seq":9}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_collected","result":{"passed":true,"returned":{"raw_score":6},"score":6}},"seq":10}
            {"actions":[{"kind":"reply","op":5,"reply":{"job":0,"kind":"status","remaining":{"calls":99,"seconds":null},"returned":{"raw_score":6},"state":"passed"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":5,"request":{"job":"0","kind":"status"}},"seq":11}
            {"actions":[{"kind":"reply","op":6,"reply":{"job":1,"kind":"judge","remaining":{"calls":98,"seconds":null}}},{"grant":"train","job":1,"kind":"start_judge","payload":{"digest":"sha256:5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6","size":4},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":6,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6","size":4}}},"seq":12}
            {"actions":[],"at_ns":"<ns>","event":{"job":1,"kind":"judge_started"},"seq":13}
            {"actions":[],"at_ns":"<ns>","event":{"job":1,"kind":"judge_timed_out"},"seq":14}
            {"actions":[],"at_ns":"<ns>","event":{"invalid":"the judge wrote no result: write it at /pumpkin/out/result.json","job":1,"kind":"judge_collected"},"seq":15}
            {"actions":[{"kind":"reply","op":7,"reply":{"failure":{"code":"judge_timeout","message":"the judge ran past its timeout"},"job":1,"kind":"status","remaining":{"calls":98,"seconds":null},"state":"failed"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":7,"request":{"job":"1","kind":"status"}},"seq":16}
            {"actions":[{"kind":"reply","op":8,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"accepted"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":8,"request":{"finalize":true,"grant":"submit","kind":"submit","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"seq":17}
            {"actions":[{"kind":"tear_down_agent"},{"grant":null,"job":2,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"120000000000"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"seq":18}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":19}
            {"actions":[],"at_ns":"<ns>","event":{"job":2,"kind":"judge_started"},"seq":20}
            {"actions":[],"at_ns":"<ns>","event":{"job":2,"kind":"judge_exited","status":{"code":0}},"seq":21}
            {"actions":[{"kind":"finish","outcome":{"score":6,"verdict":{"kind":"judged","result":{"passed":true,"returned":{"raw_score":6},"score":6},"submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"job":2,"kind":"judge_collected","result":{"passed":true,"returned":{"raw_score":6},"score":6}},"seq":22}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700", "700", "700", "700"]
            listed once it ended: []"#]],
    );
}

/// A run whose agent finalizes a submission whose final judge writes a result over the most a
/// result may hold: the result is removed from the record, and the run finishes with no score.
#[test]
fn result_over_the_limit() {
    check(
        "result-over-the-limit",
        &Sim::new(),
        &ops_workflow(&["ops", "finalize submit large"], 30),
        SANDBOXD,
        &expect![[r#"
            finished, with score None
            record: agent/log/collected, agent/out/collected, ir.json, jobs/0/log/collected, jobs/0/out/collected, log.jsonl, outcome.json, payloads/d35c416a85b807e9b5384915d6ebb4a9f7352713efd89857b45a242f473728a9, provider.json, record.json
            payloads hold: ["large"]
            provider.json is what the provider declared: true
            outcome.json: {"score":null,"state":"finished","verdict":{"failure":{"kind":"invalid_result","reason":"result.json is 1048577 bytes, over the limit of 1048576: write a smaller one"},"kind":"no_result","submission":{"finalized":true,"payload":{"digest":"sha256:d35c416a85b807e9b5384915d6ebb4a9f7352713efd89857b45a242f473728a9","size":5}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"kind":"run_started","unix_ns":"<during execute>"},"seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"accepted"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":0,"request":{"finalize":true,"grant":"submit","kind":"submit","payload":{"digest":"sha256:d35c416a85b807e9b5384915d6ebb4a9f7352713efd89857b45a242f473728a9","size":5}}},"seq":2}
            {"actions":[{"kind":"tear_down_agent"},{"grant":null,"job":0,"kind":"start_judge","payload":{"digest":"sha256:d35c416a85b807e9b5384915d6ebb4a9f7352713efd89857b45a242f473728a9","size":5},"timeout_ns":"120000000000"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"seq":3}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":4}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"seq":5}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_exited","status":{"code":0}},"seq":6}
            {"actions":[{"kind":"finish","outcome":{"score":null,"verdict":{"failure":{"kind":"invalid_result","reason":"result.json is 1048577 bytes, over the limit of 1048576: write a smaller one"},"kind":"no_result","submission":{"finalized":true,"payload":{"digest":"sha256:d35c416a85b807e9b5384915d6ebb4a9f7352713efd89857b45a242f473728a9","size":5}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"invalid":"result.json is 1048577 bytes, over the limit of 1048576: write a smaller one","job":0,"kind":"judge_collected"},"seq":7}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700", "700"]
            listed once it ended: []"#]],
    );
}

/// A run whose agent's `sandboxd` ends the session while the agent's judge call is in flight,
/// its payload over the score's slot but within the judge's, the larger, so within the most an
/// op may carry: the op arrived whole, so the core takes it, and accepts it, before the
/// session's end, which fails the agent; and the reply can't be sent, which the run log notes.
/// The call's sandbox takes seconds to create, so that the agent's end has stopped the call by
/// the time its thread looks, before the judge's command starts.
#[test]
fn crashes_with_an_op_in_flight() {
    let mut ir = ops_workflow(&["ops", "judge train 12", "crash"], 30);
    ir["roles"][1]["image"] = json!(sim::image(SLOW).as_str());
    ir["roles"][1]["runtime"]["judge"]["slots"][0]["max_bytes"] = json!(16);
    ir["roles"][2]["runtime"]["judge"]["slots"][0]["max_bytes"] = json!(8);
    check(
        "crashes-with-an-op",
        &Sim::new(),
        &ir,
        SANDBOXD,
        &expect![[r#"
            finished, with score None
            record: agent/, ir.json, jobs/0/, log.jsonl, outcome.json, payloads/59ffe12a70df15109e0345955e3230a978f31ebc28d8fe3e42d306afb28b8e81, provider.json, record.json
            payloads hold: ["xxxxxxxxxxxx"]
            provider.json is what the provider declared: true
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"kind":"run_started","unix_ns":"<during execute>"},"seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:59ffe12a70df15109e0345955e3230a978f31ebc28d8fe3e42d306afb28b8e81","size":12},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:59ffe12a70df15109e0345955e3230a978f31ebc28d8fe3e42d306afb28b8e81","size":12}}},"seq":2}
            {"at_ns":"<ns>","note":"op 0's reply failed: the sandbox ended the session","seq":3}
            {"actions":[{"job":0,"kind":"stop_judge"},{"kind":"tear_down_agent"}],"at_ns":"<ns>","event":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"},"seq":4}
            {"at_ns":"<ns>","note":"freezing the agent's sandbox failed: the sandbox ended the session","seq":5}
            {"at_ns":"<ns>","note":"the agent's out is incomplete: collecting /pumpkin/out failed: the sandbox ended the session","seq":6}
            {"at_ns":"<ns>","note":"the agent's log is incomplete: collecting /pumpkin/log failed: the sandbox ended the session","seq":7}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, nothing; log, nothing","seq":8}
            {"actions":[{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's session ended: the sandbox ended the session"}}}],"at_ns":"<ns>","event":{"job":0,"kind":"judge_failed","reason":"the call was stopped before its command started"},"seq":9}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700", "700"]
            listed once it ended: []"#]],
    );
}

/// A run whose agent makes a judge call, then exits, and whose judge's sandbox takes seconds to
/// create, past the deadline: an agent's call starts by the work phase's end, which comes by the
/// deadline, however long the provider's startup, so the sandbox's set-up fails, and the run
/// finishes without a submission.
#[test]
fn judge_starts_past_the_deadline() {
    // A deadline that the create, which begins once the agent has started, outlasts, and that
    // leaves the call its whole timeout.
    let mut ir = ops_workflow(&["ops", "judge train 6", "exit"], 2);
    ir["roles"][1]["image"] = json!(sim::image(SLOW).as_str());
    check(
        "judge-starts-past-the-deadline",
        &Sim::new(),
        &ir,
        SANDBOXD,
        &expect![[r#"
            finished, with score None
            record: agent/log/collected, agent/out/collected, ir.json, jobs/0/, log.jsonl, outcome.json, payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd, provider.json, record.json
            payloads hold: ["xxxxxx"]
            provider.json is what the provider declared: true
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"kind":"run_started","unix_ns":"<during execute>"},"seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"seq":2}
            {"actions":[{"job":0,"kind":"stop_judge"},{"kind":"tear_down_agent"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"seq":3}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":4}
            {"actions":[{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"job":0,"kind":"judge_failed","reason":"the judge's sandbox didn't start: sandbox sim-2: the directories under `/pumpkin` can't be put in the sandbox: the request's deadline passed before it was sent: expected time left to send it"},"seq":5}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700", "700"]
            listed once it ended: []"#]],
    );
}

/// A run whose train judge stalls its sandbox once its command has started, which the agent
/// awaits before it finalizes: the work phase ends at once, so the core stops the call, whose
/// sandbox's freeze runs out at the agent's teardown's end, which leaves the final judge its
/// startup and timeout before the hard end. The final judge scores the submission, but its
/// sandbox can't be deleted, which fails the run, and the run's end can't delete it either.
#[test]
fn judge_stalls() {
    let argv = [
        "ops",
        "judge train stall",
        "await 0 running",
        "finalize submit 6",
    ];
    // A deadline seconds after the agent's ops, so that the teardown's end, which follows it,
    // comes soon; and a call timeout seconds longer than the agent takes to finalize, so that
    // the stop ends the call, and seconds shorter than the time left when the call is made, so
    // that it is the call's timeout.
    let mut ir = short_workflow(&argv, 4);
    ir["main"]["solo_then_judge"]["grants"][0]["judge"]["call_timeout_seconds"] = json!(2);
    ir["roles"][2]["image"] = json!(sim::image(UNDELETABLE).as_str());
    check(
        "judge-stalls",
        &Sim::with_startup(SHORT_STARTUP),
        &ir,
        SANDBOXD,
        &expect![[r#"
            failed: deleting sandbox sim-3 failed: sim-3 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left failed: sim-3 can't be deleted by the simulated provider: create one with another `sandboxd`
            record: agent/log/collected, agent/out/collected, ir.json, jobs/0/, jobs/1/log/collected, jobs/1/out/collected, jobs/1/out/result.json, log.jsonl, outcome.json, payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd, payloads/e5b6a7364e8a1049a9236dacfa24768ccffd3c4b671dc8b02953625844789b97, provider.json, record.json
            payloads hold: ["stall", "xxxxxx"]
            provider.json is what the provider declared: true
            outcome.json: {"reason":"deleting sandbox sim-3 failed: sim-3 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left failed: sim-3 can't be deleted by the simulated provider: create one with another `sandboxd`","state":"failed"}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"kind":"run_started","unix_ns":"<during execute>"},"seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:e5b6a7364e8a1049a9236dacfa24768ccffd3c4b671dc8b02953625844789b97","size":5},"timeout_ns":"2000000000"}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:e5b6a7364e8a1049a9236dacfa24768ccffd3c4b671dc8b02953625844789b97","size":5}}},"seq":2}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"seq":3}
            {"actions":[{"kind":"reply","op":1,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"accepted"}},{"job":0,"kind":"stop_judge"},{"kind":"tear_down_agent"}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":1,"request":{"finalize":true,"grant":"submit","kind":"submit","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"seq":4}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":5}
            {"actions":[{"grant":null,"job":1,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"job":0,"kind":"judge_failed","reason":"freezing the judge's sandbox failed: the sandbox didn't answer before the request's deadline: expected its answer while the deadline lasts"},"seq":6}
            {"actions":[],"at_ns":"<ns>","event":{"job":1,"kind":"judge_started"},"seq":7}
            {"actions":[],"at_ns":"<ns>","event":{"job":1,"kind":"judge_exited","status":{"code":0}},"seq":8}
            {"at_ns":"<ns>","note":"the run failed: deleting sandbox sim-3 failed: sim-3 can't be deleted by the simulated provider: create one with another `sandboxd`; deleting the sandboxes the run left failed: sim-3 can't be deleted by the simulated provider: create one with another `sandboxd`","seq":9}
            its last record came once the agent's teardown was to end, before the hard end
            socket directories left: false
            their directory's mode at each create: ["700", "700", "700"]
            listed once it ended: ["sim-3"]"#]],
    );
}

/// A run whose agent exits while its judge call runs, so that the core stops the call, whose
/// sandbox's delete outlasts the run's hard end: the core still waits on the call when the hard
/// end comes, so the run is abandoned there, and fails once the call's thread has ended.
#[test]
fn abandoned() {
    let argv = ["ops", "judge train slow", "await 0 running", "exit"];
    // A deadline later than the agent's ops, so that the agent's exit ends the work phase.
    let mut ir = short_workflow(&argv, 2);
    ir["roles"][1]["image"] = json!(sim::image(LINGERING).as_str());
    check(
        "abandoned",
        &Sim::with_startup(SHORT_STARTUP),
        &ir,
        SANDBOXD,
        &expect![[r#"
            failed: the run was still going at its hard end, 7 s after it started: expected it to end within its deadline, its finalize grace, its final judge's timeout and its sandboxes' startups
            record: agent/log/collected, agent/out/collected, ir.json, jobs/0/log/collected, jobs/0/out/collected, log.jsonl, outcome.json, payloads/5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6, provider.json, record.json
            payloads hold: ["slow"]
            provider.json is what the provider declared: true
            outcome.json: {"reason":"the run was still going at its hard end, 7 s after it started: expected it to end within its deadline, its finalize grace, its final judge's timeout and its sandboxes' startups","state":"failed"}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"kind":"run_started","unix_ns":"<during execute>"},"seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6","size":4},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6","size":4}}},"seq":2}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"seq":3}
            {"actions":[{"job":0,"kind":"stop_judge"},{"kind":"tear_down_agent"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"seq":4}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":5}
            {"actions":[{"kind":"abandon"}],"at_ns":"<ns>","event":{"kind":"tick"},"seq":6}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_failed","reason":"the call was stopped"},"seq":7}
            {"at_ns":"<ns>","note":"the run failed: the run was still going at its hard end, 7 s after it started: expected it to end within its deadline, its finalize grace, its final judge's timeout and its sandboxes' startups","seq":8}
            its last record came once the hard end had passed
            execute returned once the hard end had passed
            socket directories left: false
            their directory's mode at each create: ["700", "700"]
            listed once it ended: []"#]],
    );
}

/// A run whose agent's judge call fails once its start runs out, since its judge never dials,
/// and whose agent's sandbox's delete, as the agent exits, outlasts the run's hard end, so that
/// the loop takes the call's last report only after it. The report came before the hard end,
/// at the time it carries, so it ends the call in time, and the run finishes without a
/// submission rather than be abandoned.
#[test]
fn reported_before_the_hard_end() {
    // A deadline later than the agent's ops, so that the agent's exit ends the work phase, and
    // bounds the judge's start.
    let mut ir = short_workflow(&["ops", "judge train 6", "exit"], 2);
    ir["roles"][0]["image"] = json!(sim::image(LINGERING).as_str());
    ir["roles"][1]["image"] = json!(sim::image(MUTE).as_str());
    check(
        "reported-before-the-hard-end",
        &Sim::with_startup(SHORT_STARTUP),
        &ir,
        SANDBOXD,
        &expect![[r#"
            finished, with score None
            record: agent/log/collected, agent/out/collected, ir.json, jobs/0/, log.jsonl, outcome.json, payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd, provider.json, record.json
            payloads hold: ["xxxxxx"]
            provider.json is what the provider declared: true
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"kind":"run_started","unix_ns":"<during execute>"},"seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":0,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"seq":2}
            {"actions":[{"job":0,"kind":"stop_judge"},{"kind":"tear_down_agent"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"seq":3}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":4}
            {"actions":[{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"job":0,"kind":"judge_failed","reason":"the judge's sandbox didn't start: sandbox sim-2: no sandbox connected to <sockets>/job-0/s by the deadline: expected its sandbox to connect while the deadline lasts; its console ends: sandboxd: a simulated one, which never dials"},"seq":5}
            its last record came before the agent's teardown was to end
            execute returned once the hard end had passed
            socket directories left: false
            their directory's mode at each create: ["700", "700"]
            listed once it ended: []"#]],
    );
}

/// A run whose agent finalizes a submission, and whose agent's sandbox's delete, in the teardown
/// that follows, outlasts the run's hard end: the final judge's start is due by then, so its
/// call fails with no sandbox created, and the run is abandoned.
#[test]
fn torn_down_past_the_hard_end() {
    check(
        "torn-down-past-the-hard-end",
        &Sim::with_startup(SHORT_STARTUP),
        &short_workflow(&["ops", "finalize submit 6"], 2),
        LINGERING,
        &expect![[r#"
            failed: the run was still going at its hard end, 7 s after it started: expected it to end within its deadline, its finalize grace, its final judge's timeout and its sandboxes' startups
            record: agent/log/collected, agent/out/collected, ir.json, log.jsonl, outcome.json, payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd, provider.json, record.json
            payloads hold: ["xxxxxx"]
            provider.json is what the provider declared: true
            outcome.json: {"reason":"the run was still going at its hard end, 7 s after it started: expected it to end within its deadline, its finalize grace, its final judge's timeout and its sandboxes' startups","state":"failed"}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"kind":"run_started","unix_ns":"<during execute>"},"seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"accepted"}},{"kind":"tear_down_agent"},{"grant":null,"job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":0,"request":{"finalize":true,"grant":"submit","kind":"submit","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"seq":2}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":3}
            {"actions":[{"kind":"abandon"}],"at_ns":"<ns>","event":{"job":0,"kind":"judge_failed","reason":"the judge's sandbox wasn't created: the call's start was due by a time that had passed"},"seq":4}
            {"at_ns":"<ns>","note":"the run failed: the run was still going at its hard end, 7 s after it started: expected it to end within its deadline, its finalize grace, its final judge's timeout and its sandboxes' startups","seq":5}
            its last record came once the hard end had passed
            execute returned once the hard end had passed
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: []"#]],
    );
}

/// A run whose agent waits 2 s once its first `ops` is answered before it makes its second,
/// timed as the loop takes it, so that its reply leaves at least 2 s less than the first's,
/// however long the agent took to start, which the agent checks, exiting with 1 if not. It then
/// exits, and its sandbox makes a judge call after the exit, as a process the agent left would:
/// the call arrives once the core has finished the run, so the core refuses it, and the refusal
/// can't be sent, which the run log notes.
#[test]
fn late_ops() {
    let argv = [
        "ops",
        "ops",
        "sleep 2",
        "ops at least 2 less",
        "exit",
        "judge train 6",
    ];
    check(
        "late-ops",
        &Sim::new(),
        &ops_workflow(&argv, 30),
        SANDBOXD,
        &expect![[r#"
            finished, with score None
            record: agent/log/collected, agent/out/collected, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            payloads hold: []
            provider.json is what the provider declared: true
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"kind":"run_started","unix_ns":"<during execute>"},"seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"grants":[{"call_timeout_seconds":1,"calls":100,"cumulative_seconds":null,"kind":"judge","max_bytes":8,"name":"train"},{"kind":"submit","max_bytes":16,"name":"submit"}],"kind":"ops","remaining_seconds":"<s>","resources":{"cpu_millis":8000,"memory_bytes":34359738368}}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":0,"request":{"kind":"ops"}},"seq":2}
            {"actions":[{"kind":"reply","op":1,"reply":{"grants":[{"call_timeout_seconds":1,"calls":100,"cumulative_seconds":null,"kind":"judge","max_bytes":8,"name":"train"},{"kind":"submit","max_bytes":16,"name":"submit"}],"kind":"ops","remaining_seconds":"<s>","resources":{"cpu_millis":8000,"memory_bytes":34359738368}}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":1,"request":{"kind":"ops"}},"seq":3}
            {"actions":[{"kind":"tear_down_agent"},{"kind":"finish","outcome":{"score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"seq":4}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 1 file of 9 bytes; log, 1 file of 9 bytes","seq":5}
            {"actions":[{"kind":"reply","op":2,"reply":{"kind":"refused","message":"the work phase has ended: make no more ops"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":2,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"seq":6}
            {"at_ns":"<ns>","note":"op 2's reply wasn't sent: the agent's sandbox is gone","seq":7}
            its last record came before the agent's teardown was to end
            socket directories left: false
            their directory's mode at each create: ["700"]
            listed once it ended: []"#]],
    );
}

/// What the judges of the run on Docker do with the payload in their slot, as the simulated
/// provider's `judge` command does: for `slow`, run past any call's timeout; for any other,
/// write a passed result whose `raw_score` is the payload's size, and evidence, and print who
/// judged it, and where.
const JUDGE: &str = r#"
    payload=/pumpkin/in/payload/raw
    if [ "$(cat $payload)" = slow ]; then sleep 300; fi
    bytes=$(wc -c < $payload)
    printf '{"status": "passed", "raw_score": %s}\n' $bytes > /pumpkin/out/result.json
    echo kept > /pumpkin/out/evidence/evidence.txt
    echo "judged $bytes bytes as $(id -u), with HOME=$HOME, in $(pwd)""#;

/// What the agent of the run on Docker does: [`ops`]'s agent's ops, through `sandboxctl`, whose
/// path stands as `SANDBOXCTL`, printing the reply each got; `sandboxctl` exits with 1 for a
/// refusal and 2 for a job that failed.
const AGENT: &str = r#"
    x() { head -c "$1" /dev/zero | tr '\0' x > "$2"; }
    x 6 six && x 9 nine && x 17 seventeen && printf slow > slow
    ctl() {
        out=$(SANDBOXCTL "$@" 2>&1)
        echo "sandboxctl $*: exit $?: $out" | sed 's/"remaining_seconds":[0-9]*/"remaining_seconds":"<s>"/'
    }
    # Waits while job $1 is queued or running, then shows its status.
    await() {
        while SANDBOXCTL status "$1" | grep -qE '"state":"(queued|running)"'; do
            sleep 1
        done
        ctl status "$1"
    }
    ctl ops
    ctl status 0
    ctl judge nope six
    ctl judge train nine
    ctl judge train seventeen
    ctl judge train six
    await 0
    ctl judge train slow
    await 1
    ctl submit submit six --finalize"#;

/// The ops of [`ops`]'s agent made on Docker, as [`AGENT`] says, and its judges' calls, as
/// [`JUDGE`] says.
#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn ops_on_docker() {
    // Spliced in, since `format!` would need each of the shell's braces doubled.
    let script = AGENT.replace("SANDBOXCTL", SANDBOXCTL_PATH);
    // A deadline far later than the agent's last op, so that a slow engine makes the run wait
    // longer, never end differently.
    let mut ir = ops_workflow(&["sh", "-c", &script], 300);
    let image = docker::image();
    for role in ir["roles"].as_array_mut().unwrap() {
        role["image"] = json!(image.as_str());
        role["resources"] = json!({ "cpu_millis": 500, "memory_bytes": 256 << 20 });
    }
    for role in [1, 2] {
        let command = &mut ir["roles"][role]["runtime"]["judge"]["command"];
        command["argv"] = json!(["sh", "-c", JUDGE]);
    }
    let sandboxd = docker::sandboxd();
    check(
        "ops-on-docker",
        docker::engine(),
        &ir,
        sandboxd.to_str().unwrap(),
        &expect![[r#"
            finished, with score Some(Number(6))
            record: agent/log/command.stderr, agent/log/command.stdout, agent/out/, ir.json, jobs/0/log/command.stderr, jobs/0/log/command.stdout, jobs/0/out/evidence/evidence.txt, jobs/0/out/result.json, jobs/1/log/command.stderr, jobs/1/log/command.stdout, jobs/1/out/evidence/, jobs/2/log/command.stderr, jobs/2/log/command.stdout, jobs/2/out/evidence/evidence.txt, jobs/2/out/result.json, log.jsonl, outcome.json, payloads/5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6, payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd, provider.json, record.json
            payloads hold: ["slow", "xxxxxx"]
            provider.json is what the provider declared: true
            outcome.json: {"score":6,"state":"finished","verdict":{"kind":"judged","result":{"passed":true,"returned":{"raw_score":6},"score":6},"submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            log.jsonl:
            {"actions":[{"kind":"start_agent"}],"at_ns":"0","event":{"kind":"run_started","unix_ns":"<during execute>"},"seq":0}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"agent_started"},"seq":1}
            {"actions":[{"kind":"reply","op":0,"reply":{"grants":[{"call_timeout_seconds":1,"calls":100,"cumulative_seconds":null,"kind":"judge","max_bytes":8,"name":"train"},{"kind":"submit","max_bytes":16,"name":"submit"}],"kind":"ops","remaining_seconds":"<s>","resources":{"cpu_millis":500,"memory_bytes":268435456}}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":0,"request":{"kind":"ops"}},"seq":2}
            {"actions":[{"kind":"reply","op":1,"reply":{"kind":"refused","message":"you have no job 0: ask about a job that one of your calls returned"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":1,"request":{"job":"0","kind":"status"}},"seq":3}
            {"actions":[{"kind":"reply","op":2,"reply":{"kind":"refused","message":"you hold no grant named `nope`: call one that `ops` lists"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":2,"request":{"grant":"nope","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"seq":4}
            {"actions":[{"kind":"reply","op":3,"reply":{"kind":"refused","message":"the payload is 9 bytes, over `train`'s limit of 8: send a smaller one"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":3,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:a73add1aecea03a894ca65d99bc5e91d21b58cdb728343bbcb6cf6cf38b809f5","size":9}}},"seq":5}
            {"actions":[],"at_ns":"<ns>","event":{"kind":"op_refused","message":"the payload is 17 bytes, more than the limit of 16: send at most 16","request":{"grant":"train","kind":"judge","payload":{"over":16,"size":17}}},"seq":6}
            {"actions":[{"kind":"reply","op":4,"reply":{"job":0,"kind":"judge","remaining":{"calls":99,"seconds":null}}},{"grant":"train","job":0,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":4,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"seq":7}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_started"},"seq":8}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_exited","status":{"code":0}},"seq":9}
            {"actions":[],"at_ns":"<ns>","event":{"job":0,"kind":"judge_collected","result":{"passed":true,"returned":{"raw_score":6},"score":6}},"seq":10}
            {"actions":[{"kind":"reply","op":5,"reply":{"job":0,"kind":"status","remaining":{"calls":99,"seconds":null},"returned":{"raw_score":6},"state":"passed"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":5,"request":{"job":"0","kind":"status"}},"seq":11}
            {"actions":[{"kind":"reply","op":6,"reply":{"job":0,"kind":"status","remaining":{"calls":99,"seconds":null},"returned":{"raw_score":6},"state":"passed"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":6,"request":{"job":"0","kind":"status"}},"seq":12}
            {"actions":[{"kind":"reply","op":7,"reply":{"job":1,"kind":"judge","remaining":{"calls":98,"seconds":null}}},{"grant":"train","job":1,"kind":"start_judge","payload":{"digest":"sha256:5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6","size":4},"timeout_ns":"1000000000"}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":7,"request":{"grant":"train","kind":"judge","payload":{"digest":"sha256:5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6","size":4}}},"seq":13}
            {"actions":[],"at_ns":"<ns>","event":{"job":1,"kind":"judge_started"},"seq":14}
            {"actions":[],"at_ns":"<ns>","event":{"job":1,"kind":"judge_timed_out"},"seq":15}
            {"actions":[],"at_ns":"<ns>","event":{"invalid":"the judge wrote no result: write it at /pumpkin/out/result.json","job":1,"kind":"judge_collected"},"seq":16}
            {"actions":[{"kind":"reply","op":8,"reply":{"failure":{"code":"judge_timeout","message":"the judge ran past its timeout"},"job":1,"kind":"status","remaining":{"calls":98,"seconds":null},"state":"failed"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":8,"request":{"job":"1","kind":"status"}},"seq":17}
            {"actions":[{"kind":"reply","op":9,"reply":{"failure":{"code":"judge_timeout","message":"the judge ran past its timeout"},"job":1,"kind":"status","remaining":{"calls":98,"seconds":null},"state":"failed"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":9,"request":{"job":"1","kind":"status"}},"seq":18}
            {"actions":[{"kind":"reply","op":10,"reply":{"kind":"submit","remaining_seconds":"<s>","state":"accepted"}}],"at_ns":"<ns>","event":{"kind":"op_arrived","op":10,"request":{"finalize":true,"grant":"submit","kind":"submit","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"seq":19}
            {"actions":[{"kind":"tear_down_agent"},{"grant":null,"job":2,"kind":"start_judge","payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6},"timeout_ns":"120000000000"}],"at_ns":"<ns>","event":{"kind":"agent_exited","status":{"code":0}},"seq":20}
            {"at_ns":"<ns>","note":"collected from the agent's sandbox: out, 0 files of 0 bytes; log, 2 files of 1464 bytes","seq":21}
            {"actions":[],"at_ns":"<ns>","event":{"job":2,"kind":"judge_started"},"seq":22}
            {"actions":[],"at_ns":"<ns>","event":{"job":2,"kind":"judge_exited","status":{"code":0}},"seq":23}
            {"actions":[{"kind":"finish","outcome":{"score":6,"verdict":{"kind":"judged","result":{"passed":true,"returned":{"raw_score":6},"score":6},"submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}}],"at_ns":"<ns>","event":{"job":2,"kind":"judge_collected","result":{"passed":true,"returned":{"raw_score":6},"score":6}},"seq":24}
            its last record came before the agent's teardown was to end
            the agent's standard output:
            sandboxctl ops: exit 0: {"kind":"ops","remaining_seconds":"<s>","resources":{"cpu_millis":500,"memory_bytes":268435456},"grants":[{"kind":"judge","name":"train","max_bytes":8,"calls":100,"cumulative_seconds":null,"call_timeout_seconds":1},{"kind":"submit","name":"submit","max_bytes":16}]}
            sandboxctl status 0: exit 1: {"kind":"refused","message":"you have no job 0: ask about a job that one of your calls returned"}
            sandboxctl judge nope six: exit 1: {"kind":"refused","message":"you hold no grant named `nope`: call one that `ops` lists"}
            sandboxctl judge train nine: exit 1: {"kind":"refused","message":"the payload is 9 bytes, over `train`'s limit of 8: send a smaller one"}
            sandboxctl judge train seventeen: exit 1: {"kind":"refused","message":"the payload is 17 bytes, more than the limit of 16: send at most 16"}
            sandboxctl judge train six: exit 0: {"kind":"judge","job":0,"remaining":{"calls":99,"seconds":null}}
            sandboxctl status 0: exit 0: {"kind":"status","job":0,"state":"passed","returned":{"raw_score":6},"remaining":{"calls":99,"seconds":null}}
            sandboxctl judge train slow: exit 0: {"kind":"judge","job":1,"remaining":{"calls":98,"seconds":null}}
            sandboxctl status 1: exit 2: {"kind":"status","job":1,"state":"failed","failure":{"code":"judge_timeout","message":"the judge ran past its timeout"},"remaining":{"calls":98,"seconds":null}}
            sandboxctl submit submit six --finalize: exit 0: {"kind":"submit","state":"accepted","remaining_seconds":"<s>"}
            job 0's standard output:
            judged 6 bytes as 65532, with HOME=/pumpkin/home, in /pumpkin/home
            job 1's standard output:
            job 2's standard output:
            judged 6 bytes as 65532, with HOME=/pumpkin/home, in /pumpkin/home
            socket directories left: false
            listed once it ended: []"#]],
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
