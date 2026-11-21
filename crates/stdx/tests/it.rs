#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]

use std::ffi::OsStr;
use std::fs::{self, File};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::num::NonZeroUsize;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::Path;
use std::process::{ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};
use std::{env, thread};

use expect_test::{Expect, expect};
use rustix::net::sockopt;
use rustix::process::{Pid, Signal, WaitOptions};
use stdx::Deadline;
use stdx::process::{Bound, Echo, Environment, Failure, Run, Stream};

/// How long a stand-in that hangs first runs before its bound, which [`check`] doubles until the
/// stand-in has started its process by then.
// Short, since the test waits it out. Each such stand-in sleeps far longer, so its bound comes
// first.
const LIMIT: Duration = Duration::from_millis(200);

// Where the bound isn't what a test is about: one never reached.
const UNBOUNDED: Bound = Bound::Within(Duration::MAX);

#[derive(Clone, Copy)]
enum Caller<'a> {
    /// Silent, within this long of the stand-in's start, or twice that, and so on, until the
    /// stand-in has started its process by then.
    Waits(Duration),
    /// Echoed, by a deadline that has passed, in a directory that doesn't exist, where the
    /// stand-in can't start.
    Late,
    /// As this says, once.
    Runs(Run<'a>),
}

/// Runs `script` with `sh`, from `caller`, in a directory of its own, and checks how it ended,
/// what this process printed meanwhile, and how each process whose ID it wrote, a line each, to
/// `pids` there ended, against `expect`. A script writes an ID once it has started that process,
/// so a test that waits for the ID never rests on how slowly this machine starts `sh`.
fn check(script: &str, caller: Caller<'_>, expect: &Expect) {
    // Adopted, a process the script left can be waited for.
    rustix::process::set_child_subreaper(Some(rustix::process::getpid())).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let pids = || fs::read_to_string(dir.path().join("pids")).unwrap_or_default();
    let printing = Printing::start();
    let ended = match caller {
        Caller::Waits(mut limit) => loop {
            let how = Run::new(Bound::Within(limit), Echo::Silent);
            match run(script, dir.path(), how) {
                // Killed before it wrote what it started, so this run says nothing of what the
                // kill reaches. A process it had started by then was killed with it.
                Err(Failure::Unended { .. }) if pids().is_empty() => {
                    limit = limit.saturating_mul(2);
                }
                Err(Failure::Unended { .. }) => break "killed at its bound".to_owned(),
                ran => break ended(&ran),
            }
        },
        Caller::Late => {
            let how = Run::new(Bound::By(Deadline::after(Duration::ZERO)), Echo::Line);
            ended(&run(script, &dir.path().join("absent"), how))
        }
        Caller::Runs(how) => ended(&run(script, dir.path(), how)),
    };
    let printed = printing.stop();
    let started: Vec<String> = (pids().lines())
        .map(|pid| reaped(pid.parse().unwrap()))
        .collect();
    expect.assert_eq(&format!(
        "{ended}\n{printed}the processes it started: {started:?}"
    ));
}

fn run(script: &str, dir: &Path, how: Run<'_>) -> Result<Output, Failure> {
    let mut command = stdx::process::command("sh", Environment::Only(&["PATH"]));
    command.args(["-c", script]).current_dir(dir);
    stdx::process::run(&mut command, how)
}

fn ended(ran: &Result<Output, Failure>) -> String {
    match ran {
        Ok(output) => written(output),
        Err(failure @ Failure::Exited { output, .. }) => format!("{failure}\n{}", written(output)),
        Err(failure) => failure.to_string(),
    }
}

fn written(output: &Output) -> String {
    format!(
        "exited with {}, with {} bytes of standard output and {} of standard error",
        output.status,
        output.stdout.len(),
        output.stderr.len()
    )
}

/// This process's standard output and error, sent to files of their own until [`Printing::stop`].
struct Printing {
    dir: tempfile::TempDir,
    stdout: std::os::fd::OwnedFd,
    stderr: std::os::fd::OwnedFd,
}

impl Printing {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let stdout = rustix::io::dup(rustix::stdio::stdout()).unwrap();
        let stderr = rustix::io::dup(rustix::stdio::stderr()).unwrap();
        rustix::stdio::dup2_stdout(File::create(dir.path().join("stdout")).unwrap()).unwrap();
        rustix::stdio::dup2_stderr(File::create(dir.path().join("stderr")).unwrap()).unwrap();
        Self {
            dir,
            stdout,
            stderr,
        }
    }

    /// Gives this process's standard output and error back, and shows what each took meanwhile.
    fn stop(self) -> String {
        rustix::stdio::dup2_stdout(&self.stdout).unwrap();
        rustix::stdio::dup2_stderr(&self.stderr).unwrap();
        let shown = ["stdout", "stderr"].map(|stream| {
            let printed = fs::read_to_string(self.dir.path().join(stream)).unwrap();
            if printed.is_empty() {
                return String::new();
            }
            format!("this process's {stream}: {printed:?}\n")
        });
        shown.concat()
    }
}

