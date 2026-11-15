//! `pumpkin`, the operator's command line, which [`cli`] parses and describes, as
//! `pumpkin --help` prints it. What a command says of an ending or a refusal ends with the
//! commands to run next, as [`next`] makes them. All that it writes on this machine, but what a
//! command names, as `--runs` does, goes in the workspace's directory of it ([`Workspace`]).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, ExitStatus};
use std::time::SystemTime;
use std::{env, fmt, fs};

use docker::{CpuList, Docker};
use driver::{ARGV0, Driver, Handoff};
use ec2::Ec2;
use engine::{ContextName, Engine};
use gateway::{Dialer, GrantListener};
use modal::Modal;
use provider::{Meeting, Provider, RunTag, SandboxId, Sweep, UnixTime};
use runtime::{BUILD, Ending, Reaped, Record, Recorded, Run, System};
use rustix::fs::Mode;
use rustix::process::{Pid, Signal, umask};
use sandboxd::Sandboxd;
use schema::ir::Name;
use schema::sandbox::HELLO_TIMEOUT;
use schema::workflow::{ByProvider, ByRole, Placed, Workflow};
use stdx::Context;
use workspace::Workspace;

mod cli;
mod config;
mod controller;
mod driver;
mod job;
mod lock;
mod manifest;
mod next;
mod progress;
mod sandboxd;
mod star;
mod style;
mod view;
mod workspace;

/// The most bytes of a final judge's standard error that a run's ending shows ([`end`]): one
/// that holds more is named, and not shown.
// A judge's diagnostic, such as a traceback, fits many times over; more is a log, which the record
// keeps for reading, rather than a message for a terminal.
const JUDGE_STDERR_SHOWN_BYTES: u64 = 1 << 20;

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

