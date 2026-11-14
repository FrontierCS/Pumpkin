//! `pumpkin`, the operator's command line, which [`cli`] parses and describes, as
//! `pumpkin --help` prints it. `run` builds a task's images on the machine's engine and starts
//! its workflow's run with [`runtime`], on the Docker provider, in a driver process
//! ([`driver`]), with the `sandboxd` that [`sandboxd`] finds, and [`config`] reads the machine's
//! configuration. `check` checks a task's source, which [`star`] reads, and lints it, and `build`
//! builds its images and prints its IR. `fmt` formats `.star` files, and `check` lints them, with
//! the formatter beside `pumpkin` ([`style`]).

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, ExitStatus};
use std::time::SystemTime;
use std::{env, fmt, fs};

use docker::Docker;
use driver::{ARGV0, Driver, Handoff};
use engine::{ContextName, Engine};
use provider::Provider;
use runtime::{Ending, Record, Run};
use sandboxd::Sandboxd;
use schema::workflow::{ParseErrors, Workflow};
use stdx::Context;

mod cli;
mod config;
mod driver;
mod sandboxd;
mod star;
mod style;

/// The most bytes of a final judge's standard error that a run's ending shows ([`end`]): one
/// that holds more is named, and not shown.
// A judge's diagnostic, such as a traceback, fits many times over; more is a log, which the record
// keeps for reading, rather than a message for a terminal.
const JUDGE_STDERR_SHOWN_BYTES: u64 = 1 << 20;

fn main() -> ExitCode {
    // A run's judge calls, each session's reader, and the evaluation of `check` and `build` run
    // on threads of their own.
    stdx::abort_on_panic();
    let mut args = env::args_os();
    let argv0 = args.next();
    if argv0.is_some_and(|name| name.as_bytes() == ARGV0.to_bytes()) {
        return finish(driver_run(args.collect()));
    }
    let done = match cli::parse(args) {
        Ok(cli::Command::Run(flags)) => run(flags),
        Ok(cli::Command::Wait(flags)) => wait(flags),
        Ok(cli::Command::Check(flags)) => check(flags),
        Ok(cli::Command::Fmt(flags)) => style::fmt(flags),
        Ok(cli::Command::Build(flags)) => build(flags),
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

/// Starts the driver of the run that `flags` ask for, recorded in their runs directory, or
/// [`default_runs`], made absolute. Once the run has started, prints its id, or with `wait`
/// follows it and prints its outcome; returns the exit status that `run`'s help gives.
fn run(flags: cli::Run) -> Result<ExitCode, String> {
    let cli::Run {
        entry,
        args: cli::TaskArgs(task_args),
        runs,
        config,
        sandboxd,
        wait,
    } = flags;
    let runs = match runs {
        Some(runs) => runs,
        None => default_runs()?,
    };
    // Absolute, so that the record's path, which the driver says and `run` opens, names it from
    // any directory.
    let runs = std::path::absolute(&runs).map_err(|error| {
        format!(
            "the runs directory `{}` can't be made absolute ({error}): name another with `--runs`",
            runs.display()
        )
    })?;
    // The driver parses these as `run`'s own, and finds everything else wrong with them. The
    // entry comes last, after `--`, so that a path that starts with `-` is not taken for a flag.
    let mut args: Vec<OsString> = vec!["run".into()];
    args.extend(["--runs".into(), runs.clone().into()]);
    for (flag, value) in [("--sandboxd", sandboxd), ("--config", config)] {
        if let Some(value) = value {
            args.extend([flag.into(), value.into()]);
        }
    }
    args.extend(arg_flags(&task_args).map(OsString::from));
    args.extend(["--".into(), entry.path().into()]);
    match Driver::spawn(&args)?.started(&runs)? {
        Handoff::Started(record) => {
            let id = record.id();
            if wait {
                say(format_args!("run {id} started"));
                return follow(&record, "run");
            }
            say(format_args!(
                "run {id} started: `pumpkin wait {id}` follows it"
            ));
            print(format!("{id}\n").as_bytes()).context(format_args!(
                "run {id} has started, but printing its id failed"
            ))?;
            Ok(ExitCode::SUCCESS)
        }
        Handoff::Ended(status, record) => unstarted(status, record.as_ref()),
    }
}

/// The exit status of `run` when its driver ended with `status` before the run started, after
/// making `record`, if it did. A driver that exited said why; `run` then prints the record's
/// `outcome.json`, if it has one. One that a signal ended said nothing, so `run` says so, and
/// follows `record` to say how it was left.
fn unstarted(status: ExitStatus, record: Option<&Record>) -> Result<ExitCode, String> {
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
            follow(record, "run")
        }
    }
}

