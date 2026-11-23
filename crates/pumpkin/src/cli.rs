//! `pumpkin`'s command line: [`parse`] reads it with clap into a [`Command`], each command a
//! struct of its operands and flags.
//!
//! The doc comments of the commands and their arguments are the help texts, as clap reads them:
//! the first line is the summary. They say what each command and option means, and the guides in
//! `docs/` say how the commands fit together. `-h` or `--help` before `--` prints the help of the
//! command that the line names, as `pumpkin help` does, which also prints the topics, such as
//! [`CONFIG`]. The commands that [`RARE`] names are hidden from clap's list and listed apart. The
//! driver's command, which `pumpkin run` starts, is hidden and has no help.
//! `pumpkin-admin`'s command line is [`admin`]'s. What a command does is left to its own module.

pub(crate) mod admin;
mod exit;
mod notes;

use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt as _;
use std::path::PathBuf;
use std::{iter, mem};

use clap::builder::styling::{AnsiColor, Effects, Style, Styles};
use clap::builder::{OsStringValueParser, StyledStr, TypedValueParser};
use clap::error::ErrorKind;
use clap::{
    Arg, ArgAction, ArgMatches, Args, CommandFactory as _, FromArgMatches, Parser, Subcommand,
};
use members::Member;
use provider::RunTag;
use schema::ir::Sha256Digest;

pub(crate) use self::exit::Exit;
use self::notes::{ADMIN, CONFIG, Layout, NOTES, Notes, section};
use crate::Reader;
use crate::ending::Printed;
use crate::star::Entry;

/// Run workflows of agents in sandboxes.
///
/// In each run, an agent works in a sandbox and a final call judges what it submits. The
/// repository's `.pumpkin` directory holds the runs' records and the state of builds and
/// providers. A command's `--runs` names another directory for the records.
#[derive(Parser)]
#[command(
    name = "pumpkin",
    disable_help_flag = true,
    disable_help_subcommand = true,
    disable_version_flag = true,
    subcommand_value_name = "command",
    subcommand_help_heading = "Commands"
)]
enum Parsed {
    /// Check tasks without building or running them.
    ///
    /// `check` evaluates each task's entry file and the files it loads with the given `--arg`s.
    /// Every task gets the same `--arg`s. It then checks each image's recipe and the workflow, and
    /// lints the files it read. It says every problem in every task, and prints nothing when it
    /// finds none. It reads only files below each task's root. It reaches no Docker engine,
    /// network or configuration, so an image's build can still fail.
    Check(Check),
    /// Format `.star` files.
    ///
    /// `fmt` rewrites each `.star` file in the one style of the workflow language. It formats the
    /// files it is given and the `.star` files below the directories it is given. Below a
    /// directory, it skips names that start with `.` and anything a symbolic link leads to. If a
    /// file can't be formatted, `fmt` says why and formats the others. It prints nothing on
    /// success.
    Fmt(Fmt),
    /// Build a task and start a run of it.
    ///
    /// `run` starts a driver process, which builds the task's images, places each role on a
    /// provider and drives the run. `run` prints the run's id once the run has started,
    /// and the run goes on without `run`.
    ///
    /// With `[controller]` in the configuration, `run` sends the task to the controller, which
    /// builds and runs it. `run` then prints the id at once and returns before the build.
    /// `pumpkin wait` follows the build and the run. `--runs` and `--sandboxd` are refused.
    Run(Run),
    /// Wait for a run to end and say how it ended.
    ///
    /// While the run goes on, `wait` says each of its milestones on the standard error. Once the
    /// run has ended, `wait` says its summary and the command that runs it again. Ending `wait`
    /// doesn't stop the run, but `pumpkin cancel` does.
    ///
    /// With `[controller]` in the configuration and no `--runs`, `wait` follows the run on the
    /// controller. It then copies the record into the default runs directory.
    Wait(Wait),
    /// List runs and their states.
    ///
    /// `ls` prints a line for each run, oldest first, then a line of totals. Without a prefix, it
    /// lists the runs that are live or ended within the last day, and the totals say how to list
    /// the others. A live run's line gives its phase and the bound on it. An ended run's line
    /// gives its state, how long it ran and its result's values. If a run can't be listed, `ls`
    /// says why and lists the others. With `[controller]` in the configuration and no `--runs`,
    /// `ls` lists the controller's runs beside this machine's copies.
    Ls(Ls),
    /// Stop a run and remove what it left.
    ///
    /// `cancel` sends SIGTERM to a live run's driver and waits for the run to end. Ending `cancel`
    /// doesn't end the stop. `cancel` then removes what the run left, as `pumpkin reap` does, and
    /// says how the run ended. A run that has ended or is lost gets no signal.
    ///
    /// With `[controller]` in the configuration and no `--runs`, `cancel` stops the run on the
    /// controller. It then copies a started run's record here, as `pumpkin wait` does.
    Cancel(CancelRun),
    /// Serve the runs to this machine's browser.
    ///
    /// `view` prints the address to open and serves until a signal stops it. It listens on
    /// 127.0.0.1 alone and needs the token in the address. The page lists the runs newest first,
    /// each as `ls` says it. A run's page shows its summary, its sandboxes' lanes, who talks to
    /// whom and its log. The page changes a run only by a cancel, which runs `pumpkin cancel`.
    /// With `[controller]` in the configuration and no `--runs`, `view` serves the controller's
    /// runs.
    View(View),
    /// Print the help of a command or a topic.
    ///
    /// Without a command or a topic, `help` prints what `pumpkin --help` does.
    Help {
        /// A command, or the topic `config`.
        #[arg(value_name = "topic")]
        topic: Option<String>,
    },
    /// Remove what ended runs left.
    ///
    /// For each run whose driver has ended, `reap` deletes the sandboxes that remain on each
    /// provider it ran on. It removes the run's socket directories and records a lost run as
    /// failed. It leaves a live run to its driver. If a run can't be reaped, `reap` says why and
    /// reaps the others. It prints the ids of the runs it cleaned.
    ///
    /// With `[controller]` in the configuration and no `--runs`, `reap` reaps the controller's
    /// runs that this build made.
    #[command(hide = true)]
    Reap(Reap),
    /// Run the controller's own commands.
    ///
    /// These actions run on the controller. `serve` answers a client's `ssh` link and starts each
    /// `job` in a unit of its own. The controller's boot runs `boot` of each installed build. The
    /// controller's admin has its own commands, `pumpkin-admin`'s.
    #[command(hide = true)]
    #[command(disable_help_subcommand = true, subcommand_value_name = "action")]
    #[command(subcommand_help_heading = "Actions")]
    Controller {
        #[command(subcommand)]
        action: Action,
    },
    /// Drive a run that `pumpkin run` starts.
    #[command(hide = true)]
    Driver(Driver),
}

