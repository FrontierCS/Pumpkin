//! `pumpkin`'s command line, parsed once into what it asks for ([`Command`]) by clap: each
//! command is a struct whose fields are its operands and flags, which clap's parser, usage lines,
//! help and errors all read. `-h` or `--help` before `--` asks for the help of the command the
//! line names, which `pumpkin help` also prints, as it does the topics, such as [`CONFIG`]; `-V`
//! or `--version` before the command, for the build ([`runtime::BUILD`]). The commands that are
//! rarely needed ([`RARE`]) are listed apart.

use std::ffi::OsString;
use std::iter;
use std::os::unix::ffi::OsStrExt as _;

use clap::builder::{OsStringValueParser, TypedValueParser};
use clap::error::ErrorKind;
use clap::{Arg, ArgAction, ArgMatches, Args, CommandFactory as _, FromArgMatches, Parser};
use recipe::Stage;

use crate::star::Entry;

/// A command, as clap parses it.
#[derive(Parser)]
#[command(
    name = "pumpkin",
    about = "\
Run workflows: in each run, an agent works in a sandbox, and judges score what
it submits.",
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
    #[command(about = "Stop a run, and remove what it left")]
    #[command(long_about = "\
Stop a run, wait until it has ended, remove what it left, and print its
outcome.json.")]
    #[command(after_help = CANCEL_NOTES)]
    Cancel(Cancel),
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
}

/// The commands that are rarely needed, which `pumpkin --help` lists after the others. Each is
/// hidden from clap's list of commands.
const RARE: [&str; 2] = ["build", "reap"];

