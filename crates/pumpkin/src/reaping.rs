//! Stopping runs, and removing what a run that ended or was lost left on its providers.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use docker::Docker;
use ec2::Ec2;
use engine::ContextName;
use modal::Modal;
use provider::{SandboxId, Sweep};
use runtime::{Reaped, Record, Writer};
use rustix::process::{Pid, Signal};
use schema::ir::Name;
use serde::{Deserialize, Serialize};
use stdx::Context;

use crate::cli::Exit;
use crate::ending::{Printed, end};
use crate::workspace::Workspace;
use crate::{
    advise, cli, config, controller, counted, default_config, found, modal_helper, next, print,
    runs_dir, say,
};

/// What a cancel says of a run whose driver it signalled. A driver that SIGTERM stops deletes the
/// run's sandboxes before it writes the outcome, which takes a while on a provider whose
/// sandboxes are instances.
pub(crate) const STOPPING: &str = "it ends once its sandboxes are deleted";

/// Stops the run, waits for it to end, and reaps it, as `pumpkin cancel` does.
pub(crate) fn cancel(flags: cli::CancelRun) -> Result<Exit, String> {
    let cli::CancelRun {
        cancel: flags,
        controller,
        any_member,
    } = flags;
    if let Some((client, prefix)) = crate::client_of(&flags.run, flags.runs.as_ref(), controller)? {
        return controller::cancel(&client, prefix, any_member);
    }
    let cli::Cancel {
        run,
        runs,
        modal_config,
    } = flags;
    let (record, named) = found(run, runs)?;
    let named = &next::At::runs(named.as_deref());
    if signalled(&record)? {
        say(format_args!(
            "stopping run {}, recorded in {}: {STOPPING}",
            record.id(),
            record.dir().display()
        ));
    }
    let reached = ended(&record, modal_config)??.said(named, Reaper::Command)?;
    // `cancel`'s exit says only that the run has ended, however that was, and whether what it
    // left on its provider is gone.
    let ending = record.wait(|| {})?;
    end(&record, &ending, Printed::Nothing, named, record.args())?;
    if !reached {
        advise(
            "remove what it left once its provider can be reached",
            &next::reap(named),
        );
        return Ok(Exit::Partial);
    }
    Ok(Exit::Success)
}

/// Signals the driver of the run of `record`, if it has one: whether it had one.
pub(crate) fn signalled(record: &Record) -> Result<bool, String> {
    let id = record.id();
    let Some(pid) = record.driver()? else {
        return Ok(false);
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
    let sent = rustix::process::kill_process(driver, Signal::TERM);
    sent.context(format_args!(
        "signalling run {id}'s driver, process {pid}, failed"
    ))?;
    Ok(true)
}

/// Waits for the run of `record` to end, then reaps it, with `modal_config` as Modal's
/// configuration for every provider of kind `modal`, if given. The inner error is that of a reap
/// that failed before it reached a provider.
pub(crate) fn ended(
    record: &Record,
    modal_config: Option<PathBuf>,
) -> Result<Result<RunReaped, String>, String> {
    // Its provider is known for certain only once it has ended: a driver yet to start the run
    // has yet to say which it is.
    record.wait(|| {})?;
    Ok(reaped(record, &mut Reaching::new(modal_config)))
}

/// Who reaps, which decides the runs that a reap takes, and what it makes of a provider of
/// theirs that it can't reach.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reaper {
    /// `pumpkin reap` or `pumpkin cancel`: every run that has ended or is lost; a provider that
    /// can't be reached makes the exit status [`Exit::Partial`], and the reap says how to reap
    /// again once it can be.
    Command,
    /// A client's request: the ended or lost runs this build wrote, and those of other builds
    /// named as theirs.
    Controller,
    /// The controller's boot: runs this build wrote that may have left something, not one that
    /// ended clean ([`Record::ended_clean`]); a provider that it can't reach, as Modal, whose
    /// credentials reach the controller only with a job, it says leaves what the run left
    /// there to end by the run's hard end, and goes on.
    Boot,
}

