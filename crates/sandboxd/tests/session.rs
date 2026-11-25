//! `sandboxd` and the gateway's end of its session, together, meeting at a socket of the
//! sandbox's own, at a TCP address on the loopback, or where `sandboxd` listens on the loopback,
//! behind the stand-in TLS edge. `sandboxd` runs here as an ordinary process, not as a
//! sandbox's first process, and a temporary directory stands in for the sandbox. So `sandboxd`
//! serves no ops, which it serves only at the sandbox's own `/pumpkin`, nor the model endpoint,
//! at a port of the sandbox's own loopback. A fake gateway sends what the gateway's end never
//! would, or holds up what only a slow link would, having read the hello.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test helper fails the test that calls it"
)]
#![expect(clippy::disallowed_methods, reason = "nextest bounds each test")]

use std::cell::Cell;
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write as _};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

use expect_test::{Expect, expect};
use gateway::{
    Arrival, Dialer, GatewayAddress, Listener, Place, Pod, Process, Put, Registration, Sockets,
};
use schema::frame;
use schema::frame::MAX_BODY_BYTES;
use schema::ir::{AbsolutePath, Argv, CompanionName, Env, EnvVar, LinkPort, Member, Mode, Name};
use schema::sandbox::tree::Limits;
use schema::sandbox::{
    Endpoint, Entry, ExitStatus, FromSandbox, HELLO_TIMEOUT, HelloReply, Launch, Listening,
    MAX_ATTESTATION_BYTES, PROTOCOL_VERSION, PrivateDial, RegistrationGrant, RelativePath, Relayed,
    Root, SANDBOXD_ATTESTATION, SANDBOXD_GRANT, SANDBOXD_TLS_CERTIFICATE, SANDBOXD_TLS_KEY,
    SOCKET_NAME, SocketDial, ToSandbox, dir_mode, file_mode,
};
use serde_json::{Value, json};
use stdx::Deadline;
use stdx::process::Environment;
use stdx::time::Machine;
use test_support::{HUNG, TestDir, hung};

type Session = gateway::Session<Machine>;

const PATH_ONLY: Environment = Environment::Only(&["PATH"]);

// The programs that `sandboxd` and the tests' commands run, on a `PATH` of their own that lacks
// `nvidia-smi`, so that the hello names no GPU and no start waits on a probe for them.
const TOOLS: [&str; 8] = ["sh", "env", "sleep", "stat", "chmod", "mkdir", "wc", "cp"];

// A registration grant that no gateway of the tests issues.
const GRANT: &str = "00112233445566778899aabbccddeeff";

// The attestation of the sandbox that dials over TCP, which its registration's check admits.
const ATTESTATION: &str = "the sandbox's attestation";

#[derive(Clone, Copy)]
enum Meeting {
    // `sandboxd` gets `GRANT` and `ATTESTATION` in its environment all the same, which a command
    // shows it doesn't get.
    Socket,
    Tcp,
    Listen,
    /// Listening on 127.0.0.2, relaying at the address given.
    Relaying(SocketAddr),
    /// A companion, `localhost`, behind the relay at the address given, listening on 127.0.0.1 at
    /// the port given, its sandbox's.
    Relayed(SocketAddr, u16),
}

#[expect(
    dead_code,
    reason = "the issuers are held for their drop, which ends the meeting"
)]
enum Gateway {
    Sockets(Sockets),
    Tcp(Listener, SocketAddr),
    Listen(SocketAddrV4),
}

struct Sandbox {
    root: PathBuf,
    path: OsString,
    gateway: Gateway,
    session: Session,
    arrivals: Receiver<Arrival>,
    sandboxd: Child,
    commands: Cell<u32>,
    dir: TestDir,
}

impl Sandbox {
    fn start(meeting: Meeting) -> Self {
        let sandboxd = stdx::process::command(env!("CARGO_BIN_EXE_sandboxd"), PATH_ONLY);
        Self::start_with(meeting, sandboxd)
    }

    fn start_with(meeting: Meeting, sandboxd: Command) -> Self {
        Self::start_placed(meeting, sandboxd, None)
    }

    fn start_placed(meeting: Meeting, mut sandboxd: Command, place: Option<Place>) -> Self {
        let dir = TestDir::new();
        // Without symbolic links, as the kernel names the working directories that `processes`
        // finds the sandbox's processes by.
        let root = fs::canonicalize(dir.path()).unwrap();
        let path = tools(&root);
        sandboxd.env("PATH", &path);
        let (gateway, registration) = match meeting {
            Meeting::Socket => {
                let sockets = Sockets::create(root.join("sockets")).unwrap();
                let name = Name::try_from("sandbox".to_owned()).unwrap();
                let (SocketDial { dir }, registration) = sockets.issue(&name).unwrap();
                sandboxd
                    .arg(dir.join(SOCKET_NAME))
                    .env(SANDBOXD_GRANT, GRANT)
                    .env(SANDBOXD_ATTESTATION, ATTESTATION);
                (Gateway::Sockets(sockets), registration)
            }
            Meeting::Tcp => {
                let loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
                let address = GatewayAddress::try_from(loopback).unwrap();
                let listener = Listener::bind(&address, HELLO_TIMEOUT).unwrap();
                let (PrivateDial { gateway }, unattested) = listener.issue(Deadline::after(HUNG));
                let registration = unattested.admitting(|attestation, peer| {
                    let ours = attestation.bytes() == ATTESTATION.as_bytes() && peer.is_loopback();
                    ours.then_some(())
                        .ok_or_else(|| "it isn't the sandbox's".to_owned())
                });
                sandboxd
                    .arg(gateway.to_string())
                    .env(SANDBOXD_ATTESTATION, ATTESTATION);
                (Gateway::Tcp(listener, gateway), registration)
            }
            Meeting::Listen => listens(&mut sandboxd, None),
            Meeting::Relaying(relay) => listens(&mut sandboxd, Some(relay)),
            Meeting::Relayed(relay, port) => relayed(&mut sandboxd, relay, port),
        };
        let sandboxd = sandboxd.spawn().unwrap();
        // Dialed only once `sandboxd` listens, as a dial before would wait a redial out.
        if let Gateway::Listen(address) = gateway {
            listened(address);
        }
        let (sender, arrivals) = mpsc::channel();
        let accepted = registration.accept(
            (&Machine, hung()),
            None,
            (false, place),
            sender,
            |a| a,
            || false,
        );
        let mut session = accepted.unwrap();
        let dirs = ["in", "out", "home", "log"].map(relative);
        session
            .put(
                &absolute(&root),
                dirs.map(|dir| Put::Dir(dir, dir_mode())),
                hung(),
            )
            .unwrap();
        Self {
            root,
            path,
            gateway,
            session,
            arrivals,
            sandboxd,
            commands: Cell::new(0),
            dir,
        }
    }

