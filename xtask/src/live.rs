//! The checks of `cargo xtask live`, which reach a real environment: Docker, or a real provider
//! through the controller that the configuration names.

use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

use stdx::Context as _;
use stdx::process::{self, Bound, Echo, Environment, Failure, Run, Stream};

use crate::repo::{Repo, STEP_TIMEOUT};
use crate::settings::{Provider, Settings};
use crate::{
    BUILD_TIMEOUT, Placed, Result, install_file, root, sandboxd, star, step, subcommand,
    target_dir, tools, wait,
};

/// The bound of a `./pumpkin` command that xtask runs to its end. pumpkin bounds each of its own
/// steps. This bound outlasts their sum: the start of a stopped controller (10 min), the
/// reconnects after it, and the send of the build and the task.
const PUMPKIN_TIMEOUT: Duration = Duration::from_hours(1);

/// How many lines the smoke's probe prints for each sandbox, each of which must say `ok`.
const CHECKS_PER_SANDBOX: usize = 6;

/// The Modal SDK that the check of a run's sandboxes there uses: the one the provider's helper
/// pins (`providers/modal/helper/pumpkin-modal.py`).
const MODAL_SDK: &str = "modal==1.6.3";

/// Prints the id of each sandbox on Modal that carries the tag of run `sys.argv[1]` in Pumpkin's
/// app, as the provider's helper finds a run's sandboxes, and nothing if there is no app.
const MODAL_LIST: &str = r#"
import sys
import modal

try:
    app = modal.App.lookup("pumpkin-images")
except modal.exception.NotFoundError:
    sys.exit(0)
for sandbox in modal.Sandbox.list(app_id=app.app_id, tags={"pumpkin-run": sys.argv[1]}):
    print(sandbox.object_id)
"#;

/// Runs the tests that need a Docker engine, with the static `sandboxd` they run in a sandbox.
pub(crate) fn docker() -> Result<()> {
    star::link()?;
    let built = sandboxd()?;
    // A sandbox runs under its role's UID. Give it the installed mode even when the
    // contributor's umask makes Cargo's own artifacts executable only by their owner.
    let install = root().join("bin/live");
    fs::create_dir_all(&install).context(format_args!("making {} failed", install.display()))?;
    let sandboxd = install::File::Sandboxd.name();
    let sandboxd = install_file(&install, sandboxd, Placed::Copy(&built, 0o755))?;
    let boot = install::File::Ec2Boot.name();
    install_file(
        &install,
        boot,
        Placed::Copy(&built.with_file_name(boot), 0o755),
    )?;
    let test = [
        "nextest",
        "run",
        "--workspace",
        "--locked",
        "--profile",
        "live",
        "--run-ignored",
        "only",
    ];
    let mut test = subcommand(tools::NEXTEST, &test)?;
    test
        // These poison the variables the Docker provider must ignore: passed on to `docker`, they
        // would steer it to an engine that doesn't exist, or to TLS without certificates, so the
        // Docker tests fail unless the provider clears `docker`'s environment.
        .env("DOCKER_HOST", "unix:///nonexistent/pumpkin-poison.sock")
        .env("DOCKER_CONTEXT", "pumpkin-poison-no-such-context")
        .env("DOCKER_TLS_VERIFY", "1")
        .env("PUMPKIN_SANDBOXD", &sandboxd)
        .env_remove("UPDATE_EXPECT");
    let how = step(BUILD_TIMEOUT).named("cargo nextest run");
    match process::run(&mut test, how) {
        Ok(_) => Ok(()),
        Err(failure @ Failure::Exited { .. }) => {
            Err(format!("{failure}, and run `cargo xtask live docker` again").into())
        }
        Err(failure) => Err(failure.to_string().into()),
    }
}

