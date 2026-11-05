//! The process control `std` lacks: a constructor of commands that makes its caller choose
//! what a child gets of the environment.

use std::env;
use std::ffi::OsStr;
use std::process::Command;

/// What a child gets of this process's environment.
#[derive(Clone, Copy, Debug)]
pub enum Environment {
    /// Only the variables named, those that are set.
    Only(&'static [&'static str]),
    /// All of it.
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