/// What a command line asks for.
pub(crate) enum Command {
    Run(Run),
    Wait(Wait),
    Cancel(Cancel),
    Reap(Reap),
    Check(Check),
    Fmt(Fmt),
    Build(Build),
    Lock(Lock),
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

/// What `pumpkin run` is asked for, as `pumpkin run --help` says. Its paths are UTF-8, so that
/// a run's record keeps them, and a command that names them names the same files.
#[derive(Args)]
pub(crate) struct Run {
    #[arg(value_name = "entry", value_parser = entry())]
    #[arg(help = "The task's entry file, as `pumpkin check` takes it.")]
    pub(crate) entry: Entry,
    #[command(flatten)]
    pub(crate) args: TaskArgs,
    #[arg(long, value_name = "dir", value_parser = utf8(RECORDED), help = "\
The directory runs are recorded in, each in <dir>/<id>.
Default: $HOME/.local/share/pumpkin/runs")]
    pub(crate) runs: Option<String>,
    #[arg(long, value_name = "file", value_parser = utf8(RECORDED), help = "\
The machine's configuration; see `pumpkin help config`.
Default: $HOME/.config/pumpkin/pumpkin.toml")]
    pub(crate) config: Option<String>,
    #[arg(long, value_name = "path", value_parser = utf8(RECORDED), help = "\
The static sandboxd that every sandbox runs, as `cargo xtask sandboxd`
builds it.
Default: the one beside this pumpkin, where `cargo xtask install` puts
one")]
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

/// The help of the `<run>` operand.
const RUN_HELP: &str = "\
The run: its id; a prefix of its id, or of the 8 hexadecimal digits that
end it, that no other run in --runs shares; or its record's path, which
has a `/`.";

/// The help of `--runs` for a command that takes a `<run>`.
const RUNS_HELP: &str = "\
The directory that <run> is looked up in.
Default: $HOME/.local/share/pumpkin/runs";

/// What `pumpkin wait` is asked for, as `pumpkin wait --help` says. Its paths are UTF-8, as
/// those of `cancel` and `reap` are, so that a command that names them names the same
/// files.
#[derive(Args)]
pub(crate) struct Wait {
    #[arg(value_name = "run", value_parser = run_name(), help = RUN_HELP)]
    pub(crate) run: RunName,
    #[arg(long, value_name = "dir", value_parser = utf8(RUNS_NAMED_NEXT), help = RUNS_HELP)]
    pub(crate) runs: Option<String>,
}

/// What `pumpkin cancel` is asked for, as `pumpkin cancel --help` says.
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
}

/// What `pumpkin reap` is asked for, as `pumpkin reap --help` says.
#[derive(Args)]
pub(crate) struct Reap {
    #[arg(long, value_name = "dir", value_parser = utf8(RUNS_NAMED_NEXT), help = "\
The directory of the runs to reap.
Default: $HOME/.local/share/pumpkin/runs")]
    pub(crate) runs: Option<String>,
}

/// What `pumpkin check` is asked for, as `pumpkin check --help` says.
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

/// What `pumpkin fmt` is asked for, as `pumpkin fmt --help` says.
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

/// The help of `--config` of a command that builds.
const BUILD_CONFIG: &str = "\
The machine's configuration, whose [docker] names the engine; see
`pumpkin help config`.
Default: $HOME/.config/pumpkin/pumpkin.toml";

/// What `pumpkin build` is asked for, as `pumpkin build --help` says.
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

/// What `pumpkin lock` is asked for, as `pumpkin lock --help` says.
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

/// A run, as a command's `<run>` names it.
#[derive(Clone)]
pub(crate) enum RunName {
    /// By its record's path, which has a `/`.
    Record(String),
    /// By its id, or a prefix of its id or of its random digits, among the runs in `--runs`.
    Prefix(String),
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
labelled pumpkin.run=<id>, and its socket directories in the record's sockets/,
which `cancel` removes too.";

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
Once the run has ended, `wait` says on the standard error how it ended, and
where its record is, then the command that runs it again, as `pumpkin run
--help` says, and of a lost run, the commands that remove what it left; any
other exit but 0 ends with a `pumpkin:` line that says why.
If the final judge returned no result, its standard error is shown first, or,
if it is long, named.

Ending `pumpkin wait` doesn't stop the run: `pumpkin cancel` does.";

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
provider that it ran on, as `pumpkin reap --help` says,
and prints its outcome.json, then says how the run ended, as `pumpkin wait`
does.";

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
so before the engine is reached and any image is built. An image is
built from a stage of its context, and tagged by the digest of the build's
inputs, in pumpkin/<its recipe's directory below the root>, or a harness's layer
in <its base's repository>/<harness>; one that the engine has under its tag
already is not built again. Images are built at once, as many as the CPUs of
this machine that `pumpkin` may run on, as Cargo's jobs are, even if the engine
is another machine's, each once those it is built on are built. All that each
build's `docker build` wrote is said at once when it ends, as `make
--output-sync` says a job's, then that it ended. A failed build is said at the
image() call that declares its image; once one has failed, no other starts but
those started with it, and each that runs is waited for, and said if it fails.";

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
     when its Docker context is gone, so what the run left there may remain.

Of each run whose driver has ended, `reap` deletes the sandboxes of the run that
remain on each provider that it ran on, such as one that a create given up at
its deadline made: on the Docker engine of the context that the record's
provider.json names, the containers labelled pumpkin.run=<id>. It removes the
run's socket directories, in the record's sockets/, and a lost run's record it
marks failed, as its driver ended without an outcome.json. It leaves a live run
to its driver: `pumpkin cancel` stops one.

Each sandbox that `reap` removes it notes in the record's reaps.jsonl, with the
time; and each that the record holds undeleted, but that its provider no longer
lists, it says, and notes there as gone.";

/// `pumpkin help config`, the machine's configuration.
const CONFIG: &str = "\
pumpkin.toml, the machine's configuration, which every `pumpkin run` and
`pumpkin build` reads: the file that --config names,
$HOME/.config/pumpkin/pumpkin.toml by default. It is TOML, with the tables below.
Every key is required, but those of the [upstreams.<name>] tables, and any
other key is refused.

Example:
  [docker]
  context = \"default\"

  [providers.local.docker]

  [upstreams.gateway]
  url = \"https://gateway.example/v1\"
  key_file = \"gateway-key\"
  models = [{ name = \"model-a\", upstream_name = \"provider/model-a\" }]

[docker], the machine's Docker engine:
  context       The context that reaches it, as `docker context ls` lists it.
  cpus          The CPUs that its sandboxes may be given, each run whole cores
                of its own, as the kernel lists CPUs, such as \"1-7,9-15\";
                absent, all that are online.

[providers.<name>.<kind>], for each provider that the machine's runs may place
roles on, as provider <name>, of kind <kind>, whose table holds that kind's own
settings:
  docker        Sandboxes on the engine of [docker]; it takes no settings. A
                provider of this kind is the configuration's one provider. Its
                one class, `container`, offers the CPUs that sandboxes may be
                given, the machine's memory, the flags that /proc/cpuinfo lists
                for every CPU, no CPU model and no GPU.

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
";

/// A usage error, as clap words it, and the command that prints the help of the command it was
/// found in.
pub(crate) struct Refusal {
    pub(crate) error: clap::Error,
    pub(crate) help: String,
}

/// What `args`, a command line without its program's name, asks for.
///
/// # Errors
///
/// Fails with the first usage problem of `args`, unless they ask for help or the version.
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
    Ok(match parsed {
        Parsed::Run(run) => Command::Run(run),
        Parsed::Wait(wait) => Command::Wait(wait),
        Parsed::Cancel(cancel) => Command::Cancel(cancel),
        Parsed::Reap(reap) => Command::Reap(reap),
        Parsed::Check(check) => Command::Check(check),
        Parsed::Fmt(fmt) => Command::Fmt(fmt),
        Parsed::Build(build) => Command::Build(build),
        Parsed::Lock(lock) => Command::Lock(lock),
        Parsed::Help { topic } => return help_of(&mut cli, topic.as_deref()).map_err(refusal),
    })
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

/// Whether a help lists `command`: those that clap's list doesn't hide, and the rare ones.
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
