//! The `pumpkin` command, run as an operator runs it: its exit status, what it says, and the
//! record of its run, whose `outcome.json` it prints. A module for each command: `run`, which
//! includes the model route of a workflow that has one reaching a stand-in model provider;
//! `wait`; and `driver`, for the process that drives each run, which `run` starts. The tests on
//! Docker are ignored, as the Docker provider's are, and fail without `PUMPKIN_SANDBOXD`, which
//! `cargo xtask ci --docker` sets; they reach Docker through the context that
//! `PUMPKIN_DOCKER_CONTEXT` names, `default` if it is unset, which they write into the
//! configuration that they give `run`.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test helper fails the test that calls it"
)]
#![expect(
    clippy::print_stderr,
    reason = "a failed test shows where its scratch directory is"
)]

use std::fmt::Display;
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;
use std::{env, fs, iter, thread};

use docker::{BuildContext, Engine, Recipe};
use expect_test::Expect;
use serde_json::{Value, json};
use stdx::process::Environment;
use tempfile::TempDir;

mod driver;
mod run;
mod wait;

const LWE: &str = include_str!("../../../schema/tests/fixtures/lwe.json");

/// The `pumpkin` that the tests run.
const PUMPKIN: &str = env!("CARGO_BIN_EXE_pumpkin");

/// A small public image, pinned by digest.
// Repeated in `providers/docker/tests/it.rs`, `crates/runtime/tests/it/docker.rs` and
// `crates/harness/tests/it/docker.rs`: change all four.
const BUSYBOX: &str =
    "busybox@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e";

/// What the final judge of the runs on Docker does: scores the payload by its size.
const SCORE: &str = r#"printf '{"raw_score": %s}' \
    $(wc -c < /pumpkin/in/payload/raw) > /pumpkin/out/result.json"#;

/// The directory test `name` runs `pumpkin` in, empty. Dropping it removes it, unless the test
/// failed, when it shows where it is.
struct Scratch(PathBuf, TempDir);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix(&format!("pumpkin-{name}-"))
            .tempdir()
            .unwrap();
        Self(dir.path().to_owned(), dir)
    }

    /// Writes `contents` into the directory's file `name`, making its directory if need be.
    fn write(&self, name: &str, contents: &(impl Display + ?Sized)) {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents.to_string()).unwrap();
    }

    /// Writes `contents` into the directory's file `name`, which every user may execute.
    fn write_executable(&self, name: &str, contents: &[u8]) {
        let path = self.0.join(name);
        fs::write(&path, contents).unwrap();
        fs::set_permissions(path, Permissions::from_mode(0o755)).unwrap();
    }

    /// Writes into the directory's file `name` a stand-in for a static `sandboxd`, which only a
    /// provider that starts no sandbox takes: the header of a 64-bit ELF executable with no
    /// program headers, which every user may execute.
    fn write_sandboxd(&self, name: &str) {
        let mut header = [0; 64];
        header[..5].copy_from_slice(b"\x7fELF\x02");
        self.write_executable(name, &header);
    }

    /// `pumpkin` with `args`, to run in the directory.
    fn pumpkin(&self, args: &[&str]) -> Command {
        self.program(PUMPKIN, args)
    }

    /// The `pumpkin` executable at `program` with `args`, to run in the directory.
    fn program(&self, program: impl AsRef<Path>, args: &[&str]) -> Command {
        let mut command = stdx::process::command(program.as_ref(), Environment::Inherited);
        command.args(args).current_dir(&self.0);
        command
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if thread::panicking() {
            self.1.disable_cleanup(true);
            eprintln!("the test's scratch directory is {}", self.0.display());
        }
    }
}

/// A command whose end [`check`] shows: one for it to run, or one that a test ran itself, to act
/// on it as it ran, and showed as [`report`] does, with what it found.
enum Step {
    Run(Command),
    Ran(String),
}

impl From<Command> for Step {
    fn from(command: Command) -> Self {
        Self::Run(command)
    }
}

/// Runs each of `steps` that a test didn't run itself, `pumpkin` in `scratch`, and checks what
/// became of each: its exit status; what it says on its standard error, but for the warning of
/// a rootful engine, shown only if it doesn't match `rootful`, whether the engine the commands
/// reach runs as root; and if it names a run's record, the record's files, its `outcome.json`
/// and whether it printed that, and else what it printed, if anything. Each shows
/// `scratch` as `<scratch>`, a run's id as `<id>`, and each of `shown` as its name.
fn check(
    scratch: &Scratch,
    steps: impl IntoIterator<Item = impl Into<Step>>,
    rootful: bool,
    shown: &[(&str, &str)],
    expect: &Expect,
) {
    let ran: Vec<String> = (steps.into_iter())
        .map(|step| match step.into() {
            Step::Run(command) => ran(command, &scratch.0, rootful),
            Step::Ran(text) => text,
        })
        .collect();
    let mut text = ran.join("\n");
    text = text.replace(scratch.0.to_str().unwrap(), "<scratch>");
    for (value, name) in shown {
        text = text.replace(value, name);
    }
    expect.assert_eq(&text);
}

