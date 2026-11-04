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

use std::fs;
use std::io::Write;
use std::num::NonZeroU64;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use expect_test::{Expect, expect};
use pumpkin_docker::{Container, ContextName, Engine, Report, RunTag, Security, Start};
use pumpkin_gateway::{Arrival, Listener, Op, Process, Put, RefusedOp, Session};
use pumpkin_schema::ir::{AbsolutePath, Argv, Env, EnvVar, Image, Name, Resources, User};
use pumpkin_schema::sandbox::op::{
    Failed, Failure, FailureCode, JobState, JudgeReply, OpReply, OpsReply, Remaining, Returned,
    StatusReply, SubmitReply, SubmitState,
};
use pumpkin_schema::sandbox::tree::Limits;
use pumpkin_schema::sandbox::{
    ExitStatus, Launch, MAX_OP_PAYLOAD_BYTES, OpPayloadLimit, RelativePath,
};
use pumpkin_schema::workflow::Sandbox;

/// A small public image, pinned by digest.
const BUSYBOX: &str =
    "busybox@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e";

/// The environment of one test: its run tags and a directory for sockets and collected files.
/// Dropping it removes the containers of its runs, after showing their logs if the test failed.
struct Fixture {
    sandboxd: PathBuf,
    dir: PathBuf,
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
        let tag = |suffix: &str| RunTag::from(format!("pk-{}-{name}{suffix}", std::process::id()));
        // Socket paths are short: a directory in the system's temporary one, private to this
        // user, as the parent of every socket directory must be.
        let dir = std::env::temp_dir().join(format!("pk-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        Self {
            sandboxd: PathBuf::from(sandboxd).canonicalize().unwrap(),
            dir,
            run: tag(""),
            other_run: tag("-other"),
        }
    }

    /// Starts a passive sandbox for role `role`, which runs as 4242:4243.
    fn start(&self, run: &RunTag, role: &str) -> Container {
        self.try_start(run, role, &self.sandboxd, image(), None)
            .unwrap()
    }