// Hidden from clap's list of commands, so `command` lists them apart.
const RARE: [&str; 2] = ["reap", "controller"];

/// The actions of `pumpkin controller`.
#[derive(Subcommand)]
enum Action {
    /// Run a client's `run` as a job.
    ///
    /// `job` makes the job's tree in its working directory, which is `~/trees/<repository>/<id>`.
    /// It then runs the command there. The controller's settings are those that the host file,
    /// /etc/pumpkin/controller.toml, names.
    Job {
        /// The job's id, which its run takes.
        #[arg(value_name = "id", value_parser = run_tag)]
        id: RunTag,
        /// The SHA-256 of the manifest of the job's tree in the controller's store.
        #[arg(long, value_name = "sha256")]
        #[arg(value_parser = tree)]
        tree: Sha256Digest,
        /// `run` with its arguments, but not `--config`, which the job writes.
        #[arg(last = true, required = true, value_name = "command")]
        command: Vec<OsString>,
    },
    /// Reap what lost runs left, at the controller's boot.
    Boot,
    /// Answer a client on the standard input and output.
    ///
    /// Each job that `serve` starts belongs to the member that the link acts for. `serve` refuses
    /// to cancel another member's job unless the client asks for any member's.
    Serve {
        /// The member that the controller's forced command names for the link.
        #[arg(long, value_name = "member")]
        #[arg(value_parser = |member: &str| Member::try_from(member.to_owned()))]
        member: Member,
    },
}

