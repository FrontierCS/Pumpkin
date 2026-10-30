//! The Docker provider, against a real Docker daemon. These tests need Docker, so they are
//! ignored; `cargo xtask ci --docker` runs them, with `PUMPKIN_SANDBOXD` naming the static
//! `sandboxd` binary that `cargo xtask sandboxd` builds. Run without it, they fail. They reach
//! Docker through the context `PUMPKIN_DOCKER_CONTEXT` names, `default` if it is unset.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test helper fails the test that calls it"
)]
#![expect(
    clippy::print_stderr,
    reason = "a failed test shows its containers' logs"
)]

use std::cell::Cell;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

use expect_test::{Expect, expect};
use pumpkin_docker::{Container, ContextName, Engine, Report, RunTag, Security, Start};
use pumpkin_gateway::Put;
use pumpkin_schema::ir::{AbsolutePath, Argv, Env, EnvVar, Image, Name, Resources, User};
use pumpkin_schema::sandbox::tree::Limits;
use pumpkin_schema::sandbox::{ExitStatus, Launch, RelativePath};
use pumpkin_schema::workflow::Sandbox;

/// A small public image, pinned by digest.
const BUSYBOX: &str =
    "busybox@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e";

/// The environment of one test: its run tags and a directory for sockets and collected files.
/// Dropping it removes the containers of its runs, if it started any, after showing their logs
/// if the test failed.
struct Fixture {
    sandboxd: PathBuf,
    dir: PathBuf,
    run: RunTag,
    other_run: RunTag,
    started: Cell<bool>,
}

