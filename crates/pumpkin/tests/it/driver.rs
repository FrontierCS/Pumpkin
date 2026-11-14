//! The driver: until its run has started, what ends `run` ends the driver too; after, the run
//! goes on without `run`. The tests but one reach a stand-in `docker` ([`StandIn`]), which can
//! hold the driver before it makes the run's record, before the start, or after. The one on
//! Docker is ignored, as the Docker provider's tests are.

use std::fs::{self, File, TryLockError};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use docker::Reached;
use expect_test::expect;
use provider::{RunTag, Sweep as _};
use serde_json::Value;
use test_support::{docker_context, sandboxd};

use super::{
    IMAGE, INFO, Scratch, StandIn, Step, alive, build_first, check, containers, ended, engine,
    files, invocation, machine, poll, ran, record, report, sandboxes, signal, through_shell,
    with_room, workflow,
};

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
/// sends `signal_name` to `whom`, then lets the driver go on if `released`, and shows what
/// becomes of `run`, that the driver ends, and, if it made the run's record, what `wait` finds of
/// it.
fn unstarted(name: &str, at: &str, whom: Whom, signal_name: &str, released: bool) -> String {
    let scratch = Scratch::new(name);
    let stand_in = StandIn::new(&scratch);
    stand_in.hold(at);
    let mut run = stand_in.run("stand-in/task.star", &[]);
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
    if released {
        stand_in.release(at);
    }
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
    let mut run = stand_in.run("stand-in/task.star", &[]);
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

/// What becomes of a `pumpkin run` in a scratch directory of its own, `name`, of a task whose
/// image the stand-in engine lacks and fails to build: the failure at the image's place, and no
/// record and no id.
fn unbuilt(name: &str) -> String {
    let scratch = Scratch::new(name);
    let stand_in = StandIn::new(&scratch);
    let text = ran(stand_in.run("unbuilt/task.star", &[]), &scratch.0, false);
    let made = scratch.0.join("runs").exists();
    let text = format!("{text}\na record was made: {made}");
    text.replace(&scratch.0.display().to_string(), "<scratch>")
}

/// What becomes of a `pumpkin run` of the stand-in's `task` in a scratch directory of its own,
/// `name`, against a stand-in engine of which `docker info` prints `info`, and `docker image
/// inspect` prints `image`.
fn on_engine(name: &str, task: &str, info: &str, image: &str) -> String {
    let scratch = Scratch::new(name);
    let stand_in = StandIn::with(&scratch, info, image);
    let text = ran(
        stand_in.run(&format!("{task}/task.star"), &[]),
        &scratch.0,
        false,
    );
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
    let mut steps = vec![Step::from(stand_in.run("lacking/task.star", &[]))];
    steps.push(Step::Ran(unlogged("unlogged")));
    steps.push(Step::Ran(unbuilt("unbuilt")));
    // As an engine of the classic store reports its graph driver's status, and as one before
    // Docker 28.0, or one that drops the experimental field, inspects an image.
    let containerd = r#"["driver-type","io.containerd.snapshotter.v1"]"#;
    let classic = INFO.replace(containerd, r#"["Backing Filesystem","extfs"]"#);
    steps.push(Step::Ran(on_engine("classic", "stand-in", &classic, IMAGE)));
    let descriptor = r#","Descriptor":{"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    steps.push(Step::Ran(on_engine(
        "undescribed",
        "stand-in",
        INFO,
        &IMAGE.replace(descriptor, ""),
    )));
    // An engine that can't limit a container's swap, of a task whose image it would build.
    let unlimited = INFO.replace(r#""SwapLimit":true"#, r#""SwapLimit":false"#);
    steps.push(Step::Ran(on_engine(
        "unlimited",
        "unbuilt",
        &unlimited,
        IMAGE,
    )));
    for (at, whom, signal, released) in [
        ("context", Whom::Driver, "KILL", false),
        ("context", Whom::Driver, "TERM", false),
        ("context", Whom::Run, "KILL", false),
        ("context", Whom::Run, "TERM", false),
        ("context", Whom::Both, "INT", false),
        // While it builds the task's images, before any record is made.
        ("lookup", Whom::Driver, "KILL", false),
        ("lookup", Whom::Run, "TERM", false),
        ("lookup", Whom::Both, "INT", false),
        ("image", Whom::Driver, "KILL", false),
        ("image", Whom::Driver, "TERM", true),
        ("image", Whom::Run, "TERM", false),
        ("image", Whom::Both, "INT", false),
    ] {
        let name = format!("unstarted-{at}-{}", steps.len());
        steps.push(Step::Ran(unstarted(&name, at, whom, signal, released)));
    }
    check(
        &scratch,
        steps,
        false,
        &[],
        &expect![[r#"
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run lacking/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
            exit status: Some(1)
            pumpkin: run <id> records into <scratch>/runs/<id>
            pumpkin: run <id> failed: publishing role `agent`'s image on provider `local` failed: the engine of context `default` has no image sha256:1111111111111111111111111111111111111111111111111111111111111111: build it there, or `docker load` an archive that holds it, as Pumpkin pulls none onto it; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/lacking/task.star
            record: driver.log, driver.pid, ir.json, outcome.json, provider.json, record.json
            outcome.json: {"reason":"publishing role `agent`'s image on provider `local` failed: the engine of context `default` has no image sha256:1111111111111111111111111111111111111111111111111111111111111111: build it there, or `docker load` an archive that holds it, as Pumpkin pulls none onto it","state":"failed"}
            printed outcome.json: true
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
            exit status: Some(1)
            pumpkin: run <id> records into <scratch>/runs/<id>
            pumpkin: run <id> failed: <scratch>/runs/<id>/driver.log: No such file or directory (os error 2); its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star
            record: driver.pid, ir.json, outcome.json, provider.json, record.json
            outcome.json: {"reason":"<scratch>/runs/<id>/driver.log: No such file or directory (os error 2)","state":"failed"}
            printed outcome.json: true
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run unbuilt/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
            exit status: Some(1)
            pumpkin: building an image in pumpkin/unbuilt
            the stand-in creates nothing
            pumpkin: the build of an image in pumpkin/unbuilt failed
            unbuilt/task.star:1:9: building the image failed: `docker build` of `Dockerfile` failed with exit status: 1: its output above says why: fix what Docker reports
                1 | IMAGE = image("Dockerfile")
                  |         ^^^^^^^^^^^^^^^^^^^
            pumpkin: unbuilt/task.star: the task has 1 problem
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/unbuilt/task.star
            a record was made: false
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
            exit status: Some(1)
            pumpkin: the Docker engine of context `default` doesn't keep its images in the containerd image store, as `docker info` reports no `driver-type` of `io.containerd.snapshotter.v1`, so it can't find an image by its manifest's digest: turn the store on, as `"features": {"containerd-snapshotter": true}` in the engine's `daemon.json` does
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
            exit status: Some(1)
            pumpkin: run <id> records into <scratch>/runs/<id>
            pumpkin: run <id> failed: publishing role `agent`'s image on provider `local` failed: the engine of context `default` doesn't say what sha256:0000000000000000000000000000000000000000000000000000000000000000 is the digest of, as `docker image inspect` gives it no `Descriptor`: use Docker 28.0 or later, whose engine and command line give one; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star
            record: driver.log, driver.pid, ir.json, outcome.json, provider.json, record.json
            outcome.json: {"reason":"publishing role `agent`'s image on provider `local` failed: the engine of context `default` doesn't say what sha256:0000000000000000000000000000000000000000000000000000000000000000 is the digest of, as `docker image inspect` gives it no `Descriptor`: use Docker 28.0 or later, whose engine and command line give one","state":"failed"}
            printed outcome.json: true
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run unbuilt/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
            exit status: Some(1)
            pumpkin: reaching provider `local` failed: the Docker engine of context `default` can't enforce every limit a sandbox has: `docker info` reports SwapLimit false: use an engine that reports each true
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/unbuilt/task.star
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
            exit status: Some(1)
            pumpkin: the driver ended before the run started, by signal: 9 (SIGKILL)
            the driver is gone, and made no record: true
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
            exit status: Some(1)
            pumpkin: the driver ended before the run started, by signal: 15 (SIGTERM)
            the driver is gone, and made no record: true
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
            ended by signal: 9
            the driver is gone, and made no record: true
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
            ended by signal: 15
            the driver is gone, and made no record: true
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
            ended by signal: 2
            the driver is gone, and made no record: true
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
            exit status: Some(1)
            pumpkin: the driver ended before the run started, by signal: 9 (SIGKILL)
            the driver is gone, and made no record: true
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
            ended by signal: 15
            the driver is gone, and made no record: true
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
            ended by signal: 2
            the driver is gone, and made no record: true
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
            exit status: Some(1)
            pumpkin: run <id> records into <scratch>/runs/<id>
            pumpkin: the driver ended before the run started, by signal: 9 (SIGKILL)
            pumpkin: run <id> is lost: its driver ended without writing outcome.json, so nothing says how it ended; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star
            pumpkin: to remove what it left: pumpkin cancel --runs <scratch>/runs <id>
            pumpkin: to remove what every run that has ended left: pumpkin reap --runs <scratch>/runs
            outcome.json: none
            printed the id alone: false
            $ pumpkin wait <id> --runs runs
            exit status: Some(1)
            pumpkin: run <id> is lost: its driver ended without writing outcome.json, so nothing says how it ended; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star
            pumpkin: to remove what it left: pumpkin cancel --runs <scratch>/runs <id>
            pumpkin: to remove what every run that has ended left: pumpkin reap --runs <scratch>/runs
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
            exit status: Some(1)
            pumpkin: run <id> records into <scratch>/runs/<id>
            pumpkin: run <id> was interrupted by SIGTERM; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star
            record: driver.log, driver.pid, ir.json, outcome.json, provider.json, record.json
            outcome.json: {"signal":"SIGTERM","state":"interrupted"}
            printed outcome.json: true
            $ pumpkin wait <id> --runs runs
            exit status: Some(1)
            pumpkin: run <id> was interrupted by SIGTERM; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star
            printed outcome.json: {"signal":"SIGTERM","state":"interrupted"}
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
            ended by signal: 15
            pumpkin: run <id> records into <scratch>/runs/<id>
            outcome.json: none
            printed the id alone: false
            $ pumpkin wait <id> --runs runs
            exit status: Some(1)
            pumpkin: run <id> is lost: its driver ended without writing outcome.json, so nothing says how it ended; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star
            pumpkin: to remove what it left: pumpkin cancel --runs <scratch>/runs <id>
            pumpkin: to remove what every run that has ended left: pumpkin reap --runs <scratch>/runs
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
            ended by signal: 2
            pumpkin: run <id> records into <scratch>/runs/<id>
            outcome.json: none
            printed the id alone: false
            $ pumpkin wait <id> --runs runs
            exit status: Some(1)
            pumpkin: run <id> is lost: its driver ended without writing outcome.json, so nothing says how it ended; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star
            pumpkin: to remove what it left: pumpkin cancel --runs <scratch>/runs <id>
            pumpkin: to remove what every run that has ended left: pumpkin reap --runs <scratch>/runs"#]],
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
    let mut run = stand_in.run("stand-in/task.star", &[]);
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
    let mut run = stand_in.run("stand-in/task.star", &["--wait"]);
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
    let mut run = stand_in.run("stand-in/task.star", &[]);
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
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
            exit status: Some(0)
            pumpkin: run <id> records into <scratch>/runs/<id>
            pumpkin: run <id> started
            pumpkin: to follow it: pumpkin wait --runs <scratch>/runs <id>
            pumpkin: to stop it: pumpkin cancel --runs <scratch>/runs <id>
            outcome.json: none
            printed the id alone: true
            the driver runs, leading a session of its own: true
            the driver's name: pumpkin-driver
            $ pumpkin wait <id> --runs runs
            exit status: Some(3)
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container, failed at <t>: the agent's sandbox didn't start: `docker create` failed with exit status: 1: the stand-in creates nothing: fix what Docker reports; the work phase ended at <t>
              sandboxes: 1 on local/container, <t> in all, none created
            pumpkin: run <id> finished without a score: the agent submitted nothing, and the work phase ended when the agent failed: the agent's sandbox didn't start: `docker create` failed with exit status: 1: the stand-in creates nothing: fix what Docker reports; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star
            printed outcome.json: {"score":null,"state":"finished","usage":{"model_requests":0,"ran_ns":"<ns>","sandboxes":[{"class":"container","count":1,"lasted_ns":"<ns>","provider":"local"}],"tokens":{"input":0,"output":0,"responses":0}},"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: `docker create` failed with exit status: 1: the stand-in creates nothing: fix what Docker reports"}}
            driver.log: ""
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml --wait
            ended by signal: 2
            pumpkin: run <id> records into <scratch>/runs/<id>
            pumpkin: run <id> started
            pumpkin: following run <id>, recorded in <scratch>/runs/<id>: ending `pumpkin run` does not stop it
            outcome.json: none
            printed the id alone: false
            the driver runs, leading a session of its own: true
            the driver's name: pumpkin-driver
            $ pumpkin wait <id> --runs runs
            exit status: Some(3)
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container, failed at <t>: the agent's sandbox didn't start: `docker create` failed with exit status: 1: the stand-in creates nothing: fix what Docker reports; the work phase ended at <t>
              sandboxes: 1 on local/container, <t> in all, none created
            pumpkin: run <id> finished without a score: the agent submitted nothing, and the work phase ended when the agent failed: the agent's sandbox didn't start: `docker create` failed with exit status: 1: the stand-in creates nothing: fix what Docker reports; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star
            printed outcome.json: {"score":null,"state":"finished","usage":{"model_requests":0,"ran_ns":"<ns>","sandboxes":[{"class":"container","count":1,"lasted_ns":"<ns>","provider":"local"}],"tokens":{"input":0,"output":0,"responses":0}},"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: `docker create` failed with exit status: 1: the stand-in creates nothing: fix what Docker reports"}}
            driver.log: ""
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
            ended by signal: 15
            pumpkin: run <id> records into <scratch>/runs/<id>
            outcome.json: none
            printed the id alone: false
            the driver runs, leading a session of its own: true
            the driver's name: pumpkin-driver
            $ pumpkin wait <id> --runs runs
            exit status: Some(3)
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container, failed at <t>: the agent's sandbox didn't start: `docker create` failed with exit status: 1: the stand-in creates nothing: fix what Docker reports; the work phase ended at <t>
              sandboxes: 1 on local/container, <t> in all, none created
            pumpkin: run <id> finished without a score: the agent submitted nothing, and the work phase ended when the agent failed: the agent's sandbox didn't start: `docker create` failed with exit status: 1: the stand-in creates nothing: fix what Docker reports; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star
            printed outcome.json: {"score":null,"state":"finished","usage":{"model_requests":0,"ran_ns":"<ns>","sandboxes":[{"class":"container","count":1,"lasted_ns":"<ns>","provider":"local"}],"tokens":{"input":0,"output":0,"responses":0}},"verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_failed","reason":"the agent's sandbox didn't start: `docker create` failed with exit status: 1: the stand-in creates nothing: fix what Docker reports"}}
            driver.log: """#]],
    );
}

/// What becomes of a run that `run` started in a scratch directory of its own, `name`, with
/// SIGHUP ignored if `ignoring`, once its driver, held at `create`, has been sent `signals`, each
/// as `kill --signal` names it, and let go on if `released`: which of SIGHUP, SIGINT and SIGTERM
/// the driver catches and ignores as it is held; then, once it has ended, what `wait` finds of
/// the run, and what the driver wrote to `driver.log`.
fn signaled(name: &str, ignoring: bool, signals: &[&str], released: bool) -> String {
    let scratch = Scratch::new(name);
    let stand_in = StandIn::new(&scratch);
    stand_in.hold("create");
    let mut run = stand_in.run("stand-in/task.star", &[]);
    if ignoring {
        // As `nohup` starts a program.
        run = through_shell(&run, "trap '' HUP");
    }
    let output = run.output().unwrap();
    let driver = stand_in.held("create");
    let mut lines = vec![format!("the driver {}", dispositions(driver))];
    for name in signals {
        assert!(signal(name, driver), "the driver is gone");
    }
    if released {
        stand_in.release("create");
    }
    lines.push(wait_once_ended(&scratch, &output, driver));
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    let (id, dir) = record(&stderr).unwrap();
    let log = fs::read_to_string(scratch.0.join(dir).join("driver.log")).unwrap();
    lines.push(format!("driver.log: {:?}", log.replace(id, "<id>")));
    let text = lines.join("\n");
    text.replace(&scratch.0.display().to_string(), "<scratch>")
}

/// Which of SIGHUP, SIGINT and SIGTERM the process `pid` catches, and which it ignores.
fn dispositions(pid: i64) -> String {
    let (caught, ignored) = masks(pid).unwrap();
    let names = |mask: u64| {
        let signals = [
            (libc::SIGHUP, "HUP"),
            (libc::SIGINT, "INT"),
            (libc::SIGTERM, "TERM"),
        ];
        let set = signals.iter().filter(|(number, _)| has(mask, *number));
        let names: Vec<&str> = set.map(|(_, name)| *name).collect();
        if names.is_empty() {
            "none".to_owned()
        } else {
            names.join(" ")
        }
    };
    format!("catches {}; ignores {}", names(caught), names(ignored))
}

/// The masks of the signals that the process `pid` catches and ignores, as its `status` gives
/// them, if it runs.
fn masks(pid: i64) -> Option<(u64, u64)> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let mask = |field: &str| {
        let line = status
            .lines()
            .find_map(|line| line.strip_prefix(field))
            .unwrap();
        u64::from_str_radix(line.trim(), 16).unwrap()
    };
    Some((mask("SigCgt:"), mask("SigIgn:")))
}

/// Whether `mask`, a signal mask as `/proc` gives one, holds signal `number`.
fn has(mask: u64, number: libc::c_int) -> bool {
    mask >> (number - 1) & 1 == 1
}

#[test]
fn signaled_after_the_start() {
    let scratch = Scratch::new("signaled");
    let steps = [
        signaled("signaled-term", false, &["TERM"], true),
        signaled("signaled-hup", false, &["HUP"], true),
        signaled("signaled-int-twice", false, &["INT", "INT"], true),
        signaled("signaled-ignoring", true, &["HUP", "TERM"], true),
    ];
    check(
        &scratch,
        steps.map(Step::Ran),
        false,
        &[],
        &expect![[r#"
            the driver catches HUP INT TERM; ignores none
            $ pumpkin wait <id> --runs runs
            exit status: Some(1)
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container
              sandboxes: 1 on local/container, <t> in all, none created
            pumpkin: run <id> was interrupted by SIGTERM; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star
            printed outcome.json: {"signal":"SIGTERM","state":"interrupted","usage":{"model_requests":0,"ran_ns":"<ns>","sandboxes":[{"class":"container","count":1,"lasted_ns":"<ns>","provider":"local"}],"tokens":{"input":0,"output":0,"responses":0}}}
            driver.log: "pumpkin: run <id> was interrupted by SIGTERM; its record is <scratch>/runs/<id>\npumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star\n"
            the driver catches HUP INT TERM; ignores none
            $ pumpkin wait <id> --runs runs
            exit status: Some(1)
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container
              sandboxes: 1 on local/container, <t> in all, none created
            pumpkin: run <id> was interrupted by SIGHUP; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star
            printed outcome.json: {"signal":"SIGHUP","state":"interrupted","usage":{"model_requests":0,"ran_ns":"<ns>","sandboxes":[{"class":"container","count":1,"lasted_ns":"<ns>","provider":"local"}],"tokens":{"input":0,"output":0,"responses":0}}}
            driver.log: "pumpkin: run <id> was interrupted by SIGHUP; its record is <scratch>/runs/<id>\npumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star\n"
            the driver catches HUP INT TERM; ignores none
            $ pumpkin wait <id> --runs runs
            exit status: Some(1)
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container
              sandboxes: 1 on local/container, <t> in all, none created
            pumpkin: run <id> was interrupted by SIGINT; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star
            printed outcome.json: {"signal":"SIGINT","state":"interrupted","usage":{"model_requests":0,"ran_ns":"<ns>","sandboxes":[{"class":"container","count":1,"lasted_ns":"<ns>","provider":"local"}],"tokens":{"input":0,"output":0,"responses":0}}}
            driver.log: "pumpkin: run <id> was interrupted by SIGINT; its record is <scratch>/runs/<id>\npumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star\n"
            the driver catches INT TERM; ignores HUP
            $ pumpkin wait <id> --runs runs
            exit status: Some(1)
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container
              sandboxes: 1 on local/container, <t> in all, none created
            pumpkin: run <id> was interrupted by SIGTERM; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star
            printed outcome.json: {"signal":"SIGTERM","state":"interrupted","usage":{"model_requests":0,"ran_ns":"<ns>","sandboxes":[{"class":"container","count":1,"lasted_ns":"<ns>","provider":"local"}],"tokens":{"input":0,"output":0,"responses":0}}}
            driver.log: "pumpkin: run <id> was interrupted by SIGTERM; its record is <scratch>/runs/<id>\npumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/stand-in/task.star\n""#]],
    );
}

/// What a test on Docker leaves behind, to remove once it is over, however it ends: for each
/// run recorded in `runs` its driver and its containers; its socket directory goes with the
/// scratch directory, as its record does.
struct Cleanup<'a> {
    containers: &'a Reached,
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
            let Ok(tag) = RunTag::try_from(id) else {
                continue;
            };
            if let Ok(listed) = self.containers.list(&tag) {
                let ids: Vec<_> = listed.into_iter().map(|listed| listed.id).collect();
                let _ = self.containers.delete(&tag, &ids);
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

#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the run's steps and their snapshot"
)]
fn lost_on_docker() {
    let scratch = Scratch::new("lost");
    let sandboxd = sandboxd().into_os_string().into_string().unwrap();
    let context = docker_context();
    let engine = engine(&scratch, &context);
    let containers = containers(&context);
    scratch.write_task("sleeps", &workflow("", &["sleep", "300"]));
    scratch.write("pumpkin.toml", &machine(&context));
    build_first(&scratch, "sleeps", "pumpkin.toml");
    let _cleanup = Cleanup {
        containers: &containers,
        runs: scratch.0.join("runs"),
    };
    let temp = scratch.0.join("tmp");
    fs::create_dir(&temp).unwrap();
    let mut run = scratch.pumpkin(&["run", "sleeps/task.star", "--sandboxd", &sandboxd]);
    run.args(["--runs", "runs", "--config", "pumpkin.toml"]);
    run.env("TMPDIR", &temp);
    let invoked = invocation(&run);
    let output = with_room(&mut run);
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    let (id, dir) = record(&stderr).unwrap();
    let tag = RunTag::try_from(id.to_owned()).unwrap();
    let record = scratch.0.join(dir);
    let created = record.join("agent/sandbox.json");
    poll(|| Some(()).filter(|()| created.exists()));
    let listed = containers.list(&tag).unwrap();
    let agent = listed[0].id.as_str().to_owned();
    let driver = live_driver(&record).unwrap();
    assert!(signal("KILL", driver), "the driver is gone");
    unlocked(&record);
    let rootful = !engine.report().security.rootless;
    let mut killed = vec![report(invoked, &output, &scratch.0, rootful)];
    killed.extend(sandboxes(&record));
    // By name alone, as the agent's socket goes once the agent connects, which the kill may
    // come before or after.
    let sockets = fs::read_dir(scratch.0.join(dir).join("sockets")).unwrap();
    let mut sockets: Vec<_> = sockets.map(|entry| entry.unwrap().file_name()).collect();
    sockets.sort();
    killed.push(format!("its socket directories: {sockets:?}"));
    killed.push(format!("its temporary directory holds: {:?}", files(&temp)));
    let mut steps = vec![killed.join("\n")];
    let pumpkin = |args: &[&str]| ran(scratch.pumpkin(args), &scratch.0, false);
    steps.push(pumpkin(&["wait", id, "--runs", "runs"]));
    steps.push(pumpkin(&["cancel", id, "--runs", "runs"]));
    // The image that the run built, which its IR names.
    let ir = fs::read_to_string(record.join("ir.json")).unwrap();
    let ir: serde_json::Value = serde_json::from_str(&ir).unwrap();
    let image = ir["roles"][0]["image"].as_str().unwrap();
    let mut create = test_support::docker();
    create.args([
        "create",
        "--label",
        &format!("pumpkin.run={id}"),
        image,
        "true",
    ]);
    let created = create.output().unwrap();
    assert!(created.status.success(), "{created:?}");
    let unstarted = String::from_utf8(created.stdout).unwrap().trim().to_owned();
    steps.push(pumpkin(&["reap", "--runs", "runs"]));
    let left = containers.list(&tag).unwrap().len();
    steps.push(format!("containers left: {left}"));
    let shown = [
        (&sandboxd[..], "<sandboxd>"),
        (id, "<id>"),
        (&agent[..], "<the agent's container>"),
        (&unstarted[..], "<the unstarted container>"),
    ];
    check(
        &scratch,
        steps.into_iter().map(Step::Ran),
        false,
        &shown,
        &expect![[r#"
            $ TMPDIR=<scratch>/tmp pumpkin run sleeps/task.star --sandboxd <sandboxd> --runs runs --config pumpkin.toml
            exit status: Some(0)
            pumpkin: run <id> records into <scratch>/runs/<id>
            pumpkin: run <id> started
            pumpkin: to follow it: pumpkin wait --runs <scratch>/runs <id>
            pumpkin: to stop it: pumpkin cancel --runs <scratch>/runs <id>
            outcome.json: none
            printed the id alone: true
            the agent's sandbox, agent/sandbox.json: {"id":"<container>","provider":"local","report":null}
            its socket directories: ["agent"]
            its temporary directory holds: []
            $ pumpkin wait <id> --runs runs
            exit status: Some(1)
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container, started at <t>
              sandboxes: 1 on local/container, <t> in all, 1 created, 0 deleted
            pumpkin: run <id> is lost: its driver ended without writing outcome.json, so nothing says how it ended; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <sandboxd> <scratch>/sleeps/task.star
            pumpkin: to remove what it left: pumpkin cancel --runs <scratch>/runs <id>
            pumpkin: to remove what every run that has ended left: pumpkin reap --runs <scratch>/runs
            $ pumpkin cancel <id> --runs runs
            exit status: Some(0)
            pumpkin: run <id> left 1 sandbox on provider `local`, now removed: <the agent's container>
            pumpkin: run <id> left its socket directory, now removed: <scratch>/runs/<id>/sockets
            pumpkin: run <id> was lost, as its driver ended without an outcome.json: it is now recorded as failed
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container, started at <t>
              sandboxes: 1 on local/container, <t> in all, 1 created, 0 deleted
            pumpkin: run <id> failed: its driver ended without an outcome; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <sandboxd> <scratch>/sleeps/task.star
            printed outcome.json: {"reason":"its driver ended without an outcome","state":"failed"}
            $ pumpkin reap --runs runs
            exit status: Some(0)
            pumpkin: run <id> left 1 sandbox on provider `local`, now removed: <the unstarted container>
            printed:
              <id>
            containers left: 0"#]],
    );
}

#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn signaled_on_docker() {
    let scratch = Scratch::new("signaled-docker");
    let sandboxd = sandboxd().to_str().unwrap().to_owned();
    let context = docker_context();
    let containers = containers(&context);
    let ir = workflow("", &["sleep", "300"]);
    let deadline = ir["roles"][0]["runtime"]["agent"]["deadline_seconds"].as_u64();
    let deadline = Duration::from_secs(deadline.unwrap());
    scratch.write_task("sleeps", &ir);
    scratch.write("pumpkin.toml", &machine(&context));
    build_first(&scratch, "sleeps", "pumpkin.toml");
    let _cleanup = Cleanup {
        containers: &containers,
        runs: scratch.0.join("runs"),
    };
    let mut run = scratch.pumpkin(&["run", "sleeps/task.star", "--sandboxd", &sandboxd]);
    run.args(["--runs", "runs", "--config", "pumpkin.toml"]);
    let output = with_room(&mut run);
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    let (id, dir) = record(&stderr).unwrap();
    let record = scratch.0.join(dir);
    let log = || fs::read_to_string(record.join("log.jsonl")).unwrap_or_default();
    poll(|| Some(()).filter(|()| log().contains(r#""kind":"agent_started""#)));
    let driver = live_driver(&record).unwrap();
    assert!(signal("TERM", driver), "the driver is gone");
    // So that `wait` finds the run ended, rather than follow it for as long as its end takes.
    unlocked(&record);
    let wait = ran(
        scratch.pumpkin(&["wait", id, "--runs", "runs"]),
        &scratch.0,
        false,
    );
    // The deletes of the sandboxes the stop left follow the stop's note.
    let log = log();
    let records = log
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap());
    let last = records
        .rev()
        .find(|record| record["note"].is_string())
        .unwrap();
    let at = Duration::from_nanos(last["at_ns"].as_str().unwrap().parse().unwrap());
    let tag = RunTag::try_from(id.to_owned()).unwrap();
    let sockets = record.join("sockets");
    let lines = [
        wait,
        format!("the run log's last note: {}", last["note"]),
        format!("it came before the deadline: {}", at < deadline),
        format!("containers left: {}", containers.list(&tag).unwrap().len()),
        format!("socket directory left: {}", sockets.exists()),
    ];
    let shown = [(id, "<id>"), (&sandboxd[..], "<sandboxd>")];
    check(
        &scratch,
        lines.map(Step::Ran),
        false,
        &shown,
        &expect![[r#"
            $ pumpkin wait <id> --runs runs
            exit status: Some(1)
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container, started at <t>
              sandboxes: 1 on local/container, <t> in all, 1 created, all deleted
            pumpkin: run <id> was interrupted by SIGTERM; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <sandboxd> <scratch>/sleeps/task.star
            printed outcome.json: {"signal":"SIGTERM","state":"interrupted","usage":{"model_requests":0,"ran_ns":"<ns>","sandboxes":[{"class":"container","count":1,"lasted_ns":"<ns>","provider":"local"}],"tokens":{"input":0,"output":0,"responses":0}}}
            the run log's last note: "the run was interrupted by SIGTERM"
            it came before the deadline: true
            containers left: 0
            socket directory left: false"#]],
    );
}
