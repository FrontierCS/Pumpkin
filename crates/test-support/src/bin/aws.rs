//! A stand-in for the `aws` command line, for the tests of the EC2 provider and of `pumpkin`,
//! which find it on `PATH` through a link named `aws` in a directory of their own. It answers as
//! AWS CLI v2 would, and records there each call, in `calls`, its command and arguments, a line
//! each, ended by an empty line, and its environment, in `env`. A file there named as a command,
//! such as `sts get-caller-identity`, or `--version`, is run by `bash` in place of the answer,
//! with `$dir` the directory, `$@` the call's arguments but its global options, `fail <code>
//! <operation> <message>` to fail as AWS does, and `described <id>` to print an instance. A
//! command that `throttled` names on a line, `<attempts> <command>`, is throttled for that many
//! attempts of each call, failing as `aws` does once `AWS_MAX_ATTEMPTS`, or 3, were throttled.
//! The repositories `pumpkin/roles` and `pumpkin/sandboxd` are in the account's registry, or the
//! one at the address in `registry`, asked for a manifest as user `AWS` with the password that
//! its `ecr get-login-password` gives.
//!
//! A launch boots AMI `ami-078a05cb041c4f0b8`, its instance kept in `instances/<id>` and its user
//! data, which must come in a file only its owner can read and be a bash script with every
//! placeholder filled, in `user-data-<id>`. If `sandboxd` names a `sandboxd`, the host starts it
//! at once, with the grant and the arguments of the start in the user data
//! (`test_support::start_of`), its output prefixed as the boot's end prefixes the container's in
//! `console-<id>`, after a line for each image pulled, and its process ID in `host-<id>`. A test
//! may place an instance itself, as `instances/<id>/state` and `instances/<id>/run`. Each instance
//! is described with its run's tag, and one shutting down is terminated once described. A
//! schedule is kept in `schedules/<group>/<name>`.

use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead as _, BufReader, Write as _};
use std::net::TcpStream;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};

use schema::ec2_boot::Start;
use stdx::Context as _;
use stdx::process::{Environment, command};

const VERSION: &str = "aws-cli/2.38.1 Python/3.14.6 Linux/7.0.0 exe/x86_64.stand-in";

/// The AMI that every launch boots.
const IMAGE: &str = "ami-078a05cb041c4f0b8";

const CALLER: &str = r#"{"UserId": "AROA:i-0fedcba9876543210", "Account": "123456789012", "Arn": "arn:aws:sts::123456789012:assumed-role/host/i-0fedcba9876543210"}"#;

/// The registry's address unless `registry` names another.
const REGISTRY: &str = "123456789012.dkr.ecr.us-west-1.amazonaws.com";

/// What `ecr get-login-password` prints.
const PASSWORD: &str = "stand-in-password";

/// The base64 of `AWS:` and [`PASSWORD`]: the registry's login, as `ecr get-login-password` gives
/// it.
const LOGIN: &str = "QVdTOnN0YW5kLWluLXBhc3N3b3Jk";

/// Runs `script`, a file named as the call's command, in `bash`, with the prelude's names:
/// `dir`, `fail` and `described`, which ask this program for what they print.
const PRELUDE: &str = r#"dir=$1 self=$2 script=$3; shift 3
fail() { "$self" --fail "$@"; exit $?; }
described() { "$self" --described "$dir" "$1"; }
. "$script"
"#;

/// How a call ends without an answer: what it prints to standard error, if anything, and its
/// exit status.
struct Failed {
    message: String,
    code: u8,
}

impl From<String> for Failed {
    fn from(message: String) -> Self {
        Self { message, code: 1 }
    }
}

/// The failure of `operation` that AWS reports as `code`, with `message`.
fn fail(code: &str, operation: &str, message: &str) -> Failed {
    Failed {
        message: format!(
            "aws: [ERROR]: An error occurred ({code}) when calling the {operation} operation: \
             {message}"
        ),
        code: 254,
    }
}

fn main() -> ExitCode {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failed { message, code }) => {
            if !message.is_empty() {
                let _ = writeln!(io::stderr(), "{message}");
            }
            ExitCode::from(code)
        }
    }
}