/// What a command line asks for. `Job`, `Boot` and `Serve` are the controller's own side,
/// `pumpkin controller <action>`. `Driver` is the process that `run` starts. `Install`,
/// `CheckDeployment`, `BootAll`, `Idle`, `Members` and `CheckMembers` are `pumpkin-admin`'s.
/// `Help` and `Print` are for the standard output: `Print` holds the version, a topic or a policy.
pub(crate) enum Command {
    Run(Run),
    Wait(Wait),
    Ls(Ls),
    Cancel(CancelRun),
    Reap(Reap),
    Check(Check),
    Fmt(Fmt),
    View(View),
    Job(Job),
    Boot,
    Driver(Driver),
    Install(aws::ParameterArn),
    CheckDeployment(aws::ParameterArn),
    BootAll,
    Idle,
    Members,
    CheckMembers(PathBuf),
    Serve(Member),
    Help(Help),
    Print(StyledStr),
}

impl Command {
    /// The `run` that this command line asks for, if it asks for one.
    pub(crate) fn into_run(self) -> Option<Run> {
        match self {
            Self::Run(run) => Some(run),
            Self::Wait(_)
            | Self::Ls(_)
            | Self::Cancel(_)
            | Self::Reap(_)
            | Self::Check(_)
            | Self::Fmt(_)
            | Self::View(_)
            | Self::Job(_)
            | Self::Boot
            | Self::Driver(_)
            | Self::Install(_)
            | Self::CheckDeployment(_)
            | Self::BootAll
            | Self::Idle
            | Self::Members
            | Self::CheckMembers(_)
            | Self::Serve(_)
            | Self::Help(_)
            | Self::Print(_) => None,
        }
    }
}

/// The help of `program`, of a command that `name` names, or of the controller's `action`.
pub(crate) struct Help {
    program: Program,
    name: Option<String>,
    action: Option<String>,
}

impl Help {
    pub(crate) fn render(&self) -> StyledStr {
        // The help's data is fixed, so a debug build checks it each time it prints a help, and the
        // tests, which build with debug assertions, run the check. A release build never parses
        // its examples, nor panics on one, as clap's own check of a command is debug-only:
        // https://github.com/clap-rs/clap/blob/13f2db5072d600c11d8d6298e4e9ba53ffc6c1ab/clap_builder/src/builder/command.rs#L4487-L4488
        // As `parse` renders no help, the check can't recurse.
        if cfg!(debug_assertions) {
            notes::check();
        }
        let Self {
            program,
            name,
            action,
        } = self;
        help(&mut program.command(), name.as_deref(), action.as_deref())
    }
}

/// The program that a command line is for. One executable runs as either, by its name.
#[derive(Clone, Copy)]
enum Program {
    Pumpkin,
    Admin,
}

impl Program {
    const ALL: [Self; 2] = [Self::Pumpkin, Self::Admin];

    fn name(self) -> &'static str {
        match self {
            Self::Pumpkin => "pumpkin",
            Self::Admin => admin::NAME,
        }
    }

    fn command(self) -> clap::Command {
        match self {
            Self::Pumpkin => command(),
            Self::Admin => admin::command(),
        }
    }

    fn notes(self) -> &'static [Notes] {
        match self {
            Self::Pumpkin => NOTES,
            Self::Admin => ADMIN,
        }
    }

    fn parse(self, args: impl IntoIterator<Item = OsString>) -> Result<Command, Refusal> {
        match self {
            Self::Pumpkin => parse(args),
            Self::Admin => admin::parse(args),
        }
    }
}

const ARG: &str = "arg";

/// The `--arg` flag of the commands that evaluate a task, each value read as
/// [`lang::Args::parse`] reads it.
pub(crate) struct TaskArgs(pub(crate) lang::Args);

impl Args for TaskArgs {
    fn augment_args(command: clap::Command) -> clap::Command {
        let arg = Arg::new(ARG).long(ARG).value_name("name>=<value");
        let arg = arg.action(ArgAction::Append);
        let arg = arg.help("Set the arg that an `arg()` call declares as <name>");
        let utf8 = utf8(", as every arg's name and value is: give it in UTF-8");
        command.arg(arg.value_parser(utf8))
    }

    fn augment_args_for_update(command: clap::Command) -> clap::Command {
        Self::augment_args(command)
    }
}

impl FromArgMatches for TaskArgs {
    fn from_arg_matches(matches: &ArgMatches) -> Result<Self, clap::Error> {
        let given = matches.get_many::<String>(ARG).into_iter().flatten();
        let args = lang::Args::parse(given.map(String::as_str)).map_err(|problems| {
            // clap says the first problem of a line alone.
            let Some(first) = problems.into_iter().next() else {
                unreachable!(
                    "`lang::Args::parse` failed with no problem, though it fails with each"
                )
            };
            clap::Error::raw(ErrorKind::ValueValidation, first)
        })?;
        Ok(Self(args))
    }

