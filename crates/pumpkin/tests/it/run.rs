//! Running a workflow through the library's entry point, [`Run`].
//!
//! `mini` runs a small workflow on Docker, with the Docker tests' `busybox` image: its agent
//! calls a judge grant through `sandboxctl`, polls each job's status until it has ended, and
//! submits with finalize; its judges write a `result.json` and evidence, in one call more than
//! its limit. Like the Docker tests, it is ignored, and fails without `PUMPKIN_SANDBOXD`. The
//! refusals need no Docker.

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]
#![expect(
    clippy::print_stderr,
    reason = "a failed run shows where its record is"
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use expect_test::{Expect, expect};
use pumpkin::driver::run::Run;
use pumpkin_docker::RunTag;
use pumpkin_schema::ir;
use serde_json::{Value, json};

use crate::docker::{containers, engine, image, sandboxd};

const LWE: &str = include_str!("../../../pumpkin-schema/tests/fixtures/lwe.json");

/// The agent: calls a judge grant it doesn't hold, whose payload is refused and not kept;
/// calls `train` with a payload over its slot's `max_bytes`, which the gateway refuses; calls
/// `train`, polling each job until it has finished, with a payload, then with one whose judge
/// writes too much evidence; and finalizes the first payload.
const AGENT: &str = r#"
set -o errexit -o nounset
ctl=/.pumpkin/bin/sandboxctl
printf ledger > payload
printf evidence > evidence
printf refused > refused
head -c 1025 /dev/zero > oversized
judged() {
    job=$($ctl judge train "$1" | sed 's/.*"job":\([0-9]*\).*/\1/')
    while $ctl status "$job" | grep -qE '"state":"(queued|running)"'; do sleep 1; done
}
$ctl ops | sed 's/"remaining_seconds":[0-9]*/"remaining_seconds":<s>/'
$ctl judge nothing refused || true
$ctl judge train oversized || true
judged payload
judged evidence
$ctl submit submit payload --finalize > /dev/null
"#;

/// A judge: scores a payload by its size, and keeps evidence, over its limit for the payload
/// `evidence`.
const JUDGE: &str = r#"
if [ "$(cat /pumpkin/in/payload/raw)" = evidence ]; then
    head -c 1024 /dev/zero > /pumpkin/out/evidence/padding
fi
bytes=$(wc -c < /pumpkin/in/payload/raw)
printf '{"status": "passed", "score": %s, "bytes": %s, "secret": "for the operator"}\n' \
    $bytes $bytes > /pumpkin/out/result.json
echo kept > /pumpkin/out/evidence/evidence.txt
echo "judged $bytes bytes as $(id -u), with HOME=$HOME, in $(pwd)"
"#;

/// The runs directory of test `name`, empty. Dropping it removes it, and the containers of the
/// run whose tag it holds, unless the test failed, when it shows where the records are.
struct Runs {
    dir: PathBuf,
    tag: Option<RunTag>,
}

impl Runs {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("pk-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Self { dir, tag: None }
    }
}

