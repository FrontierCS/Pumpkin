//! The `pumpkin` command, run as an operator runs it: its exit status, what it says, and the
//! record of its run, whose `outcome.json` it prints. The test on Docker is ignored, as the
//! Docker provider's are, and fails without `PUMPKIN_SANDBOXD`, which `cargo xtask ci --docker`
//! sets; it reaches Docker through the context that `PUMPKIN_DOCKER_CONTEXT` names, `default`
//! if it is unset.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test helper fails the test that calls it"
)]
#![expect(
    clippy::print_stderr,
    reason = "a failed test shows where its scratch directory is"
)]

use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs, iter, thread};

use expect_test::{Expect, expect};
use pumpkin_docker::{ContextName, Engine};
use pumpkin_schema::sandbox::SANDBOXCTL_PATH;
use serde_json::{Value, json};
use stdx::process::Environment;

const LWE: &str = include_str!("../../pumpkin-schema/tests/fixtures/lwe.json");

/// A small public image, pinned by digest.
// Repeated in `providers/docker/tests/it.rs`, `crates/pumpkin/tests/it/docker.rs` and
// `crates/pumpkin-harness/tests/it/docker.rs`: change all four.
const BUSYBOX: &str =
    "busybox@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e";

/// What the final judge of the runs on Docker does: passes the payload, with its size as its
/// score.
const SCORE: &str = r#"printf '{"status": "passed", "raw_score": %s}' \
    $(wc -c < /pumpkin/in/payload/raw) > /pumpkin/out/result.json"#;