/// Starts the run that `flags` ask for, with an id of its own, as [`start`] does, or on the
/// controller that the configuration names ([`controller::run`]).
fn run(flags: cli::Run) -> Result<ExitCode, String> {
    if let Some(client) = config::client(flags.config.as_deref())? {
        return controller::run(&client, flags);
    }
    start(
        &runtime::new_id(SystemTime::now())?,
        flags,
        Reader::Operator,
    )
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

/// Starts the driver of run `id`, which `flags` ask for, recorded in their runs directory
/// ([`runs_dir`]). Once the run has started, prints its id, or with `wait` follows it and prints
/// its outcome; returns the exit status that `run`'s help gives. Of how to follow the run, it says
/// nothing to a follower ([`Reader`]), nor does its driver say this machine's engine.
fn start(id: &RunTag, flags: cli::Run, reader: Reader) -> Result<ExitCode, String> {
    // Absolute, so that the record's path, which the driver says and `run` opens, names it from
    // any directory. A runs directory not given is left for the driver to find as `run` does,
    // so that the driver's flags are those that the operator gave.
    let (runs, named) = runs_dir(flags.runs.clone())?;
    let wait = flags.wait;
    let flags = cli::Run {
        runs: named.clone(),
        ..flags
    };
    let named = named.as_deref();
    let runs = named.unwrap_or(&runs);
    // The driver parses these as `run`'s own, and finds everything else wrong with them. The
    // entry comes last, after `--`, so that a path that starts with `-` is not taken for a flag.
    let mut args: Vec<OsString> = vec!["run".into()];
    for (flag, value) in flags.given() {
        args.extend([flag.into(), value.into()]);
    }
    args.extend(arg_flags(&flags.args.0).map(OsString::from));
    args.extend(["--".into(), flags.entry.path().into()]);
    match Driver::spawn(id, reader, &args)?.started(Path::new(runs))? {
        Handoff::Started(record) => {
            let id = record.id();
            say(format_args!("run {id} started"));
            if wait {
                // Whether the run is still live by now depends on how fast it went, which tells
                // the operator nothing: `run` follows the run it started however soon it ends.
                following(&record, "run");
                let ending = followed(&record, reader)?;
                return end(&record, &ending, named, record.args());
            }
            if reader == Reader::Operator {
                advise("follow it", &next::of_run("wait", named, id));
            }
            advise("stop it", &next::of_run("cancel", named, id));
            print(format!("{id}\n").as_bytes()).context(format_args!(
                "run {id} has started, but printing its id failed"
            ))?;
            Ok(ExitCode::SUCCESS)
        }
        Handoff::Ended(status, record) => unstarted(status, record.as_ref(), named),
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

/// The exit status of `run` when its driver ended with `status` before the run started, after
/// making `record`, if it did, in `runs`, as commands name it ([`end`]). A driver that exited
/// said why; `run` then prints the record's `outcome.json`, if it has one. One that a signal
/// ended said nothing, so `run` says so, and follows `record` to say how it was left.
fn unstarted(
    status: ExitStatus,
    record: Option<&Record>,
    runs: Option<&str>,
) -> Result<ExitCode, String> {
    if status.code().is_some() {
        if let Some(record) = record {
            let recorded = record.dir().join("outcome.json");
            if recorded.try_exists().context(recorded.display())? {
                print_outcome(record.dir())?;
            }
        }
        return Ok(ExitCode::FAILURE);
    }
    let ended = format!("the driver ended before the run started, by {status}");
    match record {
        None => Err(ended),
        Some(record) => {
            say(ended);
            follow(record, "run", runs)
        }
    }
}

/// The arguments of the `pumpkin run` that `run` asks for, paths made absolute, which the run's
/// record keeps, so that every ending can say how to run it again: its flags, then its entry.
fn recorded_args(run: &cli::Run) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    for (flag, path) in run.given() {
        let what = format!("the path that `{flag}` names");
        words.extend([flag.to_owned(), absolute_utf8(path, &what)?]);
    }
    words.extend(arg_flags(&run.args.0));
    words.push(absolute_utf8(run.entry.path(), "the entry")?);
    Ok(words)
}

/// The driver: runs the run that `args`, `run`'s process ID, the id that `run` minted, then the
/// arguments of `pumpkin run` without `--wait`, ask for. Asks to end with `run` until the start
/// ([`driver::die_with_run`]), names its process [`ARGV0`], checks the task, reaches the engine
/// and the providers and builds the task's images ([`prepared`]), places the roles, listens where
/// private sandboxes dial ([`listen`]), has signals interrupt the run (`stdx::signal::catch`),
/// makes the record and tells `run` its id, starts the run and tells `run` so, then drives it to
/// its end. Fails, before the start, if `run` has ended or anything above fails or is refused.
fn driver_run(args: Vec<OsString>) -> Result<ExitCode, String> {
    let (id, reader, run) = driver_started(args)?;
    let words = recorded_args(&run)?;
    // A controller's client says how to run it again from its own workspace: from its copy of
    // the record, or, of a job that made none, from the job's log.
    let again = (reader == Reader::Operator).then(|| next::run_again(&words));
    let cli::Run {
        entry,
        args: cli::TaskArgs(args),
        runs,
        config,
        sandboxd,
        wait: _,
    } = run;
    // The task, `sandboxd`, the configuration and its upstream's key are checked before the engine
    // is reached, so a mistake in one needs no engine and makes no record. The roles are placed
    // in the classes that the providers offer once they are reached, before the record is begun,
    // and before the grant listener is bound, so a role that fits none is refused before anything
    // is reserved. What only a provider refuses, such as a `sandboxd` it can't mount, is refused
    // once the record exists.
    let (runs, _) = runs_dir(runs)?;
    let sandboxd = Sandboxd::find(sandboxd.map(PathBuf::from))?;
    let config = config_path(config)?;
    // Held here once reached, as the providers connected to it outlive the thread that checks and
    // builds the task.
    let mut engine = None;
    let prepared = star::checked(&entry, &args, |task| {
        prepared(task, &config, &mut engine, reader)
    });
    let prepared =
        prepared.and_then(|built| built.map_err(|problems| task_problems(&entry, &problems)));
    let Prepared {
        workflow,
        providers,
        gateway,
        servings,
    } = prepared.map_err(|error| refused(error, again.as_deref()))?;
    let (placed, placed_on) = place(
        Path::new(entry.path()),
        workflow,
        &providers,
        &config,
        again.as_deref(),
    )?;
    let private = listen(&placed_on, &gateway, &config)?;
    // Before the record is begun, so that a signal that comes once it is begun stops the run,
    // which records it, rather than ending the driver and leaving the run lost.
    let caught = stdx::signal::catch(&[Signal::INT, Signal::TERM, Signal::HUP]);
    caught.context("setting the handlers of SIGINT, SIGTERM and SIGHUP failed")?;
    let pushed = placed_on.iter().filter(|(_, connected)| connected.pushes());
    let names: Vec<String> = pushed.map(|(name, _)| format!("`{name}`")).collect();
    let on = placed_on.map(Connected::provider);
    let kept = Recorded { args: &words };
    let run = Run::prepare(&System, placed, on, Path::new(&runs), id, &servings, kept)?;
    let (id, dir) = (run.id().to_owned(), run.dir().to_owned());
    begun(&id, &dir, &names, reader);
    driver::locked(&id)?;
    // `execute` records why the run stopped in its `outcome.json`, which `run` prints if the run
    // didn't start; these lines, the driver's last, then close that ending, which a controller's
    // client says from its copy instead.
    // Binds nothing, so every run has one, whether or not a role's sandboxes listen.
    let dialer = Dialer::new(tls::Client::web_pki());
    let executed = run.execute(sandboxd.path(), private.as_ref(), &dialer, || {
        driver::announce(&dir)
    });
    if let Err(stopped) = executed {
        if let Some(again) = &again {
            say(closing(&id, &stopped.to_string(), &dir));
            advise("run it again", again);
        }
        return Ok(ExitCode::FAILURE);
    }
    Ok(ExitCode::SUCCESS)
}

/// Says to `reader` that run `id` is begun: to the operator, where it is recorded, `dir`, before
/// `run` hears of it, so that whatever ends both later, the record is named; and that it is
/// published on the providers `pushing` names, if any, as a push may take minutes, which the
/// run's start waits for. A follower is sent the record as the run ends, which names it there.
fn begun(id: &str, dir: &Path, pushing: &[String], reader: Reader) {
    if reader == Reader::Operator {
        say(format_args!("run {id} records into {}", dir.display()));
    }
    if pushing.is_empty() {
        return;
    }
    let providers = if pushing.len() == 1 {
        "provider"
    } else {
        "providers"
    };
    say(format_args!(
        "publishing run {id}'s images on {providers} {}: it starts once they are there",
        pushing.join(", ")
    ));
}

/// The run that the driver's `args` ask for, by its id, and who reads what it says, once the
/// driver has asked to end with `run` ([`driver::die_with_run`]) and named its process [`ARGV0`].
/// Fails if `args` aren't what `run` gives the driver, or `run` has ended.
fn driver_started(args: Vec<OsString>) -> Result<(RunTag, Reader, cli::Run), String> {
    // Only `run` starts the driver, with its process ID and the run's id, but an `argv[0]` of
    // the driver's name can come from anyone.
    let refused = || {
        "the driver was asked for something other than `pumpkin run` asks of it: start a run \
         with `pumpkin run`"
            .to_owned()
    };
    let mut args = args.into_iter();
    let run_pid = args.next().and_then(|pid| pid.to_str()?.parse().ok());
    driver::die_with_run(run_pid.ok_or_else(refused)?)?;
    // `ps` and `pgrep` show the driver by this name, rather than as `exe`, the base name of the
    // path that `run` executes.
    rustix::thread::set_name(ARGV0).context("naming the driver failed")?;
    let id = (args.next().and_then(|id| id.into_string().ok())).map(RunTag::try_from);
    let reader = (args.next()).and_then(|word| Reader::from_word(word.to_str()?));
    match (id, reader, cli::parse(args)) {
        (Some(Ok(id)), Some(reader), Ok(cli::Command::Run(run))) => Ok((id, reader, run)),
        _ => Err(refused()),
    }
}

/// What a run starts from: its task's workflow, built, and what it reads of the machine's
/// configuration, its providers connected.
struct Prepared<'e> {
    workflow: Workflow,
    providers: config::Providers<Connected<'e>>,
    gateway: config::Gateway,
    /// How each role's model route is served, an entry at a time.
    servings: ByRole<Vec<route::Serving>>,
}

/// What a run of `task`, checked, starts from, on the machine that the configuration at `config`
/// names, each step refused before the next begins: the configuration read, and an upstream found
/// to serve each role's model route; the engine reached, said to `reader` ([`engine()`]), and kept
/// in `engine`; each provider connected to it ([`Connected::new`]), so that an engine that can't
/// run sandboxes is refused before any image is built; then the task's images built on it. Fails
/// as each step does; gives each build that failed, at its place.
fn prepared<'e>(
    task: star::Task<'_>,
    config: &Path,
    engine: &'e mut Option<Engine>,
    reader: Reader,
) -> Result<Result<Prepared<'e>, lang::Problems>, String> {
    let config::Config {
        docker: config::Docker { context, cpus },
        providers,
        gateway,
        upstreams,
    } = config::read(config)?;
    let servings = task
        .workflow()
        .by_role(|(_, sandbox)| match &sandbox.model_route {
            Some(route) => upstreams.serving(route).context(config.display()),
            None => Ok(Vec::new()),
        })?;
    let engine = &*engine.insert(crate::engine(context, reader)?);
    let providers =
        providers.connect(|name, kind| Connected::new(name, kind, engine, cpus.as_ref()))?;
    Ok(task.build(engine, &mut ()).map(|(workflow, _)| Prepared {
        workflow,
        providers,
        gateway,
        servings,
    }))
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

