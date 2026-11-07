//! `sandboxd` and the gateway's end of its session, together. `sandboxd` runs here as an
//! ordinary process, not as a sandbox's first process, and a temporary directory stands in for
//! the sandbox. So `sandboxd` serves no ops, which it serves only at the sandbox's own
//! `/pumpkin`, nor the model endpoint, at a port of the sandbox's own loopback: the Docker
//! provider's tests make ops and model requests, in real sandboxes. A fake gateway sends
//! what the gateway's end never would, or holds up what only a slow link would.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test helper fails the test that calls it"
)]

use std::cell::Cell;
use std::io::{self, Read};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};
use std::{fs, thread};

use expect_test::{Expect, expect};
use pumpkin_gateway::{Arrival, Listener, Process, Put, Session};
use pumpkin_schema::ir::{AbsolutePath, Argv, Env, EnvVar, Mode};
use pumpkin_schema::sandbox::frame::MAX_BODY_BYTES;
use pumpkin_schema::sandbox::tree::Limits;
use pumpkin_schema::sandbox::{
    Entry, ExitStatus, FromSandbox, Launch, RelativePath, ToSandbox, dir_mode, file_mode, frame,
};
use stdx::Deadline;
use stdx::process::Environment;

/// What `sandboxd` and the tests' other programs get of the tests' environment: `PATH` alone,
/// to find the programs they run.
const PATH_ONLY: Environment = Environment::Only(&["PATH"]);

/// A request's deadline, which only a test that hangs reaches, so no outcome rests on it.
fn hung() -> Deadline {
    Deadline::after(Duration::from_secs(60))
}

/// A temporary directory standing in for a sandbox, with `sandboxd` serving its session.
struct Sandbox {
    root: PathBuf,
    session: Session,
    arrivals: Receiver<Arrival>,
    sandboxd: Child,
    commands: Cell<u32>,
}

impl Sandbox {
    /// Starts `sandboxd` and puts the directories a role expects.
    fn start(name: &str) -> Self {
        let id = std::process::id();
        let root = std::env::temp_dir().join(format!("pumpkin-sandboxd-{id}-{name}"));
        if root.exists() {
            fs::remove_dir_all(&root).unwrap();
        }
        fs::create_dir_all(&root).unwrap();
        // Without symbolic links, as the kernel names the working directories that
        // `processes` finds the sandbox's processes by.
        let root = fs::canonicalize(root).unwrap();
        let socket = root.join("gateway.sock");
        let listener = Listener::bind(socket.clone()).unwrap();
        let sandboxd = stdx::process::command(env!("CARGO_BIN_EXE_sandboxd"), PATH_ONLY)
            .arg(&socket)
            .spawn()
            .unwrap();
        let (sender, arrivals) = mpsc::channel();
        let accepted = listener.accept(hung(), None, false, sender, |arrival| arrival);
        let mut session = accepted.unwrap();
        let dirs = ["in", "out", "home", "log"].map(relative);
        session
            .put(
                &absolute(&root),
                &dirs.each_ref().map(|dir| Put::Dir(dir, dir_mode())),
                hung(),
            )
            .unwrap();
        Self {
            root,
            session,
            arrivals,
            sandboxd,
            commands: Cell::new(0),
        }
    }

    /// How `process` exited, if its exit is what arrives next, within `wait`.
    fn exit(&self, process: Process, wait: Duration) -> Option<ExitStatus> {
        match self.arrivals.recv_timeout(wait) {
            Ok(Arrival::Exited {
                process: exited,
                status,
            }) if exited == process => Some(status),
            Err(RecvTimeoutError::Timeout) => None,
            arrived => panic!("the exit of {process:?} was to arrive, not {arrived:?}"),
        }
    }

    /// A path in the sandbox.
    fn at(&self, relative: &str) -> AbsolutePath {
        absolute(&self.root.join(relative))
    }

