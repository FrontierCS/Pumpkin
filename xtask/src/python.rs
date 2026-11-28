//! `ci`'s check of the repository's Python. The pinned ruff ([`tools::RUFF`]) lints each file and
//! checks that it is formatted, by ruff's defaults but for the Python version it targets.

use std::path::Path;
use std::process::Command;

use stdx::process::{self, Environment, Failure};

use crate::repo::Repo;
use crate::{Held, Result, tidy, tools};

/// The options of each ruff that the check runs, and of each command that its failures say to run.
// `--isolated` ignores each configuration file, the user's too. Without `--no-cache`, ruff would
// write `.ruff_cache` into the checkout. The examples' `python:3.10-slim` images run the oldest
// Python here. Ruff's default target, 3.11, allows fixes that 3.10 can't run:
// https://github.com/astral-sh/ruff/blob/a7ba68b4919e56b6af949a42b2da38fe4eec67a9/crates/ruff_workspace/src/options.rs#L387-L392
const OPTIONS: [&str; 4] = ["--isolated", "--no-cache", "--target-version", "py310"];

/// Fails unless each of the repository's `.py` files, tracked or not yet, but not ignored, passes
/// ruff's lint rules and is formatted. Ruff prints each finding at its line, and the diff of each
/// file that isn't formatted.
pub(crate) fn check(held: &mut Held, repo: &Repo<'_>) -> Result<()> {
    let files = tidy::files(repo, "py")?;
    // With no file named, ruff would check the working directory.
    if files.is_empty() {
        return Ok(());
    }
    let ruff = tools::fetched(tools::RUFF)?;
    let options = OPTIONS.join(" ");
    // The lint goes first, as a fix of its findings can leave code that the formatter changes.
    let mut lint = command(&ruff, repo, "check");
    // Each command exits with 1 where it found a problem, and with 2 where it couldn't check:
    // https://github.com/astral-sh/ruff/blob/a7ba68b4919e56b6af949a42b2da38fe4eec67a9/docs/linter.md#L741-L749
    // https://github.com/astral-sh/ruff/blob/a7ba68b4919e56b6af949a42b2da38fe4eec67a9/docs/formatter.md#L475-L482
    match held.ran(lint.arg("--").args(&files), "ruff check") {
        Ok(_) => {}
        Err(Failure::Exited { output, .. }) if output.status.code() == Some(1) => {
            return Err(format!(
                "the Python files break ruff's lint rules, at the lines that ruff printed above: \
                 fix them, those marked `[*]` with `{} check {options} --fix` at the \
                 workspace's root, then run `cargo xtask ci` again",
                ruff.display()
            )
            .into());
        }
        Err(failure) => return Err(failure.to_string().into()),
    }
    let mut format = command(&ruff, repo, "format");
    match held.ran(format.arg("--check").arg("--").args(&files), "ruff format") {
        Ok(_) => Ok(()),
        Err(Failure::Exited { output, .. }) if output.status.code() == Some(1) => Err(format!(
            "the Python files aren't formatted, as the diff above shows: run `{} format \
             {options}` at the workspace's root, then run `cargo xtask ci` again",
            ruff.display()
        )
        .into()),
        Err(failure) => Err(failure.to_string().into()),
    }
}

/// A `ruff` with `subcommand` and [`OPTIONS`], at the repository's root.
fn command(ruff: &Path, repo: &Repo<'_>, subcommand: &str) -> Command {
    // Ruff gets none of this process's environment, so that no `RUFF_OUTPUT_FORMAT` or
    // `RUFF_OUTPUT_FILE` changes what it prints.
    let mut command = process::command(ruff, Environment::Only(&[]));
    command.current_dir(repo.root).arg(subcommand).args(OPTIONS);
    command
}
