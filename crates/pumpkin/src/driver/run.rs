//! The runtime: [`Run::prepare`] parses a workflow and begins its record, whose files the
//! `record` module lays out.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use pumpkin_schema::ir;
use pumpkin_schema::workflow::Workflow;
use serde_json::json;

mod record;

/// A run, prepared: its workflow parsed, and its record begun.
#[derive(Debug)]
pub struct Run {
    id: String,
    dir: PathBuf,
}

impl Run {
    /// Parses `ir` and makes the run's record in `runs`, which is created if need be:
    /// `ir.json`. The run's id holds `now`, the time by the caller's clock. A workflow that has a
    /// model route is refused, with `outcome.json` saying why.
    ///
    /// # Errors
    ///
    /// Fails if the IR breaks a parsing rule, if `now` is before 1970, if the workflow is
    /// refused, or if the record can't be written.
    pub fn prepare(ir: &ir::Workflow, runs: &Path, now: SystemTime) -> Result<Self, String> {
        let workflow = Workflow::parse(ir).map_err(|errors| errors.to_string())?;
        let (id, dir) = record::create(runs, now)?;
        record::write_json(&dir, "ir.json", &json!(ir))?;
        if let Some(reason) = model_route(&workflow) {
            let outcome = json!({ "state": "refused", "reason": reason });
            record::write_json(&dir, "outcome.json", &outcome)?;
            return Err(reason);
        }
        Ok(Self { id, dir })
    }

    /// The run's id, which names its record.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The run's record.
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

/// Why a workflow with a model route is refused. Parsing has refused a route on a judge.
fn model_route(workflow: &Workflow) -> Option<String> {
    let agent = &workflow.agent().sandbox;
    agent.model_route.as_ref()?;
    Some(format!(
        "`{}` has a model route, which the driver doesn't serve: remove the role's \
         `egress.model` to run without one",
        agent.name
    ))
}