/// What becomes of `command`, `pumpkin` in `dir`, as [`check`] shows it.
fn ran(mut command: Command, dir: &Path, rootful: bool) -> String {
    let invoked = invocation(&command);
    let output = command.output().unwrap();
    report(invoked, &output, dir, rootful)
}

/// `command` as a shell line, with the environment it sets, and its program as `pumpkin` if it
/// is the `pumpkin` that the tests run.
fn invocation(command: &Command) -> String {
    let set = command.get_envs().map(|(name, value)| match value {
        Some(value) => format!("{}={} ", name.display(), value.display()),
        None => format!("env -u {} ", name.display()),
    });
    let args = command.get_args().map(|arg| match arg.to_str().unwrap() {
        "" => "''",
        arg => arg,
    });
    let set: String = set.collect();
    let program = command.get_program().to_str().unwrap();
    let program = if program == PUMPKIN {
        "pumpkin"
    } else {
        program
    };
    let args: Vec<&str> = iter::once(program).chain(args).collect();
    format!("$ {set}{}", args.join(" "))
}

/// What `output` shows of the command that `invocation` gave, as [`check`] shows it.
fn report(invoked: String, output: &Output, dir: &Path, rootful: bool) -> String {
    let mut lines = vec![invoked];
    lines.push(match output.status.signal() {
        Some(signal) => format!("ended by signal: {signal}"),
        None => format!("exit status: {:?}", output.status.code()),
    });
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    let (warnings, said): (Vec<&str>, Vec<&str>) =
        (stderr.lines()).partition(|line| line.contains(" runs as root, "));
    if warnings.is_empty() == rootful {
        lines.push(format!(
            "warned of a rootful engine: {}",
            !warnings.is_empty()
        ));
    }
    lines.extend(said.into_iter().map(str::to_owned));
    let stdout = String::from_utf8(output.stdout.clone()).unwrap();
    let Some((id, record)) = record(&stderr) else {
        // `wait` prints an `outcome.json` without having started its run.
        if let Ok(outcome) = serde_json::from_str::<Value>(&stdout) {
            lines.push(format!("printed outcome.json: {outcome}"));
        } else if !stdout.is_empty() {
            lines.push("printed:".to_owned());
            let printed = stdout
                .lines()
                .map(|line| format!("  {line}").trim_end().to_owned());
            lines.extend(printed);
        }
        return lines.join("\n");
    };
    let record = dir.join(record);
    if record.join("outcome.json").exists() {
        lines.extend(recorded(&record, &stdout));
    } else {
        lines.push("outcome.json: none".to_owned());
        lines.push(format!(
            "printed the id alone: {}",
            stdout == format!("{id}\n")
        ));
    }
    lines.join("\n").replace(id, "<id>")
}

/// The id and the record of the run that `stderr`, `pumpkin`'s, says it records, if it does.
fn record(stderr: &str) -> Option<(&str, &str)> {
    (stderr.lines()).find_map(|line| {
        line.strip_prefix("pumpkin: run ")?
            .split_once(" records into ")
    })
}

/// The files of `record`, a run's, its run log's note on the upstream that served its model
/// route and its record of how each model request was answered, if it has a run log, what the
/// agent's `out` held of a model's answer, if anything, what the driver wrote to `driver.log`,
/// if anything, its `outcome.json`, and whether `stdout` is that.
fn recorded(record: &Path, stdout: &str) -> Vec<String> {
    let outcome = fs::read_to_string(record.join("outcome.json")).unwrap();
    let compact = serde_json::from_str::<Value>(&outcome).unwrap();
    let mut lines = vec![format!("record: {}", files(record, record).join(", "))];
    // A run that didn't start has no run log.
    let log = fs::read_to_string(record.join("log.jsonl")).unwrap_or_default();
    for line in log.lines() {
        let logged: Value = serde_json::from_str(line).unwrap();
        let note = logged["note"].as_str();
        if let Some(note) = note.filter(|note| note.starts_with("the agent's model route goes to "))
        {
            lines.push(format!("the run log notes: {note}"));
        }
        if logged["event"]["kind"] == "model_answered" {
            lines.push(format!("the run log records: {}", logged["event"]));
        }
    }
    if let Ok(answer) = fs::read_to_string(record.join("agent/out/answer")) {
        lines.push(format!(
            "the agent's answer: {}",
            answer.replace('\n', "\\n")
        ));
    }
    // A record whose `driver.log` was removed shows that in its files.
    let said = fs::read_to_string(record.join("driver.log")).unwrap_or_default();
    if !said.is_empty() {
        lines.push(format!("driver.log: {}", said.replace('\n', "\\n")));
    }
    lines.push(format!("outcome.json: {compact}"));
    lines.push(format!("printed outcome.json: {}", stdout == outcome));
    lines
}

