//! `run`: what it finds wrong before it reaches the engine, and once it has, and runs on Docker.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::thread::{self, JoinHandle};
use std::{env, fs};

use docker::{ContextName, Engine};
use expect_test::expect;
use schema::sandbox::{MODEL_PORT, SANDBOXCTL_PATH};
use serde_json::{Value, json};

use super::{LWE, Scratch, check, image, sandboxd, workflow};

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

#[test]
fn unrunnable() {
    let scratch = Scratch::new("unrunnable");
    scratch.write("lwe.json", &serde_json::from_str::<Value>(LWE).unwrap());
    scratch.write("empty.json", &json!({}));
    // `nowhere` reaches no engine, so reaching for it would fail otherwise.
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

/// `wait` finds the `outcome.json` of each run recorded in `scratch`'s `runs`, which have
/// ended: the engine is not reached.
fn wait_for_each_run(scratch: &Scratch) {
    let mut ids: Vec<String> = (fs::read_dir(scratch.0.join("runs")).unwrap())
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    ids.sort();
    let waits: Vec<Command> = (ids.iter())
        .map(|id| scratch.pumpkin(&["wait", id, "--runs", "runs"]))
        .collect();
    let shown: Vec<(&str, &str)> = ids.iter().map(|id| (&id[..], "<id>")).collect();
    check(
        scratch,
        waits,
        false,
        &shown,
        &expect![[r#"
            $ pumpkin wait <id> --runs runs
            exit status: Some(3)
            pumpkin: run <id> finished without a score: the agent submitted nothing, and the work phase ended when the agent exited
            printed outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            $ pumpkin wait <id> --runs runs
            exit status: Some(0)
            printed outcome.json: {"score":3,"state":"finished","verdict":{"kind":"judged","result":{"returned":{"raw_score":3},"score":3},"submission":{"finalized":true,"payload":{"digest":"sha256:44778d82365e4af681c40d5f0eef5cf6f5899d3f0ac335050a7ed6779cf3f674","size":3}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}"#]],
    );
}

#[test]
fn refusals() {
    let scratch = Scratch::new("refusals");
    let mut ir: Value = serde_json::from_str(LWE).unwrap();
    scratch.write("lwe.json", &ir);
    ir["roles"][1]["name"] = json!("agent");
    ir["roles"][1]["egress"] = json!({ "model": { "models": ["gpt-6-astra"] } });
    scratch.write("broken.json", &ir);
    write_configurations(&scratch);
    let run = |ir, config: &str| {
        let mut command = scratch.pumpkin(&["run", ir, "--docker-context", "nowhere"]);
        command.args([
            "--sandboxd",
            "sandboxd",
            "--runs",
            "runs",
            "--config",
            config,
        ]);
        command
    };
    let commands = [
        run("broken.json", "conf/served.toml"),
        run("lwe.json", "unserved.toml"),
    ];
    check(
        &scratch,
        commands,
        false,
        &[],
        &expect![[r#"
        $ pumpkin run broken.json --docker-context nowhere --sandboxd sandboxd --runs runs --config conf/served.toml
        exit status: Some(1)
        pumpkin: broken.json: /roles/1/name: `agent` also names /roles/0: give each a unique name
        pumpkin: broken.json: /roles/1/egress/model: `agent` is a judge, and only the agent has a model route: remove it
        pumpkin: broken.json: /main/solo_then_judge/grants/0/judge/role: no role is named `train_judge`: name one listed in `roles`
        pumpkin: broken.json: the workflow has 3 problems: fix them and run again
        $ pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --runs runs --config unserved.toml
        exit status: Some(1)
        pumpkin: unserved.toml: no upstream serves every model that the workflow's route allows, `gpt-6-astra`: list them all under one"#]],
    );
    assert!(!scratch.0.join("runs").exists(), "a refusal made a record");
}

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
    let mut served = config("conf/served");
    let mut unrouted = run("unrouted.json", &["--config", "docker.toml"]);
    // Without `docker` on `PATH`, `run` can't reach the engine past the configuration.
    for command in [&mut served, &mut unrouted] {
        command.env("PATH", scratch.0.join("nowhere"));
    }
    let refused = [
        "missing", "docker", "broken", "inline", "priced", "keyless", "invalid",
    ];
    let refused = (refused.into_iter()).chain([
        "twice",
        "again",
        "query",
        "fragment",
        "unserved",
        "conf/unkeyed",
    ]);
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
            $ pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --runs runs --config unserved.toml
            exit status: Some(1)
            pumpkin: unserved.toml: no upstream serves every model that the workflow's route allows, `gpt-6-astra`: list them all under one
            $ pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --runs runs --config conf/unkeyed.toml
            exit status: Some(1)
            pumpkin: conf/unkeyed.toml: the key file conf/missing can't be read (No such file or directory (os error 2)): name a file that holds the model provider's key
            $ PATH=<scratch>/nowhere pumpkin run lwe.json --docker-context nowhere --sandboxd sandboxd --runs runs --config conf/served.toml
            exit status: Some(1)
            pumpkin: the Docker context `nowhere` can't be read: `docker` didn't run: No such file or directory (os error 2): install Docker's command line on `PATH`: name one that `docker context ls` lists
            $ PATH=<scratch>/nowhere pumpkin run unrouted.json --docker-context nowhere --sandboxd sandboxd --runs runs --config docker.toml
            exit status: Some(1)
            pumpkin: the Docker context `nowhere` can't be read: `docker` didn't run: No such file or directory (os error 2): install Docker's command line on `PATH`: name one that `docker context ls` lists"#]],
    );
}

/// Writes into `scratch` the deployment's configurations that [`configurations`] gives `run`,
/// each named for what it holds: `conf/served`, of two upstreams, beside the key file `conf/key`;
/// and, to be refused, `docker`, with a key that a configuration doesn't have; `broken`, which
/// isn't TOML; `inline`, with an upstream's key in place of its file; `priced`, with a key that a
/// model doesn't have; `keyless`, without an upstream's file; `invalid`, with an invalid name;
/// `twice`, with two upstreams of one name; `again`, with one model listed twice; `query` and
/// `fragment`, with a URL that has one; `unserved`, without the workflow's model; and
/// `conf/unkeyed`, whose key file is missing.
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
    scratch.write("conf/key", "sk-test\n");
    let configs = [
        ("conf/served", format!("{gateway}\n{direct}")),
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
        ("unserved", direct),
        ("conf/unkeyed", gateway.replace("\"key\"", "\"missing\"")),
    ];
    for (name, config) in configs {
        scratch.write(&format!("{name}.toml"), &config);
    }
}

/// Writes into `scratch` the workflow `asks.json`, [`workflow`]'s with the fixture's model route,
/// whose agent asks its model and keeps the answer in its `out`; and the configuration
/// `conf/pumpkin.toml`, whose one upstream, the stand-in at `address`, serves the route's model
/// as `vendor/gpt-6-astra`, with its key in `conf/key`.
fn write_asks(scratch: &Scratch, image: &str, address: &str) {
    let config = format!(
        r#"[upstreams.provider]
url = "http://{address}/v1/"
key_file = "key"
models = [{{ name = "gpt-6-astra", upstream_name = "vendor/gpt-6-astra" }}]
"#
    );
    scratch.write("conf/pumpkin.toml", &config);
    fs::write(scratch.0.join("conf/key"), "sk-test\n").unwrap();
    let body = r#"{"model": "gpt-6-astra", "store": false, "input": "hi"}"#;
    let ask = format!(
        "wget -q -O /pumpkin/out/answer --header 'content-type: application/json' \
         --post-data '{body}' http://127.0.0.1:{MODEL_PORT}/responses"
    );
    let mut asks = workflow(image, &["sh", "-c", &ask]);
    let lwe: Value = serde_json::from_str(LWE).unwrap();
    asks["roles"][0]["egress"] = lwe["roles"][0]["egress"].clone();
    scratch.write("asks.json", &asks);
}

/// A stand-in for a model provider, on the loopback, which answers each `POST` with a whole
/// stream that echoes the request's line, its `authorization` and its body.
struct StandIn {
    address: String,
    thread: JoinHandle<()>,
}

impl StandIn {
    fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let thread = thread::spawn(move || {
            for connection in listener.incoming() {
                let mut connection = connection.unwrap();
                let mut reader = BufReader::new(&connection);
                let mut head = Vec::new();
                while head.last().is_none_or(|line: &String| !line.is_empty()) {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    head.push(line.trim_end().to_owned());
                }
                // The connection that `stop` makes holds no request.
                if head[0].is_empty() {
                    return;
                }
                let header = |name| head.iter().find_map(|line| line.strip_prefix(name));
                let length = header("content-length: ").unwrap();
                let mut body = vec![0; length.parse().unwrap()];
                reader.read_exact(&mut body).unwrap();
                let body = String::from_utf8(body).unwrap();
                let echo = format!(
                    "data: {} {:?} {body}\n\n",
                    head[0],
                    header("authorization: ")
                );
                let stream = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                     transfer-encoding: chunked\r\nconnection: close\r\n\r\n{:x}\r\n{echo}\r\n0\r\n\r\n",
                    echo.len()
                );
                connection.write_all(stream.as_bytes()).unwrap();
            }
        });
        Self { address, thread }
    }

    fn stop(self) {
        drop(TcpStream::connect(&self.address).unwrap());
        self.thread.join().unwrap();
    }
}

#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn runs_on_docker() {
    let scratch = Scratch::new("docker");
    let sandboxd = sandboxd();
    let context = env::var("PUMPKIN_DOCKER_CONTEXT").unwrap_or_else(|_| "default".to_owned());
    let engine = Engine::connect(ContextName::try_from(context.clone()).unwrap()).unwrap();
    let image = image(&engine);
    let stand_in = StandIn::start();
    write_asks(&scratch, &image, &stand_in.address);
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
        run(
            "asks.json",
            &["--runs", "runs", "--config", "conf/pumpkin.toml"],
        ),
        exits,
        run("submits.json", &["--runs", "runs"]),
    ];
    let context = format!("--docker-context {context} ");
    let shown = [
        (&context[..], "--docker-context <context> "),
        (&sandboxd[..], "<sandboxd>"),
        (&docker_config[..], "<docker config>"),
        (&stand_in.address[..], "<stand-in>"),
    ];
    let rootful = !engine.report().security.rootless;
    check(
        &scratch,
        commands,
        rootful,
        &shown,
        &expect![[r#"
        $ pumpkin run asks.json --docker-context <context> --sandboxd <sandboxd> --runs runs --config conf/pumpkin.toml
        exit status: Some(3)
        pumpkin: run <id> records into runs/<id>
        pumpkin: run <id> finished without a score: the agent submitted nothing, and the work phase ended when the agent exited
        record: agent/log/command.stderr, agent/log/command.stdout, agent/out/answer, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
        the run log notes: the agent's model route goes to the upstream `provider`, which takes `gpt-6-astra` as `vendor/gpt-6-astra`
        the run log records: {"answer":{"kind":"forwarded"},"kind":"model_answered","request":0,"upstream_status":200}
        the agent's answer: data: POST /v1/responses HTTP/1.1 Some("Bearer sk-test") {"input":"hi","model":"vendor/gpt-6-astra","store":false}\n\n
        outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}
        printed outcome.json: true
        $ DOCKER_CONFIG=<docker config> HOME=<scratch>/home XDG_DATA_HOME=<scratch>/xdg pumpkin run exits.json --docker-context <context> --sandboxd <sandboxd>
        exit status: Some(3)
        pumpkin: run <id> records into <scratch>/home/.local/share/pumpkin/runs/<id>
        pumpkin: run <id> finished without a score: the agent submitted nothing, and the work phase ended when the agent exited
        record: agent/log/command.stderr, agent/log/command.stdout, agent/out/, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
        outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}
        printed outcome.json: true
        $ pumpkin run submits.json --docker-context <context> --sandboxd <sandboxd> --runs runs
        exit status: Some(0)
        pumpkin: run <id> records into runs/<id>
        record: agent/log/command.stderr, agent/log/command.stdout, agent/out/, driver.pid, ir.json, jobs/0/log/command.stderr, jobs/0/log/command.stdout, jobs/0/out/evidence/, jobs/0/out/result.json, log.jsonl, outcome.json, payloads/44778d82365e4af681c40d5f0eef5cf6f5899d3f0ac335050a7ed6779cf3f674, provider.json, record.json
        outcome.json: {"score":3,"state":"finished","verdict":{"kind":"judged","result":{"returned":{"raw_score":3},"score":3},"submission":{"finalized":true,"payload":{"digest":"sha256:44778d82365e4af681c40d5f0eef5cf6f5899d3f0ac335050a7ed6779cf3f674","size":3}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}
        printed outcome.json: true"#]],
    );
    stand_in.stop();
    wait_for_each_run(&scratch);
}