/// Stops the run that `flags` name, in their runs directory or [`default_runs`], unless it has
/// ended: by SIGTERM to its driver, or by SIGKILL with `force`. Then waits until it has ended,
/// removes what it left ([`Record::reap`]), and prints its outcome; returns the exit status that
/// `cancel`'s help gives.
fn cancel(flags: cli::Cancel) -> Result<ExitCode, String> {
    if let (cli::RunName::Prefix(prefix), None) = (&flags.run, &flags.runs)
        && let Some(client) = config::client(None)?
    {
        return controller::cancel(&client, prefix, flags.force);
    }
    let (record, named, reached) = cancelled(flags, Reader::Operator)?;
    let named = named.as_deref();
    // `cancel`'s exit says only that the run has ended, however that was, and whether what it
    // left on its provider is gone.
    end(&record, &record.wait(|| {})?, named, record.args())?;
    if !reached {
        advise(
            "remove what it left once its provider can be reached",
            &next::reap(named),
        );
        return Ok(ExitCode::from(UNREACHED));
    }
    Ok(ExitCode::SUCCESS)
}

/// Stops the run that `flags` name, here, as `cancel` does, saying so to `reader`, with where it
/// is recorded to the operator ([`Reader`]), and reaps what it left once it has ended; returns
/// its record, its runs directory as commands name it ([`found`]), and whether its provider was
/// reached.
fn cancelled(flags: cli::Cancel, reader: Reader) -> Result<(Record, Option<String>, bool), String> {
    let cli::Cancel {
        run,
        runs,
        force,
        modal_config,
    } = flags;
    let (record, named) = found(run, runs)?;
    let (id, dir) = (record.id(), record.dir().display());
    if let Some(pid) = record.driver()? {
        // A driver that SIGTERM stops deletes the run's sandboxes before it writes the outcome,
        // which takes a while on a provider whose sandboxes are instances.
        let (signal, doing, until) = if force {
            (Signal::KILL, "killing the driver of run", "")
        } else {
            (
                Signal::TERM,
                "stopping run",
                ": it ends once its sandboxes are deleted",
            )
        };
        // `kill` reads 0 as this process's group, -1 as every process it may signal, and 1 is
        // init: none is a driver's.
        let driver = i32::try_from(pid).ok().filter(|pid| *pid > 1);
        let Some(driver) = driver.and_then(Pid::from_raw) else {
            return Err(format!(
                "run {id}'s driver is process {pid}, which names no single process: expected one \
                 above 1"
            ));
        };
        let sent = rustix::process::kill_process(driver, signal);
        sent.context(format_args!(
            "signalling run {id}'s driver, process {pid}, failed"
        ))?;
        match reader {
            Reader::Operator => say(format_args!("{doing} {id}, recorded in {dir}{until}")),
            Reader::Follower => say(format_args!("{doing} {id}{until}")),
        }
    }
    // Its provider is known for certain only once it has ended: a driver yet to start the run
    // has yet to say which it is.
    record.wait(|| {})?;
    let reaping = &mut Reaching::new(modal_config);
    let reaped = reaped(&record, reaping, named.as_deref(), Reaper::Command)?;
    let Swept { left: _, reached } = reaped;
    Ok((record, named, reached))
}

