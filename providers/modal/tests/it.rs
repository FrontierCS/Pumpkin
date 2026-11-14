//! The Modal provider, against [`test_support::uv`], a stand-in for `uv` that runs the provider's
//! real helper with the system's Python and a stand-in for Modal's SDK, which reach nothing and
//! record what they were asked, beside [`test_support::aws`] and
//! [`test_support::stand_in_docker`]. The provider runs the `uv`, `aws` and `docker` it finds on
//! `PATH`, so each test connects, and makes each call, in a child, this test binary run again for
//! that test alone, with the stand-ins first on its `PATH`. The stand-in's sandboxes are
//! processes of this machine's, which run the `sandboxd` they attach behind a stand-in for Modal's
//! TLS edge. The tests that publish build their images on Docker, and the conformance suite runs
//! the static `sandboxd`, so they are ignored, and `cargo xtask ci --docker`, which reaches Docker
//! and builds that `sandboxd`, runs them.

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]

use std::ffi::OsStr;
use std::num::NonZeroU32;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};
use std::{env, fs, iter};

use engine::{ContextName, Engine};
use expect_test::{Expect, expect};
use modal::{Modal, Settings};
use provider::{
    Console, Create, Created, Creator, ImageRef, Listed, Provider, Role, RunTag, SandboxId,
    SandboxdRef, Sweep, UnixTime,
};
use schema::ir::{Image, Name};
use schema::sandbox::{Listening, RegistrationGrant};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use stdx::process::Environment;
use tempfile::TempDir;
use test_support::suite::{self, Locals, Subject};

/// The variable that makes this test binary a child that connects, naming the stand-in's
/// directory, from which it reads the settings and to which it writes how it went.
const CHILD: &str = "PUMPKIN_MODAL_STAND_IN";

/// Variables of Modal's and of `uv`'s, which the child has, and which must not reach the helper.
const STEERING: [(&str, &str); 4] = [
    ("MODAL_TOKEN_ID", "ak-elsewhere"),
    ("MODAL_PROFILE", "elsewhere"),
    ("MODAL_CONFIG_PATH", "/elsewhere/.modal.toml"),
    ("UV_INDEX", "https://elsewhere.example/simple"),
];

/// A directory of one test's own, which holds links to the stand-ins, named `uv`, `aws` and
/// `docker`, and the files the stand-ins and the child write and read. Dropping it removes it.
struct StandIn {
    dir: TempDir,
    /// The test's name, which runs it alone.
    test: String,
}

impl StandIn {
    fn new(test: &str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix(&format!("modal-{test}-"))
            .tempdir()
            .unwrap();
        let stand_ins = [
            ("uv", test_support::uv()),
            ("aws", test_support::aws()),
            ("docker", test_support::stand_in_docker()),
        ];
        for (command, stand_in) in stand_ins {
            std::os::unix::fs::symlink(stand_in, dir.path().join(command)).unwrap();
        }
        // The stand-in SDK's tunnels end TLS with them.
        fs::write(dir.path().join("edge.pem"), test_support::edge::CERTIFICATE).unwrap();
        fs::write(dir.path().join("edge.key"), test_support::edge::KEY).unwrap();
        Self {
            dir,
            test: test.to_owned(),
        }
    }

    /// The settings of the stand-ins' workspace and envelope, as JSON.
    fn settings() -> Value {
        json!({
            "profile": "pumpkin",
            "images": {
                "region": "us-west-1",
                "repository": "pumpkin/roles",
                "role_arn": "arn:aws:iam::123456789012:role/pumpkin-modal-pull",
            },
            "dials_from": "203.0.113.7",
            "classes": {
                "judge": {
                    "cpu_millis": 2000,
                    "memory_bytes": 4_294_967_296_u64,
                    "startup_seconds": 600,
                },
                "h100": {
                    "gpu": "H100!:2",
                    "gpu_model": "NVIDIA H100 80GB HBM3",
                    "cpu_millis": 4000,
                    "memory_bytes": 17_179_869_184_u64,
                    "startup_seconds": 1800,
                    "usd_per_hour": 7.9,
                },
            },
        })
    }

    /// Connects with `settings`, in a child with the stand-ins first on its `PATH`, then makes
    /// `call`, if any, and returns what the provider declared, as JSON, or its error, and what
    /// the call returned.
    fn connect(&self, settings: &Value, call: Option<&Call>) -> (Result<String, String>, Value) {
        self.connect_on(&self.path(), settings, call)
    }

    /// Makes `call` once connected with the stand-ins' settings, and returns what it returned.
    fn call(&self, call: &Call) -> Value {
        let (connected, returned) = self.connect(&Self::settings(), Some(call));
        connected.unwrap();
        returned
    }

    /// Connects, and makes `call`, as [`connect`] does, in a child whose `PATH` is `path`.
    ///
    /// [`connect`]: Self::connect
    fn connect_on(
        &self,
        path: &OsStr,
        settings: &Value,
        call: Option<&Call>,
    ) -> (Result<String, String>, Value) {
        fs::write(self.dir.path().join("settings"), settings.to_string()).unwrap();
        if let Some(call) = call {
            fs::write(
                self.dir.path().join("call"),
                serde_json::to_vec(call).unwrap(),
            )
            .unwrap();
        }
        self.run_child(path);
        let read = |file: &str| {
            let path = self.dir.path().join(file);
            let fix = "name each `check` after its test";
            assert!(path.exists(), "the child wrote no {file}: {fix}");
            let read = fs::read(&path).unwrap();
            fs::remove_file(path).unwrap();
            read
        };
        let connected = serde_json::from_slice(&read("connected")).unwrap();
        let returned = match call {
            Some(_) => {
                fs::remove_file(self.dir.path().join("call")).unwrap();
                serde_json::from_slice(&read("returned")).unwrap()
            }
            None => Value::Null,
        };
        (connected, returned)
    }