/// Runs `examples/smoke` at `HEAD` on `provider` through the controller, and fails unless every
/// probe passed, every sandbox was deleted and nothing of the run is left on the provider. With
/// `lost`, it loses the run instead, and checks that its sandboxes end by themselves and that a
/// reap removes the rest. `again` names the command that repeats it.
pub(crate) fn smoke(provider: Provider, gpu_model: Option<&str>, lost: bool) -> Result<()> {
    let again = match provider {
        Provider::Ec2 => "cargo xtask live ec2",
        Provider::Modal => "cargo xtask live modal",
    };
    let home =
        env::var_os("HOME").ok_or("HOME is unset: set it, as the configuration is below it")?;
    let config = Path::new(&home).join(".config/pumpkin/pumpkin.toml");
    let text = fs::read_to_string(&config).map_err(|error| {
        format!(
            "{} can't be read ({error}): write the configuration that names the controller \
             there, as the README says",
            config.display()
        )
    })?;
    let settings = Settings::parse(&text, &config, provider, gpu_model)?;
    reached(&settings)?;
    let commit = head_commit()?;
    let repo = task_repository()?;
    if lost {
        return self::lost(&settings, &repo, again);
    }
    let mut run = pumpkin(&repo);
    run.args(["run", "--wait"]);
    if let Some(modal) = &settings.modal {
        run.args(["--arg", &format!("gpu_model={}", modal.gpu_model)]);
    }
    run.arg("smoke/task.star");
    eprintln!("xtask: running examples/smoke at {commit} through the controller");
    let (id, status) = followed(run)?;
    let mut failed = Vec::new();
    if !status.success() {
        failed.push(format!(
            "`./pumpkin run` of run {id} ended with {status}, as it said above"
        ));
    }
    let record = repo.join(".pumpkin/runs").join(&id);
    failed.extend(checked(&record, settings.modal.is_some()));
    let left = left(&settings, &id)?;
    if !left.is_empty() {
        failed.push(format!("left after the run: {}", left.join(", ")));
        let mut cancel = pumpkin(&repo);
        cancel.args(["cancel", &id]);
        let cancelled = process::run(&mut cancel, step(PUMPKIN_TIMEOUT).named("./pumpkin cancel"));
        match cancelled {
            Ok(_) => eprintln!("xtask: `./pumpkin cancel {id}` passed"),
            Err(failure) => eprintln!("xtask: {failure}"),
        }
        let still = self::left(&settings, &id)?;
        if !still.is_empty() {
            failed.push(format!("left after `pumpkin cancel`: {}", still.join(", ")));
        }
    }
    println!("xtask: run {id}, its record {}", record.display());
    if failed.is_empty() {
        println!(
            "xtask: every probe passed, every sandbox is deleted, and nothing of the run is left"
        );
        return Ok(());
    }
    Err(format!("{}: fix it, and run `{again}` again", failed.join("; ")).into())
}

/// The longest a lost smoke run's sandboxes may take to end by themselves, from when its job is
/// killed, after the run's start.
// The run's hard end is at most 94 minutes after its start: three of an EC2 sandbox's startups (29
// minutes each, the agent's twice and the final call's target's), the agent's deadline, which the
// check sets to 2, and the final call's timeout, 5. Then an EC2 timer fires within 2 minutes, as it
// is set for the minute after, and an instance takes up to 10 to be terminated.
const LOST_BY: Duration = Duration::from_mins(94 + 2 + 10);

/// How often a lost run's sandboxes are looked for.
const LOST_EVERY: Duration = Duration::from_mins(1);

