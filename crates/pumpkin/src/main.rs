//! The `pumpkin` command: `main` hands each parsed command to its module, and this file holds the
//! small commands and what the modules share to speak to the person at the terminal.
//!
//! What a command says about itself goes to the standard error as lines that start `pumpkin: `
//! ([`say`]); what it prints for a person or a program to read, such as a run's id or outcome,
//! goes to the standard output ([`print()`]).

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use std::{env, fmt, fs};

use anstream::{AutoStream, ColorChoice};
use clap::builder::StyledStr;
use engine::{ContextName, Engine};
use provider::UnixTime;
use runtime::{ReadError, Record};
use rustix::fs::Mode;
use rustix::process::umask;
pub(crate) use schema::words::{counted, size};
use stdx::Context;
use workspace::Workspace;

use crate::cli::Exit;

mod bodies;
mod cli;
mod config;
mod controller;
mod deployment;
mod driver;
mod ending;
mod host;
mod iam;
mod job;
mod keydir;
mod link;
mod manifest;
mod next;
mod progress;
mod reaping;
mod run;
mod sandboxd;
mod serve;
mod star;
mod style;
mod view;
mod whole;
mod workspace;

use ending::{end, follow};
use reaping::{Reaper, cancel, reap};
use run::{driver_run, run, start};

// musl's allocator is much slower than glibc's at reading a run's records, as ripgrep found of its
// own work, and mimalloc builds with a C compiler alone, where jemalloc needs `make`:
// <https://github.com/BurntSushi/ripgrep/blob/91f5d22ef48bde459d90a8c7dbeab416e97d0f2b/crates/core/main.rs#L19-L36>
#[cfg(target_env = "musl")]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> Exit {
    stdx::abort_on_panic();
    // A run reads back what its record holds, such as a payload or a call's result, and the
    // operator trusts the outcome there; a umask such as 002 would leave all of it writable by a
    // group that may have other members. So every `pumpkin` process, and each program it starts,
    // makes nothing that another user may write, as a service does under systemd's default
    // `UMask=0022`.
    let shared_write = Mode::from_raw_mode(0o022);
    let started = umask(shared_write);
    umask(started | shared_write);
    let mut args = env::args_os();
    let argv0 = args.next();
    // One executable runs as the command that its name gives. BusyBox runs the applet that its
    // `argv[0]`'s last component names. cargo-zigbuild runs as `ar` by its name, and xtask runs as
    // the musl C compiler by its name:
    // https://git.busybox.net/busybox/tree/libbb/appletlib.c?id=bd754746394a382e04d116df02547f61b2026da9#n1108
    // https://github.com/rust-cross/cargo-zigbuild/blob/5225cf45dad1dcd4466b0828b95351c645cc20e6/src/bin/cargo-zigbuild.rs#L33-L41
    let name = argv0.as_deref().map(Path::new).and_then(Path::file_name);
    let parsed = if name.is_some_and(|name| name == cli::admin::NAME) {
        cli::admin::parse(args)
    } else {
        cli::parse(args)
    };
    let done = match parsed {
        Ok(cli::Command::Run(flags)) => run(flags),
        Ok(cli::Command::Wait(flags)) => wait(flags),
        Ok(cli::Command::Ls(flags)) => ls(flags),
        Ok(cli::Command::Cancel(flags)) => cancel(flags),
        Ok(cli::Command::Reap(flags)) => reap(flags, Reaper::Command),
        Ok(cli::Command::Check(flags)) => check(flags),
        Ok(cli::Command::Fmt(flags)) => style::fmt(flags),
        Ok(cli::Command::View(flags)) => view::view(flags),
        Ok(cli::Command::Job(flags)) => job::job(flags),
        Ok(cli::Command::Boot) => job::boot(),
        Ok(cli::Command::Driver(driver)) => driver_run(driver),
        Ok(cli::Command::Install(target)) => host::install(&target),
        Ok(cli::Command::CheckDeployment(settings)) => deployment::check(&settings),
        Ok(cli::Command::Members) => host::members(),
        Ok(cli::Command::CheckMembers(dir)) => keydir::check(&dir),
        Ok(cli::Command::BootAll) => host::boot_all(),
        Ok(cli::Command::Idle) => host::idle(),
        Ok(cli::Command::Serve(member)) => serve::serve(&member),
        Ok(cli::Command::Help(help)) => printed(&help.render()),
        Ok(cli::Command::Print(text)) => printed(&text),
        Err(refusal) => return usage_error(refusal),
    };
    finish(done)
}