    /// How to run `script` in the sandbox's `home`, with its output in the sandbox's `log`.
    fn sh(&self, script: &str, env: &[&str]) -> Launch {
        let n = self.commands.get() + 1;
        self.commands.set(n);
        let env = env
            .iter()
            .map(|var| EnvVar::try_from((*var).to_owned()).unwrap());
        Launch {
            argv: Argv::try_from(["sh", "-c", script].map(str::to_owned).to_vec()).unwrap(),
            env: Env::try_from(env.collect::<Vec<_>>()).unwrap(),
            cwd: self.at("home"),
            stdin: None,
            stdout: self.at(&format!("log/{n}.stdout")),
            stderr: self.at(&format!("log/{n}.stderr")),
        }
    }

    /// Collects the sandbox's `dir` and lists what arrived.
    fn collect(&mut self, dir: &str) -> Result<Vec<String>, String> {
        let collected = self.root.join("collected");
        fs::create_dir_all(&collected).unwrap();
        let limits = Limits {
            bytes: 4 << 20,
            entries: 100,
        };
        let into = collected.join(dir.replace('/', "-"));
        let outcome = self.session.collect(&self.at(dir), &into, limits, hung());
        outcome.map_err(|error| self.show(&error))?;
        let mut lines = Vec::new();
        self.list(&collected, &into, &mut lines);
        Ok(lines)
    }

    /// Each file below `dir` with its contents, or its size if they aren't text, and each empty
    /// directory.
    fn list(&self, base: &Path, dir: &Path, lines: &mut Vec<String>) {
        let mut paths: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        paths.sort();
        if paths.is_empty() {
            lines.push(format!("{}/", dir.strip_prefix(base).unwrap().display()));
        }
        for path in paths {
            if path.is_dir() {
                self.list(base, &path, lines);
            } else {
                let name = path.strip_prefix(base).unwrap().display();
                let contents = match String::from_utf8(fs::read(&path).unwrap()) {
                    Ok(text) => self.show(&text.trim_end()),
                    Err(bytes) => format!("{} bytes", bytes.as_bytes().len()),
                };
                lines.push(format!("{name}: {contents}"));
            }
        }
    }

    /// `text` with the sandbox's root written `$ROOT`.
    fn show(&self, text: &dyn std::fmt::Display) -> String {
        text.to_string()
            .replace(self.root.to_str().unwrap(), "$ROOT")
    }
}

fn absolute(path: &Path) -> AbsolutePath {
    AbsolutePath::try_from(path.to_str().unwrap().to_owned()).unwrap()
}

fn relative(path: &str) -> RelativePath {
    RelativePath::try_from(path.to_owned()).unwrap()
}

/// The IDs of the processes whose working directory is in the sandbox at `root`, as the
/// commands started there and whatever they start have, polled every 10 ms until `done` holds
/// of them or `timeout` passes. A process that has exited has no working directory, so a
/// zombie isn't one of them.
fn processes(root: &Path, timeout: Duration, done: impl Fn(&[u32]) -> bool) -> Vec<u32> {
    let deadline = Instant::now() + timeout;
    loop {
        // A process may exit between being listed and being looked at.
        let found: Vec<u32> = fs::read_dir("/proc")
            .unwrap()
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let pid = entry.file_name().to_str()?.parse().ok()?;
                let cwd = fs::read_link(entry.path().join("cwd")).ok()?;
                cwd.starts_with(root).then_some(pid)
            })
            .collect();
        if done(&found) || Instant::now() >= deadline {
            return found;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// How `sandboxd` exited after its session ended. It has 10 s to, or it is killed and the test
/// fails.
fn exited(sandboxd: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Some(status) = sandboxd.try_wait().unwrap() {
            return status;
        }
        thread::sleep(Duration::from_millis(10));
    }
    sandboxd.kill().unwrap();
    sandboxd.wait().unwrap();
    panic!("sandboxd ran on 10 s after its session ended: it should exit with the session");
}

