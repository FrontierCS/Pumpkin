//! `pumpkin`'s command line: [`parse`] reads it with clap into a [`Command`], each command a
//! struct of its operands and flags.
//!
//! The doc comments of the commands and their arguments are the help texts, as clap reads them:
//! the first line is the summary. They say what each command and option means, and the guides in
//! `docs/` say how the commands fit together. `-h` or `--help` before `--` prints the help of the
//! command that the line names, as `pumpkin help` does, which also prints the topics, such as
//! [`CONFIG`]. The commands that [`RARE`] names are hidden from
//! clap's list and listed apart.
//! What a command does is left to its own module.

mod notes;

use std::ffi::OsString;
use std::iter;
use std::os::unix::ffi::OsStrExt as _;
use std::path::PathBuf;

use aws::Region;
use clap::builder::styling::{AnsiColor, Effects, Style, Styles};
use clap::builder::{OsStringValueParser, StyledStr, TypedValueParser};
use clap::error::ErrorKind;
use clap::{
    Arg, ArgAction, ArgMatches, Args, CommandFactory as _, FromArgMatches, Parser, Subcommand,
};
use provider::RunTag;
use schema::ir::Sha256Digest;

use self::notes::{
    CONFIG, cancel_for_notes, cancel_notes, check_notes, controller_notes, fmt_notes, help_notes,
    ls_notes, reap_notes, run_notes, view_notes, wait_notes,
};
use crate::ending::Printed;
use crate::iam::{self, Principal};
use crate::member::Member;
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
    #[command(after_help = check_notes())]
    Check(Check),
    /// Format `.star` files.
    ///
    /// `fmt` rewrites each `.star` file in the one style of the workflow language. It formats the
    /// files it is given and the `.star` files below the directories it is given. Below a
    /// directory, it skips names that start with `.` and anything a symbolic link leads to. If a
    /// file can't be formatted, `fmt` says why and formats the others. It prints nothing on
    /// success.
    #[command(after_help = fmt_notes())]
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
    #[command(after_help = run_notes())]
    Run(Run),
    /// Wait for a run to end and say how it ended.
    ///
    /// While the run goes on, `wait` says each of its milestones on the standard error. Once the
    /// run has ended, `wait` says its summary and the command that runs it again. Ending `wait`
    /// doesn't stop the run, but `pumpkin cancel` does.
    ///
    /// With `[controller]` in the configuration and no `--runs`, `wait` follows the run on the
    /// controller. It then copies the record into the default runs directory.
    #[command(after_help = wait_notes())]
    Wait(Wait),
    /// List runs and their states.
    ///
    /// `ls` prints a line for each run, oldest first, then a line of totals. Without a prefix, it
    /// lists the runs that are live or ended within the last day, and the totals say how to list
    /// the others. A live run's line gives its phase and the bound on it. An ended run's line
    /// gives its state, how long it ran and its result's values. If a run can't be listed, `ls`
    /// says why and lists the others. With `[controller]` in the configuration and no `--runs`,
    /// `ls` lists the controller's runs beside this machine's copies.
    #[command(after_help = ls_notes())]
    Ls(Ls),
    /// Stop a run and remove what it left.
    ///
    /// `cancel` sends SIGTERM to a live run's driver and waits for the run to end. Ending `cancel`
    /// doesn't end the stop. `cancel` then removes what the run left, as `pumpkin reap` does, and
    /// says how the run ended. A run that has ended or is lost gets no signal.
    ///
    /// With `[controller]` in the configuration and no `--runs`, `cancel` stops the run on the
    /// controller. It then copies a started run's record here, as `pumpkin wait` does.
    #[command(after_help = cancel_notes())]
    Cancel(CancelRun),
    /// Serve the runs to this machine's browser.
    ///
    /// `view` prints the address to open and serves until a signal stops it. It listens on
    /// 127.0.0.1 alone and needs the token in the address. The page lists the runs newest first,
    /// each as `ls` says it. A run's page shows its summary, its sandboxes' lanes, who talks to
    /// whom and its log. The page changes a run only by a cancel, which runs `pumpkin cancel`.
    /// With `[controller]` in the configuration and no `--runs`, `view` serves the controller's
    /// runs.
    #[command(after_help = view_notes())]
    View(View),
    /// Print the help of a command or a topic.
    ///
    /// Without a command or a topic, `help` prints what `pumpkin --help` does.
    #[command(after_help = help_notes())]
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
    #[command(hide = true, after_help = reap_notes())]
    Reap(Reap),
    /// Run the controller's own commands.
    ///
    /// These actions run on the controller. `serve` answers a client's `ssh` link and starts each
    /// `job` in a unit of its own. The controller's boot runs `boot`. `policy` runs off the
    /// controller too. It prints the IAM policy of a principal of the controller's deployment.
    #[command(hide = true, after_help = controller_notes())]
    #[command(disable_help_subcommand = true, subcommand_value_name = "action")]
    #[command(subcommand_help_heading = "Actions")]
    Controller {
        #[command(subcommand)]
        action: Action,
    },
}