    /// Runs this test alone, in a child whose `PATH` is `path`, and returns once it has passed.
    fn run_child(&self, path: &OsStr) {
        let mut child = stdx::process::command(env::current_exe().unwrap(), Environment::Inherited);
        child.args(["--exact", &self.test, "--include-ignored"]);
        child.env(CHILD, self.dir.path()).env("PATH", path);
        let output = child.envs(STEERING).output().unwrap();
        assert!(output.status.success(), "the child failed: {output:?}");
    }

    /// The `PATH` with the stand-ins first.
    fn path(&self) -> std::ffi::OsString {
        let path = env::var_os("PATH").unwrap_or_default();
        let dirs = iter::once(self.dir.path().to_owned()).chain(env::split_paths(&path));
        env::join_paths(dirs).unwrap()
    }

    /// Makes the stand-in `docker` run Docker's own, and the stand-in `aws` answer that the
    /// repository holds whatever image it is asked about.
    fn hold_images(&self) {
        fs::write(self.dir.path().join("engine"), "").unwrap();
        let held = r#"digest=$(printf '%s\n' "$@" | sed --quiet 's/^imageDigest=//p')
echo '{"images": [{"imageId": {"imageDigest": "'"$digest"'"}}], "failures": []}'"#;
        fs::write(self.dir.path().join("ecr batch-get-image"), held).unwrap();
    }

    /// Makes the stand-in `uv` run `script` with `sh` in place of the helper.
    fn helper(&self, script: &str) {
        fs::write(self.dir.path().join("helper"), script).unwrap();
    }

    /// The calls the stand-ins recorded since the last, in the order made, each as its command,
    /// `uv`, `aws` or `docker`, and its arguments, then those to the stand-in for Modal's SDK.
    fn calls(&self) -> Vec<String> {
        let taken = |file: &str| {
            let path = self.dir.path().join(file);
            let recorded = fs::read_to_string(&path).unwrap_or_default();
            let _ = fs::remove_file(path);
            recorded
        };
        let recorded = taken("calls");
        let calls = (recorded.split_terminator("\n\n"))
            .map(|call| call.lines().collect::<Vec<_>>().join(" "));
        let mut calls: Vec<String> = calls.collect();
        calls.extend(taken("modal-calls").lines().map(str::to_owned));
        let dir = self.dir.path().to_str().unwrap();
        let manifest = env!("CARGO_MANIFEST_DIR");
        let shown = |call: String| call.replace(manifest, "<crate>").replace(dir, "<dir>");
        calls.into_iter().map(shown).collect()
    }

    /// The names in the environment of the helper's last call, but those it is given.
    fn environment(&self) -> Vec<String> {
        let names = fs::read_to_string(self.dir.path().join("uv-env")).unwrap();
        let own = ["HOME", "PATH", "MODAL_PROFILE"];
        let other = (names.split_whitespace()).filter(|name| !own.contains(name));
        other.map(str::to_owned).collect()
    }
}

impl Drop for StandIn {
    fn drop(&mut self) {
        // A stand-in sandbox still running, as when its test failed, is killed; then every
        // container of the stand-in's is removed, by its label, which takes one too that the
        // engine made after its sandbox's terminate had removed it, as a create it finished
        // after its client was killed.
        for (_, _, pid) in sandboxes(self.dir.path()) {
            signal("KILL", pid);
        }
        let label = format!("label=pumpkin-modal-stand-in={}", self.dir.path().display());
        let listed = (test_support::docker())
            .args(["container", "ls", "--all", "--quiet", "--filter", &label])
            .output();
        let Ok(listed) = listed else {
            return;
        };
        let ids = String::from_utf8_lossy(&listed.stdout);
        let ids: Vec<&str> = ids.split_whitespace().collect();
        if !ids.is_empty() {
            let _ = test_support::docker()
                .args(["rm", "--force"])
                .args(&ids)
                .output();
        }
    }
}

/// The stand-in SDK's sandboxes in `dir` that run, each by its id, its run, and the ID of the
/// process that leads its group.
fn sandboxes(dir: &Path) -> Vec<(String, String, u32)> {
    let mut running = Vec::new();
    for sandbox in fs::read_dir(dir.join("sandboxes")).into_iter().flatten() {
        let sandbox = sandbox.unwrap().path();
        if sandbox.join("returncode").exists() {
            continue;
        }
        let pid = fs::read_to_string(sandbox.join("pid")).unwrap();
        let tags: Value =
            serde_json::from_slice(&fs::read(sandbox.join("tags.json")).unwrap()).unwrap();
        let run = tags["pumpkin-run"].as_str().unwrap().to_owned();
        let id = sandbox.file_name().unwrap().to_str().unwrap().to_owned();
        running.push((id, run, pid.trim().parse().unwrap()));
    }
    running.sort();
    running
}

/// The container of the stand-in SDK's sandbox `id` in `dir`, if it runs in one.
fn container(dir: &Path, id: &str) -> Option<String> {
    let sandbox = dir.join("sandboxes").join(id);
    let request: Value =
        serde_json::from_slice(&fs::read(sandbox.join("request.json")).ok()?).ok()?;
    request["digest"].as_str()?;
    fs::read_to_string(sandbox.join("container")).ok()
}

/// Sends `signal`, such as `KILL`, to the process group that process `pid`, which the stand-in
/// SDK started, leads.
fn signal(signal: &str, pid: u32) {
    let mut kill = stdx::process::command("kill", Environment::Inherited);
    let _ = kill
        .args([&format!("-{signal}"), "--", &format!("-{pid}")])
        .output();
}

/// Runs `scenario`, the test `name`'s, against a stand-in of its own, and checks the lines it
/// reports against `expect`; or, in a child that [`StandIn::connect`] runs, connects.
fn check(name: &str, scenario: impl FnOnce(&StandIn) -> Vec<String>, expect: &Expect) {
    if let Some(dir) = env::var_os(CHILD) {
        return child(Path::new(&dir));
    }
    let stand_in = StandIn::new(name);
    expect.assert_eq(&scenario(&stand_in).join("\n"));
}

