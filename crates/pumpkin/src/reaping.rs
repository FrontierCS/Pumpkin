//! Stopping runs, and removing what a run that ended or was lost left on its providers.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

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

use crate::ending::{Printed, end};
use crate::workspace::Workspace;
use crate::{
    Reader, advise, cli, config, controller, counted, default_config, found, modal_helper, next,
    print, runs_dir, say,
};

/// Stops the run, waits for it to end, and reaps it, as `pumpkin cancel` does.
pub(crate) fn cancel(flags: cli::CancelRun) -> Result<ExitCode, String> {
    let cli::CancelRun {
        cancel: flags,
        any_member,
    } = flags;
    if let (cli::RunName::Prefix(prefix), None) = (&flags.run, &flags.runs)
        && let Some(client) = config::client(None)?
    {
        return controller::cancel(&client, prefix, any_member);
    }
    let Cancelled {
        record,
        named,
        reached,
    } = cancelled(flags, Reader::Operator)?;
    let reached = reached?;
    let named = named.as_deref();
    // `cancel`'s exit says only that the run has ended, however that was, and whether what it
    // left on its provider is gone.
    let ending = record.wait(|| {})?;
    end(&record, &ending, Printed::Nothing, named, record.args())?;
    if !reached {
        advise(
            "remove what it left once its provider can be reached",
            &next::reap(named),
        );
        return Ok(ExitCode::from(UNREACHED));
    }
    Ok(ExitCode::SUCCESS)
}

pub(crate) struct Cancelled {
    record: Record,
    named: Option<String>,
    /// Whether every provider could be reached, or the error from reaping the ended run.
    pub(crate) reached: Result<bool, String>,
}

/// Signals the run's driver, if it has one, and waits for the run to end before reaping it.
pub(crate) fn cancelled(flags: cli::Cancel, reader: Reader) -> Result<Cancelled, String> {
    let cli::Cancel {
        run,
        runs,
        modal_config,
    } = flags;
    let (record, named) = found(run, runs)?;
    let (id, dir) = (record.id(), record.dir().display());
    if let Some(pid) = record.driver()? {
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
        // A driver that SIGTERM stops deletes the run's sandboxes before it writes the outcome.
        // That takes a while on a provider whose sandboxes are instances.
        let until = "it ends once its sandboxes are deleted";
        match reader {
            Reader::Operator => say(format_args!(
                "stopping run {id}, recorded in {dir}: {until}"
            )),
            Reader::Follower => say(format_args!("stopping run {id}: {until}")),
        }
    }
    // Its provider is known for certain only once it has ended: a driver yet to start the run
    // has yet to say which it is.
    record.wait(|| {})?;
    let reaping = &mut Reaching::new(modal_config);
    let reached = reaped(&record, reaping, named.as_deref(), Reaper::Command)
        .map(|Swept { left: _, reached }| reached);
    Ok(Cancelled {
        record,
        named,
        reached,
    })
}

/// The exit code of a command that could not reach a provider to remove what a run left on it.
pub(crate) const UNREACHED: u8 = 3;

struct Swept {
    /// Whether the run had left anything, now removed.
    left: bool,
    /// Whether its provider was reached, if it had one: if not, what it left there may remain.
    reached: bool,
}

/// Who reaps, which decides the runs that a reap takes, and what it makes of a provider of
/// theirs that it can't reach.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reaper {
    /// `pumpkin reap` or `pumpkin cancel`: every run that has ended or is lost; a provider that
    /// can't be reached makes the exit status [`UNREACHED`], and the reap says how to reap again
    /// once it can be.
    Command,
    /// A client's request: the ended or lost runs this build wrote.
    Controller,
    /// The controller's boot: runs this build wrote that may have left something, not one that
    /// ended clean ([`Record::ended_clean`]); a provider that it can't reach, as Modal, whose
    /// credentials reach the controller only with a job, it says leaves what the run left
    /// there to end by the run's hard end, and goes on.
    Boot,
}

/// Removes what each run in the runs directory that `flags` name left once it ended or was lost
/// ([`Record::reap`]), of the runs that `reaper` takes, printing the id of each that had left
/// something; leaves each live run to its driver. The exit status is `reap`'s, but 0 for
/// [`Reaper::Boot`] though a provider can't be reached.
pub(crate) fn reap(flags: cli::Reap, reaper: Reaper) -> Result<ExitCode, String> {
    let cli::Reap { runs, modal_config } = flags;
    if reaper == Reaper::Command
        && runs.is_none()
        && let Some(client) = config::client(None)?
    {
        return controller::reap(&client);
    }
    let (runs, named) = runs_dir(runs)?;
    let named = named.as_deref();
    let report = sweep(Path::new(&runs), modal_config, named, reaper)?;
    report.report(named, reaper)
}

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct ReapedRuns {
    ids: Vec<String>,
    unreaped: usize,
    unreached: usize,
    others: BTreeMap<String, Vec<String>>,
}