    fn update_from_arg_matches(&mut self, matches: &ArgMatches) -> Result<(), clap::Error> {
        *self = Self::from_arg_matches(matches)?;
        Ok(())
    }
}

const OUTCOME: &str = "outcome";

impl Args for Printed {
    fn augment_args(command: clap::Command) -> clap::Command {
        let outcome = Arg::new(OUTCOME).long(OUTCOME).action(ArgAction::SetTrue);
        let help = "Print the run's outcome.json on the standard output once it ends, even if it \
                    failed to start";
        command.arg(outcome.help(help))
    }

    fn augment_args_for_update(command: clap::Command) -> clap::Command {
        Self::augment_args(command)
    }
}

impl FromArgMatches for Printed {
    fn from_arg_matches(matches: &ArgMatches) -> Result<Self, clap::Error> {
        Ok(if matches.get_flag(OUTCOME) {
            Self::Outcome
        } else {
            Self::Nothing
        })
    }

    fn update_from_arg_matches(&mut self, matches: &ArgMatches) -> Result<(), clap::Error> {
        *self = Self::from_arg_matches(matches)?;
        Ok(())
    }
}

const WAIT: &str = "wait";

/// Whether `run` follows the run to its end (`Some`), and what it then prints.
pub(crate) struct Waiting(pub(crate) Option<Printed>);

impl Args for Waiting {
    fn augment_args(command: clap::Command) -> clap::Command {
        let wait = Arg::new(WAIT).long(WAIT).action(ArgAction::SetTrue);
        let wait = wait.help("Follow the run to its end and exit as `pumpkin wait` does");
        let command = Printed::augment_args(command.arg(wait));
        command.mut_arg(OUTCOME, |outcome| outcome.requires(WAIT))
    }

    fn augment_args_for_update(command: clap::Command) -> clap::Command {
        Self::augment_args(command)
    }
}

impl FromArgMatches for Waiting {
    fn from_arg_matches(matches: &ArgMatches) -> Result<Self, clap::Error> {
        let printed = Printed::from_arg_matches(matches)?;
        Ok(Self(matches.get_flag(WAIT).then_some(printed)))
    }

    fn update_from_arg_matches(&mut self, matches: &ArgMatches) -> Result<(), clap::Error> {
        *self = Self::from_arg_matches(matches)?;
        Ok(())
    }
}

/// Parses a UTF-8 value, and refuses another with `it isn't UTF-8` followed by `why`, which
/// starts with the punctuation that joins the two.
fn utf8(why: &'static str) -> impl TypedValueParser<Value = String> {
    OsStringValueParser::new().try_map(move |value| {
        value
            .into_string()
            .map_err(|_| format!("it isn't UTF-8{why}"))
    })
}

const RECORDED: &str = ", as a run's record keeps each path that `run` is given: name one that is";

const NAMED_NEXT: &str = ", as the commands that `pumpkin` says to run next name it: name one \
                          that is";

const RUNS_NAMED_NEXT: &str = ", as the commands that `pumpkin` says to run next name it: name \
                               a directory with a UTF-8 path";

fn entry() -> impl TypedValueParser<Value = Entry> {
    utf8(NAMED_NEXT).try_map(Entry::parse)
}

/// Its paths are UTF-8, so that a run's record keeps them, and a command that names them names the
/// same files.
#[derive(Args)]
pub(crate) struct Run {
    #[arg(value_name = "entry", value_parser = entry(), help = ENTRY_HELP)]
    pub(crate) entry: Entry,
    #[command(flatten)]
    pub(crate) args: TaskArgs,
    /// The directory to record runs in, each in `<dir>/<id>` (default: `runs` in the
    /// repository's `.pumpkin`).
    #[arg(long, value_name = "dir", value_parser = utf8(RECORDED))]
    pub(crate) runs: Option<String>,
    #[arg(long, value_name = "file", value_parser = utf8(RECORDED), help = CONFIG_HELP)]
    pub(crate) config: Option<String>,
    /// The static sandboxd that every sandbox runs, with the static ec2-boot beside it (default:
    /// the one beside `pumpkin`, where `cargo xtask build` puts both).
    #[arg(long, value_name = "path", value_parser = utf8(RECORDED))]
    pub(crate) sandboxd: Option<String>,
    #[command(flatten)]
    pub(crate) wait: Waiting,
}

