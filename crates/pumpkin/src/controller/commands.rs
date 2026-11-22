//! The commands that a machine makes of its controller when its runs go there: `run`, `wait`,
//! `cancel` and `reap`.
//!
//! A run follows its job over a link. The client copies an ended run's record into this machine's
//! runs directory, and says its ending there.

use std::path::PathBuf;
use std::time::SystemTime;

use retention::KEPT_DAYS;
use stdx::Context as _;

use super::Reached;
use super::follow::{self, Finished};
use super::linking::Build;
use super::send::{send, tree};
use crate::cli::Exit;
use crate::ending::Printed;
use crate::link::{
    Ask, CancelAnswer, CancelRequest, Job, Jobs, JobsRequest, Owner, ReapRequest, Unanswered,
};
use crate::workspace::Workspace;
use crate::{cli, config, next, say};

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
pub(crate) fn run(client: &config::Client, flags: cli::Run) -> Result<Exit, String> {
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
    let mut reached = Reached::reach(&client.controller)?;
    let build = send(&mut reached, client, &id, &tree, &args)?;
    let id = id.as_str();
    say(format_args!(
        "run {id} is a job on controller `{name}`, which builds it, then runs it"
    ));
    if let Some(printed) = wait {
        return follow(&mut reached, &build, id, config.as_deref(), printed);
    }
    crate::advise("follow it", &next::of_run("wait", None, id));
    crate::advise("stop it", &next::of_run("cancel", None, id));
    crate::print(format!("{id}\n").as_bytes())
        .context(format_args!("run {id} is sent, but printing its id failed"))?;
    Ok(Exit::Success)
}

/// Follows the run that `run` names on the controller to its end, copies its record here, and
/// ends as the run ended, having printed what `printed` asks for. A connection that ends
/// meanwhile is opened again, and the log goes on where it stopped.
pub(crate) fn wait(client: &config::Client, run: &str, printed: Printed) -> Result<Exit, String> {
    let mut reached = Reached::reach(&client.controller)?;
    let (build, id) = job_id(&mut reached, run, Among::Every)?;
    follow(&mut reached, &build, &id, None, printed)
}

/// Whose jobs on the controller a run is named among.
#[derive(Clone, Copy)]
enum Among {
    /// Every member's.
    Every,
    /// Those of the member that the link acts for.
    Own,
}

/// The id of the job on the controller that `run` names, which may be a prefix of it, among the
/// jobs that `among` says. Fails, naming its owner and how to name it among every member's, if
/// only another member's job matches.
fn job_id(reached: &mut Reached, run: &str, among: Among) -> Result<(Build, String), String> {
    let build = Build::of_this("expected to send it to the controller, to find the run there")?;
    let jobs = reached.asked(&build, "finding the run", |reached, linked| {
        linked.ask(reached, JobsRequest {})
    })?;
    let name = &reached.name;
    let Jobs { member, jobs } =
        jobs.map_err(|message| format!("controller `{name}` refused: {message}"))?;
    let on = format!("on controller `{name}`");
    // Why a job is another member's: it names another, or one that can't be read, which may be.
    let whose = |job: &Job| match &job.owner {
        Owner::Member { member: owner } => (*owner != member).then(|| format!("is {owner}'s")),
        Owner::Unread { error } => Some(format!("names a member that can't be read: {error}")),
    };
    let ids = |own: bool| -> Vec<String> {
        let jobs = jobs.iter().filter(|job| !own || whose(job).is_none());
        jobs.map(|job| job.id.clone()).collect()
    };
    let every = ids(false);
    let id = match (runtime::named(&every, run, &on), among) {
        (Ok(id), _) => id,
        (Err(error), Among::Every) => return Err(unnamed(&every, run, error)),
        // A prefix that matches several runs, of which one is the member's own, names that one.
        (Err(error), Among::Own) => {
            let own = ids(true);
            let id = runtime::named(&own, run, &on).map_err(|_| unnamed(&every, run, error))?;
            return Ok((build, id.to_owned()));
        }
    };
    if let Among::Own = among
        && let Some(whose) = jobs.iter().find(|job| job.id == id).and_then(whose)
    {
        let any = next::again("cancel", &["--any-member".to_owned()], &[id.to_owned()]);
        return Err(format!("run {id} {on} {whose}: to stop it: {any}"));
    }
    Ok((build, id.to_owned()))
}

/// `error`, which says that `run` names none of the jobs `ids` or several. If `run` names a run
/// whose record the controller may have removed, it adds where the record may be instead.
fn unnamed(ids: &[String], run: &str, error: String) -> String {
    // The controller removes a run's job directory with its record. Such a run has expired.
    let old = retention::jobs::expired(crate::job::started(run), SystemTime::now());
    let none = (ids.iter()).all(|id| runtime::named(std::slice::from_ref(id), run, "").is_err());
    if !old || !none {
        return error;
    }
    let removed = format!(
        "{error}; the controller removes a run's record {KEPT_DAYS} days after its member copied it"
    );
    let copy = crate::default_runs().ok().and_then(|runs| {
        let runs = PathBuf::from(runs);
        let names = runtime::Record::names(&runs).ok()?;
        let id = runtime::named(&names, run, "").ok()?;
        Some(runs.join(id))
    });
    match copy {
        Some(copy) => format!("{removed}: read that copy, here at {}", copy.display()),
        None => format!("{removed}: read that copy"),
    }
}