fn lost(settings: &Settings, repo: &Path, again: &str) -> Result<()> {
    let pumpkin = |args: &[&str]| {
        let mut command = pumpkin(repo);
        command.args(args);
        command
    };
    // The agent holds a lease of each grant, so that the run is lost with its sandboxes in use.
    let mut args = vec!["run", "--arg", "hold=true", "--arg", "deadline_seconds=120"];
    let gpu = settings
        .modal
        .as_ref()
        .map(|modal| format!("gpu_model={}", modal.gpu_model));
    if let Some(gpu) = &gpu {
        args.extend(["--arg", gpu]);
    }
    args.push("smoke/task.star");
    let how = Run::new(Bound::Within(PUMPKIN_TIMEOUT), Echo::Silent)
        .stderr(Stream::Inherited)
        .named("./pumpkin run");
    let ran = process::run(&mut pumpkin(&args), how).map_err(|failure| failure.to_string())?;
    let id = String::from_utf8_lossy(&ran.stdout).trim().to_owned();
    if id.is_empty() {
        return Err(
            "`./pumpkin run` printed no run id: expected the id of the run it started".into(),
        );
    }
    let leases = if gpu.is_some() { 2 } else { 1 };
    held(pumpkin(&["wait", &id]), &id, leases)?;
    kill_job(settings, repo, &id)?;
    let killed = Instant::now();
    println!("xtask: run {id} is lost: its job was killed");
    let before = ended_by_themselves(settings, repo, &id, killed)?;
    if before.is_empty() {
        println!(
            "xtask: the providers hold nothing of run {id} before the reap, so this check doesn't \
             show that a reap removes a lost run's timers"
        );
    } else {
        println!(
            "xtask: before the reap, the providers hold {} of run {id}",
            before.join(", ")
        );
    }
    let how = Run::new(Bound::Within(PUMPKIN_TIMEOUT), Echo::Silent)
        .stdout(Stream::Inherited)
        .named("./pumpkin reap");
    let reaped = match process::run(&mut pumpkin(&["reap"]), how) {
        Ok(output) | Err(Failure::Exited { output, .. }) => output,
        Err(failure) => return Err(failure.to_string().into()),
    };
    let said = String::from_utf8_lossy(&reaped.stderr);
    eprint!("{said}");
    // `reap` reaps every run on the controller, and fails if any one can't be reaped: only a
    // failure that names other runs alone isn't this check's.
    let failed = !reaped.status.success() && !wait::others_failed(&said, &id);
    let left = left(settings, &id)?;
    println!(
        "xtask: `./pumpkin reap` ended with {}; left of run {id}: {}",
        reaped.status,
        if left.is_empty() {
            "nothing".to_owned()
        } else {
            left.join(", ")
        }
    );
    if !failed && left.is_empty() {
        return Ok(());
    }
    Err(format!("the reap of lost run {id} failed, or left something, as said above: fix it, and run `{again} --lost` again").into())
}

/// Waits until no sandbox of run `id` is left, looking every `LOST_EVERY`, and returns what else of
/// it the providers hold, its schedules. Fails if a sandbox is left `LOST_BY` after `killed`.
fn ended_by_themselves(
    settings: &Settings,
    repo: &Path,
    id: &str,
    killed: Instant,
) -> Result<Vec<String>> {
    loop {
        let left = left(settings, id)?;
        let sandboxes: Vec<&String> = left
            .iter()
            .filter(|left| !left.starts_with("schedule "))
            .collect();
        if sandboxes.is_empty() {
            println!(
                "xtask: every sandbox of run {id} ended by itself, by {} s after its job was killed",
                killed.elapsed().as_secs(),
            );
            return Ok(left);
        }
        if killed.elapsed() > LOST_BY {
            return Err(format!(
                "{} s after its job was killed, run {id} still has {}: remove them with `./pumpkin cancel {id}` in {}, and find out why they outlived the run's hard end",
                killed.elapsed().as_secs(),
                left.join(", "),
                repo.display()
            )
            .into());
        }
        thread::sleep(LOST_EVERY);
    }
}

/// Follows `wait`, a `./pumpkin wait` of run `id`, until it has said that `leases` leases are
/// ready, and stops it.
fn held(mut wait: Command, id: &str, leases: usize) -> Result<()> {
    let (mut wait, stderr) = started(&mut wait)?;
    let mut ready = 0;
    for line in BufReader::new(stderr).lines() {
        let line = line.map_err(|error| format!("reading `./pumpkin wait` failed: {error}"))?;
        eprintln!("{line}");
        if wait::lease_ready(&line) {
            ready += 1;
            if ready == leases {
                break;
            }
        }
    }
    let _ = wait.kill();
    let _ = wait.wait();
    if ready < leases {
        return Err(format!(
            "run {id} ended before its {leases} leases were ready, as it said above"
        )
        .into());
    }
    Ok(())
}

/// Kills the unit of job `id` on the controller to leave a lost run, with the client's key and
/// pinned host key for that controller.
fn kill_job(settings: &Settings, repo: &Path, id: &str) -> Result<()> {
    let shared = repo
        .join(".pumpkin/controller")
        .join(format!("{}.{}", settings.name, settings.region));
    let found = fs::read_to_string(shared.join("found"))
        .map_err(|error| format!("reading where the controller was found failed: {error}"))?;
    let found: serde_json::Value = serde_json::from_str(&found)?;
    let Some(address) = found["address"].as_str() else {
        return Err(format!("the controller's `found` names no address: {found}").into());
    };
    let host = format!("pumpkin@{address}");
    let kill = format!("systemctl --user kill --signal=KILL pumpkin-job-{id}");
    let home =
        std::env::var_os("HOME").ok_or("HOME is unset: expected the client's SSH key under it")?;
    let kept = PathBuf::from(home).join(".config/pumpkin/ssh");
    let known_hosts = format!("UserKnownHostsFile={}", kept.join("known_hosts").display());
    let alias = format!("HostKeyAlias={}.{}", settings.name, settings.region);
    let mut ssh = process::command("ssh", Environment::Inherited);
    ssh.args(["-F", "/dev/null", "-i"])
        .arg(kept.join("key"))
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            &known_hosts,
            "-o",
            &alias,
            &host,
            &kill,
        ]);
    process::run(&mut ssh, step(STEP_TIMEOUT).named("ssh"))
        .map_err(|failure| failure.to_string())?;
    Ok(())
}