/// The exit status of `cancel` and `reap` once a run's provider couldn't be reached, as their
/// help says.
const UNREACHED: u8 = 3;

struct Swept {
    /// Whether the run had left anything, now removed.
    left: bool,
    /// Whether its provider was reached, if it had one: if not, what it left there may remain.
    reached: bool,
}

/// Who reaps, which decides the runs that a reap takes, and what it makes of a provider of
/// theirs that it can't reach.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reaper {
    /// `pumpkin reap` or `pumpkin cancel`: every run that has ended or is lost; a provider
    /// that can't be reached fails the reap, which says how to reap again once it can be.
    Command,
    /// The controller's boot: only the runs that may have left something, not one that ended
    /// clean ([`Record::ended_clean`]); a provider that it can't reach, as Modal, whose
    /// credentials reach the controller only with a job, it says leaves what the run left
    /// there to end by the run's hard end, and goes on.
    Boot,
}

/// Removes what each run in the runs directory that `flags` name, or [`default_runs`], left
/// once it ended or was lost ([`Record::reap`]), of the runs that `reaper` takes, printing each
/// such run's id; leaves each live run to its driver. Returns the exit status that `reap`'s
/// help gives, but of `reaper` [`Reaper::Boot`], 0 though a provider can't be reached.
fn reap(flags: cli::Reap, reaper: Reaper) -> Result<ExitCode, String> {
    let cli::Reap { runs, modal_config } = flags;
    if runs.is_none()
        && let Some(client) = config::client(None)?
    {
        return controller::reap(&client);
    }
    let (runs, named) = runs_dir(runs)?;
    let named = named.as_deref();
    let mut reaching = Reaching::new(modal_config);
    let (mut unreaped, mut unreached) = (0, 0);
    for record in Record::all(Path::new(&runs))? {
        let swept = record.and_then(|record| {
            if record.driver()?.is_some() || reaper == Reaper::Boot && record.ended_clean()? {
                return Ok(None);
            }
            let swept = reaped(&record, &mut reaching, named, reaper)?;
            Ok(Some((record, swept)))
        });
        match swept {
            Ok(Some((record, Swept { left, reached }))) => {
                if left {
                    print(format!("{}\n", record.id()).as_bytes())
                        .context("printing a reaped run's id failed")?;
                }
                if !reached {
                    unreached += 1;
                }
            }
            Ok(None) => {}
            Err(error) => {
                say(error);
                unreaped += 1;
            }
        }
    }
    if unreaped > 0 {
        let runs = counted(unreaped, "run");
        say(format_args!(
            "{runs} can't be reaped, as said above: fix what each says, and reap again"
        ));
        advise("reap again", &next::reap(named));
        return Ok(ExitCode::FAILURE);
    }
    if unreached > 0 && reaper == Reaper::Command {
        let runs = counted(unreached, "run");
        say(format_args!(
            "{runs} ran on a provider that can't be reached, as said above: what each left \
             there may remain"
        ));
        advise("reap again once it can be reached", &next::reap(named));
        return Ok(ExitCode::from(UNREACHED));
    }
    Ok(ExitCode::SUCCESS)
}

/// The providers that a reap has reached, or failed to reach, so that each is reached once,
/// however many of its runs it reaps: a Docker engine by the context that a run's report of it
/// names, and any other provider by the configuration that names it, by its path, and its name
/// there, each configuration read once.
struct Reaching {
    reached: BTreeMap<Reachable, Result<Box<dyn Sweep>, String>>,
    configurations: BTreeMap<PathBuf, Result<config::Providers<config::Kind>, String>>,
    /// Modal's configuration file for every provider of kind `modal`, in place of the one that
    /// a run's configuration names, as `--modal-config` gives it.
    modal_config: Option<PathBuf>,
}

