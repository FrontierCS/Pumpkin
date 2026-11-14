//! `stdx`'s helpers. `process::output` runs stand-ins for a command line, scripts for `sh`: one
//! that ends; two that hang past their deadline, one with its first process waiting on another,
//! one with that process alone holding its output; and one whose deadline has passed before it
//! starts; and `process::fed` one that reads its input, and one that exits without reading it.
//! This process adopts the processes a script starts once the script's own process has ended, so
//! a test learns how each ended. `net::set_options` is read back from a connection's socket.

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]

use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::os::unix::process::ExitStatusExt as _;
use std::path::Path;
use std::process::{ExitStatus, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use expect_test::{Expect, expect};
use rustix::net::sockopt;
use rustix::process::{Pid, Signal, WaitOptions};
use stdx::Deadline;
use stdx::process::Environment;

/// How long a stand-in that hangs first runs before its deadline, which [`check`] doubles until
/// the stand-in has started its process by then: short, since the test waits it out. Each such
/// stand-in sleeps far longer, so its deadline comes first.
const LIMIT: Duration = Duration::from_millis(200);

/// What runs a stand-in.
#[derive(Clone, Copy)]
enum Caller {
    /// [`stdx::process::output`], until this long from the stand-in's start, or twice that, and
    /// so on, until the stand-in has started its process by then.
    Waits(Duration),
    /// [`stdx::process::output`], once the deadline has passed, in a directory that doesn't
    /// exist, where the stand-in can't start.
    Late,
    /// [`stdx::process::fed`], with no deadline, with this many bytes on its standard input.
    Feeds(usize),
}

/// Runs `script` with `sh`, from `caller`, in a directory of its own, and checks how it ended,
/// and how each process whose ID it wrote, a line each, to `pids` there ended, against `expect`.
/// A script writes an ID once it has started that process, so a test that waits for the ID never
/// rests on how slowly this machine starts `sh`.
fn check(script: &str, caller: Caller, expect: &Expect) {
    // Adopted, a process the script left can be waited for.
    rustix::process::set_child_subreaper(Some(rustix::process::getpid())).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let pids = || std::fs::read_to_string(dir.path().join("pids")).unwrap_or_default();
    let ended = match caller {
        Caller::Waits(mut limit) => loop {
            match run(script, dir.path(), limit) {
                Some(output) => break exited(&output),
                // Killed before it wrote what it started, so this run says nothing of what the
                // kill reaches. A process it had started by then was killed with it.
                None if pids().is_empty() => limit = limit.saturating_mul(2),
                None => break "killed by its deadline".to_owned(),
            }
        },
        Caller::Late => match run(script, &dir.path().join("absent"), Duration::ZERO) {
            Some(output) => exited(&output),
            None => "not started, as its deadline had passed".to_owned(),
        },
        Caller::Feeds(bytes) => {
            let mut command = stdx::process::command("sh", Environment::Only(&["PATH"]));
            command.args(["-c", script]).current_dir(dir.path());
            let input = vec![b'x'; bytes];
            let fed = stdx::process::fed(&mut command, &input, Deadline::after(Duration::MAX));
            exited(&fed.unwrap().unwrap())
        }
    };
    let started: Vec<String> = (pids().lines())
        .map(|pid| reaped(pid.parse().unwrap()))
        .collect();
    expect.assert_eq(&format!("{ended}\nthe processes it started: {started:?}"));
}

/// Runs `script` with `sh` in `dir` through [`stdx::process::output`], until `limit` from now.
fn run(script: &str, dir: &Path, limit: Duration) -> Option<Output> {
    let mut command = stdx::process::command("sh", Environment::Only(&["PATH"]));
    command.args(["-c", script]).current_dir(dir);
    stdx::process::output(&mut command, Deadline::after(limit)).unwrap()
}

/// How `output` says a stand-in exited, and how much it wrote.
fn exited(output: &Output) -> String {
    format!(
        "exited with {}, after writing {} bytes to its standard output and {} to its standard \
         error",
        output.status,
        output.stdout.len(),
        output.stderr.len()
    )
}

/// How process `pid`, which [`check`] made this process's child, ended, once it has.
fn reaped(pid: i32) -> String {
    let pid = Pid::from_raw(pid).unwrap();
    let waited = rustix::process::waitpid(Some(pid), WaitOptions::empty()).unwrap();
    let (_, status) = waited.unwrap();
    ExitStatus::from_raw(status.as_raw()).to_string()
}

#[test]
fn ends() {
    check(
        "head --bytes=200000 /dev/zero; head --bytes=300000 /dev/zero >&2; exit 3",
        Caller::Waits(Duration::MAX),
        &expect![[r#"
            exited with exit status: 3, after writing 200000 bytes to its standard output and 300000 to its standard error
            the processes it started: []"#]],
    );
}

#[test]
fn hangs() {
    check(
        "sleep 60 & echo $! >>pids; wait",
        Caller::Waits(LIMIT),
        &expect![[r#"
            killed by its deadline
            the processes it started: ["signal: 9 (SIGKILL)"]"#]],
    );
}

#[test]
fn leaves() {
    check(
        "sleep 60 & echo $! >>pids",
        Caller::Waits(LIMIT),
        &expect![[r#"
            killed by its deadline
            the processes it started: ["signal: 9 (SIGKILL)"]"#]],
    );
}

#[test]
fn late() {
    check(
        "echo $$ >>pids",
        Caller::Late,
        &expect![[r#"
            not started, as its deadline had passed
            the processes it started: []"#]],
    );
}

#[test]
fn first_exited() {
    let spawn = |script: &str| {
        let mut command = stdx::process::command("sh", Environment::Inherited);
        command.args(["-c", script]).stdin(Stdio::piped());
        command.spawn().unwrap()
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
fn fed() {
    check(
        "cat",
        Caller::Feeds(300_000),
        &expect![[r#"
            exited with exit status: 0, after writing 300000 bytes to its standard output and 0 to its standard error
            the processes it started: []"#]],
    );
}

#[test]
fn fed_unread() {
    check(
        "exit 4",
        Caller::Feeds(300_000),
        &expect![[r#"
            exited with exit status: 4, after writing 0 bytes to its standard output and 0 to its standard error
            the processes it started: []"#]],
    );
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