/// Removes what each run in the runs directory that `flags` name left once it ended or was lost
/// ([`Record::reap`]), of the runs that `reaper` takes, saying each as it is reaped and printing
/// the id of each that had left something; leaves each live run to its driver. The exit status is
/// `reap`'s, but 0 for [`Reaper::Boot`] though a provider can't be reached.
pub(crate) fn reap(flags: cli::Reap, reaper: Reaper) -> Result<Exit, String> {
    let cli::Reap {
        runs,
        controller,
        modal_config,
    } = flags;
    if reaper == Reaper::Command
        && runs.is_none()
        && let Some(client) = config::client(None, controller.as_ref())?
    {
        return controller::reap(&client);
    }
    let (runs, named) = runs_dir(runs)?;
    let named = &next::At::runs(named.as_deref());
    let mut swept = ReapedRuns::default();
    sweep(Path::new(&runs), modal_config, reaper, |reaping| {
        swept.said(reaping, named, reaper)
    })?;
    Ok(swept.report(named, reaper))
}

/// Reaps each run in `runs` that `reaper` takes, ended or lost, and gives `reported` what came of
/// each that had left something, or that a problem stays with, as it comes; leaves each live run to
/// its driver. Fails, and stops, if `reported` fails.
pub(crate) fn sweep(
    runs: &Path,
    modal_config: Option<PathBuf>,
    reaper: Reaper,
    mut reported: impl FnMut(Reaping) -> Result<(), String>,
) -> Result<(), String> {
    let mut reaching = Reaching::new(modal_config);
    if reaper != Reaper::Command && !runs.try_exists().context(runs.display())? {
        return Ok(());
    }
    for id in Record::names(runs)? {
        let dir = runs.join(&id);
        let mut swept = || -> Result<Option<Reaping>, String> {
            let path = dir.join("record.json");
            if !path.try_exists().context(path.display())? {
                return Ok(None);
            }
            if reaper != Reaper::Command {
                let other = |build| {
                    let other = Reaping::Other {
                        id: id.clone(),
                        build,
                    };
                    Ok((reaper == Reaper::Controller).then_some(other))
                };
                match runtime::writer(&dir)? {
                    Writer::This => {}
                    Writer::Other(build) => return other(Some(build)),
                    Writer::Older => return other(None),
                }
            }
            let record = Record::open(&dir)?;
            if record.driver()?.is_some() || reaper == Reaper::Boot && record.ended_clean()? {
                return Ok(None);
            }
            // A run that left nothing, on providers all reached, has nothing to say.
            let run = reaped(&record, &mut reaching)?;
            let said = run.left() || !run.reached() || run.failed.is_some();
            Ok(said.then_some(Reaping::Reaped(run)))
        };
        let reaping = match swept() {
            Ok(Some(reaping)) => reaping,
            Ok(None) => continue,
            Err(message) => Reaping::Unreaped {
                id: id.clone(),
                message,
            },
        };
        reported(reaping)?;
    }
    Ok(())
}

/// What came of one run of a reap.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Reaping {
    /// A run reaped, in whole or in part.
    Reaped(RunReaped),
    /// A run that can't be reaped, for the reason that `message` gives.
    Unreaped { id: String, message: String },
    /// A run that another build made, which only that build reaps: `build` as a
    /// [`runtime::Build`] is shown, or none for one from before records named theirs.
    Other { id: String, build: Option<String> },
}

/// What the reap of a run removed, and what it couldn't.
#[derive(Serialize, Deserialize)]
pub(crate) struct RunReaped {
    id: String,
    /// The run's record directory, as the reap's words name it.
    record: String,
    /// Each provider of the run's that can't be reached, by its name, with why: what the run left
    /// there may remain.
    unreached: Vec<(Name, String)>,
    /// The sandboxes that each provider reached still listed, by the provider's name, now deleted.
    sandboxes: Vec<(Name, Vec<String>)>,
    /// The sandboxes that the record held undeleted, which their provider no longer listed, by
    /// its name, now deleted with what each left there.
    ended: Vec<(Name, String)>,
    /// The run's directory of sockets, now removed.
    sockets: Option<String>,
    /// Whether the run had no outcome, and is now recorded as failed.
    lost: bool,
    /// Why the reap failed, having done what the fields above say.
    failed: Option<String>,
}

impl RunReaped {
    /// Whether the run had left anything, now removed.
    fn left(&self) -> bool {
        let sandboxes = self.sandboxes.iter().any(|(_, ids)| !ids.is_empty());
        sandboxes || !self.ended.is_empty() || self.sockets.is_some() || self.lost
    }

