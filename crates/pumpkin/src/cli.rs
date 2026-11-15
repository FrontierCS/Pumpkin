//! `pumpkin`'s command line, parsed once into what it asks for ([`Command`]) by clap: each
//! command is a struct whose fields are its operands and flags, which clap's parser, usage lines,
//! help and errors all read. `-h` or `--help` before `--` asks for the help of the command the
//! line names, which `pumpkin help` also prints, as it does the topics, such as [`CONFIG`]; `-V`
//! or `--version` before the command, for the build ([`runtime::BUILD`]). The commands that are
//! rarely needed ([`RARE`]) are listed apart, and the controller's own side is listed nowhere.

use std::ffi::OsString;
use std::iter;
use std::os::unix::ffi::OsStrExt as _;
use std::path::PathBuf;

use aws::Region;
use clap::builder::{OsStringValueParser, TypedValueParser};
use clap::error::ErrorKind;
use clap::{
    Arg, ArgAction, ArgMatches, Args, CommandFactory as _, FromArgMatches, Parser, Subcommand,
};
use provider::RunTag;
use recipe::Stage;
use schema::ir::Sha256Digest;

use crate::star::Entry;

/// A command, as clap parses it.
#[derive(Parser)]
#[command(
    name = "pumpkin",
    about = "\
Run workflows: in each run, an agent works in a sandbox, and judges score what
it submits. It keeps what it writes in the .pumpkin directory that it is in,
where a repository's ./pumpkin builds it: the runs' records, but where --runs
names another directory, and what its builds and providers keep.",
    disable_help_flag = true,
    disable_help_subcommand = true,
    disable_version_flag = true,
    subcommand_value_name = "command",
    subcommand_help_heading = "Commands"
)]
enum Parsed {
    #[command(about = "Check tasks' sources, building and running nothing")]
    #[command(long_about = "\
Check each task's source: evaluate its entry file, and the files that it loads,
with the args given, then check the recipe of each image that it declares, and
its workflow, then lint those files, and say every problem found, in all the
tasks.")]
    #[command(after_help = CHECK_NOTES)]
    #[command(mut_arg(ARG, |arg| arg.help(CHECK_ARG)))]
    Check(Check),
    #[command(about = "Format the workflow language's `.star` files")]
    #[command(long_about = "\
Format each `.star` file named, and each below a directory named, in place, in
the one style that the workflow language's files are written in.")]
    #[command(after_help = FMT_NOTES)]
    Fmt(Fmt),
    #[command(about = "Build a task's images, and start a run of it")]
    #[command(long_about = "\
Build a task as `pumpkin build` does, then start a run of its workflow, and print
the run's id once it has started.")]
    #[command(after_help = RUN_NOTES)]
    Run(Run),
    #[command(about = "Wait until a run has ended, and print its outcome")]
    #[command(long_about = "Wait until a run has ended, then print its outcome.json.")]
    #[command(after_help = WAIT_NOTES)]
    Wait(Wait),
    #[command(about = "List the runs and their states")]
    #[command(long_about = "\
List the runs in --runs, oldest first, a line for each, then a line of totals:
without a prefix, those that are live or ended within the last day, and the
line of totals says how many others there are, and how to list them. A live
run's line gives its phase, how long it has been in it, the bound at which
Pumpkin ends the phase, its task, and what it does; an ended run's, its state,
how long it ran, its score if it has one, and its task.")]
    #[command(after_help = LS_NOTES)]
    Ls(Ls),
    #[command(about = "Stop a run, and remove what it left")]
    #[command(long_about = "\
Stop a run, wait until it has ended, remove what it left, and print its
outcome.json.")]
    #[command(after_help = CANCEL_NOTES)]
    Cancel(Cancel),
    #[command(about = "Serve the runs' records to this machine's browser")]
    #[command(long_about = "\
Serve the runs in --runs, or a controller's, to a browser on this machine, until
it is stopped, and print the address to open: a page that lists the runs as
`ls` does, newest first, and shows a run's summary, its sandboxes' lanes, who
talks to whom, and its log, which it follows while the run is live.")]
    #[command(after_help = VIEW_NOTES)]
    View(View),
    #[command(about = "Write a recipe's fetch stage, which pins what a step fetches")]
    #[command(long_about = LOCK_ABOUT, after_help = LOCK_NOTES)]
    Lock(Lock),
    #[command(about = "Print the help of a command or a topic")]
    #[command(long_about = "\
Print the help of a command or a topic, or `pumpkin --help` without one.")]
    #[command(after_help = HELP_NOTES)]
    Help {
        #[arg(value_name = "topic")]
        #[arg(help = "A command, or a topic: `config`, the machine's configuration.")]
        topic: Option<String>,
    },
    #[command(
        hide = true,
        about = "Build a task's images, and print its workflow's IR"
    )]
    #[command(long_about = "\
Build a task: check its source as `pumpkin check` does, then build each image
that it declares on the machine's Docker engine, and print the IR of its
workflow, which names each image by its ID.")]
    #[command(after_help = BUILD_NOTES)]
    Build(Build),
    #[command(hide = true, about = "Remove what the runs that have ended left")]
    #[command(long_about = "\
Remove what each run in --runs that has ended, or is lost, left, and print the
id of each run that left something.")]
    #[command(after_help = REAP_NOTES)]
    Reap(Reap),
    #[command(
        hide = true,
        about = "The controller's own side of the controller target"
    )]
    #[command(long_about = "\
On the controller: `job` runs a job of the controller target, which `pumpkin
run` and `pumpkin build` start there in a unit of its own; `boot` reaps what lost
runs left, at the controller's boot; `follow` follows a run for `pumpkin wait`,
and `cancel` cancels one for `pumpkin cancel`, on a client, which says the run's
ending from its copy of the record; `view` answers, on its standard input and
output, `pumpkin view` on a client, which shows the controller's runs.")]
    #[command(after_help = CONTROLLER_NOTES)]
    #[command(disable_help_subcommand = true, subcommand_value_name = "action")]
    #[command(subcommand_help_heading = "Actions")]
    Controller {
        #[command(subcommand)]
        action: Action,
    },
}

/// The commands that are rarely needed, which `pumpkin --help` lists after the others. Each is
/// hidden from clap's list of commands, as the controller's side is from every list.
const RARE: [&str; 2] = ["build", "reap"];

