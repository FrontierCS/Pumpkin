//! `ec2-boot`, the boot of each sandbox's instance of the EC2 provider, which the instance's user
//! data runs as `ec2-boot start`, once it has made [`GRANT_DIR`], started Docker, logged in to
//! the registry and pulled `sandboxd`'s image, with a `schema::ec2_boot::Start` on its standard
//! input.
//!
//! It drops whatever the container sends but to the gateway's address and port, refuses a Docker
//! that can't confine the container or pull the role's image, pulls that image and logs out of
//! the registry, then runs the role's container until it exits, under the confinement that the
//! `ec2` crate states; whatever ends it, it powers the instance off, as the user data does
//! however `ec2-boot` ends. Each command it runs is killed by the start's deadline, or at the
//! hard end once the container starts.
//!
//! Each line it prints, on its standard output, which the user data sends to the serial console,
//! begins with `schema::ec2_boot::CONSOLE_PREFIX`, and none holds the grant. Its files are at
//! their paths below the directory it runs in, which the user data makes `/`.

#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "its console lines are its standard output, and a command line it can't run is \
              reported on its standard error"
)]

use std::ffi::OsString;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write as _};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{self, Path, PathBuf};
use std::process::{ExitCode, Output};
use std::time::Duration;
use std::{env, fmt};

use schema::ec2_boot::{CONSOLE_PREFIX, GRANT_DIR, SANDBOXD_IN_IMAGE, Start};
use schema::sandbox::{RegistrationGrant, SANDBOXCTL_PATH, SANDBOXD_GRANT, SANDBOXD_PATH};
use stdx::process::Environment;
use stdx::{Context as _, Deadline};

/// The name of the file, in [`GRANT_DIR`], that holds the grant for the container's
/// environment, only until the container starts.
const GRANT_FILE: &str = "grant.env";

/// Where the instance keeps `sandboxd`, copied out of its image as `sandboxd`, to be mounted
/// read-only.
const HOST_DIR: &str = "opt/pumpkin";

const CONTAINER: &str = "pumpkin-sandbox";

// The container's DNS server is its own loopback, where nothing answers: on Docker's default
// bridge, the address goes into the container's `/etc/resolv.conf` with no resolver of Docker's
// between, so no lookup leaves the container, and one fails at once rather than waiting on an
// address the rules drop.
const DNS: Ipv4Addr = Ipv4Addr::LOCALHOST;

/// How many of the container's last lines of output the boot's end copies to the console.
const LAST_LINES: &str = "20";

/// How long each command of the boot's end, which reads the container's last lines and powers
/// the instance off, runs before it is killed.
// Each asks a local daemon one thing, in well under a second; the hard end's shutdown, which the
// user data armed, follows whatever this bound cuts short.
const END_TIMEOUT: Duration = Duration::from_mins(1);

fn main() -> ExitCode {
    // `stdx::process` reads each command's output on threads of its own.
    stdx::abort_on_panic();
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    if args != ["start"] {
        eprintln!(
            "ec2-boot: the command line is {args:?}: expected `ec2-boot start`, with a start on \
             standard input"
        );
        return ExitCode::from(2);
    }
    let mut container = None;
    let started = start(&mut container);
    end(container.as_deref(), &started)
}

/// Starts the sandbox that the start on standard input describes, and returns once its container
/// has exited, having put the container's ID in `container` once it started.
fn start(container: &mut Option<String>) -> Result<(), String> {
    let Start {
        hard_end_unix_seconds,
        pulls_by_unix_seconds,
        registry,
        role_image,
        sandboxd_image,
        confinement,
        gateway,
        grant,
        ops,
        model,
    } = serde_json::from_reader(io::stdin().lock()).map_err(|error| {
        // Where alone: serde's message may quote a value, such as the grant.
        format!(
            "the start on standard input can't be read, at line {}, column {}: expected a \
             start's JSON, as the provider writes it",
            error.line(),
            error.column()
        )
    })?;
    let hard_end = Deadline::at_unix(Duration::from_secs(hard_end_unix_seconds));
    let pulls_by = Deadline::at_unix(Duration::from_secs(pulls_by_unix_seconds));
    lock_down(gateway, pulls_by)?;
    check_docker(pulls_by)?;
    pull(&registry, &role_image, &sandboxd_image, pulls_by)?;
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
    let container = container.insert(run_granted(&run, &grant, hard_end)?);
    say(format_args!("container {container} started"));
    let status = command("docker", &["wait", container], b"", hard_end)?;
    say(format_args!(
        "container {container} exited with status {status}"
    ));
    Ok(())
}

