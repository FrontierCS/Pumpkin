//! The `pumpkin` command, run as an operator runs it: its exit status, what it says, and the
//! record of its run, whose `outcome.json` it prints. A module for each command: `run`, which
//! includes the model route of a workflow that has one reaching a stand-in model provider;
//! `wait`; `cancel`; `reap`; `check`; `fmt`; `build`; `lock`; and `driver`, for the process that
//! drives each run, which `run` starts. `fmt` runs the formatter beside `pumpkin`, which `cargo xtask ci` puts
//! beside the tests' own. The tests on Docker are ignored, and write the context they reach
//! ([`test_support::docker_context`]) into the configuration that they give `run` and `build`.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test helper fails the test that calls it"
)]
#![expect(
    clippy::print_stderr,
    reason = "a failed test shows where its scratch directory is, and one that can't remove its \
              images says so"
)]

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::fmt::Display;
use std::fs::Permissions;
use std::io::Read;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};
use std::{fs, iter, thread};

use docker::{Docker, Reached};
use engine::{ContextName, Engine};
use expect_test::Expect;
use serde_json::{Value, json};
use stdx::process::Environment;
use tempfile::TempDir;
use test_support::{BUSYBOX, WORKFLOW, files};

mod build;
mod cancel;
mod check;
mod driver;
mod fmt;
mod lock;
mod reap;
mod run;
mod wait;

/// The `pumpkin` that the tests run.
const PUMPKIN: &str = env!("CARGO_BIN_EXE_pumpkin");

/// What the final judge of the runs on Docker does: scores the payload by its size.
const SCORE: &str = r#"printf '{"raw_score": %s}' \
    $(wc -c < /pumpkin/in/payload/raw) > /pumpkin/out/result.json"#;