impl Drop for Runs {
    fn drop(&mut self) {
        if let Some(tag) = &self.tag {
            let _ = engine().destroy_all(tag);
        }
        if thread::panicking() {
            eprintln!("the runs' records are in {}", self.dir.display());
        } else {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}

/// Prepares each IR in a runs directory of its own, and checks how each is refused: the
/// message, the files of its record, and its outcome.
fn check_refused(irs: impl IntoIterator<Item = Value>, expect: &Expect) {
    let runs = Runs::new("refused");
    let mut lines = Vec::new();
    for (index, ir) in irs.into_iter().enumerate() {
        let ir: ir::Workflow = serde_json::from_value(ir).unwrap();
        let dir = runs.dir.join(index.to_string());
        let reason = Run::prepare(&ir, &dir).unwrap_err();
        let record = fs::read_dir(&dir).unwrap().next().unwrap().unwrap().path();
        let reason = reason.replace(&record.display().to_string(), "<record>");
        lines.push(format!("refused: {reason}"));
        lines.push(format!(
            "record: {}",
            files(&record, &record, false).join(", ")
        ));
        let outcome = fs::read_to_string(record.join("outcome.json")).unwrap();
        let outcome = outcome.replace(&record.display().to_string(), "<record>");
        lines.push(format!("outcome.json: {}", compact(&outcome)));
    }
    expect.assert_eq(&lines.join("\n"));
}

/// A workflow that has a model route is refused before anything starts, with its record
/// saying why.
#[test]
fn refusals() {
    let lwe: Value = serde_json::from_str(LWE).unwrap();
    check_refused(
        [lwe],
        &expect![[r#"
        refused: `agent` has a model route, which the driver doesn't serve: remove the role's `egress.model` to run without one
        record: ir.json, outcome.json
        outcome.json: {"reason":"`agent` has a model route, which the driver doesn't serve: remove the role's `egress.model` to run without one","state":"refused"}"#]],
    );
}

/// A run on Docker, from the agent's first op to the final judge's score, checked by its
/// outcome, its record and its log, and by what it leaves behind: nothing.
#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
#[expect(clippy::too_many_lines, reason = "most of it is the run's snapshot")]
fn mini() {
    let sandboxd = sandboxd();
    let mut runs = Runs::new("mini");
    let ir: ir::Workflow = serde_json::from_value(mini_ir(&image(), AGENT, JUDGE)).unwrap();
    let run = Run::prepare(&ir, &runs.dir).unwrap();
    let (tag, record, sockets) = (tag(run.id()), run.dir().to_owned(), sockets(run.id()));
    runs.tag = Some(tag.clone());
    let before = unix_ns();
    let outcome = run.execute(engine(), &sandboxd).unwrap();
    let during = before..=unix_ns();
    let mut lines = vec![format!("score: {:?}", outcome.score())];
    let outcome = fs::read_to_string(record.join("outcome.json")).unwrap();
    lines.push(format!("outcome.json: {}", compact(&outcome)));
    lines.push("the record:".to_owned());
    lines.extend(files(&record, &record, true));
    lines.push("the log, without the polls before the job ended:".to_owned());
    let log = fs::read_to_string(record.join("log.jsonl")).unwrap();
    lines.extend(shape(&log));
    let started: Value = serde_json::from_str(log.lines().next().unwrap()).unwrap();
    let started = nanos(&started["event"]["unix_ns"]).unwrap();
    lines.push(format!(
        "the run started during execute: {}",
        during.contains(&started)
    ));
    let left = containers(&tag);
    lines.push(format!(
        "left over: {} containers, sockets {}",
        left.len(),
        if sockets.exists() { "kept" } else { "removed" }
    ));
    expect![[r#"
        score: Some(Number(6))
        outcome.json: {"score":6,"state":"finished","verdict":{"kind":"judged","result":{"passed":true,"returned":{"bytes":6},"score":6},"submission":{"finalized":true,"payload":{"digest":"sha256:fe14010b4fe83303852f0467c919ef9a7ca089b91e96e3aad7d426dd87079297","size":6}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}
        the record:
        agent/log/command.stderr: 
        agent/log/command.stdout: {"kind":"ops","remaining_seconds":<s>,"resources":{"cpu_millis":500,"memory_bytes":268435456},"grants":[{"kind":"judge","name":"train","max_bytes":1024,"calls":2,"cumulative_seconds":null,"call_timeout_seconds":60},{"kind":"submit","name":"submit","max_bytes":1024}]}; {"kind":"refused","message":"you hold no grant named `nothing`: call one that `ops` lists"}; {"kind":"refused","message":"the payload is 1025 bytes, more than the limit of 1024: send at most 1024"}
        agent/out/
        engine.json
        ir.json
        jobs/1/log/command.stderr: 
        jobs/1/log/command.stdout: judged 6 bytes as 4242, with HOME=/pumpkin/home, in /pumpkin/home
        jobs/1/out/evidence/evidence.txt: kept
        jobs/1/out/result.json: {"status": "passed", "score": 6, "bytes": 6, "secret": "for the operator"}
        jobs/2/log/command.stderr: 
        jobs/2/log/command.stdout: judged 8 bytes as 4242, with HOME=/pumpkin/home, in /pumpkin/home
        jobs/2/out/result.json: {"status": "passed", "score": 8, "bytes": 8, "secret": "for the operator"}
        jobs/3/log/command.stderr: 
        jobs/3/log/command.stdout: judged 6 bytes as 4242, with HOME=/pumpkin/home, in /pumpkin/home
        jobs/3/out/evidence/evidence.txt: kept
        jobs/3/out/result.json: {"status": "passed", "score": 6, "bytes": 6, "secret": "for the operator"}
        log.jsonl
        outcome.json
        payloads/ee8250fb76e094b34b471f13a73dbbe51d1ae142e9df59d7c0d31ec20f0a0a8e: evidence
        payloads/fe14010b4fe83303852f0467c919ef9a7ca089b91e96e3aad7d426dd87079297: ledger
        the log, without the polls before the job ended:
        run_started {"unix_ns":"<unix_ns>"} -> start_agent
        agent_started
        op_arrived {"request":{"kind":"ops"}} -> reply {"reply":{"grants":[{"call_timeout_seconds":60,"calls":2,"cumulative_seconds":null,"kind":"judge","max_bytes":1024,"name":"train"},{"kind":"submit","max_bytes":1024,"name":"submit"}],"kind":"ops","resources":{"cpu_millis":500,"memory_bytes":268435456}}}
        op_arrived {"request":{"grant":"nothing","kind":"judge","payload":"7 bytes"}} -> reply {"reply":{"kind":"refused","message":"you hold no grant named `nothing`: call one that `ops` lists"}}
        op_refused {"message":"the payload is 1025 bytes, more than the limit of 1024: send at most 1024","request":{"grant":"train","kind":"judge","payload":{"over":1024,"size":1025}}}
        op_arrived {"request":{"grant":"train","kind":"judge","payload":"6 bytes"}} -> reply {"reply":{"job":1,"kind":"judge","remaining":{"calls":1,"seconds":null}}}; start_judge {"grant":"train","job":1,"payload":"6 bytes","timeout_ns":"60000000000"}
        judge_started {"job":1}
        judge_exited {"job":1,"status":{"code":0}}
        judge_collected {"job":1,"result":{"passed":true,"returned":{"bytes":6},"score":6}}
        op_arrived {"request":{"job":1,"kind":"status"}} -> reply {"reply":{"job":1,"kind":"status","remaining":{"calls":1,"seconds":null},"returned":{"bytes":6},"state":"passed"}}
        op_arrived {"request":{"grant":"train","kind":"judge","payload":"8 bytes"}} -> reply {"reply":{"job":2,"kind":"judge","remaining":{"calls":0,"seconds":null}}}; start_judge {"grant":"train","job":2,"payload":"8 bytes","timeout_ns":"60000000000"}
        judge_started {"job":2}
        judge_exited {"job":2,"status":{"code":0}}
        judge_collected {"invalid":"the evidence holds 1029 bytes, over the judge's `evidence_max_bytes` of 1024: write at most 1024","job":2}
        op_arrived {"request":{"job":2,"kind":"status"}} -> reply {"reply":{"failure":{"code":"invalid_result","message":"the judge wrote no valid result"},"job":2,"kind":"status","remaining":{"calls":0,"seconds":null},"state":"failed"}}
        op_arrived {"request":{"finalize":true,"grant":"submit","kind":"submit","payload":"6 bytes"}} -> reply {"reply":{"kind":"submit","state":"accepted"}}
        agent_exited {"status":{"code":0}} -> tear_down_agent; start_judge {"grant":null,"job":3,"payload":"6 bytes","timeout_ns":"18446744073709551615"}
        note: "collected from the agent's sandbox: out, 0 files of 0 bytes; log, 2 files of 465 bytes"
        judge_started {"job":3}
        judge_exited {"job":3,"status":{"code":0}}
        judge_collected {"job":3,"result":{"passed":true,"returned":{"bytes":6},"score":6}} -> finish with score 6
        the run started during execute: true
        left over: 0 containers, sockets removed"#]].assert_eq(&lines.join("\n"));
}

/// The tag of the containers of run `id`: its id.
fn tag(id: &str) -> RunTag {
    RunTag::from(id.to_owned())
}

/// The directory of run `id`'s socket directories, `pumpkin-<id>` in the temporary directory.
fn sockets(id: &str) -> PathBuf {
    std::env::temp_dir().join(format!("pumpkin-{id}"))
}

/// The mini workflow's IR, with `image` for every role, and `agent` and `judge` as the scripts
/// of the agent's and the judges' commands. The score's timeout is the longest the IR can
/// state, past what the clock can reach and what the run log's nanoseconds can count.
fn mini_ir(image: &ir::Image, agent: &str, judge: &str) -> Value {
    let role = |name: &str, runtime: Value| {
        json!({
            "name": name,
            "image": image.as_str(),
            "user": { "uid": 4242, "gid": 4243 },
            "resources": {
                "cpu_millis": 500,
                "memory_bytes": 256 << 20
            },
            "runtime": runtime
        })
    };
    let judge = json!({ "judge": {
        "command": { "argv": ["sh", "-c", judge], "cwd": "/pumpkin/home" },
        "slots": [{ "name": "payload", "max_bytes": 1024 }],
        "result": {
            "status": "/status",
            "score": "/score",
            "returns": ["/bytes"],
            "evidence_max_bytes": 1024
        }
    }});
    let agent =
        json!({ "agent": { "command": { "argv": ["sh", "-c", agent], "cwd": "/workspace" } } });
    json!({
        "ir_version": "pumpkin.ir/v0",
        "name": "test/mini",
        "roles": [
            role("agent", agent),
            role("train_judge", judge.clone()),
            role("final_judge", judge),
        ],
        "main": { "solo_then_judge": {
            "grants": [
                { "judge": {
                    "name": "train", "role": "train_judge", "slot": "payload", "calls": 2,
                    "call_timeout_seconds": 60
                } },
                { "submit": { "name": "submit" } },
            ],
            "score": { "role": "final_judge", "slot": "payload", "timeout_seconds": u64::MAX },
            "deadline_seconds": 300,
            "finalize_grace_seconds": 60
        } }
    })
}

/// The files and empty directories below `dir`, sorted, each with its contents if
/// `contents` and it is in `jobs/` or `agent/` or `payloads/`.
fn files(base: &Path, dir: &Path, contents: bool) -> Vec<String> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    paths.sort();
    let mut lines = Vec::new();
    for path in paths {
        let name = path.strip_prefix(base).unwrap().display().to_string();
        if path.is_dir() {
            let below = files(base, &path, contents);
            if below.is_empty() {
                lines.push(format!("{name}/"));
            }
            lines.extend(below);
        } else if contents
            && ["jobs/", "agent/", "payloads/"]
                .iter()
                .any(|top| name.starts_with(top))
        {
            let text = fs::read_to_string(&path).unwrap();
            lines.push(format!("{name}: {}", text.trim_end().replace('\n', "; ")));
        } else {
            lines.push(name);
        }
    }
    lines
}

/// Pretty-printed JSON on one line.
fn compact(json: &str) -> String {
    serde_json::from_str::<Value>(json).unwrap().to_string()
}

/// The log's records, one line each, leaving out the status polls answered before the job
/// ended, whose number depends on timing.
fn shape(log: &str) -> Vec<String> {
    let mut lines = Vec::new();
    for (seq, line) in log.lines().enumerate() {
        let record: Value = serde_json::from_str(line).unwrap();
        assert_eq!(record["seq"], json!(seq), "{record}");
        assert!(nanos(&record["at_ns"]).is_some(), "{record}");
        if let Some(note) = record.get("note") {
            lines.push(format!("note: {note}"));
            continue;
        }
        let actions: Vec<String> = record["actions"]
            .as_array()
            .unwrap()
            .iter()
            .map(action)
            .collect();
        let polled = record["event"]["request"]["kind"] == "status"
            && actions.iter().any(|action| {
                action.contains(r#""state":"queued""#) || action.contains(r#""state":"running""#)
            });
        if polled {
            continue;
        }
        let event = event(&record["event"]);
        lines.push(if actions.is_empty() {
            event
        } else {
            format!("{event} -> {}", actions.join("; "))
        });
    }
    lines
}

/// An event, without its op's number, and the run's start without its time.
fn event(event: &Value) -> String {
    let mut event = event.clone();
    if let Some(request) = event.get_mut("request") {
        shorten_payload(request);
    }
    if let Some(unix_ns) = event.get_mut("unix_ns") {
        *unix_ns = json!("<unix_ns>");
    }
    fields(&mut event)
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

/// An action, without its op's number or the seconds left, and the run's finish by its score
/// alone.
fn action(action: &Value) -> String {
    if action["kind"] == "finish" {
        return format!("finish with score {}", action["outcome"]["score"]);
    }
    let mut action = action.clone();
    shorten_payload(&mut action);
    if let Some(reply) = action.get_mut("reply").and_then(Value::as_object_mut) {
        reply.remove("remaining_seconds");
    }
    fields(&mut action)
}

/// A payload as its size alone, since its digest is in the record's `payloads/`. A payload
/// the gateway refused, which has no digest, stays as it is.
fn shorten_payload(value: &mut Value) {
    if let Some(payload) = value.get_mut("payload")
        && payload.get("digest").is_some()
    {
        *payload = json!(format!("{} bytes", payload["size"]));
    }
}

/// An event's or action's kind, then its other fields but an op's number.
fn fields(value: &mut Value) -> String {
    let fields = value.as_object_mut().unwrap();
    let kind = fields.remove("kind").unwrap();
    fields.remove("op");
    let kind = kind.as_str().unwrap();
    if fields.is_empty() {
        kind.to_owned()
    } else {
        format!("{kind} {value}")
    }
}
