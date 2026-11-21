//! Commands whose caller states what of this process's environment they get, and how a command
//! is shown, which leaves the environment out.

use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::process::Command;
use std::{env, iter};

#[derive(Clone, Copy, Debug)]
pub enum Environment {
    /// Only the variables named, those that are set.
    Only(&'static [&'static str]),
    Inherited,
    /// All but the variables that cargo sets for the package it builds, runs or tests
    /// ([`cargo_package_vars`]), for a `cargo` that this process runs.
    Cargo,
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
        Environment::Cargo => {
            for name in cargo_package_vars() {
                command.env_remove(name);
            }
        }
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

/// `command`'s program and arguments as one line, each word quoted as `sh` reads it.
///
/// A word that isn't UTF-8 is shown lossily. The line never shows the environment, which may hold
/// a secret, nor the working directory.
pub fn shown(command: &Command) -> String {
    joined(command.get_program(), command.get_args())
}

/// The name that a failure gives `command`: its program and the words after it, up to two, before
/// the first option, as options can be long.
pub(super) fn name(command: &Command) -> String {
    let args =
        (command.get_args().take(2)).take_while(|arg| !arg.as_encoded_bytes().starts_with(b"-"));
    joined(command.get_program(), args)
}

fn joined<'a>(program: &OsStr, args: impl Iterator<Item = &'a OsStr>) -> String {
    let program = quoted(program, Place::Program);
    let args = args.map(|arg| quoted(arg, Place::Argument));
    iter::once(program)
        .chain(args)
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Clone, Copy)]
enum Place {
    Program,
    Argument,
}

// The words that `sh` reads as reserved where a command's first word stands: POSIX's, and those
// that it lets a shell reserve too, as Bash and ksh do. The rest hold characters quoted anyway.
// https://pubs.opengroup.org/onlinepubs/9799919799/utilities/V3_chap02.html#tag_19_04
const RESERVED: [&str; 17] = [
    "case",
    "do",
    "done",
    "elif",
    "else",
    "esac",
    "fi",
    "for",
    "if",
    "in",
    "then",
    "until",
    "while",
    "function",
    "namespace",
    "select",
    "time",
];

/// `word`, at `place`, as `sh` reads it: as it is if `sh` takes it literally, and else in single
/// quotes. A program is quoted, too, where `sh` would read it as an assignment or a reserved word.
// As Python's `shlex.quote` quotes it, with the same characters taken literally:
// https://github.com/python/cpython/blob/0fb18b02c8ad56299d6a2910be0bab8ad601ef24/Lib/shlex.py#L321-L332
fn quoted(word: &OsStr, place: Place) -> Cow<'_, str> {
    let word = word.to_string_lossy();
    let literal = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
    let read_otherwise = match place {
        Place::Program => word.contains('=') || RESERVED.contains(&&*word),
        Place::Argument => false,
    };
    if !word.is_empty() && word.chars().all(literal) && !read_otherwise {
        return word;
    }
    // A single quote ends the quoted part, stands in double quotes, and starts another.
    Cow::Owned(format!("'{}'", word.replace('\'', r#"'"'"'"#)))
}
