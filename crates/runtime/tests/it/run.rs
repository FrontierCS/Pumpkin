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

use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

use expect_test::{Expect, expect};
use provider::{Provider, Ready};
use route::{Key, Serving, Upstream, UpstreamUrl};
use runtime::{Run, System};
use schema::ir::{self, ModelName, Name};
use schema::workflow::{Placed, Workflow};
use serde_json::Value;
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
/// own, with an upstream that serves each model it names, if it names any, and checks what
/// becomes of each: whether the run is prepared, named by its id, or how it is refused; and its
/// record, if it has one: its name, its files, and what they hold. Each is [placed](placed) on
/// the simulated provider.
fn check(
    cases: impl IntoIterator<Item = (i64, Value, Option<&'static [&'static str]>)>,
    expect: &Expect,
) {
    let runs = Runs::new("prepare");
    let mut lines = Vec::new();
    for (index, (seconds, value, served)) in cases.into_iter().enumerate() {
        let ir: ir::Workflow = serde_json::from_value(value.clone()).unwrap();
        let dir = runs.dir.join(index.to_string());
        let from_epoch = Duration::from_secs(seconds.unsigned_abs());
        let now = if seconds < 0 {
            UNIX_EPOCH - from_epoch
        } else {
            UNIX_EPOCH + from_epoch
        };
        let url = "http://127.0.0.1:9/responses";
        let serving = served.map(|models| serving(&runs.dir, url, models));
        let prepared = placed(&ir, Sim::new().ready())
            .and_then(|placed| Run::prepare(&System, placed, &dir, now, serving));
        let record = fs::read_dir(&dir)
            .ok()
            .and_then(|mut entries| entries.next());
        let Some(record) = record.map(|entry| entry.unwrap().path()) else {
            let made = dir.exists();
            lines.push(format!(
                "refused: {}; runs directory made: {made}",
                prepared.unwrap_err()
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
        let kept: Value =
            serde_json::from_slice(&fs::read(record.join("ir.json")).unwrap()).unwrap();
        lines.push(format!("ir.json is the IR: {}", kept == value));
        let version = fs::read_to_string(record.join("record.json")).unwrap();
        lines.push(format!("record.json: {}", compact(&version)));
        let outcome = record.join("outcome.json");
        if outcome.exists() {
            let outcome = fs::read_to_string(outcome).unwrap();
            lines.push(format!("outcome.json: {}", compact(&outcome)));
        }
    }
    expect.assert_eq(&lines.join("\n"));
}

/// `ir`, parsed and placed in the classes that a provider that declares `ready` offers.
pub(crate) fn placed(ir: &ir::Workflow, ready: &Ready) -> Result<Placed, String> {
    let workflow = Workflow::parse(ir).map_err(|errors| errors.to_string())?;
    (workflow.place(&ready.classes)).map_err(|errors| errors.to_string())
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

#[test]
fn prepare() {
    let ir: Value = serde_json::from_str(WORKFLOW).unwrap();
    let mut no_route = ir.clone();
    no_route["roles"][0]
        .as_object_mut()
        .unwrap()
        .remove("egress");
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
        ],
        &expect![[r#"
            prepared, named by its id: true
            driver.pid is locked: true
            driver.pid holds this process's id: true
            20240229-000000-hhhhhhhh: driver.log, driver.pid, ir.json, record.json
            ir.json is the IR: true
            record.json: {"record_version":"pumpkin.record/v0"}
            refused: `agent` has a model route, but no upstream serves it: give one, or remove the role's `egress.model` to run without one; runs directory made: false
            refused: `agent`'s model route allows `gpt-6-astra`, which its upstream doesn't serve: give an upstream that serves every model the route allows; runs directory made: false
            prepared, named by its id: true
            driver.pid is locked: true
            driver.pid holds this process's id: true
            20251231-235959-hhhhhhhh: driver.log, driver.pid, ir.json, record.json
            ir.json is the IR: true
            record.json: {"record_version":"pumpkin.record/v0"}
            prepared, named by its id: true
            driver.pid is locked: true
            driver.pid holds this process's id: true
            21000301-000000-hhhhhhhh: driver.log, driver.pid, ir.json, record.json
            ir.json is the IR: true
            record.json: {"record_version":"pumpkin.record/v0"}
            refused: the clock reads before 1970-01-01: set the system clock; runs directory made: false"#]],
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
