//! `pumpkin`'s command line, parsed once into what it asks for ([`Command`]) by clap: each
//! command is a struct whose fields are its operands and flags, which clap's parser, usage lines,
//! help and errors all read. `-h` or `--help` before `--` asks for the help of the command the
//! line names, which `pumpkin help` also prints, as it does the topics, such as [`CONFIG`].

use std::ffi::OsString;
use std::iter;
use std::os::unix::ffi::OsStrExt as _;
use std::path::PathBuf;

use clap::builder::{OsStringValueParser, TypedValueParser as _};
use clap::error::ErrorKind;
use clap::{Arg, ArgAction, Args, CommandFactory as _, FromArgMatches as _, Parser};

/// A command, as clap parses it.
#[derive(Parser)]
#[command(
    name = "pumpkin",
    about = "\
Run workflows: in each run, an agent works in a sandbox, and judges score what
it submits.",
    after_help = "\
Topics:
  config  The machine's configuration, pumpkin.toml

`pumpkin help <command>` prints a command's help, as `pumpkin <command> --help`
does, and `pumpkin help <topic>` a topic.",
    disable_help_flag = true,
    disable_help_subcommand = true,
    subcommand_value_name = "command",
    subcommand_help_heading = "Commands"
)]
enum Parsed {
    #[command(about = "Start a run of a workflow")]
    #[command(long_about = "Start a run of a workflow, and print its id once it has started.")]
    #[command(after_help = RUN_NOTES)]
    Run(Run),
    #[command(about = "Wait until a run has ended, and print its outcome")]
    #[command(long_about = "Wait until a run has ended, then print its outcome.json.")]
    #[command(after_help = WAIT_NOTES)]
    Wait(Wait),
    #[command(about = "Print the help of a command or a topic")]
    #[command(long_about = "\
Print the help of a command or a topic, or `pumpkin --help` without one.")]
    #[command(after_help = HELP_NOTES)]
    Help {
        #[arg(value_name = "topic")]
        #[arg(help = "A command, or a topic: `config`, the machine's configuration.")]
        topic: Option<String>,
    },
}

/// What a command line asks for.
pub(crate) enum Command {
    Run(Run),
    Wait(Wait),
    /// Help, for the standard output.
    Print(String),
}

/// What `pumpkin run` is asked for, as `pumpkin run --help` says.
#[derive(Args)]
pub(crate) struct Run {
    #[arg(value_name = "ir", help = "The workflow's IR, a JSON file.")]
    pub(crate) ir: PathBuf,
    #[arg(
        long,
        value_name = "dir",
        help = "\
The directory runs are recorded in, each in <dir>/<id>.
Default: $HOME/.local/share/pumpkin/runs"
    )]
    pub(crate) runs: Option<PathBuf>,
    #[arg(
        long,
        value_name = "file",
        help = "\
The machine's configuration; see `pumpkin help config`.
Default: $HOME/.config/pumpkin/pumpkin.toml"
    )]
    pub(crate) config: Option<PathBuf>,
    #[arg(
        long,
        value_name = "path",
        help = "\
The static sandboxd that every sandbox runs, as `cargo xtask sandboxd`
builds it.
Default: the one beside this pumpkin, where `cargo xtask install` puts
one"
    )]
    pub(crate) sandboxd: Option<PathBuf>,
    #[arg(
        long,
        help = "\
Follow the run to its end, as `pumpkin wait` does, and print its
outcome.json instead of its id."
    )]
    pub(crate) wait: bool,
}

/// What `pumpkin wait` is asked for, as `pumpkin wait --help` says.
#[derive(Args)]
pub(crate) struct Wait {
    #[arg(value_name = "run", value_parser = run_name(), help = "\
The run: its id; a prefix of its id, or of the 8 hexadecimal digits
that end it, that no other run in --runs shares; or its record's path,
which has a `/`.")]
    pub(crate) run: RunName,
    #[arg(
        long,
        value_name = "dir",
        help = "\
The directory that <run> is looked up in.
Default: $HOME/.local/share/pumpkin/runs"
    )]
    pub(crate) runs: Option<PathBuf>,
}

/// A run, as `wait`'s `<run>` names it.
#[derive(Clone)]
pub(crate) enum RunName {
    /// By its record's path, which has a `/`.
    Record(PathBuf),
    /// By its id, or a prefix of its id or of its random digits, among the runs in `--runs`.
    Prefix(String),
}

/// The run that a `<run>` names, which is UTF-8 unless it is a path.
fn run_name() -> impl clap::builder::TypedValueParser<Value = RunName> {
    OsStringValueParser::new().try_map(|run| {
        if run.as_bytes().contains(&b'/') {
            return Ok(RunName::Record(run.into()));
        }
        run.into_string().map(RunName::Prefix).map_err(|_| {
            "it isn't UTF-8, as every id is: name an id, a prefix of one, or a record's path, \
             which has a `/`"
        })
    })
}

const RUN_NOTES: &str = "\
Examples:
  pumpkin run workflow.json
  pumpkin run --wait workflow.json > outcome.json

Exit status:
  0  The run has started.
  1  An error; a run that failed to start prints its outcome.json first.
  2  A usage error.
With --wait, `run` exits as `pumpkin wait` does once the run has started. Every
exit but 0 ends with a `pumpkin:` line on the standard error, which says why.