fn printed(text: &StyledStr) -> Result<Exit, String> {
    // clap's own printing writes the help in many pieces. `head` can close the pipe between two of
    // them, and the next write fails. `print` writes the help in one `write_all`.
    print(styled(text).as_bytes()).context("printing to the standard output failed")?;
    Ok(Exit::Success)
}

/// Says `refusal` as clap words it, then the command that prints the help it breaks, and returns
/// [`Exit::Usage`], whose code is clap's for a usage error.
fn usage_error(refusal: cli::Refusal) -> Exit {
    let cli::Refusal { error, help } = refusal;
    to_stderr(error.render().to_string().as_bytes());
    say(format_args!("to read how to use it: {help}"));
    Exit::Usage
}

fn finish(done: Result<Exit, String>) -> Exit {
    match done {
        Ok(exit) => exit,
        Err(message) => {
            say(message);
            Exit::Failure
        }
    }
}

/// Who reads what a command says of a run. The operator reads it at this machine. A follower of
/// a controller's job reads it at a client: they follow the run already, and can neither open
/// this machine's paths nor change its setup.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reader {
    Operator,
    Follower,
}

impl Reader {
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

/// The runs directory as a string, and `given` made absolute if one was given.
///
/// The second is what the commands to run next name; with the default directory they name none.
fn runs_dir(given: Option<String>) -> Result<(String, Option<String>), String> {
    match given {
        Some(runs) => {
            let named = absolute_utf8(&runs, "the runs directory")?;
            Ok((runs, Some(named)))
        }
        None => Ok((default_runs()?, None)),
    }
}

fn absolute_utf8(path: &str, what: &str) -> Result<String, String> {
    let absolute = std::path::absolute(path).map_err(|error| {
        format!("{what} `{path}` can't be made absolute ({error}): name it by its absolute path")
    })?;
    absolute
        .into_os_string()
        .into_string()
        .map_err(|absolute| not_utf8(what, path, Path::new(&absolute)))
}

fn not_utf8(what: &str, path: &str, absolute: &Path) -> String {
    format!(
        "{what} `{path}` made absolute is {}, which isn't UTF-8, as the current directory's \
         path isn't: name it by its absolute path",
        absolute.display()
    )
}

/// The client of the controller that a command of `run` goes to, with the prefix of the run's id
/// there, given `runs` and `controller`, its flags; `None` for a run of this machine's, which
/// `--runs` or a record's path names, or with no controller in the configuration. Fails if
/// `controller` names a controller and `run` names a record's path.
fn client_of<'r>(
    run: &'r cli::RunName,
    runs: Option<&String>,
    controller: Option<controller::Name>,
) -> Result<Option<(config::Client, &'r str)>, String> {
    if runs.is_some() {
        return Ok(None);
    }
    match (run, controller) {
        (cli::RunName::Record(path), Some(name)) => Err(format!(
            "`{path}` is a record's path, and `--controller` names controller `{name}`, where a \
             run is named by its id: give the run's id, or leave out `--controller`"
        )),
        (cli::RunName::Record(_), None) => Ok(None),
        (cli::RunName::Prefix(prefix), name) => {
            let client = config::client(None, name.as_ref())?;
            Ok(client.map(|client| (client, prefix.as_str())))
        }
    }
}

fn wait(flags: cli::Wait) -> Result<Exit, String> {
    let cli::Wait {
        run,
        runs,
        controller,
        printed,
    } = flags;
    if let Some((client, prefix)) = client_of(&run, runs.as_ref(), controller)? {
        let name = &client.controller.name;
        // A run that has ended is copied here, and is read here; any other is waited for there.
        if let Ok(record) = Record::find(&controller::copies(name)?, prefix)
            && let Some(ending) = record.ending()?
        {
            let args = controller::here(&record, None, Some(name))?;
            let at = next::At::Controller(name.clone());
            return end(&record, &ending, printed, &at, &args);
        }
        return controller::wait(&client, prefix, printed);
    }
    let (record, named) = found(run, runs)?;
    follow(&record, "wait", &next::At::runs(named.as_deref()), printed)
}

fn ls(flags: cli::Ls) -> Result<Exit, String> {
    let cli::Ls {
        runs,
        controller,
        all,
        json,
        prefixes,
    } = flags;
    if runs.is_none()
        && let Some(client) = config::client(None, controller.as_ref())?
    {
        return controller::ls(&client, &prefixes, all, json);
    }
    let (runs, named) = runs_dir(runs)?;
    let at = next::At::runs(named.as_deref());
    let now = unix_now();
    let since = progress::listed_since(&prefixes, all, now);
    let (listing, unlisted) = listed_in(Path::new(&runs), &prefixes, since, now, &at)?;
    said_beside(&listing, &unlisted);
    printed_listing(&listing, &unlisted, &at, since, json)?;
    if !unlisted.is_empty() {
        let runs = counted(unlisted.len(), "run");
        return Err(format!("{runs} can't be listed, as said above"));
    }
    Ok(Exit::Success)
}

