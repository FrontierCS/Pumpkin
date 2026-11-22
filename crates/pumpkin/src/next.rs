//! The commands that `pumpkin` says to run next, which end what it says of a run's ending or of
//! a refusal. Each is a line that a POSIX shell runs as it is, with its words quoted where they
//! need to be ([`quoted`]) and its paths absolute; a flag left at its default is left out, as
//! the default is the same for the next command.
//!
//! A run is run again from its task's source, with the arguments that started it, as its record
//! keeps them ([`runtime::Record::args`]).

/// The command that runs a run again, given `args`, the arguments of the `pumpkin run` that
/// started it, each a word, as its record keeps them.
pub(crate) fn run_again(args: &[String]) -> String {
    again("run", args, &[])
}

/// `pumpkin <command>` with `flags`, each argument a word, and then `operands`, such as `check`
/// of the tasks whose entries are at `operands`, absolute paths.
pub(crate) fn again(command: &str, flags: &[String], operands: &[String]) -> String {
    let words = flags.iter().chain(operands).map(|word| quoted(word));
    format!(
        "pumpkin {command} {}",
        words.collect::<Vec<String>>().join(" ")
    )
}

/// `pumpkin <command>` of the run `id`, in `runs`, an absolute path, or in the default runs
/// directory without one: `wait` or `cancel`.
pub(crate) fn of_run(command: &str, runs: Option<&str>, id: &str) -> String {
    match runs {
        Some(runs) => format!("pumpkin {command} --runs {} {id}", quoted(runs)),
        None => format!("pumpkin {command} {id}"),
    }
}

/// `pumpkin reap` of `runs`, an absolute path, or of the default runs directory without one.
pub(crate) fn reap(runs: Option<&str>) -> String {
    match runs {
        Some(runs) => format!("pumpkin reap --runs {}", quoted(runs)),
        None => "pumpkin reap".to_owned(),
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