/// Runs `scenario` against a new sandbox, ends the session by dropping it, and checks what the
/// scenario reported, what arrived after it, how `sandboxd` exited, and how many processes were
/// left running in the sandbox, against `expect`. Those left get 2 s to exit, and are then
/// killed.
fn check(name: &str, scenario: impl FnOnce(&mut Sandbox) -> Vec<String>, expect: &Expect) {
    let mut sandbox = Sandbox::start(name);
    let mut lines = scenario(&mut sandbox);
    let Sandbox {
        root,
        session,
        arrivals,
        mut sandboxd,
        ..
    } = sandbox;
    drop(session);
    lines.extend(
        arrivals
            .try_iter()
            .map(|arrival| format!("then {arrival:?}")),
    );
    lines.push(format!("sandboxd: {}", exited(&mut sandboxd)));
    let left = processes(&root, Duration::from_secs(2), <[u32]>::is_empty);
    lines.push(format!("left running: {}", left.len()));
    if !left.is_empty() {
        // `kill` fails for any that exited meanwhile, which is as good as killed.
        let pids = left.iter().map(u32::to_string);
        stdx::process::command("kill", PATH_ONLY)
            .args(["--signal", "KILL"])
            .args(pids)
            .status()
            .unwrap();
    }
    expect.assert_eq(&lines.join("\n"));
    fs::remove_dir_all(root).unwrap();
}

