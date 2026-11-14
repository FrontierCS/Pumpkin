//! `cargo xtask live`, the checks that reach real environments, which `ci` leaves out: `docker`,
//! the tests that need a Docker engine, the static `sandboxd` or a server elsewhere, which are
//! ignored otherwise; and `ec2` and `modal`, a run of `examples/smoke` on that provider through
//! the person's controller, at the commit `HEAD` names, which checks its record and that the
//! provider holds nothing of the run's afterwards.

use std::fs::{self, Permissions};
use std::io::{BufRead as _, BufReader};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};
use std::{env, thread};

use stdx::process::Environment;

use crate::settings::{Provider, Settings};
use crate::{Cargo, Result, root, sandboxd, tested, tools, wait};

/// How many lines the smoke's probe prints for each sandbox, each of which must say `ok`.
const CHECKS_PER_SANDBOX: usize = 5;

/// The Modal SDK that the check of a run's sandboxes there uses: the one the provider's helper
/// pins (`providers/modal/helper/pumpkin-modal.py`).
const MODAL_SDK: &str = "modal==1.6.3";

/// Prints the ID of each sandbox on Modal that carries the tag of run `sys.argv[1]` in
/// Pumpkin's App, as the provider's helper finds a run's sandboxes.
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

/// Runs the tests that `ci` ignores: those that need Docker, through the Docker context
/// `PUMPKIN_DOCKER_CONTEXT` names, or `default`, which they inherit, with Docker's own variables
/// set to reach no engine, and the static `sandboxd` built and named in `PUMPKIN_SANDBOXD`.
pub(crate) fn docker(cargo: &Cargo) -> Result<()> {
    tools::require(cargo, tools::NEXTEST)?;
    let sandboxd = sandboxd(cargo)?;
    let test = cargo.cmd(&[
        "nextest",
        "run",
        "--workspace",
        "--locked",
        "--run-ignored",
        "only",
    ]);
    let test = tested(cargo, test)?;
    // These poison the variables the Docker provider must ignore: passed on to `docker`, they
    // would steer it to an engine that doesn't exist, or to TLS without certificates, so the
    // Docker tests fail unless the provider clears `docker`'s environment.
    let test = test
        .env("DOCKER_HOST", "unix:///nonexistent/pumpkin-poison.sock")
        .env("DOCKER_CONTEXT", "pumpkin-poison-no-such-context")
        .env("DOCKER_TLS_VERIFY", "1");
    let test = test
        .env("PUMPKIN_SANDBOXD", &sandboxd)
        .env_remove("UPDATE_EXPECT");
    let line = test.to_string();
    let status = Command::from(test).status();
    let status = status.map_err(|error| format!("running `{line}` failed: {error}"))?;
    if status.success() {
        return Ok(());
    }
    Err(format!(
        "`{line}` ended with {status}: fix what it printed above, and run `cargo xtask live \
         docker` again"
    )
    .into())
}