/// The actions of `pumpkin controller`.
#[derive(Subcommand)]
enum Action {
    #[command(about = "Run a job of the controller target")]
    Job {
        #[arg(value_name = "id", value_parser = |id: &str| RunTag::try_from(id.to_owned()))]
        #[arg(help = "The job's id, which its run takes.")]
        id: RunTag,
        #[arg(long, value_name = "region")]
        #[arg(value_parser = |region: &str| Region::try_from(region.to_owned()))]
        #[arg(help = "The controller's AWS region.")]
        region: Region,
        #[arg(long, value_name = "name")]
        #[arg(value_parser = utf8(", as every SSM parameter's name is: name one that is"))]
        #[arg(help = "The SSM parameter that holds the controller's settings.")]
        parameter: String,
        #[arg(long, value_name = "sha256")]
        #[arg(value_parser = tree)]
        #[arg(help = "\
The SHA-256 of the manifest of the job's tree, in the controller's store, from
which the job makes the tree in its working directory, which is to be the tree's
directory, trees/<repository>/<id> in the home, that the job's start made.")]
        tree: Sha256Digest,
        #[arg(last = true, required = true, value_name = "command")]
        #[arg(help = "\
`run` or `build` and their arguments, but `--config`, which the job
writes.")]
        command: Vec<OsString>,
    },
    #[command(about = "Reap what lost runs left, at the controller's boot")]
    Boot,
    #[command(about = "Cancel a run for a client, which says its ending")]
    Cancel(Cancel),
    #[command(about = "Follow a run for a client, saying its milestones until it ends")]
    Follow {
        #[arg(value_name = "id", value_parser = |id: &str| RunTag::try_from(id.to_owned()))]
        #[arg(help = "The run's id.")]
        id: RunTag,
    },
    #[command(about = "Answer a client's view of the runs, on the standard input and output")]
    View,
}

/// What a command line asks for.
pub(crate) enum Command {
    Run(Run),
    Wait(Wait),
    Ls(Ls),
    Cancel(Cancel),
    Reap(Reap),
    Check(Check),
    Fmt(Fmt),
    Build(Build),
    Lock(Lock),
    View(View),
    /// `pumpkin controller job`, a job on the controller.
    Job(Job),
    /// `pumpkin controller boot`, the controller's boot.
    Boot,
    /// `pumpkin controller follow`, a client's follow of a run.
    Follow(RunTag),
    /// `pumpkin controller cancel`, a client's cancel of a run.
    CancelFor(Cancel),
    /// `pumpkin controller view`, a client's view of the runs.
    ViewFor,
    /// Help, or the version, for the standard output.
    Print(String),
}

/// The name of the flag of the commands that evaluate a task, `--arg` ([`TaskArgs`]).
const ARG: &str = "arg";

/// The help of `--arg` of `check`, which the other commands that take it point to.
const CHECK_ARG: &str = "\
The value of the arg that an arg() call declares as <name>, read as the
type that it declares. Each arg is given at most once, and one with no
default must be, for each task.";

/// The `--arg`s of a command that evaluates a task, each `--arg <name>=<value>`, read as
/// [`lang::Args::parse`] reads them.
pub(crate) struct TaskArgs(pub(crate) lang::Args);

impl Args for TaskArgs {
    fn augment_args(command: clap::Command) -> clap::Command {
        let arg = Arg::new(ARG).long(ARG).value_name("name>=<value");
        let arg = arg.action(ArgAction::Append).help(
            "\
The value of the arg that an arg() call declares as <name>, as
`pumpkin check` takes it.",
        );
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
            // The first, as clap says the first problem of a line alone.
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

/// A value of `pumpkin`'s, a path or a name, that must be UTF-8; if it isn't, the problem says
/// so, then `why`, such as `, as every id is: name an id`.
fn utf8(why: &'static str) -> impl TypedValueParser<Value = String> {
    OsStringValueParser::new().try_map(move |value| {
        value
            .into_string()
            .map_err(|_| format!("it isn't UTF-8{why}"))
    })
}

/// Why a path that `run` is given must be UTF-8, and what to do.
const RECORDED: &str = ", as a run's record keeps each path that `run` is given: name one that is";

/// Why a path that another command is given must be UTF-8, and what to do.
const NAMED_NEXT: &str = ", as the commands that `pumpkin` says to run next name it: name one \
                          that is";

/// Why the `--runs` of a command that reads runs must be UTF-8, and what to do.
const RUNS_NAMED_NEXT: &str = ", as the commands that `pumpkin` says to run next name it: name \
                               a directory with a UTF-8 path";

/// A task's entry, as `pumpkin check` takes it ([`Entry::parse`]).
fn entry() -> impl TypedValueParser<Value = Entry> {
    utf8(NAMED_NEXT).try_map(Entry::parse)
}

/// Its paths are UTF-8, so that a run's record keeps them, and a command that names them names the
/// same files.
#[derive(Args)]
pub(crate) struct Run {
    #[arg(value_name = "entry", value_parser = entry())]
    #[arg(help = "The task's entry file, as `pumpkin check` takes it.")]
    pub(crate) entry: Entry,
    #[command(flatten)]
    pub(crate) args: TaskArgs,
    #[arg(long, value_name = "dir", value_parser = utf8(RECORDED), help = "\
The directory runs are recorded in, each in <dir>/<id>.
Default: runs/ in the .pumpkin directory that this pumpkin is in")]
    pub(crate) runs: Option<String>,
    #[arg(long, value_name = "file", value_parser = utf8(RECORDED), help = "\
The machine's configuration; see `pumpkin help config`.
Default: $HOME/.config/pumpkin/pumpkin.toml")]
    pub(crate) config: Option<String>,
    #[arg(long, value_name = "path", value_parser = utf8(RECORDED), help = "\
The static sandboxd that every sandbox runs, as `cargo xtask sandboxd`
builds it, with the static ec2-boot beside it, which an ec2 provider's
instances boot with.
Default: the one beside this pumpkin, where `cargo xtask install` puts
both")]
    pub(crate) sandboxd: Option<String>,
    #[arg(
        long,
        help = "\
Follow the run to its end, as `pumpkin wait` does, and print its
outcome.json instead of its id."
    )]
    pub(crate) wait: bool,
}