/// Fails, saying what to give, unless the credentials that the smoke's checks use reach AWS,
/// `EventBridge` Scheduler's list of schedules and, for Modal, the profile's workspace.
fn reached(settings: &Settings) -> Result<()> {
    let credentials = "give `aws` the credentials of the controller's account, as `aws \
                       configure` does, and run this again";
    listed(
        &mut aws(settings, &["sts", "get-caller-identity"]),
        Some(credentials),
    )?;
    // Credentials that can't list schedules are refused here, before a run, not at its end.
    let policy = "grant these credentials `scheduler:ListSchedules` on `*`, and run this again";
    schedules(settings, "no-such-run", Some(policy))?;
    if let Some(modal) = &settings.modal {
        let token = format!(
            "give Modal's profile `{}` a token, as `uvx --from {MODAL_SDK} modal token new \
             --profile {}` does, and run this again",
            modal.profile, modal.profile
        );
        modal_python(modal, MODAL_LIST, &["no-such-run"], Some(&token))?;
    }
    Ok(())
}

/// The message of `failure`. If `expected` is given and the command exited with a status other
/// than 0, the message says to do `expected`. It then leaves out what the failure says to fix.
fn refused(failure: &Failure, expected: Option<&str>) -> String {
    match (failure, expected) {
        (Failure::Exited { name, output, .. }, Some(expected)) => {
            let status = output.status;
            match failure.reported() {
                reported if reported.is_empty() => {
                    format!("`{name}` failed with {status}: {expected}")
                }
                reported => format!("`{name}` failed with {status}: {reported}: {expected}"),
            }
        }
        (failure, _) => failure.to_string(),
    }
}

/// The commit that a live run builds and tests, `HEAD`. Fails if a tracked file differs from it.
fn head_commit() -> Result<String> {
    let repo = Repo { root: root() };
    let commit = repo.git(["rev-parse", "HEAD"])?;
    let changed = repo.git(["status", "--porcelain", "--untracked-files=no"])?;
    if !changed.is_empty() {
        return Err(format!(
            "the working tree has changes that `HEAD` doesn't hold, and a live run builds `HEAD`: \
             commit them, or stash them:\n{changed}"
        )
        .into());
    }
    Ok(commit)
}

/// The task repository that the smoke runs in, `live/repo` in cargo's target directory, which
/// keeps its `.git` and `.pumpkin` from one live run to the next. The executable is installed
/// from this checkout so an unpublished change can be exercised.
fn task_repository() -> Result<PathBuf> {
    let repo = target_dir()?.join("live/repo");
    let made =
        |path: &Path, error: std::io::Error| format!("writing {} failed: {error}", path.display());
    fs::create_dir_all(&repo).map_err(|error| made(&repo, error))?;
    if !repo.join(".git").exists() {
        Repo { root: &repo }.git(["init", "--quiet"])?;
    }
    // The last run's copy goes, so that no file that `examples` has since lost is left behind.
    let read = |error: std::io::Error| format!("reading {} failed: {error}", repo.display());
    for entry in fs::read_dir(&repo).map_err(read)? {
        let entry = entry.map_err(read)?;
        if entry.file_name() == ".git" || entry.file_name() == ".pumpkin" {
            continue;
        }
        let path = entry.path();
        let removed = if path.is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        removed.map_err(|error| format!("removing {} failed: {error}", path.display()))?;
    }
    copy_tree(&root().join("examples"), &repo)?;
    let workspace = repo.join(".pumpkin");
    crate::build(Some(workspace.clone()), crate::MUSL)?;
    let ignore = workspace.join(".gitignore");
    fs::write(&ignore, "*\n").map_err(|error| made(&ignore, error))?;
    let entry = repo.join("pumpkin");
    std::os::unix::fs::symlink(".pumpkin/bin/pumpkin", &entry)
        .map_err(|error| made(&entry, error))?;
    Ok(repo)
}