/// The driver: runs the run that `args`, `run`'s process ID and then its own arguments, ask for.
/// Asks to end with `run` until the start ([`driver::die_with_run`]), names its process
/// [`ARGV0`], checks the task, reaches the engine and the provider and builds the task's images
/// ([`prepared`]), places the roles, makes the record and tells `run` its id, starts the run and
/// tells `run` so, then drives it to its end.
/// Fails if `run` has ended, the process can't be named, or the run is refused, can't be
/// recorded, fails to start, or fails.
fn driver_run(args: Vec<OsString>) -> Result<ExitCode, String> {
    // Only `run` starts the driver, with its process ID and the runs directory, but an
    // `argv[0]` of the driver's name can come from anyone.
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
    let Ok(cli::Command::Run(cli::Run {
        entry,
        args: cli::TaskArgs(args),
        runs: Some(runs),
        config,
        sandboxd,
        wait: _,
    })) = cli::parse(args)
    else {
        return Err(refused());
    };
    // The task, `sandboxd`, the configuration and its upstream's key are checked before the
    // engine is reached, so a mistake in one needs no engine and makes no record. The roles are
    // placed in the provider's classes before the record is begun. What only the provider
    // refuses, such as a `sandboxd` it can't mount, is refused once the record exists.
    let sandboxd = Sandboxd::find(sandboxd)?;
    let config = match config {
        Some(config) => config,
        None => default_config()?,
    };
    // Held here once reached, as the provider connected to it outlives the thread that checks
    // and builds the task.
    let mut engine = None;
    let prepared = star::checked(&entry, &args, |task| prepared(task, &config, &mut engine));
    let prepared =
        prepared.and_then(|built| built.map_err(|problems| task_problems(&entry, &problems)));
    let Prepared {
        workflow,
        docker,
        serving,
    } = prepared?;
    let placed = workflow.place(&docker.ready().classes);
    let placing = "placing the workflow's roles in the engine's classes";
    let path = Path::new(entry.path());
    let placed = placed.map_err(|errors| said(path, &errors, placing))?;
    let run = Run::prepare(placed, &runs, SystemTime::now(), serving)?;
    let (id, dir) = (run.id().to_owned(), run.dir().to_owned());
    // Said before `run` hears of it, so that whatever ends both later, the record is named.
    say(format_args!("run {id} records into {}", dir.display()));
    driver::locked(&id)?;
    // `execute` records why the run failed in its `outcome.json`, which `run` prints if the run
    // failed to start; this line, the driver's last, then closes that ending.
    let executed = run.execute(&docker, sandboxd.path(), || driver::announce(&dir));
    executed.map_err(|reason| closing(&id, &format!("failed: {reason}"), &dir))?;
    Ok(ExitCode::SUCCESS)
}

/// What a run starts from: its task's workflow, built, and the provider, connected.
struct Prepared<'e> {
    workflow: Workflow,
    docker: Docker<'e>,
    /// How the agent's model route is served, if it has one.
    serving: Option<route::Serving>,
}

