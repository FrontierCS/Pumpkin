//! The bodies that the tests' workflows call, written in Rust.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test helper fails the test that calls it"
)]

use std::collections::BTreeMap;
use std::path::Path;

use schema::ir::{self, AbsolutePath, Argv, Name};
use schema::sandbox::RESULT_MAX_BYTES;
use serde_json::{Map, Value, json};

use crate::sim;

/// The tests' bodies, each by its `def`'s name: `check` puts the payload at `/work/payload`, runs
/// `list <its keyword dir>`, and returns what it printed and its caller's `label`; `train` puts it
/// there, runs `judge` on it, and returns its result's `raw_score`; `run` puts the payload at
/// `/pumpkin/in/payload/raw` and runs its keyword `argv`; `slow` runs `slow`, which runs until it
/// is killed. `again` runs a command of two seconds, then `print two`, and returns what it printed;
/// `holds` and `waits` run `print one`, then get `/work/held`, which the simulated sandbox holds,
/// or wait for the record to show the agent's sandbox deleted, then run `print two`; `missing`
/// gets a missing file; what `deep` returns nests, its dict included, its keyword `depth` deep;
/// `large` returns [`large()`].
pub(crate) struct Bodies<'r> {
    /// The run's record.
    pub(crate) record: &'r Path,
}

/// More stack than a test's thread has, 2 MiB unless `RUST_MIN_STACK` says otherwise, and less
/// than [`Bodies`]'s `stack_bytes`, so that a now function's body runs only on a stack that big.
const DEEP_BYTES: usize = 8 << 20;

impl runtime::Bodies for Bodies<'_> {
    fn stack_bytes(&self) -> usize {
        16 << 20
    }

    fn job(
        &self,
        body: &ir::Body,
        keywords: &Map<String, Value>,
        arguments: &BTreeMap<Name, ir::ArgumentValue>,
        target: &mut dyn runtime::Target,
    ) -> Result<Vec<(Name, Value)>, String> {
        let argv = |words: &[&str]| -> Argv { serde_json::from_value(json!(words)).unwrap() };
        match body.def.as_str() {
            "check" => {
                target.put(&AbsolutePath::from_static("/work/payload"))?;
                let dir = keywords["dir"].as_str().unwrap();
                target.run(&argv(&["list", dir]))?;
                let stdout = AbsolutePath::from_static("/pumpkin/log/0/command.stdout");
                let printed = target.get(&stdout)?;
                let label = Name::from_static("label");
                let ir::ArgumentValue::Str(given) = &arguments[&label] else {
                    panic!("`check`'s `label` is a str")
                };
                Ok(vec![
                    (Name::from_static("printed"), json!(printed)),
                    (label, json!(given)),
                ])
            }
            "train" => {
                target.put(&AbsolutePath::from_static("/work/payload"))?;
                target.run(&argv(&["judge", "/work/payload", "/work/result.json"]))?;
                let result = target.get(&AbsolutePath::from_static("/work/result.json"))?;
                let result: Value = serde_json::from_str(&result).unwrap();
                let score = result["raw_score"].clone();
                Ok(vec![(Name::from_static("raw_score"), score)])
            }
            "run" => {
                target.put(&AbsolutePath::from_static("/pumpkin/in/payload/raw"))?;
                target.run(&serde_json::from_value(keywords["argv"].clone()).unwrap())?;
                Ok(Vec::new())
            }
            "board" => board(target),
            "slow" => {
                target.run(&argv(&["slow"]))?;
                Ok(Vec::new())
            }
            "again" => {
                target.run(&argv(&["ops", "sleep 2"]))?;
                target.run(&argv(&["print", "two"]))?;
                let printed =
                    target.get(&AbsolutePath::from_static("/pumpkin/log/1/command.stdout"))?;
                Ok(vec![(Name::from_static("printed"), json!(printed))])
            }
            "holds" => {
                target.run(&argv(&["print", "one"]))?;
                target.get(&AbsolutePath::from_static("/work/held"))?;
                target.run(&argv(&["print", "two"]))?;
                Ok(Vec::new())
            }
            "waits" => {
                target.run(&argv(&["print", "one"]))?;
                // The core stops the run's calls as the agent's work ends, before the agent's
                // teardown, whose delete of its sandbox the record then shows.
                let deleted =
                    r#""kind":"sandbox_deleted","provider":"simulated","sandbox":"agent""#;
                sim::logged(self.record, deleted);
                target.run(&argv(&["print", "two"]))?;
                Ok(Vec::new())
            }
            "missing" => {
                target.get(&AbsolutePath::from_static("/work/missing"))?;
                Ok(Vec::new())
            }
            "deep" => {
                let depth = keywords["depth"].as_u64().unwrap();
                let nested = (2..depth).fold(json!([]), |inner, _| json!([inner]));
                Ok(vec![(Name::from_static("deep"), nested)])
            }
            "large" => Ok(large()),
            def => Err(format!("the tests have no body `{def}`")),
        }
    }

    /// `final_call` puts the payload at `/pumpkin/in/payload/raw`, runs `judge` on it, and returns
    /// its result's `raw_score` as `score`; `routed` runs its keyword `argv`, and returns the
    /// `raw_score` of the result that it wrote as `score` and `raw`; `large` returns [`large()`].
    fn final_call(
        &self,
        body: &ir::FinalBody,
        keywords: &Map<String, Value>,
        target: &mut dyn runtime::Target,
    ) -> Result<Vec<(Name, Value)>, String> {
        let argv = |words: &[&str]| -> Argv { serde_json::from_value(json!(words)).unwrap() };
        match body.def.as_str() {
            "final_call" => {
                let payload = AbsolutePath::from_static("/pumpkin/in/payload/raw");
                target.put(&payload)?;
                target.run(&argv(&["judge", payload.as_str()]))?;
                let score = raw_score(target)?;
                Ok(vec![(Name::from_static("score"), score)])
            }
            "selections" => {
                assert_eq!(target.caller(), None);
                let path = AbsolutePath::from_static("/work/manifest.json");
                target.put(&path)?;
                let manifest: Value = serde_json::from_str(&target.get(&path)?).unwrap();
                let mut files = Map::new();
                for entry in manifest.as_array().unwrap() {
                    let role = entry["role"].as_str().unwrap();
                    let Some(path) = entry["selection"]["path"].as_str() else {
                        continue;
                    };
                    // Read the staged bytes through the sandbox, including non-UTF-8 selections.
                    target.run(&argv(&["bytes", path]))?;
                    let command = files.len();
                    let stdout =
                        AbsolutePath::try_from(format!("/pumpkin/log/{command}/command.stdout"))
                            .unwrap();
                    files.insert(
                        role.to_owned(),
                        serde_json::from_str(&target.get(&stdout)?).unwrap(),
                    );
                }
                Ok(vec![
                    (Name::from_static("manifest"), manifest),
                    (Name::from_static("files"), json!(files)),
                ])
            }
            "routed" => {
                target.run(&serde_json::from_value(keywords["argv"].clone()).unwrap())?;
                let score = raw_score(target)?;
                let raw = score.clone();
                Ok(vec![
                    (Name::from_static("score"), score),
                    (Name::from_static("raw"), raw),
                ])
            }
            "large" => Ok(large()),
            def => Err(format!("the tests have no body `{def}`")),
        }
    }

    /// `submit` selects the payload and, given `finalize`, ends the work, once it has taken
    /// [`DEEP_BYTES`] of stack.
    fn now(
        &self,
        body: &ir::Body,
        arguments: &BTreeMap<Name, ir::ArgumentValue>,
        run: &mut dyn runtime::RunState,
    ) -> Result<Vec<(Name, Value)>, String> {
        match body.def.as_str() {
            "submit" => {
                deep(DEEP_BYTES / FRAME_BYTES);
                run.select();
                let finalize = arguments.get(&Name::from_static("finalize"))
                    == Some(&ir::ArgumentValue::Bool(true));
                if finalize {
                    run.end_work();
                }
                let state = if finalize { "accepted" } else { "stored" };
                Ok(vec![(Name::from_static("state"), json!(state))])
            }
            def => Err(format!("the tests have no body `{def}`")),
        }
    }
}

