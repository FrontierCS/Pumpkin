//! The driver: until its run has started, what ends `run` ends the driver too; after, the run
//! goes on without `run`. The tests but one reach a stand-in `docker` ([`StandIn`]), which can
//! hold the driver before it makes the run's record, before the start, or after. The one on
//! Docker is ignored, as the Docker provider's tests are.

use std::fs::{self, File, TryLockError};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Output, Stdio};

use docker::{ContextName, Engine};
use expect_test::expect;
use provider::Provider as _;
use schema::provider::RunTag;

use super::{
    Scratch, StandIn, Step, check, context, docker_table, image, invocation, poll, ran, record,
    report, sandboxd, signal, workflow,
};

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

/// `pumpkin wait` for the run that `output`, `pumpkin run`'s in `scratch`, named, once its
/// driver, `driver`, has ended, and let go of the record ([`unlocked`]), as [`report`] shows it,
/// with the run's id as `<id>`.
fn wait_once_ended(scratch: &Scratch, output: &Output, driver: i64) -> String {
    // Ended, as its log is whole only then, which a driver writes after its lock goes.
    poll(|| Some(()).filter(|()| !alive(driver)));
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    let (id, dir) = record(&stderr).unwrap();
    unlocked(&scratch.0.join(dir));
    let wait = scratch.pumpkin(&["wait", id, "--runs", "runs"]);
    ran(wait, &scratch.0, false).replace(id, "<id>")
}

/// Whom [`unstarted`] signals.
#[derive(Clone, Copy)]
enum Whom {
    Run,
    Driver,
    /// Both `run` and its driver, by `run`'s process group, as a terminal signals.
    Both,
}

/// Holds the driver of a `pumpkin run` in a scratch directory of its own, `name`, at `at`, then
/// sends `signal_name` to `whom`, and shows what becomes of `run`, that the driver ends, and,
/// if it made the run's record, what `wait` finds of it.
fn unstarted(name: &str, at: &str, whom: Whom, signal_name: &str) -> String {
    let scratch = Scratch::new(name);
    let stand_in = StandIn::new(&scratch);
    stand_in.hold(at);
    let mut run = stand_in.run("stand-in.json", &[]);
    run.stdout(Stdio::piped()).stderr(Stdio::piped());
    let invoked = invocation(&run);
    let mut child = run.spawn().unwrap();
    let pid = i64::from(child.id());
    let driver = stand_in.held(at);
    let target = match whom {
        Whom::Run => pid,
        Whom::Driver => driver,
        Whom::Both => -pid,
    };
    assert!(signal(signal_name, target), "the process to signal is gone");
    // Before `run`'s output is read to its end, which a driver that lives on would hold off.
    poll(|| Some(()).filter(|()| !alive(driver)));
    let stderr = child.stderr.take().unwrap();
    let output = ended(child, stderr, String::new());
    let mut lines = vec![report(invoked, &output, &scratch.0, false)];
    if record(&String::from_utf8_lossy(&output.stderr)).is_some() {
        lines.push(wait_once_ended(&scratch, &output, driver));
    } else {
        let made = scratch.0.join("runs").exists();
        lines.push(format!("the driver is gone, and made no record: {}", !made));
    }
    let text = lines.join("\n");
    text.replace(&scratch.0.display().to_string(), "<scratch>")
}

