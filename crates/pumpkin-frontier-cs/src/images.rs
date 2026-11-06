//! A ported task's images, which the recipes in its directory build, as [`Task::build`] says.

use std::env;
use std::fs::{self, DirBuilder, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

use pumpkin_docker::{Built, Engine};
use pumpkin_harness::codex::CODEX_0_167_0;
use pumpkin_schema::ir::Image;
use serde::Serialize;
use stdx::Context;

use crate::{Task, recipe};

/// A ported task's images, by their IDs, as JSON writes them: `{"agent": <ID>, "verifier":
/// <ID>}`.
#[derive(Serialize)]
pub struct Images {
    agent: Image,
    verifier: Image,
}

impl Task {
    /// The task's images on `engine`, as [`Engine::build`] builds each, or finds it built from
    /// the same inputs: the agent's, which is the image `agent-base/Dockerfile` builds with the
    /// task's directory as its context, in the repository `pumpkin-frontier-cs/<problem
    /// id>/agent-base`, with Codex added as [`CODEX_0_167_0`]'s recipe says, in
    /// `pumpkin-frontier-cs/<problem id>/agent`; and the verifier's, from `verifier/Dockerfile`,
    /// in `pumpkin-frontier-cs/<problem id>/verifier`.
    ///
    /// # Errors
    ///
    /// Fails as [`Engine::build`] does, naming the image's repository, as when a recipe has
    /// changed since [`Task::load`] into one the check refuses; or if the context of the build
    /// that adds Codex can't be made in the system's temporary directory.
    pub fn build(&self, engine: &Engine) -> Result<Images, String> {
        let repository = |name: &str| format!("pumpkin-frontier-cs/{}/{name}", self.problem_id);
        let image = |path: &str, name: &str| {
            let repository = repository(name);
            let check = |text: &[u8]| recipe::check(&self.dir.join(path), text);
            (engine.build(&self.dir, path, &repository, check)).context(&repository)
        };
        let base = image(recipe::AGENT, "agent-base")?;
        let repository = repository("agent");
        let with_codex = CODEX_0_167_0.recipe(&base.tag);
        let Built {
            image: agent,
            tag: _,
        } = build_alone(engine, &with_codex, &repository).context(&repository)?;
        let Built {
            image: verifier,
            tag: _,
        } = image(recipe::VERIFIER, "verifier")?;
        Ok(Images { agent, verifier })
    }
}

/// The image that `recipe`, a Dockerfile that copies nothing from its context, builds in
/// `repository`, as [`Engine::build`] builds it with a context of its own: a directory of the
/// system's temporary directory, named by this process, made only if nothing is at its path,
/// so that no other build writes there, which holds `recipe` alone and is removed afterwards.
fn build_alone(engine: &Engine, recipe: &str, repository: &str) -> Result<Built, String> {
    let dir = env::temp_dir().join(format!("pumpkin-frontier-cs-{}", std::process::id()));
    (DirBuilder::new().mode(0o700).create(&dir)).map_err(|error| {
        // A run killed during its build leaves its directory behind, in the way of a later
        // process with the same ID.
        if error.kind() == io::ErrorKind::AlreadyExists {
            format!(
                "{} is in the way of a recipe's build, as a run killed during its build leaves \
                 it: remove it",
                dir.display()
            )
        } else {
            format!(
                "{} can't be made for a recipe's build: {error}: let this user make it in the \
                 system's temporary directory",
                dir.display()
            )
        }
    })?;
    // The build's tag covers the permission bits of the directory and of the recipe, so each is
    // set whatever this process's umask.
    let file = dir.join("Dockerfile");
    let written = (fs::set_permissions(&dir, Permissions::from_mode(0o700)))
        .and_then(|()| fs::write(&file, recipe))
        .and_then(|()| fs::set_permissions(&file, Permissions::from_mode(0o600)))
        .context(file.display());
    // The recipe is Pumpkin's own (`Pin::recipe`), which no check reads: the factory's is for a
    // task's, and would refuse its `FROM` of the base by tag and its `ADD` of a URL.
    let built = written.and_then(|()| engine.build(&dir, "Dockerfile", repository, |_| Ok(())));
    let _ = fs::remove_dir_all(&dir);
    built
}