/// How a reap reaches a provider of a run's: by a Docker context's name, or by the path of the
/// configuration that ran the run, and the provider's name there.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Reachable {
    Engine(String),
    Configured(PathBuf, Name),
}

impl Reaching {
    fn new(modal_config: Option<PathBuf>) -> Self {
        Self {
            reached: BTreeMap::new(),
            configurations: BTreeMap::new(),
            modal_config,
        }
    }

    /// How provider `name` of the run of `record`, which reported `report` of itself, is
    /// reached, once it has been tried: a provider whose report names a Docker context by the
    /// engine of that context, as `Docker::reach` reaches it; any other by its table in the
    /// configuration that ran the run ([`run_config`]), of kind `ec2` or `modal`. Fails, saying
    /// why, if it can't be named so, as when that configuration can't be read, or names no such
    /// provider; or, in the outer error, if the record is wrong, as when its report names no
    /// context but the configuration names the provider of kind `docker`.
    fn reach(
        &mut self,
        record: &Record,
        name: &Name,
        report: &serde_json::Value,
    ) -> Result<Result<Reachable, String>, String> {
        if let Some(context) = report["context"].as_str() {
            let reachable = Reachable::Engine(context.to_owned());
            if !self.reached.contains_key(&reachable) {
                let context = ContextName::try_from(context.to_owned())?;
                let reached = Docker::reach(context).map(|reached| Box::new(reached) as _);
                self.reached.insert(reachable.clone(), reached);
            }
            return Ok(Ok(reachable));
        }
        let path = match run_config(record) {
            Ok(path) => path,
            Err(error) => return Ok(Err(error)),
        };
        let reachable = Reachable::Configured(path.clone(), name.clone());
        if self.reached.contains_key(&reachable) {
            return Ok(Ok(reachable));
        }
        let read = (self.configurations.entry(path.clone())).or_insert_with(|| {
            let text = fs::read_to_string(&path).map_err(|error| {
                format!(
                    "the configuration that ran it, {}, can't be read ({error}): put it back \
                     there, as it was when the run ran",
                    path.display()
                )
            })?;
            let parsed = config::parse(&path, &text).map_err(|error| {
                format!("{error}: put the configuration back there as it was when the run ran")
            });
            parsed.map(|config| config.providers)
        });
        let providers = match read {
            Ok(providers) => providers,
            Err(error) => return Ok(Err(error.clone())),
        };
        let reached: Result<Box<dyn Sweep>, String> = match providers.get(name) {
            None => {
                return Ok(Err(format!(
                    "the configuration that ran it, {}, names no provider `{name}`: put its \
                     `[providers.{name}]` table back there, as it was when the run ran",
                    path.display()
                )));
            }
            Some(config::Kind::Docker(config::Empty {})) => {
                return Err(format!(
                    "{}/provider.json names no Docker context of provider `{name}`, which the \
                     configuration that ran it, {}, names of kind `docker`: expected the report \
                     of a provider of kind `docker`",
                    record.dir().display(),
                    path.display()
                ));
            }
            Some(config::Kind::Ec2(settings)) => {
                Ec2::reach(settings).map(|reached| Box::new(reached) as _)
            }
            Some(config::Kind::Modal(settings)) => (modal_helper())
                .and_then(|helper| {
                    let config = self.modal_config.as_deref();
                    Modal::reach(settings, config, &helper, &Workspace::find()?.modal())
                })
                .map(|reached| Box::new(reached) as _),
        };
        self.reached.insert(reachable.clone(), reached);
        Ok(Ok(reachable))
    }
}

/// The configuration that the run of `record` read: the one that its `--config` named, as its
/// record keeps the arguments that started it ([`Record::args`]), or the default one.
fn run_config(record: &Record) -> Result<PathBuf, String> {
    let flags = record.args();
    let at = flags.iter().position(|flag| flag == "--config");
    match at.and_then(|at| flags.get(at + 1)) {
        Some(config) => Ok(PathBuf::from(config)),
        // `default_config` fails only for want of `HOME`, and its fix, `--config`, is `run`'s.
        None => default_config().map_err(|_| {
            "its record names no `--config`, so it ran on the default configuration, which HOME \
             places, but HOME is unset or empty: set it as it was when the run ran"
                .to_owned()
        }),
    }
}

