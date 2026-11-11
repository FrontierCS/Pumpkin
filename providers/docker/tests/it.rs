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

use std::collections::BTreeMap;
use std::io::Write;
use std::num::NonZeroU64;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};
use std::{fs, thread};

use docker::{BuildContext, Built, ContextName, Engine, Recipe, Report, Security};
use expect_test::{Expect, expect};
use gateway::{Arrival, IncomingModelRequest, Listener, Op, Process, Put, Session};
use provider::Provider;
use schema::ir::{AbsolutePath, Argv, Env, EnvVar, Image, Name, Resources, User};
use schema::provider::{
    Create, Created, Dial, Listed, Local, Published, Role, RunTag, SOCKET_NAME, SandboxId,
};
use schema::sandbox::frame::MAX_MESSAGE_BYTES;
use schema::sandbox::op::{
    Failure, FailureCode, JobState, JudgeReply, OpReply, OpsReply, Remaining, Returned,
    StatusReply, SubmitReply, SubmitState,
};
use schema::sandbox::tree::Limits;
use schema::sandbox::{
    ExitStatus, Launch, MAX_MODEL_REQUEST_BYTES, MAX_MODEL_REQUESTS_IN_FLIGHT,
    MAX_OP_PAYLOAD_BYTES, MODEL_PORT, OpPayloadLimit, RelativePath, dir_mode, file_mode,
};
use stdx::process::Environment;
use stdx::{Context, Deadline};
use tempfile::TempDir;

/// A small public image, pinned by digest.
// Repeated in `crates/runtime/tests/it/docker.rs`, `crates/pumpkin/tests/it.rs` and
// `crates/harness/tests/it/docker.rs`: change all four.
const BUSYBOX: &str =
    "busybox@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e";

/// A request's deadline, which only a test that hangs reaches, so no outcome rests on it.
fn hung() -> Deadline {
    Deadline::after(Duration::from_secs(60))
}

/// The environment of one test: its run tags and a directory for sockets and collected files.
/// Dropping it removes the containers of its runs, after showing their logs if the test failed.
struct Fixture {
    sandboxd: PathBuf,
    dir: TempDir,
    run: RunTag,
    other_run: RunTag,
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
        let tag = |suffix: &str| format!("pk-{}-{name}{suffix}", std::process::id());
        let tag = |suffix: &str| RunTag::try_from(tag(suffix)).unwrap();
        // Private to this user, as the parent of every socket directory must be.
        let dir = tempfile::Builder::new()
            .prefix("docker-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        Self {
            sandboxd: PathBuf::from(sandboxd).canonicalize().unwrap(),
            dir,
            run: tag(""),
            other_run: tag("-other"),
        }
    }

    /// Starts a passive sandbox for role `role`, which runs as 4242:4243.
    fn start(&self, run: &RunTag, role: &str) -> Started {
        self.try_start(run, role, &self.sandboxd, image(), None, false)
            .unwrap()
    }

    /// Starts a sandbox for role `role` as the runtime does: creates what
    /// [`publish`](Self::publish) gives, and accepts its session where it dials, set up by the
    /// startup the provider declares, taking ops within `ops` if it gives a limit, and model
    /// requests if `model`, which has the sandbox serve the model endpoint. A sandbox whose
    /// session fails is deleted, and the error quotes its console.
    fn try_start(
        &self,
        run: &RunTag,
        role: &str,
        sandboxd: &Path,
        image: Image,
        ops: Option<OpPayloadLimit>,
        model: bool,
    ) -> Result<Started, String> {
        let (create, listener) = self.publish(run, role, sandboxd, image, ops.is_some(), model)?;
        let by = startup();
        let Created { id } = provider().create(&create)?;
        let (sender, arrivals) = mpsc::channel();
        let accepted = listener.accept(by, ops, model, sender, |arrival| arrival);
        let set_up = accepted
            .map_err(|error| error.to_string())
            .and_then(|mut session| {
                session
                    .set_up(by)
                    .context(format_args!("sandbox {}", id.as_str()))?;
                Ok(session)
            });
        match set_up {
            Ok(session) => Ok(Started {
                id,
                session,
                arrivals,
            }),
            Err(error) => {
                let console = provider().console(run, &id).unwrap();
                assert_eq!(provider().delete(run, &[id]).unwrap(), []);
                Err(format!(
                    "{error}; its console ends: {}",
                    console.text.trim()
                ))
            }
        }
    }

    /// Publishes `image` and `sandboxd` through the provider, and returns the create of a
    /// sandbox of run `run` for role `role`, which runs them as 4242:4243, makes ops if `ops`
    /// and serves the model endpoint if `model`, and the listener where it dials.
    fn publish(
        &self,
        run: &RunTag,
        role: &str,
        sandboxd: &Path,
        image: Image,
        ops: bool,
        model: bool,
    ) -> Result<(Create, Listener), String> {
        let dir = self.dir.path().join(role);
        fs::create_dir_all(&dir).unwrap();
        let listener = Listener::bind(dir.join(SOCKET_NAME)).unwrap();
        let role = Role {
            name: Name::try_from(role.to_owned()).unwrap(),
            user: serde_json::from_str::<User>(r#"{"uid": 4242, "gid": 4243}"#).unwrap(),
            resources: serde_json::from_value::<Resources>(serde_json::json!({
                "cpu_millis": 500,
                "memory_bytes": 256 << 20,
            }))
            .unwrap(),
        };
        let path = sandboxd.to_owned();
        let published = provider().publish(&[Local::Role { image }, Local::Sandboxd { path }])?;
        let [image, sandboxd] = <[Published; 2]>::try_from(published).unwrap();
        let create = Create {
            run: run.clone(),
            role,
            image,
            sandboxd,
            ops,
            model,
            dial: Dial::Socket { dir },
        };
        Ok((create, listener))
    }
}

/// A sandbox that the fixture started, its session, and what arrives on it. The fixture's drop
/// deletes it.
#[derive(Debug)]
struct Started {
    id: SandboxId,
    session: Session,
    arrivals: Receiver<Arrival>,
}

impl Started {
    fn id(&self) -> &str {
        self.id.as_str()
    }

    fn session(&mut self) -> &mut Session {
        &mut self.session
    }
}

impl Drop for Fixture {
    // Nothing here panics, since a panic while a failed test unwinds would abort the test
    // binary and hide the other tests' results: failures are printed instead. Without an
    // engine, no test started a container.
    fn drop(&mut self) {
        if let Ok(engine) = connected() {
            let provider: &dyn Provider = engine;
            for run in [&self.run, &self.other_run] {
                if thread::panicking()
                    && let Err(error) = show_logs(run)
                {
                    eprintln!("the logs of run {} can't be shown: {error}", run.as_str());
                }
                let listed = provider.list(run);
                let deleted = listed.and_then(|listed| provider.delete(run, &ids(listed)));
                if let Err(error) = deleted {
                    eprintln!("run {} can't be cleaned up: {error}", run.as_str());
                }
            }
        }
    }
}

/// The engine the tests reach Docker through, by the context `PUMPKIN_DOCKER_CONTEXT` names,
/// or `default`, or why it can't be reached.
fn connected() -> &'static Result<Engine, String> {
    static ENGINE: OnceLock<Result<Engine, String>> = OnceLock::new();
    ENGINE.get_or_init(|| {
        let name = std::env::var("PUMPKIN_DOCKER_CONTEXT").unwrap_or_else(|_| "default".to_owned());
        Engine::connect(ContextName::try_from(name)?)
    })
}

/// The engine the tests reach Docker through.
fn engine() -> &'static Engine {
    connected().as_ref().unwrap()
}

/// The engine, as the runtime holds a provider: what the tests start sandboxes through.
fn provider() -> &'static dyn Provider {
    engine()
}

/// The deadline of a start that begins now, by the startup the provider declares.
fn startup() -> Deadline {
    let seconds = provider().ready().capabilities.startup_seconds;
    Deadline::after(Duration::from_secs(seconds.get()))
}

/// The IDs of the sandboxes a list found.
fn ids(listed: Vec<Listed>) -> Vec<SandboxId> {
    listed.into_iter().map(|Listed { id }| id).collect()
}

