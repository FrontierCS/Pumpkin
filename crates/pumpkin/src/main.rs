//! `pumpkin`, the operator's command line, which [`cli`] parses and describes, as
//! `pumpkin --help` prints it. What a command says of an ending or a refusal ends with the
//! commands to run next, as [`next`] makes them. All that it writes on this machine, but what a
//! command names, as `--runs` does, goes in the workspace's directory of it ([`Workspace`]).

use std::io::{self, Write as _};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::SystemTime;
use std::{env, fmt, fs};

use driver::ARGV0;
use engine::{ContextName, Engine};
use provider::UnixTime;
use runtime::Record;
use rustix::fs::Mode;
use rustix::process::umask;
use stdx::Context;
use workspace::Workspace;

mod cli;
mod config;
mod controller;
mod driver;
mod ending;
mod job;
mod lock;
mod manifest;
mod next;
mod progress;
mod publish;
mod reaping;
mod run;
mod sandboxd;
mod star;
mod style;
mod view;
mod workspace;

use ending::{end, follow, followed};
use reaping::{Reaper, UNREACHED, cancel, cancelled, reap};
use run::{driver_run, run, start};

/// The file name of the Modal provider's helper, which `cargo xtask install` installs beside
/// `pumpkin`, with its lock, of this name and `.lock`.
const MODAL_HELPER: &str = "pumpkin-modal.py";

fn main() -> ExitCode {
    // A run's judge calls, each session's reader, and the evaluation of `check` and `build` run
    // on threads of their own.
    stdx::abort_on_panic();
    // A run reads back what its record holds, such as a payload or a judge's result, and the
    // operator trusts the outcome that it, `cancel` or `reap` writes there; a umask such as 002
    // would leave all of it writable by a group that may have other members. So every `pumpkin`
    // process, and each program it starts, makes nothing that another user may write, as a
    // service does under systemd's default `UMask=0022`.
    let shared_write = Mode::from_raw_mode(0o022);
    let started = umask(shared_write);
    umask(started | shared_write);
    let mut args = env::args_os();
    let argv0 = args.next();
    if argv0.is_some_and(|name| name.as_bytes() == ARGV0.to_bytes()) {
        return finish(driver_run(args.collect()));
    }
    let done = match cli::parse(args) {
        Ok(cli::Command::Run(flags)) => run(flags),
        Ok(cli::Command::Wait(flags)) => wait(flags),
        Ok(cli::Command::Ls(flags)) => ls(flags),
        Ok(cli::Command::Cancel(flags)) => cancel(flags),
        Ok(cli::Command::Reap(flags)) => reap(flags, Reaper::Command),
        Ok(cli::Command::Check(flags)) => check(flags),
        Ok(cli::Command::Fmt(flags)) => style::fmt(flags),
        Ok(cli::Command::Build(flags)) => build(flags, Reader::Operator),
        Ok(cli::Command::Lock(flags)) => lock::lock(flags),
        Ok(cli::Command::View(flags)) => view::view(flags),
        Ok(cli::Command::Job(flags)) => job::job(flags),
        Ok(cli::Command::Boot) => job::boot(),
        Ok(cli::Command::Follow(id)) => job::follow(&id),
        Ok(cli::Command::CancelFor(flags)) => job::cancel(flags),
        Ok(cli::Command::ViewFor) => view::over_pipe(),
        Ok(cli::Command::Publish(flags)) => publish::by_hand(flags),
        Ok(cli::Command::Print(text)) => print(text.as_bytes())
            .context("printing to the standard output failed")
            .map(|()| ExitCode::SUCCESS),
        Err(refusal) => return usage_error(refusal),
    };
    finish(done)
}

/// Says `refusal` as clap words it, then the command that prints the help it breaks, and returns
/// 2, clap's exit status for a usage error.
fn usage_error(refusal: cli::Refusal) -> ExitCode {
    let cli::Refusal { error, help } = refusal;
    to_stderr(error.render().to_string().as_bytes());
    say(format_args!("to read how to use it: {help}"));
    ExitCode::from(2)
}

/// The exit status of a command that ended with `done`: its own, or 1 with the error said.
fn finish(done: Result<ExitCode, String>) -> ExitCode {
    match done {
        Ok(exit) => exit,
        Err(message) => {
            say(message);
            ExitCode::FAILURE
        }
    }
}