/// What becomes of a `pumpkin run` in a scratch directory of its own, `name`, whose record
/// loses its `driver.log` while the stand-in holds the driver at `image`, before the start.
fn unlogged(name: &str) -> String {
    let scratch = Scratch::new(name);
    let stand_in = StandIn::new(&scratch);
    stand_in.hold("image");
    let mut run = stand_in.run("stand-in.json", &[]);
    run.stdout(Stdio::piped()).stderr(Stdio::piped());
    let invoked = invocation(&run);
    let child = run.spawn().unwrap();
    stand_in.held("image");
    let mut records = fs::read_dir(scratch.0.join("runs")).unwrap();
    let record = records.next().unwrap().unwrap().path();
    fs::remove_file(record.join("driver.log")).unwrap();
    stand_in.release("image");
    let output = child.wait_with_output().unwrap();
    let text = report(invoked, &output, &scratch.0, false);
    text.replace(&scratch.0.display().to_string(), "<scratch>")
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn does_not_start() {
    let scratch = Scratch::new("does-not-start");
    let stand_in = StandIn::new(&scratch);
    let mut steps = vec![Step::from(stand_in.run("absent.json", &[]))];
    steps.push(Step::Ran(unlogged("unlogged")));
    // The driver is held before it makes the record, at `context`, or after, at `image`.
    for (at, whom, signal) in [
        ("context", Whom::Driver, "KILL"),
        ("context", Whom::Run, "KILL"),
        ("context", Whom::Run, "TERM"),
        ("context", Whom::Both, "INT"),
        ("image", Whom::Driver, "KILL"),
        ("image", Whom::Run, "TERM"),
        ("image", Whom::Both, "INT"),
    ] {
        let name = format!("unstarted-{at}-{}", steps.len());
        steps.push(Step::Ran(unstarted(&name, at, whom, signal)));
    }
    check(
        &scratch,
        steps,
        false,
        &[],
        &expect![[r#"
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run absent.json --sandboxd sandboxd --runs runs --config pumpkin.toml
            exit status: Some(1)
            pumpkin: run <id> records into <scratch>/runs/<id>
            pumpkin: run <id> failed: publishing what the run's sandboxes run failed: the engine of context `default` has no image absent@sha256:1111111111111111111111111111111111111111111111111111111111111111: build or load it there first, as the provider pulls none
            record: driver.log, driver.pid, ir.json, outcome.json, provider.json, record.json
            outcome.json: {"reason":"publishing what the run's sandboxes run failed: the engine of context `default` has no image absent@sha256:1111111111111111111111111111111111111111111111111111111111111111: build or load it there first, as the provider pulls none","state":"failed"}
            printed outcome.json: true
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in.json --sandboxd sandboxd --runs runs --config pumpkin.toml
            exit status: Some(1)
            pumpkin: run <id> records into <scratch>/runs/<id>
            pumpkin: run <id> failed: <scratch>/runs/<id>/driver.log: No such file or directory (os error 2)
            record: driver.pid, ir.json, outcome.json, provider.json, record.json
            outcome.json: {"reason":"<scratch>/runs/<id>/driver.log: No such file or directory (os error 2)","state":"failed"}
            printed outcome.json: true
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in.json --sandboxd sandboxd --runs runs --config pumpkin.toml
            exit status: Some(1)
            pumpkin: the driver ended before the run started, by signal: 9 (SIGKILL)
            the driver is gone, and made no record: true
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in.json --sandboxd sandboxd --runs runs --config pumpkin.toml
            ended by signal: 9
            the driver is gone, and made no record: true
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in.json --sandboxd sandboxd --runs runs --config pumpkin.toml
            ended by signal: 15
            the driver is gone, and made no record: true
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in.json --sandboxd sandboxd --runs runs --config pumpkin.toml
            ended by signal: 2
            the driver is gone, and made no record: true
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in.json --sandboxd sandboxd --runs runs --config pumpkin.toml
            exit status: Some(1)
            pumpkin: run <id> records into <scratch>/runs/<id>
            pumpkin: the driver ended before the run started, by signal: 9 (SIGKILL)
            pumpkin: run <id> is lost: its driver ended without writing outcome.json, so nothing says how it ended: remove what it left, which is its containers, labelled `pumpkin.run=<id>`, and its socket directory, `pumpkin-<id>` in the temporary directory
            outcome.json: none
            printed the id alone: false
            $ pumpkin wait <id> --runs runs
            exit status: Some(1)
            pumpkin: run <id> is lost: its driver ended without writing outcome.json, so nothing says how it ended: remove what it left, which is its containers, labelled `pumpkin.run=<id>`, and its socket directory, `pumpkin-<id>` in the temporary directory
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in.json --sandboxd sandboxd --runs runs --config pumpkin.toml
            ended by signal: 15
            pumpkin: run <id> records into <scratch>/runs/<id>
            outcome.json: none
            printed the id alone: false
            $ pumpkin wait <id> --runs runs
            exit status: Some(1)
            pumpkin: run <id> is lost: its driver ended without writing outcome.json, so nothing says how it ended: remove what it left, which is its containers, labelled `pumpkin.run=<id>`, and its socket directory, `pumpkin-<id>` in the temporary directory
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in.json --sandboxd sandboxd --runs runs --config pumpkin.toml
            ended by signal: 2
            pumpkin: run <id> records into <scratch>/runs/<id>
            outcome.json: none
            printed the id alone: false
            $ pumpkin wait <id> --runs runs
            exit status: Some(1)
            pumpkin: run <id> is lost: its driver ended without writing outcome.json, so nothing says how it ended: remove what it left, which is its containers, labelled `pumpkin.run=<id>`, and its socket directory, `pumpkin-<id>` in the temporary directory"#]],
    );
}

/// What [`detaches`] shows of a run that `run`, invoked as `invoked`, started while the stand-in
/// held its driver, `driver`, at `create`, once `run` has ended with `output`.
fn detached(stand_in: &StandIn, invoked: String, output: &Output, driver: i64) -> String {
    let scratch = stand_in.scratch;
    let mut lines = vec![report(invoked, output, &scratch.0, false)];
    lines.push(format!(
        "the driver runs, leading a session of its own: {}",
        alive(driver) && session(driver) == Some(driver)
    ));
    let name = fs::read_to_string(format!("/proc/{driver}/comm")).unwrap();
    lines.push(format!("the driver's name: {}", name.trim_end()));
    stand_in.release("create");
    lines.push(wait_once_ended(scratch, output, driver));
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    let (_, dir) = record(&stderr).unwrap();
    let log = fs::read_to_string(scratch.0.join(dir).join("driver.log")).unwrap();
    lines.push(format!("driver.log: {log:?}"));
    let text = lines.join("\n");
    text.replace(&scratch.0.display().to_string(), "<scratch>")
}

/// [`detached`], for a `run` that prints the id and exits with 0.
fn returns() -> String {
    let scratch = Scratch::new("detaches-returns");
    let stand_in = StandIn::new(&scratch);
    stand_in.hold("create");
    let mut run = stand_in.run("stand-in.json", &[]);
    let invoked = invocation(&run);
    let output = run.output().unwrap();
    detached(&stand_in, invoked, &output, stand_in.held("create"))
}

/// [`detached`], for a `run --wait` that follows the run until a SIGINT to its process group,
/// as from its terminal, ends it.
fn interrupted() -> String {
    let scratch = Scratch::new("detaches-interrupted");
    let stand_in = StandIn::new(&scratch);
    stand_in.hold("create");
    let mut run = stand_in.run("stand-in.json", &["--wait"]);
    run.stdout(Stdio::piped()).stderr(Stdio::piped());
    let invoked = invocation(&run);
    let mut child = run.spawn().unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut said = String::new();
    while !said.ends_with("does not stop it\n") {
        assert_ne!(stderr.read_line(&mut said).unwrap(), 0, "run ended: {said}");
    }
    let driver = stand_in.held("create");
    assert!(signal("INT", -i64::from(child.id())), "run is gone");
    let output = ended(child, stderr, said);
    detached(&stand_in, invoked, &output, driver)
}

/// [`detached`], for a `run` stopped before it heard of the start, then sent SIGTERM once the
/// driver is past it, and let go on.
fn stopped() -> String {
    let scratch = Scratch::new("detaches-stopped");
    let stand_in = StandIn::new(&scratch);
    stand_in.hold("image");
    stand_in.hold("create");
    let mut run = stand_in.run("stand-in.json", &[]);
    run.stdout(Stdio::piped()).stderr(Stdio::piped());
    let invoked = invocation(&run);
    let mut child = run.spawn().unwrap();
    let pid = i64::from(child.id());
    stand_in.held("image");
    assert!(signal("STOP", pid), "run is gone");
    stand_in.release("image");
    let driver = stand_in.held("create");
    assert!(signal("TERM", pid) && signal("CONT", pid), "run is gone");
    let stderr = child.stderr.take().unwrap();
    let output = ended(child, stderr, String::new());
    detached(&stand_in, invoked, &output, driver)
}

#[test]
fn detaches() {
    let scratch = Scratch::new("detaches");
    let steps = [returns(), interrupted(), stopped()].map(Step::Ran);
    check(
        &scratch,
        steps,
        false,
        &[],
        &expect![[r#"
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in.json --sandboxd sandboxd --runs runs --config pumpkin.toml
            exit status: Some(0)
            pumpkin: run <id> records into <scratch>/runs/<id>
            pumpkin: run <id> started: `pumpkin wait <id>` follows it
            outcome.json: none
            printed the id alone: true
            the driver runs, leading a session of its own: true
            the driver's name: pumpkin-driver
            $ pumpkin wait <id> --runs runs
            exit status: Some(3)
            pumpkin: run <id> finished without a score: the agent submitted nothing, and the work phase ended when the agent failed: the agent's sandbox didn't start: `docker create` failed with exit status: 1: the stand-in creates nothing: fix what Docker reports
            printed outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: `docker create` failed with exit status: 1: the stand-in creates nothing: fix what Docker reports"}}
            driver.log: ""
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in.json --sandboxd sandboxd --runs runs --config pumpkin.toml --wait
            ended by signal: 2
            pumpkin: run <id> records into <scratch>/runs/<id>
            pumpkin: run <id> started
            pumpkin: following run <id>: ending `pumpkin run` does not stop it
            outcome.json: none
            printed the id alone: false
            the driver runs, leading a session of its own: true
            the driver's name: pumpkin-driver
            $ pumpkin wait <id> --runs runs
            exit status: Some(3)
            pumpkin: run <id> finished without a score: the agent submitted nothing, and the work phase ended when the agent failed: the agent's sandbox didn't start: `docker create` failed with exit status: 1: the stand-in creates nothing: fix what Docker reports
            printed outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: `docker create` failed with exit status: 1: the stand-in creates nothing: fix what Docker reports"}}
            driver.log: ""
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in.json --sandboxd sandboxd --runs runs --config pumpkin.toml
            ended by signal: 15
            pumpkin: run <id> records into <scratch>/runs/<id>
            outcome.json: none
            printed the id alone: false
            the driver runs, leading a session of its own: true
            the driver's name: pumpkin-driver
            $ pumpkin wait <id> --runs runs
            exit status: Some(3)
            pumpkin: run <id> finished without a score: the agent submitted nothing, and the work phase ended when the agent failed: the agent's sandbox didn't start: `docker create` failed with exit status: 1: the stand-in creates nothing: fix what Docker reports
            printed outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: `docker create` failed with exit status: 1: the stand-in creates nothing: fix what Docker reports"}}
            driver.log: """#]],
    );
}

/// What a test on Docker leaves behind, to remove once it is over, however it ends: for each
/// run recorded in `runs` its driver, its containers and its socket directory.
struct Cleanup<'a> {
    engine: &'a Engine,
    runs: PathBuf,
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        let Ok(entries) = fs::read_dir(&self.runs) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let Ok(id) = entry.file_name().into_string() else {
                continue;
            };
            if let Some(driver) = live_driver(&entry.path()) {
                let _ = signal("KILL", driver);
            }
            let _ = fs::remove_dir_all(std::env::temp_dir().join(format!("pumpkin-{id}")));
            let Ok(tag) = RunTag::try_from(id) else {
                continue;
            };
            if let Ok(listed) = self.engine.list(&tag) {
                let ids: Vec<_> = listed.into_iter().map(|listed| listed.id).collect();
                let _ = self.engine.delete(&tag, &ids);
            }
        }
    }
}

/// Waits until no process holds the lock of the record `dir`, by which `wait` finds that its run
/// is no longer live.
fn unlocked(dir: &Path) {
    // Not until the driver's process shows as ended: its main thread does, as a zombie, while
    // another of its threads may still be exiting, holding the descriptors, and so the lock.
    poll(|| Some(()).filter(|()| live_driver(dir).is_none()));
}

/// The process ID of the driver of the record `dir`, if it holds the record's lock.
fn live_driver(dir: &Path) -> Option<i64> {
    let pid = File::open(dir.join("driver.pid")).ok()?;
    let held = matches!(pid.try_lock_shared(), Err(TryLockError::WouldBlock));
    let text = fs::read_to_string(dir.join("driver.pid")).ok()?;
    text.trim().parse().ok().filter(|_| held)
}

/// The ID of the session that process `pid` is in, if there is one: the sixth field of its
/// `stat`, after the name in parentheses, which may hold anything.
fn session(pid: i64) -> Option<i64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, after) = stat.rsplit_once(')').unwrap();
    Some(after.split_whitespace().nth(3).unwrap().parse().unwrap())
}

/// Whether the process `pid` runs: a zombie has ended.
fn alive(pid: i64) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| !stat.contains(") Z "))
}

#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn lost_on_docker() {
    let scratch = Scratch::new("lost");
    let sandboxd = sandboxd();
    let context = context();
    let engine = Engine::connect(ContextName::try_from(context.clone()).unwrap()).unwrap();
    let image = image(&engine);
    scratch.write("sleeps.json", &workflow(&image, &["sleep", "300"]));
    scratch.write("pumpkin.toml", &docker_table(&context));
    let _cleanup = Cleanup {
        engine: &engine,
        runs: scratch.0.join("runs"),
    };
    let mut run = scratch.pumpkin(&["run", "sleeps.json", "--sandboxd", &sandboxd]);
    run.args(["--runs", "runs", "--config", "pumpkin.toml"]);
    let invoked = invocation(&run);
    let output = run.output().unwrap();
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    let (id, dir) = record(&stderr).unwrap();
    let tag = RunTag::try_from(id.to_owned()).unwrap();
    // Once a sandbox is listed, the run's `docker` children have run, and none may hold the
    // record's lock, which `unlocked` waits for no process to hold.
    poll(|| Some(()).filter(|()| !engine.list(&tag).unwrap().is_empty()));
    let driver = live_driver(&scratch.0.join(dir)).unwrap();
    assert!(signal("KILL", driver), "the driver is gone");
    unlocked(&scratch.0.join(dir));
    let rootful = !engine.report().security.rootless;
    let killed = report(invoked, &output, &scratch.0, rootful);
    let wait = scratch.pumpkin(&["wait", id, "--runs", "runs"]);
    let shown = [(&sandboxd[..], "<sandboxd>"), (id, "<id>")];
    check(
        &scratch,
        [Step::Ran(killed), Step::from(wait)],
        false,
        &shown,
        &expect![[r#"
            $ pumpkin run sleeps.json --sandboxd <sandboxd> --runs runs --config pumpkin.toml
            exit status: Some(0)
            pumpkin: run <id> records into <scratch>/runs/<id>
            pumpkin: run <id> started: `pumpkin wait <id>` follows it
            outcome.json: none
            printed the id alone: true
            $ pumpkin wait <id> --runs runs
            exit status: Some(1)
            pumpkin: run <id> is lost: its driver ended without writing outcome.json, so nothing says how it ended: remove what it left, which is its containers, labelled `pumpkin.run=<id>`, and its socket directory, `pumpkin-<id>` in the temporary directory"#]],
    );
}
