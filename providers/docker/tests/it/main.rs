//! The Docker provider, against a real Docker daemon. These tests need Docker, so they are
//! ignored.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test helper fails the test that calls it"
)]
#![expect(
    clippy::print_stderr,
    reason = "a failed test shows its containers' logs"
)]

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::num::{NonZeroU32, NonZeroU64};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant, SystemTime};
use std::{env, fs, thread};

use docker::Docker;
use engine::{BuildContext, ContextName, Engine, Recipe, Security};
use expect_test::{Expect, expect};
use gateway::{Arrival, IncomingModelRequest, Op, Process, Put, Registration, Session, Sockets};
use provider::{
    Create, Created, Creator, Listed, Provider, Reservation, Role, RunTag, SandboxId, Sweep,
    UnixTime,
};
use recipe::Given;
use schema::ir::{AbsolutePath, Argv, Env, EnvVar, Image, Name, Resources, User};
use schema::sandbox::frame::MAX_MESSAGE_BYTES;
use schema::sandbox::op::{
    CommandEnd, CommandStatus, Failure, FailureCode, JobState, JudgeReply, Last, LeaseRemaining,
    LeaseReply, LeaseState, LeaseStatusReply, OpReply, OpsReply, Remaining, Returned, StatusReply,
    SubmitReply, SubmitState,
};
use schema::sandbox::tree::Limits;
use schema::sandbox::{
    ExitStatus, Launch, MAX_MODEL_REQUEST_BYTES, MAX_MODEL_REQUESTS_IN_FLIGHT,
    MAX_OP_PAYLOAD_BYTES, MODEL_PORT, OpPayloadLimit, RelativePath, SOCKET_NAME, SocketDial,
    dir_mode, file_mode,
};
use stdx::process::Environment;
use stdx::{Context, Deadline};
use tempfile::TempDir;
use test_support::suite::{Locals, Subject, reserve_with_room};
use test_support::{BUSYBOX, docker, hung};

/// The variable that makes this test binary, run again by [`hung_create`], make a create through
/// the stand-in `docker`, and write what came of it to `created` in the directory it names.
const STAND_IN: &str = "PUMPKIN_TEST_STAND_IN";

/// The roles of the tests' runs, each its own phase, whose sandboxes a fixture's run holds one
/// CPU for at a time.
const ROLES: [&str; 13] = [
    "confined",
    "first",
    "hung",
    "judge",
    "missing",
    "model",
    "not_utf8",
    "ops",
    "refused",
    "stand-in",
    "turns",
    "unexecutable",
    "unpulled",
];

/// The environment of one test: its run tags and a directory for sockets and collected files.
/// Dropping it removes the containers of its runs, after showing their logs if the test failed.
struct Fixture {
    sandboxd: PathBuf,
    dir: TempDir,
    /// Where its sandboxes meet the gateway, below `dir`.
    sockets: Sockets,
    run: RunTag,
    other_run: RunTag,
    /// What the provider holds for `run`, whose sandboxes are of [`ROLES`].
    _held: Reservation,
}

impl Fixture {
    /// The fixture for test `name`.
    fn new(name: &str) -> Self {
        let tag = |suffix: &str| format!("pk-{}-{name}{suffix}", std::process::id());
        let tag = |suffix: &str| RunTag::try_from(tag(suffix)).unwrap();
        // Private to this user, as the parent of every socket directory must be.
        let dir = tempfile::Builder::new()
            .prefix("docker-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let run = tag("");
        let phases: Vec<Vec<Role>> = ROLES.iter().map(|&name| vec![role(name)]).collect();
        let held = reserve_with_room(provider(), &run, &phases);
        Self {
            sandboxd: test_support::sandboxd(),
            sockets: Sockets::create(dir.path().join("sockets")).unwrap(),
            dir,
            run,
            other_run: tag("-other"),
            _held: held,
        }
    }

    /// Starts a passive sandbox for role `role`, which runs as 4242:4243.
    fn start(&self, run: &RunTag, role: &str) -> Started {
        self.try_start(run, role, &self.sandboxd, &image(), None, false)
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
        image: &Image,
        ops: Option<OpPayloadLimit>,
        model: bool,
    ) -> Result<Started, String> {
        let (create, dial, registration) =
            self.publish(run, role, sandboxd, image, ops.is_some(), model)?;
        Self::start_created(&create, dial, registration, ops, model)
    }

    /// Starts the sandbox that `create` describes, as [`try_start`](Self::try_start) does, its
    /// session meeting it at `registration`, where `dial` names.
    fn start_created(
        create: &Create,
        dial: SocketDial,
        registration: Registration,
        ops: Option<OpPayloadLimit>,
        model: bool,
    ) -> Result<Started, String> {
        let run = &create.run;
        let by = startup();
        let Created { id, report: _ } = provider().create(create, dial)?;
        let (sender, arrivals) = mpsc::channel();
        // Nothing stops the start: its sandbox dials, or its create fails.
        let accepted = registration.accept(by, ops, model, sender, |arrival| arrival, || false);
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
                provider().delete(run, &[id]).unwrap();
                Err(format!(
                    "{error}; its console ends: {}",
                    console.text.trim()
                ))
            }
        }
    }

    /// Publishes `image` and `sandboxd` through the provider, and returns the create of a
    /// sandbox of run `run` for role `role`, which runs them as 4242:4243, makes ops if `ops`
    /// and serves the model endpoint if `model`, the socket it dials, and its registration.
    fn publish(
        &self,
        run: &RunTag,
        role: &str,
        sandboxd: &Path,
        image: &Image,
        ops: bool,
        model: bool,
    ) -> Result<(Create, SocketDial, Registration), String> {
        let name = Name::try_from(role.to_owned()).unwrap();
        let (dial, registration) = self.sockets.issue(&name).unwrap();
        let role = self::role(role);
        let image = provider().publish_image(image)?;
        let sandboxd = provider().publish_sandboxd(sandboxd)?;
        let now = UnixTime::of(SystemTime::now()).unwrap();
        let create = Create {
            run: run.clone(),
            name,
            role,
            instance: 0,
            // The provider's one class.
            class: provider().ready().classes.keys().next().unwrap().clone(),
            image,
            sandboxd,
            ops,
            model,
            // Far past the test's end, which comes within minutes.
            hard_end: now.saturating_add(Duration::from_hours(1)),
            // As the start that the create begins, by the startup the provider declares.
            by: now.saturating_add(declared_startup()),
        };
        Ok((create, dial, registration))
    }
}