/// Who reads what a command says of a run or a build: the operator, at this machine, or a
/// follower of a controller's job, at a client, who follows the run already, and can neither open
/// this machine's paths nor change its setup.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reader {
    Operator,
    Follower,
}

impl Reader {
    /// The word that names it among the driver's arguments.
    pub(crate) fn word(self) -> &'static str {
        match self {
            Self::Operator => "operator",
            Self::Follower => "follower",
        }
    }

    fn from_word(word: &str) -> Option<Self> {
        [Self::Operator, Self::Follower]
            .into_iter()
            .find(|reader| reader.word() == word)
    }
}

/// The runs directory that `given`, a command's `--runs`, names, or without it [`default_runs`];
/// and how the commands that `pumpkin` says to run next name it: `given` made absolute
/// ([`absolute_utf8`]), or nothing, for the default.
fn runs_dir(given: Option<String>) -> Result<(String, Option<String>), String> {
    match given {
        Some(runs) => {
            let named = absolute_utf8(&runs, "the runs directory")?;
            Ok((runs, Some(named)))
        }
        None => Ok((default_runs()?, None)),
    }
}

/// `path`, `what` the operator named, such as `the runs directory`, made absolute, which a
/// record keeps, and so UTF-8.
fn absolute_utf8(path: &str, what: &str) -> Result<String, String> {
    let absolute = std::path::absolute(path).map_err(|error| {
        format!("{what} `{path}` can't be made absolute ({error}): name it by its absolute path")
    })?;
    absolute
        .into_os_string()
        .into_string()
        .map_err(|absolute| not_utf8(what, path, Path::new(&absolute)))
}

/// Why `what`, which the operator named as `path`, can't be named by the commands that `pumpkin`
/// says to run next: `absolute`, as it is made absolute, isn't UTF-8.
fn not_utf8(what: &str, path: &str, absolute: &Path) -> String {
    format!(
        "{what} `{path}` made absolute is {}, which isn't UTF-8, as the current directory's \
         path isn't: name it by its absolute path",
        absolute.display()
    )
}

/// Waits until the run that `flags` name, in their runs directory or [`default_runs`], has
/// ended, and prints its outcome; returns the exit status that `wait`'s help gives.
fn wait(flags: cli::Wait) -> Result<ExitCode, String> {
    let cli::Wait { run, runs } = flags;
    if let (cli::RunName::Prefix(prefix), None) = (&run, &runs)
        && let Some(client) = config::client(None)?
    {
        // A run that has ended, whose record is copied here, needs no controller.
        if let Ok(record) = Record::find(Path::new(&default_runs()?), prefix)
            && let Some(ending) = record.ending()?
        {
            return end(&record, &ending, None, &controller::here(&record, None)?);
        }
        return controller::wait(&client, prefix);
    }
    let (record, named) = found(run, runs)?;
    follow(&record, "wait", named.as_deref())
}

/// Lists the runs in the runs directory that `flags` name, or [`default_runs`], whose ids start
/// with one of their prefixes, oldest first, a line for each, and their totals
/// ([`progress::lines`]); returns the exit status that `ls`'s help gives. The default directory
/// not being there yet is no runs.
fn ls(flags: cli::Ls) -> Result<ExitCode, String> {
    let cli::Ls { runs, prefixes } = flags;
    if runs.is_none()
        && let Some(client) = config::client(None)?
    {
        return controller::ls(&client, &prefixes);
    }
    let (runs, named) = runs_dir(runs)?;
    let now = unix_now();
    let since = progress::listed_since(&prefixes, now);
    let (listing, unlisted) = listed_in(Path::new(&runs), named.is_none(), &prefixes, since)?;
    let lines = progress::lines(&listing, now, named.as_deref());
    print(lines.as_bytes()).context("printing the runs failed")?;
    if unlisted > 0 {
        let runs = counted(unlisted, "run");
        return Err(format!("{runs} can't be listed, as said above"));
    }
    Ok(ExitCode::SUCCESS)
}

/// The time now, or the Unix epoch itself on a clock that reads before it.
fn unix_now() -> UnixTime {
    UnixTime::of(SystemTime::now()).unwrap_or(UnixTime::after_epoch(std::time::Duration::ZERO))
}

