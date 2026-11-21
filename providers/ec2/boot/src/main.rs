//! Boots a sandbox on its instance: reads and removes the `Start` in [`START_FILE`], locks the
//! instance's network down, pulls the role's image and runs its container with `sandboxd` in it,
//! until the container exits, then exits itself, with a failure if the boot failed. Whatever
//! happens, it reports on its standard output, which its unit connects to the serial console,
//! each line behind [`CONSOLE_PREFIX`] and none holding the grant. The `Start`'s deadlines say
//! what bounds it. Its files are at their paths below the directory it runs in, which its unit
//! makes `/`.

#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "its console lines are its standard output, and a command line it can't run is \
              reported on its standard error"
)]

use std::ffi::OsString;
use std::fs::{self, DirBuilder, OpenOptions, Permissions};
use std::io::Write as _;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{self, Path, PathBuf};
use std::process::{ExitCode, Output};
use std::time::Duration;
use std::{env, fmt};

use schema::ec2_boot::{CONSOLE_PREFIX, GRANT_DIR, SANDBOXD_IN_IMAGE, START_FILE, Start};
use schema::sandbox::{RegistrationGrant, SANDBOXCTL_PATH, SANDBOXD_GRANT, SANDBOXD_PATH};
use stdx::process::Environment;
use stdx::{Context as _, Deadline};

const GRANT_FILE: &str = "grant.env";

// Where the instance keeps `sandboxd`, copied out of its image, to be mounted read-only.
const HOST_DIR: &str = "opt/pumpkin";

const CONTAINER: &str = "pumpkin-sandbox";

// The container's DNS server is its own loopback, where nothing answers: on Docker's default
// bridge, the address goes into the container's `/etc/resolv.conf` with no resolver of Docker's
// between, so no lookup leaves the container, and one fails at once rather than waiting on an
// address the rules drop:
// https://docs.docker.com/engine/network/drivers/bridge/
const DNS: Ipv4Addr = Ipv4Addr::LOCALHOST;

// How many of the container's last lines the console gets.
const LAST_LINES: &str = "20";

// Each command of the end asks a local daemon one thing, in well under a second; the unit's
// poweroff follows whatever this bound cuts short.
const END_TIMEOUT: Duration = Duration::from_mins(1);

fn main() -> ExitCode {
    // `stdx::process` reads each command's output on threads of its own.
    stdx::abort_on_panic();
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    if args != ["start"] {
        eprintln!(
            "ec2-boot: the command line is {args:?}: expected `ec2-boot start`, with a start in \
             {START_FILE}"
        );
        return ExitCode::from(2);
    }
    // `start` sets it as soon as the container runs, so `end` shows its last lines even if
    // `start` fails afterwards.
    let mut container = None;
    let started = start(&mut container);
    end(container.as_deref(), &started)
}

fn start(container: &mut Option<String>) -> Result<(), String> {
    let Start {
        hard_end_unix_seconds,
        pulls_by_unix_seconds,
        role_image,
        sandboxd_image,
        confinement,
        gateway,
        grant,
        ops,
        model,
    } = read_start()?;
    let hard_end = Deadline::at_unix(Duration::from_secs(hard_end_unix_seconds));
    let pulls_by = Deadline::at_unix(Duration::from_secs(pulls_by_unix_seconds));
    lock_down(gateway, pulls_by)?;
    check_docker(pulls_by)?;
    pull(&role_image, &sandboxd_image, pulls_by)?;
    copy_sandboxd(&sandboxd_image, pulls_by)?;
    let mut run = vec![
        "run".to_owned(),
        "--detach".to_owned(),
        format!("--name={CONTAINER}"),
    ];
    run.extend(confinement);
    run.extend(instance_options(ops)?);
    run.extend(["--".to_owned(), role_image, gateway.to_string()]);
    run.extend(ops.then(|| "--ops".to_owned()));
    run.extend(model.then(|| "--model".to_owned()));
    let container = container.insert(run_granted(&run, &grant, pulls_by)?);
    say(format_args!("container {container} started"));
    // The start's deadline, which the unit scheduled, would power the instance off under the
    // running container: the hard end bounds it from here on.
    power_off_at(hard_end_unix_seconds, pulls_by)?;
    let status = command("docker", &["wait", container], b"", hard_end)?;
    say(format_args!(
        "container {container} exited with status {status}"
    ));
    Ok(())
}