/// Reaps the run of `record`, which has ended or is lost, on each provider that its
/// `provider.json` names, as `reaching` reaches it ([`Record::reap`]), and says what was left.
/// Of a provider that can't be reached, it says so, and, as `reaper` has it, how to reap `runs`
/// again, as commands name it ([`end`]), or that what the run left there ends by its hard end;
/// and removes the rest.
fn reaped(
    record: &Record,
    reaching: &mut Reaching,
    runs: Option<&str>,
    reaper: Reaper,
) -> Result<Swept, String> {
    let id = record.id();
    let failed = |error| format!("run {id} can't be reaped: {error}");
    let reports = record.provider_reports().map_err(failed)?;
    let mut reachable = Vec::new();
    for (name, report) in &reports {
        reachable.push((name, reaching.reach(record, name, report).map_err(failed)?));
    }
    let mut providers = Vec::new();
    for (name, reachable) in reachable {
        let reached = reachable.and_then(|reachable| match &reaching.reached[&reachable] {
            Ok(sweep) => Ok(sweep.as_ref()),
            Err(error) => Err(error.clone()),
        });
        match reached {
            Ok(sweep) => providers.push((name, sweep)),
            // Not the reap's to fail: a record's provider may be gone for good, as on a machine
            // set up again since, and a failure would fail every reap after it.
            Err(error) => match reaper {
                Reaper::Command => say(format_args!(
                    "run {id}'s sandboxes on provider `{name}`, if it left any, can't be \
                     removed, as it can't be reached: {error}; once it can be, `{}` removes \
                     them, and if it is gone for good, remove the record, {}, to stop reaping it",
                    next::reap(runs),
                    record.dir().display()
                )),
                Reaper::Boot => say(format_args!(
                    "run {id}'s sandboxes on provider `{name}`, if it left any, end by the run's \
                     hard end, as the boot can't reach it: {error}"
                )),
            },
        }
    }
    let reached = providers.len() == reports.len();
    reap_rest(record, &providers).map(|left| Swept { left, reached })
}

