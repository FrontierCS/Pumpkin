//! The `pumpkin` command, run as an operator runs it: its exit status, what it says, and the
//! record of its run, whose `outcome.json` it prints. `fmt` runs the formatter beside `pumpkin`,
//! which `cargo xtask ci` puts beside the tests' own. The tests on Docker are ignored, and write
//! the context they reach ([`test_support::docker_context`]) into the configuration that they
//! give `run` and `build`.

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
mod controller;
mod driver;
mod fmt;
mod lock;
mod ls;
mod reap;
mod run;
mod wait;

const PUMPKIN: &str = env!("CARGO_BIN_EXE_pumpkin");

/// What the final judge of the runs on Docker does: scores the payload by its size.
const SCORE: &str = r#"printf '{"raw_score": %s}' \
    $(wc -c < /pumpkin/in/payload/raw) > /pumpkin/out/result.json"#;

/// The directory test `name` runs `pumpkin` in, empty but for its workspace's `.pumpkin/bin`,
/// which holds the `pumpkin` it runs ([`Scratch::pumpkin`]). Dropping it removes it, unless the
/// test failed, when it shows where it is.
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

    /// `pumpkin` with `args`, to run in the directory, as its workspace's `.pumpkin/bin` holds
    /// it.
    fn pumpkin(&self, args: &[&str]) -> Command {
        self.program(self.0.join(".pumpkin/bin/pumpkin"), args)
    }

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
    // As `Scratch::pumpkin` runs it, or one that a `./pumpkin` built in its workspace.
    let program = if program.contains("/.pumpkin/") && program.ends_with("/bin/pumpkin") {
        Cow::Borrowed("pumpkin")
    } else {
        program
    };
    let args: Vec<Cow<str>> = iter::once(program).chain(args).collect();
    format!("$ {set}{}", args.join(" "))
}

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
    lines.extend(steady(&said));
    let stdout = String::from_utf8(output.stdout.clone()).unwrap();
    let Some((id, record)) = record(&stderr) else {
        // `wait` prints an `outcome.json` without having started its run.
        if let Ok(outcome) = serde_json::from_str::<Value>(&stdout) {
            lines.push(format!("printed outcome.json: {}", steady_outcome(outcome)));
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

/// `said`, lines of `pumpkin`'s standard error, with the times of a run's summary and of its
/// milestones, which a run on the stand-ins takes as it goes, as `<t>`: each word of their lines
/// that is a time or a span.
fn steady(said: &[&str]) -> Vec<String> {
    let timed = |word: &str| {
        let word = word.trim_end_matches([',', ';', ':']);
        let clock = word
            .split(':')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
        let span = !word.is_empty()
            && word
                .trim_end_matches(|c: char| c.is_ascii_digit() || "hms.".contains(c))
                .is_empty()
            && word.ends_with(['h', 'm', 's'])
            && word.starts_with(|c: char| c.is_ascii_digit());
        (clock && word.contains(':')) || span
    };
    let mut summary = false;
    let lines = said.iter().map(|line| {
        let first =
            line.starts_with("pumpkin: run ") && line.ends_with(':') && line.contains(" ran ");
        summary = first || (summary && line.starts_with("  "));
        // A milestone, said as `wait` follows a run, starts with its time.
        let milestone = (line.strip_prefix("pumpkin:"))
            .and_then(|rest| rest.split_whitespace().next())
            .is_some_and(|word| timed(word) && word.contains(':'));
        if !summary && !milestone {
            return (*line).to_owned();
        }
        let words = line.split(' ').map(|word| {
            if timed(word) {
                let kept = &word[word.trim_end_matches([',', ';', ':']).len()..];
                format!("<t>{kept}")
            } else {
                word.to_owned()
            }
        });
        words.collect::<Vec<String>>().join(" ")
    });
    lines.collect()
}

/// `outcome`, an `outcome.json`, with the times of its usage, which a run on the stand-ins takes
/// as it goes, as `<ns>`, and what its sandboxes cost by them as `<usd>`.
fn steady_outcome(mut outcome: Value) -> Value {
    if let Some(usage) = outcome.get_mut("usage") {
        usage["ran_ns"] = "<ns>".into();
        for sandboxes in usage["sandboxes"].as_array_mut().unwrap() {
            sandboxes["lasted_ns"] = "<ns>".into();
            if let Some(usd) = sandboxes.get_mut("usd") {
                *usd = "<usd>".into();
            }
        }
        if let Some(usd) = usage.get_mut("usd") {
            *usd = "<usd>".into();
        }
    }
    outcome
}

/// The id and the record of the run that `stderr`, `pumpkin`'s, says it records, or, of a run on
/// a controller, that it copied here, if it does.
fn record(stderr: &str) -> Option<(&str, &str)> {
    (stderr.lines()).find_map(|line| {
        let line = line.strip_prefix("pumpkin: ")?;
        if let Some(line) = line.strip_prefix("run ") {
            return line.split_once(" records into ");
        }
        let (id, from) = line
            .strip_prefix("copying run ")?
            .split_once("'s record from ")?;
        Some((id, from.split_once(" to ")?.1))
    })
}

/// The files of `record`, a run's, each sandbox's `sandbox.json` and console it holds, the model
/// routes that `provider.json` holds, if a role has one, its run log's record of how each model
/// request was answered, if it has a run log, what the agent's `out` and the first job's evidence
/// held of a model's answer, if anything, what the driver wrote to `driver.log`, if anything, its
/// `outcome.json`, and whether `stdout` is that.
fn recorded(record: &Path, stdout: &str) -> Vec<String> {
    let outcome = fs::read_to_string(record.join("outcome.json")).unwrap();
    let compact = serde_json::from_str::<Value>(&outcome).unwrap();
    let mut lines = vec![format!("record: {}", files(record).join(", "))];
    lines.extend(sandboxes(record));
    // A run that didn't start has no `provider.json`.
    if let Ok(provided) = fs::read_to_string(record.join("provider.json")) {
        let routes = &serde_json::from_str::<Value>(&provided).unwrap()["routes"];
        if routes.as_object().is_some_and(|routes| !routes.is_empty()) {
            lines.push(format!("provider.json routes: {routes}"));
        }
    }
    // A run that didn't start has no run log.
    let log = fs::read_to_string(record.join("log.jsonl")).unwrap_or_default();
    for line in log.lines() {
        let logged: Value = serde_json::from_str(line).unwrap();
        if logged["event"]["kind"] == "model_answered" {
            let mut event = logged["event"].clone();
            for time in ["first_byte_ns", "first_token_ns"] {
                if let Some(ns) = event.get_mut(time) {
                    *ns = json!("<ns>");
                }
            }
            lines.push(format!("the run log records: {event}"));
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
    lines.push(format!("outcome.json: {}", steady_outcome(compact)));
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

/// `ran`, what `ls` printed, with the times that turn on the clock as `<t>`: how long the phase
/// of a live run, whose id starts with `ids`, and each of its sandboxes, on the lines under it,
/// have gone on, since its last model answer, how long its model requests have been in flight,
/// and when the live runs end.
fn masked(ran: &str, ids: &str) -> String {
    let lines = ran.lines().map(|line| {
        let mut line = line.to_owned();
        // A line under a run's starts 25 columns further in, where its id would end.
        let indent = line.len() - line.trim_start().len();
        let row = line.trim_start().starts_with(ids).then_some(indent);
        if let Some(at) = row.or_else(|| (indent > 25).then(|| indent - 25)) {
            let at = at + 37;
            line.replace_range(at..at + 7, "    <t>");
        }
        for (from, to) in [(" answered ", " ago"), ("ends by ", " UTC")] {
            if let Some(at) = line.rfind(from)
                && let Some(end) = line[at..].find(to)
            {
                line.replace_range(at + from.len()..at + end, "<t>");
            }
        }
        if let Some(at) = line.find(" in flight")
            && let Some(span) = line[at..].find("for ")
        {
            let span = at + span + "for ".len();
            let end = line[span..].find(';').map_or(line.len(), |end| span + end);
            line.replace_range(span..end, "<t>");
        }
        line
    });
    lines.collect::<Vec<String>>().join("\n")
}

/// The IR of the task, `example/lease`, of the runs whose records the tests of `ls` and `wait`
/// write ([`write_run`]), as `ir.json` holds it: an agent, `solver`, with a model route and a
/// deadline of 30 minutes, which may call the train judge, `trainer`, which may lease a `box`,
/// and lease a `gpu` runner for a minute and a `cpu` runner for 30; and the final judge,
/// `scorer`. `trainer`, `scorer` and `gpu` each need an `NVIDIA A100-SXM4-80GB`.
fn lease_task() -> Value {
    let gpu = json!({ "gpu": { "count": 1, "model": "NVIDIA A100-SXM4-80GB" } });
    let lease = |name: &str, seconds: u64| json!({ "lease": { "name": name, "role": name, "instance_seconds": seconds } });
    json!({
        "name": "example/lease",
        "score": { "role": "scorer" },
        "roles": [
            {
                "name": "solver",
                "runtime": { "agent": { "deadline_seconds": 1800 } },
                "egress": { "model": {} },
                "grants": [
                    { "judge": { "name": "train", "role": "trainer" } },
                    lease("gpu", 60),
                    lease("cpu", 1800),
                ],
            },
            { "name": "trainer", "needs": gpu, "grants": [lease("box", 60)] },
            { "name": "scorer", "needs": gpu },
            { "name": "gpu", "needs": gpu },
            { "name": "cpu" },
            { "name": "box" },
        ],
    })
}

/// Where a run of [`lease_task`] places its roles, as `provider.json` holds it: the agent and the
/// `cpu` runner on `aws/judge`, which starts within 2 minutes, the `box` on `aws/box`, and the
/// judges and the `gpu` runner on `modal/a100_80`, which starts within 30; at $0.3672 an hour on
/// `aws/judge` and $2.5 on `modal/a100_80` if `priced`, and never on `aws/box`.
fn lease_places(priced: bool) -> Value {
    let place = |provider: &str, class: &str| json!({ "provider": provider, "class": class });
    let class = |startup_seconds: u64, usd_per_hour: f64| {
        let mut class = json!({ "startup_seconds": startup_seconds });
        if priced {
            class["usd_per_hour"] = json!(usd_per_hour);
        }
        class
    };
    json!({
        "roles": {
            "solver": place("aws", "judge"),
            "cpu": place("aws", "judge"),
            "box": place("aws", "box"),
            "trainer": place("modal", "a100_80"),
            "scorer": place("modal", "a100_80"),
            "gpu": place("modal", "a100_80"),
        },
        "providers": {
            "aws": { "classes": { "judge": class(120, 0.3672), "box": { "startup_seconds": 60 } } },
            "modal": { "classes": { "a100_80": class(1800, 2.5) } },
        },
    })
}

/// A record of a run log, of `event` and the core's `actions`, written `seconds` after the run
/// started; [`write_run`] numbers it.
fn logged(seconds: u64, event: Value, actions: Value) -> Value {
    let mut record = json!({ "at_ns": (seconds * 1_000_000_000).to_string() });
    record["event"] = event;
    record["actions"] = actions;
    record
}

/// The `outcome.json` of a run of [`lease_task`] whose final judge scored `returned`, what it
/// returned, as `score`, after the agent exited 0.
fn judged(score: f64, returned: Value) -> Value {
    let mut outcome = json!({
        "state": "finished",
        "score": score,
        "verdict": { "kind": "judged", "result": { "score": score } },
        "work_end": { "kind": "agent_exited", "status": { "code": 0 } },
    });
    outcome["verdict"]["result"]["returned"] = returned;
    outcome
}

/// The log of a run of [`lease_task`] that went the whole way, but its start, by the second
/// since the run started: the agent's three model requests, each from its start, two reporting
/// their tokens and their first token's time; four train judge calls, one scored, whose call
/// leases a box that a file is put into and a script runs in, one never started, one stopped and
/// one timed out, with no score; three leases of the agent's, a `gpu` runner whose sandbox sees
/// the GPU its role needs, closed by the agent, a `cpu` runner whose command times out, closed as
/// the work phase ends, and a `gpu` runner whose sandbox sees another GPU, which expires; and the
/// final judge's score, 49.36, after a model request whose stream begins and states a rate limit.
#[expect(clippy::too_many_lines, reason = "one line for each record")]
fn judged_log() -> Vec<Value> {
    let none = || json!([]);
    let tick = || json!({ "kind": "tick" });
    let started = |request: u64| json!({ "kind": "model_request_started", "caller": "agent", "request": request });
    let answered = |request: u64, usage: Value, first_token_ns: Option<&str>| {
        let mut answered = json!({
            "kind": "model_answered", "caller": "agent", "request": request,
            "answer": { "kind": "forwarded" }, "upstream_status": 200, "usage": usage,
        });
        if let Some(first_token_ns) = first_token_ns {
            answered["first_token_ns"] = json!(first_token_ns);
        }
        answered
    };
    let ask = |job: u64| {
        let request = json!({ "kind": "judge", "grant": "train" });
        let timeout = "600000000000";
        let start =
            json!({ "kind": "start_judge", "job": job, "grant": "train", "timeout_ns": timeout });
        (
            json!({ "kind": "op_arrived", "request": request }),
            json!([start]),
        )
    };
    let of_job = |kind: &str, job: u64| json!({ "kind": kind, "job": job });
    let of_lease = |kind: &str, lease: u64| json!({ "kind": kind, "lease": lease });
    let open = |grant: &str, lease: u64| {
        let request = json!({ "kind": "lease_open", "grant": grant });
        let start =
            json!({ "kind": "start_lease", "grant": grant, "holder": "agent", "lease": lease });
        (
            json!({ "kind": "op_arrived", "request": request }),
            json!([start]),
        )
    };
    let run = |lease: u64, argv: &[&str]| json!([{ "kind": "run_command", "lease": lease, "command": 0, "argv": argv }]);
    let ended = |lease: u64, end: Value| json!({ "kind": "command_ended", "lease": lease, "command": 0, "end": end });
    let exited = || json!({ "exited": { "code": 0 } });
    let saw = |sandbox: &str, gpu: &str| json!({ "kind": "sandbox_gpus", "sandbox": sandbox, "gpus": [gpu] });
    let close = |lease: u64| json!({ "kind": "close_lease", "lease": lease });
    let collected = |job: u64, result: Value| json!({ "kind": "judge_collected", "job": job, "result": result });
    let failed =
        |job: u64, reason: &str| json!({ "kind": "judge_failed", "job": job, "reason": reason });
    let sxm = "NVIDIA A100-SXM4-80GB";
    let tokens = |input: u64, output: u64, cached: u64| json!({ "input_tokens": input, "output_tokens": output, "cached_tokens": cached });
    let mut reasoned = tokens(1000, 100, 800);
    reasoned["reasoning_tokens"] = json!(40);
    let (asked_0, start_0) = ask(0);
    let (asked_1, start_1) = ask(1);
    let (asked_2, start_2) = ask(2);
    let (asked_3, start_3) = ask(3);
    let (opened_1, start_1_lease) = open("gpu", 1);
    let (opened_2, start_2_lease) = open("cpu", 2);
    let (opened_3, start_3_lease) = open("gpu", 3);
    let boxed = json!([{ "kind": "start_lease", "grant": "box", "holder": "job-0", "lease": 0 }]);
    let put = json!([{
        "kind": "transfer", "lease": 0, "command": 0, "direction": "put",
        "from": "/pumpkin/in/payload", "to": "/work/in",
    }]);
    let moved = json!({
        "kind": "transfer_ended", "lease": 0, "command": 0,
        "end": { "moved": { "bytes": 1, "entries": 1 } },
    });
    let closing = json!({ "kind": "op_arrived", "request": { "kind": "lease_close", "lease": 1 } });
    let unstarted = "the judge's sandbox didn't start: Modal refused the helper's `create`: \
                     ResourceExhaustedError: Insufficient capacity to create sandbox: fix what \
                     Modal reports";
    let long = "x".repeat(200);
    let unscored = json!({ "returned": { "exit_status": 1, "stderr_tail": long } });
    let final_judge = json!([{
        "kind": "start_judge", "job": 4, "grant": null, "timeout_ns": "900000000000",
    }]);
    let returned = json!({ "raw_score": 49.36 });
    let finish = json!([{ "kind": "finish", "outcome": judged(49.36, returned.clone()) }]);
    let exit = json!({ "kind": "agent_exited", "status": { "code": 0 } });
    let judge_started = json!({ "kind": "model_request_started", "caller": "job-4", "request": 3 });
    let judge_answered = json!({
        "kind": "model_answered", "caller": "job-4", "request": 3, "answer": { "kind": "forwarded" },
        "upstream_status": 200, "failed": { "code": "rate_limit_exceeded" },
        "first_token_ns": "1013000000000",
        "kept": { "request": { "dropped_headers": ["x-stainless-lang", "x-title"] } },
    });
    vec![
        logged(5, json!({ "kind": "agent_started" }), none()),
        logged(8, started(0), none()),
        logged(10, answered(0, reasoned, Some("9250000000")), none()),
        logged(15, started(1), none()),
        logged(
            20,
            answered(1, tokens(2000, 200, 1800), Some("16500000000")),
            none(),
        ),
        logged(26, started(2), none()),
        logged(31, answered(2, Value::Null, None), none()),
        logged(40, asked_0, start_0),
        logged(46, of_job("judge_started", 0), none()),
        logged(50, tick(), boxed),
        logged(55, of_lease("lease_ready", 0), none()),
        logged(56, tick(), put),
        logged(57, moved, none()),
        logged(58, tick(), run(0, &["sh", "-c", "set -e\nprint 7"])),
        logged(62, ended(0, exited()), none()),
        logged(
            100,
            collected(0, json!({ "score": 49.53, "returned": {} })),
            json!([close(0)]),
        ),
        logged(102, of_lease("lease_ended", 0), none()),
        logged(120, opened_1, start_1_lease),
        logged(120, opened_2, start_2_lease),
        logged(126, saw("lease-1", sxm), none()),
        logged(126, of_lease("lease_ready", 1), none()),
        logged(130, tick(), run(1, &["nvidia-smi"])),
        logged(132, ended(1, exited()), none()),
        logged(140, closing, json!([close(1)])),
        logged(145, of_lease("lease_ended", 1), none()),
        logged(150, of_lease("lease_ready", 2), none()),
        logged(151, tick(), run(2, &["sleep", "60"])),
        logged(181, ended(2, json!("timed_out")), none()),
        logged(200, asked_1, start_1),
        logged(260, failed(1, unstarted), none()),
        logged(300, asked_2, start_2),
        logged(310, saw("job-2", sxm), none()),
        logged(310, of_job("judge_started", 2), none()),
        logged(320, tick(), json!([of_job("stop_judge", 2)])),
        logged(330, failed(2, "the call was stopped"), none()),
        logged(400, asked_3, start_3),
        logged(405, of_job("judge_started", 3), none()),
        logged(410, of_job("judge_timed_out", 3), none()),
        logged(415, collected(3, unscored), none()),
        logged(500, opened_3, start_3_lease),
        logged(505, saw("lease-3", "NVIDIA A100 80GB PCIe"), none()),
        logged(505, of_lease("lease_ready", 3), none()),
        logged(506, tick(), run(3, &["true"])),
        logged(507, ended(3, exited()), none()),
        logged(565, tick(), json!([close(3)])),
        logged(570, of_lease("lease_ended", 3), none()),
        logged(1000, exit, json!([{ "kind": "tear_down_agent" }, close(2)])),
        json!({ "at_ns": "1002000000000", "note": "collected from the agent's sandbox: out, 1 file of 6 bytes" }),
        logged(1005, of_lease("lease_ended", 2), none()),
        logged(1006, tick(), final_judge),
        logged(1010, saw("job-4", sxm), none()),
        logged(1010, of_job("judge_started", 4), none()),
        logged(1012, judge_started, none()),
        logged(1020, judge_answered, none()),
        logged(
            1100,
            collected(4, json!({ "score": 49.36, "returned": returned })),
            finish,
        ),
    ]
}

/// The records of `log` written by `seconds` after the run started.
fn cut(log: &[Value], seconds: u64) -> Vec<Value> {
    let written = |record: &&Value| {
        let at: u64 = record["at_ns"].as_str().unwrap().parse().unwrap();
        at <= seconds * 1_000_000_000
    };
    log.iter().filter(written).cloned().collect()
}

/// Writes the record `id` in `scratch`'s `runs`, as [`write_record`] does, with `outcome`, of a
/// run of [`lease_task`] on `places` that started `started` after the Unix epoch, with a hard
/// end two hours on, whose log holds its start, then `log`, each record numbered in turn, and a
/// `sandbox.json` for each sandbox that the log says started, deleted; returns its directory.
fn write_run(
    scratch: &Scratch,
    id: &str,
    started: Duration,
    log: &[Value],
    outcome: Option<Value>,
    places: &Value,
) -> PathBuf {
    write_record(scratch, id, true, outcome);
    let dir = scratch.0.join("runs").join(id);
    scratch.write(&format!("runs/{id}/ir.json"), &lease_task());
    scratch.write(&format!("runs/{id}/provider.json"), places);
    let unix_ns = started.as_nanos().to_string();
    let start =
        json!({ "kind": "run_started", "unix_ns": unix_ns, "hard_end_ns": "7200000000000" });
    let start = logged(0, start, json!([{ "kind": "start_agent" }]));
    let mut lines = String::new();
    for (seq, record) in (0_u64..).zip(iter::once(&start).chain(log)) {
        let mut record = record.clone();
        record["seq"] = json!(seq);
        lines.push_str(&record.to_string());
        lines.push('\n');
        let event = &record["event"];
        let sandbox = match event["kind"].as_str() {
            Some("agent_started") => Some("agent".to_owned()),
            Some("judge_started") => Some(format!("jobs/{}", event["job"])),
            Some("lease_ready") => Some(format!("leases/{}", event["lease"])),
            _ => None,
        };
        if let Some(sandbox) = sandbox {
            let deleted = json!({ "delete": { "state": "deleted" } });
            scratch.write(&format!("runs/{id}/{sandbox}/sandbox.json"), &deleted);
        }
    }
    scratch.write(&format!("runs/{id}/log.jsonl"), &lines);
    dir
}

/// When the run `id` started, since the Unix epoch: `seconds` after it was asked for.
fn asked(id: &str, seconds: u64) -> Duration {
    runtime::minted(id).unwrap() + Duration::from_secs(seconds)
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

/// The table of a provider, `aws`, of kind `ec2`, whose settings name an envelope.
const EC2: &str = r#"[providers.aws.ec2]
region = "us-west-1"
classes = { agent = { template = "lt-0aaaaaaaaaaaaaaaa", version = 7, vcpus = 32, memory_mib = 262144 }, judge = { template = "lt-0bbbbbbbbbbbbbbbb", version = 2, vcpus = 8, memory_mib = 16384 } }
repositories = { roles = "pumpkin/roles", sandboxd = "pumpkin/sandboxd" }
timer = { group = "pumpkin", role_arn = "arn:aws:iam::123456789012:role/pumpkin-timer" }
"#;

/// The table of a provider, `gpus`, of kind `modal`, whose settings name a workspace and a
/// repository, and its one class, `h100`.
const MODAL: &str = r#"[providers.gpus.modal]
profile = "pumpkin"
dials_from = "203.0.113.7"
images = { region = "us-west-1", repository = "pumpkin/roles", role_arn = "arn:aws:iam::123456789012:role/pumpkin-modal-pull" }
classes = { h100 = { gpu = "H100!", gpu_model = "NVIDIA H100 80GB HBM3", cpu_millis = 4000, memory_bytes = 17179869184, startup_seconds = 1800 } }
"#;

/// Makes `scratch`'s directory `dir`, and links `pumpkin` there; returns the link's path.
fn install(scratch: &Scratch, dir: &str) -> PathBuf {
    let dir = scratch.0.join(dir);
    fs::create_dir_all(&dir).unwrap();
    // A link, which writes nothing where a copy would write the whole executable; `pumpkin` finds
    // its workspace by the path it runs from, this one.
    fs::hard_link(PUMPKIN, dir.join("pumpkin")).unwrap();
    // The formatter that `PUMPKIN_BUILDIFIER` names, as `cargo xtask ci` and `cargo xtask
    // mutants` set it, which `pumpkin fmt` and `pumpkin check` run from beside the `pumpkin` that
    // runs.
    if let Some(buildifier) = std::env::var_os("PUMPKIN_BUILDIFIER") {
        fs::hard_link(buildifier, dir.join("buildifier")).unwrap();
    }
    dir.join("pumpkin")
}

/// The engine of the Docker context `context`, as `scratch`'s `pumpkin` reaches it.
fn engine(scratch: &Scratch, context: &str) -> Engine {
    let context = ContextName::try_from(context.to_owned()).unwrap();
    Engine::connect(context, &scratch.0.join(".pumpkin/docker")).unwrap()
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
/// on a host of 8 GiB of memory, and keeps its images in the containerd image store.
const INFO: &str = r#"{"MemoryLimit":true,"SwapLimit":true,"CpuCfsQuota":true,"CPUSet":true,"PidsLimit":true,"DriverStatus":[["driver-type","io.containerd.snapshotter.v1"]],"SecurityOptions":["name=seccomp,profile=builtin","name=rootless"],"KernelVersion":"6.0","OperatingSystem":"stand-in","ServerVersion":"29.0.0","ClientInfo":{"Version":"29.0.0"},"MemTotal":8589934592}"#;

/// What `docker image inspect` prints of the stand-in's engine's image: a manifest for its
/// platform, `linux/amd64`.
const IMAGE: &str = r#"{"Os":"linux","Architecture":"amd64","Descriptor":{"mediaType":"application/vnd.oci.image.manifest.v1+json"}}"#;

/// A stand-in `docker` in a scratch directory, for the `pumpkin run`s that [`run`](Self::run)
/// makes: an engine whose one image is that of the task `stand-in`, ID all zeros, which lacks the
/// task `lacking`'s, ID all ones, and fails to build the task `unbuilt`'s; others are those that
/// the directory's `images` lists, by ID, size and tags. It builds a recipe that holds [`BUILDS`]
/// as the image of ID all twos, tagged as it is then asked; creates and lists no container; notes
/// each image removal and cache prune in `docker.pruned`; and leaves `docker.ran` once run. It
/// holds the driver at each command that [`hold`](Self::hold) names, such as `create`, until
/// [`release`](Self::release) or the driver's end; dropping it releases them all.
struct StandIn<'a> {
    scratch: &'a Scratch,
}

impl<'a> StandIn<'a> {
    /// The stand-in for an engine that [`INFO`] and [`IMAGE`] describe.
    fn new(scratch: &'a Scratch) -> Self {
        Self::with(scratch, INFO, IMAGE)
    }

    /// The stand-in for an engine of which `docker info` prints `info`, with the scratch
    /// directory's name as its ID ([`ledger`]), and `docker image inspect` prints `image` of its
    /// image.
    #[expect(clippy::too_many_lines, reason = "most of it is the stand-in's script")]
    fn with(scratch: &'a Scratch, info: &str, image: &str) -> Self {
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
case "$1 $2 $4" in
    "image inspect "*.Id*) name=lookup ;;
    "image inspect "*.Size*) name=size ;;
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
    version) echo linux/amd64 ;;
    # `image ls --all --no-trunc --quiet`, `image inspect --format <format> -- <image>`, which
    # says it has no image that the engine lacks, or for
    # the tag `pumpkin/lacking:<digest>`, the image that the engine lacks, `image inspect --format
    # {{{{.Id}}}} -- <tag>`; for the tag `pumpkin/unbuilt:<digest>`, no image, which a build
    # then fails to make; and `image inspect --format {{{{.Size}}}} -- <image>`
    image) case $2 in
        ls) echo {digest}; cut --delimiter=' ' --fields=1 "{dir}/images" 2>/dev/null || true ;;
        rm) echo "docker $*" >> "{dir}/docker.pruned" ;;
        *) case $name in
            lookup) case $6 in
                pumpkin/lacking:*) echo {absent} ;;
                pumpkin/built:*) grep --quiet --line-regexp --fixed-strings "$6" "{dir}/tagged" 2>/dev/null && echo {built} ||
                    {{ echo "Error: No such image: $6" >&2; exit 1; }} ;;
                pumpkin/unbuilt:*) echo "Error: No such image: $6" >&2; exit 1 ;;
                *) echo {digest} ;;
            esac ;;
            size) sed --quiet "s/^$6 //p" "{dir}/images" 2>/dev/null | grep . || echo 0 ;;
            *) if [ "$6" = {absent} ]; then echo "Error response from daemon: No such image: $6" >&2; exit 1; fi
               echo '{image}' ;;
        esac ;;
    esac ;;
    container) ;;
    builder) echo "docker $*" >> "{dir}/docker.pruned" ;;
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