    /// Whether each provider of the run's was reached.
    fn reached(&self) -> bool {
        self.unreached.is_empty()
    }

    /// Says what the reap of a cancelled run did, as [`Self::say`] does: whether each of its
    /// providers was reached, or why the reap failed.
    pub(crate) fn said(self, at: &next::At, reaper: Reaper) -> Result<bool, String> {
        self.say(at, reaper);
        match self.failed {
            Some(failed) => Err(failed),
            None => Ok(self.unreached.is_empty()),
        }
    }

    /// Says what the reap removed, and of each provider that it couldn't reach, what may remain
    /// and what to do, as `reaper` has it, naming the runs where `at` says. Says nothing of
    /// [`Self::failed`].
    fn say(&self, at: &next::At, reaper: Reaper) {
        let Self {
            id,
            record,
            unreached,
            sandboxes,
            ended,
            sockets,
            lost,
            failed: _,
        } = self;
        for (name, error) in unreached {
            match reaper {
                Reaper::Command | Reaper::Controller => say(format_args!(
                    "run {id}'s sandboxes on provider `{name}`, if it left any, can't be \
                     removed, as it can't be reached: {error}; once it can be, `{}` removes \
                     them, and if it is gone for good, remove the record, {record}, to stop \
                     reaping it",
                    next::reap(at),
                )),
                Reaper::Boot => say(format_args!(
                    "run {id}'s sandboxes on provider `{name}`, if it left any, end by the run's \
                     hard end, as the boot can't reach it: {error}"
                )),
            }
        }
        for (name, ids) in sandboxes {
            if ids.is_empty() {
                continue;
            }
            let sandboxes = counted(ids.len(), "sandbox");
            say(format_args!(
                "run {id} left {sandboxes} on provider `{name}`, now removed: {}",
                ids.join(", ")
            ));
        }
        for (name, sandbox) in ended {
            say(format_args!(
                "run {id}'s sandbox {sandbox} on provider `{name}` had ended, as it isn't listed \
                 there now: what it left there is now removed"
            ));
        }
        if let Some(sockets) = sockets {
            say(format_args!(
                "run {id} left its socket directory, now removed: {sockets}"
            ));
        }
        if *lost {
            say(format_args!(
                "run {id} was lost, as its driver ended without an outcome.json: it is now recorded \
                 as failed"
            ));
        }
    }
}

/// What the runs of a reap came to, as [`ReapedRuns::said`] counts them: those that can't be
/// reaped, those with a provider that can't be reached, and other builds' runs by the build that
/// made them.
#[derive(Default)]
pub(crate) struct ReapedRuns {
    unreaped: Vec<String>,
    unreached: Vec<String>,
    others: BTreeMap<String, Vec<String>>,
}

impl ReapedRuns {
    /// Says what came of a run, `reaping`, as `reaper` has it, naming the runs where `at` says;
    /// prints the id of a run that had left something, now removed; and counts it.
    pub(crate) fn said(
        &mut self,
        reaping: Reaping,
        at: &next::At,
        reaper: Reaper,
    ) -> Result<(), String> {
        match reaping {
            Reaping::Reaped(run) => {
                run.say(at, reaper);
                if let Some(failed) = &run.failed {
                    say(failed);
                    self.unreaped.push(run.id);
                    return Ok(());
                }
                if run.left() {
                    printed_id(&run.id)?;
                }
                if !run.reached() {
                    self.unreached.push(run.id);
                }
            }
            Reaping::Unreaped { id, message } => {
                say(message);
                self.unreaped.push(id);
            }
            Reaping::Other { id, build } => {
                let writer = build.map_or_else(
                    || "an older pumpkin".to_owned(),
                    |build| format!("pumpkin {build}"),
                );
                self.others.entry(writer).or_default().push(id);
            }
        }
        Ok(())
    }