/// How process `pid`, which [`check`] made this process's child, ended, once it has.
fn reaped(pid: i32) -> String {
    let pid = Pid::from_raw(pid).unwrap();
    let waited = rustix::process::waitpid(Some(pid), WaitOptions::empty()).unwrap();
    let (_, status) = waited.unwrap();
    ExitStatus::from_raw(status.as_raw()).to_string()
}

#[test]
fn captures() {
    check(
        "head --bytes=200000 /dev/zero; head --bytes=300000 /dev/zero >&2",
        Caller::Runs(Run::new(UNBOUNDED, Echo::Silent)),
        &expect![[r#"
            exited with exit status: 0, with 200000 bytes of standard output and 300000 of standard error
            the processes it started: []"#]],
    );
}

#[test]
fn hangs() {
    check(
        "sleep 60 & echo $! >>pids; wait",
        Caller::Waits(LIMIT),
        &expect![[r#"
            killed at its bound
            the processes it started: ["signal: 9 (SIGKILL)"]"#]],
    );
}

#[test]
fn leaves() {
    check(
        "sleep 60 & echo $! >>pids",
        Caller::Waits(LIMIT),
        &expect![[r#"
            killed at its bound
            the processes it started: ["signal: 9 (SIGKILL)"]"#]],
    );
}

#[test]
fn unended() {
    let how = Run::new(Bound::Within(Duration::from_millis(1)), Echo::Silent);
    check(
        "exec sleep 60",
        Caller::Runs(how),
        &expect![[r#"
        `sh` didn't end within 0.001 s, so it was killed
        the processes it started: []"#]],
    );
}

#[test]
fn late() {
    check(
        "echo $$ >>pids",
        Caller::Late,
        &expect![[r#"
        the deadline passed before `sh` ended
        the processes it started: []"#]],
    );
}

// One byte past the 128 MiB that a run captures, with a process started first, which the kill
// reaches.
#[test]
fn capped() {
    check(
        "sleep 60 & echo $! >>pids; head --bytes=134217729 /dev/zero",
        Caller::Runs(Run::new(UNBOUNDED, Echo::Silent)),
        &expect![[r#"
            running `sh` failed: its standard output passed the 128 MiB that a run captures, so it was killed: expected less
            the processes it started: ["signal: 9 (SIGKILL)"]"#]],
    );
}

#[test]
fn fed() {
    let input = vec![b'x'; 300_000];
    let how = Run::new(UNBOUNDED, Echo::Silent).input(&input);
    check(
        "cat",
        Caller::Runs(how),
        &expect![[r#"
        exited with exit status: 0, with 300000 bytes of standard output and 0 of standard error
        the processes it started: []"#]],
    );
}

#[test]
fn fed_unread() {
    let input = vec![b'x'; 300_000];
    let how = Run::new(UNBOUNDED, Echo::Silent).input(&input);
    check(
        "exit 4",
        Caller::Runs(how),
        &expect![[r#"
        `sh` failed with exit status: 4 and wrote nothing to standard error
        exited with exit status: 4, with 0 bytes of standard output and 0 of standard error
        the processes it started: []"#]],
    );
}

#[test]
fn exited() {
    let how = Run::new(UNBOUNDED, Echo::Silent).named("the script");
    check(
        "echo some words; printf '  it broke.\\n\\n' >&2; exit 3",
        Caller::Runs(how),
        &expect![[r#"
            `the script` failed with exit status: 3: it broke: fix what it reports
            exited with exit status: 3, with 11 bytes of standard output and 13 of standard error
            the processes it started: []"#]],
    );
}

#[test]
fn exited_silently() {
    check(
        "exit 3",
        Caller::Runs(Run::new(UNBOUNDED, Echo::Silent)),
        &expect![[r#"
            `sh` failed with exit status: 3 and wrote nothing to standard error
            exited with exit status: 3, with 0 bytes of standard output and 0 of standard error
            the processes it started: []"#]],
    );
}

#[test]
fn echoed_inherited() {
    let how = Run::new(UNBOUNDED, Echo::Line)
        .stdout(Stream::Inherited)
        .stderr(Stream::Inherited);
    check(
        "echo out; echo err >&2; exit 1",
        Caller::Runs(how),
        &expect![[r#"
            `sh` failed with exit status: 1: fix what it printed above
            exited with exit status: 1, with 0 bytes of standard output and 0 of standard error
            this process's stdout: "out\n"
            this process's stderr: "$ sh -c 'echo out; echo err >&2; exit 1'\nerr\n"
            the processes it started: []"#]],
    );
}

#[test]
fn discarded() {
    let how = Run::new(UNBOUNDED, Echo::Silent)
        .stdout(Stream::Discarded)
        .stderr(Stream::Discarded);
    check(
        "echo out; echo err >&2; exit 1",
        Caller::Runs(how),
        &expect![[r#"
            `sh` failed with exit status: 1
            exited with exit status: 1, with 0 bytes of standard output and 0 of standard error
            the processes it started: []"#]],
    );
}

#[test]
fn cargo_environment() {
    // Cargo sets them for the test it runs.
    assert!(env::var_os("CARGO_PKG_NAME").is_some() && env::var_os("PATH").is_some());
    let mut command = stdx::process::command("sh", Environment::Cargo);
    let script = "echo ${CARGO_PKG_NAME-unset} ${CARGO_MANIFEST_DIR-unset} ${PATH+set}";
    command.args(["-c", script]);
    let ran = stdx::process::run(&mut command, Run::new(UNBOUNDED, Echo::Silent));
    expect!["unset unset set"].assert_eq(&stdx::process::printed(ran.unwrap()).unwrap());
}

// Its last 4096 bytes start within a character of two bytes, and it ends with a period.
#[test]
fn reported() {
    let mut input = "ü".repeat(2049).into_bytes();
    input.extend(b"x.\n");
    let mut command = stdx::process::command("sh", Environment::Only(&["PATH"]));
    command.args(["-c", "cat >&2; exit 1"]);
    let how = Run::new(UNBOUNDED, Echo::Silent).input(&input);
    let failure = stdx::process::run(&mut command, how).unwrap_err();
    let reported = failure.reported();
    let chars: Vec<char> = reported.chars().collect();
    let shown = format!(
        "{} characters, {} of them ü, ending in {:?}",
        chars.len(),
        chars.iter().filter(|&&c| c == 'ü').count(),
        chars.last().unwrap()
    );
    expect!["2047 characters, 2046 of them ü, ending in 'x'"].assert_eq(&shown);
}

#[test]
fn unstarted() {
    let dir = tempfile::tempdir().unwrap();
    let only = Environment::Only(&["PATH"]);
    let run = |command: &mut std::process::Command| {
        let failure = stdx::process::run(command, Run::new(UNBOUNDED, Echo::Silent));
        failure.unwrap_err().to_string()
    };
    let mut absent = stdx::process::command("pumpkin-absent", only);
    absent.args(["nextest", "run", "--workspace"]);
    let mut at = stdx::process::command(dir.path().join("absent"), only);
    let mut lost = stdx::process::command("sh", only);
    lost.args(["-c", "true"])
        .current_dir(dir.path().join("absent"));
    let mut spawned = stdx::process::command("pumpkin-absent", only);
    let spawned = stdx::process::spawn(&mut spawned, Stdio::null(), Stdio::null(), Stdio::null());
    let shown = [
        run(&mut absent),
        run(&mut at),
        run(&mut lost),
        spawned.unwrap_err(),
    ];
    let shown = shown
        .join("\n")
        .replace(dir.path().to_str().unwrap(), "$DIR");
    expect![[r#"
        `pumpkin-absent nextest run` didn't start: No such file or directory (os error 2): install `pumpkin-absent` on `PATH`
        `$DIR/absent` didn't start: No such file or directory (os error 2): expected a program at `$DIR/absent`
        `sh` didn't start in `$DIR/absent`: No such file or directory (os error 2): expected that directory to exist
        `pumpkin-absent` didn't start: No such file or directory (os error 2): install `pumpkin-absent` on `PATH`"#]]
    .assert_eq(&shown);
}

#[test]
fn shown() {
    let mut command = stdx::process::command("git", Environment::Only(&[]));
    let words: [&OsStr; 9] = [
        "-C".as_ref(),
        "a/b-c.d".as_ref(),
        "".as_ref(),
        "two words".as_ref(),
        "it's".as_ref(),
        "$HOME".as_ref(),
        "key=value,x@y:z%+".as_ref(),
        "ü".as_ref(),
        OsStr::from_bytes(b"\xff"),
    ];
    command.args(words).env("SECRET", "kept");
    expect![[r#"git -C a/b-c.d '' 'two words' 'it'"'"'s' '$HOME' key=value,x@y:z%+ 'ü' '�'"#]]
        .assert_eq(&stdx::process::shown(&command));
}

#[test]
fn printed() {
    let output = |stdout: &[u8]| Output {
        status: ExitStatus::from_raw(0),
        stdout: stdout.to_vec(),
        stderr: Vec::new(),
    };
    let shown = [
        stdx::process::printed(output(b"two\nlines\n\n")),
        stdx::process::printed(output(b"none")),
        stdx::process::printed(output(b"not \xff")),
    ];
    let shown = format!("{shown:?}");
    expect![[r#"[Ok("two\nlines\n"), Ok("none"), Err("the standard output isn't UTF-8: invalid utf-8 sequence of 1 bytes from index 4: expected text")]"#]]
    .assert_eq(&shown);
}

#[test]
fn first_exited() {
    let spawn = |script: &str| {
        let mut command = stdx::process::command("sh", Environment::Inherited);
        command.args(["-c", script]);
        let (stdout, stderr) = (Stdio::inherit(), Stdio::inherit());
        stdx::process::spawn(&mut command, Stdio::piped(), stdout, stderr).unwrap()
    };
    // `cat` exits only once its input closes.
    let (mut waiting, mut exiting) = (spawn("cat >/dev/null; exit 4"), spawn("exit 3"));
    let mut shown = Vec::new();
    let (index, status) = stdx::process::wait_any(&mut [&mut waiting, &mut exiting]);
    shown.push(format!("child {index}: {}", status.unwrap()));
    drop(waiting.stdin.take());
    let (index, status) = stdx::process::wait_any(&mut [&mut waiting]);
    shown.push(format!("child {index}: {}", status.unwrap()));
    expect![[r#"
        child 1: exit status: 3
        child 0: exit status: 4"#]]
    .assert_eq(&shown.join("\n"));
}

#[test]
fn caught() {
    stdx::signal::catch(&[Signal::INT, Signal::TERM]).unwrap();
    let mut shown = vec![format!("before: {:?}", stdx::signal::caught())];
    rustix::process::kill_process(rustix::process::getpid(), Signal::TERM).unwrap();
    // The signal may land on another of this process's threads, after `kill` returns.
    let by = Instant::now() + Duration::from_secs(60);
    while stdx::signal::caught().is_none() && Instant::now() < by {
        thread::yield_now();
    }
    shown.push(format!("after SIGTERM: {:?}", stdx::signal::caught()));
    shown.push(format!(
        "SIGKILL: {}",
        stdx::signal::catch(&[Signal::KILL]).unwrap_err()
    ));
    expect![[r#"
        before: None
        after SIGTERM: Some("Signal::TERM")
        SIGKILL: Invalid argument (os error 22)"#]]
    .assert_eq(&shown.join("\n"));
}

#[test]
fn tcp_options() {
    // What the gateway and `sandboxd` each set at their end of a session over TCP.
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    stdx::net::set_options(&stream).unwrap();
    let read = [
        format!("nodelay {}", sockopt::tcp_nodelay(&stream).unwrap()),
        format!("keepalive {}", sockopt::socket_keepalive(&stream).unwrap()),
        format!("idle {:?}", sockopt::tcp_keepidle(&stream).unwrap()),
        format!("interval {:?}", sockopt::tcp_keepintvl(&stream).unwrap()),
        format!("probes {}", sockopt::tcp_keepcnt(&stream).unwrap()),
    ];
    expect!["nodelay true, keepalive true, idle 15s, interval 15s, probes 9"]
        .assert_eq(&read.join(", "));
}

// With each of 2 slots held, the second by a holder yet to name itself, a take waits for the
// first let go; with each held again, a take whose `waiting` fails fails with its error.
#[test]
fn slots() {
    let dir = tempfile::tempdir().unwrap();
    let count = NonZeroUsize::new(2).unwrap();
    let free = |_: &[String]| Err("waited, though a slot was free".to_owned());
    let first = stdx::slots::take(dir.path(), count, "first", free).unwrap();
    let unnamed = File::create(dir.path().join("1")).unwrap();
    unnamed.lock().unwrap();
    let mut heard = Vec::new();
    let _late = stdx::slots::take(dir.path(), count, "late", |holders| {
        heard = holders.to_vec();
        drop(first);
        Ok(())
    })
    .unwrap();
    let held = |_: &[String]| Err("given up".to_owned());
    let refused = stdx::slots::take(dir.path(), count, "refused", held).map(drop);
    let mut slots: Vec<String> = (fs::read_dir(dir.path()).unwrap())
        .map(|entry| {
            let name = entry.unwrap().file_name().into_string().unwrap();
            let holder = fs::read_to_string(dir.path().join(&name)).unwrap();
            format!("{name}: {holder:?}")
        })
        .collect();
    slots.sort();
    let shown = [
        format!("the late one, waiting, heard: {heard:?}"),
        format!("the refused one: {refused:?}"),
        format!("the slots, by their holders: {}", slots.join(", ")),
    ];
    expect![[r#"
        the late one, waiting, heard: ["first", "1"]
        the refused one: Err("given up")
        the slots, by their holders: 0: "late", 1: """#]]
    .assert_eq(&shown.join("\n"));
}
