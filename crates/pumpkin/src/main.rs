//! `pumpkin`, the operator's command line. [`flags`] holds its grammar and describes each
//! command, as `pumpkin --help` prints it. `run` starts a workflow's run with [`runtime`], on the
//! Docker provider, in a driver process ([`driver`]), with the `sandboxd` that [`sandboxd`]
//! finds, and [`config`] reads the machine's configuration.

use std::ffi::OsString;
use std::io::{self, Write as _};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, ExitStatus};
use std::time::SystemTime;
use std::{env, fmt, fs};

use docker::{ContextName, Engine};
use driver::{ARGV0, Driver, Handoff};
use provider::Provider;
use runtime::{Ending, Record, Run};
use sandboxd::Sandboxd;
use schema::ir;
use schema::workflow::{ParseErrors, Workflow};
use stdx::Context;

mod config;
mod driver;
mod sandboxd;

/// The grammar of `pumpkin`'s command line, from which xflags generates its parser, the types
/// below and `--help`.
mod flags {
    #![expect(
        unreachable_pub,
        reason = "xflags declares the types it generates `pub`"
    )]
    #![expect(
        clippy::allow_attributes,
        clippy::allow_attributes_without_reason,
        reason = "xflags generates `#[allow(dead_code)]`, without a reason, on its parsers"
    )]

    use std::path::PathBuf;

    xflags::xflags! {
        src "./src/main.rs"

        /// The operator's command line, which runs workflows.
        cmd pumpkin {
            /// Starts a run of a workflow on the engine of the configured Docker context, with its
            /// record in `<runs>/<id>/`, and prints its id once what its sandboxes run is published.
            /// A driver process drives the run, and writes what it says after the start to the
            /// record's `driver.log`. Exits with 0 once the run has started, 2 on a usage error, and
            /// 1 on any other error, after printing a run's `outcome.json` if it failed to start;
            /// each non-zero exit ends with a `pumpkin:` line on the standard error that says why.
            /// What ends this process before the start ends the driver too. Killing the driver
            /// leaves the run's containers (labelled `pumpkin.run=<id>`), its socket directory and
            /// a record without an `outcome.json`.
            cmd run {
                /// The workflow's IR, as JSON.
                required ir: PathBuf
                /// The static `sandboxd` binary for every sandbox, which `cargo xtask sandboxd`
                /// builds: by default, the `sandboxd` beside this `pumpkin`, where
                /// `cargo xtask install` puts one.
                optional --sandboxd path: PathBuf
                /// The directory runs are recorded in: `$HOME/.local/share/pumpkin/runs` by
                /// default.
                optional --runs dir: PathBuf
                /// The machine's configuration: a TOML file whose `[docker]` table gives the
                /// `context` that reaches the engine, as `docker context ls` lists it, and whose
                /// `[upstreams.<name>]` tables, if any, each give an upstream that a model route
                /// may reach: the base `url` of its API, below which the route reaches
                /// `/responses`, the `key_file` that holds its key, below the configuration's
                /// directory unless absolute, and the `models` it serves, each listed once, with
                /// the `name` a route allows and the `upstream_name` it takes.
                /// `$HOME/.config/pumpkin/pumpkin.toml` by default.
                optional --config file: PathBuf
                /// Once the run has started, follows it to its end as `pumpkin wait` does, and
                /// prints its `outcome.json` instead of the id, with the exit status that
                /// `pumpkin wait` gives.
                optional --wait
            }

            /// Waits until a run has ended, then prints its `outcome.json`. Exits with 0 if the
            /// run finished with a score, 3 if it finished without one, 2 on a usage error, and
            /// 1 on any other error, a run that failed, or was lost as its driver ended without
            /// an `outcome.json`, included; every exit but 0 ends with a `pumpkin:` line on the
            /// standard error that says why. Ending this process does not stop the run. A record
            /// made before records were locked has no `driver.pid`, and is refused.
            cmd wait {
                /// The run: its id, a prefix of its id or of the 8 random digits that end it,
                /// that no other run in `--runs` shares, or the path of its record, which has a
                /// `/`.
                required run: String
                /// The directory runs are recorded in: `$HOME/.local/share/pumpkin/runs` by
                /// default.
                optional --runs dir: PathBuf
            }
        }
    }
    // generated start
    // The following code is generated by `xflags` macro.
    // Run `env UPDATE_XFLAGS=1 cargo build` to regenerate.
    #[derive(Debug)]
    pub struct Pumpkin {
        pub subcommand: PumpkinCmd,
    }

    #[derive(Debug)]
    pub enum PumpkinCmd {
        Run(Run),
        Wait(Wait),
    }

    #[derive(Debug)]
    pub struct Run {
        pub ir: PathBuf,

        pub sandboxd: Option<PathBuf>,
        pub runs: Option<PathBuf>,
        pub config: Option<PathBuf>,
        pub wait: bool,
    }

    #[derive(Debug)]
    pub struct Wait {
        pub run: String,

        pub runs: Option<PathBuf>,
    }

    impl Pumpkin {
        #[allow(dead_code)]
        pub fn from_env_or_exit() -> Self {
            Self::from_env_or_exit_()
        }

        #[allow(dead_code)]
        pub fn from_env() -> xflags::Result<Self> {
            Self::from_env_()
        }

        #[allow(dead_code)]
        pub fn from_vec(args: Vec<std::ffi::OsString>) -> xflags::Result<Self> {
            Self::from_vec_(args)
        }
    }
    // generated end
}