    fn try_start(
        &self,
        run: &RunTag,
        role: &str,
        sandboxd: &Path,
        image: Image,
        ops: Option<OpPayloadLimit>,
    ) -> Result<Container, String> {
        let socket_dir = self.dir.join(role);
        fs::create_dir_all(&socket_dir).unwrap();
        let sandbox = Sandbox {
            name: Name::try_from(role.to_owned()).unwrap(),
            image,
            user: serde_json::from_str::<User>(r#"{"uid": 4242, "gid": 4243}"#).unwrap(),
            resources: serde_json::from_value::<Resources>(serde_json::json!({
                "cpu_millis": 500,
                "memory_bytes": 256 << 20,
            }))
            .unwrap(),
            model_route: None,
        };
        let start = Start {
            run,
            sandbox: &sandbox,
            sandboxd,
            socket_dir: &socket_dir,
            ops,
        };
        engine().start(&start)
    }
}

impl Drop for Fixture {
    // Nothing here panics, since a panic while a failed test unwinds would abort the test
    // binary and hide the other tests' results: failures are printed instead. Without an
    // engine, no test started a container.
    fn drop(&mut self) {
        if let Ok(engine) = connected() {
            for run in [&self.run, &self.other_run] {
                if thread::panicking()
                    && let Err(error) = show_logs(run)
                {
                    eprintln!("the logs of run {} can't be shown: {error}", run.as_str());
                }
                if let Err(error) = engine.destroy_all(run) {
                    eprintln!("run {} can't be cleaned up: {error}", run.as_str());
                }
            }
        }
        let _ = fs::remove_dir_all(&self.dir);
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

/// `docker`, run as the provider runs it, against the tests' context alone.
fn docker() -> Command {
    // Only what the provider's `docker` keeps of the environment, for the reasons it gives.
    let kept = ["HOME", "PATH", "DOCKER_CONFIG"]
        .into_iter()
        .filter_map(|name| Some((name, std::env::var_os(name)?)));
    let mut docker = Command::new("docker");
    docker.env_clear().envs(kept);
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
/// an anonymous volume. Built once, from the pinned image, and tagged, so that each build
/// replaces the last instead of leaving it behind untagged.
fn image() -> Image {
    static ID: OnceLock<String> = OnceLock::new();
    let id = ID.get_or_init(|| {
        let dockerfile =
            format!("FROM {BUSYBOX}\nRUN mkdir --mode=1777 /workspace /volume\nVOLUME /volume\n");
        let mut build = docker()
            .args([
                "build",
                "--quiet",
                "--tag=pumpkin-docker-tests:workspace",
                "-",
            ])
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
        stdout: absolute(&format!("/pumpkin/log/{n}.stdout")),
        stderr: absolute(&format!("/pumpkin/log/{n}.stderr")),
    }
}

/// Runs `script` as the n-th command ([`launch`]), and returns how it exited, which it has 30 s
/// to.
fn sh(sandbox: &mut Container, n: u32, script: &str) -> ExitStatus {
    let session = sandbox.session();
    let mut process = session.start(launch(n, script)).unwrap();
    process.wait(Duration::from_secs(30)).unwrap().unwrap()
}

/// Waits for `process` to exit as the session's owner does, answering each op that arrives
/// meanwhile ([`reply`]). Returns how it exited, which it has 30 s to, and what each op asked.
fn serve(
    session: &mut Session,
    process: &mut Process,
    payload: &[u8],
) -> (ExitStatus, Vec<String>) {
    // How long each wait lasts before the other's turn: what arrives meanwhile waits for it.
    let tick = Duration::from_millis(10);
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut asked = Vec::new();
    while Instant::now() < deadline {
        if let Some(status) = process.wait(tick).unwrap() {
            return (status, asked);
        }
        if let Some(arrival) = session.next_op(tick).unwrap() {
            asked.push(reply(session, arrival, payload));
        }
    }
    panic!("the command ran on for 30 s, having made ops that asked {asked:?}");
}

/// Collects the sandbox's `dir` into the fixture's directory and returns the files' contents.
fn collect(fixture: &Fixture, sandbox: &mut Container, dir: &str) -> Vec<String> {
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

/// The commands of the container's processes, from the host, by process ID, listed every
/// 10 ms until `done` holds of the list or 30 s pass.
fn processes(sandbox: &Container, done: impl Fn(&str) -> bool) -> String {
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

/// What a judge does: reads its payload, writes a result and evidence, and exits, leaving
/// processes behind that freezing kills: one orphaned to `sandboxd`, and one in a session of
/// its own, outside its command's process group.
#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn judge() {
    check(
        "judge",
        |fixture| {
            let mut sandbox = fixture.start(&fixture.run, "judge");
            let (slot, payload) = (relative("in/payload"), relative("in/payload/raw"));
            let put = [Put::Dir(&slot), Put::File(&payload, b"ledger")];
            sandbox.session().put(&absolute("/pumpkin"), &put).unwrap();
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
            sandbox.session().freeze().unwrap();
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

/// What the role's code can reach, seen from inside the container, whether the container's
/// users map to the host's as the engine's report says, and, seen from the host, the container's
/// log, which `sandboxd` alone writes, and what `docker create` pinned that the daemon or the
/// image would otherwise decide.
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
            let uid_map = fs::read_to_string(fixture.dir.join("collected-out/uid_map")).unwrap();
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
            1.stdout: interfaces: lo; outbound connect: exit 1; CapInh:	0000000000000000; CapPrm:	0000000000000000; CapEff:	0000000000000000; CapBnd:	0000000000000000; CapAmb:	0000000000000000; NoNewPrivs:	1; uid=4242 gid=4243; /.pumpkin holds: gateway sandboxd; /pumpkin holds: home in log out; sandboxctl ops: sandboxctl: sandboxd at /pumpkin/sandboxctl/socket can't be reached: No such file or directory (os error 2): run sandboxctl in a sandbox that serves ops; cpu.max: 50000 100000; memory.max: 268435456; memory.swap.max: 0; pids.max: 4096; core file size: 0; mqueue at /dev/mqueue; tmpfs at /dev/shm; tmpfs at /pumpkin; tmpfs at /tmp; /pumpkin: 4242:4243 755, 1048576 KiB; a program in /pumpkin: ran; /tmp/x: refused; /.pumpkin/sandboxd: refused; /.pumpkin/gateway/x: refused; /etc/hosts: refused; /workspace/x: writable; /volume/x: writable; /pumpkin/home/x: writable; /dev/shm/x: writable; /proc/1/fd/1: refused; /dev/mqueue/x: made
            rootless or userns, exactly when the uid map isn't the identity: true
            log config: {"Type":"json-file","Config":{"max-file":"1","max-size":"1048576"}}
            init: false
            healthcheck: {"Test":["NONE"]}
            log: """#]],
    );
}

/// What the provider reports of the engine before anything starts, as far as every engine it
/// takes shows the same, and how it refuses an empty context name, and names no context has,
/// among them `--help`, which `docker context inspect` mustn't take for its own option, which
/// prints its help and succeeds.
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
            let mut lines = vec![
                format!(
                    "endpoint, versions, kernel and OS named: {}",
                    named.iter().all(|named| !named.is_empty())
                ),
                ContextName::try_from(String::new()).unwrap_err(),
            ];
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
            the Docker context's name is empty: name one that `docker context ls` lists
            the Docker context `pumpkin-missing` can't be read: …: name one that `docker context ls` lists
            the Docker context `--help` can't be read: …: name one that `docker context ls` lists"#]],
    );
}

/// Destroying a sandbox removes its container, as dropping it does, and cleaning up a run
/// removes the rest of its containers and no others; destroying one already removed succeeds.
/// Each way removes the container's anonymous volume with it.
#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn cleanup() {
    check(
        "cleanup",
        |fixture| {
            let mut lines = Vec::new();
            let first = fixture.start(&fixture.run, "first");
            let second = fixture.start(&fixture.run, "second");
            let third = fixture.start(&fixture.run, "third");
            let other = fixture.start(&fixture.other_run, "other");
            let volumes = [&first, &second, &third, &other].map(volume);
            let roles = |run| containers(run, r#"{{.Label "pumpkin.role"}}"#).unwrap();
            first.destroy().unwrap();
            drop(second);
            lines.push(format!(
                "after destroying first and dropping second: {:?}",
                roles(&fixture.run)
            ));
            let removed = engine().destroy_all(&fixture.run).unwrap();
            lines.push(format!("removed: {}", removed.len()));
            lines.push(format!("left: {:?}", roles(&fixture.run)));
            lines.push(format!("destroying third again: {:?}", third.destroy()));
            lines.push(format!("other run's: {:?}", roles(&fixture.other_run)));
            let listed = docker().args(["volume", "ls", "--quiet"]).output().unwrap();
            let listed = String::from_utf8(listed.stdout).unwrap();
            let kept = (["first", "second", "third", "other"].iter().zip(&volumes))
                .filter(|&(_, volume)| listed.lines().any(|listed| listed == volume))
                .map(|(role, _)| role);
            lines.push(format!("volumes left: {:?}", kept.collect::<Vec<_>>()));
            lines
        },
        &expect![[r#"
            after destroying first and dropping second: ["third"]
            removed: 1
            left: []
            destroying third again: Ok(())
            other run's: ["other"]
            volumes left: ["other"]"#]],
    );
}

/// The name of the anonymous volume Docker made for the sandbox's `/volume`.
fn volume(sandbox: &Container) -> String {
    let format = r#"{{range .Mounts}}{{if eq .Destination "/volume"}}{{.Name}}{{end}}{{end}}"#;
    let inspect = ["inspect", "--format", format, sandbox.id()];
    let output = docker().args(inspect).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let name = String::from_utf8(output.stdout).unwrap().trim().to_owned();
    assert!(!name.is_empty(), "the sandbox has no volume at /volume");
    name
}

/// A socket directory that holds something at the socket's path is refused; a `sandboxd` that
/// doesn't exist is named, and one whose path has a comma, which `--mount` would read as the
/// start of another option, is refused; a container that Docker creates but can't start is
/// removed; and an image the engine doesn't have isn't pulled.
#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn failed_start() {
    check(
        "failed",
        |fixture| {
            let not_executable = fixture.dir.join("sandboxd");
            fs::write(&not_executable, "not a program").unwrap();
            let missing = fixture.dir.join("absent");
            // Something at the socket's path, as a process that was killed leaves its socket.
            fs::create_dir(fixture.dir.join("stale")).unwrap();
            fs::write(fixture.dir.join("stale/s"), "").unwrap();
            let comma = fixture.dir.join("sandboxd,readonly=false");
            fs::write(&comma, "").unwrap();
            let unpulled = Image::try_from(format!("busybox@sha256:{}", "0".repeat(64))).unwrap();
            let attempts = [
                ("missing", missing.as_path(), image()),
                ("stale", fixture.sandboxd.as_path(), image()),
                ("comma", comma.as_path(), image()),
                ("unexecutable", not_executable.as_path(), image()),
                ("unpulled", fixture.sandboxd.as_path(), unpulled),
            ];
            let mut lines = Vec::new();
            for (role, sandboxd, image) in attempts {
                let failed = fixture.try_start(&fixture.run, role, sandboxd, image, None);
                let failed = failed.unwrap_err();
                lines.push(format!("{role}: {}", redact(fixture, &failed)));
            }
            let left = containers(&fixture.run, "{{.ID}}").unwrap();
            lines.push(format!("left: {left:?}"));
            lines
        },
        &expect![[r#"
            missing: <dir>/absent can't be mounted at /.pumpkin/sandboxd: No such file or directory (os error 2): name a path that exists
            stale: the session's socket can't be made at <dir>/stale/s: Address already in use (os error 98): expected a directory that exists, which this user can write to, by a path short enough for a Unix socket, and nothing at `s`, where a process that was killed leaves its socket
            comma: <dir>/sandboxd,readonly=false can't be mounted at /.pumpkin/sandboxd: name a path of UTF-8 without commas
            unexecutable: `docker start …: fix what Docker reports
            unpulled: `docker create` failed with exit status: 1: Error response from daemon: No such image: busybox@sha256:0000000000000000000000000000000000000000000000000000000000000000: fix what Docker reports
            left: []"#]],
    );
}

/// `message`, with the fixture's directory as `<dir>`, and `…` for what varies between runs or
/// hosts in a failed `docker start`: the container's ID, and what Docker reports, which comes
/// from the container runtime, and varies with the runtime and its version.
fn redact(fixture: &Fixture, message: &str) -> String {
    let message = message.replace(&fixture.dir.display().to_string(), "<dir>");
    let (from, to) = ("`docker start ", ": fix what Docker reports");
    if let Some((before, rest)) = message.split_once(from)
        && let Some((_, after)) = rest.split_once(to)
    {
        return format!("{before}{from}…{to}{after}");
    }
    message
}

/// A shell function, `ctl`, that runs `sandboxctl` with its arguments, and shows how it exited
/// and what it printed.
const CTL: &str = r#"
    ctl() { out=$(/.pumpkin/bin/sandboxctl "$@" 2>&1); echo "\$ sandboxctl $*: exit $?"; [ -z "$out" ] || echo "  $out"; }"#;

/// Commands in a sandbox that serves ops make them through `sandboxctl`, which a stand-in for
/// the session's owner answers ([`reply`]), and the gateway refuses a payload over the limit
/// the sandbox was started with. `sandboxd` made the control socket's directory closed to all
/// but the role's user, and `sandboxctl` is read-only, so writing it fails the last command.
#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn ops() {
    check(
        "ops",
        |fixture| {
            let payload = payload();
            let limit = OpPayloadLimit::try_from(u64::try_from(payload.len()).unwrap()).unwrap();
            let sandbox =
                fixture.try_start(&fixture.run, "ops", &fixture.sandboxd, image(), Some(limit));
            let mut sandbox = sandbox.unwrap();
            let session = sandbox.session();
            let put = [Put::File(&relative("payload"), &payload)];
            session.put(&absolute("/pumpkin/home"), &put).unwrap();
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
                echo "control socket's directory: $(ls -ld /pumpkin/sandboxctl | cut -c1-10)"
                echo > /.pumpkin/bin/sandboxctl"#
            );
            let mut process = session.start(launch(1, &script)).unwrap();
            let (status, arrived) = serve(session, &mut process, &payload);
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
              {"kind":"status","job":1,"state":"passed","returned":{"score":1},"remaining":{"calls":1,"seconds":null}}
            $ sandboxctl status 2: exit 2
              {"kind":"status","job":2,"state":"failed","failure":{"code":"judge_timeout","message":"the judge ran past its timeout"},"remaining":{"calls":1,"seconds":null}}
            $ sandboxctl status 3: exit 1
              {"kind":"refused","message":"there is no job 3: name one a judge call returned"}
            $ sandboxctl judge train big: exit 1
              {"kind":"refused","message":"the payload is 2621441 bytes, more than the limit of 2621440: send at most 2621440"}
            control socket's directory: drwx------
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

/// Calls that `sandboxctl` or `sandboxd` refuse before an op opens, each failing its caller
/// alone, so that the session serves the next call: among them one with `/proc/1/mem`, the
/// memory of `sandboxd`, which can't open it, being non-dumpable. And a second `sandboxd`,
/// connected to a session of its own, is refused the control socket's directory, since the
/// first made it.
#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn refused_calls() {
    check(
        "refused",
        |fixture| {
            let ops = Some(OpPayloadLimit::try_from(64).unwrap());
            let sandbox =
                fixture.try_start(&fixture.run, "refused", &fixture.sandboxd, image(), ops);
            let mut sandbox = sandbox.unwrap();
            // A session for the second `sandboxd`, which it connects to and never serves.
            let second = Listener::bind(fixture.dir.join("refused/second")).unwrap();
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
                ctl status 1
                second=$(/.pumpkin/sandboxd /.pumpkin/gateway/second --ops 2>&1)
                echo "a second sandboxd: exit $?: $second""#,
                huge = MAX_OP_PAYLOAD_BYTES + 1,
            );
            let session = sandbox.session();
            let mut process = session.start(launch(1, &script)).unwrap();
            let (status, arrived) = serve(session, &mut process, &[]);
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
            $ sandboxctl status 1: exit 0
              {"kind":"status","job":1,"state":"passed","returned":{"score":1},"remaining":{"calls":1,"seconds":null}}
            a second sandboxd: exit 1: sandboxd: serving ops failed: the control socket's directory /pumpkin/sandboxctl can't be made: File exists (os error 17): expected nothing there yet, in a directory this user can write to
            owner: status 1"#]],
    );
}

/// Callers whose ops arrive while one is in flight wait their turns, and each is carried once
/// the op before it is answered. A caller that has left when its turn comes is dropped, its op
/// unsent; one that leaves after its op was sent has it carried out. A payload's file is read
/// in its caller's turn, so one replaced by a symbolic link while its caller waits is refused,
/// as `sandboxd` opens no link.
#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn turns() {
    check(
        "turns",
        |fixture| {
            let ops = Some(OpPayloadLimit::try_from(64).unwrap());
            let sandbox = fixture.try_start(&fixture.run, "turns", &fixture.sandboxd, image(), ops);
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
            let mut callers = session.start(launch(1, script)).unwrap();
            let first = session.next_op(Duration::from_secs(30)).unwrap().unwrap();
            let held = [Put::Dir(&relative("held"))];
            session.put(&absolute("/pumpkin/in"), &held).unwrap();
            // No op can arrive while the first is in flight.
            sh(&mut sandbox, 2, "until [ -e ready ]; do sleep 0.01; done");
            let session = sandbox.session();
            let mut arrived = vec![reply(session, first, &[])];
            let (status, rest) = serve(session, &mut callers, &[]);
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
            status 2: exit 2: {"kind":"status","job":2,"state":"failed","failure":{"code":"judge_timeout","message":"the judge ran past its timeout"},"remaining":{"calls":1,"seconds":null}}
            status 3: exit 1: {"kind":"refused","message":"there is no job 3: name one a judge call returned"}
            judge train swapped: exit 1: sandboxctl: /pumpkin/home/swapped: Symbolic link loop (os error 40)
            status 9: exit 1: {"kind":"refused","message":"there is no job 9: name one a judge call returned"}
            owner: status 1
            owner: status 2
            owner: status 3
            owner: status 9"#]],
    );
}

/// The lines of the first command's standard output, from the sandbox's `/pumpkin/log`, and
/// its standard error, if anything was written there.
fn log(fixture: &Fixture, sandbox: &mut Container) -> Vec<String> {
    collect(fixture, sandbox, "log");
    let stdout = fs::read_to_string(fixture.dir.join("collected-log/1.stdout")).unwrap();
    let stderr = fs::read_to_string(fixture.dir.join("collected-log/1.stderr")).unwrap();
    let stderr = (!stderr.is_empty()).then(|| format!("stderr: {}", stderr.trim_end()));
    stdout.lines().map(str::to_owned).chain(stderr).collect()
}

/// A payload of 2.5 MiB, over two frames, whose bytes are not UTF-8.
fn payload() -> Vec<u8> {
    (0..5 << 19)
        .map(|n: u32| n.to_le_bytes()[0] | 0x80)
        .collect()
}

/// Answers `arrival` as a stand-in for the session's owner, unless the gateway refused it: ops
/// with no grants; a judge call with job 1; a submission as stored, or accepted if final; and
/// the status of job 1, which passed, of job 2, which failed, or else a refusal. Returns what
/// it asked, saying whether its payload is `payload`.
fn reply(session: &mut Session, arrival: Arrival, payload: &[u8]) -> String {
    let incoming = match arrival {
        Arrival::Op(incoming) => incoming,
        Arrival::Refused(RefusedOp {
            call,
            payload_limit,
            ..
        }) => return format!("refused {call:?}, over {} bytes", payload_limit.bytes()),
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
    session.reply(incoming, answer).unwrap();
    asked
}

/// The status of job `job`, with `remaining`: job 1 passed, job 2 failed, and any other is
/// refused.
fn status(job: u64, remaining: Remaining) -> OpReply {
    let state = match job {
        1 => {
            let returned = Returned::from_iter([("score".to_owned(), 1.into())]);
            JobState::Passed { returned }
        }
        2 => {
            let failure = Failure {
                code: FailureCode::JudgeTimeout,
                message: "the judge ran past its timeout".to_owned(),
            };
            JobState::Failed(Failed::NoResult { failure })
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
