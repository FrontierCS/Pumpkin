//! Starting a run, and the driver that drives it: `pumpkin run`'s part, which starts the driver
//! and says that the run has started, and the driver's, which places the roles, connects the
//! providers that they are placed on, builds the task and executes the run.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::{Duration, SystemTime};

use docker::Docker;
use ec2::Ec2;
use engine::Engine;
use gateway::{Dialer, Listener};
use modal::Modal;
use provider::{Meeting, Provider, RunTag};
use runtime::{Budgets, ModelRoutes, Record, Recorded, Run, System, TokenPrices};
use rustix::process::Signal;
use schema::ir::{ModelName, Name};
use schema::sandbox::HELLO_TIMEOUT;
use schema::workflow::{ByProvider, ByRole, Class, ParseErrors, Placed, Workflow};
use stdx::Context;

use crate::bodies::{Bodies, Sources};
use crate::cli::Exit;
use crate::driver::{ARGV0, Driver, Handoff};
use crate::ending::{Printed, closing, end, follow, followed, following};
use crate::sandboxd::Sandboxd;
use crate::workspace::Workspace;
use crate::{
    Reader, absolute_utf8, advise, arg_flags, cli, closing_problems, config, config_path,
    controller, driver, modal_helper, next, print, runs_dir, say, star, task_problems,
};

/// How long a run that waits for room pauses before it tries again: each try reads a few small
/// files of a provider's, and a run that waits starts at most this late once there is room.
const ROOM_AGAIN: Duration = Duration::from_secs(5);

pub(crate) fn run(flags: cli::Run) -> Result<Exit, String> {
    if let Some(client) = config::client(flags.config.as_deref(), flags.controller.as_ref())? {
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
pub(crate) fn start(id: &RunTag, flags: cli::Run, reader: Reader) -> Result<Exit, String> {
    // Absolute, so that the record's path, which the driver says and `run` opens, names it from
    // any directory. A runs directory not given is left for the driver to find as `run` does,
    // so that the driver's flags are those that the operator gave.
    let (runs, named) = runs_dir(flags.runs.clone())?;
    let cli::Waiting(wait) = flags.wait;
    let flags = cli::Run {
        runs: named.clone(),
        ..flags
    };
    let runs = named.as_deref().unwrap_or(&runs);
    let named = &next::At::runs(named.as_deref());
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
            if let Some(printed) = wait {
                // Whether the run is still live by now depends on how fast it went, which tells
                // the operator nothing: `run` follows the run it started however soon it ends.
                following(&record, "run");
                let ending = followed(&record)?;
                return end(&record, &ending, printed, named, record.args());
            }
            if reader == Reader::Operator {
                advise("follow it", &next::of_run("wait", named, id));
            }
            advise("stop it", &next::of_run("cancel", named, id));
            print(format!("{id}\n").as_bytes()).context(format_args!(
                "run {id} has started, but printing its id failed"
            ))?;
            Ok(Exit::Success)
        }
        Handoff::Ended(status, record) => {
            let printed = wait.unwrap_or(Printed::Nothing);
            unstarted(status, record.as_ref(), named, printed)
        }
    }
}