fn end(container: Option<&str>, started: &Result<(), String>) -> ExitCode {
    match started {
        Ok(()) => say("the boot ended"),
        Err(error) => say(format_args!("the boot failed: {error}")),
    }
    // The container's last lines, which show why a boot failed.
    if let Some(container) = container {
        let logs = ["logs", "--tail", LAST_LINES, container];
        match ran("docker", &logs, b"", Deadline::after(END_TIMEOUT)) {
            Ok(Output {
                status,
                stdout,
                stderr,
            }) => {
                let (stdout, stderr) = (
                    String::from_utf8_lossy(&stdout),
                    String::from_utf8_lossy(&stderr),
                );
                // Every line `docker logs` prints, even as it fails, may be the container's own, so
                // each carries the container's prefix, never the boot's alone.
                for line in stdout.lines().chain(stderr.lines()) {
                    say(format_args!("sandboxd: {line}"));
                }
                if !status.success() {
                    say(format_args!("`docker logs` failed with {status}"));
                }
            }
            Err(error) => say(format_args!(
                "the container's last lines can't be read: {error}"
            )),
        }
    }
    match started {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

// Makes the start's directory root's alone first, as cloud-init makes it with its umask's mode,
// then reads the file and removes it before parsing it, so it is gone whether or not it parses.
fn read_start() -> Result<Start, String> {
    let dir = Path::new(GRANT_DIR.trim_start_matches('/'));
    let private = fs::set_permissions(dir, Permissions::from_mode(0o700));
    private.context(format_args!("making {} root's alone failed", dir.display()))?;
    let file = Path::new(START_FILE.trim_start_matches('/'));
    let read = fs::read(file).context(format_args!("reading {} failed", file.display()));
    let removed = fs::remove_file(file).context(format_args!("removing {} failed", file.display()));
    let text = read?;
    removed?;
    serde_json::from_slice(&text).map_err(|error| {
        // Where alone: serde's message may quote a value, such as the grant.
        format!(
            "the start in {} can't be read, at line {}, column {}: expected a start's JSON, as \
             the provider writes it",
            file.display(),
            error.line(),
            error.column()
        )
    })
}

// Schedules the instance's poweroff at `at_unix_seconds` with logind, in place of the one
// scheduled before: the call that `shutdown` makes, at an exact time, which fires at once if the
// time has passed:
// https://github.com/systemd/systemd-stable/blob/0f3ad617e60c1d037025ff29a45a6422630ad9e8/man/org.freedesktop.login1.xml#L632-L636
fn power_off_at(at_unix_seconds: u64, by: Deadline) -> Result<(), String> {
    let micros = Duration::from_secs(at_unix_seconds).as_micros().to_string();
    let call = [
        "call",
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
        "ScheduleShutdown",
        "st",
        "poweroff",
        &micros,
    ];
    command("busctl", &call, b"", by).map(drop)
}

fn lock_down(gateway: SocketAddrV4, by: Deadline) -> Result<(), String> {
    let (ip, port) = (gateway.ip(), gateway.port());
    // Containers reach the gateway and nothing else, neither the instance, its metadata included,
    // nor any other address. A drop in any base chain is final, whatever Docker's own rules accept,
    // and the `inet` family holds for IPv6 too:
    // https://wiki.nftables.org/wiki-nftables/index.php/Configuring_chains
    let rules = format!(
        "table inet pumpkin {{
    chain forward {{
        type filter hook forward priority filter - 10; policy accept;
        iifname \"docker0\" ip daddr {ip} tcp dport {port} accept
        iifname \"docker0\" drop
    }}
    chain input {{
        type filter hook input priority filter - 10; policy accept;
        iifname \"docker0\" drop
    }}
}}
"
    );
    command("nft", &["--file", "-"], rules.as_bytes(), by).map(drop)
}

fn check_docker(by: Deadline) -> Result<(), String> {
    // A limit that Docker can't enforce would leave the role unbounded.
    let format = "{{.MemoryLimit}} {{.SwapLimit}} {{.CPUCfsQuota}} {{.PidsLimit}}";
    let limits = command("docker", &["info", "--format", format], b"", by)?;
    if limits != "true true true true" {
        return Err(format!(
            "docker supports the memory, swap, CPU and process limits as `{limits}`: expected \
             all four"
        ));
    }
    let version = command(
        "docker",
        &["version", "--format", "{{.Server.Version}}"],
        b"",
        by,
    )?;
    say(format_args!("docker {version}"));
    Ok(())
}

// By the instance's role, which ECR's credential helper gets a token with for each pull, as the
// user data configures root's Docker.
fn pull(role_image: &str, sandboxd_image: &str, by: Deadline) -> Result<(), String> {
    command("docker", &["pull", "--quiet", role_image], b"", by)?;
    // Docker's own record of what it pulled goes to the console, for an audit.
    for image in [role_image, sandboxd_image] {
        let inspect = [
            "image",
            "inspect",
            "--format",
            "{{index .RepoDigests 0}}",
            image,
        ];
        let pulled = command("docker", &inspect, b"", by)?;
        say(format_args!("pulled {image}: {pulled}"));
    }
    Ok(())
}