/// Prints `listing` and `unlisted`, the runs it couldn't list, as `ls` does. Its runs that ended
/// before `since` are older. It prints JSON if `json`, else lines laid out for the standard output.
fn printed_listing(
    listing: &progress::Listing,
    unlisted: &[progress::Unlisted],
    at: &next::At,
    since: Option<UnixTime>,
    json: bool,
) -> Result<(), String> {
    let (styled, width) = terminal();
    let printed = if json {
        // Indented where the output is styled, for a person, and compact for a program, as gh
        // prints its own:
        // https://github.com/cli/cli/blob/529111a21612ff1e29dc45e567a157df24e0e243/pkg/cmdutil/json_flags.go#L225-L257
        progress::json(listing, unlisted, styled)
    } else {
        let scope = match since {
            Some(_) => progress::Scope::Day,
            None => progress::Scope::Every,
        };
        let layout = progress::Layout {
            scope,
            width,
            styled,
        };
        progress::lines(listing, at, &layout)
    };
    print(printed.as_bytes()).context("printing the runs failed")
}

/// Whether what goes to the standard output is styled, as anstream decides it for clap's help. Then
/// how many columns a line there may take: as many as `COLUMNS` says, else as many as its terminal
/// has, else no bound.
// `COLUMNS` comes first, as POSIX has it override the terminal's width, and as git reads it:
// https://github.com/git/git/blob/73876f4861cd3d187a4682290ab75c9dccadbc56/pager.c#L158-L187
fn terminal() -> (bool, Option<usize>) {
    let stdout = io::stdout();
    let styled = match AutoStream::choice(&stdout) {
        ColorChoice::Never => false,
        ColorChoice::Auto | ColorChoice::AlwaysAnsi | ColorChoice::Always => true,
    };
    let columns = env::var("COLUMNS")
        .ok()
        .and_then(|columns| columns.parse().ok());
    let width = columns.filter(|&columns: &usize| columns > 0).or_else(|| {
        let size = rustix::termios::tcgetwinsize(&stdout).ok()?;
        (size.ws_col > 0).then_some(usize::from(size.ws_col))
    });
    (styled, width)
}

/// Says what `ls` says before the table of `listing`, by id: each run that `unlisted` holds, and
/// what of each run's log this `pumpkin` can't read.
fn said_beside(listing: &progress::Listing, unlisted: &[progress::Unlisted]) {
    let unread = listing.runs.iter().filter_map(|(id, entry)| {
        let unread = entry.row()?.unread.as_ref()?;
        Some((id, format!("run {id}: {unread}")))
    });
    let unlisted = unlisted.iter().flat_map(|run| {
        let unread =
            (run.unread.as_ref()).map(|unread| (&run.id, format!("run {}: {unread}", run.id)));
        unread.into_iter().chain([(&run.id, run.message.clone())])
    });
    // Stable, so that a run's note comes before why it can't be listed.
    let mut said: Vec<(&String, String)> = unread.chain(unlisted).collect();
    said.sort_by_key(|(id, _)| *id);
    for (_, message) in said {
        say(message);
    }
}

/// The time `since_epoch` after the Unix epoch, or the latest time jiff holds if that is past it.
fn utc(since_epoch: std::time::Duration) -> jiff::Timestamp {
    let at = jiff::Timestamp::UNIX_EPOCH.saturating_add(since_epoch);
    at.unwrap_or(jiff::Timestamp::MAX)
}

const EPOCH: UnixTime = UnixTime::after_epoch(std::time::Duration::ZERO);

fn unix_now() -> UnixTime {
    UnixTime::of(SystemTime::now()).unwrap_or(EPOCH)
}

