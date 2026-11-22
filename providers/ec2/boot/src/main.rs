//! Boots a sandbox on its instance: reads the `Start` in the user data ([`USER_DATA_FILE`]),
//! locks the instance's network down, fetches the instance's identity document, pulls the role's
//! image and runs its container with `sandboxd` in it, which presents the document as its
//! attestation, until the container exits, then exits itself, with a failure if the boot failed.
//! Whatever happens, it reports on its standard output, which its unit connects to the serial
//! console, each line behind [`CONSOLE_PREFIX`] and none holding the document. The `Start`'s
//! deadlines say what bounds it. Its files are at their paths below the directory it runs in,
//! which its unit makes `/`.

#![expect(
    clippy::print_stderr,
    reason = "a command line it can't run is reported on its standard error"
)]

use std::ffi::OsString;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write as _};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{self, Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;
use std::{env, fmt};

use schema::ec2_boot::{
    ATTESTATION_DIR, CONSOLE_PREFIX, Identity, SANDBOXD_IN_IMAGE, Start, USER_DATA_FILE, start_in,
};
use schema::sandbox::{SANDBOXCTL_PATH, SANDBOXD_ATTESTATION, SANDBOXD_PATH};
use stdx::process::{self, Bound, Echo, Environment, Failure, Run, command, printed};
use stdx::{Context as _, Deadline};

const ATTESTATION_FILE: &str = "attestation.env";

// The instance's metadata service, which the instance reaches, and its container doesn't
// (`lock_down`):
// https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/instancedata-data-retrieval.html
const METADATA: &str = "http://169.254.169.254/latest";

// How long the metadata service's token lasts, in seconds: it is spent at once, on the two reads
// that follow.
const TOKEN_TTL_SECONDS: u32 = 60;

// How long each request to the metadata service may take, in seconds, so that one that hangs fails
// the boot at once rather than at the start's deadline: the service is on the instance's own
// link, and botocore gives a request to it 1 s, which this leaves room past:
// https://github.com/boto/botocore/blob/45d5dde95e31f22f0ffeea5a790ea2ea79d1035c/botocore/utils.py#L100
const METADATA_TIMEOUT_SECONDS: u32 = 10;

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
             {USER_DATA_FILE}"
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
        ops,
        model,
    } = read_start()?;
    let hard_end = Deadline::at_unix(Duration::from_secs(hard_end_unix_seconds));
    let pulls_by = Deadline::at_unix(Duration::from_secs(pulls_by_unix_seconds));
    lock_down(gateway, pulls_by)?;
    let identity = identity(pulls_by)?;
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
    let container = container.insert(run_attested(&run, &identity, pulls_by)?);
    say(format_args!("container {container} started"));
    // The start's deadline, which the unit scheduled, would power the instance off under the
    // running container: the hard end bounds it from here on.
    power_off_at(hard_end_unix_seconds, pulls_by)?;
    let mut wait = command("docker", Environment::Inherited);
    let status = stdout(
        wait.args(["wait", container]),
        silently(hard_end).named("docker wait"),
    )?;
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
        let mut logs = command("docker", Environment::Inherited);
        logs.args(["logs", "--tail", LAST_LINES, container]);
        // `docker logs` prints the container's lines even as it fails, so they are wanted then too.
        match process::run(&mut logs, silently(Deadline::after(END_TIMEOUT))) {
            Ok(output) | Err(Failure::Exited { output, .. }) => {
                let (stdout, stderr) = (
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr),
                );
                // Every line `docker logs` prints, even as it fails, may be the container's own, so
                // each carries the container's prefix, never the boot's alone.
                for line in stdout.lines().chain(stderr.lines()) {
                    say(format_args!("sandboxd: {line}"));
                }
                if !output.status.success() {
                    say(format_args!("`docker logs` failed with {}", output.status));
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

// Makes the attestation's directory, root's alone, then reads the start out of cloud-init's copy of
// the user data, which it leaves.
fn read_start() -> Result<Start, String> {
    let dir = Path::new(ATTESTATION_DIR.trim_start_matches('/'));
    let made = DirBuilder::new().mode(0o700).create(dir);
    made.context(format_args!("making {} failed", dir.display()))?;
    let file = Path::new(USER_DATA_FILE.trim_start_matches('/'));
    let text =
        fs::read_to_string(file).context(format_args!("reading {} failed", file.display()))?;
    start_in(&text)
        .map_err(|why| format!("the user data in {} can't be read: {why}", file.display()))
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
    let mut busctl = command("busctl", Environment::Inherited);
    stdout(busctl.args(call), silently(by).named("busctl call")).map(drop)
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
    let mut nft = command("nft", Environment::Inherited);
    let loaded = silently(by).input(rules.as_bytes());
    stdout(nft.args(["--file", "-"]), loaded).map(drop)
}

// The instance's identity document and its signature, as its metadata serves them, by IMDSv2, the
// only version that the envelope's templates serve.
fn identity(by: Deadline) -> Result<Identity, String> {
    let max_time = METADATA_TIMEOUT_SECONDS.to_string();
    let curl = |args: &[&str]| {
        let mut curl = command("curl", Environment::Inherited);
        let options = [
            "--fail",
            "--silent",
            "--show-error",
            "--max-time",
            &max_time,
        ];
        curl.args(options).args(args);
        // As it was served, byte for byte, which the signature signs.
        let output = process::run(&mut curl, silently(by).named("curl"));
        let output = output.map_err(|failure| failure.to_string())?;
        String::from_utf8(output.stdout)
            .map_err(|error| format!("`curl` printed what isn't UTF-8: {}", error.utf8_error()))
    };
    let ttl = format!("X-aws-ec2-metadata-token-ttl-seconds: {TOKEN_TTL_SECONDS}");
    let token = curl(&[
        "--request",
        "PUT",
        "--header",
        &ttl,
        &format!("{METADATA}/api/token"),
    ]);
    let token = token.context("getting the instance metadata's token failed")?;
    let header = format!("X-aws-ec2-metadata-token: {token}");
    let read = |name: &str, what: &str| {
        let url = format!("{METADATA}/dynamic/instance-identity/{name}");
        let read = curl(&["--header", &header, &url]);
        read.context(format_args!(
            "reading the instance's identity {what} failed"
        ))
    };
    let identity = Identity {
        document: read("document", "document")?,
        signature: read("signature", "document's signature")?,
    };
    say("read the instance's identity document");
    Ok(identity)
}

fn check_docker(by: Deadline) -> Result<(), String> {
    // A limit that Docker can't enforce would leave the role unbounded.
    let format = "{{.MemoryLimit}} {{.SwapLimit}} {{.CPUCfsQuota}} {{.PidsLimit}}";
    let mut info = command("docker", Environment::Inherited);
    let limits = stdout(info.args(["info", "--format", format]), silently(by))?;
    if limits != "true true true true" {
        return Err(format!(
            "docker supports the memory, swap, CPU and process limits as `{limits}`: expected \
             all four"
        ));
    }
    let mut version = command("docker", Environment::Inherited);
    let version = stdout(
        version.args(["version", "--format", "{{.Server.Version}}"]),
        silently(by),
    )?;
    say(format_args!("docker {version}"));
    Ok(())
}

// By the instance's role, which ECR's credential helper gets a token with for each pull, as the
// user data configures root's Docker.
fn pull(role_image: &str, sandboxd_image: &str, by: Deadline) -> Result<(), String> {
    let mut pull = command("docker", Environment::Inherited);
    stdout(pull.args(["pull", "--quiet", role_image]), silently(by))?;
    // Docker's own record of what it pulled goes to the console, for an audit.
    for image in [role_image, sandboxd_image] {
        let mut inspect = command("docker", Environment::Inherited);
        inspect.args([
            "image",
            "inspect",
            "--format",
            "{{index .RepoDigests 0}}",
            image,
        ]);
        let pulled = stdout(&mut inspect, silently(by))?;
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
    let mut create = command("docker", Environment::Inherited);
    create.args(["create", "--entrypoint=/", sandboxd_image]);
    let copied = stdout(&mut create, silently(by))?;
    let from = format!("{copied}:{SANDBOXD_IN_IMAGE}");
    let mut copy = command("docker", Environment::Inherited);
    copy.args(["cp", &from]).arg(host_sandboxd());
    stdout(&mut copy, silently(by).named("docker cp"))?;
    let mut remove = command("docker", Environment::Inherited);
    stdout(
        remove.args(["rm", &copied]),
        silently(by).named("docker rm"),
    )
    .map(drop)
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
        format!("--env-file={}", absolute(&attestation_file())?.display()),
        mount(SANDBOXD_PATH),
    ];
    if ops {
        options.push(mount(SANDBOXCTL_PATH));
    }
    options.push(format!("--entrypoint={SANDBOXD_PATH}"));
    Ok(options)
}

fn run_attested(run: &[String], identity: &Identity, by: Deadline) -> Result<String, String> {
    // The attestation reaches the container by a file, which lasts no longer than `docker run`,
    // not by an argument that `ps` shows. Nothing else of it is mounted into the container. Its
    // `sandboxd` is its first process, and this starts nothing else in it, so nothing runs there as
    // the role's user before the hello that spends the attestation. Compact JSON holds no line
    // break, which would end the variable's line.
    let Ok(json) = serde_json::to_string(identity) else {
        unreachable!("an identity can't be written as JSON, though it holds only strings")
    };
    let file = attestation_file();
    let opened = (OpenOptions::new().write(true).create_new(true).mode(0o600)).open(&file);
    let written = opened.and_then(|mut file| writeln!(file, "{SANDBOXD_ATTESTATION}={json}"));
    let written = written.context(format_args!("writing {} failed", file.display()));
    let started = written.and_then(|()| {
        let mut docker = command("docker", Environment::Inherited);
        stdout(docker.args(run), silently(by))
    });
    // Removed whether or not the container started.
    let removed =
        fs::remove_file(&file).context(format_args!("removing {} failed", file.display()));
    let started = started?;
    removed.map(|()| started)
}

fn host_sandboxd() -> PathBuf {
    Path::new(HOST_DIR).join("sandboxd")
}

fn attestation_file() -> PathBuf {
    Path::new(ATTESTATION_DIR.trim_start_matches('/')).join(ATTESTATION_FILE)
}

// Docker takes the source of a bind mount as an absolute path.
fn absolute(path: &Path) -> Result<PathBuf, String> {
    path::absolute(path).context(format_args!("making {} absolute failed", path.display()))
}

// What `command` printed on its standard output, or its failure as a message.
fn stdout(command: &mut Command, how: Run<'_>) -> Result<String, String> {
    printed(process::run(command, how).map_err(|failure| failure.to_string())?)
}

// A run that ends by `by`, with no echo of its command.
fn silently(by: Deadline) -> Run<'static> {
    Run::new(Bound::By(by), Echo::Silent)
}

fn say(text: impl fmt::Display) {
    let mut console = io::stdout().lock();
    for line in text.to_string().lines() {
        // A console that takes no more, as a hung-up terminal, ends no boot: its lines only
        // report it.
        let _ = writeln!(console, "{CONSOLE_PREFIX} {line}");
    }
}