impl Fixture {
    /// The fixture for test `name`.
    fn new(name: &str) -> Self {
        let Some(sandboxd) = std::env::var_os("PUMPKIN_SANDBOXD") else {
            panic!(
                "`PUMPKIN_SANDBOXD` is unset: run the Docker tests with `cargo xtask ci --docker`, \
                 which builds sandboxd and sets it"
            );
        };
        let tag = |suffix: &str| {
            RunTag::try_from(format!("pk-{}-{name}{suffix}", std::process::id())).unwrap()
        };
        // Socket paths are short: a directory in the system's temporary one.
        let dir = std::env::temp_dir().join(format!("pk-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self {
            sandboxd: PathBuf::from(sandboxd).canonicalize().unwrap(),
            dir,
            run: tag(""),
            other_run: tag("-other"),
            started: Cell::new(false),
        }
    }

    /// Starts a sandbox for a role that runs as 4242:4243.
    fn start(&self, run: &RunTag, name: &str) -> Container {
        self.try_start(run, name, &self.sandboxd).unwrap()
    }

    fn try_start(&self, run: &RunTag, name: &str, sandboxd: &Path) -> std::io::Result<Container> {
        let socket_dir = self.dir.join(name);
        fs::create_dir_all(&socket_dir).unwrap();
        let name = Name::try_from(format!("{}-{name}", run.as_str())).unwrap();
        let sandbox = Sandbox {
            name: Name::try_from("role".to_owned()).unwrap(),
            image: image(),
            user: serde_json::from_str::<User>(r#"{"uid": 4242, "gid": 4243}"#).unwrap(),
            resources: serde_json::from_value::<Resources>(serde_json::json!({
                "cpu_millis": 500,
                "memory_bytes": 256 << 20,
            }))
            .unwrap(),
            model_route: None,
            assets: Vec::new(),
        };
        let start = Start {
            run,
            name: &name,
            sandbox: &sandbox,
            sandboxd,
            socket_dir: &socket_dir,
        };
        self.started.set(true);
        engine().start(&start)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let runs = if self.started.get() {
            vec![&self.run, &self.other_run]
        } else {
            Vec::new()
        };
        for run in runs {
            if thread::panicking() {
                for id in engine().list(run).unwrap_or_default() {
                    let logs = docker().args(["logs", &id]).output().unwrap();
                    eprintln!(
                        "logs of {id}:\n{}{}",
                        String::from_utf8_lossy(&logs.stdout),
                        String::from_utf8_lossy(&logs.stderr)
                    );
                }
            }
            let _ = engine().destroy_all(run);
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// The context the tests reach Docker through: `PUMPKIN_DOCKER_CONTEXT`, or `default`.
fn context() -> ContextName {
    let name = std::env::var("PUMPKIN_DOCKER_CONTEXT").unwrap_or_else(|_| "default".to_owned());
    ContextName::try_from(name).unwrap()
}

/// The engine the tests' context reaches.
fn engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(|| Engine::connect(context()).unwrap())
}

/// `docker`, run as the provider runs it, against the tests' context alone.
fn docker() -> Command {
    // Only what the provider's `docker_output` keeps of the environment, for the reasons it gives.
    let kept = ["HOME", "PATH", "DOCKER_CONFIG"]
        .into_iter()
        .filter_map(|name| Some((name, std::env::var_os(name)?)));
    let mut docker = Command::new("docker");
    docker.env_clear().envs(kept);
    docker.args(["--context", context().as_str()]);
    docker
}

/// The ID of the image the tests share, which the IR accepts as a pinned image: `busybox` with
/// a world-writable `/workspace`, which a role's user needs, since the writable layer is the
/// workspace. Built once, from the pinned image.
fn image() -> Image {
    static ID: OnceLock<String> = OnceLock::new();
    let id = ID.get_or_init(|| {
        let dockerfile = format!("FROM {BUSYBOX}\nRUN mkdir --mode=1777 /workspace\n");
        let mut build = docker()
            .args(["build", "--quiet", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // Dropping its standard input once written ends the Dockerfile.
        (build.stdin.take().unwrap())
            .write_all(dockerfile.as_bytes())
            .unwrap();
        let build = build.wait_with_output().unwrap();
        assert!(build.status.success(), "{build:?}");
        String::from_utf8(build.stdout).unwrap().trim().to_owned()
    });
    Image::try_from(id.clone()).unwrap()
}

/// Runs `scenario` against a new fixture, and checks what it reported against `expect`.
fn check(name: &str, scenario: impl FnOnce(&Fixture) -> Vec<String>, expect: &Expect) {
    let fixture = Fixture::new(name);
    expect.assert_eq(&scenario(&fixture).join("\n"));
}

fn absolute(path: &str) -> AbsolutePath {
    AbsolutePath::try_from(path.to_owned()).unwrap()
}

fn relative(path: &str) -> RelativePath {
    RelativePath::try_from(path.to_owned()).unwrap()
}

/// Runs `script` as the n-th command, in `/pumpkin/home`, with its output in `/pumpkin/log`,
/// and returns how it exited.
fn sh(sandbox: &Container, n: u32, script: &str) -> ExitStatus {
    let launch = Launch {
        argv: Argv::try_from(["sh", "-c", script].map(str::to_owned).to_vec()).unwrap(),
        env: Env::try_from(vec![
            EnvVar::try_from("HOME=/pumpkin/home".to_owned()).unwrap(),
        ])
        .unwrap(),
        cwd: absolute("/pumpkin/home"),
        stdout: absolute(&format!("/pumpkin/log/{n}.stdout")),
        stderr: absolute(&format!("/pumpkin/log/{n}.stderr")),
    };
    let session = sandbox.session();
    let mut process = session.start(launch).unwrap();
    let status = session.wait(&mut process, Duration::from_secs(30));
    status.unwrap().unwrap()
}

/// Collects the sandbox's `dir` into the fixture's directory and returns the files' contents.
fn collect(fixture: &Fixture, sandbox: &Container, dir: &str) -> Vec<String> {
    let into = fixture.dir.join(format!("collected-{dir}"));
    let limits = Limits {
        bytes: 1 << 20,
        entries: 100,
    };
    let from = absolute(&format!("/pumpkin/{dir}"));
    sandbox.session().collect(&from, &into, limits).unwrap();
    let mut lines = Vec::new();
    list(&into, &into, &mut lines);
    lines
}

fn list(base: &Path, dir: &Path, lines: &mut Vec<String>) {
    let mut paths: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            list(base, &path, lines);
        } else {
            let name = path.strip_prefix(base).unwrap().display();
            let contents = fs::read_to_string(&path).unwrap();
            lines.push(format!(
                "{name}: {}",
                contents.trim_end().replace('\n', "; ")
            ));
        }
    }
}

/// The commands of the container's processes, from the host, by process ID.
fn processes(sandbox: &Container) -> String {
    let top = docker()
        .args(["top", sandbox.id(), "-o", "pid,args"])
        .output()
        .unwrap();
    let top = String::from_utf8(top.stdout).unwrap();
    let commands = top.lines().skip(1).map(|line| {
        let (_pid, command) = line.trim_start().split_once(' ').unwrap();
        command.trim_start().to_owned()
    });
    commands.collect::<Vec<_>>().join(", ")
}

/// What a judge does: reads its payload, writes a result and evidence, and exits, leaving
/// processes behind that freezing kills: one orphaned to `sandboxd`, and one in a session of
/// its own, outside its command's process group.
#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn judge() {
    check(
        "judge",
        |fixture| {
            let sandbox = fixture.start(&fixture.run, "judge");
            let (slot, payload) = (relative("in/payload"), relative("in/payload/raw"));
            let put = [Put::Dir(&slot), Put::File(&payload, b"ledger")];
            sandbox.session().put(&absolute("/pumpkin"), &put).unwrap();
            let script = r#"
                printf '{"bytes": %s}' "$(wc -c < ../in/payload/raw)" > ../out/result.json
                mkdir ../out/evidence && echo kept > ../out/evidence/evidence.txt
                echo "user $(id -u):$(id -g) home=$HOME in $(pwd)"
                (sleep 600 &); setsid sleep 600 &
                exit 3"#;
            let mut lines = vec![format!("{:?}", sh(&sandbox, 1, script))];
            lines.push(format!("before freezing: {}", processes(&sandbox)));
            sandbox.session().freeze().unwrap();
            lines.push(format!("after freezing: {}", processes(&sandbox)));
            lines.extend(collect(fixture, &sandbox, "out"));
            lines.extend(collect(fixture, &sandbox, "log"));
            lines
        },
        &expect![[r#"
            Code(3)
            before freezing: /.pumpkin/sandboxd /.pumpkin/gateway/s, sleep 600, sleep 600
            after freezing: /.pumpkin/sandboxd /.pumpkin/gateway/s
            evidence/evidence.txt: kept
            result.json: {"bytes": 6}
            1.stderr: 
            1.stdout: user 4242:4243 home=/pumpkin/home in /pumpkin/home"#]],
    );
}

/// What the role's code can reach, seen from inside the container, and the container's log,
/// which `sandboxd` alone writes, seen from the host.
#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn confinement() {
    check(
        "confinement",
        |fixture| {
            let sandbox = fixture.start(&fixture.run, "confined");
            let script = r#"
                echo "interfaces: $(ls /sys/class/net)"
                nc -w 2 1.1.1.1 80 </dev/null >/dev/null 2>&1
                echo "outbound connect: exit $?"
                grep -E '^(CapEff|NoNewPrivs)' /proc/self/status
                echo "uid=$(id -u) gid=$(id -g)"
                for file in /tmp/x /.pumpkin/sandboxd /workspace/x /pumpkin/home/x /proc/1/fd/1; do
                    if echo > $file 2>/dev/null; then echo "$file: writable"; else echo "$file: refused"; fi
                done"#;
            let status = sh(&sandbox, 1, script);
            let mut lines = vec![format!("{status:?}")];
            lines.extend(collect(fixture, &sandbox, "log"));
            let from_host = |args: &[&str]| {
                let output = docker().args(args).arg(sandbox.id()).output();
                let output = output.unwrap();
                let (stdout, stderr) = (output.stdout, output.stderr);
                String::from_utf8([stdout, stderr].concat()).unwrap()
            };
            let config = from_host(&["inspect", "--format", "{{json .HostConfig.LogConfig}}"]);
            lines.push(format!("log config: {}", config.trim_end()));
            lines.push(format!("log: {:?}", from_host(&["logs", "--tail", "20"])));
            lines
        },
        &expect![[r#"
            Code(0)
            1.stderr: sh: can't create /tmp/x: Read-only file system; sh: can't create /.pumpkin/sandboxd: Read-only file system; sh: can't create /proc/1/fd/1: Permission denied
            1.stdout: interfaces: lo; outbound connect: exit 1; CapEff:	0000000000000000; NoNewPrivs:	1; uid=4242 gid=4243; /tmp/x: refused; /.pumpkin/sandboxd: refused; /workspace/x: writable; /pumpkin/home/x: writable; /proc/1/fd/1: refused
            log config: {"Type":"json-file","Config":{"max-file":"1","max-size":"1048576"}}
            log: """#]],
    );
}

/// What the provider reports of the engine before anything starts, but its versions, and how
/// it refuses a context name `docker` wouldn't take, or one that doesn't exist.
#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn report() {
    check(
        "report",
        |_| {
            let Report {
                context,
                endpoint,
                client_version,
                engine_version,
                client_proxies,
                cgroup_version,
                cgroup_driver,
                storage_driver,
                containerd_store,
                security,
                kernel,
                os,
            } = engine().report().clone();
            let Security {
                rootless,
                seccomp_profile,
                apparmor,
                userns,
            } = security;
            let named = [client_version, engine_version, kernel, os];
            let missing = ContextName::try_from("pumpkin-missing".to_owned()).unwrap();
            let missing = Engine::connect(missing).unwrap_err().to_string();
            let (wrong, _) = missing.split_once(": ").unwrap();
            let (_, todo) = missing.rsplit_once(": ").unwrap();
            vec![
                format!("context: {} at {endpoint}", context.as_str()),
                format!(
                    "versions, kernel and OS named: {}",
                    named.iter().all(|named| !named.is_empty())
                ),
                format!(
                    "proxies in the client's config.json: {}",
                    if client_proxies { "set" } else { "not set" }
                ),
                format!("rootless: {rootless}"),
                format!("cgroup: {cgroup_version}, {cgroup_driver}"),
                format!("storage: {storage_driver}, containerd store: {containerd_store}"),
                format!("seccomp: {seccomp_profile:?}, AppArmor: {apparmor}, userns: {userns}"),
                format!("{wrong}: …: {todo}"),
                ContextName::try_from("-v".to_owned()).unwrap_err(),
            ]
        },
        &expect![[r#"
            context: default at unix:///var/run/docker.sock
            versions, kernel and OS named: true
            proxies in the client's config.json: not set
            rootless: false
            cgroup: 2, systemd
            storage: overlayfs, containerd store: true
            seccomp: Some("builtin"), AppArmor: true, userns: false
            the Docker context `pumpkin-missing` can't be read: …: name one that `docker context ls` lists
            `-v` is not a valid Docker context name: use a letter or digit, then at least one more letter, digit, `_`, `.`, `+` or `-`, as `docker context ls` lists them"#]],
    );
}

/// Destroying a sandbox removes its container, and cleaning up a run removes the rest of its
/// containers and no others.
#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn cleanup() {
    check(
        "cleanup",
        |fixture| {
            let mut lines = Vec::new();
            let first = fixture.start(&fixture.run, "first");
            let _second = fixture.start(&fixture.run, "second");
            let _third = fixture.start(&fixture.run, "third");
            let _other = fixture.start(&fixture.other_run, "other");
            first.destroy().unwrap();
            lines.push(format!(
                "listed after destroying one: {}",
                engine().list(&fixture.run).unwrap().len()
            ));
            let removed = engine().destroy_all(&fixture.run).unwrap();
            lines.push(format!("removed: {}", removed.len()));
            lines.push(format!(
                "left: {}",
                engine().list(&fixture.run).unwrap().len()
            ));
            let others = engine().list(&fixture.other_run).unwrap();
            lines.push(format!("other run's: {}", others.len()));
            lines
        },
        &expect![[r#"
            listed after destroying one: 2
            removed: 2
            left: 0
            other run's: 1"#]],
    );
}

/// A container that Docker creates but can't start is removed, and its name is free again.
#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn failed_start() {
    check(
        "failed",
        |fixture| {
            let not_executable = fixture.dir.join("sandboxd");
            fs::write(&not_executable, "not a program").unwrap();
            let mut lines = Vec::new();
            for attempt in 1..=2 {
                let started = fixture.try_start(&fixture.run, "failed", &not_executable);
                lines.push(format!("attempt {attempt} failed: {}", started.is_err()));
            }
            lines.push(format!(
                "left: {}",
                engine().list(&fixture.run).unwrap().len()
            ));
            lines
        },
        &expect![[r#"
            attempt 1 failed: true
            attempt 2 failed: true
            left: 0"#]],
    );
}
