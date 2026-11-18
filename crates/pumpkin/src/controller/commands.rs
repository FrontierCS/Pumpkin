//! The commands that a machine whose runs go to a controller makes of it: `run`, `build`,
//! `wait`, `cancel` and `reap`.
//!
//! A run or build follows its job over a link. An ended run's record is copied into this
//! machine's runs directory, where its ending is said. Cancel and reap run over a shell session.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::SystemTime;

use stdx::Context as _;

use super::Reached;
use super::follow::{self, Finished};
use super::linking::Build;
use super::send::{send, tree};
use crate::ending::Printed;
use crate::job::JOBS;
use crate::link::{JobKind, JobsRequest};
use crate::workspace::Workspace;
use crate::{cli, config, next, say};

// The code of a script that finds no job of the run's id on the controller.
const NO_JOB: i32 = 100;

// The code of a script that finds the job there, but no run it started.
const UNSTARTED: i32 = 101;

/// Fails, naming each flag given, as each names something of this machine's that a run on the
/// controller can't use.
fn unsendable(client: &config::Client, flags: &[(&str, bool)]) -> Result<(), String> {
    let given: Vec<&str> = flags
        .iter()
        .filter(|(_, given)| *given)
        .map(|(flag, _)| *flag)
        .collect();
    if given.is_empty() {
        return Ok(());
    }
    Err(format!(
        "{} names this machine's, but its configuration sends runs to controller `{}`: leave it \
         out",
        given.join(" and "),
        client.controller.name
    ))
}

/// Sends the task to the controller as a job that builds and runs it there. Without `--wait`
/// it prints the run's id and the commands that follow and stop it; with, it follows the run
/// as [`wait`] does.
pub(crate) fn run(client: &config::Client, flags: cli::Run) -> Result<ExitCode, String> {
    let cli::Run {
        entry,
        args: cli::TaskArgs(args),
        runs,
        config,
        sandboxd,
        wait: cli::Waiting(wait),
    } = flags;
    unsendable(
        client,
        &[
            ("`--runs`", runs.is_some()),
            ("`--sandboxd`", sandboxd.is_some()),
        ],
    )?;
    let id = runtime::new_id(SystemTime::now())?;
    let name = &client.controller.name;
    let tree = tree(&entry, &args)?;
    // The job refuses a budget that the prices sent with it can't count: refused here, before
    // anything is sent.
    let prices = client.upstreams.prices();
    runtime::budgets(&tree.workflow, &prices).context(client.path.display())?;
    say(format_args!(
        "run {} goes to controller `{name}`",
        id.as_str()
    ));
    let reached = Reached::reach(&client.controller)?;
    let build = send(&reached, client, &id, JobKind::Run, &tree, &args)?;
    let id = id.as_str();
    say(format_args!(
        "run {id} is a job on controller `{name}`, which builds it, then runs it"
    ));
    if let Some(printed) = wait {
        return follow(&reached, &build, id, config.as_deref(), printed);
    }
    crate::advise("follow it", &next::of_run("wait", None, id));
    crate::advise("stop it", &next::of_run("cancel", None, id));
    crate::print(format!("{id}\n").as_bytes())
        .context(format_args!("run {id} is sent, but printing its id failed"))?;
    Ok(ExitCode::SUCCESS)
}

/// Follows the run that `run` names on the controller to its end, copies its record here, and
/// ends as the run ended, having printed what `printed` asks for. A connection that ends
/// meanwhile is opened again, and the log goes on where it stopped.
pub(crate) fn wait(
    client: &config::Client,
    run: &str,
    printed: Printed,
) -> Result<ExitCode, String> {
    let reached = Reached::reach(&client.controller)?;
    let (build, id) = job_id(&reached, run)?;
    follow(&reached, &build, &id, None, printed)
}

/// The id of the job on the controller that `run` names, which may be a prefix of it.
fn job_id(reached: &Reached, run: &str) -> Result<(Build, String), String> {
    let build = Build::of_this("expected to send it to the controller, to find the run there")?;
    let ids = reached.asked(&build, "finding the run", |linked| {
        linked.ask(reached, JobsRequest {})
    })?;
    let name = &reached.name;
    let ids = ids.map_err(|message| format!("controller `{name}` refused: {message}"))?;
    let id = runtime::named(&ids, run, &format!("on controller `{name}`"))?;
    Ok((build, id.to_owned()))
}

