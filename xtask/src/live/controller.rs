//! `cargo xtask live controller`: a controller on this machine's Docker engine, made by
//! `deploy/docker` with `HEAD`'s install, which a new client joins and runs `examples/smoke`
//! through. It needs no cloud and no model. Everything it makes on the engine is of one compose
//! project, [`PROJECT`], which it removes as it ends, and as it starts, of a run that ended early.

use std::fs::Permissions;
use std::net::{Ipv4Addr, TcpListener};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use std::{fs, thread};

use stdx::Deadline;
use stdx::process::{self, Bound, Echo, Environment, Failure, Run};

use super::{
    PUMPKIN_TIMEOUT, checked, copy_tree, followed, head_commit, holding, kill_job, pumpkin,
    task_repository,
};
use crate::repo::STEP_TIMEOUT;
use crate::{BUILD_TIMEOUT, MUSL, Result, ran, root, step, target_dir};

/// The compose project of the controller, which names its container, its volumes and its image.
const PROJECT: &str = "pumpkin-live";

/// The service of `deploy/docker/compose.yaml`.
const SERVICE: &str = "controller";

/// The name of the controller, in its settings, and of the client's member.
const NAME: &str = "live";

/// How long the controller takes to admit a key once it is listed: the members' timer copies the
/// list every minute.
const ADMITTED_BY: Duration = Duration::from_mins(3);

const ADMITTED_EVERY: Duration = Duration::from_secs(5);

/// How long the container's systemd has, from its start, to reach a manager that answers.
const BOOT_TIMEOUT: Duration = Duration::from_mins(1);

const BOOT_POLL: Duration = Duration::from_secs(1);

/// The controller's settings: the Docker engine in its container, and the members' list beside
/// them in the directory that compose mounts.
const SETTINGS: &str = r#"envelope = "1.1"

[deployment]
name = "live"
members = "/etc/pumpkin/conf/members"

[docker]
context = "default"

[providers.local.docker]
"#;

/// Makes the controller, joins it from a new home, and runs the smoke through it; with `lost`,
/// loses a run instead, restarts the controller, and checks that its boot reaped the run. Removes
/// the controller on every path.
pub(crate) fn controller(lost: bool) -> Result<()> {
    let again = if lost {
        "cargo xtask live controller --lost"
    } else {
        "cargo xtask live controller"
    };
    let commit = head_commit()?;
    let repo = task_repository("live/controller/repo")?;
    let scratch = target_dir()?.join("live/controller/scratch");
    if scratch.exists() {
        fs::remove_dir_all(&scratch)
            .map_err(|error| format!("removing {} failed: {error}", scratch.display()))?;
    }
    let deploy = scratch.join("deploy");
    for dir in ["docker", "aws"] {
        copy_tree(&root().join("deploy").join(dir), &deploy.join(dir))?;
    }
    let compose = Compose {
        dir: deploy.join("docker"),
        port: free_port()?,
    };
    compose.down()?;
    eprintln!("xtask: making a controller on Docker with Pumpkin at {commit}");
    let ran = made(&compose, &scratch).and_then(|()| {
        let client = Client::join(&compose, &repo, &scratch)?;
        if lost {
            reaped_at_boot(&compose, &client)
        } else {
            smoke(&compose, &client)
        }
    });
    let removed = compose.down();
    match (ran, removed) {
        (Ok(()), removed) => removed,
        (Err(error), Ok(())) => Err(format!("{error}: fix it, and run `{again}` again").into()),
        (Err(error), Err(left)) => Err(format!(
            "{error}: fix it, and run `{again}` again; removing the controller failed too: {left}"
        )
        .into()),
    }
}

/// The controller's compose project, from the copy of `deploy` in `dir`, whose `sshd` this
/// machine reaches at loopback port `port`.
struct Compose {
    dir: PathBuf,
    port: u16,
}

impl Compose {
    fn command(&self, args: &[&str]) -> Command {
        let mut docker = process::command("docker", Environment::Inherited);
        docker
            .args(["compose", "--project-name", PROJECT])
            .args(args)
            .current_dir(&self.dir)
            .env("PUMPKIN_PORT", format!("127.0.0.1:{}", self.port));
        docker
    }

