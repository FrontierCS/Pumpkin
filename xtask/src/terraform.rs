//! `ci`'s check of the Terraform module `deploy/aws`. The pinned Terraform
//! ([`tools::TERRAFORM`]) checks that the module is formatted. Then it validates the module with
//! the pinned AWS provider, without a backend, a registry or the user's configuration.

use std::path::Path;
use std::process::Command;

use stdx::process::{self, Environment, Failure, Stream};

use crate::repo::STEP_TIMEOUT;
use crate::{Result, ran, root, step, tools};

/// The module, from the workspace's root.
const MODULE: &str = "deploy/aws";

/// Fails unless the module is formatted and valid. `terraform fmt` prints the diff of each file
/// that isn't formatted, and `terraform validate` prints each error.
pub(crate) fn check() -> Result<()> {
    let terraform = tools::fetched(tools::TERRAFORM)?;
    let plugins = tools::terraform_plugins()?;
    let mut fmt = command(&terraform);
    fmt.args(["fmt", "-check", "-diff", "-recursive", MODULE]);
    match process::run(&mut fmt, step(STEP_TIMEOUT).named("terraform fmt")) {
        Ok(_) => {}
        Err(Failure::Exited { .. }) => {
            return Err(format!(
                "{MODULE} isn't formatted, as the diff above shows: run `{} fmt -recursive \
                 {MODULE}`, then run `cargo xtask ci` again",
                terraform.display()
            )
            .into());
        }
        Err(failure) => return Err(failure.to_string().into()),
    }
    let chdir = format!("-chdir={MODULE}");
    let mut init = command(&terraform);
    init.args([&chdir, "init", "-backend=false", "-input=false"])
        .arg(format!("-plugin-dir={}", plugins.display()))
        // A lock file that an `init` from the registry left in the module may select a newer
        // provider, which the plugin directory lacks.
        .arg("-upgrade");
    // `init` prints its progress and warnings on its standard output, and its errors on its
    // standard error. Its standard output is shown only on a failure, as pre-commit-terraform
    // shows init's output:
    // https://github.com/antonbabenko/pre-commit-terraform/blob/b9e409b0fee34ee0eee7e6f6a407061f15032926/hooks/_common.sh#L859-L881
    let quiet = step(STEP_TIMEOUT).stdout(Stream::Captured);
    if let Err(failure) = process::run(&mut init, quiet.named("terraform init")) {
        if let Failure::Exited { output, .. } = &failure {
            print!("{}", String::from_utf8_lossy(&output.stdout));
        }
        return Err(failure.to_string().into());
    }
    let mut validate = command(&terraform);
    validate.args([&chdir, "validate"]);
    ran(
        &mut validate,
        step(STEP_TIMEOUT).named("terraform validate"),
    )
    .map(drop)
}

/// A `terraform`, at the workspace's root.
fn command(terraform: &Path) -> Command {
    // Terraform gets `PATH` alone of this process's environment, so that no `TF_CLI_ARGS` or
    // `TF_DATA_DIR` changes what it checks. `fmt -diff` runs `diff` from `PATH`. Terraform reads no
    // CLI configuration file, and doesn't ask HashiCorp's Checkpoint service for a newer version:
    // https://github.com/hashicorp/web-unified-docs/blob/45e8f63984f52111e8efbb7610095c195d31427a/content/terraform/v1.15.x/docs/cli/config/environment-variables.mdx#L161-L163
    // https://github.com/hashicorp/web-unified-docs/blob/45e8f63984f52111e8efbb7610095c195d31427a/content/terraform/v1.15.x/docs/cli/commands/index.mdx#L154-L155
    let mut terraform = process::command(terraform, Environment::Only(&["PATH"]));
    terraform
        .current_dir(root())
        .env("TF_CLI_CONFIG_FILE", "/dev/null")
        .env("CHECKPOINT_DISABLE", "1");
    terraform
}