/// The files and empty directories below `dir`, by their paths from `base`, sorted.
fn files(base: &Path, dir: &Path) -> Vec<String> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    paths.sort();
    let mut lines = Vec::new();
    for path in paths {
        let name = path.strip_prefix(base).unwrap().display().to_string();
        if path.is_dir() {
            let below = files(base, &path);
            if below.is_empty() {
                lines.push(format!("{name}/"));
            }
            lines.extend(below);
        } else {
            lines.push(name);
        }
    }
    lines
}

/// What `look` returns once it returns something, tried every 100 ms for a minute.
fn poll<T>(mut look: impl FnMut() -> Option<T>) -> T {
    for _ in 0..600 {
        if let Some(found) = look() {
            return found;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("nothing came in a minute");
}

/// The static `sandboxd` that `PUMPKIN_SANDBOXD` names.
fn sandboxd() -> String {
    let Some(sandboxd) = env::var_os("PUMPKIN_SANDBOXD") else {
        panic!(
            "`PUMPKIN_SANDBOXD` is unset: run the Docker tests with `cargo xtask ci --docker`, \
             which builds sandboxd and sets it"
        );
    };
    let sandboxd = PathBuf::from(sandboxd).canonicalize().unwrap();
    sandboxd.into_os_string().into_string().unwrap()
}

/// The image of the roles that run on Docker, which the provider builds on `engine`: `busybox`
/// with a world-writable `/workspace`, the fixture's agent's working directory.
fn image(engine: &Engine) -> String {
    let recipe = format!("FROM {BUSYBOX}\nRUN mkdir --mode=1777 /workspace\n");
    let (context, recipe) = (BuildContext::default(), Recipe::Text(recipe));
    let built = engine.build(&context, &recipe, &[], "pumpkin-tests");
    built.unwrap().image().as_str().to_owned()
}

/// The fixture's workflow, without the agent's model route, with `argv` as the agent's command,
/// the final judge as [`SCORE`] says, and every role running `image` with a small machine's
/// share.
fn workflow(image: &str, argv: &[&str]) -> Value {
    let mut ir: Value = serde_json::from_str(LWE).unwrap();
    ir["roles"][0].as_object_mut().unwrap().remove("egress");
    ir["roles"][0]["runtime"]["agent"]["command"]["argv"] = json!(argv);
    ir["roles"][2]["runtime"]["judge"]["command"]["argv"] = json!(["sh", "-c", SCORE]);
    for role in ir["roles"].as_array_mut().unwrap() {
        role["image"] = json!(image);
        role["needs"] = json!({ "cpu_millis": 500, "memory_bytes": 256 << 20 });
    }
    // Far later than the agent's end, so that a slow engine makes a run wait longer, never end
    // differently.
    ir["main"]["solo_then_judge"]["deadline_seconds"] = json!(300);
    ir
}

/// Sends the signal `name`, as `kill -s` takes it, to `target`: a process ID, or a process
/// group's, negated; whether there was one to signal.
fn signal(name: &str, target: i64) -> bool {
    let kill = stdx::process::command("kill", Environment::Inherited)
        .args(["-s", name, "--", &target.to_string()])
        .status();
    kill.unwrap().success()
}

/// The Docker context the tests on Docker reach: `PUMPKIN_DOCKER_CONTEXT`, or `default`.
fn context() -> String {
    env::var("PUMPKIN_DOCKER_CONTEXT").unwrap_or_else(|_| "default".to_owned())
}

/// The table of a configuration that names `context` as the one that reaches the engine.
fn docker_table(context: &str) -> String {
    format!("[docker]\ncontext = \"{context}\"\n")
}

/// What `docker info` prints of the stand-in's engine: a rootless one that enforces every limit,
/// on a host of 4 CPUs and 8 GiB of memory.
const INFO: &str = r#"{"MemoryLimit":true,"SwapLimit":true,"CpuCfsQuota":true,"PidsLimit":true,"SecurityOptions":["name=seccomp,profile=builtin","name=rootless"],"KernelVersion":"6.0","OperatingSystem":"stand-in","ServerVersion":"29.0.0","ClientInfo":{"Version":"29.0.0"},"NCPU":4,"MemTotal":8589934592}"#;

/// A stand-in `docker` in a scratch directory, for the `pumpkin run`s that [`run`](Self::run)
/// makes: an engine that has every image but those of the repository `absent`, creates no
/// container and lists none. It holds the driver at each of its commands that
/// [`hold`](Self::hold) names, such as `create`, until [`release`](Self::release) or the
/// driver's end; dropping it releases them all.
struct StandIn<'a> {
    scratch: &'a Scratch,
}

impl<'a> StandIn<'a> {
    fn new(scratch: &'a Scratch) -> Self {
        let dir = scratch.0.display();
        let digest = "0".repeat(64);
        let docker = format!(
            r#"#!/bin/sh
# `--context=<name>` comes first.
shift
if [ -e "{dir}/hold-$1" ]; then
    echo $PPID > "{dir}/at-$1.tmp" && mv "{dir}/at-$1.tmp" "{dir}/at-$1"
    # At most the minute that `poll` waits, so that the driver of a test killed meanwhile ends.
    i=0
    while [ -e "{dir}/hold-$1" ] && kill -0 $PPID 2>/dev/null && [ $i -lt 1200 ]; do
        sleep 0.05; i=$((i + 1))
    done
fi
case $1 in
    context) echo unix:///stand-in.sock ;;
    info) echo '{INFO}' ;;
    # `image inspect --format <format> -- <image>`
    image) case $6 in
        absent@*) echo "Error response from daemon: No such image: $6" >&2; exit 1 ;;
        *) echo sha256:{digest} ;;
    esac ;;
    container) ;;
    *) echo "the stand-in creates nothing" >&2; exit 1 ;;