/// As the child: connects with the settings in `dir`, and the engine of the tests' context, with
/// the crate's helper, and writes there what the provider declared, or its error; then, if `dir`
/// holds a [`Call`], makes it, and writes there what it returned, or its error. The stand-ins'
/// calls then hold the call's alone.
fn child(dir: &Path) {
    let settings = serde_json::from_slice(&fs::read(dir.join("settings")).unwrap()).unwrap();
    let settings = serde_json::from_value::<Settings>(settings).map_err(|error| error.to_string());
    let context = ContextName::try_from(test_support::docker_context()).unwrap();
    // Reached only past settings that parse, as `pumpkin` reaches it before it connects.
    let engine = (settings.as_ref().map_err(Clone::clone))
        .and_then(|_| Engine::connect(context, &dir.join(".pumpkin/docker")));
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("helper/pumpkin-modal.py");
    let connected = settings.and_then(|settings| {
        let engine = engine.as_ref().map_err(Clone::clone)?;
        Modal::connect(
            &settings,
            &script,
            &dir.join(".pumpkin/modal"),
            engine.pusher()?,
        )
    });
    let declared = (connected.as_ref().map_err(Clone::clone))
        .map(|modal| serde_json::to_string_pretty(modal.ready()).unwrap());
    let write = |file: &str, value: &Result<Value, String>| {
        fs::write(dir.join(file), serde_json::to_vec(value).unwrap()).unwrap();
    };
    write("connected", &declared.map(Value::String));
    if let (Ok(modal), Ok(call)) = (connected, fs::read(dir.join("call"))) {
        for calls in ["calls", "modal-calls"] {
            let _ = fs::remove_file(dir.join(calls));
        }
        let returned = called(&modal, serde_json::from_slice(&call).unwrap());
        write("returned", &returned);
    }
}

/// A call that a child makes once it has connected: a publish of a role's image or of a
/// `sandboxd`; a create, as [`asked`] reads it; or a call on run `run-1`'s sandboxes.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Call {
    PublishImage(String),
    PublishSandboxd(PathBuf),
    Create(Value),
    List,
    Delete(Vec<String>),
    Console(String),
}

/// What `modal` returns of `call`, as JSON.
fn called(modal: &Modal<'_>, call: Call) -> Result<Value, String> {
    let run = RunTag::try_from("run-1".to_owned()).unwrap();
    match call {
        Call::PublishImage(image) => {
            let published = modal.publish_image(&Image::try_from(image).unwrap());
            published.map(|published| json!(published.as_str()))
        }
        Call::PublishSandboxd(path) => {
            let published = modal.publish_sandboxd(&path);
            published.map(|published| json!(published.as_str()))
        }
        Call::Create(create) => {
            let listening = Listening {
                grant: RegistrationGrant::from([0x5a; 16]),
            };
            let created = modal.create(&asked(&create), listening);
            created.map(|(Created { id, report }, endpoint)| {
                json!({"id": id.as_str(), "report": report, "endpoint": endpoint.to_string()})
            })
        }
        Call::List => modal.list(&run).map(|listed| {
            let listed = listed.into_iter();
            let listed =
                listed.map(|Listed { id, ended }| json!({"id": id.as_str(), "ended": ended}));
            listed.collect()
        }),
        Call::Delete(ids) => {
            let ids: Vec<SandboxId> = ids.into_iter().map(SandboxId::from).collect();
            modal.delete(&run, &ids).map(|()| Value::Null)
        }
        Call::Console(id) => {
            let read = modal.console(&run, &SandboxId::from(id));
            read.map(|Console { text }| json!(text))
        }
    }
}

/// The create that `create` asks for, as JSON: `run`, or `run-1`, `name`, `class`, `sandboxd`,
/// what a publish of one returned, of a role of 1.5 CPUs and 256 MiB that makes ops and serves
/// the model endpoint, from image `im-0`, of the digest of all zeros, by a deadline a minute
/// away; and `hard_end`, in seconds since the Unix epoch, or an hour away if it is left out.
fn asked(create: &Value) -> Create {
    let text = |key: &str| create[key].as_str().unwrap().to_owned();
    let name = Name::try_from(text("name")).unwrap();
    let run = create
        .get("run")
        .map_or("run-1".to_owned(), |run| run.as_str().unwrap().to_owned());
    let at = |since: Duration| UnixTime::of(UNIX_EPOCH + since).unwrap();
    let hard_end = (create.get("hard_end")).map_or(since_epoch(Duration::from_hours(1)), |end| {
        Duration::from_secs(end.as_u64().unwrap())
    });
    Create {
        run: RunTag::try_from(run).unwrap(),
        name: name.clone(),
        role: Role {
            name,
            user: serde_json::from_value(json!({"uid": 4242, "gid": 4243})).unwrap(),
            resources: serde_json::from_value(
                create
                    .get("resources")
                    .cloned()
                    .unwrap_or(json!({"cpu_millis": 1500, "memory_bytes": 268_435_456})),
            )
            .unwrap(),
            instances: NonZeroU32::MIN,
        },
        instance: 0,
        class: Name::try_from(text("class")).unwrap(),
        image: ImageRef::from(format!("im-0@sha256:{}", "0".repeat(64))),
        sandboxd: SandboxdRef::from(text("sandboxd")),
        ops: true,
        model: true,
        hard_end: at(hard_end),
        by: at(
            (create.get("by")).map_or(since_epoch(Duration::from_mins(1)), |by| {
                Duration::from_secs(by.as_u64().unwrap())
            }),
        ),
    }
}

/// The time `after` from now, since the Unix epoch.
fn since_epoch(after: Duration) -> Duration {
    UNIX_EPOCH.elapsed().unwrap() + after
}