fn main() -> ExitCode {
    // A run's judge calls, and each session's reader, run on threads of their own.
    stdx::abort_on_panic();
    let mut args = env::args_os();
    let argv0 = args.next();
    if argv0.is_some_and(|name| name.as_bytes() == ARGV0.to_bytes()) {
        return finish(driver_run(args.collect()));
    }
    let done = match flags::Pumpkin::from_env().map(|flags| flags.subcommand) {
        Ok(flags::PumpkinCmd::Run(flags)) => run(flags),
        Ok(flags::PumpkinCmd::Wait(flags::Wait { run, runs })) => wait(&run, runs),
        Err(error) if error.is_help() => error.exit(),
        // A usage error exits with 2, as xflags exits, apart from the command's errors.
        Err(error) => {
            say(format_args!("{error}: see `pumpkin --help`"));
            return ExitCode::from(2);
        }
    };
    finish(done)
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
fn run(flags: flags::Run) -> Result<ExitCode, String> {
    let flags::Run {
        ir,
        sandboxd,
        runs,
        config,
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
    // IR comes last, after `--`, so that a path that starts with `-` is not taken for a flag.
    let mut args: Vec<OsString> = vec!["run".into()];
    args.extend(["--runs".into(), runs.clone().into()]);
    for (flag, value) in [("--sandboxd", sandboxd), ("--config", config)] {
        if let Some(value) = value {
            args.extend([flag.into(), value.into()]);
        }
    }
    args.extend(["--".into(), ir.into()]);
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
            let mut stdout = io::stdout().lock();
            (writeln!(stdout, "{id}").and_then(|()| stdout.flush())).context(format_args!(
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
/// [`ARGV0`], reads the configuration and key, reaches the engine, places the roles, makes the
/// record and tells `run` its id, starts the run and tells `run` so, then drives it to its end.
///
/// # Errors
///
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
    let flags = flags::Pumpkin::from_vec(args.collect()).map_err(|error| error.to_string())?;
    let flags::PumpkinCmd::Run(flags::Run {
        ir,
        sandboxd,
        runs: Some(runs),
        config,
        wait: _,
    }) = flags.subcommand
    else {
        return Err(refused());
    };
    // The IR, `sandboxd`, the configuration and its upstream's key are checked before the
    // engine is reached, so a mistake in one needs no engine and makes no record. The roles are
    // placed in the engine's classes before the record is begun. What only the provider refuses,
    // such as a `sandboxd` it can't mount, is refused once the record exists.
    let workflow = read_workflow(&ir)?;
    let sandboxd = Sandboxd::find(sandboxd)?;
    let config = match config {
        Some(config) => config,
        None => default_config()?,
    };
    let config::Config {
        docker: config::Docker { context },
        upstreams,
    } = config::read(&config)?;
    let serving = match &workflow.agent().sandbox.model_route {
        Some(route) => Some(upstreams.serving(&route.models).context(config.display())?),
        None => None,
    };
    let engine = engine(context)?;
    let placed = workflow.place(&engine.ready().classes);
    let placing = "placing the workflow's roles in the engine's classes";
    let placed = placed.map_err(|errors| said(&ir, &errors, placing))?;
    let run = Run::prepare(placed, &runs, SystemTime::now(), serving)?;
    let (id, dir) = (run.id().to_owned(), run.dir().to_owned());
    // Said before `run` hears of it, so that whatever ends both later, the record is named.
    say(format_args!("run {id} records into {}", dir.display()));
    driver::locked(&id)?;
    // `execute` records why the run failed in its `outcome.json`, which `run` prints if the run
    // failed to start.
    let executed = run.execute(&engine, sandboxd.path(), || driver::announce(&dir));
    executed.map_err(|reason| format!("run {id} failed: {reason}"))?;
    Ok(ExitCode::SUCCESS)
}

/// Waits until the run that `wait` names in `runs`, or [`default_runs`], has ended, and prints its
/// outcome; returns the exit status that `wait`'s help gives.
fn wait(run: &str, runs: Option<PathBuf>) -> Result<ExitCode, String> {
    // A path needs no runs directory, and so no `HOME`.
    let record = if run.contains('/') {
        Record::open(Path::new(run))?
    } else {
        let runs = match runs {
            Some(runs) => runs,
            None => default_runs()?,
        };
        Record::find(&runs, run)?
    };
    follow(&record, "wait")
}

/// Waits until the run of `record` has ended, saying that ending `pumpkin command` does not stop
/// it, and prints its outcome; returns the exit status that `wait`'s help gives.
fn follow(record: &Record, command: &str) -> Result<ExitCode, String> {
    let ending = record.wait(|| {
        say(format_args!(
            "following run {}: ending `pumpkin {command}` does not stop it",
            record.id()
        ));
    })?;
    end(record, &ending)
}

/// Prints the outcome of `record`, a run that has `ended`, and returns the exit status that
/// `run`'s and `wait`'s help give; a run without a score says why on the standard error.
fn end(record: &Record, ending: &Ending) -> Result<ExitCode, String> {
    print_outcome(record.dir())?;
    let id = record.id();
    Ok(match ending {
        Ending::Scored => ExitCode::SUCCESS,
        Ending::Unscored(why) => {
            say(format_args!("run {id} {why}"));
            ExitCode::from(3)
        }
        Ending::Failed(why) => {
            say(format_args!("run {id} {why}"));
            ExitCode::FAILURE
        }
    })
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

/// The workflow whose IR is in the file at `path`.
fn read_workflow(path: &Path) -> Result<Workflow, String> {
    let json = fs::read(path).map_err(|error| {
        format!(
            "the workflow's IR {} can't be read ({error}): name the file that holds it",
            path.display()
        )
    })?;
    let ir: ir::Workflow = serde_json::from_slice(&json)
        .context(format_args!("the workflow's IR {}", path.display()))?;
    Workflow::parse(&ir).map_err(|errors| said(path, &errors, "the workflow"))
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
    let mut stdout = io::stdout().lock();
    (stdout.write_all(&printed))
        .and_then(|()| stdout.flush())
        .context("printing the outcome failed")
}

/// Writes `message` to standard error, as `pumpkin: <message>`.
fn say(message: impl fmt::Display) {
    // Writing to a terminal that has hung up fails, and `eprintln!` would panic; the exit
    // status and the run's record still say how it ended.
    let _ = writeln!(io::stderr(), "pumpkin: {message}");
}