fn run(raw: &[OsString]) -> Result<(), Failed> {
    let args: Vec<String> = raw
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    // The calls that the prelude's names make are no calls to `aws`, so they are not recorded.
    match args[..] {
        ["--fail", code, operation, message] => return Err(fail(code, operation, message)),
        ["--described", dir, id] => return print(&described(Path::new(dir), id)?),
        _ => {}
    }
    let dir = directory()?;
    record(&dir, raw)?;
    call(&dir, &args)
}

fn print(text: &str) -> Result<(), Failed> {
    Ok(io::stdout()
        .write_all(text.as_bytes())
        .context("standard output")?)
}

/// The directory of the first `aws` on `PATH` that is a link to this program.
fn directory() -> Result<PathBuf, String> {
    let exe = env::current_exe().context("the stand-in's own path")?;
    let path = env::var_os("PATH").unwrap_or_default();
    let linked = |dir: &PathBuf| fs::canonicalize(dir.join("aws")).is_ok_and(|aws| aws == exe);
    (env::split_paths(&path).find(linked)).ok_or_else(|| {
        format!(
            "`PATH` names no directory with a link to {}, named `aws`: put one first on it",
            exe.display()
        )
    })
}

/// Appends the call to `dir`'s `calls`, and writes the environment to its `env`.
fn record(dir: &Path, args: &[OsString]) -> Result<(), String> {
    let mut call = b"aws\n".to_vec();
    for arg in args {
        call.extend_from_slice(arg.as_bytes());
        call.push(b'\n');
    }
    call.push(b'\n');
    let calls = OpenOptions::new()
        .append(true)
        .create(true)
        .open(dir.join("calls"));
    let mut calls = calls.context("opening `calls`")?;
    calls.write_all(&call).context("writing `calls`")?;
    let mut variables: Vec<(OsString, OsString)> = env::vars_os().collect();
    variables.sort();
    let mut written = Vec::new();
    for (name, value) in variables {
        written.extend_from_slice(name.as_bytes());
        written.push(b'=');
        written.extend_from_slice(value.as_bytes());
        written.push(b'\n');
    }
    fs::write(dir.join("env"), written).context("writing `env`")
}

/// Answers `args`, a call to `aws` from the directory `dir`.
fn call(dir: &Path, args: &[&str]) -> Result<(), Failed> {
    let mut rest = args;
    let command = if args.first() == Some(&"--version") {
        "--version".to_owned()
    } else {
        // Global options: `--no-cli-pager` alone, and the others with their values.
        while let Some(option) = rest.first().filter(|arg| arg.starts_with("--")) {
            let width = if *option == "--no-cli-pager" { 1 } else { 2 };
            rest = rest.get(width..).unwrap_or_default();
        }
        format!("{} {}", arg(rest, 0), arg(rest, 1))
    };
    throttle(dir, &command)?;
    let script = dir.join(&command);
    if script.exists() {
        return source(dir, &script, rest);
    }
    print(&answer(dir, &command, rest)?)
}

/// Fails the call of `command` as `aws` does once AWS has throttled every attempt that it makes,
/// as many as `AWS_MAX_ATTEMPTS` says, or 3 without it, if `throttled` says that AWS throttles
/// at least as many attempts of the command's.
fn throttle(dir: &Path, command: &str) -> Result<(), Failed> {
    let Ok(throttled) = fs::read_to_string(dir.join("throttled")) else {
        return Ok(());
    };
    let attempts = env::var("AWS_MAX_ATTEMPTS").ok();
    let attempts: u32 = attempts.and_then(|n| n.parse().ok()).unwrap_or(3);
    let names = |line: &str| {
        let (count, named) = line.split_once(' ').unwrap_or((line, ""));
        named == command && count.parse::<u32>().is_ok_and(|count| count >= attempts)
    };
    if !throttled.lines().any(names) {
        return Ok(());
    }
    Err(Failed {
        message: format!(
            "aws: [ERROR]: An error occurred (ThrottlingException) when calling the {command} \
             operation (reached max retries: {}): Rate exceeded",
            attempts - 1
        ),
        code: 254,
    })
}

/// The `index`th of `args`, or nothing.
fn arg<'a>(args: &[&'a str], index: usize) -> &'a str {
    args.get(index).copied().unwrap_or_default()
}