/// Each run in `runs` whose id starts with one of `prefixes`, or every run without any, as `ls`
/// lists it, but those that ended before `since`, if given, since the Unix epoch, which it leaves
/// out; and how many can't be listed, each of which is said; none if `runs` is the `default`
/// directory, and not there yet.
fn listed_in(
    runs: &Path,
    default: bool,
    prefixes: &[String],
    since: Option<std::time::Duration>,
) -> Result<(progress::Listing, usize), String> {
    let mut listing = progress::Listing::default();
    if default && !runs.try_exists().context(runs.display())? {
        return Ok((listing, 0));
    }
    let mut unlisted = 0;
    for record in Record::all(runs)? {
        if let Ok(record) = &record
            && !picked(prefixes, record.id())
        {
            continue;
        }
        let run = record.and_then(|record| {
            let id = record.id().to_owned();
            if let Some(since) = since
                && ended(record.dir())?.is_some_and(|ended| ended < since)
            {
                return Ok((id, None));
            }
            Ok((id, Some(progress::Listed::run(&record, record.ending()?)?)))
        });
        match run {
            Ok((id, Some(run))) => {
                listing.runs.insert(id, run);
            }
            Ok((id, None)) => {
                listing.older.insert(id);
            }
            Err(error) => {
                say(error);
                unlisted += 1;
            }
        }
    }
    Ok((listing, unlisted))
}

