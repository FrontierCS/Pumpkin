//! Starting a run, and the driver that drives it: `pumpkin run`'s part, which starts the driver
//! and says that the run has started, and the driver's, which builds the task, connects the
//! providers, places the roles and executes the run.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, ExitStatus};
use std::time::SystemTime;

use docker::{CpuList, Docker};
use ec2::Ec2;
use engine::Engine;
use gateway::{Dialer, GrantListener};
use modal::Modal;
use provider::{Meeting, Provider, RunTag};
use runtime::{Record, Recorded, Run, System, TokenPrices};
use rustix::process::Signal;
use schema::ir::{ModelName, Name};
use schema::sandbox::HELLO_TIMEOUT;
use schema::workflow::{ByProvider, ByRole, Placed, Workflow};
use stdx::Context;

use crate::driver::{ARGV0, Driver, Handoff};
use crate::ending::{closing, end, follow, followed, following};
use crate::sandboxd::Sandboxd;
use crate::workspace::Workspace;
use crate::{
    Reader, absolute_utf8, advise, arg_flags, cli, closing_problems, config, config_path,
    controller, driver, modal_helper, next, print, print_outcome, runs_dir, say, star,
    task_problems,
};

pub(crate) fn run(flags: cli::Run) -> Result<ExitCode, String> {
    if let Some(client) = config::client(flags.config.as_deref())? {
        return controller::run(&client, flags);
    }
    start(
        &runtime::new_id(SystemTime::now())?,
        flags,
        Reader::Operator,
    )
}

/// Starts run `id` in a driver of its own, and returns once the run has started, or with
/// `flags.wait` once it has ended. It says nothing of how to follow the run to a follower
/// ([`Reader`]).
pub(crate) fn start(id: &RunTag, flags: cli::Run, reader: Reader) -> Result<ExitCode, String> {
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

/// The arguments of `run` as its record keeps them, paths made absolute, so that every ending can
/// say how to run it again: its flags, then its entry.
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

/// The driver of the run that `args` name, until the run ends; `args` are those that
/// [`Driver::spawn`] gives it. A run that stops short of its outcome is a failure.
pub(crate) fn driver_run(args: Vec<OsString>) -> Result<ExitCode, String> {
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
        prices,
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
    let kept = Recorded {
        args: &words,
        prices: &prices,
    };
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
/// `run` hears of it, so that whatever ends both later, the record is named; and that its images
/// are published on the providers `pushing` names, if any, as a push may take minutes, which the
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

struct Prepared<'e> {
    workflow: Workflow,
    providers: config::Providers<Connected<'e>>,
    gateway: config::Gateway,
    servings: ByRole<Vec<route::Serving>>,
    prices: BTreeMap<ModelName, TokenPrices>,
}

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
        publish: _,
    } = config::read(config)?;
    let servings = task
        .workflow()
        .by_role(|(_, sandbox)| match &sandbox.model_route {
            Some(route) => upstreams.serving(route).context(config.display()),
            None => Ok(Vec::new()),
        })?;
    let prices = upstreams.prices();
    let engine = &*engine.insert(crate::engine(context, reader)?);
    // Each provider is connected before any image is built, so that an engine that can't run
    // sandboxes is refused first.
    let providers =
        providers.connect(|name, kind| Connected::new(name, kind, engine, cpus.as_ref()))?;
    Ok(task.build(engine, &mut ()).map(|(workflow, _)| Prepared {
        workflow,
        providers,
        gateway,
        servings,
        prices,
    }))
}

enum Connected<'e> {
    Docker(Docker<'e>),
    Ec2(Ec2<'e>),
    Modal(Modal<'e>),
}

impl<'e> Connected<'e> {
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

/// The listener that the sandboxes of the providers in `placed_on` dial, for those that dial over
/// a private network, and `None` if none does.
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
