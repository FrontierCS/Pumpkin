//! Preparing a run through the library's entry point, [`Run`]: its record begun, or its
//! workflow refused.

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]
#![expect(
    clippy::print_stderr,
    reason = "a failed run shows where its record is"
)]

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

use expect_test::{Expect, expect};
use provider::{Provider, Ready};
use route::{Key, Serving, UpstreamUrl};
use runtime::{Record, Recorded, Run, System, new_id};
use schema::ir::{self, Api, ModelName, Name, Sha256Digest};
use schema::workflow::{ByProvider, ByRole, Placed, Workflow};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use stdx::process::Environment;
use tempfile::TempDir;
use test_support::{WORKFLOW, files};

use crate::sim::Sim;

/// The runs directory of test `name`. Dropping it removes it, unless the test failed, when it
/// shows where the records are.
pub(crate) struct Runs {
    pub(crate) dir: PathBuf,
    scratch: TempDir,
}

impl Runs {
    pub(crate) fn new(name: &str) -> Self {
        let scratch = tempfile::Builder::new()
            .prefix(&format!("runtime-{name}-"))
            .tempdir()
            .unwrap();
        let dir = scratch.path().to_owned();
        Self { dir, scratch }
    }
}

impl Drop for Runs {
    fn drop(&mut self) {
        if thread::panicking() {
            self.scratch.disable_cleanup(true);
            eprintln!("the runs' records are in {}", self.dir.display());
        }
    }
}

/// Prepares each IR at its time, in seconds from the Unix epoch, in a runs directory of its
/// own, with an upstream that serves each model it names, if it names any, as started with the
/// args `--config /pumpkin.toml`, and checks what becomes of each: whether the run is prepared,
/// named by its id, or how it is refused; and its record, if it has one: its name, its files,
/// and what they hold, with the build it names shown as `<this build>` if it is this one, as
/// [`Record::other_build`] says. Each is [placed](placed) on the simulated provider.
fn check(
    cases: impl IntoIterator<Item = (i64, Value, Option<&'static [&'static str]>)>,
    expect: &Expect,
) {
    let runs = Runs::new("prepare");
    let args = ["--config".to_owned(), "/pumpkin.toml".to_owned()];
    let no_tags = BTreeMap::new();
    let mut lines = Vec::new();
    for (index, (seconds, value, served)) in cases.into_iter().enumerate() {
        let ir: ir::Workflow = serde_json::from_value(value).unwrap();
        let dir = runs.dir.join(index.to_string());
        let from_epoch = Duration::from_secs(seconds.unsigned_abs());
        let now = if seconds < 0 {
            UNIX_EPOCH - from_epoch
        } else {
            UNIX_EPOCH + from_epoch
        };
        let url = "http://127.0.0.1:9/responses";
        let sim = Sim::new();
        let prepared = new_id(now).and_then(|id| {
            let placed = placed(&ir, sim.ready())?;
            let servings = agent_servings(placed.workflow(), &runs.dir, url, served);
            let on = providers_of(&placed, |_| &sim);
            let recorded = Recorded {
                args: &args,
                tags: &no_tags,
            };
            Run::prepare(&System, placed, on, &dir, id, &servings, recorded)
        });
        let record = fs::read_dir(&dir)
            .ok()
            .and_then(|mut entries| entries.next());
        let Some(record) = record.map(|entry| entry.unwrap().path()) else {
            let made = dir.exists();
            lines.push(format!(
                "refused: {}; runs directory made: {made}",
                prepared.err().unwrap()
            ));
            continue;
        };
        let name = record.file_name().unwrap().to_str().unwrap();
        match prepared {
            Ok(run) => {
                let named = run.id() == name && run.dir() == record;
                lines.push(format!("prepared, named by its id: {named}"));
                let pid = File::open(record.join("driver.pid")).unwrap();
                let held = matches!(pid.try_lock_shared(), Err(TryLockError::WouldBlock));
                lines.push(format!("driver.pid is locked: {held}"));
                let pid = fs::read_to_string(record.join("driver.pid")).unwrap();
                let ours = pid == format!("{}\n", std::process::id());
                lines.push(format!("driver.pid holds this process's id: {ours}"));
            }
            Err(reason) => lines.push(format!("refused: {reason}")),
        }
        let files = files(&record).join(", ");
        lines.push(format!("{}: {files}", id_form(name)));
        let kept = fs::read_to_string(record.join("ir.json")).unwrap();
        let encoded = Workflow::parse(&ir).map(|workflow| runtime::ir_json(&workflow));
        lines.push(format!("ir.json is the IR: {}", encoded.ok() == Some(kept)));
        let version = fs::read_to_string(record.join("record.json")).unwrap();
        let mut version: Value = serde_json::from_str(&version).unwrap();
        if Record::open(&record).unwrap().other_build().is_none() {
            version["pumpkin"] = "<this build>".into();
        }
        lines.push(format!("record.json: {version}"));
        let outcome = record.join("outcome.json");
        if outcome.exists() {
            let outcome = fs::read_to_string(outcome).unwrap();
            lines.push(format!("outcome.json: {}", compact(&outcome)));
        }
    }
    expect.assert_eq(&lines.join("\n"));
}

