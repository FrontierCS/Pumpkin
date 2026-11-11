//! The `pumpkin` command, run as an operator runs it: its exit status, what it says, and the
//! record of its run, whose `outcome.json` it prints. A module for each command: `run`, which
//! includes the model route of a workflow that has one reaching a stand-in model provider;
//! `wait`; and `driver`, for the process that drives each run. The tests on Docker are ignored,
//! as the Docker provider's are, and fail without `PUMPKIN_SANDBOXD`, which `cargo xtask ci
//! --docker` sets; they reach Docker through the context that `PUMPKIN_DOCKER_CONTEXT` names,
//! `default` if it is unset.

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

    /// `pumpkin` with `args`, to run in the directory.
    fn pumpkin(&self, args: &[&str]) -> Command {
        let pumpkin = env!("CARGO_BIN_EXE_pumpkin");
        let mut command = stdx::process::command(pumpkin, Environment::Inherited);
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

/// Runs each of `commands`, `pumpkin` in `scratch`, and checks what becomes of it: its exit
/// status; what it says on its standard error, but for the warning of a rootful engine, shown
/// only if it doesn't match `rootful`, whether the engine the commands reach runs as root; and
/// if it names a run's record, the record's files, its `outcome.json` and whether it printed
/// that, and else the first line it printed, if any. Each shows `scratch` as `<scratch>`, a
/// run's id as `<id>`, and each of `shown` as its name.
fn check(
    scratch: &Scratch,
    commands: impl IntoIterator<Item = Command>,
    rootful: bool,
    shown: &[(&str, &str)],
    expect: &Expect,
) {
    let ran: Vec<String> = (commands.into_iter())
        .map(|command| ran(command, &scratch.0, rootful))
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

/// `command` as a shell line, with the environment it sets.
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
    let args: Vec<&str> = iter::once("pumpkin").chain(args).collect();
    format!("$ {set}{}", args.join(" "))
}

/// What `output` shows of the command that `invocation` gave, as [`check`] shows it.
fn report(invoked: String, output: &Output, dir: &Path, rootful: bool) -> String {
    let mut lines = vec![invoked];
    lines.push(format!("exit status: {:?}", output.status.code()));
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
        } else {
            let first = stdout.lines().next();
            lines.extend(first.map(|line| format!("printed first: {line}")));
        }
        return lines.join("\n");
    };
    lines.extend(recorded(&dir.join(record), &stdout));
    lines.join("\n").replace(id, "<id>")
}

/// The id and the record of the run that `stderr`, `pumpkin`'s, names, if it names one.
fn record(stderr: &str) -> Option<(&str, &str)> {
    let named = stderr
        .lines()
        .find_map(|line| line.strip_prefix("pumpkin: run "));
    named?.split_once(" records into ")
}

/// The files of `record`, a run's, its run log's note on the upstream that served its model
/// route and its record of how each model request was answered, what the agent's `out` held of
/// a model's answer, if anything, its `outcome.json`, and whether `stdout` is that.
fn recorded(record: &Path, stdout: &str) -> Vec<String> {
    let outcome = fs::read_to_string(record.join("outcome.json")).unwrap();
    let compact = serde_json::from_str::<Value>(&outcome).unwrap();
    let mut lines = vec![format!("record: {}", files(record, record).join(", "))];
    let log = fs::read_to_string(record.join("log.jsonl")).unwrap();
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
        role["resources"] = json!({ "cpu_millis": 500, "memory_bytes": 256 << 20 });
    }
    // Far later than the agent's end, so that a slow engine makes a run wait longer, never end
    // differently.
    ir["main"]["solo_then_judge"]["deadline_seconds"] = json!(300);
    ir
}