fn pumpkin(repo: &Path) -> Command {
    let mut command = process::command(repo.join("pumpkin"), Environment::Cargo);
    command.current_dir(repo).env_remove("PUMPKIN_WORKSPACE");
    command
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to).map_err(|error| format!("making {} failed: {error}", to.display()))?;
    let entries = fs::read_dir(from)
        .map_err(|error| format!("reading {} failed: {error}", from.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("reading {} failed: {error}", from.display()))?;
        let (from, to) = (entry.path(), to.join(entry.file_name()));
        if entry.path().is_dir() {
            copy_tree(&from, &to)?;
        } else {
            fs::copy(&from, &to)
                .map_err(|error| format!("copying {} failed: {error}", from.display()))?;
        }
    }
    Ok(())
}

/// Runs `run`, echoing what it says, and returns the status it ends with and the id of the run it
/// started, which its first words name.
fn followed(mut run: Command) -> Result<(String, ExitStatus)> {
    let (mut child, stderr) = started(&mut run)?;
    let mut id = None;
    for line in BufReader::new(stderr).lines() {
        let line =
            line.map_err(|error| format!("reading what `./pumpkin` said failed: {error}"))?;
        eprintln!("{line}");
        if id.is_none()
            && let Some(rest) = line.strip_prefix("pumpkin: run ")
            && let Some((run, _)) = rest.split_once(" goes to controller ")
        {
            id = Some(run.to_owned());
        }
    }
    let status = child
        .wait()
        .map_err(|error| format!("waiting for `./pumpkin` failed: {error}"))?;
    match id {
        Some(id) => Ok((id, status)),
        None => Err(format!(
            "`./pumpkin run` ended with {status} before it started a run, as it said above: fix \
             that, and run this again"
        )
        .into()),
    }
}

/// Starts `pumpkin`, a `./pumpkin` command that the caller follows. It returns the child and the
/// pipe of its standard error.
fn started(pumpkin: &mut Command) -> Result<(Child, ChildStderr)> {
    let mut child = process::spawn(pumpkin, Stdio::inherit(), Stdio::inherit(), Stdio::piped())?;
    let Some(stderr) = child.stderr.take() else {
        unreachable!("the child's standard error is missing, though it starts piped")
    };
    Ok((child, stderr))
}

/// What is wrong in the record of a smoke run at `record`: a run that didn't finish, a probe that
/// failed or is missing, and a sandbox that isn't deleted. `gpu` is whether the run has a GPU
/// runner.
fn checked(record: &Path, gpu: bool) -> Vec<String> {
    let read = |path: &Path| -> std::result::Result<serde_json::Value, String> {
        let text = fs::read_to_string(path)
            .map_err(|error| format!("reading {} failed: {error}", path.display()))?;
        serde_json::from_str(&text)
            .map_err(|error| format!("{} isn't JSON: {error}", path.display()))
    };
    let mut failed = Vec::new();
    // An outcome that can't be read still leaves the sandboxes to check.
    let outcome = read(&record.join("outcome.json")).unwrap_or_else(|error| {
        failed.push(error);
        serde_json::Value::Null
    });
    if outcome["state"] != "finished" {
        failed.push(format!("the run didn't finish: {}", outcome["state"]));
    }
    let values = outcome["verdict"]["result"]["values"].as_array();
    let detail = (values.into_iter().flatten())
        .find(|value| value["name"] == "detail")
        .and_then(|detail| detail["value"].as_array())
        .cloned()
        .unwrap_or_default();
    let sandboxes = if gpu { 4 } else { 3 };
    for line in &detail {
        let line = line.as_str().unwrap_or_default();
        println!("probe: {line}");
        if !line.contains(": ok (") {
            failed.push(format!("a probe failed: {line}"));
        }
    }
    if detail.len() != sandboxes * CHECKS_PER_SANDBOX {
        failed.push(format!(
            "the final call returned {} probe lines: expected {} for {sandboxes} sandboxes",
            detail.len(),
            sandboxes * CHECKS_PER_SANDBOX
        ));
    }
    let mut dirs = vec![record.join("agents/agent")];
    for kind in ["jobs", "leases"] {
        if let Ok(entries) = fs::read_dir(record.join(kind)) {
            dirs.extend(entries.flatten().map(|entry| entry.path()));
        }
    }
    for dir in dirs {
        let sandbox = match read(&dir.join("sandbox.json")) {
            Ok(sandbox) => sandbox,
            Err(error) => {
                failed.push(error);
                continue;
            }
        };
        let deleted = sandbox["delete"]["state"].as_str().unwrap_or("not deleted");
        println!(
            "sandbox {} on {}: {deleted}",
            sandbox["id"], sandbox["provider"]
        );
        if deleted != "deleted" {
            failed.push(format!("sandbox {} is {deleted}", sandbox["id"]));
        }
    }
    failed
}