// Hidden from clap's list of commands, so `command` lists them apart.
const RARE: [&str; 2] = ["reap", "controller"];

/// The actions of `pumpkin controller`.
#[derive(Subcommand)]
enum Action {
    /// Run a client's `run` as a job.
    ///
    /// `job` makes the job's tree in its working directory, which is `~/trees/<repository>/<id>`.
    /// It then runs the command there.
    Job {
        /// The job's id, which its run takes.
        #[arg(value_name = "id", value_parser = |id: &str| RunTag::try_from(id.to_owned()))]
        id: RunTag,
        /// The controller's AWS region.
        #[arg(long, value_name = "region")]
        #[arg(value_parser = |region: &str| Region::try_from(region.to_owned()))]
        region: Region,
        /// The SSM parameter that holds the controller's settings.
        #[arg(long, value_name = "name")]
        #[arg(value_parser = utf8(", as every SSM parameter's name is: name one that is"))]
        parameter: String,
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
    /// Cancel a run for a client.
    ///
    /// The client then says how the run ended, from its copy of the record.
    #[command(after_help = cancel_for_notes())]
    Cancel(Cancel),
    /// Answer a client on the standard input and output.
    ///
    /// Each job that `serve` starts belongs to the member that the link acts for. `serve` refuses
    /// to cancel another member's job unless the client asks for any member's.
    Serve {
        /// The member that the controller's forced command names for the link (default: none).
        #[arg(long, value_name = "member")]
        #[arg(value_parser = |member: &str| Member::try_from(member.to_owned()))]
        member: Option<Member>,
    },
    /// Print the IAM policy of a principal of the controller's deployment.
    ///
    /// `policy` prints the policy as IAM's JSON. It holds each statement that this build's calls
    /// on AWS need as that principal, and nothing else. A value that only the deployment knows,
    /// such as its account, is a placeholder that Terraform's `templatefile` fills.
    #[command(after_help = iam::placeholders())]
    Policy {
        /// The principal whose policy to print.
        #[arg(value_name = "principal")]
        principal: Principal,
    },
}

/// What a command line asks for. `Job` and the variants after it are the controller's own side,
/// `pumpkin controller <action>`; `Print` holds help, the version or a policy, for the standard
/// output.
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
    CancelFor(Cancel),
    Serve(Option<Member>),
    Print(StyledStr),
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
    /// Match the run among every member's runs on the controller, not only this member's and
    /// those that no member started.
    #[arg(long)]
    pub(crate) any_member: bool,
}

#[derive(Args)]
pub(crate) struct Cancel {
    #[arg(value_name = "run", value_parser = run_name(), help = RUN_HELP)]
    pub(crate) run: RunName,
    #[arg(long, value_name = "dir", value_parser = utf8(RUNS_NAMED_NEXT), help = RUNS_HELP)]
    pub(crate) runs: Option<String>,
    #[arg(long, hide = true, value_name = "file", help = MODAL_CONFIG_HELP)]
    pub(crate) modal_config: Option<PathBuf>,
}

#[derive(Args)]
pub(crate) struct Reap {
    /// The directory of the runs to reap (default: `runs` in the repository's `.pumpkin`).
    #[arg(long, value_name = "dir", value_parser = utf8(RUNS_NAMED_NEXT))]
    pub(crate) runs: Option<String>,
    #[arg(long, hide = true, value_name = "file", help = MODAL_CONFIG_HELP)]
    pub(crate) modal_config: Option<PathBuf>,
}

// Hidden: only the controller target gives `--modal-config`, as a job's Modal configuration
// file goes with the job, and the client sends it again.
const MODAL_CONFIG_HELP: &str = "Modal's configuration file, in place of the one that the run's \
                                 configuration names";

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
    pub(crate) region: Region,
    pub(crate) parameter: String,
    pub(crate) tree: Sha256Digest,
    pub(crate) run: Run,
}