fn unstarted(
    status: ExitStatus,
    record: Option<&Record>,
    runs: &next::At,
    printed: Printed,
) -> Result<Exit, String> {
    if status.code().is_some() {
        if let Some(record) = record {
            let recorded = record.dir().join("outcome.json");
            if recorded.try_exists().context(recorded.display())? {
                printed.print(record.dir())?;
            }
        }
        return Ok(Exit::Failure);
    }
    let ended = format!("the driver ended before the run started, by {status}");
    match record {
        None => Err(ended),
        Some(record) => {
            say(ended);
            follow(record, "run", runs, printed)
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
pub(crate) fn driver_run(args: Vec<OsString>) -> Result<Exit, String> {
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
        controller: _,
        sandboxd,
        wait: _,
    } = run;
    // The task, `sandboxd`, the configuration and its upstream's key are checked before the engine
    // is reached, so a mistake in one needs no engine and makes no record. The roles are placed
    // before any image is built, so a role that fits no class that the providers offer is refused
    // before anything is built or reserved. What only a provider refuses, such as a `sandboxd` it
    // can't mount, is refused once the record exists.
    let (runs, _) = runs_dir(runs)?;
    let sandboxd = Sandboxd::find(sandboxd.map(PathBuf::from))?;
    let config = config_path(config)?;
    // Held here once reached, as the providers connected to it outlive the thread that checks and
    // builds the task.
    let mut engine = None;
    let prepared = star::checked(&entry, &args, |task| {
        prepared(task, Path::new(entry.path()), &config, &mut engine, reader)
    });
    let prepared =
        prepared.and_then(|built| built.map_err(|problems| task_problems(&entry, &problems)));
    let Prepared {
        placed,
        placed_on,
        sources,
        gateway,
        servings,
        budgets,
        prices,
    } = prepared.map_err(|error| refused(error, again.as_deref()))?;
    let private = listen(&placed_on, &gateway, &config)?;
    // Before the record is begun, so that a signal that comes once it is begun stops the run,
    // which records it, rather than ending the driver and leaving the run lost.
    let caught = stdx::signal::catch(&[Signal::INT, Signal::TERM, Signal::HUP]);
    caught.context("setting the handlers of SIGINT, SIGTERM and SIGHUP failed")?;
    let pushed = placed_on.iter().filter(|(_, connected)| connected.pushes());
    let names: Vec<String> = pushed.map(|(name, _)| format!("`{name}`")).collect();
    let on = placed_on.as_ref().map(Connected::provider);
    let kept = Recorded {
        args: &words,
        prices: &prices,
        sources: sources.texts(),
    };
    let models = ModelRoutes {
        servings: &servings,
        budgets,
    };
    let (runs, mut room) = (Path::new(&runs), room(id.as_str(), waiting(&id, reader)?));
    let run = Run::prepare(&System, placed, on, runs, id, models, kept, &mut room)?;
    let (id, dir) = (run.id().to_owned(), run.dir().to_owned());
    begun(&id, &dir, &names, reader);
    driver::locked(&id)?;
    // `execute` records why the run stopped in its `outcome.json`; these lines, the driver's last,
    // then close that ending, which a controller's client says from its copy instead.
    // Binds nothing, so every run has one, whether or not a role's sandboxes listen.
    let dialer = Dialer::new(tls::Client::web_pki());
    let bodies = Bodies::new(&sources, &args);
    let executed = run.execute(sandboxd.path(), private.as_ref(), &dialer, &bodies, || {
        driver::announce(&dir)
    });
    if let Err(stopped) = executed {
        if let Some(again) = &again {
            say(closing(&id, &stopped.to_string(), &dir));
            advise("run it again", again);
        }
        return Ok(Exit::Failure);
    }
    Ok(Exit::Success)
}

/// Where a controller's job marks that run `id` waits, as for a build slot, so that `ls` lists it
/// as waiting until its record lists it: `None` for a run of the operator's.
fn waiting(id: &RunTag, reader: Reader) -> Result<Option<PathBuf>, String> {
    match reader {
        Reader::Operator => Ok(None),
        Reader::Follower => Ok(Some(
            crate::job::jobs()?
                .join(id.as_str())
                .join(crate::job::WAITING),
        )),
    }
}

/// The wait of run `id` for room that [`Run::prepare`] takes: it marks `waiting`, if given, says
/// what holds the room each time that changes, then pauses for `ROOM_AGAIN`.
fn room(id: &str, waiting: Option<PathBuf>) -> impl FnMut(&str) -> Result<(), String> + use<> {
    let id = id.to_owned();
    let mut said = None;
    move |held: &str| {
        if said.is_none()
            && let Some(waiting) = &waiting
        {
            fs::File::create(waiting).context(waiting.display())?;
        }
        if said.as_deref() != Some(held) {
            say(format_args!(
                "run {id} waits for room {held}: it starts once there is"
            ));
            said = Some(held.to_owned());
        }
        std::thread::sleep(ROOM_AGAIN);
        Ok(())
    }
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
    placed: Placed,
    placed_on: ByProvider<Connected<'e>>,
    sources: Sources,
    gateway: config::Gateway,
    servings: ByRole<Vec<route::Serving>>,
    budgets: Budgets,
    prices: BTreeMap<ModelName, TokenPrices>,
}

fn prepared<'e>(
    task: star::Task<'_>,
    entry: &Path,
    config: &Path,
    engine: &'e mut Option<Engine>,
    reader: Reader,
) -> Result<Result<Prepared<'e>, lang::Problems>, String> {
    let config::Config {
        docker: config::Docker { context },
        providers,
        gateway,
        upstreams,
    } = config::read(config)?;
    let config::Models {
        servings,
        budgets,
        prices,
    } = upstreams
        .models(task.workflow())
        .context(config.display())?;
    let engine = &*engine.insert(crate::engine(context, reader)?);
    let here = Offered::Here { entry, config };
    let reached = reached(task.workflow(), providers, engine, &here)?;
    let (workflow, _, sources) = match task.build(engine, &mut ()) {
        Ok(built) => built,
        Err(problems) => return Ok(Err(problems)),
    };
    // Only `Workflow::place` makes a `Placed`, so the workflow with its images built is placed
    // again, on the providers reached. A build changes no role's needs, so each role is placed
    // where it was.
    let (placed, placed_on) = place(workflow, reached, Connected::classes, &here)?;
    Ok(Ok(Prepared {
        placed,
        placed_on,
        sources,
        gateway,
        servings,
        budgets,
        prices,
    }))
}