impl Run {
    /// The path flags that were given, each as `(flag, value)`, in the order of the usage.
    pub(crate) fn given(&self) -> impl Iterator<Item = (&'static str, &str)> {
        let Self {
            entry: _,
            args: _,
            runs,
            config,
            sandboxd,
            wait: _,
        } = self;
        let flags = [
            ("--runs", runs),
            ("--config", config),
            ("--sandboxd", sandboxd),
        ];
        (flags.into_iter()).filter_map(|(flag, value)| Some((flag, value.as_deref()?)))
    }
}

const RUN_HELP: &str = "The run's id, a prefix of the id or of its last 8 digits that no other \
                        run shares, or its record's path with a `/`";

const RUNS_HELP: &str =
    "The directory to look the run up in (default: `runs` in the repository's `.pumpkin`)";

/// Its paths are UTF-8, as those of `ls`, `cancel` and `reap` are, so that a command that names
/// them names the same files.
#[derive(Args)]
pub(crate) struct Wait {
    #[arg(value_name = "run", value_parser = run_name(), help = RUN_HELP)]
    pub(crate) run: RunName,
    #[arg(long, value_name = "dir", value_parser = utf8(RUNS_NAMED_NEXT), help = RUNS_HELP)]
    pub(crate) runs: Option<String>,
    #[command(flatten)]
    pub(crate) printed: Printed,
}

#[derive(Args)]
pub(crate) struct View {
    /// The directory of the runs to serve (default: `runs` in the repository's `.pumpkin`).
    #[arg(long, value_name = "dir", value_parser = utf8(RUNS_NAMED_NEXT))]
    pub(crate) runs: Option<String>,
    /// The port on 127.0.0.1 to serve at (default: a free one).
    #[arg(long, value_name = "port")]
    pub(crate) port: Option<u16>,
}

#[derive(Args)]
pub(crate) struct Ls {
    /// The directory of the runs to list (default: `runs` in the repository's `.pumpkin`).
    #[arg(long, value_name = "dir", value_parser = utf8(RUNS_NAMED_NEXT))]
    pub(crate) runs: Option<String>,
    /// A prefix of the ids of the runs to list, such as a day's in UTC, as `20261110`.
    #[arg(value_name = "prefix", value_parser = utf8(", as every id is: name a prefix of an id"))]
    pub(crate) prefixes: Vec<String>,
}

/// What `pumpkin cancel` is asked for: a [`Cancel`], and whose runs its run is named among.
#[derive(Args)]
pub(crate) struct CancelRun {
    #[command(flatten)]
    pub(crate) cancel: Cancel,
    /// Match the run among every member's runs on the controller, not only this member's.
    #[arg(long)]
    pub(crate) any_member: bool,
}

#[derive(Args)]
pub(crate) struct Cancel {
    #[arg(value_name = "run", value_parser = run_name(), help = RUN_HELP)]
    pub(crate) run: RunName,
    #[arg(long, value_name = "dir", value_parser = utf8(RUNS_NAMED_NEXT), help = RUNS_HELP)]
    pub(crate) runs: Option<String>,
    // The Modal configuration file that a client sends with a cancel, which no flag gives.
    #[arg(skip)]
    pub(crate) modal_config: Option<PathBuf>,
}

#[derive(Args)]
pub(crate) struct Reap {
    /// The directory of the runs to reap (default: `runs` in the repository's `.pumpkin`).
    #[arg(long, value_name = "dir", value_parser = utf8(RUNS_NAMED_NEXT))]
    pub(crate) runs: Option<String>,
    // The Modal configuration file that a client sends with a reap, as with a cancel.
    #[arg(skip)]
    pub(crate) modal_config: Option<PathBuf>,
}

#[derive(Args)]
pub(crate) struct Check {
    #[arg(value_name = "entry", required = true, value_parser = entry(), help = ENTRY_HELP)]
    pub(crate) entries: Vec<Entry>,
    #[command(flatten)]
    pub(crate) args: TaskArgs,
}

#[derive(Args)]
pub(crate) struct Fmt {
    /// A `.star` file or a directory to format (default: the working directory).
    #[arg(value_name = "path", value_parser = utf8(NAMED_NEXT))]
    pub(crate) paths: Vec<String>,
    /// Name each file that isn't formatted, change none, and fail if there is one.
    #[arg(long)]
    pub(crate) check: bool,
}