#[derive(Clone)]
/// A run named by its record's path, which has a `/`, or by its id or a prefix of it.
pub(crate) enum RunName {
    Record(String),
    Prefix(String),
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
/// `-h` or `--help` before `--`, and `-V` or `--version` before the command, are answered as
/// [`Command::Print`], not refused.
pub(crate) fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Command, Refusal> {
    let args: Vec<OsString> = args.into_iter().collect();
    let mut cli = command();
    // clap's error doesn't name the command it was found in; the first word does, as only `-h`,
    // `--help`, `-V` and `--version` may come before it.
    let first = args
        .iter()
        .position(|arg| !arg.as_bytes().starts_with(b"-"));
    let named = first.and_then(|at| cli.find_subcommand(&args[at]));
    let named = named.map(|command| command.get_name().to_owned());
    let action = (named.as_deref() == Some("controller"))
        .then(|| first.and_then(|at| args.get(at + 1)))
        .flatten()
        .and_then(|word| word.to_str())
        .filter(|word| !word.starts_with('-'))
        .map(str::to_owned);
    let mut options = args.iter().take_while(|arg| *arg != "--");
    if options.any(|arg| arg == "-h" || arg == "--help") {
        return Ok(Command::Print(help(
            &mut cli,
            named.as_deref(),
            action.as_deref(),
        )));
    }
    let before = &args[..first.unwrap_or(args.len())];
    if before.iter().any(|arg| arg == "-V" || arg == "--version") {
        let version = format!("pumpkin {}\n", runtime::BUILD);
        return Ok(Command::Print(version.into()));
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
            action: Action::Cancel(cancel),
        } => Command::CancelFor(cancel),
        Parsed::Controller {
            action: Action::Serve { member },
        } => Command::Serve(member),
        Parsed::Controller {
            action: Action::Policy { principal },
        } => Command::Print(iam::policy(principal).into()),
        Parsed::Controller {
            action:
                Action::Job {
                    id,
                    region,
                    parameter,
                    tree,
                    command,
                },
        } => Command::Job(Job {
            id,
            region,
            parameter,
            tree,
            run: job_run(cli, command)?,
        }),
        Parsed::Help { topic } => return help_of(cli, topic.as_deref()),
    })
}

fn job_run(cli: &mut clap::Command, command: Vec<OsString>) -> Result<Run, clap::Error> {
    // The job's command is a whole command line of its own, so it is parsed as one.
    match parse(command).map_err(|refusal| refusal.error)? {
        Command::Run(run) => Ok(run),
        Command::Wait(_)
        | Command::Ls(_)
        | Command::Cancel(_)
        | Command::Reap(_)
        | Command::Check(_)
        | Command::Fmt(_)
        | Command::View(_)
        | Command::Job(_)
        | Command::Boot
        | Command::CancelFor(_)
        | Command::Serve(_)
        | Command::Print(_) => {
            let problem = "the job's command isn't `run`: give one";
            let controller = cli.find_subcommand_mut("controller");
            Err(
                match controller.and_then(|it| it.find_subcommand_mut("job")) {
                    Some(job) => job.error(ErrorKind::InvalidValue, problem),
                    None => cli.error(ErrorKind::InvalidValue, problem),
                },
            )
        }
    }
}

fn help_of(cli: &mut clap::Command, topic: Option<&str>) -> Result<Command, clap::Error> {
    let Some(topic) = topic else {
        return Ok(Command::Print(help(cli, None, None)));
    };
    if topic == "config" {
        return Ok(Command::Print(CONFIG.into()));
    }
    let names: Vec<String> = (cli.get_subcommands())
        .filter(|command| listed(command))
        .map(|command| format!("`{}`", command.get_name()))
        .collect();
    if names.iter().any(|name| name.trim_matches('`') == topic) {
        return Ok(Command::Print(help(cli, Some(topic), None)));
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

fn command() -> clap::Command {
    // No command has a help flag of clap's, so a usage error ends without clap's pointer to it,
    // and `pumpkin help <command>` is said instead; `parse` answers `-h`, `--help`, `-V` and
    // `--version`.
    let help = Arg::new("help").short('h').long("help").global(true);
    let help = help.action(ArgAction::SetTrue).help("Print help");
    let version = Arg::new("version").short('V').long("version");
    let version = version.action(ArgAction::SetTrue).help("Print version");
    let cli = Parsed::command()
        .styles(STYLES)
        .arg(version)
        .arg(help.display_order(usize::MAX));
    let width = (cli.get_subcommands())
        .filter(|command| listed(command))
        .map(|command| command.get_name().len())
        .max()
        .unwrap_or_default();
    let rare: Vec<String> = (cli.get_subcommands())
        .filter(|command| RARE.contains(&command.get_name()))
        .map(|command| {
            let about = command.get_about().map(ToString::to_string);
            let name = command.get_name();
            // The padding is unstyled, as in clap's own list of commands.
            let pad = width - name.len();
            format!(
                "  {LITERAL}{name}{LITERAL:#}{:pad$}  {}\n",
                "",
                about.unwrap_or_default()
            )
        })
        .collect();
    let rare = rare.concat();
    cli.after_help(format!(
        "\
{HEADER}Rarely needed:{HEADER:#}
{rare}
{HEADER}Topics:{HEADER:#}
  {LITERAL}config{LITERAL:#}{:pad$}  The machine's configuration, pumpkin.toml

{HEADER}Exit status:{HEADER:#}
  {LITERAL}0{LITERAL:#}  Success.
  {LITERAL}1{LITERAL:#}  An error. A `pumpkin:` line on the standard error says why.
  {LITERAL}2{LITERAL:#}  A usage error.
  {LITERAL}3{LITERAL:#}  An outcome that the help of `wait`, `cancel` and `reap` says.

`pumpkin help <command>` prints a command's help, and `pumpkin help <topic>`
prints a topic. The guide to running workflows is docs/running.md:
https://github.com/FrontierCS/Pumpkin/blob/main/docs/running.md",
        "",
        pad = width - "config".len()
    ))
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
