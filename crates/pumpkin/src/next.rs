//! The commands that `pumpkin` says to run next, which end what it says of a run's ending or of
//! a refusal. Each is a line that a POSIX shell runs as it is, with its words quoted where they
//! need to be ([`quoted`]) and its paths absolute; a flag left at its default is left out, as
//! the default is the same for the next command.
//!
//! A run is run again from its task's source, with the arguments that started it, as its record
//! keeps them ([`runtime::Record::args`]).

use crate::controller::Name;

/// Where the runs are that a command to run next names: by the flag that names them, or by none.
#[derive(Clone, Debug)]
pub(crate) enum At {
    /// Where a command without `--runs` or `--controller` finds them.
    Default,
    /// The runs directory at an absolute path, by `--runs`.
    Runs(String),
    /// A controller's runs, by `--controller`.
    Controller(Name),
}

impl At {
    /// The runs directory `runs`, an absolute path, or the default without one.
    pub(crate) fn runs(runs: Option<&str>) -> Self {
        runs.map_or(Self::Default, |runs| Self::Runs(runs.to_owned()))
    }

    /// The flag that names where the runs are, with its value, as two words; none for the
    /// default.
    pub(crate) fn flag(&self) -> Option<[String; 2]> {
        match self {
            Self::Default => None,
            Self::Runs(runs) => Some(["--runs".to_owned(), runs.clone()]),
            Self::Controller(name) => Some(["--controller".to_owned(), name.to_string()]),
        }
    }

    /// The runs directory that `--runs` names, if it does.
    pub(crate) fn dir(&self) -> Option<&str> {
        match self {
            Self::Runs(runs) => Some(runs),
            Self::Default | Self::Controller(_) => None,
        }
    }
}

/// The command that runs a run again, given `args`, the arguments of the `pumpkin run` that
/// started it, each a word, as its record keeps them.
pub(crate) fn run_again(args: &[String]) -> String {
    again("run", args, &[])
}

/// `pumpkin <command>` with `flags`, each argument a word, and then `operands`. An example is
/// `check` of the tasks whose entries are at `operands`, absolute paths.
pub(crate) fn again(command: &str, flags: &[String], operands: &[String]) -> String {
    let words = flags.iter().chain(operands).map(|word| quoted(word));
    format!(
        "pumpkin {command} {}",
        words.collect::<Vec<String>>().join(" ")
    )
}

/// `pumpkin <command>` of the run `id`, or of the runs that `id` gives the prefixes of, where `at`
/// says: `wait`, `cancel` or `ls`.
pub(crate) fn of_run(command: &str, at: &At, id: &str) -> String {
    match at.flag() {
        Some([flag, value]) => format!("pumpkin {command} {flag} {} {id}", quoted(&value)),
        None => format!("pumpkin {command} {id}"),
    }
}

/// `pumpkin reap` of the runs where `at` says.
pub(crate) fn reap(at: &At) -> String {
    of_runs("reap", at)
}

/// `pumpkin <command>` of the runs where `at` says, such as `ls`.
pub(crate) fn of_runs(command: &str, at: &At) -> String {
    match at.flag() {
        Some([flag, value]) => format!("pumpkin {command} {flag} {}", quoted(&value)),
        None => format!("pumpkin {command}"),
    }
}

/// `word` as a POSIX shell reads it back: as it is if every character in it is one that no shell
/// treats specially, and else in single quotes. A word with a newline spans two lines, which a
/// shell still reads as one word.
pub(crate) fn quoted(word: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || "%+,-./:=@_".contains(c);
    if !word.is_empty() && word.chars().all(plain) {
        return word.to_owned();
    }
    // A single quote can't be quoted inside single quotes: it ends them, is escaped, and they
    // start again.
    format!("'{}'", word.replace('\'', r"'\''"))
}
