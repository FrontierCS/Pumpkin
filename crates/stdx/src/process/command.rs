//! Commands whose caller states what of this process's environment they get.

use std::env;
use std::ffi::{OsStr, OsString};
use std::process::Command;

#[derive(Clone, Copy, Debug)]
pub enum Environment {
    /// Only the variables named, those that are set.
    Only(&'static [&'static str]),
    Inherited,
}

/// A [`Command`] that runs `program` with what `environment` says of this process's
/// environment: the one way to make a [`Command`] that `clippy.toml` allows.
pub fn command(program: impl AsRef<OsStr>, environment: Environment) -> Command {
    #[expect(
        clippy::disallowed_methods,
        reason = "the constructor that `clippy.toml` names, which makes its caller state the \
                  child's environment"
    )]
    let mut command = Command::new(program);
    match environment {
        Environment::Only(names) => {
            command.env_clear();
            let set = names
                .iter()
                .filter_map(|name| Some((name, env::var_os(name)?)));
            command.envs(set);
        }
        Environment::Inherited => {}
    }
    command
}

/// The names of the variables of this process's environment that cargo sets for the package it
/// builds, runs or tests: `CARGO_MANIFEST_DIR`, `CARGO_MANIFEST_PATH` and `CARGO_PKG_*`.
///
/// A `cargo` that this process runs is to get none of them: some build scripts, such as ring's,
/// rerun when one of them changes, so a nested build with another package's would rebuild such a
/// crate, and every crate that depends on it, and the next build outside it would again.
pub fn cargo_package_vars() -> Vec<OsString> {
    env::vars_os()
        .map(|(name, _)| name)
        .filter(|name| {
            name.to_str().is_some_and(|name| {
                name.starts_with("CARGO_PKG_")
                    || matches!(name, "CARGO_MANIFEST_DIR" | "CARGO_MANIFEST_PATH")
            })
        })
        .collect()
}