/// What a judge does: reads its payload, writes a result and evidence, and exits. The payload
/// is longer than two frames, and the judge keeps it as evidence, so that it is put and then
/// collected in several frames.
#[test]
fn judge() {
    check(
        "judge",
        |sandbox| {
            let (slot, payload) = (relative("in/payload"), relative("in/payload/raw"));
            let raw = pattern(2 * MAX_BODY_BYTES + 12345);
            let root = absolute(&sandbox.root);
            let put = [
                Put::Dir(&slot, dir_mode()),
                Put::File(&payload, file_mode(), &raw),
            ];
            sandbox.session.put(&root, &put, hung()).unwrap();
            let script = r#"
                printf '{"bytes": %s}' "$(wc --bytes < ../in/payload/raw)" > ../out/result.json
                mkdir ../out/evidence && cp ../in/payload/raw ../out/evidence/payload
                mkdir ../out/empty
                echo "home=$HOME mode=$MODE in $(pwd)"
                echo warning >&2
                exit 3"#;
            let launch = sandbox.sh(script, &["HOME=/pumpkin/home", "MODE=train"]);
            let judge = sandbox.session.start(launch, hung()).unwrap();
            let exited = sandbox.exit(judge, Duration::from_secs(10)).unwrap();
            let mut lines = vec![format!("exited: {exited:?}")];
            sandbox.session.freeze(hung()).unwrap();
            lines.extend(sandbox.collect("out").unwrap());
            let kept = fs::read(sandbox.root.join("collected/out/evidence/payload")).unwrap();
            lines.push(format!("payload collected intact: {}", kept == raw));
            lines.extend(sandbox.collect("log").unwrap());
            lines
        },
        &expect![[r#"
            exited: Code(3)
            out/empty/
            out/evidence/payload: 2109497 bytes
            out/result.json: {"bytes": 2109497}
            payload collected intact: true
            log/1.stderr: warning
            log/1.stdout: home=/pumpkin/home mode=train in $ROOT/home
            then Ended { reason: "the owner dropped the session" }
            sandboxd: exit status: 0
            left running: 0"#]],
    );
}

/// A tree put gives what it makes its mode exactly, whatever the umask, and keeps a directory
/// that exists as it is; a command reads its standard input from the regular file its launch
/// names, which must exist.
#[test]
fn modes_and_stdin() {
    check(
        "modes-and-stdin",
        |sandbox| {
            let mode = |mode: &str| Mode::try_from(mode.to_owned()).unwrap();
            let paths = ["home", "in/tools", "in/tools/tool", "in/prompt"].map(relative);
            let put = [
                Put::Dir(&paths[0], mode("0700")),
                Put::Dir(&paths[1], mode("0777")),
                Put::File(&paths[2], mode("0750"), b"#!/bin/sh\nwc --lines\n"),
                Put::File(&paths[3], mode("0666"), b"line one\nline two\n"),
            ];
            sandbox
                .session
                .put(&absolute(&sandbox.root), &put, hung())
                .unwrap();
            let script = "cd .. && stat --format='%a %n' home in/tools in/tools/tool in/prompt; \
                          in/tools/tool";
            let mut launch = sandbox.sh(script, &[]);
            launch.stdin = Some(sandbox.at("in/prompt"));
            let command = sandbox.session.start(launch, hung()).unwrap();
            let exited = sandbox.exit(command, Duration::from_secs(10)).unwrap();
            let mut lines = vec![format!("exited: {exited:?}")];
            let mut missing = sandbox.sh("true", &[]);
            missing.stdin = Some(sandbox.at("in/missing"));
            let started = sandbox.session.start(missing, hung()).map(|_| ());
            lines.push(format!("missing stdin: {}", show_error(sandbox, started)));
            lines.extend(sandbox.collect("log").unwrap());
            lines
        },
        &expect![[r#"
            exited: Code(0)
            missing stdin: $ROOT/in/missing: No such file or directory (os error 2)
            log/1.stderr: 
            log/1.stdout: 755 home
            777 in/tools
            750 in/tools/tool
            666 in/prompt
            2
            then Ended { reason: "the owner dropped the session" }
            sandboxd: exit status: 0
            left running: 0"#]],
    );
}

/// `len` bytes that repeat out of step with the frames, so that a frame lost, repeated or out of
/// order shows.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|n| u8::try_from(n % 251).unwrap()).collect()
}

/// Freezing kills every process in each command's group, not only the command's own, returns
/// once the commands have exited, and refuses new commands.
#[test]
fn freezing() {
    check(
        "freezing",
        |sandbox| {
            let launch = sandbox.sh("sleep 60 & sleep 60 & wait", &[]);
            let group = sandbox.session.start(launch, hung()).unwrap();
            // The shell and both sleeps, so that freezing finds each of them to kill.
            let timeout = Duration::from_secs(10);
            let running = processes(&sandbox.root, timeout, |found| found.len() == 3);
            sandbox.session.freeze(hung()).unwrap();
            let exited = sandbox.exit(group, Duration::ZERO);
            let mut lines = vec![
                format!("running before freezing: {}", running.len()),
                format!("exited when frozen: {exited:?}"),
            ];
            let after = sandbox.session.start(sandbox.sh("true", &[]), hung());
            lines.push(format!("start after freezing: {}", after.unwrap_err()));
            lines
        },
        &expect![[r#"
            running before freezing: 3
            exited when frozen: Some(Signal(9))
            start after freezing: the sandbox is frozen: start commands before freezing it
            then Ended { reason: "the owner dropped the session" }
            sandboxd: exit status: 0
            left running: 0"#]],
    );
}

/// A sandbox whose processes fill its limit, as a fork bomb's do, so that `sandboxd` can start
/// neither a command nor a thread, still freezes, and its trees are still collected, as the
/// run's record. `sandboxd`'s own limit on its user's processes, lowered to 1 once it runs,
/// stands in for the sandbox's: either fails a new process or thread alike.
#[test]
fn full_sandbox() {
    check(
        "full",
        |sandbox| {
            let launch = sandbox.sh("exec sleep 60", &[]);
            let sleep = sandbox.session.start(launch, hung()).unwrap();
            fs::write(sandbox.root.join("out/kept"), "kept").unwrap();
            let pid = sandbox.sandboxd.id().to_string();
            let limited = stdx::process::command("prlimit", PATH_ONLY)
                .args(["--pid", &pid, "--nproc=1"])
                .status()
                .unwrap_or_else(|error| {
                    panic!("`prlimit` didn't run: {error}: install util-linux, which has it")
                });
            assert!(limited.success(), "`prlimit` failed: {limited}");
            let started = sandbox.session.start(sandbox.sh("true", &[]), hung());
            let started = started.map(|_| ());
            assert!(
                started.is_err(),
                "a command started past a limit of 1 process: run the test as a user the kernel \
                 holds to it, not root, and without `CAP_SYS_RESOURCE` or `CAP_SYS_ADMIN`"
            );
            let mut lines = vec![format!("start: {}", show_error(sandbox, started))];
            sandbox.session.freeze(hung()).unwrap();
            let exited = sandbox.exit(sleep, Duration::ZERO);
            lines.push(format!("exited when frozen: {exited:?}"));
            lines.extend(sandbox.collect("out").unwrap());
            lines
        },
        &expect![[r#"
            start: `sh` didn't start in $ROOT/home: Resource temporarily unavailable (os error 11)
            exited when frozen: Some(Signal(9))
            out/kept: kept
            then Ended { reason: "the owner dropped the session" }
            sandboxd: exit status: 0
            left running: 0"#]],
    );
}

/// Requests that `sandboxd` refuses, each with the reason it gives.
#[test]
fn refusals() {
    check(
        "refusals",
        |sandbox| {
            let mut lines = Vec::new();
            let mut missing = sandbox.sh("", &[]);
            missing.argv = Argv::try_from(vec!["/nonexistent/program".to_owned()]).unwrap();
            let started = sandbox.session.start(missing, hung()).map(|_| ());
            lines.push(format!("missing program: {}", show_error(sandbox, started)));
            // The role's code can put anything at a command's output paths, such as a link to
            // `sandboxd`'s own standard error, or a FIFO that nothing reads.
            let link = sandbox.sh("echo leaked", &[]);
            std::os::unix::fs::symlink("/proc/self/fd/2", link.stdout.as_str()).unwrap();
            let started = sandbox.session.start(link, hung()).map(|_| ());
            lines.push(format!("stdout a link: {}", show_error(sandbox, started)));
            let fifo = sandbox.sh("true", &[]);
            let made = stdx::process::command("mkfifo", PATH_ONLY)
                .arg(fifo.stdout.as_str())
                .status();
            assert!(made.unwrap().success());
            let started = sandbox.session.start(fifo, hung()).map(|_| ());
            lines.push(format!("stdout a FIFO: {}", show_error(sandbox, started)));
            // Started by name with its own PATH, a missing program is waited for by the
            // standard library itself, which must not race the reaper while another command
            // runs.
            let sleep = sandbox.sh("exec sleep 60", &[]);
            let _running = sandbox.session.start(sleep, hung()).unwrap();
            let mut failures: Vec<String> = (0..1000)
                .map(|_| {
                    let mut by_name = sandbox.sh("", &["PATH=/usr/bin:/bin"]);
                    by_name.argv = Argv::try_from(vec!["no-such-program".to_owned()]).unwrap();
                    let started = sandbox.session.start(by_name, hung()).map(|_| ());
                    show_error(sandbox, started)
                })
                .collect();
            failures.dedup();
            lines.push(format!("missing program by name, 1000 times: {failures:?}"));
            let file = relative("in/file");
            let twice = [
                Put::File(&file, file_mode(), b"first"),
                Put::File(&file, file_mode(), b"second"),
            ];
            let put = sandbox
                .session
                .put(&absolute(&sandbox.root), &twice, hung());
            lines.push(format!("same file twice: {}", show_error(sandbox, put)));
            std::os::unix::fs::symlink("/etc/passwd", sandbox.root.join("out/link")).unwrap();
            lines.push(format!("link: {}", sandbox.collect("out").unwrap_err()));
            lines.push(format!(
                "missing: {}",
                sandbox.collect("missing").unwrap_err()
            ));
            lines
        },
        &expect![[r#"
            missing program: `/nonexistent/program` didn't start in $ROOT/home: No such file or directory (os error 2)
            stdout a link: $ROOT/log/2.stdout: Too many levels of symbolic links (os error 40)
            stdout a FIFO: $ROOT/log/3.stdout: No such device or address (os error 6)
            missing program by name, 1000 times: ["`no-such-program` didn't start in $ROOT/home: No such file or directory (os error 2)"]
            same file twice: in/file: File exists (os error 17)
            link: $ROOT/out/link is neither a directory nor a regular file: collect only those
            missing: $ROOT/missing: No such file or directory (os error 2)
            then Ended { reason: "the owner dropped the session" }
            sandboxd: exit status: 0
            left running: 0"#]],
    );
}

fn show_error(sandbox: &Sandbox, outcome: io::Result<()>) -> String {
    sandbox.show(&outcome.unwrap_err())
}

/// Ending the session kills the commands still running.
#[test]
fn ending_the_session() {
    check(
        "ending",
        |sandbox| {
            let launch = sandbox.sh("exec sleep 60", &[]);
            sandbox.session.start(launch, hung()).unwrap();
            let timeout = Duration::from_secs(10);
            let running = processes(&sandbox.root, timeout, |found| !found.is_empty());
            vec![format!("running: {}", running.len())]
        },
        &expect![[r#"
            running: 1
            then Ended { reason: "the owner dropped the session" }
            sandboxd: exit status: 0
            left running: 0"#]],
    );
}

/// A `sandboxd` that serves no model endpoint opens no model requests, so it exits 1, saying
/// why, once its gateway answers one. A fake gateway sends it, as the gateway's end never answers
/// a request it didn't take.
#[test]
fn model_answers() {
    check(
        "models",
        |sandbox| {
            let socket = sandbox.root.join("fake.sock");
            let end = ToSandbox::ModelEnd {
                request: 0,
                cut: false,
            };
            vec![format!(
                "a model response's end: {}",
                answered(&socket, &end)
            )]
        },
        &expect![[r#"
            a model response's end: exit status: 1: sandboxd: the session with the gateway failed: the gateway broke the protocol: it answered model request 0, which is not in flight
            then Ended { reason: "the owner dropped the session" }
            sandboxd: exit status: 0
            left running: 0"#]],
    );
}

/// A `sandboxd` whose gateway, a fake at `socket`, sends it `message` once it connects: how it
/// exited, and why.
fn answered(socket: &Path, message: &ToSandbox) -> String {
    let (mut connection, sandboxd) = fake_gateway(socket);
    frame::write(&mut connection, message, &[]).unwrap();
    ended(sandboxd)
}

/// A tree put while a collect is held up, as a slow link holds one, is written meanwhile, and
/// the collected tree then arrives whole: `sandboxd` reads on while it sends a tree, as the
/// gateway's writers beside its requests need. A fake gateway holds the collect up by reading
/// nothing until it has sent the put. Each tree is several frames, far more than a connection
/// buffers, so a `sandboxd` that read nothing while it collected would hold the put's write up
/// until its deadline.
#[test]
fn reading_while_collecting() {
    check(
        "collecting",
        |sandbox| {
            let tree = pattern(4 * MAX_BODY_BYTES);
            fs::write(sandbox.root.join("out/tree"), &tree).unwrap();
            let (mut gateway, sandboxd) = fake_gateway(&sandbox.root.join("fake.sock"));
            let frames = collect_and_put(&sandbox.root, &tree);
            let sent = (frames.iter())
                .try_for_each(|(message, body)| frame::write(&mut gateway, message, body));
            let mut lines = match sent {
                Ok(()) => {
                    let mut lines = vec!["the put, while the collect is held: sent".to_owned()];
                    lines.extend(hear_both(&mut gateway, &tree));
                    let put = fs::read(sandbox.root.join("in/put")).unwrap();
                    lines.push(format!("the put's file: intact: {}", put == tree));
                    lines
                }
                Err(error) => vec![format!("the put, while the collect is held: {error}")],
            };
            drop(gateway);
            lines.push(format!("the fake gateway's sandboxd: {}", ended(sandboxd)));
            lines
        },
        &expect![[r#"
            the put, while the collect is held: sent
            the collect: File { path: RelativePath("tree"), size: 4194304 }, intact: true
            the put: done
            the put's file: intact: true
            the fake gateway's sandboxd: exit status: 0
            then Ended { reason: "the owner dropped the session" }
            sandboxd: exit status: 0
            left running: 0"#]],
    );
}

/// The frames of a collect of the sandbox's `out` on stream 1, then of a put of `bytes` as
/// `in/put` on stream 2.
fn collect_and_put<'a>(root: &Path, bytes: &'a [u8]) -> Vec<(ToSandbox, &'a [u8])> {
    let collect = ToSandbox::Collect {
        stream: 1,
        dir: absolute(&root.join("out")),
    };
    let put = ToSandbox::Put {
        stream: 2,
        dir: absolute(&root.join("in")),
    };
    let entry = Entry::File {
        path: relative("put"),
        size: bytes.len() as u64,
    };
    let mut frames = vec![
        (collect, &[][..]),
        (put, &[]),
        (
            ToSandbox::Entry {
                stream: 2,
                entry,
                mode: file_mode(),
            },
            &[],
        ),
    ];
    let data = bytes.chunks(MAX_BODY_BYTES);
    frames.extend(data.map(|chunk| (ToSandbox::Data { stream: 2 }, chunk)));
    frames.push((ToSandbox::End { stream: 2 }, &[]));
    frames
}

/// What `gateway` hears in answer to a collect of `tree` on stream 1 and a put on stream 2, in
/// that order, whichever ends first.
fn hear_both(gateway: &mut UnixStream, tree: &[u8]) -> Vec<String> {
    let (mut entries, mut collected) = (Vec::new(), Vec::new());
    let (mut collect, mut put) = (None, None);
    while collect.is_none() || put.is_none() {
        let (message, body) = match frame::read::<FromSandbox>(gateway) {
            Ok(Some(frame)) => frame,
            Ok(None) => return vec!["sandboxd ended the session".to_owned()],
            Err(error) => return vec![format!("reading the session failed: {error}")],
        };
        match message {
            FromSandbox::Entry { stream: 1, entry } => entries.push(format!("{entry:?}")),
            FromSandbox::Data { stream: 1 } => collected.extend_from_slice(&body),
            FromSandbox::End { stream: 1 } => {
                let intact = collected == tree;
                let entries = entries.join(", ");
                collect = Some(format!("the collect: {entries}, intact: {intact}"));
            }
            FromSandbox::Done { stream: 2 } => put = Some("the put: done".to_owned()),
            other => return vec![format!("sandboxd sent {other:?} out of turn")],
        }
    }
    [collect, put].into_iter().flatten().collect()
}

/// A `sandboxd` whose gateway is a fake at `socket`, and the fake's end of its session once it
/// connects, which waits on each read and write as long as a request does ([`hung`]).
fn fake_gateway(socket: &Path) -> (UnixStream, Child) {
    let gateway = UnixListener::bind(socket).unwrap();
    gateway.set_nonblocking(true).unwrap();
    let sandboxd = stdx::process::command(env!("CARGO_BIN_EXE_sandboxd"), PATH_ONLY)
        .arg(socket)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // A `sandboxd` that never connects fails the test rather than hang it.
    let by = hung();
    let connection = loop {
        match gateway.accept() {
            Ok((connection, _)) => break connection,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                assert!(
                    by.remaining().is_some(),
                    "sandboxd never connected: it should connect as it starts"
                );
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("{error}"),
        }
    };
    connection.set_nonblocking(false).unwrap();
    connection.set_read_timeout(hung().remaining()).unwrap();
    connection.set_write_timeout(hung().remaining()).unwrap();
    (connection, sandboxd)
}

/// How `sandboxd`, whose standard error is piped, exited after its session ended, and why, if it
/// said.
fn ended(mut sandboxd: Child) -> String {
    let status = exited(&mut sandboxd);
    let mut stderr = String::new();
    let mut pipe = sandboxd.stderr.take().unwrap();
    pipe.read_to_string(&mut stderr).unwrap();
    match stderr.trim_end() {
        "" => status.to_string(),
        why => format!("{status}: {why}"),
    }
}