// What the provider declares of the stand-ins' workspace and repository, the calls that
// reported it, and what of this process's environment the helper got beyond what it is given:
// nothing of Modal's or of `uv`'s, though the child has `MODAL_TOKEN_ID`, `MODAL_PROFILE`,
// `MODAL_CONFIG_PATH` and `UV_INDEX`, but the provider's directories for `uv` and for temporary
// files; and with `config` in the settings, that file too.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the declaration's snapshot"
)]
fn connect() {
    check(
        "connect",
        |stand_in| {
            let (declared, _) = stand_in.connect(&StandIn::settings(), None);
            let mut declared: Value = serde_json::from_str(&declared.unwrap()).unwrap();
            // The system's, which runs the helper here.
            declared["report"]["python"] = json!("<python>");
            let mut lines = vec![serde_json::to_string_pretty(&declared).unwrap()];
            lines.extend(stand_in.calls());
            lines.push(format!(
                "the helper's other variables: {:?}",
                stand_in.environment()
            ));
            let mut settings = StandIn::settings();
            let config = stand_in.dir.path().join("modal.toml");
            fs::write(&config, "[pumpkin]\ntoken_id = \"ak-x\"\n").unwrap();
            settings["config"] = json!(config);
            let (declared, _) = stand_in.connect(&settings, None);
            declared.unwrap();
            stand_in.calls();
            lines.push(format!(
                "with `config`, the helper's other variables: {:?}",
                stand_in.environment()
            ));
            lines
        },
        &expect![[r#"
            {
              "classes": {
                "h100": {
                  "offers": {
                    "cpu_features": [],
                    "cpu_millis": 4000,
                    "cpu_model": null,
                    "gpu": {
                      "count": 2,
                      "model": "NVIDIA H100 80GB HBM3"
                    },
                    "memory_bytes": 17179869184
                  },
                  "startup_seconds": 1800,
                  "usd_per_hour": 7.9
                },
                "judge": {
                  "offers": {
                    "cpu_features": [],
                    "cpu_millis": 2000,
                    "cpu_model": null,
                    "gpu": null,
                    "memory_bytes": 4294967296
                  },
                  "startup_seconds": 600
                }
              },
              "report": {
                "aws": "aws-cli/2.38.1 Python/3.14.6 Linux/7.0.0 exe/x86_64.stand-in",
                "dials_from": "203.0.113.7",
                "docker": {
                  "client_version": "29.9.2",
                  "context": "default",
                  "endpoint": "unix:///nonexistent/docker-stand-in.sock",
                  "engine_version": "29.9.2",
                  "kernel": "7.0.0",
                  "os": "stand-in",
                  "platform": "linux/amd64",
                  "security": {
                    "apparmor": false,
                    "rootless": false,
                    "seccomp_profile": "builtin",
                    "userns": false
                  }
                },
                "images": {
                  "region": "us-west-1",
                  "repository": "pumpkin/roles",
                  "role_arn": "arn:aws:iam::123456789012:role/pumpkin-modal-pull"
                },
                "modal": "1.6.3",
                "profile": "pumpkin",
                "python": "<python>",
                "repository": {
                  "name": "pumpkin/roles",
                  "uri": "123456789012.dkr.ecr.us-west-1.amazonaws.com/pumpkin/roles"
                },
                "workspace": "pumpkin-workspace"
              }
            }
            uv run --frozen --quiet --no-config --script <crate>/helper/pumpkin-modal.py
            aws --version
            aws --no-cli-pager --output json --region us-west-1 ecr describe-repositories --repository-names pumpkin/roles
            modal: Workspace.hydrate {"profile": "pumpkin"}
            the helper's other variables: ["TMPDIR", "UV_CACHE_DIR", "UV_PYTHON_INSTALL_DIR"]
            with `config`, the helper's other variables: ["MODAL_CONFIG_PATH", "TMPDIR", "UV_CACHE_DIR", "UV_PYTHON_INSTALL_DIR"]"#]],
    );
}

// Settings refused as they parse: with a configuration file not named by an absolute path, with
// a GPU whose count is no number, or none, or of no model, and with a field they lack; and a
// connect refused: with a profile that Modal's configuration lacks, a repository that AWS lacks, a helper that fails, one that prints what isn't an answer, one
// that answers an error, and with no `uv` on `PATH`.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn refusals() {
    check(
        "refusals",
        |stand_in| {
            let mut lines = Vec::new();
            let with = |pointer: &str, value: Value| {
                let mut settings = StandIn::settings();
                *settings.pointer_mut(pointer).unwrap() = value;
                settings
            };
            let mut odd = StandIn::settings();
            odd["images"]["tag"] = json!("latest");
            let mut unmodeled = StandIn::settings();
            unmodeled["classes"]["h100"]
                .as_object_mut()
                .unwrap()
                .remove("gpu_model");
            let mut relative = StandIn::settings();
            relative["config"] = json!("modal.toml");
            let cases = [
                ("a relative configuration file", relative),
                (
                    "a GPU count that is no number",
                    with("/classes/h100/gpu", json!("H100:abc")),
                ),
                (
                    "a GPU count of none",
                    with("/classes/h100/gpu", json!("H100:0")),
                ),
                ("a GPU of no model", unmodeled),
                (
                    "a GPU's model that names none",
                    with("/classes/h100/gpu_model", json!("")),
                ),
                ("a field they lack", odd),
                ("a profile Modal lacks", with("/profile", json!("unknown"))),
                (
                    "a repository AWS lacks",
                    with("/images/repository", json!("pumpkin/other")),
                ),
            ];
            for (case, settings) in cases {
                let (connected, _) = stand_in.connect(&settings, None);
                lines.push(format!("{case}: {}", connected.unwrap_err()));
            }
            let helpers = [
                ("a helper that fails", "echo 'Traceback: boom' >&2; exit 1"),
                ("one that prints what isn't an answer", "echo nonsense"),
                (
                    "one that answers an error",
                    r#"echo '{"error": "AuthError: token expired"}'"#,
                ),
            ];
            for (case, script) in helpers {
                stand_in.helper(script);
                let (connected, _) = stand_in.connect(&StandIn::settings(), None);
                lines.push(format!("{case}: {}", connected.unwrap_err()));
            }
            // A `PATH` of a directory with the stand-ins but `uv`.
            let no_uv = stand_in.dir.path().join("no-uv");
            fs::create_dir(&no_uv).unwrap();
            for command in ["aws", "docker"] {
                let link = std::os::unix::fs::symlink(
                    stand_in.dir.path().join(command),
                    no_uv.join(command),
                );
                link.unwrap();
            }
            let settings = StandIn::settings();
            let (connected, _) = stand_in.connect_on(no_uv.as_os_str(), &settings, None);
            lines.push(format!("no uv on PATH: {}", connected.unwrap_err()));
            let dir = stand_in.dir.path().to_str().unwrap();
            let manifest = env!("CARGO_MANIFEST_DIR");
            let shown = |line: String| line.replace(manifest, "<crate>").replace(dir, "<dir>");
            lines.into_iter().map(shown).collect()
        },
        &expect![[r#"
            a relative configuration file: `modal.toml` is not an absolute path: name Modal's configuration file by one
            a GPU count that is no number: GPU `H100:abc` has the count `abc`, which is not a whole number of GPUs from 1 to 4294967295: give one, as in `A100-80GB:2`
            a GPU count of none: GPU `H100:0` has the count `0`, which is not a whole number of GPUs from 1 to 4294967295: give one, as in `A100-80GB:2`
            a GPU of no model: a class names one of `gpu` and `gpu_model`: name both, as `gpu = "H100!"` and `gpu_model = "NVIDIA H100 80GB HBM3"`, or neither
            a GPU's model that names none: `` is not a GPU's model: use one as `nvidia-smi` names it, such as `NVIDIA H100 80GB HBM3`
            a field they lack: unknown field `tag`, expected one of `region`, `repository`, `role_arn`
            a profile Modal lacks: Modal refused the helper's `ready`: AuthError: Modal profile 'unknown' was not found in ~/.modal.toml. Run `modal profile list` to see available profiles: fix what Modal reports
            a repository AWS lacks: AWS described no repository `pumpkin/other`: expected each repository named to exist
            a helper that fails: the Modal helper `<crate>/helper/pumpkin-modal.py`'s `ready` failed with exit status: 1: Traceback: boom: fix what it reports
            one that prints what isn't an answer: the Modal helper's `ready` printed what isn't its answer: expected value at line 1 column 1: expected `{"ok": …}`, of the form the call returns, or `{"error": "…"}`
            one that answers an error: Modal refused the helper's `ready`: AuthError: token expired: fix what Modal reports
            no uv on PATH: `uv` didn't run: No such file or directory (os error 2): install uv on `PATH`"#]],
    );
}

// Publishing a role's image, which the repository holds already, as the stand-in `aws` answers
// here: the image is checked on the engine before any other call, then looked up in the
// repository, then imported into Modal from there by its digest, through the settings' role,
// and its id and the digest come back; the same image again gets the same id, and another
// another; an image the engine lacks, and one for another platform, are refused before any call
// to AWS or Modal; and an import that Modal refuses fails the publish.
#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the publishes' snapshot"
)]
fn publish() {
    check(
        "publish",
        |stand_in| {
            stand_in.hold_images();
            let built = Built::new(stand_in.dir.path(), &stand_in.test);
            let unpulled = format!("sha256:{}", "0".repeat(64));
            let mut lines = Vec::new();
            let mut ids = Vec::new();
            let mut publish = |what: &str, image: &str| {
                let call = Call::PublishImage(image.to_owned());
                let (connected, published) = stand_in.connect(&StandIn::settings(), Some(&call));
                connected.unwrap();
                // The stand-in's id, of the reference, whose digest is this run's.
                let published = match published["Ok"].as_str().and_then(|id| id.split_once('@')) {
                    Some((id, digest)) => {
                        ids.push(id.to_owned());
                        json!({"Ok": format!("im-<id>@{digest}")})
                    }
                    None => published,
                };
                lines.push(format!("{what}: {published}"));
                lines.extend(stand_in.calls().iter().map(|call| format!("  {call}")));
            };
            publish("a role's image", &built.role);
            publish("again", &built.role);
            publish("another", &built.other);
            publish("an image the engine lacks", &unpulled);
            publish("an image for another platform", &built.foreign);
            fs::write(
                stand_in.dir.path().join("import-fails"),
                "the registry refused the role",
            )
            .unwrap();
            publish("an import Modal refuses", &built.role);
            lines.push(format!("again, the same id: {}", ids[0] == ids[1]));
            lines.push(format!("another, another id: {}", ids[0] != ids[2]));
            let names = [
                (built.role.as_str(), "<role>"),
                (built.other.as_str(), "<other>"),
                (built.foreign.as_str(), "<foreign>"),
            ];
            let shown = |line: &String| {
                (names.iter()).fold(line.clone(), |line, (from, to)| line.replace(from, to))
            };
            lines.iter().map(shown).collect()
        },
        &expect![[r#"
            a role's image: {"Ok":"im-<id>@<role>"}
              docker --context=default image inspect --format {{json .}} -- <role>
              aws --no-cli-pager --output json --region us-west-1 ecr batch-get-image --repository-name pumpkin/roles --image-ids imageDigest=<role> --accepted-media-types application/vnd.oci.image.manifest.v1+json application/vnd.docker.distribution.manifest.v2+json
              uv run --frozen --quiet --no-config --script <crate>/helper/pumpkin-modal.py
              modal: App.lookup {"create_if_missing": true, "name": "pumpkin-images"}
              modal: Image.build {"app": "pumpkin-images", "reference": "123456789012.dkr.ecr.us-west-1.amazonaws.com/pumpkin/roles@<role>", "secret": {"AWS_REGION": "us-west-1", "AWS_ROLE_ARN": "arn:aws:iam::123456789012:role/pumpkin-modal-pull"}}
            again: {"Ok":"im-<id>@<role>"}
              docker --context=default image inspect --format {{json .}} -- <role>
              aws --no-cli-pager --output json --region us-west-1 ecr batch-get-image --repository-name pumpkin/roles --image-ids imageDigest=<role> --accepted-media-types application/vnd.oci.image.manifest.v1+json application/vnd.docker.distribution.manifest.v2+json
              uv run --frozen --quiet --no-config --script <crate>/helper/pumpkin-modal.py
              modal: App.lookup {"create_if_missing": true, "name": "pumpkin-images"}
              modal: Image.build {"app": "pumpkin-images", "reference": "123456789012.dkr.ecr.us-west-1.amazonaws.com/pumpkin/roles@<role>", "secret": {"AWS_REGION": "us-west-1", "AWS_ROLE_ARN": "arn:aws:iam::123456789012:role/pumpkin-modal-pull"}}
            another: {"Ok":"im-<id>@<other>"}
              docker --context=default image inspect --format {{json .}} -- <other>
              aws --no-cli-pager --output json --region us-west-1 ecr batch-get-image --repository-name pumpkin/roles --image-ids imageDigest=<other> --accepted-media-types application/vnd.oci.image.manifest.v1+json application/vnd.docker.distribution.manifest.v2+json
              uv run --frozen --quiet --no-config --script <crate>/helper/pumpkin-modal.py
              modal: App.lookup {"create_if_missing": true, "name": "pumpkin-images"}
              modal: Image.build {"app": "pumpkin-images", "reference": "123456789012.dkr.ecr.us-west-1.amazonaws.com/pumpkin/roles@<other>", "secret": {"AWS_REGION": "us-west-1", "AWS_ROLE_ARN": "arn:aws:iam::123456789012:role/pumpkin-modal-pull"}}
            an image the engine lacks: {"Err":"the engine of context `default` has no image sha256:0000000000000000000000000000000000000000000000000000000000000000: build it there, or `docker load` an archive that holds it, as Pumpkin pulls none onto it"}
              docker --context=default image inspect --format {{json .}} -- sha256:0000000000000000000000000000000000000000000000000000000000000000
            an image for another platform: {"Err":"<foreign> is an image for linux/arm64, but Modal runs sandboxes on linux/amd64: name an image for linux/amd64"}
              docker --context=default image inspect --format {{json .}} -- <foreign>
            an import Modal refuses: {"Err":"Modal refused the helper's `import`: InvalidError: the registry refused the role: fix what Modal reports"}
              docker --context=default image inspect --format {{json .}} -- <role>
              aws --no-cli-pager --output json --region us-west-1 ecr batch-get-image --repository-name pumpkin/roles --image-ids imageDigest=<role> --accepted-media-types application/vnd.oci.image.manifest.v1+json application/vnd.docker.distribution.manifest.v2+json
              uv run --frozen --quiet --no-config --script <crate>/helper/pumpkin-modal.py
              modal: App.lookup {"create_if_missing": true, "name": "pumpkin-images"}
              modal: Image.build {"app": "pumpkin-images", "reference": "123456789012.dkr.ecr.us-west-1.amazonaws.com/pumpkin/roles@<role>", "secret": {"AWS_REGION": "us-west-1", "AWS_ROLE_ARN": "arn:aws:iam::123456789012:role/pumpkin-modal-pull"}}
            again, the same id: true
            another, another id: true"#]],
    );
}

/// The images that the publish test builds, each holding a file of this process's own, so that
/// no other test's image goes when they are removed, as dropping them removes them: a role's
/// image for `linux/amd64`, on `busybox`, whose `sleep` the suite runs, another, and one for
/// `linux/arm64`.
struct Built {
    role: String,
    other: String,
    foreign: String,
}

impl Built {
    /// The images, built in `dir`, test `test`'s.
    fn new(dir: &Path, test: &str) -> Self {
        let context = dir.join("context");
        fs::create_dir(&context).unwrap();
        let build = |content: &str, platform: &str, base: &str| {
            let recipe = format!("FROM {base}\nCOPY file /\n");
            fs::write(context.join("Dockerfile"), recipe).unwrap();
            let file = format!("{content} of {}-{test}\n", std::process::id());
            fs::write(context.join("file"), file).unwrap();
            let build = test_support::docker()
                .args(["build", "--quiet", "--provenance=false"])
                .arg(format!("--platform={platform}"))
                .arg(&context)
                .output()
                .unwrap();
            assert!(build.status.success(), "{build:?}");
            String::from_utf8(build.stdout).unwrap().trim().to_owned()
        };
        Self {
            role: build("role", "linux/amd64", test_support::BUSYBOX),
            other: build("other", "linux/amd64", "scratch"),
            foreign: build("foreign", "linux/arm64", "scratch"),
        }
    }
}

impl Drop for Built {
    fn drop(&mut self) {
        for image in [&self.role, &self.other, &self.foreign] {
            let _ = test_support::docker()
                .args(["image", "rm", "--force", image])
                .output();
        }
    }
}

// Creating a sandbox in class `h100`, which the stand-in SDK runs, with a `sandboxd` that prints
// its arguments, then waits: the calls to Modal ask for Pumpkin's App, its run's and
// its role's tags, its image with `sandboxd`, and `sandboxctl` too, attached, `sandboxd`
// listening as the role's user, the grant, its timeout, the class's GPU, the role's CPU and
// memory as request and limit, gVisor, no connection out, and a tunnel that takes connections
// from the settings' address alone, its timeout the seconds to the hard end, exactly, as the
// helper reads the clock as it creates; the create returns its id, its report and the tunnel's
// endpoint. The run's list finds it, and its console holds what it printed; another run's
// sandbox can be neither read nor deleted; a delete of it and of an id no sandbox has is done,
// and the list then finds nothing. Refused before any call to Modal: a create that dials a
// socket, whose role's memory isn't whole MiB, or of a class the provider lacks; and by the
// helper, one whose hard end has passed, or whose `sandboxd` has changed since it was published.
// A sandbox whose `sandboxd` prints, then exits with status 137, as one the kernel's OOM killer
// ends does, has a console that ends with that status.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the calls and their snapshot"
)]
fn creates() {
    check(
        "creates",
        |stand_in| {
            let sandboxd = stand_in.dir.path().join("sandboxd");
            let script = b"#!/bin/sh\necho \"sandboxd $*\"\nexec sleep 300\n";
            fs::write(&sandboxd, script).unwrap();
            fs::set_permissions(&sandboxd, fs::Permissions::from_mode(0o755)).unwrap();
            let published = stand_in.call(&Call::PublishSandboxd(sandboxd.clone()));
            let mut lines = vec![format!("published: {published}")];
            let sandboxd_ref = published["Ok"].as_str().unwrap().to_owned();
            // The helper's clock an hour before the hard end, so the timeout is exactly that.
            let hard_end = since_epoch(Duration::from_hours(1)).as_secs();
            fs::write(
                stand_in.dir.path().join("clock"),
                (hard_end - 3600).to_string(),
            )
            .unwrap();
            let create = |changes: Value| {
                let mut create = json!({
                    "name": "score",
                    "class": "h100",
                    "sandboxd": sandboxd_ref,
                    "hard_end": hard_end,
                    "by": hard_end - 3540,
                });
                create
                    .as_object_mut()
                    .unwrap()
                    .extend(changes.as_object().unwrap().clone());
                Call::Create(create)
            };
            lines.push(format!("created: {}", stand_in.call(&create(json!({})))));
            lines.extend(stand_in.calls());
            lines.push(format!("listed: {}", stand_in.call(&Call::List)));
            lines.push(format!(
                "its console: {}",
                stand_in.call(&Call::Console("sb-1".to_owned()))
            ));
            let theirs = stand_in.call(&create(json!({"run": "run-2"})));
            lines.push(format!("another run's: {theirs}"));
            let read = stand_in.call(&Call::Console("sb-2".to_owned()));
            lines.push(format!("its console, as run-1's: {read}"));
            let deleted = stand_in.call(&Call::Delete(vec!["sb-2".to_owned()]));
            lines.push(format!("deleting it as run-1's: {deleted}"));
            let ids = vec!["sb-1".to_owned(), "pumpkin-none".to_owned()];
            lines.push(format!(
                "deleting the sandbox and an id no sandbox has: {}",
                stand_in.call(&Call::Delete(ids))
            ));
            lines.push(format!("listed: {}", stand_in.call(&Call::List)));
            let past = hard_end - 3600 - 1;
            let refusals = [
                ("a hard end past", json!({"hard_end": past})),
                (
                    "memory not whole MiB",
                    json!({"resources": {"cpu_millis": 1500, "memory_bytes": 1_000_000}}),
                ),
                ("a class the provider lacks", json!({"class": "tpu"})),
            ];
            for (case, changes) in refusals {
                let refused = stand_in.call(&create(changes));
                let modal = stand_in
                    .calls()
                    .iter()
                    .filter(|call| call.starts_with("uv "))
                    .count();
                lines.push(format!("{case}: {refused}, calls to Modal: {modal}"));
            }
            fs::write(&sandboxd, "#!/bin/sh\nexit 0\n").unwrap();
            lines.push(format!(
                "a sandboxd changed since published: {}",
                stand_in.call(&create(json!({})))
            ));
            fs::write(&sandboxd, "#!/bin/sh\necho \"sandboxd $*\"\nexit 137\n").unwrap();
            let published = stand_in.call(&Call::PublishSandboxd(sandboxd.clone()));
            let sandboxd_ref = published["Ok"].as_str().unwrap();
            let killed = stand_in.call(&create(json!({"sandboxd": sandboxd_ref})));
            let killed = killed["Ok"]["id"].as_str().unwrap().to_owned();
            // The stand-in records the status only once `sandboxd` has exited, which may be after
            // the create returns.
            let ended = stand_in
                .dir
                .path()
                .join("sandboxes")
                .join(&killed)
                .join("returncode");
            let by = std::time::Instant::now() + Duration::from_secs(30);
            while !ended.exists() && std::time::Instant::now() < by {
                std::thread::sleep(Duration::from_millis(50));
            }
            lines.push(format!(
                "the console of one that exited with status 137: {}",
                stand_in.call(&Call::Console(killed.clone()))
            ));
            let deleted = stand_in.call(&Call::Delete(vec![killed]));
            lines.push(format!("deleting it: {deleted}"));
            let left = sandboxes(stand_in.dir.path()).into_iter();
            let left: Vec<_> = left.map(|(id, run, _)| (id, run)).collect();
            lines.push(format!("the sandboxes left, each with its run: {left:?}"));
            let dir = stand_in.dir.path().to_str().unwrap();
            let manifest = env!("CARGO_MANIFEST_DIR");
            let by = (hard_end - 3540).to_string();
            let shown = |line: &String| {
                let line = line.replace(manifest, "<crate>").replace(dir, "<dir>");
                tunnelled(&line.replace(&by, "<by>"))
            };
            lines.iter().map(shown).collect()
        },
        &expect![[r#"
            published: {"Ok":"<dir>/sandboxd@sha256:4a1a1f2fc48d755ca56db1f268bc2a71d0401e9abe03e0274249d193d6f48042"}
            created: {"Ok":{"endpoint":"localhost:<port>","id":"sb-1","report":{"app":"pumpkin-images","digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000","gpu":"H100!:2","image":"im-0","sandboxd":"<dir>/sandboxd@sha256:4a1a1f2fc48d755ca56db1f268bc2a71d0401e9abe03e0274249d193d6f48042"}}}
            uv run --frozen --quiet --no-config --script <crate>/helper/pumpkin-modal.py
            modal: App.lookup {"create_if_missing": true, "name": "pumpkin-images"}
            modal: Sandbox.create {"app": "pumpkin-images", "args": ["/.pumpkin/sandboxd", "0.0.0.0:7434", "--listen", "--by", "<by>", "--user", "4242:4243", "--ops", "--model"], "cpu": [1.5, 1.5], "encrypted_ports": [7434], "env": {"SANDBOXD_GRANT": "<grant>"}, "gpu": "H100!:2", "image": [{"from_id": "im-0"}, {"add_local_file": "<dir>/sandboxd", "copy": false, "remote": "/.pumpkin/sandboxd"}, {"add_local_file": "<dir>/sandboxd", "copy": false, "remote": "/.pumpkin/bin/sandboxctl"}], "inbound_cidr_allowlist": ["203.0.113.7/32"], "memory": [256, 256], "outbound_cidr_allowlist": [], "runtime": "gvisor", "tags": {"pumpkin-role": "score", "pumpkin-run": "run-1"}, "timeout": 3600}
            modal: Sandbox.tunnels {"id": "sb-1"}
            modal: Sandbox.list {"app_id": "pumpkin-images", "tags": {"pumpkin-run": "run-1"}}
            listed: {"Ok":[{"ended":false,"id":"sb-1"}]}
            its console: {"Ok":"sandboxd 127.0.0.1:<port> --listen --by <by> --ops --model\n"}
            another run's: {"Ok":{"endpoint":"localhost:<port>","id":"sb-2","report":{"app":"pumpkin-images","digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000","gpu":"H100!:2","image":"im-0","sandboxd":"<dir>/sandboxd@sha256:4a1a1f2fc48d755ca56db1f268bc2a71d0401e9abe03e0274249d193d6f48042"}}}
            its console, as run-1's: {"Err":"Modal refused the helper's `console`: sandbox sb-2 is not run run-1's: expected one of its own: fix what Modal reports"}
            deleting it as run-1's: {"Err":"Modal failed to delete 1 of the sandboxes: sandbox sb-2: sandbox sb-2 is not run run-1's: expected one of its own: fix what Modal reports"}
            deleting the sandbox and an id no sandbox has: {"Ok":null}
            listed: {"Ok":[]}
            a hard end past: {"Err":"Modal refused the helper's `create`: the create's hard end has passed: expected one to come: fix what Modal reports; what Modal made by then, if anything, the run's list finds"}, calls to Modal: 1
            memory not whole MiB: {"Err":"role `score` asks for 1000000 bytes of memory, which Modal gives in whole MiB: ask for a multiple of 1048576"}, calls to Modal: 0
            a class the provider lacks: {"Err":"the provider has no class `tpu`: expected one of those it declared"}, calls to Modal: 0
            a sandboxd changed since published: {"Err":"Modal refused the helper's `create`: the sandboxd at <dir>/sandboxd has changed since it was published: expected the file whose SHA-256 is 4a1a1f2fc48d755ca56db1f268bc2a71d0401e9abe03e0274249d193d6f48042: fix what Modal reports; what Modal made by then, if anything, the run's list finds"}
            the console of one that exited with status 137: {"Ok":"sandboxd 127.0.0.1:<port> --listen --by <by> --ops --model\npumpkin-modal: sandbox sb-3 exited with status 137\n"}
            deleting it: {"Ok":null}
            the sandboxes left, each with its run: [("sb-2", "run-2")]"#]],
    );
}

/// `line` with the port of each tunnel at `localhost`, and of each loopback address that a
/// stand-in sandbox's `sandboxd` listens at, as `<port>`.
fn tunnelled(line: &str) -> String {
    let mut shown = line.to_owned();
    for host in ["localhost:", "127.0.0.1:"] {
        let mut rest = shown.as_str();
        let mut out = String::new();
        while let Some(at) = rest.find(host) {
            let (before, after) = rest.split_at(at + host.len());
            out.push_str(before);
            let digits = after.bytes().take_while(u8::is_ascii_digit).count();
            if digits > 0 {
                out.push_str("<port>");
            }
            rest = &after[digits..];
        }
        out.push_str(rest);
        shown = out;
    }
    shown
}

// The conformance suite every provider passes, on the stand-in SDK, whose sandboxes run the
// static `sandboxd` behind its stand-in for Modal's tunnel, which the suite dials, as it meets a
// provider's sandboxes where its reach says.
#[test]
#[ignore = "needs Docker and the static sandboxd: run cargo xtask ci --docker"]
fn conformance() {
    if let Some(dir) = env::var_os(CHILD) {
        let fake = Fake::new(Path::new(&dir));
        return suite::check(&[("modal", &fake)]);
    }
    let stand_in = StandIn::new("conformance");
    stand_in.hold_images();
    stand_in.run_child(&stand_in.path());
}

/// The stand-ins in `dir` as the suite's subject: the provider connects with the stand-ins'
/// settings, and publishes an image built on Docker.
struct Fake {
    dir: PathBuf,
    engine: Engine,
    settings: Settings,
    built: Built,
}

impl Fake {
    fn new(dir: &Path) -> Self {
        let context = ContextName::try_from(test_support::docker_context()).unwrap();
        Self {
            dir: dir.to_owned(),
            engine: Engine::connect(context, &dir.join(".pumpkin/docker")).unwrap(),
            settings: serde_json::from_value(StandIn::settings()).unwrap(),
            built: Built::new(dir, "conformance"),
        }
    }
}

impl Subject for Fake {
    fn connect(&self) -> Result<Box<dyn Provider + '_>, String> {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("helper/pumpkin-modal.py");
        let modal = Modal::connect(
            &self.settings,
            &script,
            &self.dir.join(".pumpkin/modal"),
            self.engine.pusher()?,
        )?;
        Ok(Box::new(modal))
    }

    fn locals(&self, dir: &Path) -> Locals {
        let mute = dir.join("mute");
        fs::write(&mute, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&mute, fs::Permissions::from_mode(0o755)).unwrap();
        // Not executable, so its sandbox ends as it starts.
        let broken = dir.join("broken");
        fs::write(&broken, "not a program\n").unwrap();
        Locals {
            image: Image::try_from(self.built.role.clone()).unwrap(),
            sandboxd: test_support::sandboxd(),
            mute,
            broken,
            unreachable_image: Image::try_from(format!("sha256:{}", "0".repeat(64))).unwrap(),
            unreachable_sandboxd: dir.join("nowhere"),
        }
    }

    fn stall(&self, id: &SandboxId) {
        // Its `sandboxd`, in its container, and the tunnel before it, which then takes no more
        // than its buffers hold.
        for (held, _, pid) in sandboxes(&self.dir) {
            if held == id.as_str() {
                let container = container(&self.dir, &held).unwrap();
                let paused = test_support::docker().args(["pause", &container]).output();
                assert!(paused.unwrap().status.success(), "{container} was to pause");
                signal("STOP", pid);
            }
        }
    }

    fn held(&self, run: &RunTag) -> Vec<SandboxId> {
        let held = sandboxes(&self.dir).into_iter();
        let held = held.filter(|(_, of, _)| of == run.as_str());
        held.map(|(id, _, _)| SandboxId::from(id)).collect()
    }

    // Each sandbox has resources of its own on Modal.
    fn reserves(&self) -> bool {
        false
    }
}