/// Role `name`, as the tests' sandboxes have it: run as 4242:4243, with half a CPU and 256 MiB.
fn role(name: &str) -> Role {
    Role {
        name: Name::try_from(name.to_owned()).unwrap(),
        user: serde_json::from_str::<User>(r#"{"uid": 4242, "gid": 4243}"#).unwrap(),
        resources: serde_json::from_value::<Resources>(serde_json::json!({
            "cpu_millis": 500,
            "memory_bytes": 256 << 20,
        }))
        .unwrap(),
        instances: NonZeroU32::MIN,
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
        if let Ok(provider) = connected() {
            let provider: &dyn Provider = provider;
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
fn reached() -> &'static Result<Engine, String> {
    static ENGINE: OnceLock<Result<Engine, String>> = OnceLock::new();
    ENGINE.get_or_init(|| {
        let context = ContextName::try_from(test_support::docker_context())?;
        Engine::connect(context, &engine_dir())
    })
}

/// The provider on the engine, or why either can't be reached.
fn connected() -> &'static Result<Docker<'static>, String> {
    static PROVIDER: OnceLock<Result<Docker<'static>, String>> = OnceLock::new();
    PROVIDER.get_or_init(|| Docker::connect(reached().as_ref().map_err(String::clone)?, None))
}

/// The directory of what the tests' engine writes on this machine, which every run of the tests
/// shares, as a machine's runs share their workspace's.
fn engine_dir() -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join("docker")
}

/// The engine the tests reach Docker through.
fn engine() -> &'static Engine {
    reached().as_ref().unwrap()
}

/// The provider on the engine, as the runtime holds one: what the tests start sandboxes through.
fn provider() -> &'static Docker<'static> {
    connected().as_ref().unwrap()
}

/// The deadline of a start that begins now, by the startup that the provider's one class
/// declares.
fn startup() -> Deadline {
    Deadline::after(declared_startup())
}

/// The startup that the provider's one class declares.
fn declared_startup() -> Duration {
    let class = provider().ready().classes.values().next().unwrap();
    Duration::from_secs(class.startup_seconds.get())
}

/// The IDs of the sandboxes a list found.
fn ids(listed: Vec<Listed>) -> Vec<SandboxId> {
    (listed.into_iter())
        .map(|Listed { id, ended: _ }| id)
        .collect()
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

/// The ID of the image the tests share, its manifest's digest, as the IR names it: `busybox` with
/// a world-writable `/workspace`, which a role's user needs, since the writable layer is the
/// workspace, and a world-writable `VOLUME` at `/volume`, for which Docker makes each container
/// an anonymous volume. Built once, from the pinned image, by the engine, as `pumpkin build`
/// builds, so that every sandbox the tests start runs an image as a workflow's are made; a build
/// of the same inputs is found by its tag, so test processes that run at once share the image.
fn image() -> Image {
    static ID: OnceLock<Image> = OnceLock::new();
    let id = ID.get_or_init(|| {
        let recipe =
            format!("FROM {BUSYBOX}\nRUN mkdir --mode=1777 /workspace /volume\nVOLUME /volume\n");
        let context = BuildContext::default();
        let given = Given::default();
        let built = engine().build(
            &context,
            &Recipe::Text(recipe),
            &given,
            "pumpkin-docker-tests",
        );
        built.unwrap().image().clone()
    });
    id.clone()
}

/// An image that no engine has: its digest is all zeros.
fn unpulled() -> Image {
    Image::try_from(format!("sha256:{}", "0".repeat(64))).unwrap()
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

// What a judge does: reads its payload, writes a result and evidence, and exits, leaving
// processes behind that freezing kills: one orphaned to `sandboxd`, and one in a session of
// its own, outside its command's process group.
#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
fn judge() {
    check(
        "judge",
        |fixture| {
            let mut sandbox = fixture.start(&fixture.run, "judge");
            let (slot, payload) = (relative("in/payload"), relative("in/payload/raw"));
            let put = [
                Put::Dir(slot, dir_mode()),
                Put::File(payload, file_mode(), b"ledger"),
            ];
            sandbox
                .session()
                .put(&absolute("/pumpkin"), put, hung())
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

// What the role's code can reach, seen from inside the container, which was created to serve
// neither ops nor the model endpoint, whether the container's users map to the host's as the
// engine's report says, and, seen from the host, the container's log, which `sandboxd` alone
// writes, and what `docker create` pinned that the daemon or the image would otherwise decide.
#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
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

// Each run's containers run on cores of its own: the roles of a run that run at once get whole
// cores that no other's share, each instance of a role that runs several at once its own, and
// another run's are apart from all of them; each container sees only its own CPUs. A run that
// needs more CPUs than the others leave is refused, naming those that hold them, as is one that
// needs more than the machine has.
#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is what each share shows, and the snapshot"
)]
fn cpus() {
    check(
        "cpus",
        |fixture| {
            let sized = |name: &str, cpus: u64| Role {
                resources: Resources {
                    cpu_millis: NonZeroU64::new(cpus * 1000).unwrap(),
                    ..role(name).resources
                },
                ..role(name)
            };
            let first = &fixture.other_run;
            let runners = || Role {
                instances: NonZeroU32::new(2).unwrap(),
                ..sized("runner", 1)
            };
            let phases = [
                vec![sized("agent", 2), sized("train", 1), runners()],
                vec![sized("final", 2)],
            ];
            let reserved = reserve_with_room(provider(), first, &phases);
            let start = |run: &RunTag, name: &str, role: Role, instance: u32| {
                let made = fixture.publish(run, name, &fixture.sandboxd, &image(), false, false);
                let (mut create, dial, registration) = made.unwrap();
                (create.role, create.instance) = (role, instance);
                Fixture::start_created(&create, dial, registration, None, false).unwrap()
            };
            let mut started = [
                ("the agent", start(first, "agent", sized("agent", 2), 0)),
                (
                    "the train judge",
                    start(first, "train", sized("train", 1), 0),
                ),
                ("a runner", start(first, "runner-0", runners(), 0)),
                ("another runner", start(first, "runner-1", runners(), 1)),
                (
                    "another run's judge",
                    start(&fixture.run, "judge", role("judge"), 0),
                ),
            ];
            let mut lines = Vec::new();
            let mut sets = Vec::new();
            for (name, sandbox) in &mut started {
                let (given, seen) = given_and_seen(fixture, sandbox);
                let whole = given.iter().all(|&cpu| siblings(cpu).is_subset(&given));
                lines.push(format!(
                    "{name}: on whole cores: {whole}, as it sees them: {}",
                    seen == given
                ));
                sets.push(given);
            }
            let apart =
                (0..sets.len()).all(|a| (a + 1..sets.len()).all(|b| sets[a].is_disjoint(&sets[b])));
            lines.push(format!("each apart from the others: {apart}"));
            let online = fs::read_to_string("/sys/devices/system/cpu/online").unwrap();
            let online = cpu_list(&online);
            let count = online.len() as u64;
            let cores = online
                .into_iter()
                .map(siblings)
                .collect::<BTreeSet<_>>()
                .len();
            let threads = count / cores as u64;
            let held = sets.iter().flatten().map(|&cpu| siblings(cpu));
            let held = held.collect::<BTreeSet<_>>().len() as u64;
            let wide = RunTag::try_from(format!("{}-wide", fixture.run.as_str())).unwrap();
            // A core more than those runs leave.
            let left = threads * (cores as u64 - held + 1);
            let left = provider().reserve(&wide, &[vec![sized("wide", left)]]);
            let refused = left.unwrap_err();
            let named = [first, &fixture.run]
                .iter()
                .all(|run| refused.contains(run.as_str()));
            lines.push(format!(
                "one that needs more than are left: refused, naming them: {named}"
            ));
            let more = provider().reserve(&wide, &[vec![sized("wide", count + 1)]]);
            let more = more.unwrap_err();
            let more = more.replace(&(count + 1).to_string(), "<cpus + 1>");
            let more = more.replace(&cores.to_string(), "<cores>");
            lines.push(format!("one that needs more than the machine has: {more}"));
            drop(reserved);
            lines
        },
        &expect![[r#"
            the agent: on whole cores: true, as it sees them: true
            the train judge: on whole cores: true, as it sees them: true
            a runner: on whole cores: true, as it sees them: true
            another runner: on whole cores: true, as it sees them: true
            another run's judge: on whole cores: true, as it sees them: true
            each apart from the others: true
            one that needs more than are left: refused, naming them: true
            one that needs more than the machine has: the run's roles need more whole cores at once than the <cores> that this machine gives sandboxes, to hold their CPUs (`wide` <cpus + 1>): give them fewer CPUs"#]],
    );
}

/// The CPUs that the engine gave `sandbox`'s container, and those its processes may run on, as
/// they see them.
fn given_and_seen(fixture: &Fixture, sandbox: &mut Started) -> (BTreeSet<u32>, BTreeSet<u32>) {
    let inspect = [
        "inspect",
        "--format",
        "{{.HostConfig.CpusetCpus}}",
        sandbox.id(),
    ];
    let given = docker().args(inspect).output().unwrap();
    let given = cpu_list(&String::from_utf8(given.stdout).unwrap());
    sh(
        sandbox,
        1,
        "grep Cpus_allowed_list /proc/self/status | cut -f 2",
    );
    let seen = collect(fixture, sandbox, "log");
    fs::remove_dir_all(fixture.dir.path().join("collected-log")).unwrap();
    let seen = seen.iter().find_map(|line| line.strip_prefix("1.stdout: "));
    (given, cpu_list(seen.unwrap_or_default()))
}

/// The CPUs that share a core with `cpu`, `cpu` among them.
fn siblings(cpu: u32) -> BTreeSet<u32> {
    let path = format!("/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list");
    cpu_list(&fs::read_to_string(path).unwrap())
}

/// The CPUs that `text`, as the kernel writes a list of them, names.
fn cpu_list(text: &str) -> BTreeSet<u32> {
    let ranges = text.trim().split(',').filter(|range| !range.is_empty());
    ranges
        .flat_map(|range| {
            let (first, last) = range.split_once('-').unwrap_or((range, range));
            first.parse::<u32>().unwrap()..=last.parse().unwrap()
        })
        .collect()
}

// What the provider declares as ready: the engine's report, and its one class, which offers
// what the host has.
#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
fn ready() {
    let mut ready = serde_json::to_value(provider().ready()).unwrap();
    let reported: Vec<&String> = ready["report"].as_object().unwrap().keys().collect();
    let reported = format!("{reported:?}");
    // The host's, which every engine has its own of: of its CPU's flags, whether they hold
    // `sse2`, which every x86-64 CPU has.
    let classes = ready["classes"].as_object_mut().unwrap();
    for class in classes.values_mut() {
        let offers = &mut class["offers"];
        offers["cpu_millis"] = "<the host's>".into();
        offers["memory_bytes"] = "<the host's>".into();
        let sse2 = offers["cpu_features"]
            .as_array()
            .unwrap()
            .contains(&"sse2".into());
        offers["cpu_features"] = format!("<the host's, with sse2: {sse2}>").into();
    }
    let ready = format!(
        "a report of {reported}, and the classes {}",
        ready["classes"]
    );
    expect![[r#"a report of ["client_version", "context", "endpoint", "engine_version", "kernel", "os", "platform", "security"], and the classes {"container":{"offers":{"cpu_features":"<the host's, with sse2: true>","cpu_millis":"<the host's>","cpu_model":null,"gpu":null,"memory_bytes":"<the host's>"},"startup_seconds":540}}"#]]
    .assert_eq(&ready);
}

// Deleting what a run's list finds removes its containers' anonymous volumes, and no other
// run's.
#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
fn volumes() {
    check(
        "volumes",
        |fixture| {
            let _other = reserve_with_room(provider(), &fixture.other_run, &[vec![role("other")]]);
            let started = [("first", &fixture.run), ("other", &fixture.other_run)];
            let started = started.map(|(role, run)| (role, fixture.start(run, role)));
            let volumes = started.each_ref().map(|(_, sandbox)| volume(sandbox));
            let listed = ids(provider().list(&fixture.run).unwrap());
            provider().delete(&fixture.run, &listed).unwrap();
            let listed = docker().args(["volume", "ls", "--quiet"]).output().unwrap();
            let listed = String::from_utf8(listed.stdout).unwrap();
            let kept = (started.iter().zip(&volumes))
                .filter(|&(_, volume)| listed.lines().any(|listed| listed == volume))
                .map(|((role, _), _)| role);
            vec![format!("volumes left: {:?}", kept.collect::<Vec<_>>())]
        },
        &expect![[r#"volumes left: ["other"]"#]],
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

// The suite every provider passes ([`test_support::suite`]), on Docker ([`Platform`]).
#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
fn suite() {
    test_support::suite::check(&[("docker", &Platform)]);
}

/// The provider on the tests' engine, as the suite's subject: each instance connects afresh.
struct Platform;

impl Subject for Platform {
    fn connect(&self) -> Result<Box<dyn Provider + '_>, String> {
        Ok(Box::new(Docker::connect(engine(), None)?))
    }

    fn locals(&self, dir: &Path) -> Locals {
        // One that no user may execute, which the container's start fails to run.
        let broken = dir.join("unexecutable");
        fs::write(&broken, "not a program").unwrap();
        Locals {
            image: image(),
            sandboxd: test_support::sandboxd(),
            mute: mute_sandboxd(dir),
            broken,
            unreachable_image: unpulled(),
            unreachable_sandboxd: dir.join("absent"),
        }
    }

    // Pausing freezes every process in the container, `sandboxd` among them, before it returns.
    fn stall(&self, id: &SandboxId) {
        run(&["pause", id.as_str()]);
    }

    fn held(&self, run: &RunTag) -> Vec<SandboxId> {
        let held = containers(run, "{{.ID}}").unwrap();
        held.into_iter().map(SandboxId::from).collect()
    }

    // The engine's ledger holds each run's cores.
    fn reserves(&self) -> bool {
        true
    }
}

// A create whose `docker create` hangs after the engine has made its container, as when the
// engine's answer never comes, fails once its deadline has passed, and the run's list finds the
// container it left: this test binary, run again with a stand-in `docker` first on its `PATH`
// that runs Docker's `create`, then hangs, and passes every other command to Docker's, makes
// the create.
#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
fn hung_create() {
    if let Some(dir) = env::var_os(STAND_IN) {
        let fixture = Fixture::new("hung-create-caller");
        let made = fixture.publish(
            &fixture.run,
            "hung",
            &fixture.sandboxd,
            &image(),
            false,
            false,
        );
        let (mut create, dial, _registration) = made.unwrap();
        // Long after the stand-in has started Docker's create, and long before its sleep would end.
        create.by = UnixTime::of(SystemTime::now() + Duration::from_secs(5)).unwrap();
        let failed = provider().create(&create, dial).unwrap_err();
        let listed = provider().list(&fixture.run).unwrap().len();
        let created = format!("the create failed: {failed}\nthe run's list then finds: {listed}");
        fs::write(Path::new(&dir).join("created"), created).unwrap();
        return;
    }
    check(
        "hung-create",
        |fixture| {
            let bin = fixture.dir.path().join("bin");
            fs::create_dir(&bin).unwrap();
            // `--context=<name>` comes first, and Docker's `docker` on `PATH` after `bin`. Docker's
            // create runs in a session of its own, which the kill at the deadline doesn't reach, so
            // it makes its container however slow the engine is, and the provider's create, which
            // waits for the output it holds, returns once it has ended.
            let script = "#!/bin/sh\nPATH=${PATH#*:}\n\
                          [ \"$2\" = create ] && { setsid --wait docker \"$@\"; exec sleep 60; }\n\
                          exec docker \"$@\"\n";
            let docker = bin.join("docker");
            fs::write(&docker, script).unwrap();
            fs::set_permissions(&docker, fs::Permissions::from_mode(0o755)).unwrap();
            let path = format!("{}:{}", bin.display(), env::var("PATH").unwrap());
            let kept = &[
                "HOME",
                "DOCKER_CONFIG",
                "PUMPKIN_DOCKER_CONTEXT",
                "PUMPKIN_SANDBOXD",
            ];
            let mut caller =
                stdx::process::command(env::current_exe().unwrap(), Environment::Only(kept));
            caller.args(["--exact", "hung_create", "--include-ignored"]);
            let caller = caller.env("PATH", path).env(STAND_IN, fixture.dir.path());
            let output = caller.output().unwrap();
            assert!(output.status.success(), "{output:?}");
            let created = fs::read_to_string(fixture.dir.path().join("created")).unwrap();
            created.lines().map(str::to_owned).collect()
        },
        &expect![[r#"
            the create failed: the deadline passed before `docker create` ended: check that the engine runs and isn't overloaded
            the run's list then finds: 1"#]],
    );
}

// A `sandboxd` that doesn't exist is named, and one whose path isn't UTF-8, which the engine's
// API can't carry, is refused; a container that Docker creates but can't start is removed; an
// image the engine doesn't have is refused, not pulled.
#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
fn failed_start() {
    check(
        "failed",
        |fixture| {
            let not_executable = fixture.dir.path().join("sandboxd");
            fs::write(&not_executable, "not a program").unwrap();
            let missing = fixture.dir.path().join("absent");
            let not_utf8 = fixture.dir.path().join(OsStr::from_bytes(b"sandboxd\xff"));
            fs::write(&not_utf8, "").unwrap();
            let attempts = [
                ("missing", missing.as_path(), image()),
                ("not_utf8", not_utf8.as_path(), image()),
                ("unexecutable", not_executable.as_path(), image()),
                ("unpulled", fixture.sandboxd.as_path(), unpulled()),
            ];
            let mut lines = Vec::new();
            for (role, sandboxd, image) in attempts {
                let failed = fixture.try_start(&fixture.run, role, sandboxd, &image, None, false);
                lines.push(format!("{role}: {}", redact(fixture, &failed.unwrap_err())));
            }
            let left = containers(&fixture.run, "{{.ID}}").unwrap();
            lines.push(format!("left: {left:?}"));
            lines
        },
        &expect![[r#"
            missing: publishing `sandboxd` failed: <dir>/absent can't be mounted at /.pumpkin/sandboxd: No such file or directory (os error 2): name a path that exists
            not_utf8: publishing `sandboxd` failed: <dir>/sandboxd� can't be mounted at /.pumpkin/sandboxd: name a path of UTF-8
            unexecutable: `docker start …: fix what Docker reports
            unpulled: the engine of context `<context>` has no image sha256:0000000000000000000000000000000000000000000000000000000000000000: build it there, or `docker load` an archive that holds it, as Pumpkin pulls none onto it
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

// Once a sandbox's container has stopped, its console holds what its `sandboxd` printed: here a
// stand-in's, which exits without dialing.
#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
fn console() {
    check(
        "console",
        |fixture| {
            let stand_in = mute_sandboxd(fixture.dir.path());
            let published =
                fixture.publish(&fixture.run, "stand-in", &stand_in, &image(), false, false);
            let (create, dial, _) = published.unwrap();
            let Created { id, report: _ } = provider().create(&create, dial).unwrap();
            // Docker finishes writing a container's log before it marks the container stopped,
            // which `docker wait` waits for, so the console then holds all the stand-in printed.
            run(&["wait", id.as_str()]);
            let console = provider().console(&fixture.run, &id).unwrap();
            vec![format!("console: {}", console.text.trim())]
        },
        &expect![[r#"
            console: sandboxd: a stand-in, which exits without dialing"#]],
    );
}

/// Runs `docker` with `args`, which must succeed.
fn run(args: &[&str]) {
    let output = docker().args(args).output().unwrap();
    assert!(output.status.success(), "{output:?}");
}

/// A `sandboxd` in `dir` that fails as it starts, saying why.
fn mute_sandboxd(dir: &Path) -> PathBuf {
    let mute = dir.join("stand-in-sandboxd");
    let script = "#!/bin/sh\necho 'sandboxd: a stand-in, which exits without dialing' >&2\n";
    fs::write(&mute, script).unwrap();
    fs::set_permissions(&mute, fs::Permissions::from_mode(0o755)).unwrap();
    mute
}

/// A shell function, `ctl`, that runs `sandboxctl` with its arguments, and shows how it exited
/// and what it printed.
const CTL: &str = r#"
    ctl() { out=$(/.pumpkin/bin/sandboxctl "$@" 2>&1); echo "\$ sandboxctl $*: exit $?"; [ -z "$out" ] || echo "  $out"; }"#;

// Commands in a sandbox that serves ops make them through `sandboxctl`, which a stand-in for
// the session's owner answers ([`reply`]), and the gateway refuses a payload over the limit
// the sandbox was started with. `sandboxd` made the control socket's directory closed to all
// but the role's user, and `sandboxctl` is read-only, so writing it fails the last command.
// The sandbox, created to serve ops but not the model endpoint, has no TCP listener.
#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the script and the snapshot"
)]
fn ops() {
    check(
        "ops",
        |fixture| {
            let payload = payload();
            let limit = OpPayloadLimit::try_from(u64::try_from(payload.len()).unwrap()).unwrap();
            let (sandboxd, ops) = (&fixture.sandboxd, Some(limit));
            let sandbox = fixture.try_start(&fixture.run, "ops", sandboxd, &image(), ops, false);
            let mut sandbox = sandbox.unwrap();
            let session = sandbox.session();
            let put = [Put::File(relative("payload"), file_mode(), &payload)];
            session
                .put(&absolute("/pumpkin/home"), put, hung())
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
                ctl lease open gpu
                ctl lease open gpu --seconds 30
                ctl lease open gpu --seconds 0
                ctl lease run 0 --timeout 5 -- echo -n hi
                ctl lease run 0 -- sh -c true
                ctl lease status 0
                ctl lease put 0 payload /work/in
                ctl lease put 0 payload /work/in --replace
                ctl lease get 0 /work/out ../got
                ctl lease put 0 payload work
                ctl lease close 0
                ctl lease run 0
                ctl lease close
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
            $ sandboxctl lease open gpu: exit 0
              {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":60,"cumulative_seconds":null,"commands":3}}
            $ sandboxctl lease open gpu --seconds 30: exit 0
              {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":60,"cumulative_seconds":null,"commands":3}}
            $ sandboxctl lease open gpu --seconds 0: exit 1
              sandboxctl: `0` is not a lease's time: give a whole number of seconds, 1 or more
            $ sandboxctl lease run 0 --timeout 5 -- echo -n hi: exit 0
              {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":60,"cumulative_seconds":null,"commands":3}}
            $ sandboxctl lease run 0 -- sh -c true: exit 0
              {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":60,"cumulative_seconds":null,"commands":3}}
            $ sandboxctl lease status 0: exit 0
              {"kind":"lease_status","lease":0,"state":"ready","elapsed_seconds":5,"busy_seconds":3,"idle_seconds":1,"failure":null,"last":{"kind":"command","command":0,"end":{"exited":{"code":0}},"stdout":"hi","stderr":""},"remaining":{"instances":0,"instance_seconds":60,"cumulative_seconds":null,"commands":3}}
            $ sandboxctl lease put 0 payload /work/in: exit 0
              {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":60,"cumulative_seconds":null,"commands":3}}
            $ sandboxctl lease put 0 payload /work/in --replace: exit 0
              {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":60,"cumulative_seconds":null,"commands":3}}
            $ sandboxctl lease get 0 /work/out ../got: exit 0
              {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":60,"cumulative_seconds":null,"commands":3}}
            $ sandboxctl lease put 0 payload work: exit 1
              sandboxctl: `work` is a path in the runner, which needs a whole one: start it with `/`
            $ sandboxctl lease close 0: exit 0
              {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":60,"cumulative_seconds":null,"commands":3}}
            $ sandboxctl lease run 0: exit 1
              sandboxctl: `lease run 0` is no lease op: give `open <grant> [--seconds <seconds>]`, `run <lease> [--timeout <seconds>] -- <program> [<arg>...]`, `put <lease> <tree> <to> [--replace]`, `get <lease> <from> <to>`, `status <lease>` or `close <lease>`
            $ sandboxctl lease close: exit 1
              sandboxctl: `lease close` is no lease op: give `open <grant> [--seconds <seconds>]`, `run <lease> [--timeout <seconds>] -- <program> [<arg>...]`, `put <lease> <tree> <to> [--replace]`, `get <lease> <from> <to>`, `status <lease>` or `close <lease>`
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
            owner: refused Judge { grant: Name("train") }, over 2621440 bytes
            owner: lease open gpu, None
            owner: lease open gpu, Some(30)
            owner: lease run 0: ["echo","-n","hi"], Some(5)
            owner: lease run 0: ["sh","-c","true"], None
            owner: lease status 0
            owner: lease put 0: /pumpkin/home/payload to /work/in, replace false
            owner: lease put 0: /pumpkin/home/payload to /work/in, replace true
            owner: lease get 0: /work/out to /pumpkin/got
            owner: lease close 0"#]],
    );
}

// Calls that `sandboxctl` or `sandboxd` refuse before an op opens, each failing its caller
// alone, so that the session serves the next call: among them one with `/proc/1/mem`, the
// memory of `sandboxd`, which can't open it, being non-dumpable. And a second `sandboxd`,
// with a session of its own, is refused the control socket's directory, since the first made
// it.
#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
fn refused_calls() {
    check(
        "refused",
        |fixture| {
            let ops = Some(OpPayloadLimit::try_from(64).unwrap());
            let (sandboxd, image) = (&fixture.sandboxd, image());
            let sandbox = fixture.try_start(&fixture.run, "refused", sandboxd, &image, ops, false);
            let mut sandbox = sandbox.unwrap();
            // The second `sandboxd`'s session, at a socket moved to where the sandbox dials.
            let name = Name::try_from("second".to_owned()).unwrap();
            let (_, second) = fixture.sockets.issue(&name).unwrap();
            let issued = fixture.dir.path().join("sockets/second").join(SOCKET_NAME);
            fs::rename(issued, fixture.dir.path().join("sockets/refused/second")).unwrap();
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
            // The command waits for the second `sandboxd`, which waits for its session to open,
            // so that session is accepted beside the owner's loop.
            let (status, arrived) = thread::scope(|scope| {
                let second = scope.spawn(move || {
                    let (arrivals, _) = mpsc::channel();
                    second.accept(hung(), None, false, arrivals, |arrival| arrival, || false)
                });
                let served = serve(&mut sandbox, process, &[]);
                drop(second.join().unwrap().unwrap());
                served
            });
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

// Callers whose ops arrive while one is in flight wait their turns, and each is carried once
// the op before it is answered. A caller that has left when its turn comes is dropped, its op
// unsent; one that leaves after its op was sent has it carried out. A payload's file is read
// in its caller's turn, so one replaced by a symbolic link while its caller waits is refused,
// as `sandboxd` opens no link.
#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
fn turns() {
    check(
        "turns",
        |fixture| {
            let ops = Some(OpPayloadLimit::try_from(64).unwrap());
            let sandboxd = &fixture.sandboxd;
            let sandbox = fixture.try_start(&fixture.run, "turns", sandboxd, &image(), ops, false);
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
            let held = [Put::Dir(relative("held"), dir_mode())];
            sandbox
                .session()
                .put(&absolute("/pumpkin/in"), held, hung())
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

// A harness's requests to the model endpoint of a sandbox created to serve both it and ops, from
// inside the sandbox, which has no network but loopback, and the test, as the session's owner,
// responding ([`respond`]). The endpoint listens on the loopback alone. Of eight requests at once,
// a full flight is held while the one past it is refused (503); then the flight is answered. A
// body far larger than its client's buffers arrives whole, though the client sends a byte past its
// request and reads only once `sandboxd` has stopped writing. Then a request is streamed whole,
// one cut short, one refused by the owner, and one ended without a head; one of HTTP/1.9 answered
// 204, then cut short, and one answered 103, an interim status, get nothing, and one of `HEAD` a
// head alone, though the owner sends data for each; and a chunked body, two lengths that differ, a
// length with a sign, a chunked body with a length, one over the protocol's bound, HTTP/1.0, of
// `HEAD` with a head alone and of `POST` with a body, and a head over the endpoint's bound are
// refused before they reach the session.
#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
fn model_endpoint() {
    check(
        "model",
        |fixture| {
            let ops = Some(OpPayloadLimit::try_from(64).unwrap());
            let sandboxd = &fixture.sandboxd;
            let sandbox = fixture.try_start(&fixture.run, "model", sandboxd, &image(), ops, true);
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
    let late = [Put::Dir(relative("late"), dir_mode())];
    sandbox
        .session()
        .put(&absolute("/pumpkin/in"), late, hung())
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
        lease @ (Op::LeaseOpen { .. }
        | Op::LeaseRun { .. }
        | Op::LeasePut { .. }
        | Op::LeaseGet { .. }
        | Op::LeaseStatus { .. }
        | Op::LeaseClose { .. }) => lease_reply(lease),
    };
    session.reply(incoming, answer, hung()).unwrap();
    asked
}

/// What a lease op asked, as the owner notes it, and the stand-in's answer.
fn lease_reply(op: &Op) -> (String, OpReply) {
    match op {
        Op::LeaseOpen {
            grant,
            instance_seconds,
        } => (
            format!("lease open {grant}, {instance_seconds:?}"),
            leased(0),
        ),
        Op::LeaseRun {
            lease,
            argv,
            timeout_seconds,
        } => {
            let asked = format!(
                "lease run {lease}: {}, {timeout_seconds:?}",
                serde_json::json!(argv)
            );
            (asked, leased(*lease))
        }
        Op::LeasePut {
            lease,
            tree,
            to,
            replace,
        } => (
            format!("lease put {lease}: {tree} to {to}, replace {replace}"),
            leased(*lease),
        ),
        Op::LeaseGet { lease, from, to } => {
            (format!("lease get {lease}: {from} to {to}"), leased(*lease))
        }
        &Op::LeaseStatus { lease } => {
            let command = CommandStatus {
                command: 0,
                end: Some(CommandEnd::Exited(ExitStatus::Code(0))),
                stdout: "hi".to_owned(),
                stderr: String::new(),
            };
            let status = LeaseStatusReply {
                lease,
                state: LeaseState::Ready,
                elapsed_seconds: 5,
                busy_seconds: 3,
                idle_seconds: 1,
                failure: None,
                last: Some(Last::Command(command)),
                remaining: LEASE_LEFT,
            };
            (
                format!("lease status {lease}"),
                OpReply::LeaseStatus(status),
            )
        }
        &Op::LeaseClose { lease } => (format!("lease close {lease}"), leased(lease)),
        other => panic!("{other:?} is no lease op"),
    }
}

/// What the stand-in's leases have left.
const LEASE_LEFT: LeaseRemaining = LeaseRemaining {
    instances: 0,
    instance_seconds: 60,
    cumulative_seconds: None,
    commands: Some(3),
};

/// The reply that names lease `lease`.
fn leased(lease: u64) -> OpReply {
    OpReply::Lease(LeaseReply {
        lease,
        remaining: LEASE_LEFT,
    })
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