/// `docker`, run as the provider runs it, against the tests' context alone.
// Repeated in `crates/runtime/tests/it/docker.rs` and
// `crates/harness/tests/it/docker.rs`: change all three.
fn docker() -> Command {
    // Only what the provider's `docker` keeps of the environment, for the reasons it gives.
    let kept = Environment::Only(&["HOME", "PATH", "DOCKER_CONFIG"]);
    let mut docker = stdx::process::command("docker", kept);
    docker.arg(format!("--context={}", engine().report().context.as_str()));
    docker
}

/// What `format` shows of each of the run's containers, running or not, as `docker ps` finds
/// them by their run's label, sorted.
fn containers(run: &RunTag, format: &str) -> Result<Vec<String>, String> {
    let filter = format!("label=pumpkin.run={}", run.as_str());
    let ps = [
        "ps",
        "--all",
        "--no-trunc",
        "--filter",
        &filter,
        "--format",
        format,
    ];
    let output = docker()
        .args(ps)
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!("`docker ps` failed: {output:?}"));
    }
    let mut lines: Vec<String> = (String::from_utf8_lossy(&output.stdout).lines())
        .map(str::to_owned)
        .collect();
    lines.sort();
    Ok(lines)
}

/// Prints the logs of the run's containers, for a test that failed.
fn show_logs(run: &RunTag) -> Result<(), String> {
    for id in containers(run, "{{.ID}}")? {
        let logs = docker().args(["logs", &id]).output();
        let logs = logs.map_err(|error| error.to_string())?;
        eprintln!(
            "logs of {id}:\n{}{}",
            String::from_utf8_lossy(&logs.stdout),
            String::from_utf8_lossy(&logs.stderr)
        );
    }
    Ok(())
}

