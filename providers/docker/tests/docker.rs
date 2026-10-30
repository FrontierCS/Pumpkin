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
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
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
        let tag = |suffix: &str| {
            RunTag::try_from(format!("pk-{}-{name}{suffix}", std::process::id())).unwrap()
        };
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

    /// Starts a sandbox for role `role`, which runs as 4242:4243.
    fn start(&self, run: &RunTag, role: &str) -> Container {
        self.try_start(run, role, &self.sandboxd, image()).unwrap()
    }

    fn try_start(
        &self,
        run: &RunTag,
        role: &str,
        sandboxd: &Path,
        image: Image,
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
            assets: Vec::new(),
        };
        let start = Start {
            run,
            sandbox: &sandbox,
            sandboxd,
            socket_dir: &socket_dir,
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
    // Only what the provider's `docker_output` keeps of the environment, for the reasons it gives.
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
            let sandbox = fixture.start(&fixture.run, "confined");
            let script = r#"
                echo "interfaces: $(ls /sys/class/net)"
                nc -w 2 1.1.1.1 80 </dev/null >/dev/null 2>&1
                echo "outbound connect: exit $?"
                grep -E '^(Cap|NoNewPrivs)' /proc/self/status
                echo "uid=$(id -u) gid=$(id -g)"
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
            let status = sh(&sandbox, 1, script);
            let mut lines = vec![format!("{status:?}")];
            lines.extend(collect(fixture, &sandbox, "log"));
            // Rootless Docker and user namespaces each map the container's users to others on
            // the host; a rootful engine without them maps every user to itself.
            collect(fixture, &sandbox, "out");
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
            1.stdout: interfaces: lo; outbound connect: exit 1; CapInh:	0000000000000000; CapPrm:	0000000000000000; CapEff:	0000000000000000; CapBnd:	0000000000000000; CapAmb:	0000000000000000; NoNewPrivs:	1; uid=4242 gid=4243; cpu.max: 50000 100000; memory.max: 268435456; memory.swap.max: 0; pids.max: 4096; core file size: 0; mqueue at /dev/mqueue; tmpfs at /dev/shm; tmpfs at /pumpkin; tmpfs at /tmp; /pumpkin: 4242:4243 755, 1048576 KiB; a program in /pumpkin: ran; /tmp/x: refused; /.pumpkin/sandboxd: refused; /.pumpkin/gateway/x: refused; /etc/hosts: refused; /workspace/x: writable; /volume/x: writable; /pumpkin/home/x: writable; /dev/shm/x: writable; /proc/1/fd/1: refused; /dev/mqueue/x: made
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
/// removed, as is one whose `sandboxd` exits without connecting, after the connect timeout,
/// with the end of its log quoted; and an image the engine doesn't have isn't pulled.
#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn failed_start() {
    check(
        "failed",
        |fixture| {
            let not_executable = fixture.dir.join("sandboxd");
            fs::write(&not_executable, "not a program").unwrap();
            let missing = fixture.dir.join("absent");
            // Something at the socket's path, as a driver that was killed leaves its socket.
            fs::create_dir(fixture.dir.join("stale")).unwrap();
            fs::write(fixture.dir.join("stale/s"), "").unwrap();
            let comma = fixture.dir.join("sandboxd,readonly=false");
            fs::write(&comma, "").unwrap();
            // Its standard error first, to show that the log quotes its standard output first.
            let exiting = fixture.dir.join("exiting-sandboxd");
            let script = "#!/bin/sh\necho 'to standard error' >&2\necho 'to standard output'\n";
            fs::write(&exiting, script).unwrap();
            fs::set_permissions(&exiting, fs::Permissions::from_mode(0o755)).unwrap();
            let unpulled = Image::try_from(format!("busybox@sha256:{}", "0".repeat(64))).unwrap();
            let attempts = [
                ("missing", missing.as_path(), image()),
                ("stale", fixture.sandboxd.as_path(), image()),
                ("comma", comma.as_path(), image()),
                ("unexecutable", not_executable.as_path(), image()),
                ("exiting", exiting.as_path(), image()),
                ("unpulled", fixture.sandboxd.as_path(), unpulled),
            ];
            let mut lines = Vec::new();
            for (role, sandboxd, image) in attempts {
                let failed = fixture.try_start(&fixture.run, role, sandboxd, image);
                let failed = failed.unwrap_err();
                lines.push(format!("{role}: {}", redact(fixture, &failed)));
            }
            let left = containers(&fixture.run, "{{.ID}}").unwrap();
            lines.push(format!("left: {left:?}"));
            lines
        },
        &expect![[r#"
            missing: <dir>/absent can't be mounted at /.pumpkin/sandboxd: No such file or directory (os error 2): name a path that exists
            stale: the session's socket can't be made at <dir>/stale/s: Address already in use (os error 98): expected a directory that exists, which this user can write to, by a path short enough for a Unix socket, and nothing at `s`, where a driver that was killed leaves its socket
            comma: <dir>/sandboxd,readonly=false can't be mounted at /.pumpkin/sandboxd: name a path of UTF-8 without commas
            unexecutable: `docker start <id>` failed with exit status: 1: …: fix what Docker reports
            exiting: the `sandboxd` of container <id> didn't connect: no sandbox connected to <dir>/exiting/s within 30s: expected it to connect as it starts; the container's logs end: to standard output
            to standard error
            unpulled: `docker create` failed with exit status: 1: Error response from daemon: No such image: busybox@sha256:0000000000000000000000000000000000000000000000000000000000000000: fix what Docker reports
            left: []"#]],
    );
}

/// `message`, with the fixture's directory and any container ID, 64 hexadecimal digits that
/// aren't a digest, in placeholders, and with what Docker reports of a failed `docker start`
/// elided: that comes from the container runtime, and varies with the runtime and its version.
fn redact(fixture: &Fixture, message: &str) -> String {
    let message = message.replace(&fixture.dir.display().to_string(), "<dir>");
    let mut redacted = String::new();
    let mut word = String::new();
    // A space after the last word ends it, and is taken off again.
    for char in message.chars().chain([' ']) {
        if char.is_ascii_alphanumeric() {
            word.push(char);
            continue;
        }
        let id = word.len() == 64
            && word.chars().all(|char| char.is_ascii_hexdigit())
            && !redacted.ends_with("sha256:");
        redacted.push_str(if id { "<id>" } else { &word });
        redacted.push(char);
        word.clear();
    }
    redacted.pop();
    let failed = "`docker start <id>` failed with exit status: ";
    let advice = ": fix what Docker reports";
    let Some((before, after)) = redacted.split_once(failed) else {
        return redacted;
    };
    let (status, reported) = after.split_once(": ").unwrap();
    let (_, after) = reported.split_once(advice).unwrap();
    format!("{before}{failed}{status}: …{advice}{after}")
}
