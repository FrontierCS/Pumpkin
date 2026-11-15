//! `run`, `wait`, `build`, `cancel` and `reap` on the controller, each by a `pumpkin` there.
//!
//! What the controller's commands say is forwarded as they say it, and the record of a run that
//! has ended is copied into this machine's runs directory. A command there that finds no job, or
//! one that ended before its run started, says so by an exit status of its own, which this turns
//! into an error; any other exit status is this command's.
//!
//! Sending a job is [`super::send`]'s, copying a record is [`super::copy`]'s, and `ls` is
//! [`super::listing`]'s.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::SystemTime;

use stdx::Context as _;

use super::send::{send, tree};
use super::{Reached, SSH_FAILED};
use crate::job::{BUILDS, JOBS};
use crate::workspace::Workspace;
use crate::{cli, config, next, say};

/// The exit status of a command on the controller that finds no job of the id it was given.
const NO_JOB: i32 = 100;

/// The exit status of a command on the controller that finds a job that ended before its run
/// started, or its build printed an IR.
const UNSTARTED: i32 = 101;

/// What a command on the controller runs to follow job `id`, below `kind`: shows its unit's
/// standard error, `log`, as it grows, until the job ends or `done`, a test in its directory,
/// holds, then runs `then`. Without `after`, the log goes to standard error, all of it; with
/// it, to standard output, past its first `after` lines, and then [`LOGGED`].
// `tail` follows the log until the loop before it ends, then shows what the log holds by then.
fn following(kind: &str, id: &str, done: &str, after: Option<u64>, then: &str) -> String {
    let (first, to, logged) = match after {
        None => (1, " >&2", ""),
        Some(after) => (after + 1, "", "printf '\\0pumpkin: the log ends\\0\\n'\n"),
    };
    format!(
        r#"cd "$HOME/{kind}/{id}" 2>/dev/null || exit {NO_JOB}
pid=$(systemctl --user show --property=MainPID --value pumpkin-job-{id})
while [ "${{pid:-0}}" -gt 0 ] && kill -0 "$pid" 2>/dev/null && ! {done}; do sleep 1; done &
tail --lines=+{first} --follow --pid=$! log{to} &
wait
{logged}{then}
"#
    )
}

/// What [`following`] prints after the log, on the end of the log's last line, which the job may
/// have left in part: a fixed line between NUL bytes, which a build's output can't plausibly end
/// a line with. What comes after it is not the log's.
const LOGGED: &[u8] = b"\0pumpkin: the log ends\0\n";

/// What a follow of a run's job has shown, over each connection it went over, so that the
/// follow over the next shows no line twice and loses none: of the job's log, which only grows,
/// the whole lines shown, which the next skips ([`following`]'s `after`); of what the run's
/// follower says after it, which it says again from its start, each line once. A line that came
/// in part is shown only once it is whole, so the next connection shows it whole.
#[derive(Default)]
struct Shown {
    /// The whole lines of the log shown.
    log_lines: u64,
    /// Whether [`LOGGED`] has come, over this connection or an earlier one.
    logged: bool,
    /// Whether [`LOGGED`] has come over the current connection.
    past_log: bool,
    /// The lines that the run's follower said.
    said: BTreeSet<Vec<u8>>,
}

impl Shown {
    /// Readies this for what a new connection prints.
    fn again(&mut self) {
        self.past_log = false;
    }

    /// How far this has shown, which changes with each line it shows.
    fn shown(&self) -> (u64, bool, usize) {
        (self.log_lines, self.logged, self.said.len())
    }

    /// Shows `line`, a whole line that [`following`] printed, unless it was shown before.
    fn show(&mut self, line: &[u8]) {
        if self.past_log {
            if self.said.insert(line.to_vec()) {
                crate::to_stderr(line);
            }
            return;
        }
        if let Some(rest) = line.strip_suffix(LOGGED) {
            self.past_log = true;
            // The log's last line, shown in part as the job wrote it.
            if !self.logged && !rest.is_empty() {
                crate::to_stderr(&[rest, b"\n"].concat());
            }
            self.logged = true;
        } else if !self.logged {
            self.log_lines += 1;
            crate::to_stderr(line);
        }
    }
}