/// The providers of `providers` that the roles of `workflow` are placed on, each reached on
/// `engine`. Fails, before the task is built and anything is reserved, if a role fits no class that
/// they offer, a provider can't be reached, or one could never hold the roles that run at once
/// ([`runtime::fits`]), and says each problem as `offered` has it. A run that fits once other runs
/// end passes.
fn reached<'e>(
    workflow: &Workflow,
    providers: config::Providers<config::Kind>,
    engine: &'e Engine,
    offered: &Offered<'_>,
) -> Result<BTreeMap<Name, Connected<'e>>, String> {
    // The engine's own provider is reached here, so that an engine that can't run sandboxes is
    // refused before any image is built.
    let offers = (providers.into_by_name().into_iter())
        .map(|(name, kind)| {
            let offer = Offer::new(&name, kind, engine)?;
            Ok((name, offer))
        })
        .collect::<Result<_, String>>()?;
    let (_, placed_on) = place(workflow.clone(), offers, Offer::classes, offered)?;
    // Each provider that a role is placed on is reached here, so that one that can't be reached is
    // refused before any image is built. A provider that no role is placed on is never reached.
    let reached = (placed_on.into_iter())
        .map(|(name, offer)| {
            let connected = Connected::new(&name, offer, engine)?;
            Ok((name, connected))
        })
        .collect::<Result<_, String>>()?;
    let (placed, placed_on) = place(workflow.clone(), reached, Connected::classes, offered)?;
    let fits = runtime::fits(&placed, &placed_on.as_ref().map(Connected::provider));
    fits.map_err(|error| match offered {
        Offered::Here { config, .. } => format!("{}: {error}", config.display()),
        Offered::Controller(name) => format!("controller `{name}`: {error}"),
    })?;
    Ok(placed_on.into_iter().collect())
}

/// A provider that the configuration names, as the roles are placed: the machine's engine,
/// reached, whose one class is what the engine has; or another, unreached, with the classes that
/// its settings state.
enum Offer<'e> {
    Docker(Docker<'e>),
    Ec2(ec2::Settings, BTreeMap<Name, Class>),
    Modal(modal::Settings, BTreeMap<Name, Class>),
}

impl<'e> Offer<'e> {
    fn new(name: &Name, kind: config::Kind, engine: &'e Engine) -> Result<Self, String> {
        let offer = match kind {
            config::Kind::Docker(config::Empty {}) => {
                let docker = Docker::connect(engine);
                Self::Docker(docker.context(format_args!("reaching provider `{name}` failed"))?)
            }
            config::Kind::Ec2(settings) => {
                let classes = settings.classes();
                Self::Ec2(settings, classes)
            }
            config::Kind::Modal(settings) => {
                let classes = settings.classes();
                Self::Modal(settings, classes)
            }
        };
        Ok(offer)
    }

