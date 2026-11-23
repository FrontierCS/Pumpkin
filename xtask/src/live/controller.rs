//! `cargo xtask live controller`: a controller on this machine's Docker engine, made by
//! `deploy/docker` with `HEAD`'s install, which a new client joins and runs `examples/smoke`
//! through, and whose `sshd`, members' timer and `serve` keep its members apart. It needs no
//! cloud and no model. Everything it makes on the engine is of one compose
//! project, [`PROJECT`], which it removes as it ends, and as it starts, of a run that ended early.

use std::fmt::Write as _;
use std::fs::Permissions;
use std::net::{Ipv4Addr, TcpListener};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use std::{fs, thread};

use stdx::Deadline;
use stdx::process::{self, Bound, Echo, Environment, Failure, Run, Stream};

use super::{
    PUMPKIN_TIMEOUT, checked, controlled, copy_tree, followed, head_commit, holding, kill_job,
    member_ssh, pumpkin, task_repository,
};
use crate::repo::STEP_TIMEOUT;
use crate::{BUILD_TIMEOUT, MUSL, Result, ran, root, step, target_dir};

/// The compose project of the controller, which names its container, its volumes and its image.
const PROJECT: &str = "pumpkin-live";

/// The service of `deploy/docker/compose.yaml`.
const SERVICE: &str = "controller";

/// The name of the controller, in its settings and in each client's configuration.
const NAME: &str = "live";

/// Another controller that each client's configuration names, which no command reaches, as each
/// names [`NAME`] by `--controller`.
const ELSEWHERE: &str = "elsewhere";

/// The member of the first client.
const FIRST: &str = "first";

/// The member of the second client, whose runs are not the first's.
const OTHER: &str = "other";

const LISTED_EVERY: Duration = Duration::from_secs(5);

/// How long after the members' list changes the controller may still answer a key as before: the
/// members' timer copies the list each minute, within its accuracy of 5 s, and the check asks again
/// each `LISTED_EVERY`.
const LISTED_BY: Duration = Duration::from_secs(60 + 5 + LISTED_EVERY.as_secs());

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