    /// Says what the runs came to, and returns the exit status that it makes.
    pub(crate) fn report(self, at: &next::At, reaper: Reaper) -> Exit {
        let Self {
            unreaped,
            unreached,
            others,
        } = self;
        for (writer, ids) in others {
            say(format_args!(
                "to reap runs made by {writer}, use that build: {}",
                ids.join(", ")
            ));
        }
        if !unreaped.is_empty() {
            let runs = counted(unreaped.len(), "run");
            say(format_args!(
                "{runs} can't be reaped, as said above: fix what each says, and reap again"
            ));
            advise("reap again", &next::reap(at));
            return Exit::Failure;
        }
        if !unreached.is_empty() && reaper != Reaper::Boot {
            let runs = counted(unreached.len(), "run");
            say(format_args!(
                "{runs} ran on a provider that can't be reached, as said above: what each left \
             there may remain"
            ));
            advise("reap again once it can be reached", &next::reap(at));
            return Exit::Partial;
        }
        Exit::Success
    }
}

fn printed_id(id: &str) -> Result<(), String> {
    print(format!("{id}\n").as_bytes()).context("printing a reaped run's id failed")
}

/// The providers that a reap has reached, or failed to reach, so that each is reached once,
/// however many of its runs it reaps: a Docker engine by the context that a run's report of it
/// names, and any other provider by the configuration that names it, by its path, and its name
/// there, each configuration read once.
struct Reaching {
    reached: BTreeMap<Reachable, Result<Box<dyn Sweep>, String>>,
    configurations: BTreeMap<PathBuf, Result<config::Providers<config::Kind>, String>>,
    /// Modal's configuration file for every provider of kind `modal`, in place of the one that
    /// a run's configuration names, as a client sends it with a cancel or a reap.
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

    /// How provider `name` of the run of `record`, which reported `report` of itself, is reached,
    /// once it has been tried: a provider whose report names a Docker context by the engine of
    /// that context; any other by its table in the configuration that ran the run
    /// ([`run_config`]). Fails, saying why, if it can't be named so, as when that configuration
    /// can't be read or names no such provider; or, in the outer error, if the record is wrong,
    /// as when its report names no context but the configuration names a provider of kind
    /// `docker`.
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

/// The configuration that the run ran with: the file that its record's `--config` names, or the
/// default one.
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
/// `provider.json` names, as `reaching` reaches it ([`Record::reap`]), but those that can't be
/// reached, which it names. Fails before it reaches any provider if the record names one wrong.
fn reaped(record: &Record, reaching: &mut Reaching) -> Result<RunReaped, String> {
    let id = record.id();
    let failed = |error| format!("run {id} can't be reaped: {error}");
    let reports = record.provider_reports().map_err(failed)?;
    let mut reachable = Vec::new();
    for (name, report) in &reports {
        reachable.push((name, reaching.reach(record, name, report).map_err(failed)?));
    }
    let mut providers = Vec::new();
    let mut unreached = Vec::new();
    for (name, reachable) in reachable {
        let reached = reachable.and_then(|reachable| match &reaching.reached[&reachable] {
            Ok(sweep) => Ok(sweep.as_ref()),
            Err(error) => Err(error.clone()),
        });
        match reached {
            Ok(sweep) => providers.push((name, sweep)),
            // Not the reap's to fail: a record's provider may be gone for good, as on a machine
            // set up again since, and a failure would fail every reap after it.
            Err(error) => unreached.push((name.clone(), error)),
        }
    }
    let mut run = RunReaped {
        id: id.to_owned(),
        record: record.dir().display().to_string(),
        unreached,
        sandboxes: Vec::new(),
        ended: Vec::new(),
        sockets: None,
        lost: false,
        failed: None,
    };
    let reaped = match record.reap(&providers) {
        Ok(reaped) => reaped,
        Err(error) => {
            run.failed = Some(failed(error));
            return Ok(run);
        }
    };
    let Reaped {
        sandboxes,
        ended,
        sockets,
        lost,
        failed: unswept,
    } = reaped;
    let ids = |ids: Vec<SandboxId>| -> Vec<String> {
        ids.iter().map(|id| id.as_str().to_owned()).collect()
    };
    run.sandboxes = (sandboxes.into_iter())
        .map(|(name, sandboxes)| (name, ids(sandboxes)))
        .collect();
    run.ended = (ended.into_iter())
        .map(|(name, sandbox)| (name, sandbox.as_str().to_owned()))
        .collect();
    run.sockets = sockets.map(|sockets| sockets.display().to_string());
    run.lost = lost;
    run.failed = unswept.map(failed);
    Ok(run)
}