impl Run {
    /// The path flags it was given, each named as on the command line, with its value, in the
    /// order its usage lists them.
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

const RUN_HELP: &str = "\
The run: its id; a prefix of its id, or of the 8 hexadecimal digits that
end it, that no other run in --runs shares; or its record's path, which
has a `/`.";

const RUNS_HELP: &str = "\
The directory that <run> is looked up in.
Default: runs/ in the .pumpkin directory that this pumpkin is in";

/// Its paths are UTF-8, as those of `ls`, `cancel` and `reap` are, so that a command that names
/// them names the same files.
#[derive(Args)]
pub(crate) struct Wait {
    #[arg(value_name = "run", value_parser = run_name(), help = RUN_HELP)]
    pub(crate) run: RunName,
    #[arg(long, value_name = "dir", value_parser = utf8(RUNS_NAMED_NEXT), help = RUNS_HELP)]
    pub(crate) runs: Option<String>,
}

/// What `pumpkin view` is asked for, as `pumpkin view --help` says.
#[derive(Args)]
pub(crate) struct View {
    #[arg(long, value_name = "dir", value_parser = utf8(RUNS_NAMED_NEXT), help = "\
The directory of the runs to serve.
Default: runs/ in the .pumpkin directory that this pumpkin is in")]
    pub(crate) runs: Option<String>,
    #[arg(
        long,
        value_name = "port",
        help = "\
The port to serve at, on 127.0.0.1.
Default: one that is free, which `view` says"
    )]
    pub(crate) port: Option<u16>,
    #[arg(
        long,
        value_name = "dir",
        conflicts_with = "port",
        help = "\
Serve nothing: write into <dir> the runs' data as files that a static
copy of the page reads, with the page, and a robots.txt that asks
search engines to stay out. Each file that holds a reply that the page
reads, as `pumpkin controller view` answers it, is deflated: a host
serves it with `Content-Encoding: deflate`. In each reply, a stand-in
replaces each sandbox's id, as `<sandbox of job-3>`; the home and user
of this machine and of the run's arguments, as `~` and `<user>`; and
each string of a provider's report but its Docker engine's and its
processors', as `<aws.account>`; a home, user or report's string only if
8 bytes or more. A file is written only if its bytes changed, one whose
reply can't change only once, and a run that had ended at the last
export is read no further. Says how many files it wrote.
Default: serve the runs"
    )]
    pub(crate) export: Option<PathBuf>,
}

#[derive(Args)]
pub(crate) struct Ls {
    #[arg(long, value_name = "dir", value_parser = utf8(RUNS_NAMED_NEXT), help = "\
The directory of the runs to list.
Default: runs/ in the .pumpkin directory that this pumpkin is in")]
    pub(crate) runs: Option<String>,
    /// The prefixes of the ids of the runs to list, or none for every run.
    #[arg(value_name = "prefix", value_parser = utf8(", as every id is: name a prefix of an id"))]
    #[arg(help = "\
A prefix of the ids of the runs to list, such as a day's or an hour's,
as ids start with the UTC time of their `pumpkin run`.
Default: every run")]
    pub(crate) prefixes: Vec<String>,
}

#[derive(Args)]
pub(crate) struct Cancel {
    #[arg(value_name = "run", value_parser = run_name(), help = RUN_HELP)]
    pub(crate) run: RunName,
    #[arg(long, value_name = "dir", value_parser = utf8(RUNS_NAMED_NEXT), help = RUNS_HELP)]
    pub(crate) runs: Option<String>,
    #[arg(
        long,
        help = "\
End the run's driver at once, by SIGKILL, rather than stop the run by
SIGTERM; the run is then lost, and recorded as failed. A create that the
driver had given up on may still make a sandbox after `cancel` has
ended, which `pumpkin reap` removes."
    )]
    pub(crate) force: bool,
    #[arg(long, hide = true, value_name = "file", help = MODAL_CONFIG_HELP)]
    pub(crate) modal_config: Option<PathBuf>,
}

#[derive(Args)]
pub(crate) struct Reap {
    #[arg(long, value_name = "dir", value_parser = utf8(RUNS_NAMED_NEXT), help = "\
The directory of the runs to reap.
Default: runs/ in the .pumpkin directory that this pumpkin is in")]
    pub(crate) runs: Option<String>,
    #[arg(long, hide = true, value_name = "file", help = MODAL_CONFIG_HELP)]
    pub(crate) modal_config: Option<PathBuf>,
}

/// The help of `--modal-config` of `cancel` and `reap`, which the controller target alone gives
/// them: a job's Modal configuration file goes with the job, and the client sends it again.
const MODAL_CONFIG_HELP: &str = "\
Modal's configuration file, read for each provider of kind modal in place of
the one that the configuration of the run names.";

#[derive(Args)]
pub(crate) struct Check {
    #[arg(value_name = "entry", required = true, value_parser = entry(), help = "\
A task's entry file, a .star file below the root of its repository: the
nearest directory, at or above the entry's, that holds PUMPKIN.star.
Each task is checked with the same args.")]
    pub(crate) entries: Vec<Entry>,
    #[command(flatten)]
    pub(crate) args: TaskArgs,
}

#[derive(Args)]
pub(crate) struct Fmt {
    /// The files and directories to format, as given; none for the working directory.
    #[arg(value_name = "path", value_parser = utf8(NAMED_NEXT), help = "\
A `.star` file, or a directory, each `.star` file below which is
formatted, but those whose name, or that of a directory on the way to
them, starts with `.`, and those that a symbolic link leads to.
Default: the working directory")]
    pub(crate) paths: Vec<String>,
    #[arg(
        long,
        help = "\
Change no file: say each one that isn't formatted, and fail if one
isn't."
    )]
    pub(crate) check: bool,
}

const BUILD_CONFIG: &str = "\
The machine's configuration, whose [docker] names the engine; see
`pumpkin help config`.
Default: $HOME/.config/pumpkin/pumpkin.toml";

#[derive(Args)]
pub(crate) struct Build {
    #[arg(value_name = "entry", value_parser = entry())]
    #[arg(help = "The task's entry file, as `pumpkin check` takes it.")]
    pub(crate) entry: Entry,
    #[command(flatten)]
    pub(crate) args: TaskArgs,
    #[arg(long, value_name = "file", value_parser = utf8(NAMED_NEXT), help = BUILD_CONFIG)]
    pub(crate) config: Option<String>,
}