    fn classes(&self) -> &BTreeMap<Name, Class> {
        match self {
            Self::Docker(docker) => &docker.ready().classes,
            Self::Ec2(_, classes) | Self::Modal(_, classes) => classes,
        }
    }
}

enum Connected<'e> {
    Docker(Docker<'e>),
    Ec2(Ec2<'e>),
    Modal(Modal<'e>),
}

impl<'e> Connected<'e> {
    fn new(name: &Name, offer: Offer<'e>, engine: &'e Engine) -> Result<Self, String> {
        let connected = match offer {
            Offer::Docker(docker) => Ok(Self::Docker(docker)),
            Offer::Ec2(settings, _) => Workspace::find()
                .and_then(|workspace| Ec2::connect(&settings, engine.pusher()?, &workspace.ec2()))
                .map(Self::Ec2),
            Offer::Modal(settings, _) => modal_helper()
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

    fn classes(&self) -> &BTreeMap<Name, Class> {
        &self.provider().ready().classes
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

/// `workflow` placed in the classes that `classes` gives of each of `providers`, which `offered`
/// offers. It comes with each provider that it places a role on. Otherwise, once it has said why
/// each role that fits none fits none, it gives how many there are.
fn place<P>(
    workflow: Workflow,
    providers: BTreeMap<Name, P>,
    classes: impl Fn(&P) -> &BTreeMap<Name, Class>,
    offered: &Offered<'_>,
) -> Result<(Placed, ByProvider<P>), String> {
    (workflow.place(providers, classes)).map_err(|errors| misplaced(&errors, offered))
}

/// Who offers the sandboxes of a run that a refusal of it names.
enum Offered<'a> {
    /// The providers of the configuration at `config`, to the operator of the task whose entry is
    /// at `entry`.
    Here { entry: &'a Path, config: &'a Path },
    /// A controller, to its member, by the name that the member's configuration gives it.
    Controller(&'a controller::Name),
}

/// Says each of `errors`, a placement's, as `offered` has it, and returns the error that closes
/// them.
fn misplaced(errors: &ParseErrors, offered: &Offered<'_>) -> String {
    let count = errors.as_slice().len();
    match offered {
        Offered::Here { entry, config } => {
            let path = entry.display();
            for error in errors.as_slice() {
                say(format_args!("{path}: {error}"));
            }
            let has = format_args!(
                "{path}: placing the workflow's roles in the classes that {}'s providers offer has",
                config.display()
            );
            closing_problems(has, count)
        }
        Offered::Controller(name) => {
            for error in errors.as_slice() {
                say(format_args!("controller `{name}`: {error}"));
            }
            let has = format_args!(
                "controller `{name}`: placing the workflow's roles in the classes of sandbox \
                 that it offers has"
            );
            closing_problems(has, count)
        }
    }
}

/// Fails as [`reached`] does, to the member of the controller that they name `controller`, if
/// the roles of `workflow` never fit the providers of the configuration at `config`, which it
/// reaches on `engine`.
pub(crate) fn never_fits_controller(
    workflow: &Workflow,
    config: &Path,
    engine: &Engine,
    controller: &controller::Name,
) -> Result<(), String> {
    let config::Config {
        docker: _,
        providers,
        gateway: _,
        upstreams: _,
    } = config::read(config)?;
    reached(workflow, providers, engine, &Offered::Controller(controller))?;
    Ok(())
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
    placed_on: &ByProvider<Connected<'_>>,
    gateway: &config::Gateway,
    config: &Path,
) -> Result<Option<Listener>, String> {
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
    let bound = Listener::bind(address, HELLO_TIMEOUT).map_err(|error| {
        format!(
            "{}: {error}: name in `[gateway] private` an address of this machine's where no other \
             run listens",
            config.display()
        )
    })?;
    Ok(Some(bound))
}