/// The runs of `runs` whose ids start with one of `prefixes`, or every run if there are none, and
/// those that can't be listed. A run that ended before `since` is listed as older, and one that
/// another build made and this can't read is an `unread` row. Each row is made at `now`, with its
/// commands naming the runs where `at` says. A `runs` that is missing is an empty listing unless
/// `at` names it by `--runs`.
fn listed_in(
    runs: &Path,
    prefixes: &[String],
    since: Option<UnixTime>,
    now: UnixTime,
    at: &next::At,
) -> Result<(progress::Listing, Vec<progress::Unlisted>), String> {
    let mut listing = progress::Listing::default();
    let mut unlisted = Vec::new();
    if at.dir().is_none() && !runs.try_exists().context(runs.display())? {
        return Ok((listing, unlisted));
    }
    for (id, record) in Record::all(runs)? {
        if !picked(prefixes, &id) {
            continue;
        }
        // What of its log this `pumpkin` can't read, said even of a run that then can't be listed.
        let mut unread = None;
        let listed = record
            .and_then(|record| {
                if let Some(since) = since
                    && ended(record.dir())?.is_some_and(|ended| ended < since)
                {
                    return Ok(None);
                }
                let ending = record.ending()?;
                let runtime::Readout {
                    progress,
                    unread: parts,
                } = record.progress()?;
                unread = progress::unread(&parts);
                let used = record.used()?;
                let listed = progress::Listed::of(&record, record.task()?, ending, progress, used);
                Ok(Some(listed))
            })
            .or_else(|error| {
                let dir = runs.join(&id);
                if let Some(since) = since
                    && ended(&dir)?.is_some_and(|ended| ended < since)
                {
                    return Ok(None);
                }
                progress::Listed::unread(&dir, error).map(Some)
            });
        match listed {
            Ok(Some(listed)) => {
                let mut row = progress::Row::of(&id, &listed, now, at);
                row.unread = unread;
                listing.runs.insert(id, progress::Entry::Row(Box::new(row)));
            }
            Ok(None) => {
                listing.runs.insert(id, progress::Entry::Older);
            }
            Err(message) => unlisted.push(progress::Unlisted {
                id,
                unread,
                message,
            }),
        }
    }
    Ok((listing, unlisted))
}

fn ended(dir: &Path) -> Result<Option<UnixTime>, ReadError> {
    let outcome = dir.join("outcome.json");
    let modified = fs::metadata(&outcome).and_then(|metadata| metadata.modified());
    match modified {
        Ok(modified) => Ok(Some(UnixTime::of(modified).unwrap_or(EPOCH))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(ReadError::Failed(format!("{}: {error}", outcome.display()))),
    }
}

fn picked(prefixes: &[String], id: &str) -> bool {
    prefixes.is_empty()
        || prefixes
            .iter()
            .any(|prefix| id.starts_with(prefix.as_str()))
}

/// The record that `run` names, and the runs directory that the commands to run next name for it
/// ([`runs_dir`]'s second).
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

fn check(flags: cli::Check) -> Result<Exit, String> {
    let cli::Check {
        entries,
        args: cli::TaskArgs(args),
        config,
    } = flags;
    let formatter = style::Formatter::beside()?;
    let upstreams = check_upstreams(config.as_deref())?;
    let mut failed = Vec::new();
    for entry in &entries {
        let models = |workflow: &schema::workflow::Workflow| match &upstreams {
            Some((path, upstreams)) => {
                (upstreams.models(workflow).map(drop)).context(path.display())
            }
            None => Ok(()),
        };
        match star::check(entry, &args, &formatter, models) {
            Ok(Ok(star::Checked {
                lint: Ok(()),
                models: Ok(()),
            })) => continue,
            Ok(Ok(star::Checked { lint, models })) => {
                if let Err(problems) = lint {
                    say(task_problems(entry, &problems));
                }
                if let Err(error) = models {
                    say(format_args!("{entry}: {error}"));
                }
            }
            Ok(Err(problems)) => say(task_problems(entry, &problems)),
            Err(error) => say(format_args!("{entry}: {error}")),
        }
        failed.push(entry);
    }
    if failed.is_empty() {
        return Ok(Exit::Success);
    }
    let mut flags = arg_flags(&args).collect::<Vec<String>>();
    if let Some(config) = &config {
        flags.extend([
            "--config".to_owned(),
            absolute_utf8(config, "the configuration")?,
        ]);
    }
    let entries: Result<Vec<String>, String> = (failed.iter())
        .map(|entry| absolute_utf8(entry.path(), "the entry"))
        .collect();
    let again = next::again("check", &flags, &entries?);
    let them = if failed.len() == 1 { "it" } else { "them" };
    Err(format!("to check {them} again: {again}"))
}

/// The configuration whose upstreams `check` checks each workflow's model routes against, and its
/// path: the file that `config` names, or else the default one, unless there is none.
fn check_upstreams(config: Option<&str>) -> Result<Option<(PathBuf, config::Upstreams)>, String> {
    let path = match config {
        Some(config) => PathBuf::from(config),
        None => match default_config() {
            // A file that can't be looked up is read, which says why.
            Ok(path) if path.try_exists().unwrap_or(true) => path,
            // A machine with no configuration, or no `HOME`, has no upstreams to check against.
            Ok(_) | Err(_) => return Ok(None),
        },
    };
    let upstreams = config::upstreams(&path)?;
    Ok(Some((path, upstreams)))
}

fn arg_flags(args: &lang::Args) -> impl Iterator<Item = String> {
    let flags = args
        .iter()
        .map(|(name, value)| ["--arg".to_owned(), format!("{name}={value}")]);
    flags.flatten()
}

fn task_problems(entry: &star::Entry, problems: &lang::Problems) -> String {
    // Each problem reads as its first line names its place and those after show it, so only the
    // line that closes them is said as `pumpkin`'s.
    to_stderr(format!("{problems}\n").as_bytes());
    closing_problems(format_args!("{entry}: the task has"), problems.count())
}

/// `text`, which a sandbox may have shaped, on one line for a terminal, as [`Escaped`] shows it.
fn escaped(text: &str) -> String {
    Escaped(text).to_string()
}

/// Shows its text on one line for a terminal: each control character but a tab escaped, a
/// newline as `\n`. It writes a character at a time, so a write that fails stops it there.
struct Escaped<'a>(&'a str);