/// The directory test `name` runs `pumpkin` in, empty. Dropping it removes it, unless the test
/// failed, when it shows where it is.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = env::temp_dir().join(format!("pk-cli-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).unwrap();
        Self(dir)
    }

    /// Writes `contents` into the directory's file `name`.
    fn write(&self, name: &str, contents: &(impl Display + ?Sized)) {
        fs::write(self.0.join(name), contents.to_string()).unwrap();
    }

    /// `pumpkin` with `args`, to run in the directory.
    fn pumpkin(&self, args: &[&str]) -> Command {
        let pumpkin = env!("CARGO_BIN_EXE_pumpkin");
        let mut command = stdx::process::command(pumpkin, Environment::Inherited);
        command.args(args).current_dir(&self.0);
        command
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if thread::panicking() {
            eprintln!("the test's scratch directory is {}", self.0.display());
        } else {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

/// Runs each of `commands`, `pumpkin` in `scratch`, and checks what becomes of it: its exit
/// status; what it says on its standard error, but for the warning of a rootful engine, shown
/// only if it doesn't match `rootful`, whether the engine the commands reach runs as root; and
/// if it names a run's record, the record's files, its `outcome.json` and whether it printed
/// that, and else the first line it printed, if any. Each shows `scratch` as `<scratch>`, a
/// run's id as `<id>`, and each of `shown` as its name.
fn check(
    scratch: &Scratch,
    commands: impl IntoIterator<Item = Command>,
    rootful: bool,
    shown: &[(&str, &str)],
    expect: &Expect,
) {
    let ran: Vec<String> = (commands.into_iter())
        .map(|command| ran(command, &scratch.0, rootful))
        .collect();
    let mut text = ran.join("\n");
    text = text.replace(scratch.0.to_str().unwrap(), "<scratch>");
    for (value, name) in shown {
        text = text.replace(value, name);
    }
    expect.assert_eq(&text);
}

/// What becomes of `command`, `pumpkin` in `dir`, as [`check`] shows it.
fn ran(mut command: Command, dir: &Path, rootful: bool) -> String {
    let set = command.get_envs().map(|(name, value)| match value {
        Some(value) => format!("{}={} ", name.display(), value.display()),
        None => format!("env -u {} ", name.display()),
    });
    let args = command.get_args().map(|arg| match arg.to_str().unwrap() {
        "" => "''",
        arg => arg,
    });
    let set: String = set.collect();
    let args: Vec<&str> = iter::once("pumpkin").chain(args).collect();
    let mut lines = vec![format!("$ {set}{}", args.join(" "))];
    let output = command.output().unwrap();
    lines.push(format!("exit status: {:?}", output.status.code()));
    let stderr = String::from_utf8(output.stderr).unwrap();
    let (warnings, said): (Vec<&str>, Vec<&str>) =
        (stderr.lines()).partition(|line| line.contains(" runs as root, "));
    if warnings.is_empty() == rootful {
        lines.push(format!(
            "warned of a rootful engine: {}",
            !warnings.is_empty()
        ));
    }
    lines.extend(said.into_iter().map(str::to_owned));
    let stdout = String::from_utf8(output.stdout).unwrap();
    let Some((id, record)) = record(&stderr) else {
        let first = stdout.lines().next();
        lines.extend(first.map(|line| format!("printed first: {line}")));
        return lines.join("\n");
    };
    lines.extend(recorded(&dir.join(record), &stdout));
    lines.join("\n").replace(id, "<id>")
}

/// The id and the record of the run that `stderr`, `pumpkin`'s, names, if it names one.
fn record(stderr: &str) -> Option<(&str, &str)> {
    let named = stderr
        .lines()
        .find_map(|line| line.strip_prefix("pumpkin: run "));
    named?.split_once(" records into ")
}

/// The files of `record`, a run's, its `outcome.json`, and whether `stdout` is that.
fn recorded(record: &Path, stdout: &str) -> Vec<String> {
    let outcome = fs::read_to_string(record.join("outcome.json")).unwrap();
    let compact = serde_json::from_str::<Value>(&outcome).unwrap();
    vec![
        format!("record: {}", files(record, record).join(", ")),
        format!("outcome.json: {compact}"),
        format!("printed outcome.json: {}", stdout == outcome),
    ]
}

/// The files and empty directories below `dir`, by their paths from `base`, sorted.
fn files(base: &Path, dir: &Path) -> Vec<String> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    paths.sort();
    let mut lines = Vec::new();
    for path in paths {
        let name = path.strip_prefix(base).unwrap().display().to_string();
        if path.is_dir() {
            let below = files(base, &path);
            if below.is_empty() {
                lines.push(format!("{name}/"));
            }
            lines.extend(below);
        } else {
            lines.push(name);
        }
    }
    lines
}

/// Usage errors exit with 2, and `--help` with 0.
#[test]
fn usage() {
    let scratch = Scratch::new("usage");
    let run = ["run", "lwe.json", "--docker-context", "nowhere"];
    let commands = [
        scratch.pumpkin(&[]),
        scratch.pumpkin(&["--help"]),
        scratch.pumpkin(&run),
        scratch.pumpkin(&[&run[..], &["--sandboxd", "sandboxd", "--quiet"]].concat()),
    ];
    check(
        &scratch,
        commands,
        false,
        &[],
        &expect![[r#"
        $ pumpkin
        exit status: Some(2)
        pumpkin: subcommand is required: see `pumpkin --help`
        $ pumpkin --help
        exit status: Some(0)
        printed first: pumpkin
        $ pumpkin run lwe.json --docker-context nowhere
        exit status: Some(2)
        pumpkin: flag is required: `--sandboxd`: see `pumpkin --help`
        $ pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --quiet
        exit status: Some(2)
        pumpkin: unexpected flag: `--quiet`: see `pumpkin --help`"#]],
    );
}

/// What `run` finds wrong before it reaches the engine exits with 1: a Docker context's empty
/// name, an IR that can't be read or isn't one, and `HOME` unset or empty where it places the
/// runs. The context `nowhere` reaches no engine, so reaching for it would fail otherwise.
#[test]
fn unrunnable() {
    let scratch = Scratch::new("unrunnable");
    scratch.write("lwe.json", &serde_json::from_str::<Value>(LWE).unwrap());
    scratch.write("empty.json", &json!({}));
    let run = |ir, context, rest: &[&str]| {
        let mut command = scratch.pumpkin(&["run", ir, "--docker-context", context]);
        command.args(["--sandboxd", "sandboxd"]).args(rest);
        command
    };
    let mut unset = run("lwe.json", "nowhere", &[]);
    unset.env_remove("HOME");
    let mut empty = run("lwe.json", "nowhere", &[]);
    empty.env("HOME", "");
    let runs = ["--runs", "runs"];
    let commands = [
        run("lwe.json", "", &runs),
        run("missing.json", "nowhere", &runs),
        run("empty.json", "nowhere", &runs),
        unset,
        empty,
    ];
    check(
        &scratch,
        commands,
        false,
        &[],
        &expect![[r#"
        $ pumpkin run lwe.json --docker-context '' --sandboxd sandboxd --runs runs
        exit status: Some(1)
        pumpkin: the Docker context's name is empty: name one that `docker context ls` lists
        $ pumpkin run missing.json --docker-context nowhere --sandboxd sandboxd --runs runs
        exit status: Some(1)
        pumpkin: missing.json: No such file or directory (os error 2)
        $ pumpkin run empty.json --docker-context nowhere --sandboxd sandboxd --runs runs
        exit status: Some(1)
        pumpkin: empty.json: missing field `ir_version` at line 1 column 2
        $ env -u HOME pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd
        exit status: Some(1)
        pumpkin: HOME is unset or empty: set it, or name a directory with `--runs`
        $ HOME= pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd
        exit status: Some(1)
        pumpkin: HOME is unset or empty: set it, or name a directory with `--runs`"#]],
    );
}

/// What `run` finds wrong in the deployment's configuration, which it reads for a workflow with
/// a model route, exits with 1: a configuration that can't be read, where `HOME` places it or
/// where `--config` names it, `HOME` unset where it would place it, and the configurations
/// [`write_configurations`] writes to be refused. Past a configuration it accepts, and past a
/// broken one that a workflow without a route doesn't read, it reaches for the engine, which it
/// can't without `docker` on `PATH`.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn configurations() {
    let scratch = Scratch::new("configurations");
    let mut ir: Value = serde_json::from_str(LWE).unwrap();
    scratch.write("lwe.json", &ir);
    ir["roles"][0].as_object_mut().unwrap().remove("egress");
    scratch.write("unrouted.json", &ir);
    write_configurations(&scratch);
    let run = |ir, config: &[&str]| {
        let mut command = scratch.pumpkin(&["run", ir, "--docker-context", "nowhere"]);
        (command.args(["--sandboxd", "sandboxd", "--runs", "runs"])).args(config);
        command
    };
    let config = |name: &str| run("lwe.json", &["--config", &format!("{name}.toml")]);
    let mut home = run("lwe.json", &[]);
    home.env("HOME", scratch.0.join("home"));
    let mut homeless = run("lwe.json", &[]);
    homeless.env_remove("HOME");
    let mut served = config("served");
    let mut unrouted = run("unrouted.json", &["--config", "docker.toml"]);
    for command in [&mut served, &mut unrouted] {
        command.env("PATH", scratch.0.join("nowhere"));
    }
    let refused = ["missing", "docker", "broken", "inline", "priced", "keyless"];
    let refused = refused
        .into_iter()
        .chain(["invalid", "twice", "again", "query", "fragment"]);
    let commands = [home, homeless]
        .into_iter()
        .chain(refused.map(config))
        .chain([served, unrouted]);
    check(
        &scratch,
        commands,
        false,
        &[],
        &expect![[r#"
        $ HOME=<scratch>/home pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --runs runs
        exit status: Some(1)
        pumpkin: the configuration <scratch>/home/.config/pumpkin/pumpkin.toml can't be read (No such file or directory (os error 2)): write the upstreams of the workflow's model route there, or name another file with `--config`
        $ env -u HOME pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --runs runs
        exit status: Some(1)
        pumpkin: HOME is unset or empty: set it, or name the configuration with `--config`
        $ pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --runs runs --config missing.toml
        exit status: Some(1)
        pumpkin: the configuration missing.toml can't be read (No such file or directory (os error 2)): write the upstreams of the workflow's model route there, or name another file with `--config`
        $ pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --runs runs --config docker.toml
        exit status: Some(1)
        pumpkin: docker.toml: TOML parse error at line 6, column 2
          |
        6 | [docker]
          |  ^^^^^^
        unknown field `docker`, expected `upstreams`
        $ pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --runs runs --config broken.toml
        exit status: Some(1)
        pumpkin: broken.toml: TOML parse error at line 1, column 19
          |
        1 | [upstreams.gateway
          |                   ^
        unclosed table, expected `]`
        $ pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --runs runs --config inline.toml
        exit status: Some(1)
        pumpkin: inline.toml: TOML parse error at line 3, column 1
          |
        3 | key = "sk-1"
          | ^^^
        unknown field `key`, expected one of `url`, `key_file`, `models`
        $ pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --runs runs --config priced.toml
        exit status: Some(1)
        pumpkin: priced.toml: TOML parse error at line 4, column 73
          |
        4 | models = [{ name = "gpt-6-astra", upstream_name = "vendor/gpt-6-astra", price = 1 }]
          |                                                                         ^^^^^
        unknown field `price`, expected `name` or `upstream_name`
        $ pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --runs runs --config keyless.toml
        exit status: Some(1)
        pumpkin: keyless.toml: TOML parse error at line 1, column 1
          |
        1 | [upstreams.gateway]
          | ^^^^^^^^^^^^^^^^^^^
        missing field `key_file`
        $ pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --runs runs --config invalid.toml
        exit status: Some(1)
        pumpkin: invalid.toml: TOML parse error at line 1, column 12
          |
        1 | [upstreams.Gateway]
          |            ^^^^^^^
        `Gateway` is not a valid name: use 1 to 64 characters from a-z, 0-9, `_` and `-`, starting with a letter
        $ pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --runs runs --config twice.toml
        exit status: Some(1)
        pumpkin: twice.toml: TOML parse error at line 6, column 12
          |
        6 | [upstreams.gateway]
          |            ^^^^^^^
        duplicate key
        $ pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --runs runs --config again.toml
        exit status: Some(1)
        pumpkin: again.toml: `gpt-6-astra` is listed more than once: list each model once, under the upstream that serves it
        $ pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --runs runs --config query.toml
        exit status: Some(1)
        pumpkin: query.toml: TOML parse error at line 2, column 7
          |
        2 | url = "https://models.example.com/v1?project=1"
          |       ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
        the upstream's URL has a query or a fragment: name the API's base URL alone, below which the route reaches `/responses`
        $ pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --runs runs --config fragment.toml
        exit status: Some(1)
        pumpkin: fragment.toml: TOML parse error at line 2, column 7
          |
        2 | url = "https://models.example.com/v1#v2"
          |       ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
        the upstream's URL has a query or a fragment: name the API's base URL alone, below which the route reaches `/responses`
        $ PATH=<scratch>/nowhere pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --runs runs --config served.toml
        exit status: Some(1)
        pumpkin: the Docker context `nowhere` can't be read: `docker` didn't run: No such file or directory (os error 2): install Docker's command line on `PATH`: name one that `docker context ls` lists
        $ PATH=<scratch>/nowhere pumpkin run unrouted.json --docker-context nowhere --sandboxd sandboxd --runs runs --config docker.toml
        exit status: Some(1)
        pumpkin: the Docker context `nowhere` can't be read: `docker` didn't run: No such file or directory (os error 2): install Docker's command line on `PATH`: name one that `docker context ls` lists"#]],
    );
}

/// Writes into `scratch` the deployment's configurations that [`configurations`] gives `run`,
/// each named for what it holds: `served`, of two upstreams; and, to be refused, `docker`,
/// with a key that a configuration doesn't have; `broken`, which isn't TOML; `inline`, with an
/// upstream's key in place of its file; `priced`, with a key that a model doesn't have; `keyless`, without an upstream's
/// file; `invalid`, with an invalid name; `twice`, with two upstreams of one name; `again`,
/// with one model listed twice; and `query` and `fragment`, with a URL that has one.
fn write_configurations(scratch: &Scratch) {
    let table = |name: &str, model: &str| {
        format!(
            r#"[upstreams.{name}]
url = "https://models.example.com/v1"
key_file = "key"
models = [{{ name = "{model}", upstream_name = "vendor/{model}" }}]
"#
        )
    };
    let gateway = table("gateway", "gpt-6-astra");
    let direct = table("direct", "flash");
    let configs = [
        ("served", format!("{gateway}\n{direct}")),
        ("docker", format!("{gateway}\n[docker]\n")),
        (
            "broken",
            gateway.replace("[upstreams.gateway]", "[upstreams.gateway"),
        ),
        (
            "inline",
            gateway.replace("key_file = \"key\"", "key = \"sk-1\""),
        ),
        ("priced", gateway.replace(" }", ", price = 1 }")),
        ("keyless", gateway.replace("key_file = \"key\"\n", "")),
        ("invalid", table("Gateway", "gpt-6-astra")),
        ("twice", format!("{gateway}\n{}", table("gateway", "flash"))),
        (
            "again",
            format!("{gateway}\n{}", table("direct", "gpt-6-astra")),
        ),
        ("query", gateway.replace("/v1", "/v1?project=1")),
        ("fragment", gateway.replace("/v1", "/v1#v2")),
    ];
    for (name, config) in configs {
        scratch.write(&format!("{name}.toml"), &config);
    }
}

/// The static `sandboxd` that `PUMPKIN_SANDBOXD` names.
fn sandboxd() -> String {
    let Some(sandboxd) = env::var_os("PUMPKIN_SANDBOXD") else {
        panic!(
            "`PUMPKIN_SANDBOXD` is unset: run the Docker tests with `cargo xtask ci --docker`, \
             which builds sandboxd and sets it"
        );
    };
    let sandboxd = PathBuf::from(sandboxd).canonicalize().unwrap();
    sandboxd.into_os_string().into_string().unwrap()
}

/// The image of the roles that run on Docker, which the provider builds on `engine`: `busybox`
/// with a world-writable `/workspace`, the fixture's agent's working directory.
fn image(scratch: &Scratch, engine: &Engine) -> String {
    let dir = scratch.0.join("image");
    fs::create_dir(&dir).unwrap();
    let recipe = format!("FROM {BUSYBOX}\nRUN mkdir --mode=1777 /workspace\n");
    fs::write(dir.join("Dockerfile"), recipe).unwrap();
    let built = engine.build(&dir, "Dockerfile", "pumpkin-cli-tests", |_| Ok(()));
    built.unwrap().image.as_str().to_owned()
}

/// The fixture's workflow, without the agent's model route, which `pumpkin run` refuses, with
/// `argv` as the agent's command, the final judge as [`SCORE`] says, and every role running
/// `image` with a small machine's share.
fn workflow(image: &str, argv: &[&str]) -> Value {
    let mut ir: Value = serde_json::from_str(LWE).unwrap();
    ir["roles"][0].as_object_mut().unwrap().remove("egress");
    ir["roles"][0]["runtime"]["agent"]["command"]["argv"] = json!(argv);
    ir["roles"][2]["runtime"]["judge"]["command"]["argv"] = json!(["sh", "-c", SCORE]);
    for role in ir["roles"].as_array_mut().unwrap() {
        role["image"] = json!(image);
        role["resources"] = json!({ "cpu_millis": 500, "memory_bytes": 256 << 20 });
    }
    // Far later than the agent's end, so that a slow engine makes a run wait longer, never end
    // differently.
    ir["main"]["solo_then_judge"]["deadline_seconds"] = json!(300);
    ir
}

/// Runs on Docker: a workflow it refuses, with a model route that its configuration serves,
/// before any sandbox starts; one whose agent exits
/// without a submission, recorded below `HOME` whatever `XDG_DATA_HOME` says; and one whose
/// agent submits a payload, which the final judge scores.
#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn runs_on_docker() {
    let scratch = Scratch::new("docker");
    let sandboxd = sandboxd();
    let context = env::var("PUMPKIN_DOCKER_CONTEXT").unwrap_or_else(|_| "default".to_owned());
    let engine = Engine::connect(ContextName::try_from(context.clone()).unwrap()).unwrap();
    let image = image(&scratch, &engine);
    scratch.write("lwe.json", &serde_json::from_str::<Value>(LWE).unwrap());
    let upstream = r#"[upstreams.provider]
url = "https://models.example.com/v1"
key_file = "key"
models = [{ name = "gpt-6-astra", upstream_name = "gpt-6-astra" }]
"#;
    scratch.write("pumpkin.toml", upstream);
    scratch.write("exits.json", &workflow(&image, &["true"]));
    let submit = format!("printf six > six && {SANDBOXCTL_PATH} submit submit six --finalize");
    scratch.write("submits.json", &workflow(&image, &["sh", "-c", &submit]));
    let run = |ir, runs: &[&str]| {
        let mut command = scratch.pumpkin(&["run", ir, "--docker-context", &context]);
        command.args(["--sandboxd", &sandboxd]).args(runs);
        command
    };
    // `docker` finds its contexts below `HOME` unless `DOCKER_CONFIG` says where.
    let home = PathBuf::from(env::var_os("HOME").unwrap());
    let docker_config = env::var_os("DOCKER_CONFIG").map_or(home.join(".docker"), PathBuf::from);
    let docker_config = docker_config.into_os_string().into_string().unwrap();
    let mut exits = run("exits.json", &[]);
    (exits.env("HOME", scratch.0.join("home")))
        .env("XDG_DATA_HOME", scratch.0.join("xdg"))
        .env("DOCKER_CONFIG", &docker_config);
    let commands = [
        run("lwe.json", &["--runs", "runs", "--config", "pumpkin.toml"]),
        exits,
        run("submits.json", &["--runs", "runs"]),
    ];
    let context = format!("--docker-context {context} ");
    let shown = [
        (&context[..], "--docker-context <context> "),
        (&sandboxd[..], "<sandboxd>"),
        (&docker_config[..], "<docker config>"),
    ];
    let rootful = !engine.report().security.rootless;
    check(
        &scratch,
        commands,
        rootful,
        &shown,
        &expect![[r#"
        $ pumpkin run lwe.json --docker-context <context> --sandboxd <sandboxd> --runs runs --config pumpkin.toml
        exit status: Some(1)
        pumpkin: `agent` has a model route, but no upstream serves it: give one, or remove the role's `egress.model` to run without one
        $ DOCKER_CONFIG=<docker config> HOME=<scratch>/home XDG_DATA_HOME=<scratch>/xdg pumpkin run exits.json --docker-context <context> --sandboxd <sandboxd>
        exit status: Some(3)
        pumpkin: run <id> records into <scratch>/home/.local/share/pumpkin/runs/<id>
        record: agent/log/command.stderr, agent/log/command.stdout, agent/out/, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
        outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}
        printed outcome.json: true
        $ pumpkin run submits.json --docker-context <context> --sandboxd <sandboxd> --runs runs
        exit status: Some(0)
        pumpkin: run <id> records into runs/<id>
        record: agent/log/command.stderr, agent/log/command.stdout, agent/out/, ir.json, jobs/0/log/command.stderr, jobs/0/log/command.stdout, jobs/0/out/evidence/, jobs/0/out/result.json, log.jsonl, outcome.json, payloads/44778d82365e4af681c40d5f0eef5cf6f5899d3f0ac335050a7ed6779cf3f674, provider.json, record.json
        outcome.json: {"score":3,"state":"finished","verdict":{"kind":"judged","result":{"passed":true,"returned":{"raw_score":3},"score":3},"submission":{"finalized":true,"payload":{"digest":"sha256:44778d82365e4af681c40d5f0eef5cf6f5899d3f0ac335050a7ed6779cf3f674","size":3}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}
        printed outcome.json: true"#]],
    );
}
