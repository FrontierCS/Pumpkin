//! The `pumpkin` command, run as an operator runs it: its exit status, what it says, and the
//! record of its run, whose `outcome.json` it prints, with the model route of a workflow that
//! has one reaching a stand-in model provider ([`StandIn`]). The test on Docker is ignored, as the
//! Docker provider's are, and fails without `PUMPKIN_SANDBOXD`, which `cargo xtask ci --docker`
//! sets; it reaches Docker through the context that `PUMPKIN_DOCKER_CONTEXT` names, `default`
//! if it is unset.

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
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread::JoinHandle;
use std::time::Duration;
use std::{env, fs, iter, thread};

use docker::{ContextName, Engine};
use expect_test::{Expect, expect};
use provider::Provider as _;
use schema::provider::RunTag;
use schema::sandbox::{MODEL_PORT, SANDBOXCTL_PATH};
use serde_json::{Value, json};
use stdx::process::Environment;
use tempfile::TempDir;

const LWE: &str = include_str!("../../schema/tests/fixtures/lwe.json");

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

/// Usage errors exit with 2, and `--help` with 0.
#[test]
fn usage() {
    let scratch = Scratch::new("usage");
    let run = ["run", "lwe.json", "--docker-context", "nowhere"];
    let commands = [
        scratch.pumpkin(&[]),
        scratch.pumpkin(&["--help"]),
        scratch.pumpkin(&run),
        scratch.pumpkin(&[&run[..], &["--sandboxd", "sandboxd", "--quiet"]].concat()),
    ];
    check(
        &scratch,
        commands,
        false,
        &[],
        &expect![[r#"
        $ pumpkin
        exit status: Some(2)
        pumpkin: subcommand is required: see `pumpkin --help`
        $ pumpkin --help
        exit status: Some(0)
        printed first: pumpkin
        $ pumpkin run lwe.json --docker-context nowhere
        exit status: Some(2)
        pumpkin: flag is required: `--sandboxd`: see `pumpkin --help`
        $ pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --quiet
        exit status: Some(2)
        pumpkin: unexpected flag: `--quiet`: see `pumpkin --help`"#]],
    );
}

/// What `run` finds wrong before it reaches the engine exits with 1: a Docker context's empty
/// name, an IR that can't be read or isn't one, and `HOME` unset or empty where it places the
/// runs. The context `nowhere` reaches no engine, so reaching for it would fail otherwise.
#[test]
fn unrunnable() {
    let scratch = Scratch::new("unrunnable");
    scratch.write("lwe.json", &serde_json::from_str::<Value>(LWE).unwrap());
    scratch.write("empty.json", &json!({}));
    let run = |ir, context, rest: &[&str]| {
        let mut command = scratch.pumpkin(&["run", ir, "--docker-context", context]);
        command.args(["--sandboxd", "sandboxd"]).args(rest);
        command
    };
    let mut unset = run("lwe.json", "nowhere", &[]);
    unset.env_remove("HOME");
    let mut empty = run("lwe.json", "nowhere", &[]);
    empty.env("HOME", "");
    let runs = ["--runs", "runs"];
    let commands = [
        run("lwe.json", "", &runs),
        run("missing.json", "nowhere", &runs),
        run("empty.json", "nowhere", &runs),
        unset,
        empty,
    ];
    check(
        &scratch,
        commands,
        false,
        &[],
        &expect![[r#"
        $ pumpkin run lwe.json --docker-context '' --sandboxd sandboxd --runs runs
        exit status: Some(1)
        pumpkin: the Docker context's name is empty: name one that `docker context ls` lists
        $ pumpkin run missing.json --docker-context nowhere --sandboxd sandboxd --runs runs
        exit status: Some(1)
        pumpkin: missing.json: No such file or directory (os error 2)
        $ pumpkin run empty.json --docker-context nowhere --sandboxd sandboxd --runs runs
        exit status: Some(1)
        pumpkin: empty.json: missing field `ir_version` at line 1 column 2
        $ env -u HOME pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd
        exit status: Some(1)
        pumpkin: HOME is unset or empty: set it, or name a directory with `--runs`
        $ HOME= pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd
        exit status: Some(1)
        pumpkin: HOME is unset or empty: set it, or name a directory with `--runs`"#]],
    );
}

/// Runs on Docker: a run killed while its agent sleeps leaves its `driver.pid` unlocked and no
/// `outcome.json`, so `wait` says it is lost. The run's `docker` children, which have run by
/// then, hold no lock.
#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn lost_on_docker() {
    let scratch = Scratch::new("lost");
    let sandboxd = sandboxd();
    let context = env::var("PUMPKIN_DOCKER_CONTEXT").unwrap_or_else(|_| "default".to_owned());
    let engine = Engine::connect(ContextName::try_from(context.clone()).unwrap()).unwrap();
    let image = image(&scratch, &engine);
    scratch.write("sleeps.json", &workflow(&image, &["sleep", "300"]));
    let mut run = scratch.pumpkin(&["run", "sleeps.json", "--docker-context", &context]);
    run.args(["--sandboxd", &sandboxd, "--runs", "runs"]);
    let run = run.stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    let mut killed = Killed {
        run: run.unwrap(),
        engine: &engine,
        id: None,
    };
    // A record appears locked, so its directory is the run's.
    let record = scratch.0.join("runs");
    let id = poll(|| {
        let entries = fs::read_dir(&record).ok()?;
        entries
            .filter_map(|entry| entry.unwrap().file_name().into_string().ok())
            .find(|name| !name.starts_with('.'))
    });
    killed.id = Some(id.clone());
    let tag = RunTag::try_from(id.clone()).unwrap();
    poll(|| Some(()).filter(|()| !engine.list(&tag).unwrap().is_empty()));
    killed.run.kill().unwrap();
    killed.run.wait().unwrap();
    let wait = scratch.pumpkin(&["wait", &id, "--runs", "runs"]);
    check(
        &scratch,
        [wait],
        false,
        &[(&id, "<id>")],
        &expect![[r#"
        $ pumpkin wait <id> --runs runs
        exit status: Some(1)
        pumpkin: run <id> is lost: its driver ended without writing outcome.json, so nothing says how it ended: remove what it left, which is its containers, labelled `pumpkin.run=<id>`, and its socket directory, `pumpkin-<id>` in the temporary directory"#]],
    );
}

/// A `pumpkin run` to kill, and what it leaves on Docker to remove, once the test is over,
/// however it ends: the containers of the run `id` and its socket directory.
struct Killed<'a> {
    run: Child,
    engine: &'a Engine,
    id: Option<String>,
}

impl Drop for Killed<'_> {
    fn drop(&mut self) {
        let _ = self.run.kill();
        let _ = self.run.wait();
        let Some(id) = self.id.take() else { return };
        let _ = fs::remove_dir_all(env::temp_dir().join(format!("pumpkin-{id}")));
        let Ok(tag) = RunTag::try_from(id) else {
            return;
        };
        if let Ok(listed) = self.engine.list(&tag) {
            let ids: Vec<_> = listed.into_iter().map(|listed| listed.id).collect();
            let _ = self.engine.delete(&tag, &ids);
        }
    }
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

/// `wait` finds the `outcome.json` of each run recorded in `scratch`'s `runs`, which have
/// ended: the engine is not reached.
fn wait_for_each_run(scratch: &Scratch) {
    let mut ids: Vec<String> = (fs::read_dir(scratch.0.join("runs")).unwrap())
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    ids.sort();
    let waits: Vec<Command> = (ids.iter())
        .map(|id| scratch.pumpkin(&["wait", id, "--runs", "runs"]))
        .collect();
    let shown: Vec<(&str, &str)> = ids.iter().map(|id| (&id[..], "<id>")).collect();
    check(
        scratch,
        waits,
        false,
        &shown,
        &expect![[r#"
            $ pumpkin wait <id> --runs runs
            exit status: Some(3)
            pumpkin: run <id> finished without a score: the agent submitted nothing, and the work phase ended when the agent exited
            printed outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            $ pumpkin wait <id> --runs runs
            exit status: Some(0)
            printed outcome.json: {"score":3,"state":"finished","verdict":{"kind":"judged","result":{"returned":{"raw_score":3},"score":3},"submission":{"finalized":true,"payload":{"digest":"sha256:44778d82365e4af681c40d5f0eef5cf6f5899d3f0ac335050a7ed6779cf3f674","size":3}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}"#]],
    );
}

/// Writes the record `id` in `scratch`'s `runs`, as `pumpkin run` leaves it, with `driver.pid`
/// unless `pid` is false, and `outcome` as its `outcome.json`, if it has one.
fn write_record(scratch: &Scratch, id: &str, pid: bool, outcome: Option<Value>) {
    scratch.write(
        &format!("runs/{id}/record.json"),
        &json!({ "record_version": "pumpkin.record/v0" }),
    );
    if pid {
        scratch.write(&format!("runs/{id}/driver.pid"), "4242\n");
    }
    if let Some(outcome) = outcome {
        scratch.write(&format!("runs/{id}/outcome.json"), &outcome);
    }
}

/// `wait` prints the `outcome.json` of a run that has ended and exits as `run` does, by its
/// state and score, for the run named by its id, by a prefix of it or of its random digits, or
/// by its path. It refuses a name that matches no run or several, listing them, a run whose
/// driver ended without an `outcome.json`, and a record that has no `driver.pid`.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn waits() {
    let scratch = Scratch::new("waits");
    let finished = |score: Value| json!({ "state": "finished", "score": score });
    let ended = |id, outcome| write_record(&scratch, id, true, Some(outcome));
    ended("20261107-120000-1b8a6389", finished(json!(3)));
    ended("20261107-120100-2c9b0001", finished(json!(null)));
    ended(
        "20261107-120200-2c9b0002",
        json!({ "state": "failed", "reason": "r" }),
    );
    ended(
        "20261107-120300-3d0c0003",
        json!({ "state": "failed", "reason": "f" }),
    );
    // A record in the making is not there yet, though it matches.
    write_record(&scratch, ".20261107-120600-2c9b0099.partial", true, None);
    scratch.write("runs/notes/todo", "");
    let mut no_home = scratch.pumpkin(&["wait", "runs/20261107-120000-1b8a6389"]);
    no_home.env_remove("HOME");
    let wait = |run| scratch.pumpkin(&["wait", run, "--runs", "runs"]);
    let commands = [
        wait("20261107-120000-1b8a6389"),
        wait("1b8a"),
        wait("20261107-1201"),
        scratch.pumpkin(&["wait", "runs/20261107-120200-2c9b0002"]),
        wait("2c9b"),
        wait("20261107"),
        wait("notes"),
        scratch.pumpkin(&["wait", "runs/notes"]),
        scratch.pumpkin(&["wait", "nowhere/x"]),
        no_home,
        scratch.pumpkin(&["wait", "6a", "--runs", "nowhere"]),
        wait("6a"),
        wait(""),
    ];
    check(
        &scratch,
        commands,
        false,
        &[],
        &expect![[r#"
            $ pumpkin wait 20261107-120000-1b8a6389 --runs runs
            exit status: Some(0)
            printed outcome.json: {"score":3,"state":"finished"}
            $ pumpkin wait 1b8a --runs runs
            exit status: Some(0)
            printed outcome.json: {"score":3,"state":"finished"}
            $ pumpkin wait 20261107-1201 --runs runs
            exit status: Some(3)
            pumpkin: run 20261107-120100-2c9b0001 finished without a score: the verdict has no score, after the work phase ended in a way this pumpkin doesn't know
            printed outcome.json: {"score":null,"state":"finished"}
            $ pumpkin wait runs/20261107-120200-2c9b0002
            exit status: Some(1)
            pumpkin: run 20261107-120200-2c9b0002 failed: r
            printed outcome.json: {"reason":"r","state":"failed"}
            $ pumpkin wait 2c9b --runs runs
            exit status: Some(1)
            pumpkin: `2c9b` matches 2 runs: name one by more of its id
              20261107-120100-2c9b0001
              20261107-120200-2c9b0002
            $ pumpkin wait 20261107 --runs runs
            exit status: Some(1)
            pumpkin: `20261107` matches 4 runs: name one by more of its id
              20261107-120000-1b8a6389
              20261107-120100-2c9b0001
              20261107-120200-2c9b0002
              20261107-120300-3d0c0003
            $ pumpkin wait notes --runs runs
            exit status: Some(1)
            pumpkin: runs/notes has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait runs/notes
            exit status: Some(1)
            pumpkin: runs/notes has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait nowhere/x
            exit status: Some(1)
            pumpkin: nowhere/x is not a directory: expected the directory of a run's record
            $ env -u HOME pumpkin wait runs/20261107-120000-1b8a6389
            exit status: Some(0)
            printed outcome.json: {"score":3,"state":"finished"}
            $ pumpkin wait 6a --runs nowhere
            exit status: Some(1)
            pumpkin: nowhere: No such file or directory (os error 2): expected the directory that runs are recorded in, as `--runs` names it
            $ pumpkin wait 6a --runs runs
            exit status: Some(1)
            pumpkin: no run in runs matches `6a`: name an id, a unique prefix of an id or of its random digits, or a record's path
            $ pumpkin wait '' --runs runs
            exit status: Some(1)
            pumpkin: the run is empty: name an id, a prefix of one, or a record's path"#]],
    );
}

/// `wait` follows a live run, one whose driver holds its `driver.pid`'s lock: it says so, and
/// ends with the outcome once the driver has written its `outcome.json` and let go. The test's
/// driver does so after `wait`'s line that it follows the run, which comes before it blocks.
#[test]
fn follows() {
    let scratch = Scratch::new("follows");
    let id = "20261107-120000-1b8a6389";
    write_record(&scratch, id, true, None);
    let driver = File::open(scratch.0.join(format!("runs/{id}/driver.pid"))).unwrap();
    driver.lock().unwrap();
    let mut command = scratch.pumpkin(&["wait", "1b8a", "--runs", "runs"]);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let invoked = invocation(&command);
    let mut child = command.spawn().unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut said = String::new();
    stderr.read_line(&mut said).unwrap();
    let outcome = json!({ "state": "finished", "score": 7 });
    scratch.write(&format!("runs/{id}/outcome.json"), &outcome);
    drop(driver);
    stderr.read_to_string(&mut said).unwrap();
    let mut output = child.wait_with_output().unwrap();
    output.stderr = said.into_bytes();
    let text = report(invoked, &output, &scratch.0, false);
    expect![[r#"
        $ pumpkin wait 1b8a --runs runs
        exit status: Some(0)
        pumpkin: following run 20261107-120000-1b8a6389: ending `pumpkin wait` does not stop it
        printed outcome.json: {"score":7,"state":"finished"}"#]]
    .assert_eq(&text);
}

/// `wait` on a record in each state it can be in: its `outcome.json` absent, of each kind, or
/// unreadable; its `driver.pid` absent, free or locked; and its `record.json` of this version,
/// of another, or absent. A live record of this version, locked with no outcome, is `follows`'s.
/// A record that is no record of this version, or has no `driver.pid`, is refused.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn states() {
    let scratch = Scratch::new("states");
    let unscored = r#"{"state":"finished","score":null,"verdict":{"kind":"no_submission"},"work_end":{"kind":"deadline"}}"#;
    let outcomes = [
        ("no outcome", None),
        ("scored", Some(r#"{"state":"finished","score":3}"#)),
        ("unscored", Some(unscored)),
        ("failed", Some(r#"{"state":"failed","reason":"r"}"#)),
        ("unknown state", Some(r#"{"state":"paused"}"#)),
        ("not JSON", Some("{")),
    ];
    let pids = ["no driver.pid", "free", "locked"];
    let versions = [Some("v0"), Some("v9"), None];
    let (mut commands, mut shown, mut held) = (Vec::new(), Vec::new(), Vec::new());
    for (outcome, contents) in outcomes {
        for pid in pids {
            for version in versions {
                // A live record of this version, locked with no outcome, is `wait`'s to follow, which
                // `follows` covers; the matrix would block on it.
                if pid == "locked" && contents.is_none() && version == Some("v0") {
                    continue;
                }
                let id = format!("20261107-120000-{:08x}", shown.len());
                let dir = scratch.0.join("runs").join(&id);
                fs::create_dir_all(&dir).unwrap();
                if let Some(version) = version {
                    let version = json!({ "record_version": format!("pumpkin.record/{version}") });
                    fs::write(dir.join("record.json"), version.to_string()).unwrap();
                }
                if pid != "no driver.pid" {
                    fs::write(dir.join("driver.pid"), "4242\n").unwrap();
                }
                if pid == "locked" {
                    held.push(File::open(dir.join("driver.pid")).unwrap());
                    held.last().unwrap().lock().unwrap();
                }
                if let Some(contents) = contents {
                    fs::write(dir.join("outcome.json"), contents).unwrap();
                }
                let version = version.unwrap_or("no record.json");
                let label = format!("<{outcome}, {pid}, {version}>");
                commands.push(scratch.pumpkin(&["wait", &id, "--runs", "runs"]));
                shown.push((id, label));
            }
        }
    }
    let shown: Vec<(&str, &str)> = (shown.iter())
        .map(|(id, label)| (&id[..], &label[..]))
        .collect();
    check(
        &scratch,
        commands,
        false,
        &shown,
        &expect![[r#"
            $ pumpkin wait <no outcome, no driver.pid, v0> --runs runs
            exit status: Some(1)
            pumpkin: runs/<no outcome, no driver.pid, v0> has no driver.pid: it was made before records were locked, which `wait` needs: name a record that this version of `pumpkin run` made
            $ pumpkin wait <no outcome, no driver.pid, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<no outcome, no driver.pid, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <no outcome, no driver.pid, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<no outcome, no driver.pid, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <no outcome, free, v0> --runs runs
            exit status: Some(1)
            pumpkin: run <no outcome, free, v0> is lost: its driver ended without writing outcome.json, so nothing says how it ended: remove what it left, which is its containers, labelled `pumpkin.run=<no outcome, free, v0>`, and its socket directory, `pumpkin-<no outcome, free, v0>` in the temporary directory
            $ pumpkin wait <no outcome, free, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<no outcome, free, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <no outcome, free, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<no outcome, free, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <no outcome, locked, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<no outcome, locked, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <no outcome, locked, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<no outcome, locked, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <scored, no driver.pid, v0> --runs runs
            exit status: Some(1)
            pumpkin: runs/<scored, no driver.pid, v0> has no driver.pid: it was made before records were locked, which `wait` needs: name a record that this version of `pumpkin run` made
            $ pumpkin wait <scored, no driver.pid, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<scored, no driver.pid, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <scored, no driver.pid, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<scored, no driver.pid, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <scored, free, v0> --runs runs
            exit status: Some(0)
            printed outcome.json: {"score":3,"state":"finished"}
            $ pumpkin wait <scored, free, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<scored, free, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <scored, free, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<scored, free, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <scored, locked, v0> --runs runs
            exit status: Some(0)
            printed outcome.json: {"score":3,"state":"finished"}
            $ pumpkin wait <scored, locked, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<scored, locked, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <scored, locked, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<scored, locked, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <unscored, no driver.pid, v0> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unscored, no driver.pid, v0> has no driver.pid: it was made before records were locked, which `wait` needs: name a record that this version of `pumpkin run` made
            $ pumpkin wait <unscored, no driver.pid, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unscored, no driver.pid, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <unscored, no driver.pid, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unscored, no driver.pid, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <unscored, free, v0> --runs runs
            exit status: Some(3)
            pumpkin: run <unscored, free, v0> finished without a score: the agent submitted nothing, and the work phase ended at its deadline
            printed outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"deadline"}}
            $ pumpkin wait <unscored, free, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unscored, free, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <unscored, free, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unscored, free, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <unscored, locked, v0> --runs runs
            exit status: Some(3)
            pumpkin: run <unscored, locked, v0> finished without a score: the agent submitted nothing, and the work phase ended at its deadline
            printed outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"deadline"}}
            $ pumpkin wait <unscored, locked, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unscored, locked, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <unscored, locked, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unscored, locked, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <failed, no driver.pid, v0> --runs runs
            exit status: Some(1)
            pumpkin: runs/<failed, no driver.pid, v0> has no driver.pid: it was made before records were locked, which `wait` needs: name a record that this version of `pumpkin run` made
            $ pumpkin wait <failed, no driver.pid, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<failed, no driver.pid, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <failed, no driver.pid, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<failed, no driver.pid, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <failed, free, v0> --runs runs
            exit status: Some(1)
            pumpkin: run <failed, free, v0> failed: r
            printed outcome.json: {"reason":"r","state":"failed"}
            $ pumpkin wait <failed, free, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<failed, free, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <failed, free, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<failed, free, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <failed, locked, v0> --runs runs
            exit status: Some(1)
            pumpkin: run <failed, locked, v0> failed: r
            printed outcome.json: {"reason":"r","state":"failed"}
            $ pumpkin wait <failed, locked, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<failed, locked, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <failed, locked, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<failed, locked, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <unknown state, no driver.pid, v0> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown state, no driver.pid, v0> has no driver.pid: it was made before records were locked, which `wait` needs: name a record that this version of `pumpkin run` made
            $ pumpkin wait <unknown state, no driver.pid, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown state, no driver.pid, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <unknown state, no driver.pid, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown state, no driver.pid, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <unknown state, free, v0> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown state, free, v0>/outcome.json's state is not one pumpkin knows: expected `finished` or `failed`
            $ pumpkin wait <unknown state, free, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown state, free, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <unknown state, free, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown state, free, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <unknown state, locked, v0> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown state, locked, v0>/outcome.json's state is not one pumpkin knows: expected `finished` or `failed`
            $ pumpkin wait <unknown state, locked, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown state, locked, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <unknown state, locked, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<unknown state, locked, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <not JSON, no driver.pid, v0> --runs runs
            exit status: Some(1)
            pumpkin: runs/<not JSON, no driver.pid, v0> has no driver.pid: it was made before records were locked, which `wait` needs: name a record that this version of `pumpkin run` made
            $ pumpkin wait <not JSON, no driver.pid, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<not JSON, no driver.pid, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <not JSON, no driver.pid, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<not JSON, no driver.pid, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <not JSON, free, v0> --runs runs
            exit status: Some(1)
            pumpkin: runs/<not JSON, free, v0>/outcome.json: EOF while parsing an object at line 1 column 1
            $ pumpkin wait <not JSON, free, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<not JSON, free, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <not JSON, free, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<not JSON, free, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made
            $ pumpkin wait <not JSON, locked, v0> --runs runs
            exit status: Some(1)
            pumpkin: runs/<not JSON, locked, v0>/outcome.json: EOF while parsing an object at line 1 column 1
            $ pumpkin wait <not JSON, locked, v9> --runs runs
            exit status: Some(1)
            pumpkin: runs/<not JSON, locked, v9>/record.json holds record_version "pumpkin.record/v9": expected "pumpkin.record/v0", which this `pumpkin` reads
            $ pumpkin wait <not JSON, locked, no record.json> --runs runs
            exit status: Some(1)
            pumpkin: runs/<not JSON, locked, no record.json> has no record.json, so it is no run's record: expected the directory of one that `pumpkin run` made"#]],
    );
}

/// A workflow that is refused, for breaking parsing rules or for a route no upstream serves,
/// exits with 1, makes no record, and ends its standard error with one `pumpkin:` line, after a
/// line for each rule broken.
#[test]
fn refusals() {
    let scratch = Scratch::new("refusals");
    let mut ir: Value = serde_json::from_str(LWE).unwrap();
    scratch.write("lwe.json", &ir);
    ir["roles"][1]["name"] = json!("agent");
    ir["roles"][1]["egress"] = json!({ "model": { "models": ["gpt-6-astra"] } });
    scratch.write("broken.json", &ir);
    write_configurations(&scratch);
    let run = |ir, config: &str| {
        let mut command = scratch.pumpkin(&["run", ir, "--docker-context", "nowhere"]);
        command.args([
            "--sandboxd",
            "sandboxd",
            "--runs",
            "runs",
            "--config",
            config,
        ]);
        command
    };
    let commands = [
        run("broken.json", "conf/served.toml"),
        run("lwe.json", "unserved.toml"),
    ];
    check(
        &scratch,
        commands,
        false,
        &[],
        &expect![[r#"
        $ pumpkin run broken.json --docker-context nowhere --sandboxd sandboxd --runs runs --config conf/served.toml
        exit status: Some(1)
        pumpkin: broken.json: roles[1].name: `agent` also names roles[0]: give each a unique name
        pumpkin: broken.json: roles[1].egress.model: `agent` is a judge, and only the agent has a model route: remove it
        pumpkin: broken.json: main.solo_then_judge.grants[train].judge.role: no role is named `train_judge`: name one listed in `roles`
        pumpkin: broken.json: the workflow has 3 problems: fix them and run again
        $ pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --runs runs --config unserved.toml
        exit status: Some(1)
        pumpkin: unserved.toml: no upstream serves every model that the workflow's route allows, `gpt-6-astra`: list them all under one"#]],
    );
    assert!(!scratch.0.join("runs").exists(), "a refusal made a record");
}

/// What `run` finds wrong in the deployment's configuration, which it reads for a workflow with
/// a model route, exits with 1: a configuration that can't be read, where `HOME` places it or
/// where `--config` names it, `HOME` unset where it would place it, and the configurations
/// [`write_configurations`] writes to be refused. Past a configuration it accepts, whose key it
/// reads from the file that the configuration names below its own directory, and past a broken
/// one that a workflow without a route doesn't read, it reaches for the engine, which it can't
/// without `docker` on `PATH`.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn configurations() {
    let scratch = Scratch::new("configurations");
    let mut ir: Value = serde_json::from_str(LWE).unwrap();
    scratch.write("lwe.json", &ir);
    ir["roles"][0].as_object_mut().unwrap().remove("egress");
    scratch.write("unrouted.json", &ir);
    write_configurations(&scratch);
    let run = |ir, config: &[&str]| {
        let mut command = scratch.pumpkin(&["run", ir, "--docker-context", "nowhere"]);
        (command.args(["--sandboxd", "sandboxd", "--runs", "runs"])).args(config);
        command
    };
    let config = |name: &str| run("lwe.json", &["--config", &format!("{name}.toml")]);
    let mut home = run("lwe.json", &[]);
    home.env("HOME", scratch.0.join("home"));
    let mut homeless = run("lwe.json", &[]);
    homeless.env_remove("HOME");
    let mut served = config("conf/served");
    let mut unrouted = run("unrouted.json", &["--config", "docker.toml"]);
    for command in [&mut served, &mut unrouted] {
        command.env("PATH", scratch.0.join("nowhere"));
    }
    let refused = [
        "missing", "docker", "broken", "inline", "priced", "keyless", "invalid",
    ];
    let refused = (refused.into_iter()).chain([
        "twice",
        "again",
        "query",
        "fragment",
        "unserved",
        "conf/unkeyed",
    ]);
    let commands = [home, homeless]
        .into_iter()
        .chain(refused.map(config))
        .chain([served, unrouted]);
    check(
        &scratch,
        commands,
        false,
        &[],
        &expect![[r#"
            $ HOME=<scratch>/home pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --runs runs
            exit status: Some(1)
            pumpkin: the configuration <scratch>/home/.config/pumpkin/pumpkin.toml can't be read (No such file or directory (os error 2)): write the upstreams of the workflow's model route there, or name another file with `--config`
            $ env -u HOME pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --runs runs
            exit status: Some(1)
            pumpkin: HOME is unset or empty: set it, or name the configuration with `--config`
            $ pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --runs runs --config missing.toml
            exit status: Some(1)
            pumpkin: the configuration missing.toml can't be read (No such file or directory (os error 2)): write the upstreams of the workflow's model route there, or name another file with `--config`
            $ pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --runs runs --config docker.toml
            exit status: Some(1)
            pumpkin: docker.toml: TOML parse error at line 6, column 2
              |
            6 | [docker]
              |  ^^^^^^
            unknown field `docker`, expected `upstreams`
            $ pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --runs runs --config broken.toml
            exit status: Some(1)
            pumpkin: broken.toml: TOML parse error at line 1, column 19
              |
            1 | [upstreams.gateway
              |                   ^
            unclosed table, expected `]`
            $ pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --runs runs --config inline.toml
            exit status: Some(1)
            pumpkin: inline.toml: TOML parse error at line 3, column 1
              |
            3 | key = "sk-1"
              | ^^^
            unknown field `key`, expected one of `url`, `key_file`, `models`
            $ pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --runs runs --config priced.toml
            exit status: Some(1)
            pumpkin: priced.toml: TOML parse error at line 4, column 73
              |
            4 | models = [{ name = "gpt-6-astra", upstream_name = "vendor/gpt-6-astra", price = 1 }]
              |                                                                         ^^^^^
            unknown field `price`, expected `name` or `upstream_name`
            $ pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --runs runs --config keyless.toml
            exit status: Some(1)
            pumpkin: keyless.toml: TOML parse error at line 1, column 1
              |
            1 | [upstreams.gateway]
              | ^^^^^^^^^^^^^^^^^^^
            missing field `key_file`
            $ pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --runs runs --config invalid.toml
            exit status: Some(1)
            pumpkin: invalid.toml: TOML parse error at line 1, column 12
              |
            1 | [upstreams.Gateway]
              |            ^^^^^^^
            `Gateway` is not a valid name: use 1 to 64 characters from a-z, 0-9, `_` and `-`, starting with a letter
            $ pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --runs runs --config twice.toml
            exit status: Some(1)
            pumpkin: twice.toml: TOML parse error at line 6, column 12
              |
            6 | [upstreams.gateway]
              |            ^^^^^^^
            duplicate key
            $ pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --runs runs --config again.toml
            exit status: Some(1)
            pumpkin: again.toml: `gpt-6-astra` is listed more than once: list each model once, under the upstream that serves it
            $ pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --runs runs --config query.toml
            exit status: Some(1)
            pumpkin: query.toml: TOML parse error at line 2, column 7
              |
            2 | url = "https://models.example.com/v1?project=1"
              |       ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
            the upstream's URL has a query or a fragment: name the API's base URL alone, below which the route reaches `/responses`
            $ pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --runs runs --config fragment.toml
            exit status: Some(1)
            pumpkin: fragment.toml: TOML parse error at line 2, column 7
              |
            2 | url = "https://models.example.com/v1#v2"
              |       ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
            the upstream's URL has a query or a fragment: name the API's base URL alone, below which the route reaches `/responses`
            $ pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --runs runs --config unserved.toml
            exit status: Some(1)
            pumpkin: unserved.toml: no upstream serves every model that the workflow's route allows, `gpt-6-astra`: list them all under one
            $ pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --runs runs --config conf/unkeyed.toml
            exit status: Some(1)
            pumpkin: conf/unkeyed.toml: the key file conf/missing can't be read (No such file or directory (os error 2)): name a file that holds the model provider's key
            $ PATH=<scratch>/nowhere pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --runs runs --config conf/served.toml
            exit status: Some(1)
            pumpkin: the Docker context `nowhere` can't be read: `docker` didn't run: No such file or directory (os error 2): install Docker's command line on `PATH`: name one that `docker context ls` lists
            $ PATH=<scratch>/nowhere pumpkin run unrouted.json --docker-context nowhere --sandboxd sandboxd --runs runs --config docker.toml
            exit status: Some(1)
            pumpkin: the Docker context `nowhere` can't be read: `docker` didn't run: No such file or directory (os error 2): install Docker's command line on `PATH`: name one that `docker context ls` lists"#]],
    );
}

/// Writes into `scratch` the deployment's configurations that [`configurations`] gives `run`,
/// each named for what it holds: `conf/served`, of two upstreams, beside the key file `conf/key`;
/// and, to be refused, `docker`, with a key that a configuration doesn't have; `broken`, which
/// isn't TOML; `inline`, with an upstream's key in place of its file; `priced`, with a key that a
/// model doesn't have; `keyless`, without an upstream's file; `invalid`, with an invalid name;
/// `twice`, with two upstreams of one name; `again`, with one model listed twice; `query` and
/// `fragment`, with a URL that has one; `unserved`, without the workflow's model; and
/// `conf/unkeyed`, whose key file is missing.
fn write_configurations(scratch: &Scratch) {
    let table = |name: &str, model: &str| {
        format!(
            r#"[upstreams.{name}]
url = "https://models.example.com/v1"
key_file = "key"
models = [{{ name = "{model}", upstream_name = "vendor/{model}" }}]
"#
        )
    };
    let gateway = table("gateway", "gpt-6-astra");
    let direct = table("direct", "flash");
    scratch.write("conf/key", "sk-test\n");
    let configs = [
        ("conf/served", format!("{gateway}\n{direct}")),
        ("docker", format!("{gateway}\n[docker]\n")),
        (
            "broken",
            gateway.replace("[upstreams.gateway]", "[upstreams.gateway"),
        ),
        (
            "inline",
            gateway.replace("key_file = \"key\"", "key = \"sk-1\""),
        ),
        ("priced", gateway.replace(" }", ", price = 1 }")),
        ("keyless", gateway.replace("key_file = \"key\"\n", "")),
        ("invalid", table("Gateway", "gpt-6-astra")),
        ("twice", format!("{gateway}\n{}", table("gateway", "flash"))),
        (
            "again",
            format!("{gateway}\n{}", table("direct", "gpt-6-astra")),
        ),
        ("query", gateway.replace("/v1", "/v1?project=1")),
        ("fragment", gateway.replace("/v1", "/v1#v2")),
        ("unserved", direct),
        ("conf/unkeyed", gateway.replace("\"key\"", "\"missing\"")),
    ];
    for (name, config) in configs {
        scratch.write(&format!("{name}.toml"), &config);
    }
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
fn image(scratch: &Scratch, engine: &Engine) -> String {
    let dir = scratch.0.join("image");
    fs::create_dir(&dir).unwrap();
    let recipe = format!("FROM {BUSYBOX}\nRUN mkdir --mode=1777 /workspace\n");
    fs::write(dir.join("Dockerfile"), recipe).unwrap();
    let built = engine.build(&dir, "Dockerfile", "pumpkin-tests", |_| Ok(()));
    built.unwrap().image.as_str().to_owned()
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

/// Writes into `scratch` the workflow `asks.json`, [`workflow`]'s with the fixture's model route,
/// whose agent asks its model and keeps the answer in its `out`; and the configuration
/// `conf/pumpkin.toml`, whose one upstream, the stand-in at `address`, serves the route's model
/// as `vendor/gpt-6-astra`, with its key in `conf/key`.
fn write_asks(scratch: &Scratch, image: &str, address: &str) {
    let config = format!(
        r#"[upstreams.provider]
url = "http://{address}/v1/"
key_file = "key"
models = [{{ name = "gpt-6-astra", upstream_name = "vendor/gpt-6-astra" }}]
"#
    );
    scratch.write("conf/pumpkin.toml", &config);
    fs::write(scratch.0.join("conf/key"), "sk-test\n").unwrap();
    let body = r#"{"model": "gpt-6-astra", "store": false, "input": "hi"}"#;
    let ask = format!(
        "wget -q -O /pumpkin/out/answer --header 'content-type: application/json' \
         --post-data '{body}' http://127.0.0.1:{MODEL_PORT}/responses"
    );
    let mut asks = workflow(image, &["sh", "-c", &ask]);
    let lwe: Value = serde_json::from_str(LWE).unwrap();
    asks["roles"][0]["egress"] = lwe["roles"][0]["egress"].clone();
    scratch.write("asks.json", &asks);
}

/// A stand-in for a model provider, on the loopback, which answers each `POST` with a whole
/// stream that echoes the request's line, its `authorization` and its body.
struct StandIn {
    address: String,
    thread: JoinHandle<()>,
}

impl StandIn {
    fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let thread = thread::spawn(move || {
            for connection in listener.incoming() {
                let mut connection = connection.unwrap();
                let mut reader = BufReader::new(&connection);
                let mut head = Vec::new();
                while head.last().is_none_or(|line: &String| !line.is_empty()) {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    head.push(line.trim_end().to_owned());
                }
                // The connection that `stop` makes holds no request.
                if head[0].is_empty() {
                    return;
                }
                let header = |name| head.iter().find_map(|line| line.strip_prefix(name));
                let length = header("content-length: ").unwrap();
                let mut body = vec![0; length.parse().unwrap()];
                reader.read_exact(&mut body).unwrap();
                let body = String::from_utf8(body).unwrap();
                let echo = format!(
                    "data: {} {:?} {body}\n\n",
                    head[0],
                    header("authorization: ")
                );
                let stream = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                     transfer-encoding: chunked\r\nconnection: close\r\n\r\n{:x}\r\n{echo}\r\n0\r\n\r\n",
                    echo.len()
                );
                connection.write_all(stream.as_bytes()).unwrap();
            }
        });
        Self { address, thread }
    }

    fn stop(self) {
        drop(TcpStream::connect(&self.address).unwrap());
        self.thread.join().unwrap();
    }
}

/// Runs on Docker: a workflow whose agent asks its model, through its route, as the
/// configuration's upstream serves it, a stand-in, which takes the key that the configuration
/// names, gets the request renamed for it, with that key, and which the run
/// log names, with how the request was answered; one whose agent exits without a submission,
/// recorded below `HOME` whatever `XDG_DATA_HOME` says; and one whose agent submits a payload,
/// which the final judge scores; then `wait` finds the outcome of each run that recorded into
/// `runs`.
#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn runs_on_docker() {
    let scratch = Scratch::new("docker");
    let sandboxd = sandboxd();
    let context = env::var("PUMPKIN_DOCKER_CONTEXT").unwrap_or_else(|_| "default".to_owned());
    let engine = Engine::connect(ContextName::try_from(context.clone()).unwrap()).unwrap();
    let image = image(&scratch, &engine);
    let stand_in = StandIn::start();
    write_asks(&scratch, &image, &stand_in.address);
    scratch.write("exits.json", &workflow(&image, &["true"]));
    let submit = format!("printf six > six && {SANDBOXCTL_PATH} submit submit six --finalize");
    scratch.write("submits.json", &workflow(&image, &["sh", "-c", &submit]));
    let run = |ir, runs: &[&str]| {
        let mut command = scratch.pumpkin(&["run", ir, "--docker-context", &context]);
        command.args(["--sandboxd", &sandboxd]).args(runs);
        command
    };
    // `docker` finds its contexts below `HOME` unless `DOCKER_CONFIG` says where.
    let home = PathBuf::from(env::var_os("HOME").unwrap());
    let docker_config = env::var_os("DOCKER_CONFIG").map_or(home.join(".docker"), PathBuf::from);
    let docker_config = docker_config.into_os_string().into_string().unwrap();
    let mut exits = run("exits.json", &[]);
    (exits.env("HOME", scratch.0.join("home")))
        .env("XDG_DATA_HOME", scratch.0.join("xdg"))
        .env("DOCKER_CONFIG", &docker_config);
    let commands = [
        run(
            "asks.json",
            &["--runs", "runs", "--config", "conf/pumpkin.toml"],
        ),
        exits,
        run("submits.json", &["--runs", "runs"]),
    ];
    let context = format!("--docker-context {context} ");
    let shown = [
        (&context[..], "--docker-context <context> "),
        (&sandboxd[..], "<sandboxd>"),
        (&docker_config[..], "<docker config>"),
        (&stand_in.address[..], "<stand-in>"),
    ];
    let rootful = !engine.report().security.rootless;
    check(
        &scratch,
        commands,
        rootful,
        &shown,
        &expect![[r#"
        $ pumpkin run asks.json --docker-context <context> --sandboxd <sandboxd> --runs runs --config conf/pumpkin.toml
        exit status: Some(3)
        pumpkin: run <id> records into runs/<id>
        pumpkin: run <id> finished without a score: the agent submitted nothing, and the work phase ended when the agent exited
        record: agent/log/command.stderr, agent/log/command.stdout, agent/out/answer, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
        the run log notes: the agent's model route goes to the upstream `provider`, which takes `gpt-6-astra` as `vendor/gpt-6-astra`
        the run log records: {"answer":{"kind":"forwarded"},"kind":"model_answered","request":0,"upstream_status":200}
        the agent's answer: data: POST /v1/responses HTTP/1.1 Some("Bearer sk-test") {"input":"hi","model":"vendor/gpt-6-astra","store":false}\n\n
        outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}
        printed outcome.json: true
        $ DOCKER_CONFIG=<docker config> HOME=<scratch>/home XDG_DATA_HOME=<scratch>/xdg pumpkin run exits.json --docker-context <context> --sandboxd <sandboxd>
        exit status: Some(3)
        pumpkin: run <id> records into <scratch>/home/.local/share/pumpkin/runs/<id>
        pumpkin: run <id> finished without a score: the agent submitted nothing, and the work phase ended when the agent exited
        record: agent/log/command.stderr, agent/log/command.stdout, agent/out/, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
        outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}
        printed outcome.json: true
        $ pumpkin run submits.json --docker-context <context> --sandboxd <sandboxd> --runs runs
        exit status: Some(0)
        pumpkin: run <id> records into runs/<id>
        record: agent/log/command.stderr, agent/log/command.stdout, agent/out/, driver.pid, ir.json, jobs/0/log/command.stderr, jobs/0/log/command.stdout, jobs/0/out/evidence/, jobs/0/out/result.json, log.jsonl, outcome.json, payloads/44778d82365e4af681c40d5f0eef5cf6f5899d3f0ac335050a7ed6779cf3f674, provider.json, record.json
        outcome.json: {"score":3,"state":"finished","verdict":{"kind":"judged","result":{"returned":{"raw_score":3},"score":3},"submission":{"finalized":true,"payload":{"digest":"sha256:44778d82365e4af681c40d5f0eef5cf6f5899d3f0ac335050a7ed6779cf3f674","size":3}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}
        printed outcome.json: true"#]],
    );
    stand_in.stop();
    wait_for_each_run(&scratch);
}