    fn run(&self, args: &[&str], bound: Duration) -> Result<String> {
        let how = step(bound).named("docker compose");
        let output = ran(&mut self.command(args), how)?;
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// What `args` print, run as root in the controller's container.
    fn exec(&self, args: &[&str]) -> Result<String> {
        self.run(&[&["exec", "-T", SERVICE], args].concat(), STEP_TIMEOUT)
    }

    /// Removes the project, its volumes and its image.
    fn down(&self) -> Result<()> {
        let args = ["down", "--volumes", "--remove-orphans"];
        self.run(&args, STEP_TIMEOUT)?;
        // Compose removes only the images it built.
        let image = format!("{PROJECT}-{SERVICE}");
        let mut listed = process::command("docker", Environment::Inherited);
        listed.args(["image", "ls", "--quiet", &image]);
        let how = || Run::new(Bound::Within(STEP_TIMEOUT), Echo::Silent);
        if ran(&mut listed, how())?.stdout.is_empty() {
            return Ok(());
        }
        let mut removed = process::command("docker", Environment::Inherited);
        removed.args(["image", "rm", &image]);
        ran(&mut removed, how()).map(drop)
    }

    /// Waits until the controller's systemd has started every unit, and fails unless each did.
    fn booted(&self) -> Result<()> {
        let by = Deadline::after(BOOT_TIMEOUT);
        let state = loop {
            let how = Run::new(Bound::Within(STEP_TIMEOUT), Echo::Silent);
            let mut running = self.command(&["exec", "-T", SERVICE]);
            running.args(["systemctl", "is-system-running", "--wait"]);
            let state = process::run(&mut running, how);
            let shown = match &state {
                Ok(output) | Err(Failure::Exited { output, .. }) => {
                    String::from_utf8_lossy(&output.stdout).trim().to_owned()
                }
                Err(_) => String::new(),
            };
            // Before systemd has started its bus, `systemctl` reaches no manager, and prints no
            // state.
            if !shown.is_empty() || by.remaining().is_none() {
                break state;
            }
            thread::sleep(BOOT_POLL);
        };
        match state {
            Ok(_) => Ok(()),
            Err(Failure::Exited { .. }) => {
                let failed = self.exec(&["systemctl", "--failed", "--no-legend"])?;
                let install = self.exec(&["journalctl", "--unit=pumpkin-install", "--no-pager"])?;
                Err(format!(
                    "the controller started with units that failed:\n{failed}\nthe install \
                     said:\n{install}"
                )
                .into())
            }
            Err(failure) => Err(failure.to_string().into()),
        }
    }
}

/// A loopback port that no socket holds now.
fn free_port() -> Result<u16> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    Ok(listener.local_addr()?.port())
}

/// Builds the controller's image with an install of `HEAD` in place of the release, and starts
/// it, with its settings and an empty members' list.
fn made(compose: &Compose, scratch: &Path) -> Result<()> {
    let conf = compose.dir.join("conf");
    fs::create_dir_all(&conf)?;
    // Readable by every user, as each job reads the settings as `pumpkin`, whatever the umask.
    for (file, text) in [("settings.toml", SETTINGS), ("members", "")] {
        fs::write(conf.join(file), text)?;
        fs::set_permissions(conf.join(file), Permissions::from_mode(0o644))?;
    }
    let install = scratch.join("install");
    crate::build(Some(install.clone()), MUSL)?;
    let aws = compose.dir.with_file_name("aws");
    let mut build = process::command("docker", Environment::Inherited);
    build
        .args(["buildx", "build", "--load", "--tag"])
        .arg(format!("{PROJECT}-{SERVICE}"))
        .arg("--build-context")
        .arg(format!("pumpkin={}", install.display()))
        .arg("--build-context")
        .arg(format!("aws={}", aws.display()))
        .arg(&compose.dir);
    ran(&mut build, step(BUILD_TIMEOUT).named("docker buildx build"))?;
    compose.run(&["up", "--detach", "--no-build"], STEP_TIMEOUT)?;
    compose.booted()
}

/// A client in a home of its own, whose key the controller admits, and the task repository it
/// runs from.
struct Client {
    home: PathBuf,
    repo: PathBuf,
}

impl Client {
    /// Joins the controller as a new member does: its first command makes its key and is
    /// refused, and once the key is in the members' list, the controller admits it.
    fn join(compose: &Compose, repo: &Path, scratch: &Path) -> Result<Self> {
        let home = scratch.join("home");
        let config = home.join(".config/pumpkin");
        fs::create_dir_all(&config)?;
        let text = format!(
            "[controller]\nname = \"{NAME}\"\naddress = \"127.0.0.1:{}\"\n",
            compose.port
        );
        fs::write(config.join("pumpkin.toml"), text)?;
        let client = Self {
            home,
            repo: repo.to_owned(),
        };
        let refused = client.ls()?;
        if !refused.contains("refused this machine's key") {
            return Err(format!(
                "`./pumpkin ls` of a client that no list names said: {refused}: expected that \
                 the controller refused its key"
            )
            .into());
        }
        let key = fs::read_to_string(config.join("ssh/key.pub"))?;
        let members = compose.dir.join("conf/members");
        fs::write(&members, format!("{NAME} {key}"))?;
        fs::set_permissions(members, Permissions::from_mode(0o644))?;
        let by = Deadline::after(ADMITTED_BY);
        loop {
            let said = client.ls()?;
            if said.is_empty() {
                println!("xtask: the controller admitted the client's key once listed");
                return Ok(client);
            }
            if by.remaining().is_none() {
                return Err(format!(
                    "the controller still refuses the client's key {} s after it was listed: \
                     {said}",
                    ADMITTED_BY.as_secs()
                )
                .into());
            }
            thread::sleep(ADMITTED_EVERY);
        }
    }

