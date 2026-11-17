//! The bodies that the tests' workflows call, written in Rust.

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]

use std::path::Path;

use schema::ir::{self, AbsolutePath, Argv, Name};
use serde_json::{Map, Value, json};

use crate::sim;

/// The tests' bodies, each by its `def`'s name: `check` puts the payload at `/work/payload`, runs
/// `list <its keyword dir>`, and returns what it printed; `train` puts the payload at
/// `/work/payload`, runs `judge` on it, and returns its result's `raw_score`; `run` puts the
/// payload at `/pumpkin/in/payload/raw` and runs its keyword `argv`; `slow` runs `slow`, which runs
/// until it is killed. `again` runs a command of two seconds, then `print two`, and returns what it
/// printed; `late` runs the same command, then puts the payload and gets it back; `holds` and
/// `waits` run `print one`, then get `/work/held`, which the simulated sandbox holds, or wait for
/// the record to show the agent's sandbox deleted, then run `print two`; `missing` gets a file that
/// isn't there; what `deep` returns nests, its dict included, its keyword `depth` deep.
pub(crate) struct Bodies<'r> {
    /// The run's record.
    pub(crate) record: &'r Path,
}

impl runtime::Bodies for Bodies<'_> {
    fn stack_bytes(&self) -> usize {
        2 << 20
    }

    fn job(
        &self,
        body: &ir::Body,
        keywords: &Map<String, Value>,
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
                Ok(vec![(Name::from_static("printed"), json!(printed))])
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
            "late" => {
                target.run(&argv(&["ops", "sleep 2"]))?;
                let payload = AbsolutePath::from_static("/work/payload");
                target.put(&payload)?;
                let got = target.get(&payload)?;
                Ok(vec![(Name::from_static("got"), json!(got))])
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
            def => Err(format!("the tests have no body `{def}`")),
        }
    }
}