/// A return of a string of [`RESULT_MAX_BYTES`], which its quotes and its dict take past that as
/// JSON.
fn large() -> Vec<(Name, Value)> {
    let text = "x".repeat(usize::try_from(RESULT_MAX_BYTES).unwrap());
    vec![(Name::from_static("large"), json!(text))]
}

const FRAME_BYTES: usize = 64 << 10;

// Takes `frames` frames of stack, each of `FRAME_BYTES`.
#[expect(
    clippy::large_stack_arrays,
    reason = "the frame is on the stack to take its room there"
)]
fn deep(frames: usize) -> u8 {
    let mut frame = [0_u8; FRAME_BYTES];
    std::hint::black_box(&mut frame);
    if frames == 0 {
        frame[0]
    } else {
        deep(frames - 1).wrapping_add(frame[1])
    }
}

fn raw_score(target: &mut dyn runtime::Target) -> Result<Value, String> {
    let result = target.get(&AbsolutePath::from_static("/pumpkin/out/result.json"))?;
    let mut result: Value = serde_json::from_str(&result).map_err(|error| error.to_string())?;
    Ok(result["raw_score"].take())
}

fn board(target: &mut dyn runtime::Target) -> Result<Vec<(Name, Value)>, String> {
    let caller = target.caller().unwrap().to_owned();
    let path = AbsolutePath::from_static("/work/message");
    if caller == "alpha" {
        target.put(&path)?;
    }
    target.run(&serde_json::from_value(json!(["print", caller])).unwrap())?;
    let message = target.get(&path)?;
    Ok(vec![(Name::from_static("message"), json!(message))])
}