/// The directory test `name` runs `pumpkin` in, empty but for its `.pumpkin/bin`, which holds
/// the `pumpkin` it runs ([`Scratch::pumpkin`]). Dropping it removes it, unless the test failed,
/// when it shows where it is.
struct Scratch(PathBuf, TempDir);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix(&format!("pumpkin-{name}-"))
            .tempdir()
            .unwrap();
        // `run` refuses a runs directory below one that its group may write, which the umask, or
        // a setgid temporary directory, could make this one, and names records by their
        // canonical path.
        fs::set_permissions(dir.path(), Permissions::from_mode(0o755)).unwrap();
        let scratch = Self(fs::canonicalize(dir.path()).unwrap(), dir);
        install(&scratch, ".pumpkin/bin");
        scratch
    }

    /// Writes `contents` into the directory's file `name`, making its directory if need be.
    fn write(&self, name: &str, contents: &(impl Display + ?Sized)) {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents.to_string()).unwrap();
    }

    /// Writes into the directory the task `name`, whose entry is `<name>/task.star`: the workflow
    /// `ir` gives, but for its images, which every role takes from the recipe `<name>/Dockerfile`
    /// (`busybox` with a world-writable `/workspace`), but a role whose image is [`RUNNER`], from
    /// `<name>/runner/Dockerfile` (`busybox` with a world-writable `/work`); and the marker
    /// `PUMPKIN.star`, which makes the directory the root of its tasks.
    fn write_task(&self, name: &str, ir: &Value) {
        self.write_task_in("", name, ir);
    }

    /// Writes the task `name` as [`write_task`](Self::write_task) does, in the directory `root`,
    /// the root of its tasks.
    fn write_task_in(&self, root: &str, name: &str, ir: &Value) {
        let at = |file: &str| {
            let dir = Path::new(root).join(name).join(file);
            dir.to_str().unwrap().to_owned()
        };
        self.write(
            &Path::new(root).join("PUMPKIN.star").display().to_string(),
            "# The root of the tasks.\n",
        );
        let recipe = format!("FROM {BUSYBOX}\nRUN mkdir --mode=1777 /workspace\n");
        self.write(&at("Dockerfile"), &recipe);
        let roles = ir["roles"].as_array().unwrap();
        let mut images = "IMAGE = image(\"Dockerfile\")\n".to_owned();
        if roles.iter().any(|role| role["image"] == RUNNER) {
            let recipe = format!("FROM {BUSYBOX}\nRUN mkdir --mode=1777 /work\n");
            self.write(&at("runner/Dockerfile"), &recipe);
            images.push_str("RUNNER = image(\"runner/Dockerfile\")\n");
        }
        let task = format!(
            "{images}\nworkflow(\n    name = {},\n    roles = {},\n    score = {},\n)\n",
            starlark(&ir["name"]),
            starlark(&ir["roles"]),
            starlark(&ir["score"]),
        );
        self.write(&at("task.star"), &task);
    }

    /// Writes `contents` into the directory's file `name`, which every user may execute, making
    /// its directory if need be.
    fn write_executable(&self, name: &str, contents: &[u8]) {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
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

    /// `pumpkin` with `args`, to run in the directory, as its `.pumpkin/bin` holds it.
    fn pumpkin(&self, args: &[&str]) -> Command {
        self.program(self.0.join(".pumpkin/bin/pumpkin"), args)
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

/// `value`, JSON, as the workflow language writes it, with each role's image as `IMAGE`, the
/// recipe's image.
fn starlark(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => serde_json::to_string(text).unwrap(),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(starlark).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(fields) => {
            let fields = fields
                .iter()
                .map(|(key, value)| match (key.as_str(), value) {
                    ("image", Value::String(image)) if image == RUNNER => {
                        "\"image\": RUNNER".to_owned()
                    }
                    ("image", Value::String(_)) => "\"image\": IMAGE".to_owned(),
                    _ => format!(
                        "{}: {}",
                        serde_json::to_string(key).unwrap(),
                        starlark(value)
                    ),
                });
            format!("{{{}}}", fields.collect::<Vec<_>>().join(", "))
        }
    }
}

/// The image that a role of a task that [`Scratch::write_task`] writes runs in place of the task's
/// one, `IMAGE`: one of its own, `RUNNER`, as a runner needs, which no judge's image may be.
const RUNNER: &str = "runner";

/// A command whose end [`check`] shows: one for it to run, or one that a test ran itself, to act
/// on it as it ran, and showed as [`report`] does, with what it found.
enum Step {
    Run(Command),
    /// A `pumpkin run` on the real engine, as [`with_room`] runs it.
    OnEngine(Command),
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
            Step::OnEngine(mut command) => {
                let invoked = invocation(&command);
                report(invoked, &with_room(&mut command), &scratch.0, rootful)
            }
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

/// What `command`, a `pumpkin run` on the real engine, printed once the engine had room for its
/// run: one refused only for the cores that other runs hold, as other tests' runs on the engine
/// may, is run again every second, for 10 minutes at most.
fn with_room(command: &mut Command) -> Output {
    let room = Instant::now() + Duration::from_mins(10);
    loop {
        let output = command.output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        let crowded = output.status.code() == Some(1)
            && stderr.contains("pumpkin: reserving on provider")
            && stderr.contains("run it once");
        if !crowded || Instant::now() >= room {
            return output;
        }
        thread::sleep(Duration::from_secs(1));
    }
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
        None => format!("env --unset={} ", name.display()),
    });
    // An argument that isn't UTF-8 shows with U+FFFD for each byte that isn't.
    let args = command.get_args().map(|arg| match arg.to_string_lossy() {
        arg if arg.is_empty() => Cow::Borrowed("''"),
        arg => arg,
    });
    let set: String = set.collect();
    let program = command.get_program().to_string_lossy();
    // As `Scratch::pumpkin` runs it.
    let program = if program.contains("/.pumpkin/") && program.ends_with("/bin/pumpkin") {
        Cow::Borrowed("pumpkin")
    } else {
        program
    };
    let args: Vec<Cow<str>> = iter::once(program).chain(args).collect();
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

/// The files of `record`, a run's, each sandbox's `sandbox.json` and console it holds, its run
/// log's notes on the upstreams that served its model routes and its record of how each model
/// request was answered, if it has a run log, what the agent's `out` and the first job's evidence
/// held of a model's answer, if anything, what the driver wrote to `driver.log`, if anything, its `outcome.json`, and whether
/// `stdout` is that.
fn recorded(record: &Path, stdout: &str) -> Vec<String> {
    let outcome = fs::read_to_string(record.join("outcome.json")).unwrap();
    let compact = serde_json::from_str::<Value>(&outcome).unwrap();
    let mut lines = vec![format!("record: {}", files(record).join(", "))];
    lines.extend(sandboxes(record));
    // A run that didn't start has no run log.
    let log = fs::read_to_string(record.join("log.jsonl")).unwrap_or_default();
    for line in log.lines() {
        let logged: Value = serde_json::from_str(line).unwrap();
        let note = logged["note"].as_str();
        if let Some(note) = note.filter(|note| note.contains("'s model route: ")) {
            lines.push(format!("the run log notes: {note}"));
        }
        if logged["event"]["kind"] == "model_answered" {
            lines.push(format!("the run log records: {}", logged["event"]));
        }
    }
    for (whose, path) in [
        ("the agent's", "agent/out/answer"),
        ("the final judge's", "jobs/0/out/evidence/answer"),
    ] {
        if let Ok(answer) = fs::read_to_string(record.join(path)) {
            lines.push(format!("{whose} answer: {}", answer.replace('\n', "\\n")));
        }
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

/// What `record`, a run's, keeps of its sandboxes, as [`test_support::sandboxes`] shows it, with
/// each container's ID, which Docker makes anew each run, as `<container>`.
fn sandboxes(record: &Path) -> Vec<String> {
    let container = |line: String| {
        let Some((before, after)) = line.split_once(r#""id":""#) else {
            return line;
        };
        let hex = |byte: u8| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte);
        let id = after.split('"').next().unwrap();
        if id.len() == 64 && id.bytes().all(hex) {
            format!(r#"{before}"id":"<container>{}"#, &after[64..])
        } else {
            line
        }
    };
    let lines = test_support::sandboxes(record).into_iter();
    lines.map(container).collect()
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

/// How `child` ended, waited for a minute at most, and what it printed: its standard output,
/// and `said` and then the rest of its standard error, from `stderr`.
fn ended(mut child: Child, mut stderr: impl Read, mut said: String) -> Output {
    let status = poll(|| child.try_wait().unwrap());
    stderr.read_to_string(&mut said).unwrap();
    let mut stdout = Vec::new();
    (child.stdout.take().unwrap().read_to_end(&mut stdout)).unwrap();
    Output {
        status,
        stdout,
        stderr: said.into_bytes(),
    }
}

/// Whether the process `pid` runs: a zombie has ended.
fn alive(pid: i64) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| !stat.contains(") Z "))
}

/// Writes the record `id` in `scratch`'s `runs`, as `pumpkin run --runs runs` leaves it, with
/// `driver.pid` unless `pid` is false, and `outcome` as its `outcome.json`, if it has one.
fn write_record(scratch: &Scratch, id: &str, pid: bool, outcome: Option<Value>) {
    let args = [
        "--runs".to_owned(),
        scratch.0.join("runs").display().to_string(),
        scratch.0.join("task.star").display().to_string(),
    ];
    scratch.write(
        &format!("runs/{id}/record.json"),
        &json!({ "record_version": "pumpkin.record/v0", "args": args }),
    );
    if pid {
        scratch.write(&format!("runs/{id}/driver.pid"), "4242\n");
    }
    if let Some(outcome) = outcome {
        scratch.write(&format!("runs/{id}/outcome.json"), &outcome);
    }
}

/// Writes the record `id` in `scratch`'s `runs` as [`write_record`] does, with `outcome`, of a
/// run on provider `local`, of the Docker context `gone`, and a stand-in `docker` in `scratch`'s
/// `bin` that has no such context, which says so as Docker's command line does; returns a `PATH`
/// that finds it.
fn write_gone(scratch: &Scratch, id: &str, outcome: Option<Value>) -> String {
    let docker = "#!/bin/sh\necho 'context \"gone\": context not found' >&2\nexit 1\n";
    write_on(scratch, id, "gone", outcome, docker)
}

/// Writes the record `id` in `scratch`'s `runs` as [`write_record`] does, with `outcome`, of a
/// run on provider `local`, of the Docker context `context`, and `docker`, a script, as the
/// stand-in `docker` in `scratch`'s `bin`; returns a `PATH` that finds it.
fn write_on(
    scratch: &Scratch,
    id: &str,
    context: &str,
    outcome: Option<Value>,
    docker: &str,
) -> String {
    write_record(scratch, id, true, outcome);
    let ready = json!({ "report": { "context": context } });
    let provided = json!({ "providers": { "local": ready } });
    scratch.write(&format!("runs/{id}/provider.json"), &provided);
    scratch.write_executable("bin/docker", docker.as_bytes());
    format!("{}:/usr/bin:/bin", scratch.0.join("bin").display())
}

/// Builds the images of the task `task`, which `write_task` wrote, on the engine that `config`
/// names, so that a `run` of it after finds them by their tags and says nothing of building them.
fn build_first(scratch: &Scratch, task: &str, config: &str) {
    let entry = format!("{task}/task.star");
    let built = scratch
        .pumpkin(&["build", &entry, "--config", config])
        .output();
    let built = built.unwrap();
    assert!(built.status.success(), "{built:?}");
}

/// The fixture's workflow, without the agent's model route, with `argv` as the agent's command,
/// the final judge as [`SCORE`] says, and every role running `image` with a small machine's
/// share.
fn workflow(image: &str, argv: &[&str]) -> Value {
    let mut ir: Value = serde_json::from_str(WORKFLOW).unwrap();
    ir["roles"][0].as_object_mut().unwrap().remove("egress");
    ir["roles"][0]["runtime"]["agent"]["command"]["argv"] = json!(argv);
    ir["roles"][2]["runtime"]["judge"]["command"]["argv"] = json!(["sh", "-c", SCORE]);
    for role in ir["roles"].as_array_mut().unwrap() {
        role["image"] = json!(image);
        role["needs"] = json!({ "cpu_millis": 500, "memory_bytes": 256 << 20 });
    }
    // Far later than the agent's end, so that a slow engine makes a run wait longer, never end
    // differently.
    ir["roles"][0]["runtime"]["agent"]["deadline_seconds"] = json!(300);
    ir
}

/// How many CPUs the machine has online, all of which the Docker provider gives sandboxes.
fn machine_cpus() -> u64 {
    cpu_list(&fs::read_to_string("/sys/devices/system/cpu/online").unwrap()).len() as u64
}

/// How many cores the machine's online CPUs are on, each CPU's core by the CPUs it shares it
/// with.
fn machine_cores() -> u64 {
    let online = cpu_list(&fs::read_to_string("/sys/devices/system/cpu/online").unwrap());
    let cores: BTreeSet<BTreeSet<u32>> = online.into_iter().map(siblings).collect();
    cores.len() as u64
}

/// The CPUs that `text`, as the kernel writes a list of them, names.
fn cpu_list(text: &str) -> BTreeSet<u32> {
    let ranges = text.trim().split(',').filter(|range| !range.is_empty());
    ranges
        .flat_map(|range| {
            let (first, last) = range.split_once('-').unwrap_or((range, range));
            first.parse::<u32>().unwrap()..=last.parse().unwrap()
        })
        .collect()
}

/// The CPUs that share a core with `cpu`, `cpu` among them.
fn siblings(cpu: u32) -> BTreeSet<u32> {
    let path = format!("/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list");
    cpu_list(&fs::read_to_string(path).unwrap())
}

/// Sends the signal `name`, as `kill --signal` takes it, to `target`: a process ID, or a process
/// group's, negated; whether there was one to signal.
fn signal(name: &str, target: i64) -> bool {
    let kill = stdx::process::command("kill", Environment::Inherited)
        .args(["--signal", name, "--", &target.to_string()])
        .status();
    kill.unwrap().success()
}

/// The table of a configuration that names `context` as the one that reaches the engine.
fn docker_table(context: &str) -> String {
    format!("[docker]\ncontext = \"{context}\"\n")
}

/// The table of a configuration's one provider, `local`, of kind `docker`, in whose one class
/// each role of [`workflow`]'s fits.
const LOCAL: &str = "[providers.local.docker]\n";

/// Makes `scratch`'s directory `dir`, and links `pumpkin` there; returns the link's path.
fn install(scratch: &Scratch, dir: &str) -> PathBuf {
    let dir = scratch.0.join(dir);
    fs::create_dir_all(&dir).unwrap();
    // A link, which writes nothing where a copy would write the whole executable.
    fs::hard_link(PUMPKIN, dir.join("pumpkin")).unwrap();
    // The formatter that `PUMPKIN_BUILDIFIER` names, as `cargo xtask ci` and `cargo xtask
    // mutants` set it, which `pumpkin fmt` and `pumpkin check` run from beside the `pumpkin` that
    // runs.
    if let Some(buildifier) = std::env::var_os("PUMPKIN_BUILDIFIER") {
        fs::hard_link(buildifier, dir.join("buildifier")).unwrap();
    }
    dir.join("pumpkin")
}

/// The engine of the Docker context `context`.
fn engine(context: &str) -> Engine {
    let context = ContextName::try_from(context.to_owned()).unwrap();
    Engine::connect(context).unwrap()
}

/// The containers of the Docker context `context`, as a reap reaches them.
fn containers(context: &str) -> Reached {
    Docker::reach(ContextName::try_from(context.to_owned()).unwrap()).unwrap()
}

/// A configuration of the context `context` alone, which places the fixture's roles as
/// [`LOCAL`] does.
fn machine(context: &str) -> String {
    format!("{}\n{LOCAL}", docker_table(context))
}

/// What a recipe holds that the stand-in `docker` builds ([`StandIn`]).
const BUILDS: &str = "# The stand-in builds this recipe.";

/// What `docker info` prints of the stand-in's engine: a rootless one that enforces every limit,
/// on a host of 8 GiB of memory.
const INFO: &str = r#"{"MemoryLimit":true,"SwapLimit":true,"CpuCfsQuota":true,"CPUSet":true,"PidsLimit":true,"SecurityOptions":["name=seccomp,profile=builtin","name=rootless"],"KernelVersion":"6.0","OperatingSystem":"stand-in","ServerVersion":"29.0.0","ClientInfo":{"Version":"29.0.0"},"MemTotal":8589934592}"#;

/// A stand-in `docker` in a scratch directory, for the `pumpkin run`s that [`run`](Self::run)
/// makes: an engine whose one image is that of the task `stand-in`, ID all zeros, which lacks the
/// task `lacking`'s, ID all ones, and fails to build the task `unbuilt`'s. It builds a recipe
/// that holds [`BUILDS`] as the image of ID all twos, tagged as it is then asked; creates and
/// lists no container; and leaves `docker.ran` once run. It holds the driver at each command that [`hold`](Self::hold) names, such as `create`, until
/// [`release`](Self::release) or the driver's end; dropping it releases them all.
struct StandIn<'a> {
    scratch: &'a Scratch,
}

impl<'a> StandIn<'a> {
    /// The stand-in for an engine that [`INFO`] describes.
    fn new(scratch: &'a Scratch) -> Self {
        Self::with(scratch, INFO)
    }

    /// The stand-in for an engine of which `docker info` prints `info`, with the scratch
    /// directory's name as its ID ([`ledger`]).
    fn with(scratch: &'a Scratch, info: &str) -> Self {
        let id = scratch.0.file_name().unwrap().to_str().unwrap();
        let info = info.replace(r#""MemTotal""#, &format!(r#""ID":"{id}","MemTotal""#));
        let dir = scratch.0.display();
        let digest = format!("sha256:{}", "0".repeat(64));
        let absent = format!("sha256:{}", "1".repeat(64));
        let built = format!("sha256:{}", "2".repeat(64));
        let docker = format!(
            r#"#!/bin/sh
touch "{dir}/docker.ran"
# `--context=<name>` comes first.
shift
# The build's look for an image by its tag, `image inspect --format {{{{.Id}}}} -- <tag>`, is a
# command of its own to hold.
name=$1
case "$1 $6" in
    "image pumpkin/"*) name=lookup ;;
esac
if [ -e "{dir}/hold-$name" ]; then
    echo $PPID > "{dir}/at-$name.tmp" && mv "{dir}/at-$name.tmp" "{dir}/at-$name"
    # At most the minute that `poll` waits, so that the driver of a test killed meanwhile ends.
    i=0
    while [ -e "{dir}/hold-$name" ] && kill -0 $PPID 2>/dev/null && [ $i -lt 1200 ]; do
        sleep 0.05; i=$((i + 1))
    done
fi
case $1 in
    context) echo unix:///stand-in.sock ;;
    info) echo '{info}' ;;
    # `image inspect --format <format> -- <image>`, which says it has no image that the engine
    # lacks, or for the tag `pumpkin/lacking:<digest>`, the image that the engine lacks; for the
    # tag `pumpkin/unbuilt:<digest>`, no image, which a build then fails to make.
    image) case $6 in
        pumpkin/lacking:*) echo {absent} ;;
        pumpkin/built:*) grep -qxF "$6" "{dir}/tagged" 2>/dev/null && echo {built} ||
            {{ echo "Error: No such image: $6" >&2; exit 1; }} ;;
        pumpkin/unbuilt:*) echo "Error: No such image: $6" >&2; exit 1 ;;
        {absent}) echo "Error response from daemon: No such image: $6" >&2; exit 1 ;;
        *) echo {digest} ;;
    esac ;;
    container) ;;
    # `build ... --file <recipe> --iidfile <file> -- <stage>`, which writes the image's ID there.
    build) for arg; do
        case $previous in
            --file) grep --quiet --fixed-strings "{BUILDS}" "$arg" || {{ echo "the stand-in creates nothing" >&2; exit 1; }} ;;
            --iidfile) echo {built} > "$arg" ;;
        esac
        previous=$arg
    done ;;
    # `tag -- <image> <tag>`.
    tag) echo "$4" >> "{dir}/tagged" ;;
    *) echo "the stand-in creates nothing" >&2; exit 1 ;;