/// Replaces this program with `bash` running `script`, with the prelude's names and `args` as
/// its arguments, which ends as the script does.
fn source(dir: &Path, script: &Path, args: &[&str]) -> Result<(), Failed> {
    let exe = env::current_exe().context("the stand-in's own path")?;
    let mut bash = command("bash", Environment::Inherited);
    bash.args(["-c", PRELUDE, "aws"]).args([dir, &exe, script]);
    Err(format!("running `bash`: {}", bash.args(args).exec()).into())
}

/// What `aws` prints for the call `command`, whose arguments, but its global options, are
/// `args`.
fn answer(dir: &Path, command: &str, args: &[&str]) -> Result<String, Failed> {
    let said = |text: &str| Ok(format!("{text}\n"));
    match command {
        "--version" => said(VERSION),
        "sts get-caller-identity" => said(CALLER),
        "ecr describe-repositories" => said(&repositories(dir)),
        "ecr get-login-password" => said(PASSWORD),
        "ecr batch-get-image" => said(&batch_get_image(dir, args)?),
        "ec2 run-instances" => said(&run_instances(dir, args)?),
        "ec2 describe-instances" => said(&describe_instances(dir, args)?),
        "ec2 terminate-instances" => said(&terminate_instances(dir, args)?),
        "ec2 get-console-output" => said(&console_output(dir, arg(args, 3))?),
        "scheduler create-schedule" => said(&create_schedule(dir, args)?),
        "scheduler delete-schedule" => delete_schedule(dir, args).map(|()| String::new()),
        _ => Err(fail(
            "InvalidAction",
            command,
            "the stand-in doesn't know it",
        )),
    }
}

/// The text of `path` without its trailing newlines, as a command substitution has it.
fn text(path: &Path) -> io::Result<String> {
    fs::read_to_string(path).map(|text| text.trim_end_matches('\n').to_owned())
}

/// The address of the registry: the one in `registry`, or the account's.
fn repositories(dir: &Path) -> String {
    let registry = text(&dir.join("registry")).unwrap_or_else(|_| REGISTRY.to_owned());
    format!(
        r#"{{"repositories": [{{"repositoryName": "pumpkin/roles", "repositoryUri": "{registry}/pumpkin/roles"}}, {{"repositoryName": "pumpkin/sandboxd", "repositoryUri": "{registry}/pumpkin/sandboxd"}}]}}"#
    )
}

/// The answer to `ecr batch-get-image`: whether the registry in `registry` has the manifest of
/// the image, which it is asked for by a `HEAD` request.
fn batch_get_image(dir: &Path, args: &[&str]) -> Result<String, Failed> {
    // The media types are the call's last arguments, which the registry is asked to accept.
    let rest = args.get(2..).unwrap_or_default();
    let split = rest.iter().position(|arg| *arg == "--accepted-media-types");
    let (rest, accept) = match split {
        Some(at) => (&rest[..at], rest[at + 1..].join(",")),
        None => (rest, String::new()),
    };
    let [repository, digest] = flags(rest, ["--repository-name", "--image-ids"]);
    let digest = digest.strip_prefix("imageDigest=").unwrap_or(digest);
    let registry = text(&dir.join("registry")).context("reading `registry`")?;
    let asked = manifest_status(&registry, repository, digest, &accept);
    let status = asked.map_err(|error| {
        let message = format!("the stand-in's registry can't be asked: {error}");
        fail("ServerException", "BatchGetImage", &message)
    })?;
    // `HTTP/1.<digit> <code> <reason>`.
    let code = status
        .starts_with("HTTP/1.")
        .then(|| status.split(' ').nth(1));
    match code.flatten() {
        Some("200") => Ok(format!(
            r#"{{"images": [{{"registryId": "123456789012", "repositoryName": "{repository}", "imageId": {{"imageDigest": "{digest}"}}}}], "failures": []}}"#
        )),
        Some("404") => Ok(format!(
            r#"{{"images": [], "failures": [{{"imageId": {{"imageDigest": "{digest}"}}, "failureCode": "ImageNotFound", "failureReason": "Requested image not found"}}]}}"#
        )),
        _ => Err(fail(
            "ServerException",
            "BatchGetImage",
            &format!("the stand-in's registry answered `{status}`"),
        )),
    }
}