fn follow(
    reached: &mut Reached,
    build: &Build,
    id: &str,
    config: Option<&str>,
    printed: Printed,
) -> Result<Exit, String> {
    let finished = follow::job(reached, build, id)?;
    let name = &reached.name;
    match finished {
        Finished::Ran { writer } => said_here(reached, build, id, writer, config, printed),
        Finished::Unstarted => Err(format!(
            "run {id}'s job on controller `{name}` ended before the run started, as its log \
             says above: once that is fixed, run the task again as it was sent; to see that log \
             again: {}",
            next::of_run("wait", None, id)
        )),
    }
}

/// Copies the record of run `id`, which has ended, and says its ending from the copy, as `wait`
/// does, after printing what `printed` asks for, ending with the command that runs it again from
/// here.
fn said_here(
    reached: &mut Reached,
    build: &Build,
    id: &str,
    writer: Option<String>,
    config: Option<&str>,
    printed: Printed,
) -> Result<Exit, String> {
    let writer = reached.copy(build, id)?.or(writer);
    let dir = PathBuf::from(crate::default_runs()?).join(id);
    if let Some(writer) = writer {
        return Err(format!(
            "copied run {id}'s record to {}; it was made by pumpkin {writer}: read it with that build",
            dir.display()
        ));
    }
    let record = runtime::Record::open(&dir)?;
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

/// Stops the run that `run` names on the controller and copies its record once it has ended.
/// Unless `any_member`, `run` is named among the member's own runs, as [`Among::Own`] says.
pub(crate) fn cancel(client: &config::Client, run: &str, any_member: bool) -> Result<Exit, String> {
    let mut reached = Reached::reach(&client.controller)?;
    let among = if any_member { Among::Every } else { Among::Own };
    let (build, id) = job_id(&mut reached, run, among)?;
    let modal = client.modal_file()?;
    let request = CancelRequest {
        id: id.clone(),
        any_member,
        modal_bytes: modal.as_ref().map(Vec::len),
    };
    let name = &client.controller.name;
    let answer = reached.asked(&build, "cancelling the run", |reached, linked| {
        linked.ask_with_body(
            reached,
            request.clone(),
            modal.as_deref().unwrap_or_default(),
        )
    })??;
    match answer {
        CancelAnswer::Unstarted => say(format_args!(
            "run {id}'s job on controller `{name}` is stopped, before the run started"
        )),
        CancelAnswer::EndedUnreaped { message } => {
            if let Err(error) = reached.copy(&build, &id) {
                say(error);
            }
            return Err(message);
        }
        CancelAnswer::Stopped { writer } => {
            let writer = reached.copy(&build, &id)?.or(writer);
            let copy = PathBuf::from(crate::default_runs()?).join(&id);
            let writer = writer.map_or_else(
                || "the pumpkin that made it".to_owned(),
                |build| format!("pumpkin {build}"),
            );
            say(format_args!(
                "stopped job {id} on controller `{name}`; copied its record to {}: read it with {writer}",
                copy.display()
            ));
        }
        CancelAnswer::Ended | CancelAnswer::EndedUnreached => {
            said_here(&mut reached, &build, &id, None, None, Printed::Nothing)?;
            if matches!(answer, CancelAnswer::EndedUnreached) {
                crate::advise(
                    "remove what it left once its provider can be reached",
                    &next::reap(None),
                );
                return Ok(Exit::Partial);
            }
        }
    }
    Ok(Exit::Success)
}

/// Removes on the controller what runs left, as `pumpkin reap` does there.
pub(crate) fn reap(client: &config::Client) -> Result<Exit, String> {
    let mut reached = Reached::reach(&client.controller)?;
    let build = Build::of_this("expected to send it to the controller, to reap the runs there")?;
    let modal = client.modal_file()?;
    let request = Ask::Reap(ReapRequest {
        modal_bytes: modal.as_ref().map(Vec::len),
    });
    let report = reached.asked(&build, "reaping the runs", |reached, linked| {
        linked.stream(reached, |link| {
            link.send(&request, modal.as_deref().unwrap_or_default())?;
            link.answer::<ReapRequest>().inspect_err(|error| {
                if matches!(error, Unanswered::Unreached(_)) {
                    say(
                        "the reap's reply was cut: the output may omit ids of runs already cleaned",
                    );
                }
            })
        })
    })??;
    report.report(None, crate::Reaper::Controller)
}