/// The name of the one provider that [`placed`] places a workflow's roles on.
pub(crate) const PROVIDER: &str = "simulated";

/// `ir`, parsed and placed on one provider, [`PROVIDER`], that declares `ready` would
/// ([`placed_on`]).
pub(crate) fn placed(ir: &ir::Workflow, ready: &Ready) -> Result<Placed, String> {
    placed_on(ir, &BTreeMap::from([(PROVIDER, ready)]))
}

/// `ir`, parsed and placed on `providers`, each by its name with what it declares, in the classes
/// they offer, as a run places it.
pub(crate) fn placed_on(
    ir: &ir::Workflow,
    providers: &BTreeMap<&str, &Ready>,
) -> Result<Placed, String> {
    let offered: BTreeMap<Name, &Ready> = (providers.iter())
        .map(|(provider, ready)| (Name::try_from((*provider).to_owned()).unwrap(), *ready))
        .collect();
    let workflow = Workflow::parse(ir).map_err(|errors| errors.to_string())?;
    let placed = workflow.place(&offered, |ready| &ready.classes);
    let (placed, _) = placed.map_err(|errors| errors.to_string())?;
    Ok(placed)
}

/// Each provider that `placed` places a role on, as `provider` names it, as a run is prepared on
/// them.
pub(crate) fn providers_of<'p>(
    placed: &Placed,
    provider: impl Fn(&Name) -> &'p dyn Provider,
) -> ByProvider<&'p dyn Provider> {
    let Ok(providers) = placed.by_provider(|name| Ok::<_, Infallible>(provider(name)));
    providers
}

/// How a run of `workflow` serves its agent's route alone, through the upstream that [`serving`]
/// gives for `models`, if any are given.
fn agent_servings(
    workflow: &Workflow,
    dir: &Path,
    url: &str,
    models: Option<&[&str]>,
) -> ByRole<Vec<Serving>> {
    let agent = workflow.agent().role;
    let servings = workflow.by_role(|(role, _)| {
        let served = models.filter(|_| role == agent).into_iter();
        Ok::<_, Infallible>(served.map(|models| serving(dir, url, models)).collect())
    });
    let Ok(servings) = servings;
    servings
}

/// The upstream `stand-in` at `url`, whose key is in a file in `dir`, which serves each of
/// `models` under the name `vendor/<model>` through the Responses API.
pub(crate) fn serving(dir: &Path, url: &str, models: &[&str]) -> Serving {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join("key"), "sk-test\n").unwrap();
    let url = UpstreamUrl::try_from(url.to_owned()).unwrap();
    let key = Key::read(&dir.join("key")).unwrap();
    let model = |name: String| ModelName::try_from(name).unwrap();
    let models = models
        .iter()
        .map(|name| (model((*name).to_owned()), model(format!("vendor/{name}"))));
    Serving {
        name: Name::try_from("stand-in".to_owned()).unwrap(),
        api: Api::Responses,
        url,
        key,
        models: models.collect(),
    }
}