/// What a run of `task`, checked, starts from, on the machine that the configuration at `config`
/// names, each step refused before the next begins: the configuration read, and an upstream found
/// to serve the agent's model route; the engine reached, and kept in `engine`; the provider
/// connected to it, so that an engine that can't run sandboxes is refused before any image is
/// built; then the task's images built on it ([`star::Task::build`]).
///
/// # Errors
///
/// As each step fails; gives each build that failed, at its place.
fn prepared<'e>(
    task: star::Task<'_>,
    config: &Path,
    engine: &'e mut Option<Engine>,
) -> Result<Result<Prepared<'e>, lang::Problems>, String> {
    let config::Config {
        docker: config::Docker { context },
        upstreams,
    } = config::read(config)?;
    let workflow = task.workflow();
    let serving = match &workflow.sandbox(workflow.agent().role).model_route {
        Some(route) => Some(upstreams.serving(&route.models).context(config.display())?),
        None => None,
    };
    let engine = &*engine.insert(crate::engine(context)?);
    let docker = Docker::connect(engine)?;
    Ok(task.build(engine).map(|workflow| Prepared {
        workflow,
        docker,
        serving,
    }))
}

/// Checks the task that `flags` name, as `pumpkin check --help` says, saying each of its
/// problems, if it has any; returns 0 if it has none.
fn check(flags: cli::Check) -> Result<ExitCode, String> {
    let cli::Check {
        entry,
        args: cli::TaskArgs(args),
    } = flags;
    let formatter = style::Formatter::beside()?;
    match star::check(&entry, &args, &formatter) {
        Ok(Ok(())) => Ok(ExitCode::SUCCESS),
        Ok(Err(problems)) => Err(task_problems(&entry, &problems)),
        Err(error) => Err(format!("{entry}: {error}")),
    }
}

/// Builds the task that `flags` name, as `pumpkin build --help` says, on the engine that the
/// configuration names, saying each of its problems if it has any, and else printing its IR;
/// returns 0 once the IR is printed.
fn build(flags: cli::Build) -> Result<ExitCode, String> {
    let cli::Build {
        entry,
        args: cli::TaskArgs(args),
        config,
    } = flags;
    let built = star::checked(&entry, &args, |task| {
        let config = match config {
            Some(config) => config,
            None => default_config()?,
        };
        let engine = configured_engine(&config)?;
        Ok(task.build(&engine))
    })?;
    let workflow = built.map_err(|problems| task_problems(&entry, &problems))?;
    // As the run's record keeps it in `ir.json`.
    let ir = format!("{:#}\n", serde_json::json!(workflow.ir()));
    print(ir.as_bytes()).context("printing the IR failed")?;
    Ok(ExitCode::SUCCESS)
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

/// Waits until the run that `flags` name, in their runs directory or [`default_runs`], has
/// ended, and prints its outcome; returns the exit status that `wait`'s help gives.
fn wait(flags: cli::Wait) -> Result<ExitCode, String> {
    let cli::Wait { run, runs } = flags;
    let record = match run {
        // A path needs no runs directory, and so no `HOME`.
        cli::RunName::Record(path) => Record::open(&path)?,
        cli::RunName::Prefix(prefix) => {
            let runs = match runs {
                Some(runs) => runs,
                None => default_runs()?,
            };
            Record::find(&runs, &prefix)?
        }
    };
    follow(&record, "wait")
}

/// Waits until the run of `record` has ended, saying that ending `pumpkin command` does not stop
/// it, and prints its outcome; returns the exit status that `wait`'s help gives.
fn follow(record: &Record, command: &str) -> Result<ExitCode, String> {
    let ending = record.wait(|| {
        say(format_args!(
            "following run {}, recorded in {}: ending `pumpkin {command}` does not stop it",
            record.id(),
            record.dir().display()
        ));
    })?;
    end(record, &ending)
}

/// Prints the outcome of the run of `record`, which ended as `ending` says, then the standard
/// error of a final judge that returned no result ([`show_judge_stderr`]), then the line that
/// closes the ending ([`closing`]); returns the exit status that `run`'s and `wait`'s help give.
fn end(record: &Record, ending: &Ending) -> Result<ExitCode, String> {
    print_outcome(record.dir())?;
    let (exit, how) = match ending {
        Ending::Scored(how) => (ExitCode::SUCCESS, how),
        Ending::Unscored(why) => (ExitCode::from(3), why),
        Ending::NoResult { why, judge_stderr } => {
            match judge_stderr {
                Ok(path) => show_judge_stderr(path),
                Err(error) => say(format_args!(
                    "the final judge's standard error can't be found: {error}"
                )),
            }
            (ExitCode::from(3), why)
        }
        Ending::Failed(how) => (ExitCode::FAILURE, how),
    };
    say(closing(record.id(), how, record.dir()));
    Ok(exit)
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
        for c in line.chars() {
            // Control characters would steer the operator's terminal.
            if c.is_control() && c != '\t' {
                shown.extend(c.escape_debug());
            } else {
                shown.push(c);
            }
        }
        shown.push('\n');
    }
    shown
}