fn follow(
    reached: &Reached,
    build: &Build,
    id: &str,
    config: Option<&str>,
    printed: Printed,
) -> Result<ExitCode, String> {
    let name = &reached.name;
    match follow::job(reached, build, id)? {
        Finished::Ran { writer: None } => said_here(reached, id, config, printed),
        Finished::Ran {
            writer: Some(build),
        } => {
            reached.copy(id)?;
            let copy = PathBuf::from(crate::default_runs()?).join(id);
            Err(format!(
                "copied run {id}'s record to {}; it was made by pumpkin {build}: read it with that build",
                copy.display()
            ))
        }
        Finished::Unstarted => Err(format!(
            "run {id}'s job on controller `{name}` ended before the run started, as its log \
             says above: once that is fixed, run the task again as it was sent; to see that log \
             again: {}",
            next::of_run("wait", None, id)
        )),
        Finished::Built(_) | Finished::Unbuilt => Err(format!(
            "job {id} on controller `{name}` is a build: expected a run"
        )),
    }
}

/// Copies the record of run `id`, which has ended, and says its ending from the copy, as `wait`
/// does, after printing what `printed` asks for, ending with the command that runs it again from
/// here.
fn said_here(
    reached: &Reached,
    id: &str,
    config: Option<&str>,
    printed: Printed,
) -> Result<ExitCode, String> {
    reached.copy(id)?;
    let runs = PathBuf::from(crate::default_runs()?);
    let record = runtime::Record::open(&runs.join(id))?;
    let Some(ending) = record.ending()? else {
        return Err(format!(
            "run {id}'s copy, {}, reads as live, though its run has ended: wait for what holds \
             its driver.pid, then follow it again: {}",
            record.dir().display(),
            next::of_run("wait", None, id)
        ));
    };
    crate::end(&record, &ending, printed, None, &here(&record, config)?)
}

/// The command line of a run that went to the controller, for this machine: the task's entry as
/// its path in this machine's repository, whose files the run's tree holds where the repository
/// does, and `config`, if given, in place of the `--config` it had. A record whose command doesn't
/// end with an entry below the run's tree on the controller gives its command as it is.
pub(crate) fn here(record: &runtime::Record, config: Option<&str>) -> Result<Vec<String>, String> {
    let args = record.args();
    let tree = format!("/{}/", record.id());
    let below = args.split_last().and_then(|(entry, flags)| {
        let (_, below) = entry.split_once(&tree)?;
        Some((below, flags))
    });
    let Some((below, flags)) = below else {
        return Ok(args.to_vec());
    };
    let mut words = Vec::new();
    if let Some(config) = config {
        words.extend([
            "--config".to_owned(),
            crate::absolute_utf8(config, "the configuration")?,
        ]);
    }
    let mut flags = flags.iter();
    while let Some(flag) = flags.next() {
        if flag == "--config" {
            flags.next();
        } else {
            words.push(flag.clone());
        }
    }
    let entry = Workspace::find()?.repository().join(below);
    words.push(entry.to_string_lossy().into_owned());
    Ok(words)
}

fn exit(code: i32) -> ExitCode {
    u8::try_from(code).map_or(ExitCode::FAILURE, ExitCode::from)
}

/// Sends the task to the controller as a build job and prints its IR once the images are built.
/// If the connection ends first, the build goes on there.
pub(crate) fn build(client: &config::Client, flags: cli::Build) -> Result<ExitCode, String> {
    let cli::Build {
        entry,
        args: cli::TaskArgs(args),
        config: _,
    } = flags;
    let id = runtime::new_id(SystemTime::now())?;
    let tree = tree(&entry, &args)?;
    let reached = Reached::reach(&client.controller)?;
    let build = send(&reached, client, &id, JobKind::Build, &tree, &args)?;
    let name = &reached.name;
    match follow::job(&reached, &build, id.as_str())? {
        Finished::Built(ir) => {
            crate::print(&ir).context("printing the IR failed")?;
            Ok(ExitCode::SUCCESS)
        }
        Finished::Unbuilt => Err(format!(
            "the build on controller `{name}` failed, as its log says above"
        )),
        Finished::Ran { .. } | Finished::Unstarted => Err(format!(
            "job {} on controller `{name}` is a run: expected a build",
            id.as_str()
        )),
    }
}