/// What of run `id` the providers still hold: an instance not terminated, a schedule, a sandbox.
fn left(settings: &Settings, id: &str) -> Result<Vec<String>> {
    let mut left = Vec::new();
    let filter = format!("Name=tag:pumpkin:run,Values={id}");
    let mut described = aws(
        settings,
        &["ec2", "describe-instances", "--filters", &filter],
    );
    let described = listed(&mut described, None)?;
    let reservations = described["Reservations"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for instance in reservations.iter().flat_map(|reservation| {
        reservation["Instances"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }) {
        let state = instance["State"]["Name"].as_str().unwrap_or_default();
        println!(
            "EC2 instance {} of the run: {state}",
            instance["InstanceId"]
        );
        if state != "terminated" {
            left.push(format!("EC2 instance {} ({state})", instance["InstanceId"]));
        }
    }
    left.extend(schedules(settings, id, None)?);
    if let Some(modal) = &settings.modal {
        let listed = modal_python(modal, MODAL_LIST, &[id], None)?;
        left.extend(
            listed
                .lines()
                .map(|sandbox| format!("Modal sandbox {sandbox}")),
        );
    }
    Ok(left)
}

/// The schedules of `EventBridge` Scheduler whose names start with `id`, each as
/// `schedule <group>/<name>`.
fn schedules(settings: &Settings, id: &str, expected: Option<&str>) -> Result<Vec<String>> {
    let list = ["scheduler", "list-schedules", "--name-prefix", id];
    let listed = listed(&mut aws(settings, &list), expected)?;
    let listed = listed["Schedules"].as_array().cloned().unwrap_or_default();
    let named = listed
        .iter()
        .map(|schedule| format!("schedule {}/{}", schedule["GroupName"], schedule["Name"]));
    Ok(named.collect())
}

/// An `aws` with `args`, in the controller's region, which prints JSON.
// Its global options follow the command, so that a failure names the command.
fn aws(settings: &Settings, args: &[&str]) -> Command {
    let mut aws = process::command("aws", Environment::Inherited);
    aws.args(args).args([
        "--no-cli-pager",
        "--output",
        "json",
        "--region",
        &settings.region,
    ]);
    aws
}

/// What `command` printed, as JSON. If the command fails, the error says what [`refused`] says.
fn listed(command: &mut Command, expected: Option<&str>) -> Result<serde_json::Value> {
    let how = Run::new(Bound::Within(STEP_TIMEOUT), Echo::Silent);
    let output = process::run(command, how).map_err(|failure| refused(&failure, expected))?;
    let shown = process::shown(command);
    let json = serde_json::from_slice(&output.stdout);
    Ok(json.context(format_args!("`{shown}` printed what isn't JSON"))?)
}

/// What `script` printed, run with `args` in a Python that has the Modal SDK, under the profile
/// of `modal`.
fn modal_python(
    modal: &crate::settings::Modal,
    script: &str,
    args: &[&str],
    expected: Option<&str>,
) -> Result<String> {
    let mut python = process::command("uv", Environment::Inherited);
    python
        .args(["run", "--quiet", "--no-project", "--with", MODAL_SDK])
        .args(["python", "-I", "-c", script])
        .args(args)
        .env("MODAL_PROFILE", &modal.profile);
    let how = Run::new(Bound::Within(STEP_TIMEOUT), Echo::Silent).named("uv run python");
    let output = process::run(&mut python, how).map_err(|failure| refused(&failure, expected))?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
