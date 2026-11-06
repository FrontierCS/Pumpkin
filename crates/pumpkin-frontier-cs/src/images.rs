//! A ported task's images, which the recipes in its directory build, as [`Task::build`] says.

use pumpkin_docker::Engine;
use pumpkin_schema::ir::Image;
use serde::Serialize;
use stdx::Context;

use crate::Task;

/// Where a task's directory holds the recipe of the agent's image.
const AGENT_RECIPE: &str = "agent-base/Dockerfile";

/// Where a task's directory holds the recipe of the verifier's image.
const VERIFIER_RECIPE: &str = "verifier/Dockerfile";

/// A ported task's images, by their IDs, as JSON writes them: `{"agent": <ID>, "verifier":
/// <ID>}`.
#[derive(Serialize)]
pub struct Images {
    agent: Image,
    verifier: Image,
}

impl Task {
    /// The task's images on `engine`, as [`Engine::build`] builds each, or finds it built from
    /// the same inputs, with the task's directory as its context: the agent's, from
    /// `agent-base/Dockerfile`, in the repository `pumpkin-frontier-cs/<problem id>/agent`, and
    /// the verifier's, from `verifier/Dockerfile`, in `pumpkin-frontier-cs/<problem
    /// id>/verifier`.
    ///
    /// # Errors
    ///
    /// Fails as [`Engine::build`] does, naming the image's repository.
    pub fn build(&self, engine: &Engine) -> Result<Images, String> {
        let image = |name: &str, recipe: &str| {
            let repository = format!("pumpkin-frontier-cs/{}/{name}", self.problem_id);
            (engine.build(&self.dir, recipe, &repository)).context(&repository)
        };
        Ok(Images {
            agent: image("agent", AGENT_RECIPE)?,
            verifier: image("verifier", VERIFIER_RECIPE)?,
        })
    }
}
