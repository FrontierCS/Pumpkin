//! `sandboxd` and the gateway's end of its session, together. `sandboxd` runs here as an
//! ordinary process, not as a sandbox's first process, and a temporary directory stands in for
//! the sandbox.

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]

use std::cell::Cell;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread;
use std::time::Duration;

use expect_test::{Expect, expect};
use pumpkin::gateway::{Listener, Process, Put, Session};
use pumpkin_schema::ir::{AbsolutePath, Argv, Env, EnvVar};
use pumpkin_schema::sandbox::tree::Limits;
use pumpkin_schema::sandbox::{Launch, RelativePath, Signal};

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
        let root = std::env::temp_dir().join(format!("pumpkin-sandboxd-{name}"));
        if root.exists() {
            fs::remove_dir_all(&root).unwrap();
        }
        fs::create_dir_all(&root).unwrap();
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
            bytes: 1 << 20,
            entries: 100,
        };
        let into = collected.join(dir.replace('/', "-"));
        let outcome = self.session.collect(&self.at(dir), &into, limits);
        outcome.map_err(|error| self.show(&error))?;
        let mut lines = Vec::new();
        self.list(&collected, &into, &mut lines);
        Ok(lines)
    }

    /// Each file below `dir` with its contents, and each empty directory.
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
                let contents = fs::read_to_string(&path).unwrap();
                let name = path.strip_prefix(base).unwrap().display();
                lines.push(format!("{name}: {}", self.show(&contents.trim_end())));
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

/// Whether process `pid` is running, neither gone nor a zombie.
fn running(pid: u32) -> bool {
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let state = stat.rsplit_once(") ").map(|(_, rest)| rest.chars().next());
    state != Some(Some('Z'))
}

/// Runs `scenario` against a new sandbox, ends the session, and checks what the scenario
/// reported, and how `sandboxd` exited, against `expect`.
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
    lines.push(format!("sandboxd: {}", sandboxd.wait().unwrap()));
    expect.assert_eq(&lines.join("\n"));
    fs::remove_dir_all(root).unwrap();
}

/// What a judge does: reads its payload, writes a result and evidence, and exits.
#[test]
fn judge() {
    check(
        "judge",
        |sandbox| {
            let (slot, payload) = (relative("in/payload"), relative("in/payload/raw"));
            let root = absolute(&sandbox.root);
            let put = [Put::Dir(&slot), Put::File(&payload, b"ledger")];
            sandbox.session.put(&root, &put).unwrap();
            let script = r#"
                printf '{"bytes": %s}' "$(wc --bytes < ../in/payload/raw)" > ../out/result.json
                mkdir ../out/evidence && echo kept > ../out/evidence/evidence.txt
                mkdir ../out/empty
                echo "home=$HOME mode=$MODE in $(pwd)"
                echo warning >&2
                exit 3"#;
            let launch = sandbox.sh(script, &["HOME=/pumpkin/home", "MODE=train"]);
            let mut judge = sandbox.session.start(launch).unwrap();
            let mut lines = vec![sandbox.wait(&mut judge)];
            sandbox.session.freeze().unwrap();
            lines.extend(sandbox.collect("out").unwrap());
            lines.extend(sandbox.collect("log").unwrap());
            lines
        },
        &expect![[r#"
            exited: Code(3)
            out/empty/
            out/evidence/evidence.txt: kept
            out/result.json: {"bytes": 6}
            log/1.stderr: warning
            log/1.stdout: home=/pumpkin/home mode=train in $ROOT/home
            sandboxd: exit status: 0"#]],
    );
}

/// Signals reach a command's process group, and freezing kills every group and refuses new
/// commands.
#[test]
fn signals_and_freezing() {
    check(
        "signals",
        |sandbox| {
            let launch = sandbox.sh("exec sleep 1000", &[]);
            let mut sleeper = sandbox.session.start(launch).unwrap();
            sandbox.session.signal(&sleeper, Signal::Terminate).unwrap();
            let mut lines = vec![sandbox.wait(&mut sleeper)];
            let again = sandbox.session.signal(&sleeper, Signal::Kill);
            lines.push(format!("signal again: {}", again.unwrap_err()));
            let launch = sandbox.sh("sleep 1000 & sleep 1000 & wait", &[]);
            let mut group = sandbox.session.start(launch).unwrap();
            sandbox.session.freeze().unwrap();
            lines.push(sandbox.wait(&mut group));
            let after = sandbox.session.start(sandbox.sh("true", &[]));
            lines.push(format!("start after freezing: {}", after.unwrap_err()));
            lines
        },
        &expect![[r#"
            exited: Signal(15)
            signal again: no command started on stream 2 is running: signal a running one
            exited: Signal(9)
            start after freezing: the sandbox is frozen: start commands before freezing it
            sandboxd: exit status: 0"#]],
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
            let _running = sandbox.session.start(sandbox.sh("exec sleep 1000", &[]));
            let mut failures: Vec<String> = (0..200)
                .map(|_| {
                    let mut by_name = sandbox.sh("", &["PATH=/usr/bin:/bin"]);
                    by_name.argv = Argv::try_from(vec!["no-such-program".to_owned()]).unwrap();
                    show_error(sandbox, sandbox.session.start(by_name).map(|_| ()))
                })
                .collect();
            failures.dedup();
            lines.push(format!("missing program by name, 200 times: {failures:?}"));
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
            missing program by name, 200 times: ["`no-such-program` didn't start: No such file or directory (os error 2)"]
            same file twice: in/file: File exists (os error 17)
            link: $ROOT/out/link is neither a directory nor a regular file: collect only those
            missing: $ROOT/missing: No such file or directory (os error 2)
            sandboxd: exit status: 0"#]],
    );
}

fn show_error(sandbox: &Sandbox, outcome: io::Result<()>) -> String {
    sandbox.show(&outcome.unwrap_err())
}

/// Ending the session kills the commands still running.
#[test]
fn ending_the_session() {
    let mut pid = 0;
    check(
        "ending",
        |sandbox| {
            let launch = sandbox.sh("echo $$ > ../pid; exec sleep 1000", &[]);
            sandbox.session.start(launch).unwrap();
            let pid_file = sandbox.root.join("pid");
            while !fs::read_to_string(&pid_file).is_ok_and(|text| text.ends_with('\n')) {
                thread::sleep(Duration::from_millis(10));
            }
            pid = fs::read_to_string(&pid_file)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            vec![format!("running: {}", running(pid))]
        },
        &expect![[r#"
            running: true
            sandboxd: exit status: 0"#]],
    );
    for _ in 0..500 {
        if !running(pid) {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("the command outlived its session");
}
