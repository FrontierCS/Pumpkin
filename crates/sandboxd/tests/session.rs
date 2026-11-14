//! `sandboxd` and the gateway's end of its session, together, meeting at a socket of the
//! sandbox's own ([`Meeting`]). `sandboxd` runs here as an ordinary process, not as a sandbox's first process, and a temporary directory stands in for
//! the sandbox. So `sandboxd` serves no ops, which it serves only at the sandbox's own
//! `/pumpkin`, nor the model endpoint, at a port of the sandbox's own loopback: the Docker
//! provider's tests make ops and model requests, in real sandboxes. A fake gateway sends
//! what the gateway's end never would, or holds up what only a slow link would, having read the
//! hello.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test helper fails the test that calls it"
)]

use std::cell::Cell;
use std::ffi::OsStr;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};
use std::{fs, thread};

use expect_test::{Expect, expect};
use gateway::{Arrival, Process, Put, Session, Sockets};
use schema::ir::{AbsolutePath, Argv, Env, EnvVar, Mode, Name};
use schema::sandbox::frame::MAX_BODY_BYTES;
use schema::sandbox::tree::Limits;
use schema::sandbox::{
    Entry, ExitStatus, FromSandbox, HelloReply, Launch, PROTOCOL_VERSION, RelativePath,
    SOCKET_NAME, SocketDial, ToSandbox, dir_mode, file_mode, frame,
};
use serde_json::{Value, json};
use stdx::process::Environment;
use test_support::hung;

/// What `sandboxd` and the tests' other programs get of the tests' environment: `PATH` alone,
/// to find the programs they run.
const PATH_ONLY: Environment = Environment::Only(&["PATH"]);

/// Where a test's `sandboxd` meets the gateway.
#[derive(Clone, Copy)]
enum Meeting {
    /// At a socket of the sandbox's own.
    Socket,
}

/// What issued a sandbox's meeting, which lasts as long as the sandbox.
#[expect(
    dead_code,
    reason = "the issuers are held for their drop, which ends the meeting"
)]
enum Gateway {
    Sockets(Sockets),
}

/// A temporary directory standing in for a sandbox, with `sandboxd` serving its session.
struct Sandbox {
    root: PathBuf,
    gateway: Gateway,
    session: Session,
    arrivals: Receiver<Arrival>,
    sandboxd: Child,
    commands: Cell<u32>,
}