esac
"#
        );
        scratch.write("bin/docker", &docker);
        let mode = Permissions::from_mode(0o755);
        fs::set_permissions(scratch.0.join("bin/docker"), mode).unwrap();
        scratch.write_sandboxd("sandboxd");
        scratch.write("pumpkin.toml", &machine("default"));
        scratch.write_task("stand-in", &workflow(&digest, &["true"]));
        scratch.write_task("lacking", &workflow(&absent, &["true"]));
        scratch.write_task("unbuilt", &workflow(&digest, &["true"]));
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

    /// `pumpkin run` of the task whose entry is `entry`, and `rest`, against the stand-in, in a
    /// process group of its own, so that a signal to the group misses the test. Fails the test if
    /// it ignores or blocks SIGINT or SIGTERM, which `run` would inherit.
    fn run(&self, entry: &str, rest: &[&str]) -> Command {
        signals_at_default();
        let mut run = (self.scratch).pumpkin(&["run", entry, "--sandboxd", "sandboxd"]);
        run.args(["--runs", "runs", "--config", "pumpkin.toml"])
            .args(rest);
        let bin = self.scratch.0.join("bin");
        run.env("PATH", format!("{}:/usr/bin:/bin", bin.display()));
        run.process_group(0);
        run
    }
}

/// `command`, in a process group of its own, started by a shell that runs `setup`, such as
/// `umask 002`, then executes `command` in its place, which so inherits what `setup` set.
fn through_shell(command: &Command, setup: &str) -> Command {
    let mut shell = stdx::process::command("/bin/sh", Environment::Inherited);
    shell.args(["-c", &format!("{setup} && exec \"$@\""), "sh"]);
    shell.arg(command.get_program()).args(command.get_args());
    for (name, value) in command.get_envs() {
        match value {
            Some(value) => shell.env(name, value),
            None => shell.env_remove(name),
        };
    }
    if let Some(dir) = command.get_current_dir() {
        shell.current_dir(dir);
    }
    shell.process_group(0);
    shell
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
        let _ = fs::remove_dir_all(ledger(self.scratch));
    }
}

/// The ledger of the CPUs that runs on the stand-in's engine, of `scratch`, hold: an engine of
/// its own, so that the stand-in's runs never wait for others.
fn ledger(scratch: &Scratch) -> PathBuf {
    Path::new("/var/tmp/pumpkin-cpus").join(scratch.0.file_name().unwrap())
}