/// Says how the boot ended, `started`, then the container's last lines, if it started, then
/// powers the instance off; returns the exit code that `started` gives.
fn end(container: Option<&str>, started: &Result<(), String>) -> ExitCode {
    match started {
        Ok(()) => say("the boot ended"),
        Err(error) => say(format_args!("the boot failed: {error}")),
    }
    if let Some(container) = container {
        let logs = ["logs", "--tail", LAST_LINES, container];
        // Every line `docker logs` prints, even as it fails, may be the container's own, so
        // each carries the container's prefix, never the boot's alone.
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
    let off = command(
        "shutdown",
        &["--poweroff", "now"],
        b"",
        Deadline::after(END_TIMEOUT),
    );
    if let Err(error) = off {
        say(format_args!("the instance can't power itself off: {error}"));
    }
    match started {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

/// Drops whatever the container sends, but to `gateway`: to this instance and anything past it,
/// its metadata included. A drop in any base chain is final, whatever Docker's own rules accept,
/// and the `inet` family holds for IPv6 too.
fn lock_down(gateway: SocketAddrV4, by: Deadline) -> Result<(), String> {
    let (ip, port) = (gateway.ip(), gateway.port());
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

/// Refuses a Docker that lacks a limit that the container's confinement sets, as the Docker
/// provider refuses its own engine, since it would leave the role unbounded.
fn check_docker(by: Deadline) -> Result<(), String> {
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

/// Pulls `role_image`, then says, for an audit of the console, Docker's own record of what it
/// pulled of it and of `sandboxd_image`, then logs out of `registry`, so that the login lasts only
/// while the images are pulled.
fn pull(
    registry: &str,
    role_image: &str,
    sandboxd_image: &str,
    by: Deadline,
) -> Result<(), String> {
    command("docker", &["pull", "--quiet", role_image], b"", by)?;
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
    command("docker", &["logout", registry], b"", by).map(drop)
}

/// Copies `sandboxd` out of `sandboxd_image` to [`host_sandboxd`].
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

/// The options of `docker run` that give the container its place on the instance: its cgroup
/// namespace, its network, its DNS, the grant's file for its environment, and its `sandboxd`, and
/// `sandboxctl` too if `ops`, mounted read-only from [`host_sandboxd`], as its first process.
fn instance_options(ops: bool) -> Result<Vec<String>, String> {
    let sandboxd = absolute(&host_sandboxd())?;
    let mount = |target: &str| {
        format!(
            "--mount=type=bind,source={},target={target},readonly",
            sandboxd.display()
        )
    };
    let mut options = vec![
        // Docker's default on cgroup v2, which the AMI has, pinned against the daemon's setting:
        // the role reads its own limits at its cgroup's root.
        "--cgroupns=private".to_owned(),
        // Docker's default bridge, pinned against the daemon's setting: the instance's rules
        // confine it, by its interface's name ([`lock_down`]).
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

/// Runs `docker` with `run`, with `grant` in a root-only file in memory, [`grant_file`], only
/// until it returns, killed by `by`; returns what it printed, the container's ID.
// Nothing else of the grant is mounted into the container. Its `sandboxd` is its first process,
// and this starts nothing else in it, so nothing runs there as the role's user before the hello
// that spends the grant.
fn run_granted(run: &[String], grant: &RegistrationGrant, by: Deadline) -> Result<String, String> {
    let digits = grant.digits();
    let file = grant_file();
    let opened = (OpenOptions::new().write(true).create_new(true).mode(0o600)).open(&file);
    let written = opened.and_then(|mut file| writeln!(file, "{SANDBOXD_GRANT}={digits}"));
    let written = written.context(format_args!("writing {} failed", file.display()));
    let run: Vec<&str> = run.iter().map(String::as_str).collect();
    let started = written.and_then(|()| command("docker", &run, b"", by));
    let removed =
        fs::remove_file(&file).context(format_args!("removing {} failed", file.display()));
    let started = started?;
    removed.map(|()| started)
}

/// Where the instance keeps `sandboxd`, in [`HOST_DIR`], below the directory this runs in.
fn host_sandboxd() -> PathBuf {
    Path::new(HOST_DIR).join("sandboxd")
}

/// The grant's file, [`GRANT_FILE`] in [`GRANT_DIR`], which the user data made, below the
/// directory this runs in.
fn grant_file() -> PathBuf {
    Path::new(GRANT_DIR.trim_start_matches('/')).join(GRANT_FILE)
}

/// `path`, below the directory this runs in, as an absolute path, as Docker takes it.
fn absolute(path: &Path) -> Result<PathBuf, String> {
    path::absolute(path).context(format_args!("making {} absolute failed", path.display()))
}

/// Runs `program` with `args`, and `input` on its standard input, killed by `by`, and returns what
/// it printed on its standard output, without the whitespace at its end.
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

/// Runs `program` as [`command`] does, and returns its output, whether it succeeded or not.
fn ran(program: &str, args: &[&str], input: &[u8], by: Deadline) -> Result<Output, String> {
    let shown = shown(program, args);
    let mut command = stdx::process::command(program, Environment::Inherited);
    let ran = stdx::process::fed(command.args(args), input, by);
    let ran = ran.context(format_args!("{shown} can't run"))?;
    ran.ok_or_else(|| format!("{shown} didn't end by its deadline, and was killed"))
}

/// `program` and its first argument, as an error names the command.
fn shown(program: &str, args: &[&str]) -> String {
    format!("`{program} {}`", args.first().copied().unwrap_or_default())
}

/// Writes `text` to the console, each of its lines after [`CONSOLE_PREFIX`], so that no line of a
/// command's output that it holds passes for one of the boot's own.
fn say(text: impl fmt::Display) {
    for line in text.to_string().lines() {
        println!("{CONSOLE_PREFIX} {line}");
    }
}