impl Sandbox {
    /// Starts `sandboxd`, meeting the gateway as `meeting` says, with the shell script
    /// `nvidia_smi`, if given, as the `nvidia-smi` on its `PATH`, and puts the directories a role
    /// expects.
    fn start(meeting: Meeting, nvidia_smi: Option<&str>) -> Self {
        // Kept if the test fails.
        let root = tempfile::Builder::new()
            .prefix("sandboxd-")
            .tempdir()
            .unwrap()
            .keep();
        // Without symbolic links, as the kernel names the working directories that
        // `processes` finds the sandbox's processes by.
        let root = fs::canonicalize(root).unwrap();
        let mut sandboxd = stdx::process::command(env!("CARGO_BIN_EXE_sandboxd"), PATH_ONLY);
        if let Some(script) = nvidia_smi {
            sandboxd.env("PATH", stand_in(&root, script));
        }
        let (gateway, registration) = match meeting {
            Meeting::Socket => {
                let sockets = Sockets::create(root.join("sockets")).unwrap();
                let name = Name::try_from("sandbox".to_owned()).unwrap();
                let (SocketDial { dir }, registration) = sockets.issue(&name).unwrap();
                sandboxd.arg(dir.join(SOCKET_NAME));
                (Gateway::Sockets(sockets), registration)
            }
        };
        let sandboxd = sandboxd.spawn().unwrap();
        let (sender, arrivals) = mpsc::channel();
        let accepted =
            registration.accept(hung(), None, false, sender, |arrival| arrival, || false);
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
            gateway,
            session,
            arrivals,
            sandboxd,
            commands: Cell::new(0),
        }
    }

    /// The path of a socket `name` in the root.
    fn socket(&self, name: &str) -> PathBuf {
        self.root.join(name)
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

/// A `PATH` on which the `nvidia-smi` that runs is the shell script `script`, in `bin` in `root`.
fn stand_in(root: &Path, script: &str) -> String {
    let bin = root.join("bin");
    fs::create_dir(&bin).unwrap();
    fs::write(bin.join("nvidia-smi"), format!("#!/bin/sh\n{script}\n")).unwrap();
    fs::set_permissions(bin.join("nvidia-smi"), fs::Permissions::from_mode(0o755)).unwrap();
    format!("{}:{}", bin.display(), std::env::var("PATH").unwrap())
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

/// Runs `scenario` against a new sandbox, whose `sandboxd` meets the gateway as `meeting` says,
/// ends the session by dropping it, and checks what the scenario reported, what arrived after it,
/// how `sandboxd` exited, and how many processes were left running in the sandbox, against
/// `expect`. Those left get 2 s to exit, and are then killed.
fn check(meeting: Meeting, scenario: impl FnOnce(&mut Sandbox) -> Vec<String>, expect: &Expect) {
    let mut sandbox = Sandbox::start(meeting, None);
    let mut lines = scenario(&mut sandbox);
    lines.extend(sandbox.end());
    expect.assert_eq(&lines.join("\n"));
}

impl Sandbox {
    /// Ends the session by dropping it, and says what arrived after it, how `sandboxd` exited, and
    /// how many processes were left running in the sandbox, which get 2 s to exit, and are then
    /// killed.
    fn end(self) -> Vec<String> {
        let mut lines = Vec::new();
        let Self {
            root,
            gateway,
            session,
            arrivals,
            mut sandboxd,
            commands: _,
        } = self;
        drop(session);
        // The meeting ends with the session it opened.
        drop(gateway);
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
        fs::remove_dir_all(root).unwrap();
        lines
    }
}

#[test]
fn gpus() {
    // A stand-in that names two GPUs, as `nvidia-smi` does, if asked as `sandboxd` asks.
    let named = r#"[ "$*" = "--query-gpu=name --format=csv,noheader" ] || exit 1
printf 'NVIDIA A100-SXM4-80GB\nNVIDIA A100 80GB PCIe\n'"#;
    let long = format!(
        "for i in $(seq 12); do echo 'NVIDIA {}'; done",
        "X".repeat(100)
    );
    let cases = [
        ("named, at a socket", Meeting::Socket, Some(named)),
        ("none on PATH", Meeting::Socket, None),
        (
            "failing",
            Meeting::Socket,
            Some("echo 'No devices were found'; exit 6"),
        ),
        ("naming none", Meeting::Socket, Some("true")),
        (
            "naming more than a hello holds",
            Meeting::Socket,
            Some(&long),
        ),
    ];
    let lines = cases.map(|(case, meeting, nvidia_smi)| {
        let sandbox = Sandbox::start(meeting, nvidia_smi);
        let gpus = format!("{case}: {:?}", sandbox.session.gpus());
        let ended = sandbox.end();
        assert_eq!(ended.last().map(String::as_str), Some("left running: 0"));
        gpus
    });
    let expected = expect![[r#"
        named, at a socket: Some([GpuModel("NVIDIA A100-SXM4-80GB"), GpuModel("NVIDIA A100 80GB PCIe")])
        none on PATH: None
        failing: None
        naming none: None
        naming more than a hello holds: None"#]];
    expected.assert_eq(&lines.join("\n"));
}

#[test]
fn judge() {
    check(
        Meeting::Socket,
        judging,
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

/// [`judge`]'s scenario.
fn judging(sandbox: &mut Sandbox) -> Vec<String> {
    let (slot, payload) = (relative("in/payload"), relative("in/payload/raw"));
    // Over two frames, and kept as evidence, so that it is put and collected in several.
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
}

#[test]
fn modes_and_stdin() {
    check(
        Meeting::Socket,
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

#[test]
fn freezing() {
    check(
        Meeting::Socket,
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

#[test]
fn killing() {
    check(
        Meeting::Socket,
        |sandbox| {
            let launch = sandbox.sh("sleep 60 & sleep 60 & wait", &[]);
            let group = sandbox.session.start(launch, hung()).unwrap();
            let timeout = Duration::from_secs(10);
            let running = processes(&sandbox.root, timeout, |found| found.len() == 3);
            sandbox.session.kill(group, hung()).unwrap();
            let exited = sandbox.exit(group, timeout);
            let left = processes(&sandbox.root, timeout, <[u32]>::is_empty);
            let next = sandbox.session.start(sandbox.sh("true", &[]), hung());
            let next = next.unwrap();
            let next_exited = sandbox.exit(next, timeout);
            let again = sandbox.session.kill(group, hung());
            vec![
                format!("running before the kill: {}", running.len()),
                format!("exited when killed: {exited:?}"),
                format!("left after it: {}", left.len()),
                format!("the next command exited: {next_exited:?}"),
                format!("killing it again: {again:?}"),
            ]
        },
        &expect![[r#"
            running before the kill: 3
            exited when killed: Some(Signal(9))
            left after it: 0
            the next command exited: Some(Code(0))
            killing it again: Ok(())
            then Ended { reason: "the owner dropped the session" }
            sandboxd: exit status: 0
            left running: 0"#]],
    );
}

#[test]
fn full_sandbox() {
    check(
        Meeting::Socket,
        |sandbox| {
            let launch = sandbox.sh("exec sleep 60", &[]);
            let sleep = sandbox.session.start(launch, hung()).unwrap();
            fs::write(sandbox.root.join("out/kept"), "kept").unwrap();
            let pid = sandbox.sandboxd.id().to_string();
            // `sandboxd`'s own limit on its user's processes stands in for the sandbox's: either
            // fails a new process or thread alike.
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

#[test]
fn refusals() {
    check(
        Meeting::Socket,
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

#[test]
fn ending_the_session() {
    check(
        Meeting::Socket,
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

#[test]
fn model_answers() {
    check(
        Meeting::Socket,
        |sandbox| {
            // A fake gateway, as the gateway's end never answers a request it didn't take.
            let socket = sandbox.socket("fake.sock");
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
    let (mut connection, sandboxd) = accepting_gateway(socket);
    frame::write(&mut connection, message, &[]).unwrap();
    ended(sandboxd)
}

#[test]
fn hello_answers() {
    check(
        Meeting::Socket,
        |sandbox| {
            let message = format!("a stand-in for sandbox protocol {}", PROTOCOL_VERSION + 1);
            let refusal = json!({ "type": "refused", "message": message, "later": true });
            let (mut gateway, sandboxd, hello) = fake_gateway(&sandbox.socket("refused.sock"));
            frame::write(&mut gateway, &refusal, &[]).unwrap();
            let refused = sandbox.show(&ended(sandboxd));
            let (gateway, sandboxd, _) = fake_gateway(&sandbox.socket("unanswered.sock"));
            drop(gateway);
            let unanswered = sandbox.show(&ended(sandboxd));
            vec![
                format!("its hello: {hello}; refused: {refused}"),
                format!("unanswered: {unanswered}"),
            ]
        },
        &expect![[r#"
            its hello: {"version":0}; refused: exit status: 1: sandboxd: opening the session with the gateway at $ROOT/refused.sock failed: it refused the session: a stand-in for sandbox protocol 1
            unanswered: exit status: 1: sandboxd: opening the session with the gateway at $ROOT/unanswered.sock failed: it closed the connection before it answered the hello: expected it to accept or refuse the session
            then Ended { reason: "the owner dropped the session" }
            sandboxd: exit status: 0
            left running: 0"#]],
    );
}

#[test]
fn reading_while_collecting() {
    check(
        Meeting::Socket,
        |sandbox| {
            // Far more than a connection buffers, so that a `sandboxd` that read nothing while it
            // collected would hold the put's write up until its deadline.
            let tree = pattern(4 * MAX_BODY_BYTES);
            fs::write(sandbox.root.join("out/tree"), &tree).unwrap();
            let (mut gateway, sandboxd) = accepting_gateway(&sandbox.socket("fake.sock"));
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

/// [`fake_gateway`], which accepts the hello.
fn accepting_gateway(socket: &Path) -> (UnixStream, Child) {
    let (mut connection, sandboxd, _) = fake_gateway(socket);
    frame::write(&mut connection, &HelloReply::Accepted, &[]).unwrap();
    (connection, sandboxd)
}

/// A `sandboxd` whose gateway is a fake at `socket`, the fake's end of its session once it
/// connects, which waits on each read and write as long as a request does ([`hung`]), and the
/// hello it read there.
fn fake_gateway(socket: &Path) -> (UnixStream, Child, Value) {
    let gateway = UnixListener::bind(socket).unwrap();
    gateway.set_nonblocking(true).unwrap();
    let sandboxd = sandboxd(&[socket.as_ref()]);
    // A `sandboxd` that never connects fails the test rather than hang it.
    let by = hung();
    let mut connection = loop {
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
    let (hello, _) = frame::read::<Value>(&mut connection).unwrap().unwrap();
    (connection, sandboxd, hello)
}

/// A `sandboxd` run with `args`, with its standard error piped.
fn sandboxd(args: &[&OsStr]) -> Child {
    let mut sandboxd = stdx::process::command(env!("CARGO_BIN_EXE_sandboxd"), PATH_ONLY);
    sandboxd.args(args).stderr(Stdio::piped());
    sandboxd.spawn().unwrap()
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