const ENTRY_HELP: &str = "A task's entry file, below the nearest directory that holds PUMPKIN.star";

const CONFIG_HELP: &str = "The machine's configuration, which `pumpkin help config` describes \
                           (default: `$HOME/.config/pumpkin/pumpkin.toml`)";

pub(crate) struct Job {
    pub(crate) id: RunTag,
    pub(crate) tree: Sha256Digest,
    pub(crate) run: Run,
}

#[derive(Args)]
pub(crate) struct Driver {
    /// The process ID of the `run` that starts the driver.
    #[arg(value_name = "run-pid")]
    pub(crate) run_pid: u32,
    /// The run's id, which `run` mints.
    #[arg(value_name = "id", value_parser = run_tag)]
    pub(crate) id: RunTag,
    #[arg(value_name = "reader", value_parser = reader)]
    pub(crate) reader: Reader,
    /// `run` with its arguments.
    #[arg(last = true, required = true, value_name = "command")]
    pub(crate) run: Vec<OsString>,
}

fn reader(word: &str) -> Result<Reader, &'static str> {
    Reader::from_word(word).ok_or("it is neither `operator` nor `follower`: name one")
}

#[derive(Clone)]
/// A run named by its record's path, which has a `/`, or by its id or a prefix of it.
pub(crate) enum RunName {
    Record(String),
    Prefix(String),
}

fn run_tag(id: &str) -> Result<RunTag, String> {
    RunTag::try_from(id.to_owned())
}

fn tree(hex: &str) -> Result<Sha256Digest, &'static str> {
    crate::manifest::digest(hex)
        .ok_or("it isn't 64 lowercase hexadecimal digits: give the SHA-256 of the tree's manifest")
}

fn run_name() -> impl TypedValueParser<Value = RunName> {
    OsStringValueParser::new().try_map(|run| {
        if run.as_bytes().contains(&b'/') {
            return run.into_string().map(RunName::Record).map_err(|_| {
                "it isn't UTF-8, as the commands that `pumpkin` says to run next name it: name a \
                 record with a UTF-8 path"
            });
        }
        run.into_string().map(RunName::Prefix).map_err(|_| {
            "it isn't UTF-8, as every id is: name an id, a prefix of one, or a record's path, \
             which has a `/`"
        })
    })
}

/// A command line that clap refuses.
pub(crate) struct Refusal {
    pub(crate) error: clap::Error,
    /// The command line that prints the help of the command that was refused.
    pub(crate) help: String,
}

/// Reads `args`, which leave out the program's name, into the command that they ask for.
/// `-h` or `--help` before `--` is answered as [`Command::Help`], and `-V` or `--version` before
/// the command as [`Command::Print`], not refused.
pub(crate) fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Command, Refusal> {
    let args: Vec<OsString> = args.into_iter().collect();
    let mut cli = command();
    let (first, named) = first_word(&cli, &args);
    let action = (named.as_deref() == Some("controller"))
        .then(|| first.and_then(|at| args.get(at + 1)))
        .flatten()
        .and_then(|word| word.to_str())
        .filter(|word| !word.starts_with('-'))
        .filter(|word| {
            let controller = named.as_deref().and_then(|name| cli.find_subcommand(name));
            controller.is_some_and(|controller| controller.find_subcommand(word).is_some())
        })
        .map(str::to_owned);
    let printing = printed(
        Program::Pumpkin,
        &args,
        first,
        named.as_deref(),
        action.as_deref(),
    );
    if let Some(print) = printing {
        return Ok(print);
    }
    let refusal = |error| Refusal {
        error,
        help: match (&named, &action) {
            (Some(controller), Some(action)) => format!("pumpkin {controller} {action} --help"),
            (Some(name), None) => format!("pumpkin help {name}"),
            (None, _) => "pumpkin help".to_owned(),
        },
    };
    let line = iter::once(OsString::from("pumpkin")).chain(args.iter().cloned());
    let matches = cli.try_get_matches_from_mut(line).map_err(refusal)?;
    let parsed = Parsed::from_arg_matches(&matches).map_err(|error| {
        // Only `--arg`'s check fails here, in a command that the line names, whose usage it shows.
        match named
            .as_deref()
            .and_then(|name| cli.find_subcommand_mut(name))
        {
            Some(command) => refusal(error.format(command)),
            None => refusal(error.format(&mut cli)),
        }
    })?;
    command_of(&mut cli, parsed).map_err(refusal)
}