#[derive(Args)]
pub(crate) struct Lock {
    #[arg(value_name = "entry", value_parser = entry())]
    #[arg(help = "The task's entry file, as `pumpkin check` takes it.")]
    pub(crate) entry: Entry,
    #[arg(value_name = "recipe", value_parser = utf8(": name one that is"), help = "\
The recipe, a file that an image() call of the task builds, by its path;
the lock rewrites it in place.")]
    pub(crate) recipe: String,
    #[arg(value_name = "stage", help = "The fetch stage's name.")]
    #[arg(value_parser = utf8(": name one that is").try_map(Stage::try_from))]
    pub(crate) stage: Stage,
    #[command(flatten)]
    pub(crate) args: TaskArgs,
    #[arg(long, value_name = "file", value_parser = utf8(NAMED_NEXT), help = BUILD_CONFIG)]
    pub(crate) config: Option<String>,
}

pub(crate) struct Job {
    pub(crate) id: RunTag,
    pub(crate) region: Region,
    pub(crate) parameter: String,
    pub(crate) tree: Sha256Digest,
    pub(crate) command: JobCommand,
}

/// The command that a job runs, as `pumpkin` takes it on its own.
pub(crate) enum JobCommand {
    Run(Run),
    Build(Build),
}

/// A run, as a command's `<run>` names it.
#[derive(Clone)]
pub(crate) enum RunName {
    /// By its record's path, which has a `/`.
    Record(String),
    /// By its id, or a prefix of its id or of its random digits, among the runs in `--runs`.
    Prefix(String),
}

/// The digest that `hex`, a `--tree`, names.
fn tree(hex: &str) -> Result<Sha256Digest, &'static str> {
    crate::manifest::digest(hex)
        .ok_or("it isn't 64 lowercase hexadecimal digits: give the SHA-256 of the tree's manifest")
}

/// The run that a `<run>` names, which is UTF-8, as every id is, and as the commands that
/// `pumpkin` says to run next name a record.
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