// Puts `sandboxd` on the instance, to mount into the role's container, which holds none.
fn copy_sandboxd(sandboxd_image: &str, by: Deadline) -> Result<(), String> {
    let made = DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(HOST_DIR);
    made.context(format_args!("making {HOST_DIR} failed"))?;
    let create = ["create", "--entrypoint=/", sandboxd_image];
    let copied = command("docker", &create, b"", by)?;
    let from = format!("{copied}:{SANDBOXD_IN_IMAGE}");
    let to = host_sandboxd();
    command("docker", &["cp", &from, &to.to_string_lossy()], b"", by)?;
    command("docker", &["rm", &copied], b"", by).map(drop)
}

fn instance_options(ops: bool) -> Result<Vec<String>, String> {
    let sandboxd = absolute(&host_sandboxd())?;
    let mount = |target: &str| {
        format!(
            "--mount=type=bind,source={},target={target},readonly",
            sandboxd.display()
        )
    };
    let mut options = vec![
        // Docker's default on cgroup v2, pinned against the daemon's setting: the role reads its
        // own limits at its cgroup's root.
        "--cgroupns=private".to_owned(),
        // Docker's default bridge, pinned against the daemon's setting: the instance's rules
        // confine it, by its interface's name (`lock_down`).
        "--network=bridge".to_owned(),
        format!("--dns={DNS}"),
        // The instance's search domains would otherwise reach the container's resolver.
        "--dns-search=.".to_owned(),
        format!("--env-file={}", absolute(&grant_file())?.display()),
        mount(SANDBOXD_PATH),
    ];
    if ops {
        options.push(mount(SANDBOXCTL_PATH));
    }
    options.push(format!("--entrypoint={SANDBOXD_PATH}"));
    Ok(options)
}

fn run_granted(run: &[String], grant: &RegistrationGrant, by: Deadline) -> Result<String, String> {
    // The grant reaches the container by a file that only this user reads, not by an argument that
    // `ps` shows, and the file lasts no longer than `docker run`. Nothing else of it is mounted
    // into the container. Its `sandboxd` is its first process, and this starts nothing else in
    // it, so nothing runs there as the role's user before the hello that spends the grant.
    let digits = grant.digits();
    let file = grant_file();
    let opened = (OpenOptions::new().write(true).create_new(true).mode(0o600)).open(&file);
    let written = opened.and_then(|mut file| writeln!(file, "{SANDBOXD_GRANT}={digits}"));
    let written = written.context(format_args!("writing {} failed", file.display()));
    let run: Vec<&str> = run.iter().map(String::as_str).collect();
    let started = written.and_then(|()| command("docker", &run, b"", by));
    // Removed whether or not the container started.
    let removed =
        fs::remove_file(&file).context(format_args!("removing {} failed", file.display()));
    let started = started?;
    removed.map(|()| started)
}

fn host_sandboxd() -> PathBuf {
    Path::new(HOST_DIR).join("sandboxd")
}

fn grant_file() -> PathBuf {
    Path::new(GRANT_DIR.trim_start_matches('/')).join(GRANT_FILE)
}

// Docker takes the source of a bind mount as an absolute path.
fn absolute(path: &Path) -> Result<PathBuf, String> {
    path::absolute(path).context(format_args!("making {} absolute failed", path.display()))
}

fn command(program: &str, args: &[&str], input: &[u8], by: Deadline) -> Result<String, String> {
    let Output {
        status,
        stdout,
        stderr,
    } = ran(program, args, input, by)?;
    if !status.success() {
        return Err(format!(
            "{} failed with {status}: {}: fix what it reports",
            shown(program, args),
            String::from_utf8_lossy(&stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&stdout).trim_end().to_owned())
}

fn ran(program: &str, args: &[&str], input: &[u8], by: Deadline) -> Result<Output, String> {
    let shown = shown(program, args);
    let mut command = stdx::process::command(program, Environment::Inherited);
    let ran = stdx::process::fed(command.args(args), input, by);
    let ran = ran.context(format_args!("{shown} can't run"))?;
    ran.ok_or_else(|| format!("{shown} didn't end by its deadline, and was killed"))
}

fn shown(program: &str, args: &[&str]) -> String {
    format!("`{program} {}`", args.first().copied().unwrap_or_default())
}

fn say(text: impl fmt::Display) {
    for line in text.to_string().lines() {
        println!("{CONSOLE_PREFIX} {line}");
    }
}