/// The first word of `args` that isn't a flag, and the name of `cli`'s listed command that it
/// names, if any. `args` are a command line of `cli` without the program's name.
// clap's error doesn't name the command it was found in; the first word does, as only `-h`,
// `--help`, `-V` and `--version` may come before it. A command that no list shows has no help to
// print or to point to.
fn first_word(cli: &clap::Command, args: &[OsString]) -> (Option<usize>, Option<String>) {
    let first = args
        .iter()
        .position(|arg| !arg.as_bytes().starts_with(b"-"));
    let named = first.and_then(|at| cli.find_subcommand(&args[at]));
    let named = named.filter(|command| listed(command));
    (first, named.map(|command| command.get_name().to_owned()))
}

/// What `args`, a command line of `program` without its name, ask it to print: the help of the
/// command that `named` and `action` name, for `-h` or `--help` before `--`, or the version, for
/// `-V` or `--version` before the word at `first`.
fn printed(
    program: Program,
    args: &[OsString],
    first: Option<usize>,
    named: Option<&str>,
    action: Option<&str>,
) -> Option<Command> {
    let mut options = args.iter().take_while(|arg| *arg != "--");
    if options.any(|arg| arg == "-h" || arg == "--help") {
        return Some(Command::Help(Help {
            program,
            name: named.map(str::to_owned),
            action: action.map(str::to_owned),
        }));
    }
    let before = &args[..first.unwrap_or(args.len())];
    if before.iter().any(|arg| arg == "-V" || arg == "--version") {
        let version = format!("{} {}\n", program.name(), runtime::BUILD);
        return Some(Command::Print(version.into()));
    }
    None
}

fn command_of(cli: &mut clap::Command, parsed: Parsed) -> Result<Command, clap::Error> {
    Ok(match parsed {
        Parsed::Run(run) => Command::Run(run),
        Parsed::Wait(wait) => Command::Wait(wait),
        Parsed::Ls(ls) => Command::Ls(ls),
        Parsed::Cancel(cancel) => Command::Cancel(cancel),
        Parsed::Reap(reap) => Command::Reap(reap),
        Parsed::Check(check) => Command::Check(check),
        Parsed::Fmt(fmt) => Command::Fmt(fmt),
        Parsed::View(view) => Command::View(view),
        Parsed::Controller {
            action: Action::Boot,
        } => Command::Boot,
        Parsed::Controller {
            action: Action::Serve { member },
        } => Command::Serve(member),
        Parsed::Controller {
            action: Action::Job { id, tree, command },
        } => Command::Job(Job {
            id,
            tree,
            run: job_run(cli, command)?,
        }),
        Parsed::Driver(driver) => Command::Driver(driver),
        Parsed::Help { topic } => return help_of(cli, topic.as_deref()),
    })
}

fn job_run(cli: &mut clap::Command, command: Vec<OsString>) -> Result<Run, clap::Error> {
    // The job's command is a whole command line of its own, so it is parsed as one.
    let parsed = parse(command).map_err(|refusal| refusal.error)?;
    parsed.into_run().ok_or_else(|| {
        let problem = "the job's command isn't `run`: give one";
        let controller = cli.find_subcommand_mut("controller");
        match controller.and_then(|it| it.find_subcommand_mut("job")) {
            Some(job) => job.error(ErrorKind::InvalidValue, problem),
            None => cli.error(ErrorKind::InvalidValue, problem),
        }
    })
}

fn help_of(cli: &mut clap::Command, topic: Option<&str>) -> Result<Command, clap::Error> {
    let Some(topic) = topic else {
        return Ok(Command::Help(Help {
            program: Program::Pumpkin,
            name: None,
            action: None,
        }));
    };
    if topic == "config" {
        return Ok(Command::Print(CONFIG.into()));
    }
    let names: Vec<String> = (cli.get_subcommands())
        .filter(|command| listed(command))
        .map(|command| format!("`{}`", command.get_name()))
        .collect();
    if names.iter().any(|name| name.trim_matches('`') == topic) {
        return Ok(Command::Help(Help {
            program: Program::Pumpkin,
            name: Some(topic.to_owned()),
            action: None,
        }));
    }
    let problem = format!(
        "there is no help for `{topic}`: expected a command ({}) or a topic (`config`)",
        names.join(", ")
    );
    // Found in `help`, so the error shows that command's usage.
    Err(match cli.find_subcommand_mut("help") {
        Some(command) => command.error(ErrorKind::InvalidValue, problem),
        None => cli.error(ErrorKind::InvalidValue, problem),
    })
}