/// The status line that the registry at `registry`, `host:port`, answers to a `HEAD` request for
/// the manifest of `digest` in `repository`, of the media types `accept`.
fn manifest_status(
    registry: &str,
    repository: &str,
    digest: &str,
    accept: &str,
) -> io::Result<String> {
    let (host, port) = registry.rsplit_once(':').unwrap_or((registry, ""));
    let mut stream = TcpStream::connect((host, port.parse().map_err(io::Error::other)?))?;
    write!(
        stream,
        "HEAD /v2/{repository}/manifests/{digest} HTTP/1.0\r\nHost: {registry}\r\nAccept: \
         {accept}\r\nAuthorization: Basic {LOGIN}\r\n\r\n"
    )?;
    let mut status = String::new();
    BufReader::new(stream).read_line(&mut status)?;
    Ok(status.replace('\r', "").trim_end_matches('\n').to_owned())
}

/// The value of each `flag` that `args` pairs with one of `flags`, in order, skipping two at a
/// time as the stand-in always has.
fn flags<'a, const N: usize>(args: &[&'a str], flags: [&str; N]) -> [&'a str; N] {
    let mut values = [""; N];
    for pair in args.chunks(2) {
        if let Some(at) = flags.iter().position(|flag| *flag == pair[0]) {
            values[at] = arg(pair, 1);
        }
    }
    values
}