const CHECK_NOTES: &str = "\
Examples:
  pumpkin check tasks/bench/task.star --arg model=model-a
  pumpkin check tasks/bench/task.star --arg model=model-a --arg deadline=3600
  pumpkin check tasks/*/task.star --arg model=model-a

Exit status:
  0  The check found no problem.
  1  A task has problems, each said at its place; or another error. Every
     task is checked all the same.
  2  A usage error.
Every exit but 0 ends with a `pumpkin:` line on the standard error, which says
why, and an exit of 1 then with the command that checks again the tasks that
failed, their entries' paths made absolute. On success, `check` prints nothing.

A problem is said at its file's path below the root, its line and its column,
then its source line, with carets under what it is about; a recipe's, at the
image() call that declares it. Past 20 problems, the rest are counted.

The lint, once a task has no other problem, finds in the files that it read
mistakes that evaluating them doesn't, as they may lie on paths that the args
given don't run: a name loaded but never used; a name bound but never used, as
a function's local, or a function or a global whose name starts with `_`; an
expression whose value is never used; a name used where a path to the use may
not have bound it; a statement never reached; a function that returns a value
on some paths but not on others; and what the language refuses once run, as
iterating over a string. It holds each file to one convention too: a docstring
for the file and for each public function, which names its parameters under
`Args:` and says what it returns under `Returns:`; and names in
lower_snake_case, constants in UPPER_SNAKE_CASE, and none of `l`, `I` or `O`.
It runs the formatter beside `pumpkin`, as `pumpkin fmt` does.

`check` reads only files below the root, with no symbolic link on the way to
them, and PUMPKIN.star holds only comments. It reaches no Docker engine, no
network and no configuration, so an image's build may still fail, as on a
recipe's step that fails.";

const FMT_NOTES: &str = "\
Examples:
  pumpkin fmt
  pumpkin fmt --check lib tasks/bench/task.star

Exit status:
  0  Every file is formatted; with --check, every file was already.
  1  With --check, a file isn't formatted; or a file can't be formatted, as it
     isn't valid Starlark; or another error. Every file is formatted, or
     checked, all the same.
  2  A usage error.
Every exit but 0 ends with a `pumpkin:` line on the standard error, which says
why, and an exit of 1 then with the command that formats the files that
failed, their paths made absolute. On success, `fmt` prints nothing.

`fmt` runs the formatter beside `pumpkin`, where `cargo xtask install` puts it,
which reads no configuration, so that every `.star` file is written alike.";

const RUN_NOTES: &str = "\
Examples:
  pumpkin run tasks/bench/task.star --arg model=model-a
  pumpkin run tasks/bench/task.star --arg model=model-a --wait > outcome.json

Exit status:
  0  The run has started.
  1  An error; a run that failed to start prints its outcome.json first.
  2  A usage error.
With --wait, `run` exits as `pumpkin wait` does once the run has started. Every
exit but 0 ends with a `pumpkin:` line on the standard error, which says why.

Every ending of a run ends with the command that runs it again: `pumpkin run`
of the task's entry, with the args and the flags that started it, their paths
made absolute. It builds the task as its source is then. A run takes no seed:
its id's random digits, and the grants by which its sandboxes reach it, are
fresh in every run, so that no two runs share them.

The task is checked, and the configuration read, before the machine's Docker
engine is reached, warning if that engine runs as root; the task's images are
built there; and then each provider that the configuration names is reached,
before anything starts. The run places each role in the smallest class of
sandbox that offers what the role needs, of every provider's, and refuses a
role that fits none, saying why of each class, before anything is reserved.
A driver process that `run` starts does all of that, and drives the run, and
writes what it says after the start to driver.log in the record. Whatever ends
`run` before the start ends the driver too; after it, the run goes on without
`run`, and `pumpkin wait` follows it.

On the Docker engine, each run holds whole cores of its own until it ends, which
no other run's containers are given, each role as many as hold its CPU limit
rounded up. A run whose roles need more cores at once than other runs leave is
refused before its record is made: run it again once they have ended.

The run outlives a logout unless its engine stops then, as rootless Docker does
without `loginctl enable-linger`, or logind kills the session's processes
(KillUserProcesses): then only a run under the user's manager outlives it, so
enable lingering and start `run` under `systemd-run --user --scope`.

`pumpkin cancel` stops the run by SIGTERM to the driver, whose pid is in the
record's driver.pid, as SIGINT and SIGHUP do too: the run removes its
sandboxes, and its outcome.json says which signal interrupted it. A signal
ignored as `run` starts stays ignored. SIGKILL to the driver leaves the run
lost, with its sandboxes, which `pumpkin cancel` removes: its containers,
labelled pumpkin.run=<id>, its EC2 instances, tagged pumpkin:run=<id>, and its
Modal sandboxes, tagged pumpkin-run=<id>, which end at the run's hard end all
the same; and its socket directories in the record's sockets/, which `cancel`
removes too.

On a machine whose configuration names [controller], as `pumpkin help config`
says, `run` takes neither --runs nor --sandboxd: it says the run's id at once,
starts the controller if it is stopped, sends there the files of the task's
repository that git lists and the task reaches, and starts the run's job, which
builds the task, then runs it there, with the id; it then returns, or with
--wait follows the job.";

const WAIT_NOTES: &str = "\
Examples:
  pumpkin wait 20261108-000905-126a21e9
  pumpkin wait 126a
  pumpkin wait runs/20261108-000905-126a21e9

Exit status:
  0  The run finished with a score.
  3  The run finished without one.
  1  The run failed, was interrupted, or was lost as its driver ended without
     an outcome.json, which `pumpkin cancel` then records; or another error.
  2  A usage error.
While the run goes on, `wait` says on the standard error each of its milestones
as its record logs it: the agent's start, each judge call's start and end, each
lease's start, its runner's readiness and its end, and the end of the work
phase. Once the run has ended, it says its summary: when it started, how long it
ran, the agent's start and end, each judge call with how long it took and how it
ended, each lease with how long its runner lived and was charged for, and how
much of that a command or transfer ran, its commands and how it ended, the
model requests of an agent with a model route and the tokens reported, and the
sandboxes, by provider and class, with how long they lasted and how many were
deleted; then how it ended, and where its record
is, then the command that runs it again, as `pumpkin run --help` says, and of a
lost run, the commands that remove what it left; any other exit but 0 ends with
a `pumpkin:` line that says why.
If the final judge returned no result, its standard error is shown first, or,
if it is long, named.

Ending `pumpkin wait` doesn't stop the run: `pumpkin cancel` does.

With [controller] in the configuration, and no --runs, `wait` follows the run's
job on the controller, its build included, then the run, and copies its record
into the default runs directory once it has ended; a run whose copy there has
ended needs no controller.";

const LS_NOTES: &str = "\
Examples:
  pumpkin ls
  pumpkin ls 20261110-1
  pumpkin ls --runs runs

Exit status:
  0  The runs are listed.
  1  A run can't be listed, which `ls` says, or another error; it lists the
     others all the same.
  2  A usage error.

A live run's phase is `publishing` what its sandboxes run, `starting` its agent,
`agent` at work, `ending` the work phase, or `final judge`; under it, a line for
each judge call the agent made, by its grant, and each lease, by its grant and
state, that hasn't ended; a phase past its bound is a bug, which `ls` says. An
ended run's state is `finished`, `failed`, `interrupted` (by a signal), or
`lost`, if its driver ended without an outcome.json. Its task is its workflow's
name, or `-` if its record has no ir.json yet. A directory of --runs that is not
there yet is no runs, if it is the default one.

With [controller] in the configuration, and no --runs, `ls` lists the
controller's runs, a job yet to start its run as `waiting` while it waits for a
build slot, else as `building`, or as `unstarted` once it has ended without one,
beside the copies in the default runs directory; and says first how long the
controller has been up, its jobs, how full its disk is, and whether its boot
failed.";

const CANCEL_NOTES: &str = "\
Examples:
  pumpkin cancel 126a
  pumpkin cancel --force 126a

Exit status:
  0  The run has ended, and nothing that it left remains.
  1  An error.
  2  A usage error.
  3  The run has ended, but a provider of it can't be reached, so what it left
     there may remain.

A live run's driver, whose pid is in the record's driver.pid, gets SIGTERM, and
the run stops as `pumpkin run --help` says, which `cancel` waits for; ending
`pumpkin cancel` doesn't end the stop. A run that has ended, or is lost, gets no
signal. Then `cancel` removes what `pumpkin reap` would of the run, on each
provider that it ran on, Docker, EC2 or Modal, as `pumpkin reap --help` says,
and prints its outcome.json, then says how the run ended, as `pumpkin wait`
does.

With [controller] in the configuration, and no --runs, `cancel` stops the job
of a run that is yet to start on the controller, and cancels one that has
started there, sending this machine's Modal configuration file for the reap,
then copies its record here, as `pumpkin wait` does.";

const VIEW_NOTES: &str = "\
Examples:
  pumpkin view
  pumpkin view --runs runs --port 8080

Exit status:
  1  The port can't be bound, or another error.
  2  A usage error.
`view` serves until a signal stops it, as Ctrl-C's SIGINT or SIGTERM does, so it
doesn't exit 0.

`view` prints on the standard error the address to open, such as
`http://127.0.0.1:41873/?t=0123...`. It listens on 127.0.0.1 alone, so no other
machine reaches it, and answers a request only if it names this machine as its
host, and, for the runs' data, carries the token after `?t=`, which the page
sends: another user of this machine can't read the runs, nor can a page of
another site that the browser has open. It reads the records as `ls` and `wait`
do, and asks nothing of a provider.

A run's page links to the trace of each caller that used a model, the agent or
a judge call: its model requests as threads of steps, each with the entries it
added and the model's answer, as the record's log and the bodies it keeps say.

The page's colours follow the browser's light or dark setting; the choice in its
header overrides that with light or dark, and the browser keeps the choice.
`&scheme=light`, `&scheme=dark` or `&scheme=system` after the token picks one in
the address.

The page's cancel of a run runs `pumpkin cancel --runs <dir> <run>`, or of a
controller's run `pumpkin cancel <run>`, and shows what that says. Nothing else
the page does changes a run.

With [controller] in the configuration, and no --runs, `view` serves the
controller's runs that have started, and reads them there: it sends each of the
page's requests for the runs' data over one `ssh` to `pumpkin controller view`
on the controller, of the `pumpkin` of this one's commit, which a `pumpkin run`
or `pumpkin build` of a task that pins the commit builds there, and refuses to
start without it. It reaches the controller as it starts, starting it if it is
stopped; once the connection has ended, the next request opens it again, and
while that fails the page says so and asks again after 5 s, then after twice
the wait before, up to a minute. --runs serves a directory here, such as the
copies of the controller's runs that `wait` makes.";

const LOCK_ABOUT: &str = "\
Write the fetch stage <stage> of the recipe <recipe> of a task afresh, from its
lock line, `# pumpkin lock: <resolver> <argument>...`, in the comment right
above its line `FROM scratch AS <stage>`, up to the blank line that ends it: an
`ADD` for each file that the resolver prints, pinned by its SHA-256. The
resolver runs online, in the image of the recipe up to the one later `RUN` that
mounts the stage, built on the machine's Docker engine with the inputs that the
task's images give the recipe: its context, mounts, args and named contexts,
which are built first. `apt <package>...` pins what apt fetches to install the
packages, and the archive's indexes that apt reads, so that the stage is a
mirror of that part of the archive.";

const LOCK_NOTES: &str = "\
Examples:
  pumpkin lock tasks/bench/task.star tasks/bench/verifier/Dockerfile apt
  pumpkin lock tasks/bench/task.star tasks/bench/agent/Dockerfile apt --arg model=m

Exit status:
  0  The stage is written.
  1  The task has problems; its images give the recipe different inputs; the
     recipe has no such fetch stage; or a build, the resolver or the write
     failed; or another error.
  2  A usage error.
Every exit but 0 ends with a `pumpkin:` line on the standard error, which says
why. The task is checked as `pumpkin check` checks it, and the stage read,
before the configuration is read and the engine reached. The builds' output
goes to the standard error. The image the resolver runs in is tagged in
pumpkin/lock, where a later lock finds it.";

const HELP_NOTES: &str = "\
Examples:
  pumpkin help run
  pumpkin help config

`pumpkin help <command>` prints what `pumpkin <command> --help` does.";

const BUILD_NOTES: &str = "\
Examples:
  pumpkin build tasks/bench/task.star --arg model=model-a > workflow.json
  pumpkin build tasks/bench/task.star --arg model=model-a --config pumpkin.toml

Exit status:
  0  The images are built, and the IR printed.
  1  The task has problems, or an image's build failed, each said at its place;
     or another error.
  2  A usage error.
Every exit but 0 ends with a `pumpkin:` line on the standard error, which says
why, and an exit of 1 then with the command that builds the task again, its
paths made absolute. The standard output holds only the IR, as a run's record
keeps it in ir.json; the builds' output goes to the standard error.

Every problem that `check` finds is said before the configuration is read, and
so before the engine is reached and any image is built. An image is built from
a stage of its context, and tagged by the digest of the build's inputs, in
pumpkin/<its recipe's directory below the root>, or, for a recipe of the
standard library, in pumpkin-library/<its directory below @pumpkin//>; one that
the engine has under its tag already is not built again. Images are built at
once, as many as the CPUs of this machine that `pumpkin` may run on, as Cargo's
jobs are, even if the engine is another machine's, each once those it is built
on are built. All that each build's `docker build` wrote is said at once when it
ends, as `make --output-sync` says a job's, then that it ended. A failed build
is said at the image() call that declares its image; once one has failed, no
other starts but those started with it, and each that runs is waited for, and
said if it fails.

With [controller] in the configuration, `build` sends the task as `pumpkin run`
does, follows the build's job on the controller, and prints the IR it printed.
A job there, of `run` or `build`, builds once it takes one of the controller's
build slots, one for every two of its CPUs, waiting, if each is taken, until one
is free; and, once a prune has removed what images no job holds, refuses to
build while more than 85% of the controller's volume is used.";

const REAP_NOTES: &str = "\
Examples:
  pumpkin reap
  pumpkin reap --runs runs

Exit status:
  0  Nothing that a run which has ended left remains.
  1  A run can't be reaped, which `reap` says, or another error; it reaps the
     others all the same.
  2  A usage error.
  3  Only that a provider of a run, which `reap` names, can't be reached, as
     when its Docker context is gone, or the configuration that ran the run no
     longer names it, so what the run left there may remain.

Of each run whose driver has ended, `reap` deletes the sandboxes of the run that
remain on each provider that it ran on, such as one that a create given up at
its deadline made: on the Docker engine of the context that the record's
provider.json names, the containers labelled pumpkin.run=<id>; and on each
provider of kind ec2 or modal, as the configuration that ran the run names it,
the one its record.json's flags name or else the default, the EC2 instances
tagged pumpkin:run=<id>, or the Modal sandboxes tagged pumpkin-run=<id>. It
removes the run's socket directories, in the record's sockets/, and a lost run's
record it marks failed, as its driver ended without an outcome.json. It leaves a
live run to its driver: `pumpkin cancel` stops one.

Each sandbox that `reap` removes it notes in the record's reaps.jsonl, with the
time; and each that the record holds undeleted, but that its provider no longer
lists, as when the configuration now points to another region or profile, it
says, and notes there as gone.

With [controller] in the configuration, and no --runs, `reap` reaps the
controller's runs there, as the controller does at each boot, but with this
machine's Modal configuration file, which the boot lacks.";

const CONTROLLER_NOTES: &str = "\
Examples:
  cd ~/trees/tasks/20261108-000905-126a21e9 && pumpkin controller job 20261108-000905-126a21e9 --region us-west-1 --parameter /pumpkin-alice/settings --tree 5d0c5f0e83a3c40be3e7f2b4d1f9a2b6c8e7d6a5b4c3d2e1f0a9b8c7d6e5f4a3 -- run tasks/bench/task.star --arg model=model-a
  pumpkin controller boot

Exit status:
  0  The job's command, or the boot, has ended as it should.
  1  An error, which a `pumpkin:` line on the standard error says.
  2  A usage error.";

/// `pumpkin help config`, the machine's configuration.
const CONFIG: &str = "\
pumpkin.toml, the machine's configuration, which every `pumpkin run` and
`pumpkin build` reads: the file that --config names,
$HOME/.config/pumpkin/pumpkin.toml by default. It is TOML, with the tables below.
Every key is required, but those of [gateway] and the [upstreams.<name>] tables,
and any other key is refused. [controller] takes the place of [docker],
[providers.<name>.<kind>] and [gateway] on a machine whose runs go to the
person's controller.

Example, of a machine whose runs go to EC2 and Modal:
  [docker]
  context = \"default\"

  [providers.aws.ec2]
  region = \"us-west-1\"
  repositories = { roles = \"pumpkin/roles\", sandboxd = \"pumpkin/sandboxd\" }

  [providers.aws.ec2.timer]
  group = \"pumpkin\"
  role_arn = \"arn:aws:iam::123456789012:role/pumpkin-timer\"

  [providers.aws.ec2.classes.judge]
  template = \"lt-0123456789abcdef0\"
  version = 1
  vcpus = 8
  memory_mib = 16384
  cpu_features = [\"avx2\"]

  [providers.gpus.modal]
  profile = \"pumpkin\"
  dials_from = \"203.0.113.7\"

  [providers.gpus.modal.images]
  region = \"us-west-1\"
  repository = \"pumpkin/roles\"
  role_arn = \"arn:aws:iam::123456789012:role/pumpkin-modal-pull\"

  [providers.gpus.modal.classes.a100]
  gpu = \"A100-80GB\"
  gpu_model = \"NVIDIA A100-SXM4-80GB\"
  cpu_millis = 8000
  memory_bytes = 68719476736
  startup_seconds = 1800

  [gateway]
  private = \"172.31.0.10:7433\"

  [upstreams.gateway]
  url = \"https://gateway.example/v1\"
  key_file = \"gateway-key\"
  models = [{ name = \"model-a\", upstream_name = \"provider/model-a\" }]

Example, of a machine whose runs go to the person's controller, and Modal:
  [controller]
  name = \"alice\"
  region = \"us-west-1\"

  [controller.modal]
  profile = \"pumpkin\"

  [controller.modal.classes.a100]
  gpu = \"A100-80GB\"
  gpu_model = \"NVIDIA A100-SXM4-80GB\"
  cpu_millis = 8000
  memory_bytes = 68719476736
  startup_seconds = 1800

[docker], the machine's Docker engine:
  context       The context that reaches it, as `docker context ls` lists it.
  cpus          The CPUs that its sandboxes may be given, each run whole cores
                of its own, as the kernel lists CPUs, such as \"1-7,9-15\";
                absent, all that are online.

[providers.<name>.<kind>], for each provider that the machine's runs may place
roles on, as provider <name>, of kind <kind>, whose table holds that kind's own
settings:
  docker        Sandboxes on the engine of [docker]; it takes no settings. A
                provider of this kind is the configuration's one provider, as
                a machine runs its roles either on its own engine or on others,
                of kind ec2 and modal. Its one class, `container`, offers the
                CPUs that sandboxes may be given, the machine's memory, the
                flags that /proc/cpuinfo lists for every CPU, no CPU model and
                no GPU.
  ec2           Sandboxes on EC2 instances of their own, in the envelope that
                its settings name, which pull their images from it, pushed
                there from the engine of [docker]; they dial [gateway]'s
                private address. It runs `aws`, AWS CLI v2, from PATH, with the
                credentials that `aws` finds, such as the instance's role.
                Its settings:
    region        The envelope's AWS region, which every call names.
    classes       Each class of sandbox, by its name; one at least: its
                  `template`, the ID of its launch template, and `version`, the
                  template's version that its instances launch from; `vcpus`
                  and `memory_mib`, its instance type's CPUs and memory, in
                  MiB, of which a role gets all the CPUs and all the memory but
                  a share the instance keeps; and `cpu_features`, its CPU's
                  flags, and `cpu_model`, its CPU's model, as /proc/cpuinfo
                  names them: without `cpu_features`, the class states no
                  flag, and without `cpu_model`, no model, and a class that
                  states a model is offered only to roles that need it; and
                  `usd_per_hour`, what an instance costs an hour, if given,
                  by which `ls` and a run's summary price its sandboxes.
    repositories  The envelope's ECR repositories: `roles`, of the roles'
                  images, and `sandboxd`, of the images that hold sandboxd.
    timer         The sandboxes' timer, which terminates each instance at its
                  run's hard end: `group`, its EventBridge Scheduler schedule
                  group, and `role_arn`, the ARN of the role that its schedules
                  terminate instances with.
  modal         Sandboxes in a Modal workspace, which run images that Modal
                imports from an ECR repository, pushed there from the engine of
                [docker]; each listens behind Modal's TLS tunnel, which the run
                dials. It runs Modal's helper, pumpkin-modal.py, beside
                `pumpkin`, with `uv` from PATH, and `aws`, AWS CLI v2, as ec2
                does. Its settings:
    profile       The profile of Modal's configuration whose token reaches the
                  workspace.
    config        The file of Modal's configuration that holds the profile, by
                  its absolute path; absent, ~/.modal.toml.
    dials_from    This machine's public IPv4 address, from which alone the
                  sandboxes' tunnels take connections.
    images        Where Modal imports images from: `region`, `repository`,
                  an ECR repository there, and `role_arn`, the ARN of the AWS
                  role that Modal pulls with, which trusts Modal's identity.
    classes       Each class of sandbox, by its name; one at least: its
                  `cpu_millis`, `memory_bytes`, in whole MiB, and
                  `startup_seconds`, and its `gpu`, if any, as Modal names
                  one, such as \"H100!\", or \"A100-80GB:2\" for two, with
                  `gpu_model`, its model as nvidia-smi names it, such as
                  \"NVIDIA H100 80GB HBM3\". A class with a GPU is offered
                  only to roles that need its GPUs. With `usd_per_hour`, what
                  a sandbox costs an hour, `ls` and a run's summary price its
                  sandboxes.

[gateway], where the gateway meets the sandboxes that dial it over a network:
  private       The address, ip:port, that the sandboxes of an ec2 provider
                dial: one of this machine's on the envelope's network, at a
                port that its security group admits. A run listens there while
                it places a role on such a provider, so one such run at a time.

[upstreams.<name>], for each upstream that a workflow's model route may reach:
  url           The base URL of its API: https, or http to a loopback IP
                address, such as 127.0.0.1 but not localhost. The route reaches
                each API's endpoint below it: /responses, or /chat/completions.
  key_file      The file that holds its key, below this file's directory unless
                absolute.
  models        The models it serves, each with the `name` that a route allows
                and the `upstream_name` that the upstream takes for it. No model
                is listed twice, under one upstream or two.

Each entry of a workflow's model route reaches the one upstream that serves
every model it allows.

[controller], the person's controller, which deploy/aws makes, and to which
`run`, `wait`, `ls`, `cancel`, `build` and `reap` go, as each one's help says.
Each run is sent the [upstreams.<name>] tables, their keys, [controller.modal]
and Modal's configuration file, into a directory of its own in the
controller's tmpfs, which its end removes. They run `aws`, AWS CLI v2, with the
Session Manager plugin, and `ssh`, `tar`, `ssh-keygen`
and `git`, from PATH; the controller's settings are the module's, but for
[controller.modal]. This machine's key to it, which the first command makes
and enrols through EC2 Instance Connect, and the host key it pins, are in
$HOME/.config/pumpkin/ssh/.
  name          The `name` that deploy/aws was applied with.
  region        The region it was applied in.

[controller.modal], the Modal provider that the controller's runs may place
roles on, if any: the settings of a provider of kind modal, but `images`, which
the module's settings give, and `dials_from`, which each run sets:
  profile       As modal's.
  config        Modal's configuration file, below this file's directory unless
                absolute; absent, ~/.modal.toml, if it is there.
  classes       As modal's.
";

/// A usage error, as clap words it, and the command that prints the help of the command it was
/// found in.
pub(crate) struct Refusal {
    pub(crate) error: clap::Error,
    pub(crate) help: String,
}

/// What `args`, a command line without its program's name, asks for. Fails with the first usage
/// problem of `args`, unless they ask for help or the version.
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
    let mut options = args.iter().take_while(|arg| *arg != "--");
    if options.any(|arg| arg == "-h" || arg == "--help") {
        return Ok(Command::Print(help(&mut cli, named.as_deref())));
    }
    let before = &args[..first.unwrap_or(args.len())];
    if before.iter().any(|arg| arg == "-V" || arg == "--version") {
        return Ok(Command::Print(format!("pumpkin {}\n", runtime::BUILD)));
    }
    let refusal = |error| Refusal {
        error,
        help: match &named {
            Some(name) => format!("pumpkin help {name}"),
            None => "pumpkin help".to_owned(),
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

/// The command that `parsed`, a line of `cli`, asks for. Fails with the usage problem of a job's
/// command or of a help's topic.
fn command_of(cli: &mut clap::Command, parsed: Parsed) -> Result<Command, clap::Error> {
    Ok(match parsed {
        Parsed::Run(run) => Command::Run(run),
        Parsed::Wait(wait) => Command::Wait(wait),
        Parsed::Ls(ls) => Command::Ls(ls),
        Parsed::Cancel(cancel) => Command::Cancel(cancel),
        Parsed::Reap(reap) => Command::Reap(reap),
        Parsed::Check(check) => Command::Check(check),
        Parsed::Fmt(fmt) => Command::Fmt(fmt),
        Parsed::Build(build) => Command::Build(build),
        Parsed::Lock(lock) => Command::Lock(lock),
        Parsed::View(view) => Command::View(view),
        Parsed::Controller {
            action: Action::Boot,
        } => Command::Boot,
        Parsed::Controller {
            action: Action::Follow { id },
        } => Command::Follow(id),
        Parsed::Controller {
            action: Action::Cancel(cancel),
        } => Command::CancelFor(cancel),
        Parsed::Controller {
            action: Action::View,
        } => Command::ViewFor,
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
            command: job_command(cli, command)?,
        }),
        Parsed::Help { topic } => return help_of(cli, topic.as_deref()),
    })
}

/// The command of a job, `command`, as `pumpkin` takes it on its own, if it is `run` or `build`.
/// Fails with the usage problem of `command`, or that it is neither.
fn job_command(cli: &mut clap::Command, command: Vec<OsString>) -> Result<JobCommand, clap::Error> {
    match parse(command).map_err(|refusal| refusal.error)? {
        Command::Run(run) => Ok(JobCommand::Run(run)),
        Command::Build(build) => Ok(JobCommand::Build(build)),
        Command::Wait(_)
        | Command::Ls(_)
        | Command::Cancel(_)
        | Command::Reap(_)
        | Command::Check(_)
        | Command::Fmt(_)
        | Command::Lock(_)
        | Command::View(_)
        | Command::Job(_)
        | Command::Boot
        | Command::Follow(_)
        | Command::CancelFor(_)
        | Command::ViewFor
        | Command::Print(_) => {
            let problem = "the job's command is neither `run` nor `build`: give one";
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

/// `pumpkin help <topic>`: a command's help, a topic, or `pumpkin --help` without one.
fn help_of(cli: &mut clap::Command, topic: Option<&str>) -> Result<Command, clap::Error> {
    let Some(topic) = topic else {
        return Ok(Command::Print(help(cli, None)));
    };
    if topic == "config" {
        return Ok(Command::Print(CONFIG.to_owned()));
    }
    let names: Vec<String> = (cli.get_subcommands())
        .filter(|command| listed(command))
        .map(|command| format!("`{}`", command.get_name()))
        .collect();
    if names.iter().any(|name| name.trim_matches('`') == topic) {
        return Ok(Command::Print(help(cli, Some(topic))));
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

/// Whether a help lists `command`: all but the controller's side.
fn listed(command: &clap::Command) -> bool {
    !command.is_hide_set() || RARE.contains(&command.get_name())
}

/// `pumpkin`, with `-h` and `--help` listed last in each command's help, and after its options
/// the commands that are rarely needed ([`RARE`]) and the topics.
fn command() -> clap::Command {
    // No command has a help flag of clap's, so a usage error ends without clap's pointer to
    // it, and `pumpkin help <command>` is said instead; `parse` answers `-h` and `--help`, and
    // `-V` and `--version`.
    let help = Arg::new("help").short('h').long("help").global(true);
    let help = help.action(ArgAction::SetTrue).help("Print this help.");
    let version = Arg::new("version").short('V').long("version");
    let version = version
        .action(ArgAction::SetTrue)
        .help("Print the version.");
    let cli = Parsed::command()
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
            format!(
                "  {:width$}  {}\n",
                command.get_name(),
                about.unwrap_or_default()
            )
        })
        .collect();
    let rare = rare.concat();
    cli.after_help(format!(
        "\
Rarely needed:
{rare}
Topics:
  {:width$}  The machine's configuration, pumpkin.toml

`pumpkin help <command>` prints a command's help, as `pumpkin <command> --help`
does, and `pumpkin help <topic>` a topic.",
        "config"
    ))
}

/// The help of the command of `cli` that `name` names, or of `pumpkin` if it names none.
fn help(cli: &mut clap::Command, name: Option<&str>) -> String {
    cli.build();
    let help = match name.and_then(|name| cli.find_subcommand_mut(name)) {
        Some(command) => command.render_long_help(),
        None => cli.render_long_help(),
    };
    help.to_string()
}