esac
"#
        );
        scratch.write("bin/docker", &docker);
        let mode = Permissions::from_mode(0o755);
        fs::set_permissions(scratch.0.join("bin/docker"), mode).unwrap();
        scratch.write_sandboxd("sandboxd");
        scratch.write("pumpkin.toml", &docker_table("default"));
        scratch.write("stand-in.json", &workflow(BUSYBOX, &["true"]));
        let absent = format!("absent@sha256:{}", "1".repeat(64));
        scratch.write("absent.json", &workflow(&absent, &["true"]));
        Self { scratch }
    }

    /// Holds the driver at `command` from now on.
    fn hold(&self, command: &str) {
        self.scratch.write(&format!("hold-{command}"), "");
    }

    /// The driver's process ID, once the stand-in holds it at `command`.
    fn held(&self, command: &str) -> i64 {
        let at = self.scratch.0.join(format!("at-{command}"));
        poll(|| fs::read_to_string(&at).ok()?.trim().parse().ok())
    }

    /// Lets the driver go on from `command`.
    fn release(&self, command: &str) {
        let _ = fs::remove_file(self.scratch.0.join(format!("hold-{command}")));
    }

    /// `pumpkin run` of the workflow whose IR is in `ir`, and `rest`, against the stand-in, in a
    /// process group of its own, so that a signal to the group misses the test. Fails the test if
    /// it ignores or blocks SIGINT or SIGTERM, which `run` would inherit.
    fn run(&self, ir: &str, rest: &[&str]) -> Command {
        signals_at_default();
        let mut run = (self.scratch).pumpkin(&["run", ir, "--sandboxd", "sandboxd"]);
        run.args(["--runs", "runs", "--config", "pumpkin.toml"])
            .args(rest);
        let bin = self.scratch.0.join("bin");
        run.env("PATH", format!("{}:/usr/bin:/bin", bin.display()));
        run.process_group(0);
        run
    }
}

/// Fails the test if its thread ignores or blocks SIGINT or SIGTERM: the tests stand for a
/// terminal's foreground job, whose signals are neither, and a child inherits both.
fn signals_at_default() {
    let status = fs::read_to_string("/proc/thread-self/status").unwrap();
    let mask = |field: &str| {
        let hex = status
            .lines()
            .find_map(|line| line.strip_prefix(field))
            .unwrap();
        u64::from_str_radix(hex.trim(), 16).unwrap()
    };
    let signals = [libc::SIGINT, libc::SIGTERM].map(|number| 1_u64 << (number - 1));
    let signals = signals.into_iter().fold(0, |all, signal| all | signal);
    let (ignored, blocked) = (mask("SigIgn:") & signals, mask("SigBlk:") & signals);
    assert!(
        ignored == 0 && blocked == 0,
        "the test ignores SIGINT or SIGTERM ({ignored:#x}) or blocks them ({blocked:#x}), and \
         `pumpkin run` would inherit that: run the tests from a terminal's foreground job, as \
         `cargo xtask ci` there does"
    );
}

impl Drop for StandIn<'_> {
    fn drop(&mut self) {
        for command in ["context", "image", "create"] {
            self.release(command);
        }
    }
}
