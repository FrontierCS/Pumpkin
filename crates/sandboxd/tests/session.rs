//! `sandboxd` and the gateway's end of its session, together. `sandboxd` runs here as an
//! ordinary process, not as a sandbox's first process, and a temporary directory stands in for
//! the sandbox.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test helper fails the test that calls it"
)]

use std::cell::Cell;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

use expect_test::{Expect, expect};
use pumpkin::gateway::{Listener, Process, Put, Session};
use pumpkin_schema::ir::{AbsolutePath, Argv, Env, EnvVar};
use pumpkin_schema::sandbox::frame::MAX_BODY_BYTES;
use pumpkin_schema::sandbox::tree::Limits;
use pumpkin_schema::sandbox::{Launch, RelativePath};

/// A temporary directory standing in for a sandbox, with `sandboxd` serving its session.
struct Sandbox {
    root: PathBuf,
    session: Session,
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
        let sandboxd = Command::new(env!("CARGO_BIN_EXE_sandboxd"))
            .arg(&socket)
            .spawn()
            .unwrap();
        let session = listener.accept(Duration::from_secs(10)).unwrap();
        let dirs = ["in", "out", "home", "log"].map(relative);
        session
            .put(&absolute(&root), &dirs.each_ref().map(Put::Dir))
            .unwrap();
        Self {
            root,
            session,
            sandboxd,
            commands: Cell::new(0),
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
            stdout: self.at(&format!("log/{n}.stdout")),
            stderr: self.at(&format!("log/{n}.stderr")),
        }
    }

    fn wait(&self, process: &mut Process) -> String {
        let status = self.session.wait(process, Duration::from_secs(10));
        format!("exited: {:?}", status.unwrap().unwrap())
    }

    /// Collects the sandbox's `dir` and lists what arrived.
    fn collect(&self, dir: &str) -> Result<Vec<String>, String> {
        let collected = self.root.join("collected");
        fs::create_dir_all(&collected).unwrap();
        let limits = Limits {
            bytes: 4 << 20,
            entries: 100,
        };
        let into = collected.join(dir.replace('/', "-"));
        let outcome = self.session.collect(&self.at(dir), &into, limits);
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

/// Runs `scenario` against a new sandbox, ends the session, and checks what the scenario
/// reported, how `sandboxd` exited, and how many processes were left running in the sandbox,
/// against `expect`. Those left get 2 s to exit, and are then killed.
fn check(name: &str, scenario: impl FnOnce(&Sandbox) -> Vec<String>, expect: &Expect) {
    let sandbox = Sandbox::start(name);
    let mut lines = scenario(&sandbox);
    let Sandbox {
        root,
        session,
        mut sandboxd,
        ..
    } = sandbox;
    drop(session);
    lines.push(format!("sandboxd: {}", exited(&mut sandboxd)));
    let left = processes(&root, Duration::from_secs(2), <[u32]>::is_empty);
    lines.push(format!("left running: {}", left.len()));
    if !left.is_empty() {
        // `kill` fails for any that exited meanwhile, which is as good as killed.
        let pids = left.iter().map(u32::to_string);
        Command::new("kill")
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
            // Bytes that repeat out of step with the frames, so that a frame lost, repeated or
            // out of order shows.
            let raw: Vec<u8> = (0..2 * MAX_BODY_BYTES + 12345)
                .map(|n| u8::try_from(n % 251).unwrap())
                .collect();
            let root = absolute(&sandbox.root);
            let put = [Put::Dir(&slot), Put::File(&payload, &raw)];
            sandbox.session.put(&root, &put).unwrap();
            let script = r#"
                printf '{"bytes": %s}' "$(wc --bytes < ../in/payload/raw)" > ../out/result.json
                mkdir ../out/evidence && cp ../in/payload/raw ../out/evidence/payload
                mkdir ../out/empty
                echo "home=$HOME mode=$MODE in $(pwd)"
                echo warning >&2
                exit 3"#;
            let launch = sandbox.sh(script, &["HOME=/pumpkin/home", "MODE=train"]);
            let mut judge = sandbox.session.start(launch).unwrap();
            let mut lines = vec![sandbox.wait(&mut judge)];
            sandbox.session.freeze().unwrap();
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
            sandboxd: exit status: 0
            left running: 0"#]],
    );
}

/// Freezing kills every process in each command's group, not only the command's own, returns
/// once the commands have exited, and refuses new commands.
#[test]
fn freezing() {
    check(
        "freezing",
        |sandbox| {
            let launch = sandbox.sh("sleep 60 & sleep 60 & wait", &[]);
            let mut group = sandbox.session.start(launch).unwrap();
            // The shell and both sleeps, so that freezing finds each of them to kill.
            let timeout = Duration::from_secs(10);
            let running = processes(&sandbox.root, timeout, |found| found.len() == 3);
            sandbox.session.freeze().unwrap();
            let exited = sandbox.session.wait(&mut group, Duration::ZERO).unwrap();
            let mut lines = vec![
                format!("running before freezing: {}", running.len()),
                format!("exited when frozen: {exited:?}"),
            ];
            let after = sandbox.session.start(sandbox.sh("true", &[]));
            lines.push(format!("start after freezing: {}", after.unwrap_err()));
            lines
        },
        &expect![[r#"
            running before freezing: 3
            exited when frozen: Some(Signal(9))
            start after freezing: the sandbox is frozen: start commands before freezing it
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
            let started = sandbox.session.start(missing).map(|_| ());
            lines.push(format!("missing program: {}", show_error(sandbox, started)));
            // Started by name with its own PATH, a missing program is waited for by the
            // standard library itself, which must not race the reaper while another command
            // runs.
            let _running = sandbox.session.start(sandbox.sh("exec sleep 60", &[]));
            let mut failures: Vec<String> = (0..1000)
                .map(|_| {
                    let mut by_name = sandbox.sh("", &["PATH=/usr/bin:/bin"]);
                    by_name.argv = Argv::try_from(vec!["no-such-program".to_owned()]).unwrap();
                    show_error(sandbox, sandbox.session.start(by_name).map(|_| ()))
                })
                .collect();
            failures.dedup();
            lines.push(format!("missing program by name, 1000 times: {failures:?}"));
            let file = relative("in/file");
            let twice = [Put::File(&file, b"first"), Put::File(&file, b"second")];
            let put = sandbox.session.put(&absolute(&sandbox.root), &twice);
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
            missing program: `/nonexistent/program` didn't start: No such file or directory (os error 2)
            missing program by name, 1000 times: ["`no-such-program` didn't start: No such file or directory (os error 2)"]
            same file twice: in/file: File exists (os error 17)
            link: $ROOT/out/link is neither a directory nor a regular file: collect only those
            missing: $ROOT/missing: No such file or directory (os error 2)
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
            sandbox.session.start(launch).unwrap();
            let timeout = Duration::from_secs(10);
            let running = processes(&sandbox.root, timeout, |found| !found.is_empty());
            vec![format!("running: {}", running.len())]
        },
        &expect![[r#"
            running: 1
            sandboxd: exit status: 0
            left running: 0"#]],
    );
}