/// The answer to `ec2 run-instances`: launches the next instance.
fn run_instances(dir: &Path, args: &[&str]) -> Result<String, Failed> {
    let [tags, user_data] = flags(
        args.get(2..).unwrap_or_default(),
        ["--tag-specifications", "--user-data"],
    );
    let user_data = Path::new(user_data.strip_prefix("file://").unwrap_or(user_data));
    let launched = fs::read_dir(dir).context("listing the directory")?;
    let count = (launched.flatten())
        .filter(|entry| entry.file_name().as_bytes().starts_with(b"user-data-"))
        .count();
    let id = format!("i-{:017x}", count + 1);
    let private = fs::metadata(user_data)
        .is_ok_and(|user_data| user_data.permissions().mode() & 0o7777 == 0o600);
    if !private {
        return Err(
            "the stand-in: others than its owner can read the user data's file"
                .to_owned()
                .into(),
        );
    }
    let kept = dir.join(format!("user-data-{id}"));
    fs::copy(user_data, &kept).context("keeping the user data")?;
    let script = fs::read_to_string(&kept).context("reading the user data")?;
    let syntax = command("bash", Environment::Inherited)
        .args(["-o", "noexec"])
        .arg(&kept)
        .status();
    if !syntax.context("running `bash`")?.success() || has_placeholder(&script) {
        return Err(
            "the stand-in: the user data isn't a bash script with each placeholder filled"
                .to_owned()
                .into(),
        );
    }
    launch(dir, &id, tags, &script)?;
    // The launch's answer names the AMI first.
    let image = format!(r#"{{"ImageId": "{IMAGE}", "#);
    let instance = described(dir, &id)?.replacen('{', &image, 1);
    Ok(format!(r#"{{"Instances": [{instance}]}}"#))
}

/// Whether `script` holds a placeholder, such as `@GRANT@`, that was not filled.
fn has_placeholder(script: &str) -> bool {
    script.match_indices('@').any(|(at, _)| {
        let rest = &script.as_bytes()[at + 1..];
        let name = rest
            .iter()
            .take_while(|b| b.is_ascii_uppercase() || **b == b'_');
        let name = name.count();
        name > 0 && rest.get(name) == Some(&b'@')
    })
}

/// Places the instance `id`, pending, with the run that `tags` name, and starts its host's
/// `sandboxd` if the directory names one, as `script`, its user data, has it started.
fn launch(dir: &Path, id: &str, tags: &str, script: &str) -> Result<(), Failed> {
    let instance = dir.join("instances").join(id);
    fs::create_dir_all(&instance).context("placing the instance")?;
    // The last run tag, or all of `tags` if it holds none.
    let key = r#""Key":"pumpkin:run","Value":""#;
    let run = (tags.rfind(key))
        .map(|at| &tags[at + key.len()..])
        .and_then(|value| value.split_once('"'))
        .map_or(tags, |(run, _)| run);
    fs::write(instance.join("run"), run).context("writing the run")?;
    fs::write(instance.join("state"), "pending\n").context("writing the state")?;
    match text(&dir.join("sandboxd")) {
        Ok(sandboxd) => start_host(dir, id, &sandboxd, script),
        Err(_) => Ok(()),
    }
}

/// Starts the instance `id`'s host: `sandboxd`, the program at that path, with the grant and
/// the arguments that the start in `script`, the user data, gives the container's, and its
/// console.
fn start_host(dir: &Path, id: &str, sandboxd: &str, script: &str) -> Result<(), Failed> {
    let Some(start) = test_support::start_of(script) else {
        return Err("the stand-in: the user data holds no start"
            .to_owned()
            .into());
    };
    let Start {
        role_image,
        sandboxd_image,
        gateway,
        grant,
        ops,
        model,
        ..
    } = start;
    // The boot's lines for the pulls come first, each with the image's reference as the start
    // has it, which Docker's record of a pull by digest repeats.
    let pulls: Vec<String> = [role_image, sandboxd_image]
        .iter()
        .map(|image| format!("pumpkin-ec2: pulled {image}: {image}\n"))
        .collect();
    let console = dir.join(format!("console-{id}"));
    fs::write(&console, pulls.concat()).context("writing the console")?;
    let mut args = vec![gateway.to_string()];
    args.extend(ops.then(|| "--ops".to_owned()));
    args.extend(model.then(|| "--model".to_owned()));
    let grant = serde_json::to_value(&grant).context("writing the grant")?;
    let grant = grant.as_str().unwrap_or_default().to_owned();
    // Its output closes, so that `aws` ends as its call returns, and each line reaches the
    // console as it is printed.
    let console = OpenOptions::new()
        .append(true)
        .open(&console)
        .context("opening the console")?;
    let mut prefix = command("sed", Environment::Inherited);
    prefix.args(["--unbuffered", "s/^/pumpkin-ec2: sandboxd: /"]);
    prefix
        .stdin(Stdio::piped())
        .stdout(console.try_clone().context("opening the console")?);
    let mut prefix = prefix.stderr(console).spawn().context("running `sed`")?;
    let pipe = (prefix.stdin.take().map(OwnedFd::from))
        .ok_or_else(|| "`sed` has no standard input: expected a pipe".to_owned())?;
    let mut host = command(sandboxd, Environment::Inherited);
    host.args(args)
        .env("SANDBOXD_GRANT", grant)
        .stdin(Stdio::null());
    host.stderr(pipe.try_clone().context("duplicating the pipe")?)
        .stdout(pipe);
    let host = host.spawn().context("starting `sandboxd`")?;
    let pid = format!("{}\n", host.id());
    Ok(fs::write(dir.join(format!("host-{id}")), pid).context("writing the host")?)
}

/// The description of the instance `id`, which is terminated from now on if it is shutting down.
fn described(dir: &Path, id: &str) -> Result<String, Failed> {
    let instance = dir.join("instances").join(id);
    let state = text(&instance.join("state")).context("reading the state")?;
    let tags = match text(&instance.join("run")) {
        Ok(run) => format!(r#", "Tags": [{{"Key": "pumpkin:run", "Value": "{run}"}}]"#),
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("reading the run: {error}").into()),
    };
    if state == "shutting-down" {
        fs::write(instance.join("state"), "terminated\n").context("writing the state")?;
    }
    Ok(format!(
        r#"{{"InstanceId": "{id}", "State": {{"Name": "{state}"}}{tags}}}"#
    ))
}

/// The answer to `ec2 describe-instances`, with the filters that name a run or instance IDs.
fn describe_instances(dir: &Path, args: &[&str]) -> Result<String, Failed> {
    let (mut run, mut ids) = (None, None);
    for filter in args.get(3..).unwrap_or_default() {
        let values = |at: &str| filter.strip_prefix(at).map(str::to_owned);
        if let Some(values) = values("Name=tag:pumpkin:run,Values=") {
            run = Some(values).filter(|run| !run.is_empty());
        } else if let Some(values) = values("Name=instance-id,Values=") {
            ids = Some(values);
        } else {
            let message = format!("The filter '{filter}' is unknown to the stand-in.");
            return Err(fail("InvalidParameterValue", "DescribeInstances", &message));
        }
    }
    let mut instances: Vec<_> = fs::read_dir(dir.join("instances"))
        .into_iter()
        .flatten()
        .flatten()
        .collect();
    instances.sort_by_key(fs::DirEntry::file_name);
    let mut found = Vec::new();
    for instance in instances {
        let id = instance.file_name().to_string_lossy().into_owned();
        if !instance.path().is_dir() {
            continue;
        }
        let ran = text(&instance.path().join("run")).unwrap_or_default();
        let listed = |ids: &String| ids.split(',').any(|listed| listed == id);
        if run.as_ref().is_some_and(|run| *run != ran)
            || ids.as_ref().is_some_and(|ids| !listed(ids))
        {
            continue;
        }
        found.push(described(dir, &id)?);
    }
    if found.is_empty() {
        Ok(r#"{"Reservations": []}"#.to_owned())
    } else {
        Ok(format!(
            r#"{{"Reservations": [{{"Instances": [{}]}}]}}"#,
            found.join(", ")
        ))
    }
}

/// The answer to `ec2 terminate-instances`: each instance is shutting down, and its host's
/// `sandboxd` is sent SIGTERM.
fn terminate_instances(dir: &Path, args: &[&str]) -> Result<String, Failed> {
    for id in args.get(3..).unwrap_or_default() {
        let instance = dir.join("instances").join(id);
        if !instance.is_dir() {
            let message = format!("The instance ID '{id}' does not exist");
            return Err(fail(
                "InvalidInstanceID.NotFound",
                "TerminateInstances",
                &message,
            ));
        }
        fs::write(instance.join("state"), "shutting-down\n").context("writing the state")?;
        if let Ok(host) = text(&dir.join(format!("host-{id}"))) {
            let mut kill = command("kill", Environment::Inherited);
            kill.arg(host)
                .stderr(Stdio::null())
                .status()
                .context("running `kill`")?;
        }
    }
    Ok(r#"{"TerminatingInstances": []}"#.to_owned())
}

/// The answer to `scheduler create-schedule`: makes the schedule that the call names.
fn create_schedule(dir: &Path, args: &[&str]) -> Result<String, Failed> {
    let [group, name] = flags(
        args.get(2..).unwrap_or_default(),
        ["--group-name", "--name"],
    );
    let schedule = dir.join("schedules").join(group).join(name);
    if schedule.exists() {
        let message = format!("Schedule {name} already exists.");
        return Err(fail("ConflictException", "CreateSchedule", &message));
    }
    fs::create_dir_all(dir.join("schedules").join(group)).context("placing the group")?;
    fs::write(&schedule, "").context("making the schedule")?;
    Ok(format!(
        r#"{{"ScheduleArn": "arn:aws:scheduler:us-west-1:123456789012:schedule/{group}/{name}"}}"#
    ))
}

/// The answer to `scheduler delete-schedule`: removes the schedule that the call names.
fn delete_schedule(dir: &Path, args: &[&str]) -> Result<(), Failed> {
    let [group, name] = flags(
        args.get(2..).unwrap_or_default(),
        ["--group-name", "--name"],
    );
    let schedule = dir.join("schedules").join(group).join(name);
    if !schedule.exists() {
        let message = format!("Schedule {name} does not exist.");
        return Err(fail(
            "ResourceNotFoundException",
            "DeleteSchedule",
            &message,
        ));
    }
    Ok(fs::remove_file(schedule).context("removing the schedule")?)
}

/// The answer to `ec2 get-console-output` for the instance `id`.
fn console_output(dir: &Path, id: &str) -> Result<String, Failed> {
    if !dir.join("instances").join(id).is_dir() {
        let message = format!("The instance ID '{id}' does not exist");
        return Err(fail(
            "InvalidInstanceID.NotFound",
            "GetConsoleOutput",
            &message,
        ));
    }
    // As `aws` prints it, decoded from the base64 that EC2 sends, and escaped for JSON.
    let output = match fs::read_to_string(dir.join(format!("console-{id}"))) {
        Ok(console) => {
            let lines = console.split_terminator('\n');
            let lines = lines.map(|line| line.replace('\\', "\\\\").replace('"', "\\\"") + "\\n");
            let escaped = lines.collect::<Vec<_>>().concat();
            format!(r#", "Output": "{escaped}""#)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("reading the console: {error}").into()),
    };
    Ok(format!(
        r#"{{"InstanceId": "{id}"{output}, "Timestamp": "2026-11-08T00:00:00+00:00"}}"#
    ))
}
