//! [`Engine::lock`] runs a recipe's resolver online, as the image's user. Its commands share
//! the writable layer of the recipe's image up to the step that reads the [`FetchStage`].

use std::time::Duration;

use recipe::Given;
use recipe::fetch::{FetchStage, Resolver};
use stdx::Deadline;

use crate::{
    Bound, BuildContext, Built, ContextName, Engine, REMOVE_TIMEOUT, Recipe, docker_to,
    fix_reported,
};

mod apt;

/// The repository that the images of recipes' prefixes are tagged in ([`Engine::build`]).
const REPOSITORY: &str = "pumpkin/lock";

// A resolver reads an archive's indexes and its packages' metadata, tens of megabytes from a
// server that may be slow, as a dated snapshot is, and fetches no package: this is sized only to
// turn one that hangs into a failure.
const LOCK_TIMEOUT: Duration = Duration::from_mins(30);

impl Engine {
    /// The recipe of `stage`, with its body written afresh from the files its resolver finds.
    /// It resolves in the image of the recipe up to the step that reads the stage, built with
    /// `context` and `given`'s args and named contexts as the recipe's own build is.
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
        let context = &self.report.context;
        let by = Deadline::after(LOCK_TIMEOUT);
        // Even if the client goes away before it gets the ID, the container ends and removes
        // itself. An exec sees the same apt lists and dpkg status as the update before it.
        let seconds = LOCK_TIMEOUT.as_secs().to_string();
        let start = [
            "run",
            "--detach",
            "--rm",
            "--pull=never",
            "--entrypoint=sleep",
            built.image().as_str(),
            &seconds,
        ];
        let output = docker_to(context, start, Bound::Until(by)).map_err(|e| fix_reported(&e))?;
        let id = str::from_utf8(&output.stdout)
            .map_err(|e| format!("docker printed a container ID other than UTF-8: {e}"))?
            .trim();
        let container = Container { context, id, by };
        let pins = match stage.resolver() {
            Resolver::Apt(packages) => apt::resolve(&container, packages),
        };
        let remove = ["rm", "--force", "--volumes", id];
        let removed = docker_to(context, remove, Bound::Within(REMOVE_TIMEOUT));
        let pins = match (pins, removed) {
            (pins, Ok(_)) => pins?,
            (Ok(_), Err(e)) => return Err(fix_reported(&e)),
            (Err(error), Err(e)) => return Err(format!("{error}; {}", fix_reported(&e))),
        };
        stage.written(&pins)
    }
}

struct Container<'a> {
    context: &'a ContextName,
    id: &'a str,
    by: Deadline,
}

impl Container<'_> {
    fn exec<'a>(
        &'a self,
        program: &'a str,
        args: impl IntoIterator<Item = &'a str>,
    ) -> Result<Vec<u8>, String> {
        let command = ["exec", self.id, program].into_iter().chain(args);
        let output = docker_to(self.context, command, Bound::Until(self.by)).map_err(|error| {
            format!(
                "the resolver's `{program}` failed: {}",
                fix_reported(&error)
            )
        })?;
        Ok(output.stdout)
    }

    fn apt_get<'a>(&'a self, args: impl IntoIterator<Item = &'a str>) -> Result<String, String> {
        String::from_utf8(self.exec("apt-get", args)?)
            .map_err(|e| format!("apt-get printed other than UTF-8: {e}"))
    }
}
