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

use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};
use std::{fs, thread};

use expect_test::{Expect, expect};
use route::{Key, Serving, Upstream, UpstreamUrl};
use runtime::Run;
use schema::ir::{self, ModelName, Name};
use serde_json::Value;

const LWE: &str = include_str!("../../../schema/tests/fixtures/lwe.json");

/// The runs directory of test `name`, empty. Dropping it removes it, unless the test failed,
/// when it shows where the records are.
pub(crate) struct Runs {
    pub(crate) dir: PathBuf,
}

impl Runs {
    pub(crate) fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("pk-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Self { dir }
    }
}

impl Drop for Runs {
    fn drop(&mut self) {
        if thread::panicking() {
            eprintln!("the runs' records are in {}", self.dir.display());
        } else {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}

/// Prepares each IR at its time, in seconds from the Unix epoch, in a runs directory of its
/// own, with an upstream that serves each model it names, if it names any, and checks what
/// becomes of each: whether the run is prepared, named by its id, or how it is refused; and its
/// record, if it has one: its name, its files, and what they hold.
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
        let prepared = Run::prepare(&ir, &dir, now, serving);
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
            }
            Err(reason) => lines.push(format!("refused: {reason}")),
        }
        let files = files(&record, &record).join(", ");
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

/// A workflow is prepared, its record named by its run's id, from the UTC time it is prepared
/// at, and holding its IR; one that has a model route is prepared with an upstream that serves
/// every model the route allows, and else refused before anything starts, with its record
/// saying why. The times are a leap day's first second, a year's last, and the first of March
/// 2100, which follows the 28th of February in a year that isn't a leap year. A clock that
/// reads before 1970 is refused before anything is made.
#[test]
fn prepare() {
    let lwe: Value = serde_json::from_str(LWE).unwrap();
    let mut no_route = lwe.clone();
    no_route["roles"][0]
        .as_object_mut()
        .unwrap()
        .remove("egress");
    check(
        [
            (1_709_164_800, no_route.clone(), None),
            (1_767_225_599, lwe.clone(), None),
            (1_767_225_599, lwe.clone(), Some(&["gpt-6-sol"][..])),
            (1_767_225_599, lwe, Some(&["gpt-6-sol", "gpt-6-astra"][..])),
            (4_107_542_400, no_route.clone(), None),
            (-1, no_route, None),
        ],
        &expect![[r#"
            prepared, named by its id: true
            20240229-000000-hhhhhhhh: ir.json, record.json
            ir.json is the IR: true
            record.json: {"record_version":"pumpkin.record/v0"}
            refused: `agent` has a model route, but no upstream serves it: give one, or remove the role's `egress.model` to run without one
            20251231-235959-hhhhhhhh: ir.json, outcome.json, record.json
            ir.json is the IR: true
            record.json: {"record_version":"pumpkin.record/v0"}
            outcome.json: {"reason":"`agent` has a model route, but no upstream serves it: give one, or remove the role's `egress.model` to run without one","state":"refused"}
            refused: `agent`'s model route allows `gpt-6-astra`, which its upstream doesn't serve: give an upstream that serves every model the route allows
            20251231-235959-hhhhhhhh: ir.json, outcome.json, record.json
            ir.json is the IR: true
            record.json: {"record_version":"pumpkin.record/v0"}
            outcome.json: {"reason":"`agent`'s model route allows `gpt-6-astra`, which its upstream doesn't serve: give an upstream that serves every model the route allows","state":"refused"}
            prepared, named by its id: true
            20251231-235959-hhhhhhhh: ir.json, record.json
            ir.json is the IR: true
            record.json: {"record_version":"pumpkin.record/v0"}
            prepared, named by its id: true
            21000301-000000-hhhhhhhh: ir.json, record.json
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

/// The files and empty directories below `dir`, sorted.
pub(crate) fn files(base: &Path, dir: &Path) -> Vec<String> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    paths.sort();
    let mut lines = Vec::new();
    for path in paths {
        let name = path.strip_prefix(base).unwrap().display().to_string();
        if path.is_dir() {
            let below = files(base, &path);
            if below.is_empty() {
                lines.push(format!("{name}/"));
            }
            lines.extend(below);
        } else {
            lines.push(name);
        }
    }
    lines
}

/// Pretty-printed JSON on one line.
pub(crate) fn compact(json: &str) -> String {
    serde_json::from_str::<Value>(json).unwrap().to_string()
}