/// Refuses `flags` of `run` or `build` that name what this machine's runs would use, but its
/// configuration sends them to the controller.
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

/// `pumpkin run` on the controller: sends the task's repository and starts the job, which
/// builds the task, then runs it with the id minted here, and says so at once; then prints the
/// id, or with `wait` follows the run as [`wait`] does.
pub(crate) fn run(client: &config::Client, flags: cli::Run) -> Result<ExitCode, String> {
    let cli::Run {
        entry,
        args: cli::TaskArgs(args),
        runs,
        config,
        sandboxd,
        wait,
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
    say(format_args!(
        "run {} goes to controller `{name}`",
        id.as_str()
    ));
    let reached = Reached::reach(&client.controller)?;
    let mut words = vec!["run".to_owned()];
    words.extend(crate::arg_flags(&args));
    words.extend(["--".to_owned(), tree.entry.clone()]);
    send(&reached, client, &id, JOBS, &tree, &words)?;
    let id = id.as_str();
    say(format_args!(
        "run {id} is a job on controller `{name}`, which builds it, then runs it"
    ));
    if wait {
        return follow(&reached, id, config.as_deref());
    }
    crate::advise("follow it", &next::of_run("wait", None, id));
    crate::advise("stop it", &next::of_run("cancel", None, id));
    crate::print(format!("{id}\n").as_bytes())
        .context(format_args!("run {id} is sent, but printing its id failed"))?;
    Ok(ExitCode::SUCCESS)
}

/// `pumpkin wait` on the controller, of the run that `run` names there.
pub(crate) fn wait(client: &config::Client, run: &str) -> Result<ExitCode, String> {
    let reached = Reached::reach(&client.controller)?;
    let id = reached.resolve(run)?;
    follow(&reached, &id, None)
}

/// Follows run `id`'s job on `reached` until it has started the run, or ended, then the run,
/// saying its milestones as they come, by the `pumpkin` that the job runs
/// ([`crate::job::follow`]), and copies the run's record here, from which it says the run's
/// ending, which ends with the command that runs it again from here, with the configuration at
/// `config`, if given ([`here`]); returns the exit status that `wait`'s help gives.
fn follow(reached: &Reached, id: &str, config: Option<&str>) -> Result<ExitCode, String> {
    // A job prints its run's id once it has said all it says of the run's start.
    let started = "[ -s out ]";
    // A job that predates its own link is followed by the controller's `wait`, which says the
    // run's ending as the controller would run it again.
    let then = format!(
        "cd\n[ -d {runs}/{id} ] || exit {UNSTARTED}\n\
         [ ! -x {JOBS}/{id}/{own} ] || exec {JOBS}/{id}/{own} controller follow {id} 2>&1\n\
         exec ./{current} wait {id} >&2",
        runs = crate::job::runs(),
        own = crate::job::OWN,
        current = crate::job::CURRENT
    );
    let again = format!("follow it again: {}", next::of_run("wait", None, id));
    let mut attempt = 0;
    let mut shown = Shown::default();
    let code = loop {
        let script = following(JOBS, id, started, Some(shown.log_lines), &then);
        shown.again();
        let before = shown.shown();
        let (attached, rest) = reached.relayed(&script, |line| shown.show(line))?;
        // A connection that showed a line moved the follow on, as `pause` counts attempts.
        if shown.shown() != before {
            attempt = 0;
        }
        let ended = reached.ended_connection(attached.status, &[SSH_FAILED], attached.master);
        let what = format!("following run {id} again from where it was");
        // `rest`, a line cut in part, is shown only once nothing follows ([`Shown`]).
        if ended && reached.pause(attempt, attached.master, &what) {
            attempt += 1;
            continue;
        }
        crate::to_stderr(&rest);
        break reached.ended(attached, &again)?;
    };
    let name = &reached.name;
    match code {
        NO_JOB => Err(format!("controller `{name}` has no run {id}")),
        UNSTARTED => Err(format!(
            "run {id}'s job on controller `{name}` ended before the run started, as its log \
             says above: once that is fixed, run the task again as it was sent; to see that log \
             again: {}",
            next::of_run("wait", None, id)
        )),
        code if code == i32::from(crate::job::ENDED) => said_here(reached, id, config),
        code => {
            reached.copy(id)?;
            Ok(exit(code))
        }
    }
}

/// Copies the record of run `id`, which has ended on `reached`, here, and says its ending from the
/// copy, as `wait` does, ending with the command that runs it again from here, with the
/// configuration at `config`, if given ([`here`]); returns the exit status that `wait`'s help
/// gives.
fn said_here(reached: &Reached, id: &str, config: Option<&str>) -> Result<ExitCode, String> {
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
    crate::end(&record, &ending, None, &here(&record, config)?)
}

/// The arguments of the `pumpkin run` that runs the run of `record` again from here, of a copy of
/// a record that a job on the controller made: the job's, but its configuration, which is this
/// machine's at `config`, if given, else the default one, and its entry, in this workspace's
/// repository, whose files the job's tree, named by the run's id, holds where the repository does.
/// Those of a record that no job made are its own.
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

/// The exit status `code`, of a command on the controller, as this command's.
fn exit(code: i32) -> ExitCode {
    u8::try_from(code).map_or(ExitCode::FAILURE, ExitCode::from)
}

/// `pumpkin build` on the controller: sends the task's repository, starts the job, which builds
/// the task, follows it, and prints the IR it printed.
pub(crate) fn build(client: &config::Client, flags: cli::Build) -> Result<ExitCode, String> {
    let cli::Build {
        entry,
        args: cli::TaskArgs(args),
        config: _,
    } = flags;
    let id = runtime::new_id(SystemTime::now())?;
    let tree = tree(&entry, &args)?;
    let reached = Reached::reach(&client.controller)?;
    let mut words = vec!["build".to_owned()];
    words.extend(crate::arg_flags(&args));
    words.extend(["--".to_owned(), tree.entry.clone()]);
    send(&reached, client, &id, BUILDS, &tree, &words)?;
    let then = format!("[ -s out ] || exit {UNSTARTED}\nexec cat out");
    // Its IR, in `out`, is whole only once the job has ended.
    let following = following(BUILDS, id.as_str(), "false", None, &then);
    let status = reached.attached(&following, None)?;
    let name = &reached.name;
    let again = format!(
        "the build goes on there: its IR is in ~/{BUILDS}/{}/out there once it ends",
        id.as_str()
    );
    match reached.ended(status, &again)? {
        UNSTARTED => Err(format!(
            "the build on controller `{name}` failed, as its log says above"
        )),
        code => Ok(exit(code)),
    }
}

/// The script of a command on the controller that `body` makes of the flag that names Modal's
/// configuration file of `client`'s, if it has one ([`config::Client::modal_file`]), and
/// what the script is fed: that file, which it takes first into a directory of its own in tmpfs,
/// which its end removes, so that the command reaches the Modal providers of runs whose jobs,
/// which held the file, have ended. Without one, the flag is empty, and nothing is fed.
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

/// `pumpkin cancel` on the controller, of the run that `run` names there: stops its job's unit
/// while the run builds, and removes the job's directory, unless the run has a record by then;
/// and cancels the run as the controller's `pumpkin cancel` does once it has started
/// ([`with_modal`]), by the `pumpkin` that its job runs ([`crate::job::cancel`]), then says its
/// ending from its copy here ([`said_here`]).
pub(crate) fn cancel(client: &config::Client, run: &str, force: bool) -> Result<ExitCode, String> {
    let reached = Reached::reach(&client.controller)?;
    let id = reached.resolve(run)?;
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
        // `cancel`'s exit says only that the run has ended, however that was, and whether what
        // it left on its provider is gone, as a local one's does.
        code if code == i32::from(crate::job::ENDED) => {
            said_here(&reached, &id, None)?;
            Ok(ExitCode::SUCCESS)
        }
        code if code == i32::from(crate::job::ENDED_UNREACHED) => {
            said_here(&reached, &id, None)?;
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

/// `pumpkin reap` on the controller, as its own `pumpkin reap` does ([`with_modal`]).
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
