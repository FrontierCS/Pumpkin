//! [`Engine::lock`], which runs a recipe's [`Resolver`] online, as the image's user, in a
//! container of the image that the recipe builds up to the step that reads its fetch stage
//! ([`FetchStage`]). The apt resolver is the script `lock/apt.sh`.

use std::time::Duration;

use recipe::Given;
use recipe::fetch::{FetchStage, Resolver};

use crate::{Bound, BuildContext, Built, Engine, Recipe, docker_to, fix_reported};

/// The repository that the images of recipes' prefixes are tagged in ([`Engine::build`]).
const REPOSITORY: &str = "pumpkin/lock";

/// The apt resolver ([`Resolver::Apt`]): it prints a pin, `<sha256> <url> <path>`, for each file.
const APT: &str = include_str!("lock/apt.sh");

// A resolver reads an archive's indexes and its packages' metadata, tens of megabytes from a
// server that may be slow, as a dated snapshot is, and fetches no package: this is sized only to
// turn one that hangs into a failure.
const LOCK_TIMEOUT: Duration = Duration::from_mins(30);

impl Engine {
    /// The recipe of `stage`, with the stage's body written afresh from what its resolver
    /// prints, as [`FetchStage`] says, in the image of the recipe up to the step that
    /// reads the stage, built with `context` and `given`'s args and named contexts, as the
    /// recipe's own build is given them.
    pub fn lock(
        &self,
        stage: &FetchStage,
        context: &BuildContext,
        given: Given<&Built>,
    ) -> Result<String, String> {
        // The target names a stage of the whole recipe, and the step's stage is the prefix's last.
        let given = Given {
            target: None,
            ..given
        };
        let prefix = Recipe::Text(stage.prefix().to_owned());
        let built = self.build(context, &prefix, &given, REPOSITORY)?;
        let (script, args) = match stage.resolver() {
            Resolver::Apt(packages) => (APT, packages),
        };
        let image = built.image().as_str();
        let run = [
            "run",
            "--rm",
            "--pull=never",
            "--entrypoint=/bin/sh",
            image,
            "-c",
            script,
            "sh",
        ];
        let output = docker_to(
            &self.report.context,
            run.into_iter()
                .map(str::to_owned)
                .chain(args.iter().cloned()),
            Bound::Within(LOCK_TIMEOUT),
        )
        .map_err(|error| format!("the resolver failed: {}", fix_reported(&error)))?;
        let stdout = String::from_utf8(output.stdout)
            .map_err(|error| format!("the resolver printed other than UTF-8: {error}"))?;
        stage.written(&stdout)
    }
}