    fn pumpkin(&self, args: &[&str]) -> Command {
        let mut command = pumpkin(&self.repo);
        command.args(args).env("HOME", &self.home);
        command
    }

    /// Nothing once `./pumpkin ls` passes, or what it said as it failed.
    fn ls(&self) -> Result<String> {
        let how = Run::new(Bound::Within(PUMPKIN_TIMEOUT), Echo::Silent).named("./pumpkin ls");
        match process::run(&mut self.pumpkin(&["ls"]), how) {
            Ok(_) => Ok(String::new()),
            Err(Failure::Exited { output, .. }) => {
                Ok(String::from_utf8_lossy(&output.stderr).trim().to_owned())
            }
            Err(failure) => Err(failure.to_string().into()),
        }
    }
}

/// Runs the smoke through the controller, and fails unless every probe passed, every sandbox
/// was deleted and no container of the run is left on the controller's engine.
fn smoke(compose: &Compose, client: &Client) -> Result<()> {
    let run = client.pumpkin(&["run", "--wait", "smoke/task.star"]);
    let (id, status) = followed(run)?;
    let mut failed = Vec::new();
    if !status.success() {
        failed.push(format!(
            "`./pumpkin run` of run {id} ended with {status}, as it said above"
        ));
    }
    let record = client.repo.join(".pumpkin/runs").join(&id);
    failed.extend(checked(&record, false));
    let left = left(compose, &id)?;
    if !left.is_empty() {
        failed.push(format!("left of the run: containers {}", left.join(", ")));
    }
    println!("xtask: run {id}, its record {}", record.display());
    if failed.is_empty() {
        println!(
            "xtask: every probe passed, every sandbox is deleted, and nothing of the run is left"
        );
        return Ok(());
    }
    Err(failed.join("; ").into())
}

/// Loses a run whose sandboxes are in use, by killing its job, makes the controller's container
/// anew with its volumes, as an upgrade does, and fails unless its boot reaped the run: no
/// container of it is left, and `wait` says that it failed as its driver ended without an outcome.
fn reaped_at_boot(compose: &Compose, client: &Client) -> Result<()> {
    let pumpkin = |args: &[&str]| client.pumpkin(args);
    let id = holding(&pumpkin, None)?;
    let alias = format!("[127.0.0.1]:{}", compose.port);
    kill_job(&client.home, "127.0.0.1", Some(compose.port), &alias, &id)?;
    println!("xtask: run {id} is lost: its job was killed; making the controller's container anew");
    let again = ["up", "--detach", "--no-build", "--force-recreate"];
    compose.run(&again, STEP_TIMEOUT)?;
    compose.booted()?;
    let left = left(compose, &id)?;
    let how = Run::new(Bound::Within(PUMPKIN_TIMEOUT), Echo::Silent).named("./pumpkin wait");
    let waited = match process::run(&mut client.pumpkin(&["wait", &id]), how) {
        Ok(output) | Err(Failure::Exited { output, .. }) => output,
        Err(failure) => return Err(failure.to_string().into()),
    };
    let said = String::from_utf8_lossy(&waited.stderr);
    eprint!("{said}");
    let mut failed = Vec::new();
    if !left.is_empty() {
        failed.push(format!(
            "left of run {id} after the boot: containers {}",
            left.join(", ")
        ));
    }
    if !said.contains(&format!(
        "run {id} failed: its driver ended without an outcome"
    )) {
        failed.push(format!(
            "`./pumpkin wait {id}` ended with {} and didn't say that the run failed as its \
             driver ended without an outcome",
            waited.status
        ));
    }
    if failed.is_empty() {
        println!("xtask: the controller's boot reaped lost run {id}, and nothing of it is left");
        return Ok(());
    }
    Err(failed.join("; ").into())
}

/// The containers of run `id` on the controller's engine, running or not.
fn left(compose: &Compose, id: &str) -> Result<Vec<String>> {
    let filter = format!("label=pumpkin.run={id}");
    let listed = compose.exec(&["docker", "ps", "--all", "--quiet", "--filter", &filter])?;
    Ok(listed.lines().map(str::to_owned).collect())
}