/// Reaps the run of `record` on `providers`, each by its name ([`Record::reap`]), and says what
/// was left, now removed; returns whether anything was left.
fn reap_rest(record: &Record, providers: &[(&Name, &dyn Sweep)]) -> Result<bool, String> {
    let id = record.id();
    let failed = |error| format!("run {id} can't be reaped: {error}");
    let reap = record.reap(providers);
    let Reaped {
        sandboxes,
        ended,
        sockets,
        lost,
    } = reap.map_err(failed)?;
    let mut left = sockets.is_some() || lost || !ended.is_empty();
    for (name, ids) in &sandboxes {
        if ids.is_empty() {
            continue;
        }
        left = true;
        let ids: Vec<&str> = ids.iter().map(SandboxId::as_str).collect();
        let sandboxes = counted(ids.len(), "sandbox");
        say(format_args!(
            "run {id} left {sandboxes} on provider `{name}`, now removed: {}",
            ids.join(", ")
        ));
    }
    for (name, sandbox) in &ended {
        say(format_args!(
            "run {id}'s sandbox {} on provider `{name}` had ended, as it isn't listed there now: \
             what it left there is now removed",
            sandbox.as_str()
        ));
    }
    if let Some(sockets) = &sockets {
        say(format_args!(
            "run {id} left its socket directory, now removed: {}",
            sockets.display()
        ));
    }
    if lost {
        say(format_args!(
            "run {id} was lost, as its driver ended without an outcome.json: it is now recorded \
             as failed"
        ));
    }
    Ok(left)
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

/// Waits until the run of `record`, in `runs`, as commands name it ([`end`]), has ended, saying
/// first, if it is live, that ending `pumpkin command` does not stop it ([`following`]), then its
/// milestones as they come ([`followed`]), and prints its outcome; returns the exit status that
/// `wait`'s help gives.
fn follow(record: &Record, command: &str, runs: Option<&str>) -> Result<ExitCode, String> {
    let ending = if let Some(ending) = record.ending()? {
        ending
    } else {
        following(record, command);
        followed(record, Reader::Operator)?
    };
    end(record, &ending, runs, record.args())
}

// A milestone said within a second is timely, and each poll reads only what the log gained, and
// each sandbox's `sandbox.json`.
const FOLLOW_POLL: std::time::Duration = std::time::Duration::from_secs(1);

/// Waits until the run of `record`, which was live, has ended, or been lost, and returns how;
/// says each of its milestones meanwhile, as its log records it ([`progress::milestones`]), and,
/// once, the parts of its log that can't be read, if any ([`progress::unread`]), to `reader`.
/// Fails if a follower can't be told.
fn followed(record: &Record, reader: Reader) -> Result<Ending, String> {
    let mut said = std::collections::BTreeSet::new();
    let mut said_unread = false;
    let mut log = record.fold();
    let tell_reader = |message: &dyn fmt::Display| match reader {
        Reader::Operator => {
            say(message);
            Ok(())
        }
        // Its standard error is a connection's end, so a write that fails says that the other end
        // has closed, and nothing else would end this loop before the run does.
        Reader::Follower => tell(message).context(format_args!(
            "telling the client of run {} failed",
            record.id()
        )),
    };
    loop {
        // How it ended first, so that the log read after holds every milestone before the end.
        let ending = record.ending()?;
        if let Some(started_over) = log.take()? {
            tell_reader(&format_args!("run {}: {started_over}", record.id()))?;
        }
        let runtime::Readout { progress, unread } = log.readout()?;
        if !said_unread && let Some(unread) = progress::unread(&unread) {
            tell_reader(&format_args!("run {}: {unread}", record.id()))?;
            said_unread = true;
        }
        if let Some(progress) = progress {
            for milestone in progress::milestones(&progress) {
                if said.insert(milestone.clone()) {
                    tell_reader(&milestone)?;
                }
            }
        }
        if let Some(ending) = ending {
            return Ok(ending);
        }
        std::thread::sleep(FOLLOW_POLL);
    }
}

/// Says that `pumpkin command` follows the run of `record`, and that ending it does not stop
/// the run.
fn following(record: &Record, command: &str) {
    say(format_args!(
        "following run {}, recorded in {}: ending `pumpkin {command}` does not stop it",
        record.id(),
        record.dir().display()
    ));
}

/// Prints the outcome of `record`, a run that has `ended`, then says its summary
/// ([`summarize`]) and how it ended ([`closing`]), after the standard error of a final judge
/// that returned no result ([`show_judge_stderr`]), or why it can't be found; or of a lost run,
/// says its summary, and that it is lost. Ends with the command that runs it again with `args`
/// ([`again`]), and of a lost run, those that remove what it left, which name its runs directory
/// `runs`, absolute, or none if it is the default. Returns the exit status that `run`'s and
/// `wait`'s help give.
fn end(
    record: &Record,
    ending: &Ending,
    runs: Option<&str>,
    args: &[String],
) -> Result<ExitCode, String> {
    let id = record.id();
    let (exit, how, judge_stderr) = match ending {
        Ending::Scored { score, after } => {
            let score = progress::shown_score(score);
            let how = format!("finished with a score of {score}, {after}");
            (ExitCode::SUCCESS, how, None)
        }
        Ending::Unscored(why) => (ExitCode::from(3), why.clone(), None),
        Ending::NoResult { why, judge_stderr } => {
            (ExitCode::from(3), why.clone(), Some(judge_stderr))
        }
        Ending::Failed(how) | Ending::Interrupted(how) => (ExitCode::FAILURE, how.clone(), None),
        // It has no `outcome.json` to print.
        Ending::Lost => {
            let how = "is lost: its driver ended without writing outcome.json, so nothing says \
                       how it ended";
            summarize(record);
            say(closing(id, how, record.dir()));
            again(record, args);
            advise("remove what it left", &next::of_run("cancel", runs, id));
            advise(
                "remove what every run that has ended left",
                &next::reap(runs),
            );
            return Ok(ExitCode::FAILURE);
        }
    };
    print_outcome(record.dir())?;
    summarize(record);
    match judge_stderr {
        Some(Ok(path)) => show_judge_stderr(path),
        Some(Err(error)) => say(format_args!(
            "the final judge's standard error can't be found: {}",
            escaped(error)
        )),
        None => {}
    }
    say(closing(id, &how, record.dir()));
    again(record, args);
    Ok(exit)
}

/// Says the summary of the run of `record`, which has ended ([`progress::summary`]), if it has a
/// log, or why it can't be read.
fn summarize(record: &Record) {
    match progress::summary(record) {
        Ok(Some(summary)) => {
            // Its first line is `pumpkin`'s, and the rest are indented beneath it.
            to_stderr(format!("pumpkin: {summary}").as_bytes());
        }
        Ok(None) => {}
        Err(error) => say(format_args!("the run's summary can't be read: {error}")),
    }
}

/// Says the command that runs the run of `record` again, with `args`, those of a `pumpkin run`.
fn again(record: &Record, args: &[String]) {
    if let Some(build) = record.other_build() {
        say(format_args!(
            "run {} was made by pumpkin {build}, and this is pumpkin {BUILD}: to run it as it \
             ran, run it with the pumpkin that made it",
            record.id()
        ));
    }
    advise("run it again", &next::run_again(args));
}

/// The line that closes a run's ending on the standard error: the run `id`, `how` it ended, as
/// an [`Ending`] says, and where its record is, `dir`.
fn closing(id: &str, how: &str, dir: &Path) -> String {
    format!("run {id} {how}; its record is {}", dir.display())
}

/// Shows on the standard error the final judge's standard error, which its record holds at
/// `path`, under a line that names it ([`indented`]). One of more than
/// [`JUDGE_STDERR_SHOWN_BYTES`], or that can't be read, is named alone; one that is empty, or
/// wasn't collected, is not shown.
fn show_judge_stderr(path: &Path) {
    let named = format!("the final judge's standard error, {},", path.display());
    let mut bytes = Vec::new();
    let read = File::open(path).and_then(|file| {
        let size = file.metadata()?.len();
        if size <= JUDGE_STDERR_SHOWN_BYTES {
            file.take(JUDGE_STDERR_SHOWN_BYTES)
                .read_to_end(&mut bytes)?;
        }
        Ok(size)
    });
    match read {
        Ok(size) if size > JUDGE_STDERR_SHOWN_BYTES => say(format_args!(
            "{named} holds {size} bytes, more than the {JUDGE_STDERR_SHOWN_BYTES} shown here: read \
             it there"
        )),
        Ok(_) if bytes.is_empty() => {}
        Ok(_) => {
            say(format_args!("{named} holds:"));
            to_stderr(indented(&bytes).as_bytes());
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => say(format_args!("{named} can't be read: {error}")),
    }
}

/// `bytes`, which a sandbox wrote, as text for a terminal: each line indented, and each control
/// character but a tab escaped.
fn indented(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut shown = String::new();
    for line in text.strip_suffix('\n').unwrap_or(&text).split('\n') {
        shown.push_str("  ");
        shown.push_str(&escaped(line));
        shown.push('\n');
    }
    shown
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

/// A provider of the machine's configuration, connected as its kind says.
enum Connected<'e> {
    /// The machine's Docker engine, as a provider.
    Docker(Docker<'e>),
    /// An EC2 envelope, which pushes what its sandboxes pull from the machine's Docker engine.
    Ec2(Ec2<'e>),
    /// A Modal workspace, which imports what its sandboxes run, pushed from the machine's Docker
    /// engine.
    Modal(Modal<'e>),
}

impl<'e> Connected<'e> {
    /// Provider `name`, of `kind`, connected, with `engine`, the machine's Docker engine, whose
    /// sandboxes, if it is one, are given the CPUs of `cpus`. Fails, naming the provider, if the
    /// engine can't run sandboxes, or push what the provider's sandboxes pull, or the provider
    /// can't be reached, as one whose helper isn't beside this `pumpkin`.
    fn new(
        name: &Name,
        kind: config::Kind,
        engine: &'e Engine,
        cpus: Option<&CpuList>,
    ) -> Result<Self, String> {
        let connected = match kind {
            config::Kind::Docker(config::Empty {}) => {
                Docker::connect(engine, cpus).map(Self::Docker)
            }
            config::Kind::Ec2(settings) => Workspace::find()
                .and_then(|workspace| Ec2::connect(&settings, engine.pusher()?, &workspace.ec2()))
                .map(Self::Ec2),
            config::Kind::Modal(settings) => modal_helper()
                .and_then(|helper| {
                    let pusher = engine.pusher()?;
                    Modal::connect(&settings, &helper, &Workspace::find()?.modal(), pusher)
                })
                .map(Self::Modal),
        };
        connected.context(format_args!("reaching provider `{name}` failed"))
    }

    fn provider(&self) -> &dyn Provider {
        match self {
            Self::Docker(docker) => docker,
            Self::Ec2(ec2) => ec2,
            Self::Modal(modal) => modal,
        }
    }

    /// Whether publishing what its sandboxes run pushes it from the machine's engine, which may
    /// take minutes: the engine's own sandboxes run what it has.
    fn pushes(&self) -> bool {
        match self {
            Self::Docker(_) => false,
            Self::Ec2(_) | Self::Modal(_) => true,
        }
    }
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

/// `workflow`, of the task whose entry is `path`, placed in the classes that `providers`, read
/// from the configuration at `config`, offer ([`Workflow::place`]), with each provider that it
/// places a role on. Of one that can't be, says each problem on a line of its own, as cargo does,
/// then fails with how many, as [`refused`] does with `again`.
fn place<'p, 'e>(
    path: &Path,
    workflow: Workflow,
    providers: &'p config::Providers<Connected<'e>>,
    config: &Path,
    again: Option<&str>,
) -> Result<(Placed, ByProvider<&'p Connected<'e>>), String> {
    let classes = |connected: &'p Connected<'e>| &connected.provider().ready().classes;
    let placed = workflow.place(providers.by_name(), classes);
    placed.map_err(|errors| {
        let path = path.display();
        for error in errors.as_slice() {
            say(format_args!("{path}: {error}"));
        }
        let has = format_args!(
            "{path}: placing the workflow's roles in the classes that {}'s providers offer has",
            config.display()
        );
        refused(closing_problems(has, errors.as_slice().len()), again)
    })
}

/// What the driver fails with, refused `error` before the record is begun: once it has said
/// `error`, the command that runs it again, `again`; or `error` alone without one, as a
/// controller's client says what to do next ([`driver_run`]).
fn refused(error: String, again: Option<&str>) -> String {
    let Some(again) = again else {
        return error;
    };
    say(error);
    format!("to run it again: {again}")
}

/// The grant listener that the sandboxes of the private providers of `placed_on`, those that a
/// run places roles on, dial, bound at the `private` address of `gateway`, read from the
/// configuration at `config`; or `None`, binding nothing, if none is private.
///
/// # Errors
///
/// Fails, naming each private provider that roles are placed on, if `gateway` names no `private`
/// address; or if it can't be bound, as one not of this machine's, or where another run listens.
fn listen(
    placed_on: &ByProvider<&Connected<'_>>,
    gateway: &config::Gateway,
    config: &Path,
) -> Result<Option<GrantListener>, String> {
    let mut private = Vec::new();
    for (name, connected) in placed_on.iter() {
        match connected.provider().meeting() {
            Meeting::Local(_) | Meeting::Listen(_) => {}
            Meeting::Private(_) => private.push(format!("`{name}`")),
        }
    }
    if private.is_empty() {
        return Ok(None);
    }
    let Some(address) = &gateway.private else {
        let s = if private.len() == 1 { "" } else { "s" };
        return Err(format!(
            "{}: roles are placed on provider{s} {}, whose sandboxes dial the gateway on a private \
             network, but `[gateway]` names no `private` address: name the address of this \
             machine's that they dial",
            config.display(),
            private.join(", ")
        ));
    };
    let bound = GrantListener::bind(address, HELLO_TIMEOUT).map_err(|error| {
        format!(
            "{}: {error}: name in `[gateway] private` an address of this machine's where no other \
             run listens",
            config.display()
        )
    })?;
    Ok(Some(bound))
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