    fn socket(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

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

    fn at(&self, relative: &str) -> AbsolutePath {
        absolute(&self.root.join(relative))
    }

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

    fn show(&self, text: &dyn std::fmt::Display) -> String {
        text.to_string()
            .replace(self.root.to_str().unwrap(), "$ROOT")
    }

    fn sandboxd(&self, args: &[&OsStr], env: &[(&str, &OsStr)]) -> Child {
        sandboxd(&self.path, args, env)
    }
}

fn absolute(path: &Path) -> AbsolutePath {
    AbsolutePath::try_from(path.to_str().unwrap().to_owned()).unwrap()
}

fn relative(path: &str) -> RelativePath {
    RelativePath::try_from(path.to_owned()).unwrap()
}

/// A `PATH` of one directory below `dir`, which holds links to [`TOOLS`] alone.
fn tools(dir: &Path) -> OsString {
    let path = env::var_os("PATH").unwrap();
    let bin = dir.join("bin");
    fs::create_dir(&bin).unwrap();
    for name in TOOLS {
        let Some(tool) = (env::split_paths(&path).map(|dir| dir.join(name))).find(|t| t.is_file())
        else {
            panic!("`{name}` isn't on PATH: install it");
        };
        symlink(tool, bin.join(name)).unwrap();
    }
    bin.into_os_string()
}

fn unlistened() -> SocketAddrV4 {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = SocketAddrV4::new(Ipv4Addr::LOCALHOST, listener.local_addr().unwrap().port());
    // A child that another test spawns holds a copy of each descriptor until it executes its
    // program, which keeps a dropped listener listening, so a dial there would meet it, then a
    // reset, and a listen there would fail. Shut down for reads, a listener stops listening and
    // frees its port, in every copy, at once.
    // Another process may take the freed port before `sandboxd` binds it, a race left as known.
    let socket = TcpStream::from(OwnedFd::from(listener));
    socket.shutdown(Shutdown::Read).unwrap();
    address
}

/// Returns once a socket listens at `address`, as `/proc/net/tcp` lists it.
// Its local address is the IP address's bytes as a native integer, then the port, in hexadecimal,
// and `0A` is the state LISTEN (`proc_net_tcp(5)`).
fn listened(address: SocketAddrV4) {
    let ip = u32::from_ne_bytes(address.ip().octets());
    let local = format!("{ip:08X}:{:04X}", address.port());
    let by = Deadline::after(HUNG);
    loop {
        let table = fs::read_to_string("/proc/net/tcp").unwrap();
        let listed = table.lines().skip(1).any(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            fields[1] == local && fields[3] == "0A"
        });
        if listed {
            return;
        }
        assert!(
            by.remaining().is_some(),
            "sandboxd never listened: it should listen as it starts"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn processes(root: &Path, timeout: Duration, done: impl Fn(&[u32]) -> bool) -> Vec<u32> {
    let deadline = Instant::now() + timeout;
    loop {
        // A process may exit between being listed and being looked at, and a zombie has no working
        // directory.
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

fn check(meeting: Meeting, scenario: impl FnOnce(&mut Sandbox) -> Vec<String>, expect: &Expect) {
    let mut sandbox = Sandbox::start(meeting);
    let mut lines = scenario(&mut sandbox);
    lines.extend(sandbox.end());
    // The protocol's version, which a change to the protocol moves, as a hello shows it.
    let text = (lines.join("\n"))
        .replace(
            &format!("\"version\":{PROTOCOL_VERSION}"),
            "\"version\":<version>",
        )
        .replace(
            &format!("\"version\": Number({PROTOCOL_VERSION})"),
            "\"version\": Number(<version>)",
        );
    expect.assert_eq(&text);
}

impl Sandbox {
    fn end(self) -> Vec<String> {
        let mut lines = Vec::new();
        let Self {
            root,
            gateway,
            session,
            arrivals,
            path: _,
            mut sandboxd,
            commands: _,
            dir,
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
        let left = processes(&root, HUNG, <[u32]>::is_empty);
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
        drop(dir);
        lines
    }
}

#[test]
fn puts_runs_and_collects() {
    check(
        Meeting::Socket,
        put_run_collect,
        &expect![[r#"
            exited: Code(3)
            out/empty/
            out/evidence/payload: 2109497 bytes
            out/result.json: {"bytes": 2109497}
            payload collected intact: true
            log/1.stderr: warning
            log/1.stdout: home=/pumpkin/home mode=train grant=unset attestation=unset key=unset path=/usr/bin:/bin in $ROOT/home
            then Ended { reason: "the owner dropped the session" }
            sandboxd: exit status: 0
            left running: 0"#]],
    );
}

fn put_run_collect(sandbox: &mut Sandbox) -> Vec<String> {
    let (dir, payload) = (relative("in/payload"), relative("in/payload/raw"));
    // Over two frames, and kept as evidence, so that it is put and collected in several.
    let raw = pattern(2 * MAX_BODY_BYTES + 12345);
    let root = absolute(&sandbox.root);
    let put = [
        Put::Dir(dir, dir_mode()),
        Put::File(payload, file_mode(), &raw),
    ];
    sandbox.session.put(&root, put, hung()).unwrap();
    let script = r#"
        printf '{"bytes": %s}' "$(wc --bytes < ../in/payload/raw)" > ../out/result.json
        mkdir ../out/evidence && cp ../in/payload/raw ../out/evidence/payload
        mkdir ../out/empty
        echo "home=$HOME mode=$MODE grant=${SANDBOXD_GRANT-unset} attestation=${SANDBOXD_ATTESTATION-unset} key=${SANDBOXD_TLS_KEY-unset} path=$PATH in $(pwd)"
        echo warning >&2
        exit 3"#;
    let launch = sandbox.sh(
        script,
        &["HOME=/pumpkin/home", "MODE=train", "PATH=/usr/bin:/bin"],
    );
    let command = sandbox.session.start(launch, hung()).unwrap();
    let exited = sandbox.exit(command, Duration::from_secs(10)).unwrap();
    let mut lines = vec![format!("exited: {exited:?}")];
    sandbox.session.freeze(hung()).unwrap();
    lines.extend(sandbox.collect("out").unwrap());
    let kept = fs::read(sandbox.root.join("collected/out/evidence/payload")).unwrap();
    lines.push(format!("payload collected intact: {}", kept == raw));
    lines.extend(sandbox.collect("log").unwrap());
    lines
}

#[test]
#[expect(clippy::too_many_lines, reason = "most of it is the transcript")]
// A tree keeps each entry's mode within the host's umask, but a directory's owner always has
// all three bits and no set-ID or sticky bit: the receiver can always remove what it got.
fn modes_and_stdin() {
    // The umask that the collected modes are within: this test's, whatever the machine's.
    rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o027));
    check(
        Meeting::Socket,
        |sandbox| {
            let mode = |mode: &str| Mode::try_from(mode.to_owned()).unwrap();
            let dir = |path, mode_: &str| Put::Dir(relative(path), mode(mode_));
            let file = |path, mode_: &str, bytes| Put::File(relative(path), mode(mode_), bytes);
            let put = [
                dir("home", "0700"),
                dir("in/tools", "0555"),
                file("in/tools/tool", "0755", b"#!/bin/sh\nwc --lines\n"),
                dir("in/private", "0700"),
                file("in/private/key", "0600", b"k"),
                file("in/prompt", "0444", b"line one\nline two\n"),
                dir("in/open", "0777"),
                file("in/open/shared", "0666", b"s"),
            ];
            sandbox
                .session
                .put(&absolute(&sandbox.root), put, hung())
                .unwrap();
            let script = "cd .. && stat --format='%a %n' home in/tools in/tools/tool in/private \
                          in/private/key in/prompt in/open in/open/shared && \
                          chmod 4755 in/tools/tool && \
                          chmod 1700 in/private && mkdir --mode=0500 in/shut && in/tools/tool";
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
            sandbox.collect("in").unwrap();
            let collected = sandbox.root.join("collected/in");
            for path in [
                "tools",
                "tools/tool",
                "private",
                "private/key",
                "prompt",
                "shut",
                "open",
                "open/shared",
            ] {
                let metadata = fs::metadata(collected.join(path)).unwrap();
                let mode = metadata.permissions().mode() & 0o7777;
                lines.push(format!("collected {path}: {mode:o}"));
            }
            let removed = fs::remove_dir_all(&collected);
            lines.push(format!("the collected tree removed: {removed:?}"));
            let trees = sandbox.session.trees();
            let again = [Put::File(relative("again"), mode("0644"), b"")];
            let replaced = trees.put(
                &sandbox.at("in/tools"),
                Root::Replaced,
                again,
                (&Machine, hung()),
            );
            let held: Vec<_> = (fs::read_dir(sandbox.root.join("in/tools")).unwrap())
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect();
            lines.push(format!("in/tools replaced: {replaced:?}, holds {held:?}"));
            lines
        },
        &expect![[r#"
            exited: Code(0)
            missing stdin: $ROOT/in/missing: No such file or directory (os error 2)
            log/1.stderr: 
            log/1.stdout: 755 home
            755 in/tools
            755 in/tools/tool
            700 in/private
            600 in/private/key
            444 in/prompt
            777 in/open
            666 in/open/shared
            2
            collected tools: 750
            collected tools/tool: 750
            collected private: 700
            collected private/key: 600
            collected prompt: 440
            collected shut: 700
            collected open: 750
            collected open/shared: 640
            the collected tree removed: Ok(())
            in/tools replaced: Ok(()), holds ["again"]
            then Ended { reason: "the owner dropped the session" }
            sandboxd: exit status: 0
            left running: 0"#]],
    );
}

// So that a frame lost, repeated or out of order shows.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|n| u8::try_from(n % 251).unwrap()).collect()
}

#[test]
fn inherited_ignored_child_signal() {
    // Linux reaps children itself when their parent explicitly ignores SIGCHLD.
    let mut command = stdx::process::command("env", PATH_ONLY);
    command.args(["--ignore-signal=CHLD", env!("CARGO_BIN_EXE_sandboxd")]);
    let mut sandbox = Sandbox::start_with(Meeting::Socket, command);
    let launch = sandbox.sh("exit 0", &[]);
    sandbox.session.start(launch, hung()).unwrap();
    let status = exited(&mut sandbox.sandboxd);
    expect!["exit status: 1"].assert_eq(&status.to_string());
}

#[test]
fn freezing() {
    check(
        Meeting::Socket,
        |sandbox| {
            // The shell and both sleeps, so that freezing finds each of them to kill.
            let launch = sandbox.sh("sleep 60 & sleep 60 & wait", &[]);
            let group = sandbox.session.start(launch, hung()).unwrap();
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
            // `sandboxd`'s own limit on its user's processes stands in for the sandbox's: either
            // fails a new process or thread alike.
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
            start: `sh` didn't start: Resource temporarily unavailable (os error 11): fix what the error says
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
            // Its name quotes its long first argument, each quote five bytes, which the reply cuts.
            let mut long = sandbox.sh("", &[]);
            let word = "'".repeat(20 << 10);
            long.argv = Argv::try_from(vec!["/nonexistent/program".to_owned(), word]).unwrap();
            let started = sandbox.session.start(long, hung()).map(|_| ());
            let said = show_error(sandbox, started);
            lines.push(format!(
                "missing program, a long word: {} bytes",
                said.len()
            ));
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
            // Started by name with its own PATH, a missing program is waited for by the standard
            // library itself, which must not race the reaper while another command runs.
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
                Put::File(file.clone(), file_mode(), b"first"),
                Put::File(file, file_mode(), b"second"),
            ];
            let put = sandbox.session.put(&absolute(&sandbox.root), twice, hung());
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
            missing program: `/nonexistent/program` didn't start: No such file or directory (os error 2): expected a program at `/nonexistent/program`
            missing program, a long word: 3072 bytes
            stdout a link: $ROOT/log/3.stdout: Too many levels of symbolic links (os error 40)
            stdout a FIFO: $ROOT/log/4.stdout: No such device or address (os error 6)
            missing program by name, 1000 times: ["`no-such-program` didn't start: No such file or directory (os error 2): install `no-such-program` on `PATH`"]
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
fn new_roots() {
    check(
        Meeting::Socket,
        |sandbox| {
            let trees = sandbox.session.trees();
            let root = absolute(&sandbox.root.join("in/fresh/tree"));
            let put = || [Put::Dir(relative("a"), dir_mode())];
            let first = thread::scope(|scope| {
                let put = scope.spawn(|| trees.put(&root, Root::New, put(), (&Machine, hung())));
                put.join().unwrap()
            });
            first.unwrap();
            let made = sandbox.root.join("in/fresh/tree/a").is_dir();
            let again = trees.put(&root, Root::New, put(), (&Machine, hung()));
            let mut lines = vec![
                format!("put, its parents made: {made}"),
                format!("put again: {}", show_error(sandbox, again)),
            ];
            let other = || [Put::Dir(relative("b"), dir_mode())];
            let file = sandbox.root.join("in/file");
            fs::write(&file, "x").unwrap();
            for at in [
                root.clone(),
                absolute(&file),
                absolute(&sandbox.root.join("in/none")),
            ] {
                trees
                    .put(&at, Root::Replaced, other(), (&Machine, hung()))
                    .unwrap();
                let held: Vec<_> = (fs::read_dir(at.as_str()).unwrap())
                    .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                    .collect();
                let at = at.as_str().replace(sandbox.root.to_str().unwrap(), "$ROOT");
                lines.push(format!("replaced {at}: holds {held:?}"));
            }
            lines
        },
        &expect![[r#"
            put, its parents made: true
            put again: $ROOT/in/fresh/tree: File exists (os error 17)
            replaced $ROOT/in/fresh/tree: holds ["b"]
            replaced $ROOT/in/file: holds ["b"]
            replaced $ROOT/in/none: holds ["b"]
            then Ended { reason: "the owner dropped the session" }
            sandboxd: exit status: 0
            left running: 0"#]],
    );
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
fn hello_answers() {
    check(
        Meeting::Socket,
        |sandbox| {
            // A later version's refusal may carry fields that this one doesn't know.
            let message = "a stand-in for a later sandbox protocol";
            let refusal = json!({ "type": "refused", "message": message, "later": true });
            let (mut gateway, sandboxd, hello) = fake_gateway(sandbox, "refused.sock");
            frame::write(&mut gateway, &refusal, &[]).unwrap();
            let refused = sandbox.show(&ended(sandboxd));
            let (gateway, sandboxd, _) = fake_gateway(sandbox, "unanswered.sock");
            drop(gateway);
            let unanswered = sandbox.show(&ended(sandboxd));
            vec![
                format!("its hello: {hello}; refused: {refused}"),
                format!("unanswered: {unanswered}"),
            ]
        },
        &expect![[r#"
            its hello: {"version":<version>}; refused: exit status: 1: sandboxd: opening the session with the gateway at $ROOT/refused.sock failed: it refused the session: a stand-in for a later sandbox protocol
            unanswered: exit status: 1: sandboxd: opening the session with the gateway at $ROOT/unanswered.sock failed: it closed the connection before it answered the hello: expected it to accept or refuse the session
            then Ended { reason: "the owner dropped the session" }
            sandboxd: exit status: 0
            left running: 0"#]],
    );
}

#[test]
fn over_tcp() {
    check(
        Meeting::Tcp,
        |sandbox| {
            let Gateway::Tcp(_, address) = sandbox.gateway else {
                panic!("the sandbox was to meet the gateway over TCP");
            };
            let foreign = [(SANDBOXD_ATTESTATION, OsStr::new("another"))];
            let unadmitted = sandbox.sandboxd(&[address.to_string().as_ref()], &foreign);
            let refused = ended(unadmitted).replace(&address.to_string(), "$GATEWAY");
            let again = [(SANDBOXD_ATTESTATION, OsStr::new(ATTESTATION))];
            let replayed = sandbox.sandboxd(&[address.to_string().as_ref()], &again);
            let replayed = ended(replayed).replace(&address.to_string(), "$GATEWAY");
            vec![
                format!("an attestation that no check admits: {refused}"),
                format!("the sandbox's attestation, again: {replayed}"),
            ]
        },
        &expect![[r#"
            an attestation that no check admits: exit status: 1: sandboxd: opening the session with the gateway at $GATEWAY failed: it refused the session: the attestation opens no session, as no sandbox's start waits: expected the attestation of a sandbox whose start waits, presented once
            the sandbox's attestation, again: exit status: 1: sandboxd: opening the session with the gateway at $GATEWAY failed: it refused the session: the attestation opens no session, as no sandbox's start waits: expected the attestation of a sandbox whose start waits, presented once
            then Ended { reason: "the owner dropped the session" }
            sandboxd: exit status: 0
            left running: 0"#]],
    );
}

#[test]
fn listening() {
    check(
        Meeting::Listen,
        |sandbox| {
            let Gateway::Listen(address) = sandbox.gateway else {
                panic!("the sandbox was to listen");
            };
            let second = TcpStream::connect(address)
                .map(drop)
                .map_err(|error| error.kind());
            let mut lines = vec![format!(
                "a second peer, once the session is open: {second:?}"
            )];
            lines.extend(listening_apart(sandbox));
            let by = unix_seconds(3600);
            let with_socket = ["/run/gateway.sock", "--listen", "--by", &by].map(OsStr::new);
            let refused = ended(sandbox.sandboxd(&with_socket, &grant()));
            lines.push(format!("--listen with a socket: {refused}"));
            let unbounded = ["127.0.0.1:7434", "--listen"].map(OsStr::new);
            let refused = ended(sandbox.sandboxd(&unbounded, &grant()));
            lines.push(format!("--listen without --by: {refused}"));
            lines
        },
        &expect![[r#"
            a second peer, once the session is open: Err(ConnectionRefused)
            Accepted { grant: Some(RegistrationGrant(..)) }: its hello Some(Object {"version": Number(<version>)}), then closed
            Accepted { grant: None }: its hello Some(Object {"version": Number(<version>)}), then closed
            Refused { message: "why 0" }: its hello Some(Object {"version": Number(<version>)}), then closed
            Refused { message: "why 1" }: its hello Some(Object {"version": Number(<version>)}), then closed
            Refused { message: "why 2" }: its hello Some(Object {"version": Number(<version>)}), then closed
            Refused { message: "why 3" }: its hello Some(Object {"version": Number(<version>)}), then closed
            Refused { message: "why 4" }: its hello Some(Object {"version": Number(<version>)}), then closed
            Refused { message: "why 5" }: its hello Some(Object {"version": Number(<version>)}), then closed
            Refused { message: "why 6" }: its hello Some(Object {"version": Number(<version>)}), then closed
            Refused { message: "why 7" }: its hello Some(Object {"version": Number(<version>)}), then closed
            Refused { message: "why 8" }: its hello Some(Object {"version": Number(<version>)}), then closed
            Accepted { grant: Some(RegistrationGrant(..)) }: its hello Some(Object {"version": Number(<version>)}), then served: {"message":"/nonexistent: No such file or directory (os error 2)","stream":1,"type":"failed"}
            then, its session ended: exit status: 0: sandboxd: turned away the peer at $PEER0: it presented another registration grant: expected the sandbox's
            sandboxd: turned away the peer at $PEER1: it presented no registration grant: expected the sandbox's
            sandboxd: turned away the peer at $PEER2: it refused the session: why 0
            sandboxd: turned away the peer at $PEER3: it refused the session: why 1
            sandboxd: turned away the peer at $PEER4: it refused the session: why 2
            sandboxd: turned away the peer at $PEER5: it refused the session: why 3
            sandboxd: turned away the peer at $PEER6: it refused the session: why 4
            sandboxd: turned away the peer at $PEER7: it refused the session: why 5
            sandboxd: turned away the peer at $PEER8: it refused the session: why 6
            sandboxd: turned away the peer at $PEER9: it refused the session: why 7
            sandboxd: telling of no more peers it turns away, having told of 10
            --listen with a socket: exit status: 1: sandboxd: --listen takes a TCP address of the sandbox's own: give one, such as `0.0.0.0:7434`, or no --listen with a socket
            --listen without --by: exit status: 1: sandboxd: --listen waits until the start's deadline: give it as --by, in seconds since the Unix epoch
            then Ended { reason: "the owner dropped the session" }
            sandboxd: exit status: 0
            left running: 0"#]],
    );
}

/// Has `sandboxd` listen, relaying at `relay` if given, and registers it where it listens.
fn listens(sandboxd: &mut Command, relay: Option<SocketAddr>) -> (Gateway, Registration) {
    let dialer = Dialer::new(test_support::edge::client());
    let (Listening { grant }, unreached) = dialer.issue().unwrap();
    // For `sandboxd` to listen at; until it does, the gateway dials again.
    let mut address = unlistened();
    if let Some(relay) = relay {
        // Apart from 127.0.0.1, where its companion listens at the same port.
        address.set_ip(Ipv4Addr::new(127, 0, 0, 2));
        sandboxd.args(["--relay", &relay.to_string()]);
    }
    let by = unix_seconds(3600);
    sandboxd
        .args(["--listen", &address.to_string(), "--by", &by])
        .env(SANDBOXD_GRANT, grant.digits());
    let port = test_support::edge::front(address.into());
    let endpoint = Endpoint::new(test_support::edge::NAME.to_owned(), port).unwrap();
    (Gateway::Listen(address), unreached.at(endpoint))
}

/// Has `sandboxd` listen as the companion `localhost` behind the relay at `relay`, at `port` on
/// 127.0.0.1, and registers it there.
fn relayed(sandboxd: &mut Command, relay: SocketAddr, port: u16) -> (Gateway, Registration) {
    let dialer = Dialer::new(test_support::edge::client());
    let localhost = CompanionName::try_from("localhost".to_owned()).unwrap();
    let (Relayed { grant, identity }, unreached) = dialer.relayed(&localhost).unwrap();
    let address = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
    let by = unix_seconds(3600);
    sandboxd
        .args([&address.to_string(), "--listen", "--by", &by, "--tls"])
        .env(SANDBOXD_GRANT, grant.digits())
        .env(SANDBOXD_TLS_CERTIFICATE, identity.certificate_digits())
        .env(SANDBOXD_TLS_KEY, identity.key_digits());
    let port = test_support::edge::front(relay);
    let endpoint = Endpoint::new(test_support::edge::NAME.to_owned(), port).unwrap();
    (Gateway::Listen(address), unreached.at(endpoint))
}

// A case of flags that `sandboxd` refuses: its name, its arguments and its environment.
type Refused<'a> = (&'a str, &'a [&'a str], &'a [(&'a str, &'a OsStr)]);

#[test]
fn relays() {
    let relay = SocketAddr::from(unlistened());
    let sandbox = Sandbox::start(Meeting::Relaying(relay));
    let Gateway::Listen(address) = sandbox.gateway else {
        panic!("the sandbox was to listen");
    };
    let port = address.port();
    // On a thread named apart, which a test directory of its own is named after.
    let companion = thread::Builder::new().name("relays: the companion".to_owned());
    let checked = companion.spawn(move || check(
        Meeting::Relayed(relay, port),
        |companion| {
            let mut lines = put_run_collect(companion);
            let mut unrouted = dialed(relay);
            unrouted.write_all(b"not a companion's name\n").unwrap();
            let read = unrouted.read(&mut [0]).map_err(|error| error.kind());
            lines.push(format!("a route that names no companion: {read:?}"));
            let by = unix_seconds(3600);
            let cases: [Refused<'_>; 3] = [
                ("--tls without --listen", &["127.0.0.1:1", "--tls"], &[]),
                ("--relay without --listen", &["127.0.0.1:1", "--relay", "0.0.0.0:7435"], &[]),
                ("--tls with no identity", &["127.0.0.1:1", "--listen", "--by", &by, "--tls"], &grant()),
            ];
            for (case, args, env) in cases {
                let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
                lines.push(format!("{case}: {}", ended(companion.sandboxd(&args, env))));
            }
            lines
        },
        &expect![[r#"
            exited: Code(3)
            out/empty/
            out/evidence/payload: 2109497 bytes
            out/result.json: {"bytes": 2109497}
            payload collected intact: true
            log/1.stderr: warning
            log/1.stdout: home=/pumpkin/home mode=train grant=unset attestation=unset key=unset path=/usr/bin:/bin in $ROOT/home
            a route that names no companion: Ok(0)
            --tls without --listen: exit status: 1: sandboxd: --tls serves the session to a gateway that dials sandboxd: give it with --listen
            --relay without --listen: exit status: 1: sandboxd: --relay carries connections to companions that listen as their sandbox's own sandboxd does: give it with --listen
            --tls with no identity: exit status: 1: sandboxd: SANDBOXD_TLS_CERTIFICATE is not set: set it to the digits of the TLS identity issued for the sandbox, which a sandboxd that listens with --tls serves with
            then Ended { reason: "the owner dropped the session" }
            sandboxd: exit status: 0
            left running: 0"#]],
    ));
    checked.unwrap().join().unwrap();
    let ended = sandbox.end();
    assert_eq!(ended.last().map(String::as_str), Some("left running: 0"));
}

// The loopback addresses where the links' connections to the companions are taken, as a provider
// would give them: ones that no other test listens on.
const SCHEDULER_AT: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 77);
const WORKER_AT: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 78);

// Two `sandboxd`s of a pod on one host: the test serves the companion's port on 127.0.0.1, and
// connects to main's link at the companion's address, as main's commands would.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the pod, the steps and their snapshot"
)]
fn links() {
    let pod = Pod::default();
    let companion =
        |name: &str| Member::Companion(CompanionName::try_from(name.to_owned()).unwrap());
    let (scheduler, worker) = (companion("scheduler"), companion("worker"));
    let service = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = LinkPort::try_from(service.local_addr().unwrap().port()).unwrap();
    let unserved = LinkPort::try_from(unlistened().port()).unwrap();
    // Taken by main's `sandboxd`, but given by no link of the pod.
    let unlinked = LinkPort::try_from(unlistened().port()).unwrap();
    let mut command = stdx::process::command(env!("CARGO_BIN_EXE_sandboxd"), PATH_ONLY);
    let taken = [
        (&scheduler, port, SCHEDULER_AT),
        (&scheduler, unserved, SCHEDULER_AT),
        (&scheduler, unlinked, SCHEDULER_AT),
        (&worker, port, WORKER_AT),
    ];
    for (to, at, address) in taken {
        command.args(["--link", &format!("{to}:{at}@{address}")]);
    }
    let reaches = [(&scheduler, port), (&scheduler, unserved), (&worker, port)];
    let reaches = reaches.map(|(to, at)| (to.clone(), at));
    let place = pod.place(Member::Main, BTreeSet::from(reaches));
    let main = Sandbox::start_placed(Meeting::Socket, command, Some(place));
    // On a thread named apart, which a test directory of its own is named after.
    let place = pod.place(scheduler, BTreeSet::new());
    let starting = thread::Builder::new().name("links: the companion".to_owned());
    let started = starting.spawn(move || {
        let sandboxd = stdx::process::command(env!("CARGO_BIN_EXE_sandboxd"), PATH_ONLY);
        Sandbox::start_placed(Meeting::Socket, sandboxd, Some(place))
    });
    let companion = started.unwrap().join().unwrap();
    let mut lines = vec![carried(
        &service,
        SocketAddr::from((SCHEDULER_AT, port.get())),
    )];
    let closed = |address: Ipv4Addr, at: LinkPort| {
        let mut client = dialed(SocketAddr::from((address, at.get())));
        client.set_read_timeout(Some(HELLO_TIMEOUT)).unwrap();
        client.read(&mut [0]).map_err(|error| error.kind())
    };
    lines.push(format!(
        "to a port where nothing listens: {:?}",
        closed(SCHEDULER_AT, unserved)
    ));
    lines.push(format!(
        "by no link of the pod: {:?}",
        closed(SCHEDULER_AT, unlinked)
    ));
    lines.push(format!(
        "to a container with no session: {:?}",
        closed(WORKER_AT, port)
    ));
    let mut client = dialed(SocketAddr::from((SCHEDULER_AT, port.get())));
    client.set_read_timeout(Some(HELLO_TIMEOUT)).unwrap();
    let (served, _) = service.accept().unwrap();
    lines.extend(
        companion
            .end()
            .into_iter()
            .map(|line| format!("companion: {line}")),
    );
    let read = client.read(&mut [0]).map_err(|error| error.kind());
    lines.push(format!("once the companion's session ended: {read:?}"));
    drop(served);
    let cases: [Refused<'_>; 2] = [
        (
            "--link with no address",
            &["127.0.0.1:1", "--link", "main:8000"],
            &[],
        ),
        (
            "--link off the loopback",
            &["127.0.0.1:1", "--link", "main:8000@10.0.0.1"],
            &[],
        ),
    ];
    for (case, args, env) in cases {
        let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
        lines.push(format!("{case}: {}", ended(main.sandboxd(&args, env))));
    }
    lines.extend(main.end());
    expect![[r#"
        1 MiB each way, past the window: to the service intact: true, back intact: true
        to a port where nothing listens: Ok(0)
        by no link of the pod: Ok(0)
        to a container with no session: Ok(0)
        companion: then Ended { reason: "the owner dropped the session" }
        companion: sandboxd: exit status: 0
        companion: left running: 0
        once the companion's session ended: Ok(0)
        --link with no address: exit status: 1: sandboxd: can't parse `--link`, `main:8000` is no link: it has no `@`: give `<to>:<port>@<address>`, such as `main:8000@127.0.0.2`: see `sandboxd --help`
        --link off the loopback: exit status: 1: sandboxd: can't parse `--link`, `main:8000@10.0.0.1` is no link: 10.0.0.1 is no loopback address, which only this container reaches: give `<to>:<port>@<address>`, such as `main:8000@127.0.0.2`: see `sandboxd --help`
        then Ended { reason: "the owner dropped the session" }
        sandboxd: exit status: 0
        left running: 0"#]].assert_eq(&lines.join("\n"));
}

// Connects to the link at `link`, and sends 1 MiB, eight windows, to the service that `service`
// takes it at, which sends as much back once the client has ended its way; says whether each got
// all of it, intact.
fn carried(service: &TcpListener, link: SocketAddr) -> String {
    let payload: Vec<u8> = (0..1 << 20)
        .map(|n: u32| u8::try_from(n % 251).unwrap())
        .collect();
    let mut client = dialed(link);
    client.set_read_timeout(Some(HELLO_TIMEOUT)).unwrap();
    let (mut served, _) = service.accept().unwrap();
    served.set_read_timeout(Some(HELLO_TIMEOUT)).unwrap();
    let sent = payload.clone();
    let serving = thread::spawn(move || {
        let mut got = Vec::new();
        served.read_to_end(&mut got).unwrap();
        served.write_all(&sent).unwrap();
        got == sent
    });
    client.write_all(&payload).unwrap();
    client.shutdown(Shutdown::Write).unwrap();
    let mut back = Vec::new();
    client.read_to_end(&mut back).unwrap();
    let there = serving.join().unwrap();
    format!(
        "1 MiB each way, past the window: to the service intact: {there}, back intact: {}",
        back == payload
    )
}

fn listening_apart(sandbox: &Sandbox) -> Vec<String> {
    let apart = unlistened();
    let apart_text = apart.to_string();
    let by = unix_seconds(3600);
    let listen = ["--listen", &apart_text, "--by", &by].map(OsStr::new);
    let listening = sandbox.sandboxd(&listen, &grant());
    let mut lines = Vec::new();
    let mut peers = Vec::new();
    let other = RegistrationGrant::from([0xab; 16]);
    let ours = RegistrationGrant::from(std::array::from_fn(|n| u8::try_from(n * 0x11).unwrap()));
    let refused = |n| HelloReply::Refused {
        message: format!("why {n}"),
    };
    let answers = [
        HelloReply::Accepted { grant: Some(other) },
        HelloReply::Accepted { grant: None },
    ]
    .into_iter()
    .chain((0..9).map(refused))
    .chain([HelloReply::Accepted { grant: Some(ours) }]);
    lines.extend(answers.map(|answer| greeted(apart.into(), &answer, &mut peers)));
    let mut said = ended(listening);
    for (n, peer) in peers.iter().enumerate() {
        said = said.replace(peer, &format!("$PEER{n}"));
    }
    lines.push(format!("then, its session ended: {said}"));
    lines
}

fn greeted(address: SocketAddr, answer: &HelloReply, peers: &mut Vec<String>) -> String {
    let mut peer = dialed(address);
    peers.push(peer.local_addr().unwrap().to_string());
    let hello = frame::read::<Value>(&mut peer)
        .unwrap()
        .map(|(hello, _)| hello);
    frame::write(&mut peer, answer, &[]).unwrap();
    let probe = ToSandbox::Collect {
        stream: 1,
        path: absolute(Path::new("/nonexistent")),
    };
    let _ = frame::write(&mut peer, &probe, &[]);
    let then = match frame::read::<Value>(&mut peer) {
        Ok(None) => "closed".to_owned(),
        Err(error) if error.kind() == io::ErrorKind::ConnectionReset => "closed".to_owned(),
        Ok(Some((reply, _))) => format!("served: {reply}"),
        Err(error) => error.to_string(),
    };
    format!("{answer:?}: its hello {hello:?}, then {then}")
}

fn unix_seconds(offset: i64) -> String {
    let now = std::time::UNIX_EPOCH.elapsed().unwrap().as_secs();
    now.checked_add_signed(offset).unwrap().to_string()
}

fn dialed(address: SocketAddr) -> TcpStream {
    // A `sandboxd` that never listens fails the test rather than hang it.
    let by = Deadline::after(HUNG);
    let stream = loop {
        match TcpStream::connect(address) {
            Ok(stream) => break stream,
            Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                assert!(
                    by.remaining().is_some(),
                    "sandboxd never listened: it should listen as it starts"
                );
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("{error}"),
        }
    };
    stream.set_read_timeout(Some(HUNG)).unwrap();
    stream.set_write_timeout(Some(HUNG)).unwrap();
    stream
}

#[test]
fn unclaimed() {
    check(
        Meeting::Socket,
        |sandbox| {
            let address = unlistened().to_string();
            // A minute ago, so that with the slack the deadline has just passed.
            let by = unix_seconds(-60);
            let grant = &grant();
            let listening = [&address, "--listen", "--by", &by].map(OsStr::new);
            let dialing = [&address, "--by", &by].map(OsStr::new);
            vec![
                format!("listening: {}", ended(sandbox.sandboxd(&listening, grant))),
                format!("dialing: {}", ended(sandbox.sandboxd(&dialing, grant))),
            ]
        },
        &expect![[r#"
            listening: exit status: 1: sandboxd: no gateway claimed the sandbox by its start's deadline, and the slack after it: expected the gateway to dial while the start lasts, past which nothing does, so the sandbox ends
            dialing: exit status: 1: sandboxd: --by bounds how long a sandboxd that listens waits: give it with --listen
            then Ended { reason: "the owner dropped the session" }
            sandboxd: exit status: 0
            left running: 0"#]],
    );
}

#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
fn wait_at_a_full_control_socket() {
    let dir = TestDir::new();
    let socket = dir.path().join("socket");
    let listener = UnixListener::bind(&socket).unwrap();
    rustix::net::listen(&listener, 0).unwrap();
    // Linux lets one connection through a backlog of zero. The next waits for an accept.
    let _queued = UnixStream::connect(&socket).unwrap();
    let binary = test_support::sandboxd();
    // Quiet, so that `docker` writes nothing to stderr as it pulls an image the engine lacks.
    let output = test_support::docker()
        .args(["run", "--quiet", "--rm", "--network", "none", "--volume"])
        .arg(format!("{}:/pumpkin/sandboxctl:ro", dir.path().display()))
        .arg("--volume")
        .arg(format!("{}:/sandboxctl:ro", binary.display()))
        .args([
            test_support::BUSYBOX,
            "timeout",
            "5",
            "/sandboxctl",
            "call",
            "train",
            "/etc/passwd",
            "--wait",
            "1",
        ])
        .output()
        .unwrap();
    let code = output.status.code();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(output.stdout.as_slice(), b"");
    expect![[r#"
        Some(124): sandboxctl: wait expired connecting to sandboxd: the request was not sent
    "#]]
    .assert_eq(&format!("{code:?}: {stderr}"));
}

#[test]
fn refused_grants() {
    check(
        Meeting::Socket,
        |sandbox| {
            let (closed, by) = (unlistened().to_string(), unix_seconds(3600));
            let listening = [&closed, "--listen", "--by", &by].map(OsStr::new);
            let (short, not_utf8) = (OsStr::new("0123456789abcdef"), OsStr::from_bytes(b"\xff"));
            let grants = [
                ("listening with no grant", Vec::new()),
                (
                    "listening with a grant too short",
                    vec![(SANDBOXD_GRANT, short)],
                ),
                (
                    "listening with a grant not UTF-8",
                    vec![(SANDBOXD_GRANT, not_utf8)],
                ),
            ];
            (grants.into_iter())
                .map(|(case, env)| {
                    let refused = ended(sandbox.sandboxd(&listening, &env));
                    format!("{case}: {refused}")
                })
                .collect()
        },
        &expect![[r#"
            listening with no grant: exit status: 1: sandboxd: SANDBOXD_GRANT is not set: set it to the registration grant issued for the sandbox, which the gateway that dials a sandboxd that listens presents
            listening with a grant too short: exit status: 1: sandboxd: SANDBOXD_GRANT: the registration grant isn't 32 lowercase hexadecimal digits: present the one the gateway issued for the sandbox
            listening with a grant not UTF-8: exit status: 1: sandboxd: SANDBOXD_GRANT: the registration grant isn't 32 lowercase hexadecimal digits: present the one the gateway issued for the sandbox
            then Ended { reason: "the owner dropped the session" }
            sandboxd: exit status: 0
            left running: 0"#]],
    );
}

#[test]
fn refused_before_the_session() {
    check(
        Meeting::Socket,
        |sandbox| {
            // Never accepted from: a `sandboxd` that dialed it would wait on, unanswered.
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let at = listener.local_addr().unwrap().to_string();
            let closed = unlistened().to_string();
            let long = "a".repeat(MAX_ATTESTATION_BYTES + 1);
            let attesting = |attestation| vec![(SANDBOXD_ATTESTATION, attestation)];
            let cases = [
                ("a relative socket path", "gateway.sock", Vec::new()),
                ("a host's name", "localhost:7433", Vec::new()),
                ("no attestation", &at, Vec::new()),
                ("only a grant", &at, grant().to_vec()),
                ("an empty attestation", &at, attesting(OsStr::new(""))),
                ("an attestation too long", &at, attesting(OsStr::new(&long))),
                (
                    "nothing listening",
                    &closed,
                    attesting(OsStr::new(ATTESTATION)),
                ),
            ];
            let mut lines: Vec<String> = (cases.into_iter())
                .map(|(case, gateway, env)| {
                    let ended = ended(sandbox.sandboxd(&[gateway.as_ref()], &env));
                    format!("{case}: {}", ended.replace(&closed, "$CLOSED"))
                })
                .collect();
            // Each refused as it is parsed, before sandboxd becomes anyone. The tests run as a user
            // that isn't root.
            for user in ["0:1000", "1000", "a:b", "1234:1235"] {
                let args = [&closed, "--user", user].map(OsStr::new);
                let ended = ended(sandbox.sandboxd(&args, &grant()));
                lines.push(format!("--user {user}: {ended}"));
            }
            listener.set_nonblocking(true).unwrap();
            lines.push(format!("dialed: {}", listener.accept().is_ok()));
            lines
        },
        &expect![[r#"
            a relative socket path: exit status: 1: sandboxd: can't parse `gateway`, `gateway.sock` is neither a Unix socket's absolute path nor a TCP address: give one, such as `/run/gateway.sock` or `192.0.2.1:7433`: see `sandboxd --help`
            a host's name: exit status: 1: sandboxd: can't parse `gateway`, `localhost:7433` is neither a Unix socket's absolute path nor a TCP address: give one, such as `/run/gateway.sock` or `192.0.2.1:7433`: see `sandboxd --help`
            no attestation: exit status: 1: sandboxd: SANDBOXD_ATTESTATION is not set: set it to the sandbox's attestation, which a sandboxd that dials its gateway over TCP presents
            only a grant: exit status: 1: sandboxd: SANDBOXD_ATTESTATION is not set: set it to the sandbox's attestation, which a sandboxd that dials its gateway over TCP presents
            an empty attestation: exit status: 1: sandboxd: SANDBOXD_ATTESTATION: the attestation is 0 bytes: expected 1 to 4096
            an attestation too long: exit status: 1: sandboxd: SANDBOXD_ATTESTATION: the attestation is 4097 bytes: expected 1 to 4096
            nothing listening: exit status: 1: sandboxd: connecting to the gateway at $CLOSED failed: Connection refused (os error 111)
            --user 0:1000: exit status: 1: sandboxd: can't parse `--user`, `0:1000` is not a user's `uid:gid`: give two numbers but 0, such as `1000:1000`: see `sandboxd --help`
            --user 1000: exit status: 1: sandboxd: can't parse `--user`, `1000` is not a user's `uid:gid`: give two numbers but 0, such as `1000:1000`: see `sandboxd --help`
            --user a:b: exit status: 1: sandboxd: can't parse `--user`, `a:b` is not a user's `uid:gid`: give two numbers but 0, such as `1000:1000`: see `sandboxd --help`
            --user 1234:1235: exit status: 1: sandboxd: started as a user that isn't root: `--user` needs sandboxd to start as root to drop to 1234:1235
            dialed: false
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
            let (mut gateway, sandboxd) = accepting_gateway(sandbox, "fake.sock");
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
            the collect: file RelativePath("tree") of 4194304 bytes, intact: true
            the put: done
            the put's file: intact: true
            the fake gateway's sandboxd: exit status: 0
            then Ended { reason: "the owner dropped the session" }
            sandboxd: exit status: 0
            left running: 0"#]],
    );
}

#[test]
fn collects_one_file() {
    check(
        Meeting::Socket,
        |sandbox| {
            fs::write(sandbox.root.join("out/result.json"), b"{}").unwrap();
            let (mut gateway, sandboxd) = accepting_gateway(sandbox, "fake.sock");
            let collect = ToSandbox::Collect {
                stream: 1,
                path: absolute(&sandbox.root.join("out/result.json")),
            };
            frame::write(&mut gateway, &collect, &[]).unwrap();
            let mut lines = Vec::new();
            loop {
                let (message, body) = frame::read::<FromSandbox>(&mut gateway).unwrap().unwrap();
                match message {
                    FromSandbox::Entry { entry, .. } => lines.push(entry_shown(&entry)),
                    FromSandbox::Data { .. } => {
                        lines.push(format!("its data: {}", String::from_utf8_lossy(&body)));
                    }
                    FromSandbox::End { .. } => break,
                    other => lines.push(format!("then {other:?}")),
                }
            }
            drop(gateway);
            lines.push(format!("the fake gateway's sandboxd: {}", ended(sandboxd)));
            lines
        },
        &expect![[r#"
            file RelativePath("result.json") of 2 bytes
            its data: {}
            the fake gateway's sandboxd: exit status: 0
            then Ended { reason: "the owner dropped the session" }
            sandboxd: exit status: 0
            left running: 0"#]],
    );
}

// Without its mode, which the umask decides and `modes_and_stdin` shows.
fn entry_shown(entry: &Entry) -> String {
    match entry {
        Entry::Dir { path, mode: _ } => format!("dir {path:?}"),
        Entry::File {
            path,
            size,
            mode: _,
        } => format!("file {path:?} of {size} bytes"),
    }
}

fn collect_and_put<'a>(root: &Path, bytes: &'a [u8]) -> Vec<(ToSandbox, &'a [u8])> {
    let collect = ToSandbox::Collect {
        stream: 1,
        path: absolute(&root.join("out")),
    };
    let put = ToSandbox::Put {
        stream: 2,
        dir: absolute(&root.join("in")),
        root: Root::Kept,
    };
    let entry = Entry::File {
        path: relative("put"),
        size: bytes.len() as u64,
        mode: file_mode(),
    };
    let mut frames = vec![
        (collect, &[][..]),
        (put, &[]),
        (ToSandbox::Entry { stream: 2, entry }, &[]),
    ];
    let data = bytes.chunks(MAX_BODY_BYTES);
    frames.extend(data.map(|chunk| (ToSandbox::Data { stream: 2 }, chunk)));
    frames.push((ToSandbox::End { stream: 2 }, &[]));
    frames
}

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
            FromSandbox::Entry { stream: 1, entry } => entries.push(entry_shown(&entry)),
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

fn accepting_gateway(sandbox: &Sandbox, name: &str) -> (UnixStream, Child) {
    let (mut connection, sandboxd, _) = fake_gateway(sandbox, name);
    frame::write(&mut connection, &HelloReply::Accepted { grant: None }, &[]).unwrap();
    (connection, sandboxd)
}

fn fake_gateway(sandbox: &Sandbox, name: &str) -> (UnixStream, Child, Value) {
    let socket = sandbox.socket(name);
    let gateway = UnixListener::bind(&socket).unwrap();
    gateway.set_nonblocking(true).unwrap();
    let sandboxd = sandbox.sandboxd(&[socket.as_ref()], &[]);
    // A `sandboxd` that never connects fails the test rather than hang it.
    let by = Deadline::after(HUNG);
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
    connection.set_read_timeout(Some(HUNG)).unwrap();
    connection.set_write_timeout(Some(HUNG)).unwrap();
    let (hello, _) = frame::read::<Value>(&mut connection).unwrap().unwrap();
    (connection, sandboxd, hello)
}

fn sandboxd(path: &OsStr, args: &[&OsStr], env: &[(&str, &OsStr)]) -> Child {
    let mut sandboxd = stdx::process::command(env!("CARGO_BIN_EXE_sandboxd"), PATH_ONLY);
    sandboxd
        .args(args)
        .env("PATH", path)
        .envs(env.iter().copied());
    sandboxd.stderr(Stdio::piped()).spawn().unwrap()
}

// A grant that no gateway of the tests issues, in the variable that holds it.
fn grant() -> [(&'static str, &'static OsStr); 1] {
    [(SANDBOXD_GRANT, OsStr::new(GRANT))]
}

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

#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
fn as_root_with_user() {
    let scratch = TestDir::new();
    let root = scratch.path();
    let sockets = Sockets::create(root.join("sockets")).unwrap();
    let name = Name::try_from("as-root".to_owned()).unwrap();
    let (SocketDial { dir }, registration) = sockets.issue(&name).unwrap();
    let container = format!("sandboxd-{}-as-root", std::process::id());
    start_as_root(&dir, &container, "1234:1235");
    let (sender, arrivals) = mpsc::channel();
    let accepted = registration.accept(
        (&Machine, hung()),
        None,
        (false, None),
        sender,
        |a| a,
        || false,
    );
    let mut session = accepted.unwrap();
    let mut lines = as_user(&mut session, &arrivals, &root.join("collected"));
    lines.extend(left_behind(&mut session, &arrivals, &container));
    drop(session);
    // Gone already, once the session ended, which is as good as removed.
    let _ = (test_support::docker())
        .args(["rm", "--force", &container])
        .output();
    expect![[r#"
        exited: Code(0)
        /pumpkin: 1234:1235 755
        Uid: 1234 1234 1234 1234
        Gid: 1235 1235 1235 1235
        Groups:
        CapInh: 0000000000000000
        CapPrm: 0000000000000000
        CapEff: 0000000000000000
        CapAmb: 0000000000000000
        NoNewPrivs: 1
        nsenter: setuid: Operation not permitted
        root refused
        left behind: Code(3)
        before freezing: /sbin/docker-init -- /.pumpkin/sandboxd --user 1234:1235 /.pumpkin/gateway/s, /.pumpkin/sandboxd --user 1234:1235 /.pumpkin/gateway/s, sleep 600, sleep 600
        after freezing: /sbin/docker-init -- /.pumpkin/sandboxd --user 1234:1235 /.pumpkin/gateway/s, /.pumpkin/sandboxd --user 1234:1235 /.pumpkin/gateway/s"#]]
    .assert_eq(&lines.join("\n"));
    drop(sockets);
}

fn start_as_root(dir: &Path, container: &str, user: &str) {
    let mount =
        |host: &Path, at: &str| format!("--mount=type=bind,source={},target={at}", host.display());
    let mut docker = test_support::docker();
    docker.args(["run", "--detach", "--rm", "--network=none", "--user=0:0"]);
    docker.arg("--init");
    docker.arg(format!("--name={container}"));
    docker.arg(mount(dir, "/.pumpkin/gateway"));
    docker.arg(mount(&test_support::sandboxd(), "/.pumpkin/sandboxd"));
    docker.args([test_support::BUSYBOX, "/.pumpkin/sandboxd", "--user", user]);
    docker.arg(format!("/.pumpkin/gateway/{SOCKET_NAME}"));
    // Not the container's id, which `docker run --detach` prints.
    let started = docker.stdout(Stdio::null()).status().unwrap().success();
    assert!(started, "the container was to start");
}

fn as_user(session: &mut Session, arrivals: &Receiver<Arrival>, collected: &Path) -> Vec<String> {
    let dirs = ["home", "log"].map(relative);
    let put = dirs.map(|dir| Put::Dir(dir, dir_mode()));
    session
        .put(&absolute(Path::new("/pumpkin")), put, hung())
        .unwrap();
    let script = r#"
        stat -c '/pumpkin: %u:%g %a' /pumpkin
        grep -E '^(Uid|Gid|Groups|CapInh|CapPrm|CapEff|CapAmb|NoNewPrivs):' /proc/self/status
        nsenter --setuid=0 id 2>&1 || echo "root refused""#;
    let exited = sh_as_user(session, arrivals, script);
    let limits = Limits {
        bytes: 1 << 20,
        entries: 10,
    };
    let log = absolute(Path::new("/pumpkin/log"));
    session.collect(&log, collected, limits, hung()).unwrap();
    let printed = fs::read_to_string(collected.join("out")).unwrap();
    let mut lines = vec![format!("exited: {exited:?}")];
    let printed = printed.lines().map(|line| line.replace('\t', " "));
    lines.extend(printed.map(|line| line.trim_end().to_owned()));
    lines
}

fn left_behind(
    session: &mut Session,
    arrivals: &Receiver<Arrival>,
    container: &str,
) -> Vec<String> {
    let exited = sh_as_user(
        session,
        arrivals,
        // One sleep is orphaned and the other is in a session of its own, outside its command's
        // process group: freezing kills both.
        "(sleep 600 &); setsid sleep 600 & exit 3",
    );
    // The script exits without waiting for its sleeps, which may not run yet: until then, each
    // shows as the command that starts it.
    let sandboxd = format!("/.pumpkin/sandboxd --user 1234:1235 /.pumpkin/gateway/{SOCKET_NAME}");
    let init = format!("/sbin/docker-init -- {sandboxd}");
    let running = [init.as_str(), &sandboxd, "sleep 600", "sleep 600"].join(", ");
    let before = contained(container, |listed| listed == running);
    session.freeze(hung()).unwrap();
    // Freezing returns once the processes it kills are gone.
    let after = contained(container, |_| true);
    vec![
        format!("left behind: {exited:?}"),
        format!("before freezing: {before}"),
        format!("after freezing: {after}"),
    ]
}

fn sh_as_user(session: &mut Session, arrivals: &Receiver<Arrival>, script: &str) -> ExitStatus {
    let out = absolute(Path::new("/pumpkin/log/out"));
    let launch = Launch {
        argv: Argv::try_from(["sh", "-c", script].map(str::to_owned).to_vec()).unwrap(),
        env: Env::try_from(Vec::new()).unwrap(),
        cwd: absolute(Path::new("/pumpkin/home")),
        stdin: None,
        stdout: out.clone(),
        stderr: out,
    };
    let process = session.start(launch, hung()).unwrap();
    match arrivals.recv_timeout(Duration::from_secs(30)) {
        Ok(Arrival::Exited {
            process: exited,
            status,
        }) if exited == process => status,
        arrived => panic!("the command's exit was to arrive, not {arrived:?}"),
    }
}

fn contained(container: &str, done: impl Fn(&str) -> bool) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let top = test_support::docker()
            .args(["top", container, "-o", "pid,args"])
            .output()
            .unwrap();
        let top = String::from_utf8(top.stdout).unwrap();
        let commands = top.lines().skip(1).map(|line| {
            let (_pid, command) = line.trim_start().split_once(' ').unwrap();
            command.trim_start().to_owned()
        });
        let listed = commands.collect::<Vec<_>>().join(", ");
        if done(&listed) || Instant::now() >= deadline {
            return listed;
        }
        thread::sleep(Duration::from_millis(10));
    }
}
