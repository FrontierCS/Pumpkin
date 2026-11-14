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
use route::{Key, Serving, Upstream, UpstreamUrl};
use runtime::{Record, Recorded, Run, System, new_id};
use schema::ir::{self, ModelName, Name};
use schema::workflow::{ByProvider, Placed, Workflow};
use serde_json::Value;
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
        let serving = served.map(|models| serving(&runs.dir, url, models));
        let sim = Sim::new();
        let prepared = new_id(now).and_then(|id| {
            let placed = placed(&ir, sim.ready())?;
            let on = providers_of(&placed, |_| &sim);
            let recorded = Recorded { args: &args };
            Run::prepare(&System, placed, on, &dir, id, serving, recorded)
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

/// The upstream `stand-in` at `url`, whose key is in a file in `dir`, which serves each of
/// `models` under the name `vendor/<model>`.
pub(crate) fn serving(dir: &Path, url: &str, models: &[&str]) -> Serving {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join("key"), "sk-test\n").unwrap();
    let url = UpstreamUrl::try_from(url.to_owned()).unwrap();
    let upstream = Upstream::new(url, Key::read(&dir.join("key")).unwrap());
    let model = |name: String| ModelName::try_from(name).unwrap();
    let models = models
        .iter()
        .map(|name| (model((*name).to_owned()), model(format!("vendor/{name}"))));
    Serving {
        name: Name::try_from("stand-in".to_owned()).unwrap(),
        upstream,
        models: models.collect(),
    }
}

// A workflow is prepared, its record named by its run's id, from the UTC time it is prepared
// at, and holding its IR; one that has a model route is prepared with an upstream that serves
// every model the route allows, and else refused before anything starts, with its record
// saying why. The times are a leap day's first second, a year's last, and the first of March
// 2100, which follows the 28th of February in a year that isn't a leap year. A clock that
// reads before 1970 is refused before anything is made, and so is a workflow whose roles need
// more CPUs at once than the provider can hold for it, though each fits its class: the agent and
// the train judge run at once, and the final judge, which needs as many as the agent, after.
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
            refused: `agent` has a model route, but no upstream serves it: give one, or remove the role's `egress.model` to run without one; runs directory made: false
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
        }
    }
    outcome.to_string()
}