/// The ID of the image the tests share, which the IR accepts as a pinned image: `busybox` with
/// a world-writable `/workspace`, which a role's user needs, since the writable layer is the
/// workspace, and a world-writable `VOLUME` at `/volume`, for which Docker makes each container
/// an anonymous volume. Built once, from the pinned image, and tagged, so that no build leaves
/// an image behind untagged; a build from the cache gives the same ID as the last, so test
/// processes that run at once share the image.
// Built alike in `crates/runtime/tests/it/docker.rs`: change both.
fn image() -> Image {
    static ID: OnceLock<String> = OnceLock::new();
    let id = ID.get_or_init(|| {
        let dockerfile =
            format!("FROM {BUSYBOX}\nRUN mkdir --mode=1777 /workspace /volume\nVOLUME /volume\n");
        // A provenance attestation records each build's own ID and times, so a build with one
        // gets an ID of its own; and as a build moves the tag to its ID, the containerd image
        // store keeps no image by the ID the tag had, which another test process, running at
        // once, may be using.
        let mut build = docker()
            .args(["build", "--quiet", "--provenance=false"])
            .args(["--tag=pumpkin-docker-tests:workspace", "-"])
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

/// How to run `script` as the n-th command, in `/pumpkin/home`, with its output in
/// `/pumpkin/log`. It may call `sandboxctl` if the sandbox serves ops.
fn launch(n: u32, script: &str) -> Launch {
    Launch {
        argv: Argv::try_from(["sh", "-c", script].map(str::to_owned).to_vec()).unwrap(),
        env: Env::try_from(vec![
            EnvVar::try_from("HOME=/pumpkin/home".to_owned()).unwrap(),
        ])
        .unwrap(),
        cwd: absolute("/pumpkin/home"),
        stdin: None,
        stdout: absolute(&format!("/pumpkin/log/{n}.stdout")),
        stderr: absolute(&format!("/pumpkin/log/{n}.stderr")),
    }
}

/// Runs `script` as the n-th command ([`launch`]), and returns how it exited, which it has 30 s
/// to, with no op arriving before its exit.
fn sh(sandbox: &mut Started, n: u32, script: &str) -> ExitStatus {
    let process = sandbox.session().start(launch(n, script), hung()).unwrap();
    match sandbox.arrivals.recv_timeout(Duration::from_secs(30)) {
        Ok(Arrival::Exited {
            process: exited,
            status,
        }) if exited == process => status,
        arrived => panic!("the command's exit was to arrive within 30 s, not {arrived:?}"),
    }
}

/// Waits for `process` to exit as the session's owner does, answering each op that arrives
/// meanwhile ([`reply`]). Returns how it exited, which it has 30 s to, and what each op asked.
fn serve(sandbox: &mut Started, process: Process, payload: &[u8]) -> (ExitStatus, Vec<String>) {
    let deadline = Deadline::after(Duration::from_secs(30));
    let mut asked = Vec::new();
    loop {
        let left = deadline.remaining();
        match left.and_then(|left| sandbox.arrivals.recv_timeout(left).ok()) {
            Some(Arrival::Exited {
                process: exited,
                status,
            }) if exited == process => return (status, asked),
            Some(arrival) => asked.push(reply(&mut sandbox.session, arrival, payload)),
            None => panic!("the command ran on for 30 s, having made ops that asked {asked:?}"),
        }
    }
}

/// Collects the sandbox's `dir` into the fixture's directory and returns the files' contents.
fn collect(fixture: &Fixture, sandbox: &mut Started, dir: &str) -> Vec<String> {
    let into = fixture.dir.path().join(format!("collected-{dir}"));
    let limits = Limits {
        bytes: 1 << 20,
        entries: 100,
    };
    let from = absolute(&format!("/pumpkin/{dir}"));
    sandbox
        .session()
        .collect(&from, &into, limits, hung())
        .unwrap();
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

/// The commands of the container's processes, from the host, by process ID, listed every
/// 10 ms until `done` holds of the list or 30 s pass.
fn processes(sandbox: &Started, done: impl Fn(&str) -> bool) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let top = docker()
            .args(["top", sandbox.id(), "-o", "pid,args"])
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

#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn judge() {
    check(
        "judge",
        |fixture| {
            let mut sandbox = fixture.start(&fixture.run, "judge");
            let (slot, payload) = (relative("in/payload"), relative("in/payload/raw"));
            let put = [
                Put::Dir(&slot, dir_mode()),
                Put::File(&payload, file_mode(), b"ledger"),
            ];
            sandbox
                .session()
                .put(&absolute("/pumpkin"), &put, hung())
                .unwrap();
            let script = r#"
                printf '{"bytes": %s}' "$(wc -c < ../in/payload/raw)" > ../out/result.json
                mkdir ../out/evidence && echo kept > ../out/evidence/evidence.txt
                echo "user $(id -u):$(id -g) home=$HOME in $(pwd)"
                (sleep 600 &); setsid sleep 600 &
                exit 3"#;
            let mut lines = vec![format!("{:?}", sh(&mut sandbox, 1, script))];
            // The script exits without waiting for its sleeps, which may not run yet: until
            // then, each shows as the command that starts it.
            let sleeping = |listed: &str| listed.split(", ").filter(|c| *c == "sleep 600").count();
            let before = processes(&sandbox, |listed| sleeping(listed) == 2);
            lines.push(format!("before freezing: {before}"));
            sandbox.session().freeze(hung()).unwrap();
            // Freezing returns once the processes it kills are gone.
            let after = processes(&sandbox, |_| true);
            lines.push(format!("after freezing: {after}"));
            lines.extend(collect(fixture, &mut sandbox, "out"));
            lines.extend(collect(fixture, &mut sandbox, "log"));
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

#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn confinement() {
    check(
        "confinement",
        |fixture| {
            let mut sandbox = fixture.start(&fixture.run, "confined");
            let script = r#"
                echo "interfaces: $(ls /sys/class/net)"
                nc -w 2 1.1.1.1 80 </dev/null >/dev/null 2>&1
                echo "outbound connect: exit $?"
                echo "TCP listeners: $(awk '$4 == "0A"' /proc/net/tcp /proc/net/tcp6 | wc -l)"
                grep -E '^(Cap|NoNewPrivs)' /proc/self/status
                echo "uid=$(id -u) gid=$(id -g)"
                echo "/.pumpkin holds:" $(ls /.pumpkin)
                echo "/pumpkin holds:" $(ls -A /pumpkin)
                ln -s /.pumpkin/sandboxd sandboxctl && echo "sandboxctl ops: $(./sandboxctl ops 2>&1)"
                for limit in cpu.max memory.max memory.swap.max pids.max; do
                    echo "$limit: $(cat /sys/fs/cgroup/$limit)"
                done
                echo "core file size: $(ulimit -c)"
                awk '$2 ~ /^\/(pumpkin|tmp|dev\/shm|dev\/mqueue)$/ { print $3 " at " $2 }' /proc/mounts | sort
                echo "/pumpkin: $(stat -c '%u:%g %a' /pumpkin), $(df -k /pumpkin | awk 'NR == 2 { print $2 }') KiB"
                printf '#!/bin/sh\necho ran\n' > /pumpkin/home/program && chmod +x /pumpkin/home/program
                echo "a program in /pumpkin: $(/pumpkin/home/program 2>&1)"
                for file in /tmp/x /.pumpkin/sandboxd /.pumpkin/gateway/x /etc/hosts /workspace/x \
                    /volume/x /pumpkin/home/x /dev/shm/x /proc/1/fd/1; do
                    if echo > $file 2>/dev/null; then echo "$file: writable"; else echo "$file: refused"; fi
                done
                touch /dev/mqueue/x && echo "/dev/mqueue/x: made"
                cat /proc/self/uid_map > /pumpkin/out/uid_map"#;
            let status = sh(&mut sandbox, 1, script);
            let mut lines = vec![format!("{status:?}")];
            lines.extend(collect(fixture, &mut sandbox, "log"));
            // Rootless Docker and user namespaces each map the container's users to others on
            // the host; a rootful engine without them maps every user to itself.
            collect(fixture, &mut sandbox, "out");
            let uid_map =
                fs::read_to_string(fixture.dir.path().join("collected-out/uid_map")).unwrap();
            let identity = uid_map.split_whitespace().eq(["0", "0", "4294967295"]);
            let Security {
                rootless,
                seccomp_profile: _,
                apparmor: _,
                userns,
            } = engine().report().security;
            lines.push(format!(
                "rootless or userns, exactly when the uid map isn't the identity: {}",
                (rootless || userns) != identity
            ));
            let from_host = |args: &[&str]| {
                let output = docker().args(args).arg(sandbox.id()).output();
                let output = output.unwrap();
                let (stdout, stderr) = (output.stdout, output.stderr);
                String::from_utf8([stdout, stderr].concat()).unwrap()
            };
            let pinned = "log config: {{json .HostConfig.LogConfig}}\n\
                          init: {{json .HostConfig.Init}}\n\
                          healthcheck: {{json .Config.Healthcheck}}";
            let inspected = from_host(&["inspect", "--format", pinned]);
            lines.extend(inspected.lines().map(str::to_owned));
            lines.push(format!("log: {:?}", from_host(&["logs", "--tail", "20"])));
            lines
        },
        &expect![[r#"
            Code(0)
            1.stderr: sh: can't create /tmp/x: Read-only file system; sh: can't create /.pumpkin/sandboxd: Read-only file system; sh: can't create /.pumpkin/gateway/x: Read-only file system; sh: can't create /etc/hosts: Permission denied; sh: can't create /proc/1/fd/1: Permission denied
            1.stdout: interfaces: lo; outbound connect: exit 1; TCP listeners: 0; CapInh:	0000000000000000; CapPrm:	0000000000000000; CapEff:	0000000000000000; CapBnd:	0000000000000000; CapAmb:	0000000000000000; NoNewPrivs:	1; uid=4242 gid=4243; /.pumpkin holds: gateway sandboxd; /pumpkin holds: home in log out; sandboxctl ops: sandboxctl: sandboxd at /pumpkin/sandboxctl/socket can't be reached: No such file or directory (os error 2): run sandboxctl in a sandbox that serves ops; cpu.max: 50000 100000; memory.max: 268435456; memory.swap.max: 0; pids.max: 4096; core file size: 0; mqueue at /dev/mqueue; tmpfs at /dev/shm; tmpfs at /pumpkin; tmpfs at /tmp; /pumpkin: 4242:4243 755, 1048576 KiB; a program in /pumpkin: ran; /tmp/x: refused; /.pumpkin/sandboxd: refused; /.pumpkin/gateway/x: refused; /etc/hosts: refused; /workspace/x: writable; /volume/x: writable; /pumpkin/home/x: writable; /dev/shm/x: writable; /proc/1/fd/1: refused; /dev/mqueue/x: made
            rootless or userns, exactly when the uid map isn't the identity: true
            log config: {"Type":"json-file","Config":{"max-file":"1","max-size":"1048576"}}
            init: false
            healthcheck: {"Test":["NONE"]}
            log: """#]],
    );
}

#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn report() {
    check(
        "report",
        |_| {
            let Report {
                context: _,
                endpoint,
                client_version,
                engine_version,
                security: _,
                kernel,
                os,
            } = engine().report();
            let named = [endpoint, client_version, engine_version, kernel, os];
            let ready = serde_json::to_value(provider().ready()).unwrap();
            let reported: Vec<&String> = ready["report"].as_object().unwrap().keys().collect();
            let mut lines = vec![
                format!(
                    "endpoint, versions, kernel and OS named: {}",
                    named.iter().all(|named| !named.is_empty())
                ),
                format!(
                    "ready: a report of {reported:?}, and {}",
                    ready["capabilities"]
                ),
                ContextName::try_from(String::new()).unwrap_err(),
            ];
            // `docker context inspect` mustn't take `--help` for its own option, which prints its
            // help and succeeds.
            for name in ["pumpkin-missing", "--help"] {
                let refused = Engine::connect(ContextName::try_from(name.to_owned()).unwrap());
                let refused = refused.unwrap_err();
                let (wrong, _) = refused.split_once(": ").unwrap();
                let (_, todo) = refused.rsplit_once(": ").unwrap();
                lines.push(format!("{wrong}: …: {todo}"));
            }
            lines
        },
        &expect![[r#"
            endpoint, versions, kernel and OS named: true
            ready: a report of ["client_version", "context", "endpoint", "engine_version", "kernel", "os", "security"], and {"startup_seconds":540}
            the Docker context's name is empty: name one that `docker context ls` lists
            the Docker context `pumpkin-missing` can't be read: …: name one that `docker context ls` lists
            the Docker context `--help` can't be read: …: name one that `docker context ls` lists"#]],
    );
}

#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn volumes() {
    check(
        "volumes",
        |fixture| {
            let started = [("first", &fixture.run), ("other", &fixture.other_run)];
            let started = started.map(|(role, run)| (role, fixture.start(run, role)));
            let volumes = started.each_ref().map(|(_, sandbox)| volume(sandbox));
            let listed = ids(provider().list(&fixture.run).unwrap());
            let unknown = provider().delete(&fixture.run, &listed).unwrap();
            let listed = docker().args(["volume", "ls", "--quiet"]).output().unwrap();
            let listed = String::from_utf8(listed.stdout).unwrap();
            let kept = (started.iter().zip(&volumes))
                .filter(|&(_, volume)| listed.lines().any(|listed| listed == volume))
                .map(|((role, _), _)| role);
            vec![
                format!("deleting what is listed: unknown {unknown:?}"),
                format!("volumes left: {:?}", kept.collect::<Vec<_>>()),
            ]
        },
        &expect![[r#"
            deleting what is listed: unknown []
            volumes left: ["other"]"#]],
    );
}

/// The name of the anonymous volume Docker made for the sandbox's `/volume`.
fn volume(sandbox: &Started) -> String {
    let format = r#"{{range .Mounts}}{{if eq .Destination "/volume"}}{{.Name}}{{end}}{{end}}"#;
    let inspect = ["inspect", "--format", format, sandbox.id()];
    let output = docker().args(inspect).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let name = String::from_utf8(output.stdout).unwrap().trim().to_owned();
    assert!(!name.is_empty(), "the sandbox has no volume at /volume");
    name
}

#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn failed_start() {
    check(
        "failed",
        |fixture| {
            let not_executable = fixture.dir.path().join("sandboxd");
            fs::write(&not_executable, "not a program").unwrap();
            let missing = fixture.dir.path().join("absent");
            // `--mount` would read a comma as the start of another option.
            let comma = fixture.dir.path().join("sandboxd,readonly=false");
            fs::write(&comma, "").unwrap();
            let unpulled = Image::try_from(format!("busybox@sha256:{}", "0".repeat(64))).unwrap();
            let attempts = [
                ("missing", missing.as_path(), image()),
                ("comma", comma.as_path(), image()),
                ("unexecutable", not_executable.as_path(), image()),
                ("unpulled", fixture.sandboxd.as_path(), unpulled),
            ];
            let mut lines = Vec::new();
            for (role, sandboxd, image) in attempts {
                let failed = fixture.try_start(&fixture.run, role, sandboxd, image, None, false);
                lines.push(format!("{role}: {}", redact(fixture, &failed.unwrap_err())));
            }
            let left = containers(&fixture.run, "{{.ID}}").unwrap();
            lines.push(format!("left: {left:?}"));
            lines
        },
        &expect![[r#"
            missing: publishing `sandboxd` failed: <dir>/absent can't be mounted at /.pumpkin/sandboxd: No such file or directory (os error 2): name a path that exists
            comma: publishing `sandboxd` failed: <dir>/sandboxd,readonly=false can't be mounted at /.pumpkin/sandboxd: name a path of UTF-8 without commas
            unexecutable: `docker start …: fix what Docker reports
            unpulled: the engine of context `<context>` has no image busybox@sha256:0000000000000000000000000000000000000000000000000000000000000000: build or load it there first, as the provider pulls none
            left: []"#]],
    );
}

/// `message`, with the fixture's directory as `<dir>`, the tests' context as `<context>`, and
/// `…` for what varies between runs or hosts in a failed `docker start`: the container's ID, and
/// what Docker reports, which comes from the container runtime, and varies with the runtime and
/// its version.
fn redact(fixture: &Fixture, message: &str) -> String {
    let context = format!("context `{}`", engine().report().context.as_str());
    let message = (message.replace(&fixture.dir.path().display().to_string(), "<dir>"))
        .replace(&context, "context `<context>`");
    let (from, to) = ("`docker start ", ": fix what Docker reports");
    if let Some((before, rest)) = message.split_once(from)
        && let Some((_, after)) = rest.split_once(to)
    {
        return format!("{before}{from}…{to}{after}");
    }
    message
}

#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn builds() {
    check(
        "builds",
        |fixture| {
            let mut builds = Builds::new(fixture);
            let dir = fixture.dir.path().join("context");
            let mut lines = vec![format!("built: {}", builds.build("Dockerfile"))];
            let [first] = <[String; 1]>::try_from(builds.tags(&builds.seen[0])).unwrap();
            run(&["image", "rm", &first]);
            put(&dir, "file", "two", 0o644);
            lines.push(format!("another content: {}", builds.build("Dockerfile")));
            let second = builds.seen[1].clone();
            run(&["tag", &second, &first]);
            put(&dir, "file", "one", 0o644);
            let found = builds.build("Dockerfile");
            lines.push(format!("the first inputs, their tag moved to B: {found}"));
            put(&dir, "file", "one", 0o755);
            lines.push(format!("another mode: {}", builds.build("Dockerfile")));
            lines.push(format!(
                "another recipe: {}",
                builds.build("other/Dockerfile")
            ));
            let (context, dockerfile) = (builds.context(), Recipe::File(relative("Dockerfile")));
            let base = engine().build(&context, &dockerfile, &[], &builds.repository);
            let base = base.unwrap();
            let on_base = Recipe::Text(format!("FROM {}\nCOPY file /on-base\n", base.tag()));
            let built = builds.build_in(&context, &on_base, &[&base]);
            lines.push(format!("a text on its base: {built}"));
            let refused = builds.build_in(&context, &on_base, &[]);
            let refused = refused.replace(base.tag(), "<base>");
            lines.push(format!("built before, now without its base: {refused}"));
            lines.push(format!(
                "an absent recipe: {}",
                builds.build("absent/Dockerfile")
            ));
            std::os::unix::fs::symlink("file", dir.join("link")).unwrap();
            lines.push(format!("a symbolic link: {}", builds.build("Dockerfile")));
            fs::remove_file(dir.join("link")).unwrap();
            for (what, name) in [("a space", "a b"), ("a tab", "a\tb")] {
                put(&dir, name, "", 0o644);
                lines.push(format!(
                    "a name with {what}: {}",
                    builds.build("Dockerfile")
                ));
                fs::remove_file(dir.join(name)).unwrap();
            }
            // Without `id`, the inputs are the same in every process, so the tag the refusal
            // names pins the digest's text; the lookup fails before anything is built.
            fs::remove_file(dir.join("id")).unwrap();
            let repository = "pumpkin-docker-tests/Builds";
            let uppercase = engine().build(&context, &dockerfile, &[], repository);
            let refused = redact(fixture, &uppercase.unwrap_err());
            lines.push(format!("an uppercase repository: {refused}"));
            lines
        },
        &expect![[r#"
            built: A, tagged :1
            another content: B, tagged :2
            the first inputs, their tag moved to B: B, tagged :1 :2
            another mode: C, tagged :3
            another recipe: C, tagged :3 :4
            a text on its base: D, tagged :5
            built before, now without its base: Dockerfile:1: `<base>` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, `scratch`, an earlier stage or a base the build is given
            an absent recipe: `absent/Dockerfile` is no regular file in the build's context: name the build's recipe by its path there
            a symbolic link: <dir>/context/link is neither a directory nor a regular file: give a build's context only those
            a name with a space: `<dir>/context/a b` holds whitespace or a control character: name a build context's files without them
            a name with a tab: `<dir>/context/a\tb` holds whitespace or a control character: name a build context's files without them
            an uppercase repository: the image tagged `pumpkin-docker-tests/Builds:ae5393eb92660d45221c68f0974539eb1151a715d549a35397299be084351a30` can't be looked up: `docker image inspect` failed with exit status: 1: Error response from daemon: invalid reference format: repository name (pumpkin-docker-tests/Builds) must be lowercase: fix what Docker reports"#]],
    );
}

#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn mounts() {
    check(
        "mounts",
        |fixture| {
            let mut builds = Builds::new(fixture);
            let shared = fixture.dir.path().join("shared");
            put(&shared, "tool", "one", 0o755);
            let dir = builds.context().dir;
            let at = |path: &str| BuildContext {
                dir: dir.clone(),
                mounts: BTreeMap::from([(relative(path), shared.clone())]),
            };
            let recipe = Recipe::Text("FROM scratch\nCOPY file id shared/tool /\n".to_owned());
            let built = builds.build_in(&at("shared"), &recipe, &[]);
            let mut lines = vec![format!("mounted: {built}")];
            put(&shared, "tool", "two", 0o755);
            let built = builds.build_in(&at("shared"), &recipe, &[]);
            lines.push(format!("a mounted file changed: {built}"));
            for (what, path) in [
                ("over a file", "file"),
                ("in a file", "file/shared"),
                ("in nothing", "absent/shared"),
                ("at a name with a space", "a b"),
            ] {
                let refused = builds.build_in(&at(path), &recipe, &[]);
                lines.push(format!("mounted {what}: {refused}"));
            }
            lines
        },
        &expect![[r#"
            mounted: A, tagged :1
            a mounted file changed: B, tagged :2
            mounted over a file: the build's context holds `file` already, where <dir>/shared is to be mounted: mount it where the context holds nothing
            mounted in a file: the build's context holds no directory `file` for <dir>/shared to be mounted in: mount it in a directory the context holds
            mounted in nothing: the build's context holds no directory `absent` for <dir>/shared to be mounted in: mount it in a directory the context holds
            mounted at a name with a space: `a b` holds whitespace or a control character: mount <dir>/shared at a path without them"#]],
    );
}

#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn modes() {
    check(
        "modes",
        |fixture| {
            let mut builds = Builds::new(fixture);
            let dir = fixture.dir.path().join("context");
            // Built only if the image holds each file with the mode the stage gives it.
            let recipe = Recipe::Text(format!(
                "FROM {BUSYBOX}\nCOPY . /context\nRUN cd /context && test \"$(stat -c %a file \
                 tool other other/Dockerfile | xargs)\" = '644 755 755 644'\n"
            ));
            let mut lines = Vec::new();
            for (what, [file, tool, dirs]) in [
                ("group-writable", [0o664, 0o775, 0o775]),
                ("for their owners alone", [0o600, 0o700, 0o700]),
                ("as git checks them out", [0o644, 0o755, 0o755]),
            ] {
                put(&dir, "file", "one", file);
                put(&dir, "tool", "", tool);
                for dir in [dir.join("other"), dir.clone()] {
                    fs::set_permissions(dir, fs::Permissions::from_mode(dirs)).unwrap();
                }
                let built = builds.build_in(&builds.context(), &recipe, &[]);
                lines.push(format!("{what}: {built}"));
            }
            lines
        },
        &expect![[r#"
            group-writable: A, tagged :1
            for their owners alone: A, tagged :1
            as git checks them out: A, tagged :1"#]],
    );
}

/// Runs `docker` with `args`, which must succeed.
fn run(args: &[&str]) {
    let output = docker().args(args).output().unwrap();
    assert!(output.status.success(), "{output:?}");
}

/// Writes `text` to the file at `path` in `dir`, with permission bits `mode`, and makes each
/// directory it needs with bits 0755, whatever this process's umask.
fn put(dir: &Path, path: &str, text: &str, mode: u32) {
    let file = dir.join(path);
    for parent in file.ancestors().skip(1) {
        if parent == dir.parent().unwrap() {
            break;
        }
        fs::create_dir_all(parent).unwrap();
        fs::set_permissions(parent, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fs::write(&file, text).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(mode)).unwrap();
}

/// The images a test builds in `repository`, by their IDs in the order they were first built,
/// and the tags it has shown, in the order they were first shown. Dropping it removes each tag
/// the images have in `repository`, and so each image no other repository tags.
struct Builds<'f> {
    fixture: &'f Fixture,
    repository: String,
    seen: Vec<String>,
    shown: Vec<String>,
}

impl<'f> Builds<'f> {
    /// The images of a test that builds with `context`, in the fixture's directory, as its
    /// context: a recipe that copies `file`, which holds `one`, and `id`, which holds this
    /// process's ID, into an empty image, at `Dockerfile` and at `other/Dockerfile`; each in a
    /// repository of this process's own.
    fn new(fixture: &'f Fixture) -> Self {
        let dir = fixture.dir.path().join("context");
        let recipe = "FROM scratch\nCOPY file id /\n";
        put(&dir, "Dockerfile", recipe, 0o644);
        put(&dir, "other/Dockerfile", recipe, 0o644);
        put(&dir, "file", "one", 0o644);
        // An image is named by its content, so another process's test that built the same
        // inputs would have the same image, which removing its last tag here would delete, even
        // between that test's build and its `docker tag`. With this process's ID, the images
        // are its own.
        put(&dir, "id", &std::process::id().to_string(), 0o644);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            fixture,
            repository: format!("pumpkin-docker-tests/builds-{}", std::process::id()),
            seen: Vec::new(),
            shown: Vec::new(),
        }
    }

    /// The fixture's `context`, with nothing mounted in it.
    fn context(&self) -> BuildContext {
        BuildContext {
            dir: Some(self.fixture.dir.path().join("context")),
            mounts: BTreeMap::new(),
        }
    }

    /// [`Builds::build_in`] of the file `recipe` of the fixture's `context`, with no bases.
    fn build(&mut self, recipe: &str) -> String {
        self.build_in(&self.context(), &Recipe::File(relative(recipe)), &[])
    }

    /// Builds `recipe` on `bases` with `context`, and shows the image, by the letter of its
    /// first build, and its tags in the repository, and the tag the build returned if it is
    /// none of them, each by the number of its first showing, as their digests cover `id`; or
    /// why the build failed.
    fn build_in(&mut self, context: &BuildContext, recipe: &Recipe, bases: &[&Built]) -> String {
        let built = engine().build(context, recipe, bases, &self.repository);
        let (image, returned) = match built {
            Ok(built) => (built.image().as_str().to_owned(), built.tag().to_owned()),
            Err(error) => return redact(self.fixture, &error),
        };
        let index = first_seen(&mut self.seen, &image);
        let letter = char::from(b'A' + u8::try_from(index).unwrap());
        // The returned tag first, so that a new tag's number doesn't rest on how tags sort.
        let returned = 1 + first_seen(&mut self.shown, &returned);
        let tags = self.tags(&image);
        let mut tags: Vec<usize> = (tags.iter())
            .map(|tag| 1 + first_seen(&mut self.shown, tag))
            .collect();
        tags.sort_unstable();
        let stray = (!tags.contains(&returned)).then(|| format!(", but returned :{returned}"));
        let tags: Vec<String> = tags.iter().map(|number| format!(":{number}")).collect();
        format!(
            "{letter}, tagged {}{}",
            tags.join(" "),
            stray.unwrap_or_default()
        )
    }

    /// The tags `image` has in the repository, sorted, or none if the engine has no such image.
    fn tags(&self, image: &str) -> Vec<String> {
        let format = "{{json .RepoTags}}";
        let inspect = ["image", "inspect", "--format", format, image];
        let Ok(output) = docker().args(inspect).output() else {
            return Vec::new();
        };
        let tags: Vec<String> = serde_json::from_slice(&output.stdout).unwrap_or_default();
        let prefix = format!("{}:", self.repository);
        let mut tags: Vec<String> = (tags.into_iter())
            .filter(|tag| tag.starts_with(&prefix))
            .collect();
        tags.sort();
        tags
    }
}

impl Drop for Builds<'_> {
    // Nothing here panics, for the reason the fixture's drop gives; a tag that can't be
    // removed is printed.
    fn drop(&mut self) {
        for image in &self.seen {
            for tag in self.tags(image) {
                let removed = docker().args(["image", "rm", &tag]).output();
                if !removed.is_ok_and(|removed| removed.status.success()) {
                    eprintln!("the test's image tag {tag} can't be removed");
                }
            }
        }
    }
}

/// The index of `item` in `list`, which gets it at its end if it lacks it.
fn first_seen(list: &mut Vec<String>, item: &str) -> usize {
    if let Some(index) = list.iter().position(|listed| listed == item) {
        return index;
    }
    list.push(item.to_owned());
    list.len() - 1
}

#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn console() {
    check(
        "console",
        |fixture| {
            // A `sandboxd` that fails as it starts, saying why.
            let stand_in = fixture.dir.path().join("stand-in-sandboxd");
            let script =
                "#!/bin/sh\necho 'sandboxd: a stand-in, which exits without dialing' >&2\n";
            fs::write(&stand_in, script).unwrap();
            fs::set_permissions(&stand_in, fs::Permissions::from_mode(0o755)).unwrap();
            let published =
                fixture.publish(&fixture.run, "stand-in", &stand_in, image(), false, false);
            let (create, _) = published.unwrap();
            let Created { id } = provider().create(&create).unwrap();
            // Docker finishes writing a container's log before it marks the container stopped,
            // which `docker wait` waits for, so the console then holds all the stand-in printed.
            let waited = docker().args(["wait", id.as_str()]).output().unwrap();
            assert!(waited.status.success(), "{waited:?}");
            let console = provider().console(&fixture.run, &id).unwrap();
            vec![format!("console: {}", console.text.trim())]
        },
        &expect![[r#"
            console: sandboxd: a stand-in, which exits without dialing"#]],
    );
}

/// A shell function, `ctl`, that runs `sandboxctl` with its arguments, and shows how it exited
/// and what it printed.
const CTL: &str = r#"
    ctl() { out=$(/.pumpkin/bin/sandboxctl "$@" 2>&1); echo "\$ sandboxctl $*: exit $?"; [ -z "$out" ] || echo "  $out"; }"#;

#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn ops() {
    check(
        "ops",
        |fixture| {
            let payload = payload();
            let limit = OpPayloadLimit::try_from(u64::try_from(payload.len()).unwrap()).unwrap();
            let (sandboxd, ops) = (&fixture.sandboxd, Some(limit));
            let sandbox = fixture.try_start(&fixture.run, "ops", sandboxd, image(), ops, false);
            let mut sandbox = sandbox.unwrap();
            let session = sandbox.session();
            let put = [Put::File(&relative("payload"), file_mode(), &payload)];
            session
                .put(&absolute("/pumpkin/home"), &put, hung())
                .unwrap();
            let script = format!(
                r#"{CTL}
                cp payload big && printf '!' >> big && ln -s payload link
                ctl ops
                /.pumpkin/bin/sandboxctl ops > /dev/full; echo "\$ sandboxctl ops > /dev/full: exit $?"
                ctl judge train payload
                ctl submit submit ../home/payload --finalize
                ctl submit submit payload
                ctl judge train link
                ctl status 1
                ctl status 2
                ctl status 3
                ctl judge train big
                echo "control socket's directory: $(ls -ld /pumpkin/sandboxctl | cut -c1-10); TCP listeners: $(awk '$4 == "0A"' /proc/net/tcp /proc/net/tcp6 | wc -l)"
                echo > /.pumpkin/bin/sandboxctl"#
            );
            let process = session.start(launch(1, &script), hung()).unwrap();
            let (status, arrived) = serve(&mut sandbox, process, &payload);
            let mut lines = vec![format!("{status:?}")];
            lines.extend(log(fixture, &mut sandbox));
            lines.extend(arrived.iter().map(|arrived| format!("owner: {arrived}")));
            lines
        },
        &expect![[r#"
            Code(1)
            $ sandboxctl ops: exit 0
              {"kind":"ops","remaining_seconds":0,"resources":{"cpu_millis":1,"memory_bytes":1},"grants":[]}
            $ sandboxctl ops > /dev/full: exit 0
            $ sandboxctl judge train payload: exit 0
              {"kind":"judge","job":1,"remaining":{"calls":1,"seconds":null}}
            $ sandboxctl submit submit ../home/payload --finalize: exit 0
              {"kind":"submit","state":"accepted","remaining_seconds":60}
            $ sandboxctl submit submit payload: exit 0
              {"kind":"submit","state":"stored","remaining_seconds":60}
            $ sandboxctl judge train link: exit 0
              {"kind":"judge","job":1,"remaining":{"calls":1,"seconds":null}}
            $ sandboxctl status 1: exit 0
              {"kind":"status","job":1,"state":"completed","returned":{"score":1},"scored":true,"remaining":{"calls":1,"seconds":null}}
            $ sandboxctl status 2: exit 2
              {"kind":"status","job":2,"state":"errored","failure":{"code":"judge_timeout","message":"the judge ran past its timeout"},"remaining":{"calls":1,"seconds":null}}
            $ sandboxctl status 3: exit 1
              {"kind":"refused","message":"there is no job 3: name one a judge call returned"}
            $ sandboxctl judge train big: exit 1
              {"kind":"refused","message":"the payload is 2621441 bytes, more than the limit of 2621440: send at most 2621440"}
            control socket's directory: drwx------; TCP listeners: 0
            stderr: sh: can't create /.pumpkin/bin/sandboxctl: Read-only file system
            owner: ops
            owner: ops
            owner: judge train: 2621440 bytes, intact: true
            owner: submit submit, finalize true: 2621440 bytes, intact: true
            owner: submit submit, finalize false: 2621440 bytes, intact: true
            owner: judge train: 2621440 bytes, intact: true
            owner: status 1
            owner: status 2
            owner: status 3
            owner: refused Judge { grant: Name("train") }, over 2621440 bytes"#]],
    );
}

#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn refused_calls() {
    check(
        "refused",
        |fixture| {
            let ops = Some(OpPayloadLimit::try_from(64).unwrap());
            let sandboxd = &fixture.sandboxd;
            let sandbox = fixture.try_start(&fixture.run, "refused", sandboxd, image(), ops, false);
            let mut sandbox = sandbox.unwrap();
            // A session for the second `sandboxd`, which it connects to and never serves.
            let second = Listener::bind(fixture.dir.path().join("refused/second")).unwrap();
            // `/proc/1/mem` is `sandboxd`'s memory, which it can't open, being non-dumpable.
            let script = format!(
                r#"{CTL}
                truncate -s {huge} huge
                mkfifo fifo
                ctl submit submit huge
                ctl judge train missing
                ctl judge train .
                ctl judge train fifo
                ctl judge train /proc/1/mem
                ctl judge train ''
                ctl judge Train huge
                ctl status soon
                ctl call train huge
                ctl status 5
                second=$(/.pumpkin/sandboxd /.pumpkin/gateway/second --ops 2>&1)
                echo "a second sandboxd: exit $?: $second""#,
                huge = MAX_OP_PAYLOAD_BYTES + 1,
            );
            let process = sandbox.session().start(launch(1, &script), hung()).unwrap();
            let (status, arrived) = serve(&mut sandbox, process, &[]);
            drop(second);
            let mut lines = vec![format!("{status:?}")];
            lines.extend(log(fixture, &mut sandbox));
            lines.extend(arrived.iter().map(|arrived| format!("owner: {arrived}")));
            lines
        },
        &expect![[r#"
            Code(0)
            $ sandboxctl submit submit huge: exit 1
              sandboxctl: /pumpkin/home/huge holds more than 67108864 bytes: send at most that
            $ sandboxctl judge train missing: exit 1
              sandboxctl: missing: No such file or directory (os error 2): name a file this user can reach
            $ sandboxctl judge train .: exit 1
              sandboxctl: /pumpkin/home is not a regular file: name one
            $ sandboxctl judge train fifo: exit 1
              sandboxctl: /pumpkin/home/fifo is not a regular file: name one
            $ sandboxctl judge train /proc/1/mem: exit 1
              sandboxctl: /proc/1/mem: Permission denied (os error 13)
            $ sandboxctl judge train : exit 1
              sandboxctl: the payload file's path is empty: name a file
            $ sandboxctl judge Train huge: exit 1
              sandboxctl: `Train` is not a valid name: use 1 to 64 characters from a-z, 0-9, `_` and `-`, starting with a letter
            $ sandboxctl status soon: exit 1
              sandboxctl: `soon` is not a job: name one by the number a judge call returned
            $ sandboxctl call train huge: exit 1
              sandboxctl: unexpected argument: "call": see `sandboxctl --help`
            $ sandboxctl status 5: exit 2
              {"kind":"status","job":5,"state":"completed","returned":{"score":null},"scored":false,"remaining":{"calls":1,"seconds":null}}
            a second sandboxd: exit 1: sandboxd: serving ops failed: the control socket's directory /pumpkin/sandboxctl can't be made: File exists (os error 17): expected nothing there yet, in a directory this user can write to
            owner: status 5"#]],
    );
}

#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn turns() {
    check(
        "turns",
        |fixture| {
            let ops = Some(OpPayloadLimit::try_from(64).unwrap());
            let sandboxd = &fixture.sandboxd;
            let sandbox = fixture.try_start(&fixture.run, "turns", sandboxd, image(), ops, false);
            let mut sandbox = sandbox.unwrap();
            // The owner holds the first op, and puts `in/held`, while the other callers arrive.
            // `sent` waits until a caller's `sandboxctl` sleeps, which it does only as it waits
            // for its answer, since connecting to a Unix socket waits only on a full backlog.
            // Once each has sent its call, the script touches `ready`, which the second command
            // waits for, and the owner answers the first op.
            let script = r#"
                exec 2>&1
                ctl=/.pumpkin/bin/sandboxctl
                sent() { until grep -q '(sandboxctl) S' /proc/$1/stat; do sleep 0.01; done; }
                $ctl status 1 & first=$!
                until [ -e ../in/held ]; do sleep 0.01; done
                kill $first; wait $first 2>/dev/null
                echo "the caller of status 1, which left with its op in flight: exit $?"
                $ctl status 4 & sent $!; kill $!; wait $! 2>/dev/null
                echo "the caller of status 4, which left as it waited its turn: exit $?"
                printf ledger > payload && cp payload swapped
                $ctl status 2 > 2.out 2>&1 & second=$!
                $ctl status 3 > 3.out 2>&1 & third=$!
                $ctl judge train swapped > swapped.out 2>&1 & fourth=$!
                sent $second; sent $third; sent $fourth
                ln -sf payload swapped
                touch ready
                wait $second; echo "status 2: exit $?: $(cat 2.out)"
                wait $third; echo "status 3: exit $?: $(cat 3.out)"
                wait $fourth; echo "judge train swapped: exit $?: $(cat swapped.out)"
                ninth=$($ctl status 9 2>&1); echo "status 9: exit $?: $ninth""#;
            let session = sandbox.session();
            let callers = session.start(launch(1, script), hung()).unwrap();
            let first = sandbox
                .arrivals
                .recv_timeout(Duration::from_secs(30))
                .unwrap();
            let held = [Put::Dir(&relative("held"), dir_mode())];
            sandbox
                .session()
                .put(&absolute("/pumpkin/in"), &held, hung())
                .unwrap();
            // No op can arrive while the first is in flight.
            sh(&mut sandbox, 2, "until [ -e ready ]; do sleep 0.01; done");
            let mut arrived = vec![reply(sandbox.session(), first, &[])];
            let (status, rest) = serve(&mut sandbox, callers, &[]);
            arrived.extend(rest);
            // The callers of status 2 and 3 may take their turns in either order.
            arrived.sort();
            let mut lines = vec![format!("{status:?}")];
            lines.extend(log(fixture, &mut sandbox));
            lines.extend(arrived.iter().map(|arrived| format!("owner: {arrived}")));
            lines
        },
        &expect![[r#"
            Code(0)
            the caller of status 1, which left with its op in flight: exit 143
            the caller of status 4, which left as it waited its turn: exit 143
            status 2: exit 2: {"kind":"status","job":2,"state":"errored","failure":{"code":"judge_timeout","message":"the judge ran past its timeout"},"remaining":{"calls":1,"seconds":null}}
            status 3: exit 1: {"kind":"refused","message":"there is no job 3: name one a judge call returned"}
            judge train swapped: exit 1: sandboxctl: /pumpkin/home/swapped: Symbolic link loop (os error 40)
            status 9: exit 1: {"kind":"refused","message":"there is no job 9: name one a judge call returned"}
            owner: status 1
            owner: status 2
            owner: status 3
            owner: status 9"#]],
    );
}

#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn model_endpoint() {
    check(
        "model",
        |fixture| {
            let ops = Some(OpPayloadLimit::try_from(64).unwrap());
            let sandboxd = &fixture.sandboxd;
            let sandbox = fixture.try_start(&fixture.run, "model", sandboxd, image(), ops, true);
            let mut sandbox = sandbox.unwrap();
            let script = format!(
                r#"
                awk '$2 ~ /:{MODEL_PORT:04X}$/ && $4 == "0A" {{ print "listening on", $2 }}' /proc/net/tcp
                post() {{ printf "$1" | nc -w 30 127.0.0.1 {MODEL_PORT} | tr -d '\r' | tr '\n' '|'; echo; }}
                for n in 1 2 3 4 5 6 7 8; do post 'POST /held HTTP/1.1\r\n\r\n' > flight-$n & held="$held $!"; done
                wait $held
                echo "the flight: $(grep -l ' 200 ' flight-* | wc -l) answered; $(grep -h ' 503 ' flight-*)"
                writing() {{ awk '$2 ~ /:{MODEL_PORT:04X}$/ && ($4 == "01" || $4 == "08") {{ found = 1 }} END {{ exit !found }}' /proc/net/tcp; }}
                late() {{ printf 'POST /late HTTP/1.1\r\n\r\n'; until [ -e ../in/late ]; do sleep 0.01; done; printf x; }}
                echo "a body past the client's buffers, with a byte sent late: $(late | nc -w 30 127.0.0.1 {MODEL_PORT} | {{ until [ -e ../in/late ] && ! writing; do sleep 0.01; done; wc -c; }}) bytes"
                post 'POST /responses HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: 13\r\n\r\n{{"model":"m"}}'
                post 'POST /responses HTTP/1.1\r\nContent-Length: 2\r\n\r\n{{}}'
                post 'GET /models HTTP/1.1\r\nAccept:  */*  \r\n\r\n'
                echo "ended without a head: [$(post 'POST /headless HTTP/1.1\r\n\r\n')]"
                echo "answered 204, then cut short: [$(post 'POST /empty HTTP/1.9\r\nContent-Length: 2\r\n\r\n{{}}')]"
                echo "answered 103, an interim status: [$(post 'POST /interim HTTP/1.1\r\n\r\n')]"
                post 'HEAD /models HTTP/1.1\r\n\r\n'
                post 'POST /responses HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n'
                post 'POST /responses HTTP/1.1\r\nContent-Length: 0\r\nContent-Length: 1\r\n\r\n'
                post 'POST /responses HTTP/1.1\r\nContent-Length: +0\r\n\r\n'
                post 'POST /responses HTTP/1.1\r\nContent-Length: 0\r\nTransfer-Encoding: chunked\r\n\r\n'
                post 'POST /responses HTTP/1.1\r\nContent-Length: {over}\r\n\r\n'
                post 'HEAD /models HTTP/1.0\r\n\r\n'
                post 'POST /responses HTTP/1.0\r\n\r\n'
                post "GET / HTTP/1.1\r\nx: $(head -c {pad} /dev/zero | tr '\0' a)""#,
                over = MAX_MODEL_REQUEST_BYTES + 1,
                // A head of 16 KiB, the endpoint's bound, with no end: all of it is read, so
                // the refusal reaches the client before the connection closes.
                pad = (16 << 10) - "GET / HTTP/1.1\r\nx: ".len(),
            );
            let session = sandbox.session();
            let process = session.start(launch(1, &script), hung()).unwrap();
            let mut asked = hold_flight(&mut sandbox);
            asked.push(respond_late(&mut sandbox));
            let (status, rest) = serve_models(&mut sandbox, process);
            asked.extend(rest);
            let mut lines = vec![format!("{status:?}")];
            lines.extend(log(fixture, &mut sandbox));
            lines.extend(asked);
            lines
        },
        &expect![[r#"
            Code(0)
            listening on 0100007F:2BE3
            the flight: 7 answered; HTTP/1.1 503 Service Unavailable|content-type: application/json|content-length: 86|connection: close||{"error":{"message":"7 model requests are in flight already: wait for one to finish"}}
            a body past the client's buffers, with a byte sent late: 1048657 bytes
            HTTP/1.1 200 OK|content-type: text/event-stream|transfer-encoding: chunked|connection: close||9|data: 1|||9|data: 2|||0||
            HTTP/1.1 200 OK|content-type: text/event-stream|transfer-encoding: chunked|connection: close||9|data: 1|||
            HTTP/1.1 404 Not Found|content-type: application/json|transfer-encoding: chunked|connection: close||9|data: 1|||0||
            ended without a head: []
            answered 204, then cut short: []
            answered 103, an interim status: []
            HTTP/1.1 200 OK|content-type: text/event-stream|connection: close||
            HTTP/1.1 411 Length Required|content-type: application/json|content-length: 82|connection: close||{"error":{"message":"a chunked body is refused: send its content-length instead"}}
            HTTP/1.1 400 Bad Request|content-type: application/json|content-length: 70|connection: close||{"error":{"message":"the content-length is not one number: send one"}}
            HTTP/1.1 400 Bad Request|content-type: application/json|content-length: 70|connection: close||{"error":{"message":"the content-length is not one number: send one"}}
            HTTP/1.1 411 Length Required|content-type: application/json|content-length: 82|connection: close||{"error":{"message":"a chunked body is refused: send its content-length instead"}}
            HTTP/1.1 413 Content Too Large|content-type: application/json|content-length: 73|connection: close||{"error":{"message":"the body is 67108865 bytes: send at most 67108864"}}
            HTTP/1.1 426 Upgrade Required|content-type: application/json|upgrade: HTTP/1.1|connection: upgrade|content-length: 72|connection: close||
            HTTP/1.1 426 Upgrade Required|content-type: application/json|upgrade: HTTP/1.1|connection: upgrade|content-length: 72|connection: close||{"error":{"message":"the request's version is HTTP/1.0: send HTTP/1.1"}}
            HTTP/1.1 431 Request Header Fields Too Large|content-type: application/json|content-length: 89|connection: close||{"error":{"message":"the request's head holds more than 16384 bytes: send at most that"}}
            owner: POST /held [] "", held
            owner: POST /late [] ""
            owner: POST /responses [("host", "x"), ("content-type", "application/json"), ("content-length", "13")] "{\"model\":\"m\"}"
            owner: POST /responses [("content-length", "2")] "{}"
            owner: GET /models [("accept", "*/*")] ""
            owner: POST /headless [] "": InvalidInput
            owner: POST /empty [("content-length", "2")] "{}"
            owner: POST /interim [] ""
            owner: HEAD /models [] """#]],
    );
}

/// Takes a full flight of model requests, then answers them once a command in the sandbox finds
/// that the client past the flight was refused. Returns what they asked, each once.
fn hold_flight(sandbox: &mut Started) -> Vec<String> {
    let held: Vec<IncomingModelRequest> = (0..MAX_MODEL_REQUESTS_IN_FLIGHT)
        .map(|_| next_model_request(&sandbox.arrivals))
        .collect();
    let refused = "until grep -qs ' 503 ' flight-*; do sleep 0.01; done";
    assert_eq!(sh(sandbox, 2, refused), ExitStatus::Code(0));
    let mut asked: Vec<String> = held.into_iter().map(respond).collect();
    asked.dedup();
    asked.iter().map(|line| format!("{line}, held")).collect()
}

/// Answers the request for `/late` ([`respond`]) once its client has sent a byte past it, which
/// `sandboxd` leaves unread, as a command in the sandbox finds. Returns what it asked.
fn respond_late(sandbox: &mut Started) -> String {
    let incoming = next_model_request(&sandbox.arrivals);
    // `sandboxd` has read the request, and reads no further, so the byte the client sends once
    // told to waits unread in `sandboxd`'s queue, where the command finds it.
    let late = [Put::Dir(&relative("late"), dir_mode())];
    sandbox
        .session()
        .put(&absolute("/pumpkin/in"), &late, hung())
        .unwrap();
    let unread = format!(
        "until awk '$2 ~ /:{MODEL_PORT:04X}$/ && $4 != \"0A\" && $5 !~ /:0+$/ {{ found = 1 }} \
         END {{ exit !found }}' /proc/net/tcp; do sleep 0.01; done"
    );
    assert_eq!(sh(sandbox, 3, &unread), ExitStatus::Code(0));
    respond(incoming)
}

/// The next of `arrivals`, which must be a model request that arrives within 30 s.
fn next_model_request(arrivals: &Receiver<Arrival>) -> IncomingModelRequest {
    match arrivals.recv_timeout(Duration::from_secs(30)) {
        Ok(Arrival::ModelRequest(incoming)) => incoming,
        arrived => panic!("a model request was to arrive within 30 s, not {arrived:?}"),
    }
}

/// Waits for `process` to exit as the session's owner does, answering each model request that
/// arrives meanwhile ([`respond`]). Returns how it exited, which it has 30 s to, and what each
/// request asked.
fn serve_models(sandbox: &mut Started, process: Process) -> (ExitStatus, Vec<String>) {
    let deadline = Deadline::after(Duration::from_secs(30));
    let mut asked = Vec::new();
    loop {
        let left = deadline.remaining();
        match left.and_then(|left| sandbox.arrivals.recv_timeout(left).ok()) {
            Some(Arrival::Exited {
                process: exited,
                status,
            }) if exited == process => return (status, asked),
            Some(Arrival::ModelRequest(incoming)) => asked.push(respond(incoming)),
            Some(arrival) => {
                panic!("a model request or the command's exit was to arrive, not {arrival:?}")
            }
            None => panic!("the clients ran on for 30 s, having asked {asked:?}"),
        }
    }
}

/// Responds to a model request as the session's owner: to `/headless` with a head too long for a
/// frame, and to `/late` with 200 and a body of 1 MiB; to any other with 204 for `/empty`, 103 for
/// `/interim`, 404 for a `GET` and 200 otherwise, then one event, then the end for an empty body, a
/// cut for a body of `{}`, and a second event and the end for any other. Each head also frames the
/// body, which `sandboxd` frames itself, so it drops that field. Returns what it asked.
fn respond(incoming: IncomingModelRequest) -> String {
    let request = &incoming.request;
    let body = String::from_utf8_lossy(&request.body).into_owned();
    let asked = format!(
        "owner: {} {} {:?} {body:?}",
        request.method, request.path, request.headers
    );
    if request.path == "/headless" {
        // A head too long for a frame, which ends the request without a response.
        let headers = vec![("x-pad".to_owned(), "a".repeat(MAX_MESSAGE_BYTES))];
        let refused = incoming.respond(200, headers, hung()).unwrap_err();
        return format!("{asked}: {:?}", refused.kind());
    }
    if request.path == "/late" {
        // Far more than the client's buffers hold, yet within what the kernel queues to send on
        // `sandboxd`'s socket, so that much of it is still queued there as `sandboxd` closes.
        let mut response = incoming.respond(200, Vec::new(), hung()).unwrap();
        response.send("a".repeat(1 << 20).as_bytes()).unwrap();
        response.end().unwrap();
        return asked;
    }
    let (status, kind) = match (request.method.as_str(), request.path.as_str()) {
        (_, "/empty") => (204, "text/event-stream"),
        (_, "/interim") => (103, "text/event-stream"),
        ("GET", _) => (404, "application/json"),
        _ => (200, "text/event-stream"),
    };
    let headers = [("content-type", kind), ("Content-Length", "1")];
    let headers = (headers.iter())
        .map(|&(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
    let mut response = incoming.respond(status, headers, hung()).unwrap();
    response.send(b"data: 1\n\n").unwrap();
    match body.as_str() {
        "" => response.end().unwrap(),
        "{}" => response.cut().unwrap(),
        _ => {
            response.send(b"data: 2\n\n").unwrap();
            response.end().unwrap();
        }
    }
    asked
}

/// The lines of the first command's standard output, from the sandbox's `/pumpkin/log`, and
/// its standard error, if anything was written there.
fn log(fixture: &Fixture, sandbox: &mut Started) -> Vec<String> {
    collect(fixture, sandbox, "log");
    let stdout = fs::read_to_string(fixture.dir.path().join("collected-log/1.stdout")).unwrap();
    let stderr = fs::read_to_string(fixture.dir.path().join("collected-log/1.stderr")).unwrap();
    let stderr = (!stderr.is_empty()).then(|| format!("stderr: {}", stderr.trim_end()));
    stdout.lines().map(str::to_owned).chain(stderr).collect()
}

/// A payload of 2.5 MiB, over two frames, whose bytes are not UTF-8.
fn payload() -> Vec<u8> {
    (0..5 << 19)
        .map(|n: u32| n.to_le_bytes()[0] | 0x80)
        .collect()
}

/// Answers `arrival`, an op, as a stand-in for the session's owner: one over the limit with its
/// refusal; ops with no grants; a judge call with job 1; a submission as stored, or accepted if
/// final; and the status of job 1, which completed and scored, of job 5, which did not, of job 2, which errored, or else a refusal.
/// Returns what it asked, saying whether its payload is `payload`.
fn reply(session: &mut Session, arrival: Arrival, payload: &[u8]) -> String {
    let incoming = match arrival {
        Arrival::Op(incoming) => incoming,
        Arrival::Refused(refused) => {
            let asked = format!(
                "refused {:?}, over {} bytes",
                refused.call(),
                refused.payload_limit().bytes()
            );
            session.refuse(refused, hung()).unwrap();
            return asked;
        }
        arrival @ (Arrival::ModelRequest(_) | Arrival::Exited { .. } | Arrival::Ended { .. }) => {
            panic!("an op was to arrive, not {arrival:?}")
        }
    };
    let remaining = Remaining {
        calls: Some(1),
        seconds: None,
    };
    let intact = |bytes: &[u8]| format!("{} bytes, intact: {}", bytes.len(), bytes == payload);
    let (asked, answer) = match &incoming.op {
        Op::Ops => {
            let resources = Resources {
                cpu_millis: NonZeroU64::MIN,
                memory_bytes: NonZeroU64::MIN,
            };
            let ops = OpsReply {
                remaining_seconds: 0,
                resources,
                grants: Vec::new(),
            };
            ("ops".to_owned(), OpReply::Ops(ops))
        }
        Op::Judge { grant, payload } => {
            let asked = format!("judge {grant}: {}", intact(payload));
            (asked, OpReply::Judge(JudgeReply { job: 1, remaining }))
        }
        Op::Submit {
            grant,
            finalize,
            payload,
        } => {
            let asked = format!("submit {grant}, finalize {finalize}: {}", intact(payload));
            let state = if *finalize {
                SubmitState::Accepted
            } else {
                SubmitState::Stored
            };
            let remaining_seconds = 60;
            let submitted = SubmitReply {
                state,
                remaining_seconds,
            };
            (asked, OpReply::Submit(submitted))
        }
        &Op::Status { job } => (format!("status {job}"), status(job, remaining)),
    };
    session.reply(incoming, answer, hung()).unwrap();
    asked
}

/// The status of job `job`, with `remaining`: job 1 completed and scored, job 5 completed and unscored, job 2 errored, and any other is
/// refused.
fn status(job: u64, remaining: Remaining) -> OpReply {
    let state = match job {
        1 => {
            let returned = Returned::from_iter([("score".to_owned(), 1.into())]);
            JobState::Completed {
                returned,
                scored: true,
            }
        }
        5 => {
            let returned = Returned::from_iter([("score".to_owned(), serde_json::Value::Null)]);
            JobState::Completed {
                returned,
                scored: false,
            }
        }
        2 => {
            let failure = Failure {
                code: FailureCode::JudgeTimeout,
                message: "the judge ran past its timeout".to_owned(),
            };
            JobState::Errored { failure }
        }
        _ => {
            let message = format!("there is no job {job}: name one a judge call returned");
            return OpReply::Refused { message };
        }
    };
    OpReply::Status(StatusReply {
        job,
        state,
        remaining,
    })
}