/// Makes the controller and joins it from a new home, which names another controller too. Checks
/// the uptime it says, that it refuses runs that never fit it before building them, and that a run
/// that fits once another ends waits for it. Runs the smoke through it, and checks that it refuses
/// its members a terminal, keeps a second member from cancelling the first's run and refuses a key
/// taken off its list. With `lost`, it loses a run
/// instead, makes the controller's container anew, and checks that its boot reaped the run.
/// Removes the controller on every path.
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
    for dir in ["docker", "controller"] {
        copy_tree(&root().join("deploy").join(dir), &deploy.join(dir))?;
    }
    let compose = Compose {
        dir: deploy.join("docker"),
        port: free_port()?,
    };
    compose.down()?;
    eprintln!("xtask: making a controller on Docker with Pumpkin at {commit}");
    let ran = made(&compose, &scratch).and_then(|(started, booted)| {
        let client = Client::join(&compose, &repo, &scratch, FIRST, &[])?;
        if lost {
            return reaped_at_boot(&compose, &client);
        }
        up_since(&client, started, booted)?;
        never_fits(&compose, &client)?;
        waits_for_room(&compose, &client)?;
        smoke(&compose, &client)?;
        forced(&compose, &client)?;
        let other = Client::join(&compose, &repo, &scratch, OTHER, &[&client])?;
        apart(&compose, &client, &other)?;
        unlisted(&compose, &client, &other)
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

    /// What `args` print, which this captures, as the command's standard error goes to this
    /// process's.
    fn run(&self, args: &[&str], bound: Duration) -> Result<String> {
        let how = Run::new(Bound::Within(bound), Echo::Line).stderr(Stream::Inherited);
        let how = how.named("docker compose");
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
/// it, with its settings and an empty members' list. Returns when it started its container, and
/// when its systemd had started every unit.
fn made(compose: &Compose, scratch: &Path) -> Result<(Instant, Instant)> {
    let conf = compose.dir.join("conf");
    fs::create_dir_all(&conf)?;
    // Readable by every user, as each job reads the settings as `pumpkin`, whatever the umask.
    for (file, text) in [("settings.toml", SETTINGS), ("members", "")] {
        fs::write(conf.join(file), text)?;
        fs::set_permissions(conf.join(file), Permissions::from_mode(0o644))?;
    }
    let install = scratch.join("install");
    crate::build(Some(install.clone()), MUSL)?;
    let controller = compose.dir.with_file_name("controller");
    let mut build = process::command("docker", Environment::Inherited);
    build
        .args(["buildx", "build", "--load", "--tag"])
        .arg(format!("{PROJECT}-{SERVICE}"))
        .arg("--build-context")
        .arg(format!("pumpkin={}", install.display()))
        .arg("--build-context")
        .arg(format!("controller={}", controller.display()))
        .arg(&compose.dir);
    ran(&mut build, step(BUILD_TIMEOUT).named("docker buildx build"))?;
    let started = Instant::now();
    compose.run(&["up", "--detach", "--no-build"], STEP_TIMEOUT)?;
    compose.booted()?;
    Ok((started, Instant::now()))
}

/// A client in a home of its own, whose key the controller admits under `member`, and the task
/// repository it runs from.
struct Client {
    member: &'static str,
    home: PathBuf,
    repo: PathBuf,
}

/// What a `./pumpkin` command came to: whether it passed, and what it said on its standard error.
struct Said {
    passed: bool,
    said: String,
}

impl Client {
    /// Joins the controller as a new member does: its first command makes its key and is
    /// refused, and once the key is in the members' list beside those of `listed`, the controller
    /// admits it.
    fn join(
        compose: &Compose,
        repo: &Path,
        scratch: &Path,
        member: &'static str,
        listed: &[&Self],
    ) -> Result<Self> {
        let home = scratch.join(member);
        let config = home.join(".config/pumpkin");
        fs::create_dir_all(&config)?;
        // Port 1 of loopback, where nothing listens.
        let text = format!(
            "[controllers.{NAME}]\naddress = \"127.0.0.1:{}\"\n\n[controllers.{ELSEWHERE}]\n\
             address = \"127.0.0.1:1\"\n",
            compose.port
        );
        fs::write(config.join("pumpkin.toml"), text)?;
        let client = Self {
            member,
            home,
            repo: repo.to_owned(),
        };
        client.chooses()?;
        let Said { passed, said } = client.ls()?;
        if passed || !said.contains("refused this machine's key") {
            return Err(format!(
                "`./pumpkin ls` of member {member}'s client, which no list names, said: {said}: \
                 expected that the controller refused its key"
            )
            .into());
        }
        let mut members = listed.to_vec();
        members.push(&client);
        list(compose, &members)?;
        client.until(true)?;
        println!("xtask: the controller admitted member {member}'s key once listed");
        Ok(client)
    }

    /// Waits until the controller admits this client's key, if `admitted`, or refuses it, which
    /// the members' list now says, within `LISTED_BY`, and returns what `./pumpkin ls` said then.
    fn until(&self, admitted: bool) -> Result<String> {
        let by = Deadline::after(LISTED_BY);
        loop {
            // An `ls` that starts within the bound counts, however long it takes.
            let late = by.remaining().is_none();
            let Said { passed, said } = self.ls()?;
            let refused = !passed && said.contains("refused this machine's key");
            if (admitted && passed) || (!admitted && refused) {
                return Ok(said);
            }
            if late {
                let expected = if admitted {
                    "admits it once it is listed"
                } else {
                    "refuses it once the list no longer names it"
                };
                return Err(format!(
                    "{} s after the members' list changed, `./pumpkin ls` of member {}'s client \
                     said: {said}: expected that the controller {expected} by then, as its \
                     members' timer copies the list each minute",
                    LISTED_BY.as_secs(),
                    self.member
                )
                .into());
            }
            thread::sleep(LISTED_EVERY);
        }
    }

    /// The member's key, as its `key.pub` holds it.
    fn key(&self) -> Result<String> {
        let key = fs::read_to_string(self.home.join(".config/pumpkin/ssh/key.pub"))?;
        Ok(key.trim_end().to_owned())
    }

    /// `./pumpkin` with `args`, a command and its arguments, of controller [`NAME`].
    fn pumpkin(&self, args: &[&str]) -> Command {
        let mut command = controlled(&self.repo, NAME, args);
        command.env("HOME", &self.home);
        command
    }

    /// Fails unless `./pumpkin ls` without `--controller` refuses, naming both controllers of the
    /// configuration.
    fn chooses(&self) -> Result<()> {
        let mut ls = pumpkin(&self.repo);
        ls.arg("ls").env("HOME", &self.home);
        let how = Run::new(Bound::Within(PUMPKIN_TIMEOUT), Echo::Silent).named("./pumpkin ls");
        let output = match process::run(&mut ls, how) {
            Ok(output) | Err(Failure::Exited { output, .. }) => output,
            Err(failure) => return Err(failure.to_string().into()),
        };
        let said = String::from_utf8_lossy(&output.stderr);
        let names = format!("`{ELSEWHERE}`, `{NAME}`");
        if output.status.success() || !said.contains(&names) {
            return Err(format!(
                "`./pumpkin ls` without `--controller`, of a configuration of two controllers, \
                 ended with {} and said: {}: expected a refusal that names {names}",
                output.status,
                said.trim()
            )
            .into());
        }
        Ok(())
    }

    /// What `./pumpkin` with `args` came to.
    fn said(&self, args: &[&str]) -> Result<Said> {
        let how = Run::new(Bound::Within(PUMPKIN_TIMEOUT), Echo::Silent).named("./pumpkin");
        let output = match process::run(&mut self.pumpkin(args), how) {
            Ok(output) | Err(Failure::Exited { output, .. }) => output,
            Err(failure) => return Err(failure.to_string().into()),
        };
        Ok(Said {
            passed: output.status.success(),
            said: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }

    fn ls(&self) -> Result<Said> {
        self.said(&["ls"])
    }
}

/// Writes the members' list of the controller in `compose`, a line for each of `clients`' keys.
fn list(compose: &Compose, clients: &[&Client]) -> Result<()> {
    let mut lines = String::new();
    for client in clients {
        writeln!(lines, "{} {}", client.member, client.key()?)?;
    }
    let members = compose.dir.join("conf/members");
    fs::write(&members, lines)?;
    // Readable by every user, whatever the umask, as the admin is told to keep it.
    fs::set_permissions(members, Permissions::from_mode(0o644))?;
    Ok(())
}

/// Fails unless the uptime that `ls` says of the controller is within the time since its
/// container `started`, and at least the time since its systemd had `booted`, rather than the
/// host's.
fn up_since(client: &Client, started: Instant, booted: Instant) -> Result<()> {
    let Said { said, .. } = client.ls()?;
    let up = said
        .split_once(&format!("controller `{NAME}`: up "))
        .and_then(|(_, rest)| rest.split_once(','))
        .map(|(up, _)| up);
    // `ls` says a span below an hour as `42s`, `3m07s`, or `3m` for whole minutes.
    let seconds = up.and_then(|up| {
        let (minutes, seconds) = match up.strip_suffix('s') {
            Some(up) => up.split_once('m').unwrap_or(("0", up)),
            None => (up.strip_suffix('m')?, "0"),
        };
        Some(minutes.parse::<u64>().ok()? * 60 + seconds.parse::<u64>().ok()?)
    });
    let (since, at_least) = (started.elapsed().as_secs(), booted.elapsed().as_secs());
    match seconds {
        Some(seconds) if (at_least..=since).contains(&seconds) => {
            println!(
                "xtask: `ls` says the controller is up {seconds} s, since its container started"
            );
            Ok(())
        }
        _ => Err(format!(
            "`./pumpkin ls` said: {said}: expected the controller up for {at_least} to {since} s, \
             since its container started"
        )
        .into()),
    }
}

/// Fails unless the controller refuses a member's `ssh` a terminal, as the key's `restrict` says.
fn forced(compose: &Compose, client: &Client) -> Result<()> {
    let alias = format!("[127.0.0.1]:{}", compose.port);
    let mut ssh = member_ssh(&client.home, "127.0.0.1", Some(compose.port), &alias);
    // `-tt` asks for a terminal even with none here, and ends the session if it gets none.
    ssh.args(["-tt", "true"]);
    let how = Run::new(Bound::Within(STEP_TIMEOUT), Echo::Silent).named("ssh");
    let terminal = match process::run(&mut ssh, how) {
        Ok(output) | Err(Failure::Exited { output, .. }) => output,
        Err(failure) => return Err(failure.to_string().into()),
    };
    let said = String::from_utf8_lossy(&terminal.stderr);
    if terminal.status.success() || !said.contains("PTY allocation request failed") {
        return Err(format!(
            "a member's `ssh -tt` ended with {} and said: {}: expected that the controller \
             refuses it a terminal",
            terminal.status,
            said.trim()
        )
        .into());
    }
    println!("xtask: the controller refuses a member a terminal");
    Ok(())
}

/// Fails unless `other` can't cancel `client`'s run, which then still runs, and `client` can.
fn apart(compose: &Compose, client: &Client, other: &Client) -> Result<()> {
    let pumpkin = |args: &[&str]| client.pumpkin(args);
    let id = holding(&pumpkin, None)?;
    let Said { passed, said } = other.said(&["cancel", &id])?;
    // The client's refusal, from the members that `serve` lists for the controller's jobs.
    let refusal = format!("run {id} on controller `{NAME}` is {}'s", client.member);
    let mut failed = Vec::new();
    if passed || !said.contains(&refusal) {
        failed.push(format!(
            "`./pumpkin cancel {id}` of member {}, whose run it isn't, said: {said}: expected \
             `{refusal}`",
            other.member
        ));
    }
    if left(compose, &id)?.is_empty() {
        failed.push(format!(
            "run {id} has no container after another member's `cancel`: expected it to run on"
        ));
    }
    let Said { said: listed, .. } = client.ls()?;
    let cancelled = client.said(&["cancel", &id])?;
    if !cancelled.passed {
        failed.push(format!(
            "`./pumpkin cancel {id}` of its own member said: {}: expected it to stop the run",
            cancelled.said
        ));
    }
    let left = left(compose, &id)?;
    if !left.is_empty() {
        failed.push(format!(
            "left of run {id} after its member's `cancel`: containers {}",
            left.join(", ")
        ));
    }
    if failed.is_empty() {
        println!(
            "xtask: member {} couldn't cancel member {}'s run {id}, which its own member could",
            other.member, client.member
        );
        return Ok(());
    }
    Err(format!("{}; its member's `ls` said: {listed}", failed.join("; ")).into())
}

/// Takes `client`'s key off the members' list, which keeps `other`'s, and fails unless the
/// controller refuses it again within `LISTED_BY`.
fn unlisted(compose: &Compose, client: &Client, other: &Client) -> Result<()> {
    list(compose, &[other])?;
    client.until(false)?;
    println!(
        "xtask: the controller refused member {}'s key once the list no longer named it",
        client.member
    );
    Ok(())
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
    let record = client.repo.join(".pumpkin/runs").join(NAME).join(&id);
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

/// How long a run that never fits may take to be refused: its tree's upload, its job's start, and
/// the job's connect of the Docker provider and placement of the roles, each a matter of seconds.
const REFUSED_BY: Duration = Duration::from_secs(30);

/// Fails unless the controller refuses, within `REFUSED_BY` and before building anything, each of
/// two variants of `examples/hello` that never fit its machine, though no other run holds a CPU:
/// one whose agent needs a CPU more than the machine has, and one whose agent and counter, which
/// run at once, each need over half of them.
fn never_fits(compose: &Compose, client: &Client) -> Result<()> {
    let online = compose.exec(&["getconf", "_NPROCESSORS_ONLN"])?;
    let cpus: u64 = (online.trim().parse())
        .map_err(|error| format!("`getconf _NPROCESSORS_ONLN` printed `{online}`: {error}"))?;
    let hello = fs::read_to_string(client.repo.join("hello/task.star"))?;
    let needs = r#""needs": {"cpu_millis": 500,"#;
    let needing = |count: usize, cpus: u64| {
        hello.replacen(
            needs,
            &format!(r#""needs": {{"cpu_millis": {},"#, cpus * 1000),
            count,
        )
    };
    let probes = [
        ("wide", needing(1, cpus + 1), "fits no class of sandbox"),
        ("pair", needing(2, cpus / 2 + 1), "need more whole cores"),
    ];
    let images = || compose.exec(&["docker", "image", "ls", "--all", "--quiet"]);
    let before = images()?;
    let mut failed = Vec::new();
    for (name, task, refusal) in probes {
        let entry = format!("hello/{name}.star");
        fs::write(client.repo.join(&entry), task)?;
        let asked = Instant::now();
        let Said { passed, said } = client.said(&["run", "--wait", &entry])?;
        let took = asked.elapsed();
        let named = format!("controller `{NAME}`");
        // The controller user's home holds the job's tree and configuration, which are no member's.
        let internal = said.contains("/home/pumpkin/");
        if passed
            || !said.contains(refusal)
            || !said.contains(&named)
            || internal
            || took > REFUSED_BY
        {
            failed.push(format!(
                "`./pumpkin run --wait {entry}` took {} s and said: {said}: expected a refusal \
                 within {} s that names {named}, says `{refusal}` and no path of the \
                 controller's",
                took.as_secs(),
                REFUSED_BY.as_secs()
            ));
        } else {
            let line = said
                .lines()
                .find(|line| line.contains(refusal))
                .unwrap_or_default();
            println!(
                "xtask: the controller refused {entry}, which never fits it, in {} ms: {line}",
                took.as_millis()
            );
        }
    }
    let after = images()?;
    if after != before {
        failed.push(format!(
            "the controller's engine held images {} before the runs that never fit, and {} \
             after: expected that they built nothing",
            before.split_whitespace().collect::<Vec<_>>().join(", "),
            after.split_whitespace().collect::<Vec<_>>().join(", ")
        ));
    }
    if failed.is_empty() {
        return Ok(());
    }
    Err(failed.join("; ").into())
}

/// Fails unless two runs of a variant of `examples/hello`, which fit the controller's machine one
/// at a time but not together, both finish, one of them once it has waited for the other's cores
/// and said so. The agent of each needs over half the CPUs, and stays a while.
fn waits_for_room(compose: &Compose, client: &Client) -> Result<()> {
    let online = compose.exec(&["getconf", "_NPROCESSORS_ONLN"])?;
    let cpus: u64 = (online.trim().parse())
        .map_err(|error| format!("`getconf _NPROCESSORS_ONLN` printed `{online}`: {error}"))?;
    let hello = fs::read_to_string(client.repo.join("hello/task.star"))?;
    let half = hello
        .replacen(
            r#""needs": {"cpu_millis": 500,"#,
            &format!(r#""needs": {{"cpu_millis": {},"#, (cpus / 2 + 1) * 1000),
            1,
        )
        .replacen(
            "echo hello > greeting",
            "sleep 20\necho hello > greeting",
            1,
        );
    let entry = "hello/half.star";
    fs::write(client.repo.join(entry), half)?;
    let first = client.said(&["run", entry])?;
    let id = (first.said.lines())
        .find_map(|line| {
            line.strip_prefix("pumpkin: run ")?
                .split_once(" goes to controller ")
        })
        .map(|(id, _)| id.to_owned());
    let Some(id) = id.filter(|_| first.passed) else {
        return Err(format!(
            "`./pumpkin run {entry}` said: {}: expected a run's id",
            first.said
        )
        .into());
    };
    let second = client.said(&["run", "--wait", entry])?;
    let waited = client.said(&["wait", &id])?;
    let waits = "waits for room on provider `local`";
    let finished = |said: &Said| said.passed && said.said.contains("finished with a result");
    if finished(&second)
        && finished(&waited)
        && (second.said.contains(waits) || waited.said.contains(waits))
    {
        let line = [&second.said, &waited.said]
            .into_iter()
            .flat_map(|said| said.lines())
            .find(|line| line.contains(waits))
            .unwrap_or_default();
        println!("xtask: of two runs that fit one at a time, one waited for the other: {line}");
        return Ok(());
    }
    Err(format!(
        "of two runs of {entry}, which fit one at a time, the second said: {}; and the first: {}: \
         expected that both finished with a result, one once it had said that it `{waits}`",
        second.said, waited.said
    )
    .into())
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