/// Runs `examples/smoke` on `provider` through the controller that this machine's configuration,
/// `~/.config/pumpkin/pumpkin.toml`, names, with Pumpkin at `HEAD`, from a task
/// repository of its own below the target directory, whose `./pumpkin` builds that commit from
/// this checkout; then checks that the run scored every probe, that its record has every
/// sandbox deleted, and that EC2, `EventBridge` Scheduler and Modal hold nothing of the run, and
/// removes what they do, through `pumpkin cancel`.
pub(crate) fn smoke(
    cargo: &Cargo,
    provider: Provider,
    gpu_model: Option<&str>,
    lost: bool,
) -> Result<()> {
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
    reached(cargo, &settings)?;
    let (branch, commit) = head(cargo)?;
    let repo = task_repository(cargo, &branch, &commit)?;
    if lost {
        return self::lost(cargo, &settings, &repo, again);
    }
    let mut run = stdx::process::command(repo.join("pumpkin"), Environment::Inherited);
    for var in stdx::process::cargo_package_vars() {
        run.env_remove(var);
    }
    run.current_dir(&repo)
        .arg(format!("--pumpkin-repository=file://{}", root().display()))
        .args(["run", "--wait"]);
    if let Some(modal) = &settings.modal {
        run.args(["--arg", &format!("gpu_model={}", modal.gpu_model)]);
    }
    run.arg("smoke/task.star");
    eprintln!("xtask: running examples/smoke at {commit} ({branch}) through the controller");
    let (id, status) = followed(run)?;
    let mut failed = Vec::new();
    if !status.success() {
        failed.push(format!(
            "`./pumpkin run` of run {id} ended with {status}, as it said above"
        ));
    }
    let record = repo.join(".pumpkin/runs").join(&id);
    failed.extend(checked(&record, settings.modal.is_some()));
    let left = left(cargo, &settings, &id)?;
    if !left.is_empty() {
        failed.push(format!("left after the run: {}", left.join(", ")));
        let mut cancel = stdx::process::command(repo.join("pumpkin"), Environment::Inherited);
        cancel.current_dir(&repo).args(["cancel", &id]);
        let cancelled = cancel
            .status()
            .map_err(|error| format!("`pumpkin cancel` failed: {error}"))?;
        eprintln!("xtask: `pumpkin cancel {id}` ended with {cancelled}");
        let still = self::left(cargo, &settings, &id)?;
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
// The run's hard end is at most 94 minutes after its start: three of an EC2 sandbox's startups
// (29 minutes each, the agent's twice and the final judge's), the agent's deadline, which the
// check sets to 2, and the score's timeout, 5. Then an EC2 timer fires within 2 minutes, as it is
// set for the minute after, and an instance takes up to 10 to be terminated.
const LOST_BY: Duration = Duration::from_mins(94 + 2 + 10);

/// How often a lost run's sandboxes are looked for, as they end by themselves.
const LOST_EVERY: Duration = Duration::from_mins(1);

/// Starts the smoke in `repo` with the agent holding a lease of each grant, loses it once they are
/// ready, by killing its job's unit on the controller that `settings` name, so that nothing
/// deletes its sandboxes; then waits until the providers hold none, by
/// [`LOST_BY`], and checks that `pumpkin reap` removes what is left of it, the timers that fired
/// included, and that nothing of it is left.
fn lost(cargo: &Cargo, settings: &Settings, repo: &Path, again: &str) -> Result<()> {
    let pumpkin = |args: &[&str]| {
        let mut command = stdx::process::command(repo.join("pumpkin"), Environment::Inherited);
        for var in stdx::process::cargo_package_vars() {
            command.env_remove(var);
        }
        command
            .current_dir(repo)
            .arg(format!("--pumpkin-repository=file://{}", root().display()));
        command.args(args);
        command
    };
    let mut args = vec!["run", "--arg", "hold=true", "--arg", "deadline_seconds=120"];
    let gpu = settings
        .modal
        .as_ref()
        .map(|modal| format!("gpu_model={}", modal.gpu_model));
    if let Some(gpu) = &gpu {
        args.extend(["--arg", gpu]);
    }
    args.push("smoke/task.star");
    let ran = pumpkin(&args).stderr(Stdio::inherit()).output();
    let ran = ran.map_err(|error| format!("running `./pumpkin run` failed: {error}"))?;
    let id = String::from_utf8_lossy(&ran.stdout).trim().to_owned();
    if !ran.status.success() || id.is_empty() {
        return Err(format!(
            "`./pumpkin run` ended with {}, as it said above",
            ran.status
        )
        .into());
    }
    let leases = if gpu.is_some() { 2 } else { 1 };
    held(pumpkin(&["wait", &id]), &id, leases)?;
    kill_job(cargo, settings, repo, &id)?;
    let killed = Instant::now();
    println!("xtask: run {id} is lost: its job was killed");
    let before = ended_by_themselves(cargo, settings, repo, &id, killed)?;
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
    // `reap` reaps every run on the controller, and fails if any one can't be reaped: only a
    // failure that names other runs alone isn't this check's.
    let reaped = pumpkin(&["reap"])
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped())
        .output();
    let reaped = reaped.map_err(|error| format!("`./pumpkin reap` failed: {error}"))?;
    let said = String::from_utf8_lossy(&reaped.stderr);
    eprint!("{said}");
    let failed = !reaped.status.success() && !wait::others_failed(&said, &id);
    let left = left(cargo, settings, &id)?;
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

/// Waits until the providers hold no sandbox of lost run `id`, whose job was killed at `killed`,
/// by [`LOST_BY`], saying when; returns what they held of it then, its timers; fails, saying what
/// is left, after that.
fn ended_by_themselves(
    cargo: &Cargo,
    settings: &Settings,
    repo: &Path,
    id: &str,
    killed: Instant,
) -> Result<Vec<String>> {
    loop {
        let left = left(cargo, settings, id)?;
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

/// Runs `wait`, `./pumpkin wait` of run `id`, showing what it says, until it has said that
/// `leases` leases are ready, then ends it; fails if it ends first.
fn held(mut wait: Command, id: &str, leases: usize) -> Result<()> {
    let wait = wait.stderr(Stdio::piped()).spawn();
    let mut wait = wait.map_err(|error| format!("running `./pumpkin wait` failed: {error}"))?;
    let Some(stderr) = wait.stderr.take() else {
        unreachable!("the child's standard error is missing, though it is piped just above")
    };
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

/// Kills the unit of job `id` on the controller that `settings` name, over the shared connection
/// that `./pumpkin` in `repo` opened there, so that its run is lost.
fn kill_job(cargo: &Cargo, settings: &Settings, repo: &Path, id: &str) -> Result<()> {
    let shared = repo
        .join(".pumpkin/controller")
        .join(format!("{}.{}", settings.name, settings.region));
    let instance = fs::read_to_string(shared.join("lock"))
        .map_err(|error| format!("reading the controller's instance failed: {error}"))?;
    let host = format!("pumpkin@{}", instance.trim());
    let kill = format!("systemctl --user kill --signal=KILL pumpkin-job-{id}");
    let _dir = cargo.sh.push_dir(&shared);
    let ssh = cargo.sh.cmd("ssh");
    ssh.args(["-F", "/dev/null", "-o", "ControlPath=0", &host, &kill])
        .run()?;
    Ok(())
}

/// Fails unless `aws`, and for Modal its SDK through `uv`, answer to the credentials this
/// machine has, and `aws` may list `EventBridge` Scheduler's schedules, saying how to give them.
fn reached(cargo: &Cargo, settings: &Settings) -> Result<()> {
    let aws = cargo
        .sh
        .cmd("aws")
        .args(["--no-cli-pager", "--region", &settings.region]);
    let identity = aws
        .args(["sts", "get-caller-identity"])
        .quiet()
        .ignore_stdout()
        .run();
    identity.map_err(|error| {
        format!(
            "{error}: give `aws` the credentials of the controller's account, as `aws configure` \
             does, and run this again"
        )
    })?;
    // A client whose policy can't list schedules is refused here, before a run, not at its end.
    schedules(cargo, settings, "no-such-run").map_err(|error| {
        format!(
            "{error}: give the client the policy that `deploy/aws` makes, which lets it list \
             `EventBridge` Scheduler's schedules, by applying it again, and run this again"
        )
    })?;
    if let Some(modal) = &settings.modal {
        let token = modal_python(cargo, modal, MODAL_LIST, &["no-such-run"]);
        token.map_err(|error| {
            format!(
                "{error}: give Modal's profile `{}` a token, as `uvx --from {MODAL_SDK} modal \
                 token new --profile {}` does, and run this again",
                modal.profile, modal.profile
            )
        })?;
    }
    Ok(())
}

/// `HEAD`'s branch and commit, which the smoke builds Pumpkin from; fails if `HEAD` is detached
/// or the working tree has changes to tracked files, which the commit doesn't hold.
fn head(cargo: &Cargo) -> Result<(String, String)> {
    let git = |args: &[&str]| cargo.sh.cmd("git").args(args).read();
    let branch = git(&["symbolic-ref", "--quiet", "--short", "HEAD"])
        .map_err(|_| "HEAD is detached: check out a branch that names the commit to check")?;
    let commit = git(&["rev-parse", "HEAD"])?;
    let changed = git(&["status", "--porcelain", "--untracked-files=no"])?;
    if !changed.is_empty() {
        return Err(format!(
            "the working tree has changes that `HEAD` doesn't hold, and a live run builds `HEAD`: \
             commit them, or stash them:\n{changed}"
        )
        .into());
    }
    Ok((branch, commit))
}

/// The smoke's task repository, `live/repo` in cargo's target directory, made if need be, which
/// keeps its `.pumpkin` from one live run to the next: `examples/smoke` as `smoke`, the examples'
/// root, the consumer entry as `./pumpkin`, and `.pumpkinversion` naming `branch` and `commit`.
fn task_repository(cargo: &Cargo, branch: &str, commit: &str) -> Result<PathBuf> {
    let metadata = cargo
        .cmd(&["metadata", "--format-version=1", "--no-deps"])
        .read()?;
    let metadata: serde_json::Value = serde_json::from_str(&metadata)?;
    let Some(target) = metadata["target_directory"].as_str() else {
        return Err("`cargo metadata` named no target directory: expected one".into());
    };
    let repo = Path::new(target).join("live/repo");
    let made =
        |path: &Path, error: std::io::Error| format!("writing {} failed: {error}", path.display());
    fs::create_dir_all(&repo).map_err(|error| made(&repo, error))?;
    if !repo.join(".git").exists() {
        cargo
            .sh
            .cmd("git")
            .args(["init", "--quiet"])
            .arg(&repo)
            .run()?;
    }
    let smoke = repo.join("smoke");
    if smoke.exists() {
        fs::remove_dir_all(&smoke).map_err(|error| made(&smoke, error))?;
    }
    copy_tree(&root().join("examples/smoke"), &smoke)?;
    let marker = repo.join("PUMPKIN.star");
    fs::copy(root().join("examples/PUMPKIN.star"), &marker)
        .map_err(|error| made(&marker, error))?;
    let entry = repo.join("pumpkin");
    fs::copy(root().join("deploy/pumpkin"), &entry).map_err(|error| made(&entry, error))?;
    fs::set_permissions(&entry, Permissions::from_mode(0o755))
        .map_err(|error| made(&entry, error))?;
    let version = repo.join(".pumpkinversion");
    fs::write(&version, format!("{branch} {commit}\n")).map_err(|error| made(&version, error))?;
    Ok(repo)
}

/// Copies the files below `from` to `to`, made with every directory between.
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

/// Runs `run`, `pumpkin run --wait`, showing what it says as it says it, and returns the id of
/// the run it started, as it said it, and how it ended; fails if it said none.
fn followed(mut run: Command) -> Result<(String, ExitStatus)> {
    let mut child = run
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("running `./pumpkin` failed: {error}"))?;
    let Some(stderr) = child.stderr.take() else {
        unreachable!("the child's standard error is missing, though it is piped just above")
    };
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

/// What is wrong with `record`, the copy of the smoke run's record: each probe line that isn't
/// `ok`, too few of them for the sandboxes probed, which are 3, or 4 with a GPU runner, or a
/// sandbox that isn't recorded as deleted, or a file of these that can't be read. Prints the
/// probes' lines and the sandboxes.
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
    let detail = outcome["verdict"]["result"]["returned"]["detail"]
        .as_array()
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
            "the judge returned {} probe lines: expected {} for {sandboxes} sandboxes",
            detail.len(),
            sandboxes * CHECKS_PER_SANDBOX
        ));
    }
    let mut dirs = vec![record.join("agent")];
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

/// What the providers hold of run `id`: each EC2 instance tagged with it that isn't terminated,
/// each `EventBridge` Scheduler schedule named for it, and, for Modal, each sandbox tagged with
/// it, each said with what it is.
fn left(cargo: &Cargo, settings: &Settings, id: &str) -> Result<Vec<String>> {
    let aws = |args: &[&str]| -> Result<serde_json::Value> {
        let cmd = cargo.sh.cmd("aws").args([
            "--no-cli-pager",
            "--output",
            "json",
            "--region",
            &settings.region,
        ]);
        Ok(serde_json::from_str(&cmd.args(args).read()?)?)
    };
    let mut left = Vec::new();
    let filter = format!("Name=tag:pumpkin:run,Values={id}");
    let described = aws(&["ec2", "describe-instances", "--filters", &filter])?;
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
    left.extend(schedules(cargo, settings, id)?);
    if let Some(modal) = &settings.modal {
        let listed = modal_python(cargo, modal, MODAL_LIST, &[id])?;
        left.extend(
            listed
                .lines()
                .map(|sandbox| format!("Modal sandbox {sandbox}")),
        );
    }
    Ok(left)
}

/// Each `EventBridge` Scheduler schedule named for run `id`, said with its group.
fn schedules(cargo: &Cargo, settings: &Settings, id: &str) -> Result<Vec<String>> {
    let list = cargo
        .sh
        .cmd("aws")
        .args(["--no-cli-pager", "--output", "json", "--region"]);
    let list =
        list.arg(&settings.region)
            .args(["scheduler", "list-schedules", "--name-prefix", id]);
    let listed: serde_json::Value = serde_json::from_str(&list.read()?)?;
    let listed = listed["Schedules"].as_array().cloned().unwrap_or_default();
    let named = listed
        .iter()
        .map(|schedule| format!("schedule {}/{}", schedule["GroupName"], schedule["Name"]));
    Ok(named.collect())
}

/// What `script` prints, run by Python with [`MODAL_SDK`], through `uv`, given `args`, with the
/// profile and configuration file of `modal`.
fn modal_python(
    cargo: &Cargo,
    modal: &crate::settings::Modal,
    script: &str,
    args: &[&str],
) -> Result<String> {
    let python = cargo
        .sh
        .cmd("uv")
        .args(["run", "--quiet", "--no-project", "--with", MODAL_SDK]);
    let python = python.args(["python", "-I", "-c", script]).args(args);
    let python = python.env("MODAL_PROFILE", &modal.profile);
    let python = match &modal.config {
        Some(config) => python.env("MODAL_CONFIG_PATH", config),
        None => python,
    };
    Ok(python.quiet().read()?)
}