The run runs on the engine of the Docker context that the configuration names;
if that engine runs as root, `run` says so first. A driver process that `run`
starts drives the run, and writes what it says after the start to driver.log in
the record. Whatever ends `run` before the start ends the driver too; after it,
the run goes on without `run`, and `pumpkin wait` follows it.

The run outlives a logout unless its engine stops then, as rootless Docker does
without `loginctl enable-linger`, or logind kills the session's processes
(KillUserProcesses): then only a run under the user's manager outlives it, so
enable lingering and start `run` under `systemd-run --user --scope`. Killing the
driver leaves the run's containers, labelled pumpkin.run=<id>, its socket
directory in the temporary directory, and its record without an outcome.json.";

const WAIT_NOTES: &str = "\
Examples:
  pumpkin wait 20261108-000905-126a21e9
  pumpkin wait 126a
  pumpkin wait runs/20261108-000905-126a21e9

Exit status:
  0  The run finished with a score.
  3  The run finished without one.
  1  The run failed, or was lost as its driver ended without an outcome.json; or
     another error.
  2  A usage error.
Once the run has ended, `wait` ends with a `pumpkin:` line on the standard error
that says how, and where its record is; any other exit but 0 ends with one that
says why.
If the final judge returned no result, its standard error is shown first, or,
if it is long, named.

Ending `pumpkin wait` doesn't stop the run. A record made before records were
locked has no driver.pid, and is refused.";

const HELP_NOTES: &str = "\
Examples:
  pumpkin help run
  pumpkin help config

`pumpkin help <command>` prints what `pumpkin <command> --help` does.";

/// `pumpkin help config`, the machine's configuration.
const CONFIG: &str = "\
pumpkin.toml, the machine's configuration, which every `pumpkin run` reads: the
file that --config names, $HOME/.config/pumpkin/pumpkin.toml by default. It is
TOML, with the tables below. Every key is required, but the [upstreams.<name>]
tables, and any other key is refused.

Example:
  [docker]
  context = \"default\"

  [upstreams.gateway]
  url = \"https://gateway.example/v1\"
  key_file = \"gateway-key\"
  models = [{ name = \"model-a\", upstream_name = \"provider/model-a\" }]

[docker], the Docker engine that the machine's runs run on:
  context       The context that reaches it, as `docker context ls` lists it.

[upstreams.<name>], for each upstream that a workflow's model route may reach:
  url           The base URL of its API: https, or http to a loopback IP
                address, such as 127.0.0.1 but not localhost. The route reaches
                /responses below it.
  key_file      The file that holds its key, below this file's directory unless
                absolute.
  models        The models it serves, each with the `name` that a route allows
                and the `upstream_name` that the upstream takes for it. No model
                is listed twice, under one upstream or two.

A workflow's model route reaches the one upstream that serves every model it
allows.
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
/// Fails with the first usage problem of `args`, unless they ask for help.
pub(crate) fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Command, Refusal> {
    let args: Vec<OsString> = args.into_iter().collect();
    let mut cli = command();
    // clap's error doesn't name the command it was found in; the first word does, as only `-h`
    // and `--help` may come before it.
    let first = args.iter().find(|arg| !arg.as_bytes().starts_with(b"-"));
    let named = first.and_then(|word| cli.find_subcommand(word));
    let named = named.map(|command| command.get_name().to_owned());
    let mut options = args.iter().take_while(|arg| *arg != "--");
    if options.any(|arg| arg == "-h" || arg == "--help") {
        return Ok(Command::Print(help(&mut cli, named.as_deref())));
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
    match Parsed::from_arg_matches(&matches).map_err(refusal)? {
        Parsed::Run(run) => Ok(Command::Run(run)),
        Parsed::Wait(wait) => Ok(Command::Wait(wait)),
        Parsed::Help { topic: None } => Ok(Command::Print(help(&mut cli, None))),
        Parsed::Help { topic: Some(topic) } if topic == "config" => {
            Ok(Command::Print(CONFIG.to_owned()))
        }
        Parsed::Help { topic: Some(topic) } if cli.find_subcommand(&topic).is_some() => {
            Ok(Command::Print(help(&mut cli, Some(&topic))))
        }
        Parsed::Help { topic: Some(topic) } => {
            let commands: Vec<String> = (cli.get_subcommands())
                .map(|command| format!("`{}`", command.get_name()))
                .collect();
            let problem = format!(
                "there is no help for `{topic}`: expected a command ({}) or a topic (`config`)",
                commands.join(", ")
            );
            // Found in `help`, which the line names, so the error shows that command's usage.
            let error = match named
                .as_deref()
                .and_then(|name| cli.find_subcommand_mut(name))
            {
                Some(command) => command.error(ErrorKind::InvalidValue, problem),
                None => cli.error(ErrorKind::InvalidValue, problem),
            };
            Err(refusal(error))
        }
    }
}

/// `pumpkin`, with `-h` and `--help` listed last in each command's help.
fn command() -> clap::Command {
    // No command has a help flag of clap's, so a usage error ends without clap's pointer to
    // it, and `pumpkin help <command>` is said instead; `parse` answers `-h` and `--help`.
    let help = Arg::new("help").short('h').long("help").global(true);
    let help = help.action(ArgAction::SetTrue).help("Print this help.");
    Parsed::command().arg(help.display_order(usize::MAX))
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