/// When the run recorded in `dir` ended, as it wrote its `outcome.json`, since the Unix epoch;
/// `None` while it has none, as it is live, or lost.
fn ended(dir: &Path) -> Result<Option<std::time::Duration>, String> {
    let outcome = dir.join("outcome.json");
    let modified = fs::metadata(&outcome).and_then(|metadata| metadata.modified());
    match modified {
        Ok(modified) => Ok(Some(
            (modified.duration_since(SystemTime::UNIX_EPOCH)).unwrap_or_default(),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{}: {error}", outcome.display())),
    }
}

/// Whether the run `id` is one that `prefixes` pick: one of them starts it, or there are none.
fn picked(prefixes: &[String], id: &str) -> bool {
    prefixes.is_empty()
        || prefixes
            .iter()
            .any(|prefix| id.starts_with(prefix.as_str()))
}

/// The record of the run that `run` names, in `runs` ([`runs_dir`]), and the runs directory that
/// a command run next names it in: the record's own, absolute, unless it was found in the default
/// one. Fails if the record's directory, made absolute, isn't UTF-8.
fn found(run: cli::RunName, runs: Option<String>) -> Result<(Record, Option<String>), String> {
    let path = match run {
        cli::RunName::Record(path) => path,
        cli::RunName::Prefix(prefix) => {
            let (dir, named) = runs_dir(runs)?;
            return Ok((Record::find(Path::new(&dir), &prefix)?, named));
        }
    };
    // A path needs no runs directory, and so no `HOME`.
    let record = Record::open(Path::new(&path))?;
    // A record's directory is absolute, and named by its id, so it has a parent; a UTF-8 one is
    // what the commands that follow rely on.
    match record.dir().parent().and_then(Path::to_str) {
        Some(named) => {
            let named = named.to_owned();
            Ok((record, Some(named)))
        }
        None => Err(not_utf8("the record", &path, record.dir())),
    }
}

/// Checks each task that `flags` name, as `pumpkin check --help` says, saying each of their
/// problems, if they have any; returns 0 if they have none.
fn check(flags: cli::Check) -> Result<ExitCode, String> {
    let cli::Check {
        entries,
        args: cli::TaskArgs(args),
    } = flags;
    let formatter = style::Formatter::beside()?;
    let mut failed = Vec::new();
    for entry in &entries {
        let checked = match star::check(entry, &args, &formatter) {
            Ok(Ok(())) => continue,
            Ok(Err(problems)) => task_problems(entry, &problems),
            Err(error) => format!("{entry}: {error}"),
        };
        say(checked);
        failed.push(entry);
    }
    if failed.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }
    let again = source_again("check", &failed, &args, None)?;
    let them = if failed.len() == 1 { "it" } else { "them" };
    Err(format!("to check {them} again: {again}"))
}

/// Builds the task that `flags` name, as `pumpkin build --help` says, on the engine that the
/// configuration names, saying each of its problems if it has any, and else printing its IR;
/// returns 0 once the IR is printed. It says the engine to `reader` ([`engine()`]).
fn build(flags: cli::Build, reader: Reader) -> Result<ExitCode, String> {
    if let Some(client) = config::client(flags.config.as_deref())? {
        return controller::build(&client, flags);
    }
    let cli::Build {
        entry,
        args: cli::TaskArgs(args),
        config,
    } = flags;
    let again = source_again("build", &[&entry], &args, config.as_deref())?;
    match built(&entry, &args, config, reader) {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err(error) => {
            say(error);
            Err(format!("to build it again: {again}"))
        }
    }
}

/// What [`build`] does, but for the command that ends a failure; with `config`, the
/// configuration's path, if given ([`config_path`]).
fn built(
    entry: &star::Entry,
    args: &lang::Args,
    config: Option<String>,
    reader: Reader,
) -> Result<(), String> {
    let built = star::checked(entry, args, |task| {
        let engine = configured_engine(&config_path(config)?, reader)?;
        Ok(task.build(&engine, &mut ()))
    })?;
    let (workflow, _) = built.map_err(|problems| task_problems(entry, &problems))?;
    let ir = runtime::ir_json(&workflow);
    print(ir.as_bytes()).context("printing the IR failed")
}

/// The command that runs `pumpkin <command>`, `check` or `build`, again on the tasks whose
/// entries are `entries`, given `args`, and for `build`, the configuration at `config`, if given;
/// its paths absolute ([`absolute_utf8`]).
fn source_again(
    command: &str,
    entries: &[&star::Entry],
    args: &lang::Args,
    config: Option<&str>,
) -> Result<String, String> {
    let mut flags = arg_flags(args).collect::<Vec<String>>();
    if let Some(config) = config {
        let config = absolute_utf8(config, "the configuration")?;
        flags.extend(["--config".to_owned(), config]);
    }
    let entries: Result<Vec<String>, String> = (entries.iter())
        .map(|entry| absolute_utf8(entry.path(), "the entry"))
        .collect();
    Ok(next::again(command, &flags, &entries?))
}

/// The words of the flags that give `args`, as `--arg <name>=<value>` each.
fn arg_flags(args: &lang::Args) -> impl Iterator<Item = String> {
    let flags = args
        .iter()
        .map(|(name, value)| ["--arg".to_owned(), format!("{name}={value}")]);
    flags.flatten()
}

/// Says `problems`, those of the task whose entry is `entry`, and returns the line that closes
/// them.
fn task_problems(entry: &star::Entry, problems: &lang::Problems) -> String {
    // Each problem reads as its first line names its place and those after show it, so only the
    // line that closes them is said as `pumpkin`'s.
    to_stderr(format!("{problems}\n").as_bytes());
    closing_problems(format_args!("{entry}: the task has"), problems.count())
}

/// `text`, which a sandbox may have shaped, on one line for a terminal: each control character
/// but a tab escaped, a newline as `\n`.
fn escaped(text: &str) -> String {
    let mut shown = String::new();
    for c in text.chars() {
        // Control characters would steer the operator's terminal.
        if c.is_control() && c != '\t' {
            shown.extend(c.escape_debug());
        } else {
            shown.push(c);
        }
    }
    shown
}

/// The workspace's runs directory ([`Workspace::runs`]); failing if it isn't UTF-8, as the
/// commands that `pumpkin` says to run next name it.
fn default_runs() -> Result<String, String> {
    let runs = Workspace::find()?.runs();
    runs.into_os_string().into_string().map_err(|runs| {
        format!(
            "the workspace's runs directory {} isn't UTF-8, as the commands that `pumpkin` says \
             to run next name it: move the workspace to a UTF-8 path, or name a UTF-8 directory \
             with `--runs`",
            Path::new(&runs).display()
        )
    })
}

/// The configuration's path: `given`, by `--config`, or else [`default_config`].
fn config_path(given: Option<String>) -> Result<PathBuf, String> {
    given.map_or_else(default_config, |config| Ok(PathBuf::from(config)))
}

/// The Docker engine that the configuration at `path` names, said to `reader` ([`engine()`]).
fn configured_engine(path: &Path, reader: Reader) -> Result<Engine, String> {
    let config::Config {
        docker: config::Docker { context, cpus: _ },
        providers: _,
        gateway: _,
        upstreams: _,
        publish: _,
    } = config::read(path)?;
    engine(context, reader)
}

/// `$HOME/.config/pumpkin/pumpkin.toml`.
fn default_config() -> Result<PathBuf, String> {
    Ok(home("name the configuration with `--config`")?.join(".config/pumpkin/pumpkin.toml"))
}

/// The home directory, `HOME`; failing without one, the error says what the operator can do
/// `instead`.
fn home(instead: &str) -> Result<PathBuf, String> {
    // From `HOME` alone, not `XDG_CONFIG_HOME`: a run already reads `HOME`, for `docker`, so
    // where its configuration is takes no variable of its own.
    let home = env::var_os("HOME").filter(|home| !home.is_empty());
    home.map(PathBuf::from)
        .ok_or_else(|| format!("HOME is unset or empty: set it, or {instead}"))
}

/// The Modal provider's helper, `pumpkin-modal.py`, beside the `pumpkin` executable that runs, as
/// `cargo xtask install` installs it, with its lock beside it. Fails if the helper or its lock
/// isn't beside it.
fn modal_helper() -> Result<PathBuf, String> {
    let pumpkin = workspace::exe("expected Modal's helper beside it")?;
    let helper = pumpkin.with_file_name(MODAL_HELPER);
    let lock = pumpkin.with_file_name(format!("{MODAL_HELPER}.lock"));
    for file in [&helper, &lock] {
        if !file.is_file() {
            return Err(format!(
                "{} doesn't exist: expected Modal's helper and its lock beside `pumpkin`, as \
                 `cargo xtask install` installs them",
                file.display()
            ));
        }
    }
    Ok(helper)
}

/// The line that closes a list of `count` problems, which says that `has` them, and how many.
/// The command that runs again follows it.
fn closing_problems(has: fmt::Arguments<'_>, count: usize) -> String {
    let problems = counted(count, "problem");
    format!("{has} {problems}")
}

/// `count` and `noun`, plural unless `count` is 1, as "1 problem", "2 problems", "2 entries" or
/// "2 sandboxes": every count of a thing that Pumpkin says is said so.
fn counted<C: Copy + fmt::Display + PartialEq + From<u8>>(count: C, noun: &str) -> String {
    if count == C::from(1) {
        return format!("{count} {noun}");
    }
    // English's plural of each noun that Pumpkin counts.
    let consonant_y = noun
        .strip_suffix('y')
        .filter(|stem| !stem.ends_with(['a', 'e', 'o', 'u']));
    match consonant_y {
        Some(stem) => format!("{count} {stem}ies"),
        None if noun.ends_with(['s', 'x']) || noun.ends_with("sh") || noun.ends_with("ch") => {
            format!("{count} {noun}es")
        }
        None => format!("{count} {noun}s"),
    }
}

/// The Docker engine that `context` reaches, reported, which writes in the workspace's directory
/// of it ([`Workspace::docker`]). A rootful engine is announced to the operator, since its
/// `docker` group is root-equivalent; a follower can't change it ([`Reader`]).
fn engine(context: ContextName, reader: Reader) -> Result<Engine, String> {
    let engine = Engine::connect(context, &Workspace::find()?.docker())?;
    let report = engine.report();
    if reader == Reader::Operator && !report.security.rootless {
        say(format_args!(
            "the Docker engine that context `{}` reaches runs as root, so its `docker` group is \
             root-equivalent: rootless Docker is recommended",
            report.context.as_str()
        ));
    }
    Ok(engine)
}

/// Prints the run's `outcome.json`, from its record in `dir`, on the standard output.
fn print_outcome(dir: &Path) -> Result<(), String> {
    let recorded = dir.join("outcome.json");
    let printed = fs::read(&recorded).context(recorded.display())?;
    print(&printed).context("printing the outcome failed")
}

/// Writes `bytes` to the standard output, and flushes it.
fn print(bytes: &[u8]) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(bytes)?;
    stdout.flush()
}

/// Says the `command` to run next, to do what `to` says, as `pumpkin: to <to>: <command>`, the
/// command last, so that it can be copied whole.
fn advise(to: &str, command: &str) {
    say(format_args!("to {to}: {command}"));
}

/// Writes `message` to standard error, as [`tell`] does, but drops the error of a write that
/// fails, as [`to_stderr`] does.
fn say(message: impl fmt::Display) {
    let _ = tell(message);
}

/// Writes `message` to standard error, as `pumpkin: <message>`, and returns the error of a write
/// that fails.
fn tell(message: impl fmt::Display) -> io::Result<()> {
    io::stderr().write_all(format!("pumpkin: {message}\n").as_bytes())
}

/// Writes `bytes` to standard error.
fn to_stderr(bytes: &[u8]) {
    // Writing to a terminal that has hung up fails, and `eprintln!` would panic; the exit
    // status and the run's record still say how it ended.
    let _ = io::stderr().write_all(bytes);
}