/// The script that `body` makes, and the Modal configuration of the client, if it has one, to feed
/// it. The script takes the file into a directory of its own in tmpfs, which its end removes, so
/// that the command reaches the Modal providers of runs whose jobs, which held the file, have
/// ended. `body` is given the flag that names the file there, or an empty one.
fn with_modal(
    client: &config::Client,
    body: impl FnOnce(&str) -> String,
) -> Result<(String, Option<Vec<u8>>), String> {
    let Some(file) = client.modal_file()? else {
        return Ok((body(""), None));
    };
    let script = format!(
        "umask 077\nmodal=$(mktemp --directory \"$XDG_RUNTIME_DIR/pumpkin-modal.XXXXXX\")\n\
         trap 'rm --recursive --force \"$modal\"' EXIT\ncat > \"$modal/modal.toml\"\n{}",
        body(" --modal-config \"$modal/modal.toml\"")
    );
    Ok((script, Some(file)))
}

/// Stops the run that `run` names on the controller, as `pumpkin cancel` does there, `force`
/// included, and says its ending from the copy of its record here. A job whose run hasn't
/// started is stopped and removed.
pub(crate) fn cancel(client: &config::Client, run: &str, force: bool) -> Result<ExitCode, String> {
    let reached = Reached::reach(&client.controller)?;
    let (_, id) = job_id(&reached, run)?;
    let force = if force { " --force" } else { "" };
    let (script, modal) = with_modal(client, |flag| {
        format!(
            "cd\n[ -d {JOBS}/{id} ] || exit {NO_JOB}\n\
             if [ -d {runs}/{id} ]; then\n\
             [ ! -x {JOBS}/{id}/{own} ] || {{ {JOBS}/{id}/{own} controller cancel{force}{flag} {id}; exit; }}\n\
             ./{current} cancel{force}{flag} {id}; exit; fi\n\
             systemctl --user stop pumpkin-job-{id}\n[ -d {runs}/{id} ] || rm --recursive --force {JOBS}/{id}\n\
             exit {UNSTARTED}\n",
            runs = crate::job::runs(),
            own = crate::job::OWN,
            current = crate::job::CURRENT
        )
    })?;
    let again = format!("cancel it again: {}", next::of_run("cancel", None, &id));
    let name = &reached.name;
    match reached.ended(reached.attached(&script, modal.as_deref())?, &again)? {
        NO_JOB => Err(format!("controller `{name}` has no run {id}")),
        UNSTARTED => {
            say(format_args!(
                "run {id}'s job on controller `{name}` is stopped, before the run started"
            ));
            Ok(ExitCode::SUCCESS)
        }
        // `cancel`'s exit says only that the run has ended, however that was, and whether what it
        // left on its provider is gone, as a local one's does.
        code if code == i32::from(crate::job::ENDED) => {
            said_here(&reached, &id, None, Printed::Nothing)?;
            Ok(ExitCode::SUCCESS)
        }
        code if code == i32::from(crate::job::ENDED_UNREACHED) => {
            said_here(&reached, &id, None, Printed::Nothing)?;
            crate::advise(
                "remove what it left once its provider can be reached",
                &next::reap(None),
            );
            Ok(ExitCode::from(crate::UNREACHED))
        }
        code => {
            reached.copy(&id)?;
            Ok(exit(code))
        }
    }
}

/// Removes on the controller what runs left, as `pumpkin reap` does there.
pub(crate) fn reap(client: &config::Client) -> Result<ExitCode, String> {
    let reached = Reached::reach(&client.controller)?;
    let current = crate::job::CURRENT;
    // Before its first job, the controller has no `pumpkin`, and no run to reap.
    let (script, modal) = with_modal(client, |flag| {
        format!(
            "cd\n[ -x {current} ] && [ -d {runs} ] || exit 0\n./{current} reap{flag}\n",
            runs = crate::job::runs()
        )
    })?;
    let again = "reap again: pumpkin reap";
    Ok(exit(reached.ended(
        reached.attached(&script, modal.as_deref())?,
        again,
    )?))
}