fn listed(command: &clap::Command) -> bool {
    !command.is_hide_set() || RARE.contains(&command.get_name())
}

// cargo's palette for its help. rustup uses it too, as clap-cargo's.
// https://github.com/rust-lang/cargo/blob/111443f3dc4d82a65e2306a93028f122406401c1/crates/cargo-util-terminal/src/style.rs#L4-L7
// https://github.com/rust-lang/cargo/blob/111443f3dc4d82a65e2306a93028f122406401c1/src/bin/cargo/cli.rs#L592-L601
// https://github.com/crate-ci/clap-cargo/blob/8c8b15fe2abd24034ae0d8015ba8ec38f3c40591/src/style.rs#L10-L13
// https://github.com/rust-lang/rustup/blob/08f60b0a9f445bf25153692139700a2cbc1a869b/src/cli/rustup_mode.rs#L88
const HEADER: Style = AnsiColor::BrightGreen.on_default().effects(Effects::BOLD);
const LITERAL: Style = AnsiColor::BrightCyan.on_default().effects(Effects::BOLD);
const PLACEHOLDER: Style = AnsiColor::Cyan.on_default();
const STYLES: Styles = Styles::styled()
    .header(HEADER)
    .usage(HEADER)
    .literal(LITERAL)
    .placeholder(PLACEHOLDER);

/// Styles `cli` in cargo's palette. Gives it the flags that `parse` answers, `-h`, `--help`, `-V`
/// and `--version`, in place of clap's own.
// No command has a help flag of clap's, so a usage error ends without clap's pointer to it, and
// the command that prints its help is said instead.
fn flagged(cli: clap::Command) -> clap::Command {
    let help = Arg::new("help").short('h').long("help").global(true);
    let help = help.action(ArgAction::SetTrue).help("Print help");
    let version = Arg::new("version").short('V').long("version");
    let version = version.action(ArgAction::SetTrue).help("Print version");
    cli.styles(STYLES)
        .arg(version)
        .arg(help.display_order(usize::MAX))
}

fn command() -> clap::Command {
    let cli = flagged(Parsed::command());
    let rare: Vec<(String, String)> = (cli.get_subcommands())
        .filter(|command| RARE.contains(&command.get_name()))
        .map(|command| {
            let about = command.get_about().map(ToString::to_string);
            (command.get_name().to_owned(), about.unwrap_or_default())
        })
        .collect();
    let rare: Vec<(&str, &str)> = (rare.iter())
        .map(|(name, about)| (name.as_str(), about.as_str()))
        .collect();
    let topics = [("config", "The machine's configuration, pumpkin.toml")];
    let lists = [
        section("Rarely needed", &rare, Layout::Columns),
        section("Topics", &topics, Layout::Columns),
    ];
    noted(cli.after_help(lists.join("\n\n")), NOTES)
}

/// `cli` with each of `notes` ending the help of the command that it names.
fn noted(mut cli: clap::Command, notes: &[Notes]) -> clap::Command {
    for notes in notes {
        ended(&mut cli, notes.command, &notes.render());
    }
    cli
}

/// Ends the help of the subcommand of `command` that `path` names, which it has, with `text`,
/// below what already ends it.
fn ended(command: &mut clap::Command, path: &[&str], text: &str) {
    match path {
        [] => {
            // The styles of what already ends the help stay as they are.
            let above = command.get_after_help();
            let above = above.map_or_else(String::new, |above| format!("{}\n\n", above.ansi()));
            *command = mem::take(command).after_help(format!("{above}{text}"));
        }
        [name, rest @ ..] => {
            if let Some(command) = command.find_subcommand_mut(name) {
                ended(command, rest, text);
            }
        }
    }
}

fn help(cli: &mut clap::Command, name: Option<&str>, action: Option<&str>) -> StyledStr {
    cli.build();
    match name.and_then(|name| cli.find_subcommand_mut(name)) {
        Some(command) => match action.and_then(|action| command.find_subcommand_mut(action)) {
            Some(action) => action.render_long_help(),
            None => command.render_long_help(),
        },
        None => cli.render_long_help(),
    }
}