/// `$HOME/.local/share/pumpkin/runs`.
fn default_runs() -> Result<PathBuf, String> {
    Ok(home("name a directory with `--runs`")?.join(".local/share/pumpkin/runs"))
}

/// `$HOME/.config/pumpkin/pumpkin.toml`.
fn default_config() -> Result<PathBuf, String> {
    Ok(home("name the configuration with `--config`")?.join(".config/pumpkin/pumpkin.toml"))
}

/// The home directory, `HOME`; failing without one, the error says what the operator can do
/// `instead`.
fn home(instead: &str) -> Result<PathBuf, String> {
    // From `HOME` alone, not `XDG_DATA_HOME` or `XDG_CONFIG_HOME`: a run already reads `HOME`,
    // for `docker`, so where its files are takes no variable of its own.
    let home = env::var_os("HOME").filter(|home| !home.is_empty());
    home.map(PathBuf::from)
        .ok_or_else(|| format!("HOME is unset or empty: set it, or {instead}"))
}

/// Says each of `errors`, of the IR in the file at `path`, on a line of its own, so that the
/// error the caller says last is still one line: the one returned, which says that `what` has
/// them, and how many.
fn said(path: &Path, errors: &ParseErrors, what: &str) -> String {
    let problems = errors.as_slice();
    for problem in problems {
        say(format_args!("{}: {problem}", path.display()));
    }
    let (count, s) = (problems.len(), if problems.len() == 1 { "" } else { "s" });
    let them = if count == 1 { "it" } else { "them" };
    format!(
        "{}: {what} has {count} problem{s}: fix {them} and run again",
        path.display()
    )
}

/// The line that closes a list of `count` problems, which says that `has` them, and how many.
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

/// The Docker engine of the context that the configuration at `path` names.
fn configured_engine(path: &Path) -> Result<Engine, String> {
    let config::Config {
        docker: config::Docker { context },
        upstreams: _,
    } = config::read(path)?;
    engine(context)
}

/// The Docker engine that `context` reaches, reported. A rootful engine is announced, since its
/// `docker` group is root-equivalent.
fn engine(context: ContextName) -> Result<Engine, String> {
    let engine = Engine::connect(context)?;
    let report = engine.report();
    if !report.security.rootless {
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

/// Writes `message` to standard error, as `pumpkin: <message>`.
fn say(message: impl fmt::Display) {
    to_stderr(format!("pumpkin: {message}\n").as_bytes());
}

/// Writes `bytes` to standard error.
fn to_stderr(bytes: &[u8]) {
    // Writing to a terminal that has hung up fails, and `eprintln!` would panic; the exit
    // status and the run's record still say how it ended.
    let _ = io::stderr().write_all(bytes);
}
