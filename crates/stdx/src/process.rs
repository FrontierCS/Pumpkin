//! The process control `std` lacks: a constructor of commands that makes its caller choose
//! what a child gets of the environment.
//!
//! A wait for whichever of several children exits first is [`wait_any`].

use std::ffi::OsStr;
use std::process::{Child, Command, ExitStatus};
use std::time::Duration;
use std::{env, io, thread};

/// The first pause between checks that a command has ended, which doubles up to [`MAX_PAUSE`].
const FIRST_PAUSE: Duration = Duration::from_millis(1);

/// The longest pause between checks that a command has ended.
// A command that starts a process of its own takes milliseconds at least, so ending up to 10
// ms after it is noise beside it, and the checks wake this thread at most 100 times a second.
const MAX_PAUSE: Duration = Duration::from_millis(10);

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

/// Waits until one of `children` has exited, and gives its index in `children` and how it
/// exited, having reaped it; or the index of one that can't be waited for, and why.
///
/// # Panics
///
/// Panics if `children` is empty, as none of them would ever exit.
pub fn wait_any(children: &mut [&mut Child]) -> (usize, io::Result<ExitStatus>) {
    assert!(!children.is_empty(), "a wait for one of no children");
    let mut pause = FIRST_PAUSE;
    loop {
        for (index, child) in children.iter_mut().enumerate() {
            match child.try_wait() {
                Ok(Some(status)) => return (index, Ok(status)),
                Ok(None) => {}
                Err(error) => return (index, Err(error)),
            }
        }
        thread::sleep(pause);
        pause = (pause * 2).min(MAX_PAUSE);
    }
}