#[test]
fn build() {
    let version = env!("CARGO_PKG_VERSION");
    // The build script's rule: the tree is a git checkout of Pumpkin's repository if its root
    // holds `.git`.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    if root.join(".git").exists() {
        // A checkout, as ci always runs in: the commit HEAD is at, as git gives it. A commit
        // since the build moves HEAD, whose ref the build script watches, so cargo builds again
        // before it runs this.
        // Not git's own variables, such as the `GIT_DIR` that `git rebase --exec` sets.
        let mut git = stdx::process::command("git", Environment::Only(&["PATH", "HOME"]));
        git.arg("-C").arg(&root);
        let output = git
            .args(["log", "--max-count=1", "--date=short", "--format=%H %cd"])
            .output()
            .unwrap();
        assert!(output.status.success(), "git names HEAD's commit");
        let text = String::from_utf8(output.stdout).unwrap();
        let (hash, date) = text.trim().split_once(' ').unwrap();
        let named = format!("{version} ({} {date})", &hash[..9]);
        assert_eq!(
            runtime::BUILD.to_string(),
            named,
            "the build names HEAD's commit"
        );
    } else {
        // No checkout, as cargo-mutants' copy of the tree or a source archive: no commit.
        assert_eq!(
            runtime::BUILD.to_string(),
            version,
            "the build names no commit"
        );
    }
}