pub(crate) fn sweep(
    runs: &Path,
    modal_config: Option<PathBuf>,
    named: Option<&str>,
    reaper: Reaper,
) -> Result<ReapedRuns, String> {
    let mut reaching = Reaching::new(modal_config);
    let mut report = ReapedRuns::default();
    if reaper != Reaper::Command && !runs.try_exists().context(runs.display())? {
        return Ok(report);
    }
    for id in Record::names(runs)? {
        let dir = runs.join(&id);
        let mut swept = || -> Result<Option<Swept>, String> {
            let path = dir.join("record.json");
            if !path.try_exists().context(path.display())? {
                return Ok(None);
            }
            if reaper != Reaper::Command {
                let other = match runtime::writer(&dir)? {
                    Writer::This => None,
                    Writer::Other(build) => Some(format!("pumpkin {build}")),
                    Writer::Older => Some("an older pumpkin".to_owned()),
                };
                if let Some(build) = other {
                    if reaper == Reaper::Controller {
                        report.others.entry(build).or_default().push(id.clone());
                    }
                    return Ok(None);
                }
            }
            let record = Record::open(&dir)?;
            if record.driver()?.is_some() || reaper == Reaper::Boot && record.ended_clean()? {
                return Ok(None);
            }
            reaped(&record, &mut reaching, named, reaper).map(Some)
        };
        match swept() {
            Ok(Some(Swept { left, reached })) => {
                if left {
                    if reaper == Reaper::Controller {
                        report.ids.push(id);
                    } else {
                        printed_id(&id)?;
                    }
                }
                if !reached {
                    report.unreached += 1;
                }
            }
            Ok(None) => {}
            Err(error) => {
                say(error);
                report.unreaped += 1;
            }
        }
    }
    Ok(report)
}

fn printed_id(id: &str) -> Result<(), String> {
    print(format!("{id}\n").as_bytes()).context("printing a reaped run's id failed")
}

impl ReapedRuns {
    pub(crate) fn report(self, named: Option<&str>, reaper: Reaper) -> Result<ExitCode, String> {
        let Self {
            ids,
            unreaped,
            unreached,
            others,
        } = self;
        for id in ids {
            printed_id(&id)?;
        }
        for (writer, ids) in others {
            say(format_args!(
                "to reap runs made by {writer}, use that build: {}",
                ids.join(", ")
            ));
        }
        if unreaped > 0 {
            let runs = counted(unreaped, "run");
            say(format_args!(
                "{runs} can't be reaped, as said above: fix what each says, and reap again"
            ));
            advise("reap again", &next::reap(named));
            return Ok(ExitCode::FAILURE);
        }
        if unreached > 0 && reaper != Reaper::Boot {
            let runs = counted(unreached, "run");
            say(format_args!(
                "{runs} ran on a provider that can't be reached, as said above: what each left \
             there may remain"
            ));
            advise("reap again once it can be reached", &next::reap(named));
            return Ok(ExitCode::from(UNREACHED));
        }
        Ok(ExitCode::SUCCESS)
    }
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
/// `provider.json` names, as `reaching` reaches it ([`Record::reap`]), and says what was left.
/// Of a provider that can't be reached, it says so, and, as `reaper` has it, how to reap `runs`
/// again, as commands name it, or that what the run left there ends by its hard end; and removes
/// the rest.
fn reaped(
    record: &Record,
    reaching: &mut Reaching,
    runs: Option<&str>,
    reaper: Reaper,
) -> Result<Swept, String> {
    let id = record.id();
    let failed = |error| format!("run {id} can't be reaped: {error}");
    let reports = record.provider_reports().map_err(failed)?;
    let mut reachable = Vec::new();
    for (name, report) in &reports {
        reachable.push((name, reaching.reach(record, name, report).map_err(failed)?));
    }
    let mut providers = Vec::new();
    for (name, reachable) in reachable {
        let reached = reachable.and_then(|reachable| match &reaching.reached[&reachable] {
            Ok(sweep) => Ok(sweep.as_ref()),
            Err(error) => Err(error.clone()),
        });
        match reached {
            Ok(sweep) => providers.push((name, sweep)),
            // Not the reap's to fail: a record's provider may be gone for good, as on a machine
            // set up again since, and a failure would fail every reap after it.
            Err(error) => match reaper {
                Reaper::Command | Reaper::Controller => say(format_args!(
                    "run {id}'s sandboxes on provider `{name}`, if it left any, can't be \
                     removed, as it can't be reached: {error}; once it can be, `{}` removes \
                     them, and if it is gone for good, remove the record, {}, to stop reaping it",
                    next::reap(runs),
                    record.dir().display()
                )),
                Reaper::Boot => say(format_args!(
                    "run {id}'s sandboxes on provider `{name}`, if it left any, end by the run's \
                     hard end, as the boot can't reach it: {error}"
                )),
            },
        }
    }
    let reached = providers.len() == reports.len();
    reap_rest(record, &providers).map(|left| Swept { left, reached })
}

/// Reaps the run of `record` on `providers`, each by its name, and says what was left, now
/// removed; returns whether anything was left.
fn reap_rest(record: &Record, providers: &[(&Name, &dyn Sweep)]) -> Result<bool, String> {
    let id = record.id();
    let failed = |error| format!("run {id} can't be reaped: {error}");
    let reap = record.reap(providers);
    let Reaped {
        sandboxes,
        ended,
        sockets,
        lost,
        failed: unswept,
    } = reap.map_err(failed)?;
    let mut left = sockets.is_some() || lost || !ended.is_empty();
    for (name, ids) in &sandboxes {
        if ids.is_empty() {
            continue;
        }
        left = true;
        let ids: Vec<&str> = ids.iter().map(SandboxId::as_str).collect();
        let sandboxes = counted(ids.len(), "sandbox");
        say(format_args!(
            "run {id} left {sandboxes} on provider `{name}`, now removed: {}",
            ids.join(", ")
        ));
    }
    for (name, sandbox) in &ended {
        say(format_args!(
            "run {id}'s sandbox {} on provider `{name}` had ended, as it isn't listed there now: \
             what it left there is now removed",
            sandbox.as_str()
        ));
    }
    if let Some(sockets) = &sockets {
        say(format_args!(
            "run {id} left its socket directory, now removed: {}",
            sockets.display()
        ));
    }
    if lost {
        say(format_args!(
            "run {id} was lost, as its driver ended without an outcome.json: it is now recorded \
             as failed"
        ));
    }
    if let Some(error) = unswept {
        return Err(failed(error));
    }
    Ok(left)
}