impl fmt::Display for Escaped<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for c in self.0.chars() {
            // Control characters would steer the operator's terminal.
            if c.is_control() && c != '\t' {
                write!(f, "{}", c.escape_debug())?;
            } else {
                fmt::Write::write_char(f, c)?;
            }
        }
        Ok(())
    }
}

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

fn config_path(given: Option<String>) -> Result<PathBuf, String> {
    given.map_or_else(default_config, |config| Ok(PathBuf::from(config)))
}

fn configured_engine(path: &Path, reader: Reader) -> Result<Engine, String> {
    let config::Config {
        docker: config::Docker { context },
        providers: _,
        gateway: _,
        upstreams: _,
    } = config::read(path)?;
    engine(context, reader)
}

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

fn modal_helper() -> Result<PathBuf, String> {
    let pumpkin = workspace::exe("expected Modal's helper beside it")?;
    let helper = pumpkin.with_file_name(install::File::ModalHelper.name());
    let lock = pumpkin.with_file_name(install::File::ModalLock.name());
    for file in [&helper, &lock] {
        if !file.is_file() {
            return Err(format!(
                "{} doesn't exist: expected Modal's helper and its lock beside `pumpkin`, as \
                 `cargo xtask build` puts them",
                file.display()
            ));
        }
    }
    Ok(helper)
}

fn closing_problems(has: fmt::Arguments<'_>, count: usize) -> String {
    let problems = counted(count, "problem");
    format!("{has} {problems}")
}

fn engine(context: ContextName, reader: Reader) -> Result<Engine, String> {
    let engine = Engine::connect(context, &Workspace::find()?.docker())?;
    let report = engine.report();
    // A rootful engine's `docker` group is root-equivalent; a follower can't change the engine.
    if reader == Reader::Operator && !report.security.rootless {
        say(format_args!(
            "the Docker engine that context `{}` reaches runs as root, so its `docker` group is \
             root-equivalent: rootless Docker is recommended",
            report.context.as_str()
        ));
    }
    Ok(engine)
}

fn print(bytes: &[u8]) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(bytes)?;
    stdout.flush()
}

/// `text`, without its ANSI styles where clap would print its own help without them.
fn styled(text: &StyledStr) -> String {
    // clap decides through anstream, as here.
    // https://github.com/clap-rs/clap/blob/13f2db5072d600c11d8d6298e4e9ba53ffc6c1ab/clap_builder/src/output/fmt.rs#L36-L57
    match AutoStream::choice(&io::stdout()) {
        ColorChoice::Never => text.to_string(),
        ColorChoice::Auto | ColorChoice::AlwaysAnsi | ColorChoice::Always => {
            text.ansi().to_string()
        }
    }
}

fn advise(to: &str, command: &str) {
    say(format_args!("to {to}: {command}"));
}

/// Writes `message` as a line on the standard error, and ignores a failure to write it.
fn say(message: impl fmt::Display) {
    to_stderr(format!("pumpkin: {message}\n").as_bytes());
}

fn to_stderr(bytes: &[u8]) {
    // Writing to a terminal that has hung up fails, and `eprintln!` would panic; the exit
    // status and the run's record still say how it ended.
    let _ = io::stderr().write_all(bytes);
}