// The agent and the train judge run at once, and the final judge after, so `wide` needs 1100 CPUs
// at once, more than the provider holds, though each role fits its class.
#[test]
fn prepare() {
    let ir: Value = serde_json::from_str(WORKFLOW).unwrap();
    let mut no_route = ir.clone();
    no_route["roles"][0]
        .as_object_mut()
        .unwrap()
        .remove("egress");
    let mut wide = no_route.clone();
    for (role, cpu_millis) in [(0, 1_000_000), (1, 100_000), (2, 1_000_000)] {
        wide["roles"][role]["needs"]["cpu_millis"] = cpu_millis.into();
    }
    check(
        [
            // A leap day's first second, a year's last, and 2100's first of March, which follows
            // its 28th of February, as 2100 isn't a leap year.
            (1_709_164_800, no_route.clone(), None),
            (1_767_225_599, ir.clone(), None),
            (1_767_225_599, ir.clone(), Some(&["gpt-6-sol"][..])),
            (1_767_225_599, ir, Some(&["gpt-6-sol", "gpt-6-astra"][..])),
            (4_107_542_400, no_route.clone(), None),
            (-1, no_route, None),
            (1_767_225_599, wide, None),
        ],
        &expect![[r#"
            prepared, named by its id: true
            driver.pid is locked: true
            driver.pid holds this process's id: true
            20240229-000000-hhhhhhhh: driver.log, driver.pid, ir.json, record.json
            ir.json is the IR: true
            record.json: {"args":["--config","/pumpkin.toml"],"pumpkin":"<this build>","record_version":"pumpkin.record/v0"}
            refused: `agent` has a model route, but no upstream serves its `responses` entry: give one, or remove the role's `egress.model` to run without one; runs directory made: false
            refused: `agent`'s model route allows `gpt-6-astra`, which its upstream doesn't serve: give an upstream that serves every model the route allows; runs directory made: false
            prepared, named by its id: true
            driver.pid is locked: true
            driver.pid holds this process's id: true
            20251231-235959-hhhhhhhh: driver.log, driver.pid, ir.json, record.json
            ir.json is the IR: true
            record.json: {"args":["--config","/pumpkin.toml"],"pumpkin":"<this build>","record_version":"pumpkin.record/v0"}
            prepared, named by its id: true
            driver.pid is locked: true
            driver.pid holds this process's id: true
            21000301-000000-hhhhhhhh: driver.log, driver.pid, ir.json, record.json
            ir.json is the IR: true
            record.json: {"args":["--config","/pumpkin.toml"],"pumpkin":"<this build>","record_version":"pumpkin.record/v0"}
            refused: the clock reads before 1970-01-01: set the system clock; runs directory made: false
            refused: reserving on provider `simulated`: the run needs 1100 simulated CPUs, and 0 of the 1024 are held: run it once others end; runs directory made: false"#]],
    );
}

/// `id` with each hexadecimal digit of its random suffix shown as `h`.
fn id_form(id: &str) -> String {
    let (time, suffix) = id.rsplit_once('-').unwrap();
    let suffix = suffix.replace(|c| matches!(c, '0'..='9' | 'a'..='f'), "h");
    format!("{time}-{suffix}")
}

/// Pretty-printed JSON on one line.
pub(crate) fn compact(json: &str) -> String {
    serde_json::from_str::<Value>(json).unwrap().to_string()
}

/// `outcome`, an `outcome.json`, compact, with the times of its usage, how long its sandboxes
/// lasted and were charged for, and busy, which a run on the system's clock takes as it goes, as
/// `<ns>`.
pub(crate) fn compact_outcome(outcome: &str) -> String {
    let mut outcome: Value = serde_json::from_str(outcome).unwrap();
    if let Some(usage) = outcome.get_mut("usage") {
        usage["ran_ns"] = "<ns>".into();
        for sandboxes in usage["sandboxes"].as_array_mut().unwrap() {
            sandboxes["lasted_ns"] = "<ns>".into();
            for key in ["charged_ns", "busy_ns"] {
                if let Some(time) = sandboxes.get_mut(key) {
                    *time = "<ns>".into();
                }
            }
            if let Some(usd) = sandboxes.get_mut("usd") {
                *usd = "<usd>".into();
            }
        }
        if let Some(usd) = usage.get_mut("usd") {
            *usd = "<usd>".into();
        }
    }
    outcome.to_string()
}

// A run of Codex's, trimmed to its first 10 requests, each with its tools cut to its first
// two.
#[test]
fn rebuilds_kept_requests() {
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/kept/20261113-145222-a5111887");
    let record = Record::open(&fixture).unwrap();
    let sums = fs::read_to_string(fixture.join("requests.sha256")).unwrap();
    let lines: Vec<String> = sums
        .lines()
        .map(|line| {
            let [number, size, digest] = line.splitn(3, ' ').collect::<Vec<_>>()[..] else {
                panic!("`{line}` is no line of requests.sha256");
            };
            let rebuilt = record.model_request(number.parse().unwrap()).unwrap();
            let sha256: [u8; 32] = Sha256::digest(&rebuilt).into();
            let same =
                rebuilt.len().to_string() == size && Sha256Digest::from(sha256).as_str() == digest;
            format!(
                "request {number}: {} bytes, as the run sent it: {same}",
                rebuilt.len()
            )
        })
        .collect();
    let items = fs::read_dir(fixture.join("model/items")).unwrap().count();
    let kept: u64 = fs::read_dir(fixture.join("model"))
        .unwrap()
        .chain(fs::read_dir(fixture.join("model/items")).unwrap())
        .map(|entry| entry.unwrap().metadata().unwrap())
        .filter(std::fs::Metadata::is_file)
        .map(|metadata| metadata.len())
        .sum();
    expect![[r#"
        request 0: 25402 bytes, as the run sent it: true
        request 1: 36910 bytes, as the run sent it: true
        request 2: 46629 bytes, as the run sent it: true
        request 3: 55080 bytes, as the run sent it: true
        request 4: 62291 bytes, as the run sent it: true
        request 5: 74456 bytes, as the run sent it: true
        request 6: 87158 bytes, as the run sent it: true
        request 7: 88641 bytes, as the run sent it: true
        request 8: 89794 bytes, as the run sent it: true
        request 9: 90582 bytes, as the run sent it: true
        kept: 38 items, 141000 bytes in all"#]]
    .assert_eq(&format!(
        "{}\nkept: {items} items, {kept} bytes in all",
        lines.join("\n")
    ));
}

// None of the 429, the cut stream, the failing 200, the route's refusal and the bar is good, so the
// first's time stays the last good one's.
#[test]
fn reads_model_answers() {
    let dir = TempDir::new().unwrap();
    let record = dir.path().join("20261114-120000-00000001");
    fs::create_dir(&record).unwrap();
    let version = r#"{"record_version":"pumpkin.record/v0","args":[]}"#;
    fs::write(record.join("record.json"), version).unwrap();
    let answers = [
        r#"{"answer":{"kind":"forwarded"},"upstream_status":200}"#,
        r#"{"answer":{"kind":"forwarded"},"upstream_status":429}"#,
        r#"{"answer":{"kind":"cut"},"upstream_status":200}"#,
        r#"{"answer":{"kind":"forwarded"},"upstream_status":200,"failed":{"code":"rate_limit_exceeded"}}"#,
        r#"{"answer":{"kind":"refused","status":502,"message":"m"},"upstream_status":null}"#,
        r#"{"answer":{"kind":"barred","status":400,"message":"m"},"upstream_status":null}"#,
    ];
    let mut log = String::from(
        r#"{"seq":0,"at_ns":"0","observed_ns":"0","actions":[],"event":{"kind":"run_started","unix_ns":"0","hard_end_ns":"600000000000"}}"#,
    );
    for (seq, answer) in (1_u64..).zip(answers) {
        let mut event: Value = serde_json::from_str(answer).unwrap();
        event["kind"] = "model_answered".into();
        event["caller"] = "agent".into();
        event["request"] = (seq - 1).into();
        let at = (seq * 1_000_000_000).to_string();
        let answered =
            serde_json::json!({ "seq": seq, "at_ns": at, "actions": [], "event": event });
        log.push('\n');
        log.push_str(&answered.to_string());
    }
    log.push('\n');
    fs::write(record.join("log.jsonl"), log).unwrap();
    let progress = Record::open(&record)
        .unwrap()
        .progress()
        .unwrap()
        .progress
        .unwrap();
    expect![[r#"
        ModelUse {
            requests: 6,
            answers: {
                "200": 2,
                "200 cut": 1,
                "429": 1,
                "barred": 1,
                "refused": 1,
            },
            failed: {
                Some(
                    "rate_limit_exceeded",
                ): 1,
            },
            not_2xx: 4,
            last_good: Some(
                1s,
            ),
            tokens: Tokens {
                reported: 0,
                input: 0,
                output: 0,
                cached: None,
                reasoning: None,
            },
        }"#]]
    .assert_eq(&format!("{:#?}", progress.model));
}

// Each command is busy until its lease's charge ends, so that busy is never more than charged.
#[test]
fn reads_unended_commands() {
    let dir = TempDir::new().unwrap();
    let record = dir.path().join("20261114-120000-00000001");
    fs::create_dir(&record).unwrap();
    let version = r#"{"record_version":"pumpkin.record/v0","args":[]}"#;
    fs::write(record.join("record.json"), version).unwrap();
    let ir = serde_json::json!({ "roles": [{
        "name": "solver",
        "runtime": { "agent": {} },
        "grants": [
            { "lease": { "name": "stopped", "role": "stopped" } },
            { "lease": { "name": "expired", "role": "expired" } },
        ],
    }] });
    fs::write(record.join("ir.json"), ir.to_string()).unwrap();
    let place = |class: &str| serde_json::json!({ "provider": "aws", "class": class });
    let provided = serde_json::json!({ "roles": {
        "solver": place("agent"), "stopped": place("stopped"), "expired": place("expired"),
    } });
    fs::write(record.join("provider.json"), provided.to_string()).unwrap();
    let run_started =
        serde_json::json!({ "kind": "run_started", "unix_ns": "0", "hard_end_ns": "600000000000" });
    let tick = serde_json::json!({ "kind": "tick" });
    let event = |kind: &str, lease: u64| serde_json::json!({ "kind": kind, "lease": lease });
    let deleted =
        |sandbox: &str| serde_json::json!({ "kind": "sandbox_deleted", "sandbox": sandbox });
    let start = |grant: &str, lease: u64| {
        serde_json::json!({
            "kind": "start_lease", "grant": grant, "holder": "agent", "lease": lease,
        })
    };
    let run = |lease: u64| serde_json::json!({ "kind": "run_command", "lease": lease, "command": 0, "argv": [] });
    let close = serde_json::json!({ "kind": "close_lease", "lease": 1 });
    let records = [
        (0, run_started, vec![]),
        (
            1,
            tick.clone(),
            vec![start("stopped", 0), start("expired", 1)],
        ),
        (2, event("lease_ready", 0), vec![]),
        (2, event("lease_ready", 1), vec![]),
        (3, tick.clone(), vec![run(0), run(1)]),
        (5, tick, vec![close]),
        (6, deleted("lease-1"), vec![]),
        (7, event("lease_ended", 1), vec![]),
        (12, deleted("lease-0"), vec![]),
        (20, deleted("agent"), vec![]),
    ];
    let mut log = String::new();
    for (seq, (seconds, event, actions)) in (0_u64..).zip(records) {
        let at = (seconds * 1_000_000_000_u64).to_string();
        let record =
            serde_json::json!({ "seq": seq, "at_ns": at, "actions": actions, "event": event });
        log.push_str(&record.to_string());
        log.push('\n');
    }
    fs::write(record.join("log.jsonl"), log).unwrap();
    let progress = Record::open(&record)
        .unwrap()
        .progress()
        .unwrap()
        .progress
        .unwrap();
    let sandboxes = progress.sandboxes().into_iter();
    let used = sandboxes.map(|(place, used)| format!("{}: {:?}", place.class, used.charged));
    expect![[r#"
        agent: None
        expired: Some((3s, 2s))
        stopped: Some((10s, 9s))"#]]
    .assert_eq(&used.collect::<Vec<_>>().join("\n"));
}

// Unknown kinds read as nothing, a start without its hard end as no start; a line, event or action
// that doesn't read is left out and given, and the rest of its line counts.
#[test]
fn reads_other_builds_lines() {
    let dir = TempDir::new().unwrap();
    let record = dir.path().join("20261114-120000-00000001");
    fs::create_dir(&record).unwrap();
    let version = r#"{"record_version":"pumpkin.record/v0","args":[]}"#;
    fs::write(record.join("record.json"), version).unwrap();
    let started = r#"{"seq":0,"at_ns":"0","observed_ns":"0","event":{"kind":"run_started","unix_ns":"0","hard_end_ns":"600000000000"},"actions":[]}"#;
    let later = r#"{"seq":1,"at_ns":"1000000000","event":{"kind":"later","job":0},"actions":[{"kind":"later"}]}"#;
    let unnumbered =
        r#"{"seq":2,"at_ns":"2000000000","event":{"kind":"judge_started"},"actions":[]}"#;
    let killed = r#"{"seq":3,"at_ns":"3000000000","event":{"kind":"command_ended","lease":0,"end":"killed"},"actions":[]}"#;
    let ungranted = r#"{"seq":4,"at_ns":"4000000000","event":{"kind":"agent_started"},"actions":[{"kind":"start_lease","lease":0},{"kind":"finish"}]}"#;
    let leased = r#"{"seq":5,"at_ns":"5000000000","event":{"kind":"model_answered","caller":"lease-1","upstream_status":200,"answer":{"kind":"forwarded"}},"actions":[]}"#;
    let unbounded = r#"{"seq":0,"at_ns":"0","event":{"kind":"run_started","unix_ns":"0"},"actions":[{"kind":"start_agent"}]}"#;
    let untimed =
        r#"{"seq":0,"at_ns":"0","event":{"kind":"run_started","hard_end_ns":"0"},"actions":[]}"#;
    let mut read = Vec::new();
    let logs = [
        &[started, later][..],
        &[unbounded, later],
        &[started, unnumbered, "{\"seq\":", killed, later],
        &[started, ungranted, leased],
        &[untimed, later],
    ];
    for lines in logs {
        let mut log = lines.join("\n");
        log.push('\n');
        fs::write(record.join("log.jsonl"), log).unwrap();
        let readout = Record::open(&record).unwrap().progress().unwrap();
        let progress = (readout.progress)
            .map(|progress| (progress.last, progress.agent.sandbox.started, progress.end));
        read.push(format!("{progress:?} {:?}", readout.unread));
    }
    expect![[r#"
        Some((1s, None, None)) []
        None []
        Some((3s, None, None)) [Unread { line: 2, part: Event, reason: "missing field `job`" }, Unread { line: 3, part: Line, reason: "EOF while parsing a value, at column 7" }, Unread { line: 4, part: Event, reason: "unknown variant `killed`, expected one of `exited`, `timed_out`, `unstarted`" }]
        Some((5s, Some(4s), Some(4s))) [Unread { line: 2, part: Action(0), reason: "missing field `grant`" }, Unread { line: 3, part: Event, reason: "lease-1 is no caller: expected `agent` or `job-<n>`" }]
        None [Unread { line: 1, part: Event, reason: "missing field `unix_ns`" }]"#]].assert_eq(&read.join("\n"));
}

// Each span runs between the core's times, its `observed_ns`, but where a record has none, as
// lease 1's delete, its `at_ns`.
#[test]
fn reads_core_times() {
    let dir = TempDir::new().unwrap();
    let record = dir.path().join("20261114-120000-00000001");
    fs::create_dir(&record).unwrap();
    let version = r#"{"record_version":"pumpkin.record/v0","args":[]}"#;
    fs::write(record.join("record.json"), version).unwrap();
    let ir = serde_json::json!({ "roles": [{
        "name": "solver",
        "runtime": { "agent": {} },
        "grants": [
            { "judge": { "name": "train", "role": "trainer" } },
            { "lease": { "name": "runner", "role": "runner" } },
        ],
    }] });
    fs::write(record.join("ir.json"), ir.to_string()).unwrap();
    let place = |class: &str| serde_json::json!({ "provider": "aws", "class": class });
    let provided = serde_json::json!({ "roles": {
        "solver": place("agent"), "trainer": place("trainer"), "runner": place("runner"),
    } });
    fs::write(record.join("provider.json"), provided.to_string()).unwrap();
    let log = r#"{"seq":0,"at_ns":"0","observed_ns":"0","event":{"kind":"run_started","unix_ns":"0","hard_end_ns":"600000000000"},"actions":[]}
{"seq":1,"at_ns":"2000000000","observed_ns":"1000000000","event":{"kind":"op_arrived","request":{"kind":"judge","grant":"train"}},"actions":[{"kind":"start_judge","job":0,"grant":"train","timeout_ns":"60000000000"}]}
{"seq":2,"at_ns":"3000000000","observed_ns":"1000000000","event":{"kind":"tick"},"actions":[{"kind":"start_lease","grant":"runner","holder":"agent","lease":0},{"kind":"start_lease","grant":"runner","holder":"agent","lease":1}]}
{"seq":3,"at_ns":"5000000000","observed_ns":"2000000000","event":{"kind":"judge_started","job":0},"actions":[]}
{"seq":4,"at_ns":"8000000000","observed_ns":"3000000000","event":{"kind":"lease_ready","lease":0},"actions":[]}
{"seq":5,"at_ns":"9000000000","observed_ns":"3000000000","event":{"kind":"lease_ready","lease":1},"actions":[]}
{"seq":6,"at_ns":"10000000000","observed_ns":"4000000000","event":{"kind":"tick"},"actions":[{"kind":"run_command","lease":0,"command":0,"argv":[]},{"kind":"run_command","lease":1,"command":0,"argv":[]}]}
{"seq":7,"at_ns":"14000000000","observed_ns":"6000000000","event":{"kind":"command_ended","lease":0,"end":{"exited":{"code":0}}},"actions":[]}
{"seq":8,"at_ns":"15000000000","observed_ns":"9000000000","event":{"kind":"op_arrived","request":{"kind":"lease_close","lease":0}},"actions":[{"kind":"close_lease","lease":0}]}
{"seq":9,"at_ns":"16000000000","observed_ns":"11000000000","event":{"kind":"lease_ended","lease":0},"actions":[]}
{"seq":10,"at_ns":"20000000000","observed_ns":"14000000000","event":{"kind":"judge_exited","job":0},"actions":[]}
{"seq":11,"at_ns":"21000000000","observed_ns":"15000000000","event":{"kind":"judge_collected","job":0,"result":{"score":1,"returned":{}}},"actions":[]}
{"seq":12,"at_ns":"25000000000","event":{"kind":"sandbox_deleted","sandbox":"lease-1"},"actions":[]}
"#;
    fs::write(record.join("log.jsonl"), log).unwrap();
    let progress = Record::open(&record)
        .unwrap()
        .progress()
        .unwrap()
        .progress
        .unwrap();
    let sandboxes = progress.sandboxes().into_iter();
    let charged = sandboxes.map(|(place, used)| format!("{}: {:?}", place.class, used.charged));
    let calls = (progress.calls.iter()).map(|call| {
        let runtime::Sandbox {
            asked,
            started,
            gone,
            ..
        } = &call.sandbox;
        format!(
            "job {}: asked {asked:?}, started {started:?}, gone {gone:?}",
            call.job
        )
    });
    let leases = (progress.leases.iter()).map(|lease| {
        let runtime::Sandbox { started, gone, .. } = &lease.sandbox;
        format!("lease {}: ready {started:?}, gone {gone:?}", lease.lease)
    });
    let last = format!("last: {:?}", progress.last);
    let read: Vec<String> = charged.chain(calls).chain(leases).chain([last]).collect();
    expect![[r#"
        agent: None
        runner: Some((28s, 23s))
        trainer: None
        job 0: asked 1s, started Some(2s), gone Some(15s)
        lease 0: ready Some(3s), gone Some(11s)
        lease 1: ready Some(3s), gone Some(25s)
        last: 25s"#]]
    .assert_eq(&read.join("\n"));
}
