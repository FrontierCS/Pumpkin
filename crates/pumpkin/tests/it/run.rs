//! `run`: what it finds wrong before it reaches the engine, and once it has, and runs on Docker
//! followed to their ends with `--wait`.

use std::ffi::OsStr;
use std::fs::Permissions;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::Command;
use std::thread::{self, JoinHandle};
use std::{env, fs};

use docker::{ContextName, Engine};
use expect_test::expect;
use schema::sandbox::{MODEL_PORT, SANDBOXCTL_PATH};
use serde_json::{Value, json};

use super::{
    BUSYBOX, LWE, PUMPKIN, Scratch, StandIn, Step, check, context, docker_table, image, report,
    sandboxd, workflow,
};

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn usage() {
    let scratch = Scratch::new("usage");
    let same = |args: &[&str], like: &[&str]| {
        let ran = |args| scratch.pumpkin(args).output().unwrap();
        let same = ran(args) == ran(like);
        let (args, like) = (args.join(" "), like.join(" "));
        Step::Ran(format!(
            "$ pumpkin {args}\ndoes what `pumpkin {like}` does: {same}"
        ))
    };
    let pumpkin = |args: &[&str]| Step::from(scratch.pumpkin(args));
    let mut wait = scratch.pumpkin(&["wait"]);
    let output = wait.arg(OsStr::from_bytes(b"\xff")).output().unwrap();
    let not_utf8 = Step::Ran(report(
        "$ pumpkin wait $'\\xff'".to_owned(),
        &output,
        &scratch.0,
        false,
    ));
    let steps = [
        pumpkin(&["--help"]),
        pumpkin(&["run", "--help"]),
        pumpkin(&["wait", "-h"]),
        pumpkin(&["help", "--help"]),
        pumpkin(&["help", "config"]),
        same(&["help", "run"], &["run", "--help"]),
        same(&["help"], &["--help"]),
        same(&["-h"], &["--help"]),
        same(&["bogus", "--help"], &["--help"]),
        same(&["run", "--bogus", "-h", "extra"], &["run", "--help"]),
        pumpkin(&[]),
        pumpkin(&["bogus"]),
        pumpkin(&["--version"]),
        pumpkin(&["run", "--docker-context", "nowhere"]),
        pumpkin(&["run"]),
        pumpkin(&["run", "--runs=a", "--runs", "b", "a.json"]),
        not_utf8,
        pumpkin(&["help", "bogus"]),
        pumpkin(&["wait", "--runs=nowhere", "6a"]),
    ];
    check(
        &scratch,
        steps,
        false,
        &[],
        &expect![[r#"
            $ pumpkin --help
            exit status: Some(0)
            printed:
              Run workflows: in each run, an agent works in a sandbox, and judges score what
              it submits.

              Usage: pumpkin <command>

              Commands:
                run   Start a run of a workflow
                wait  Wait until a run has ended, and print its outcome
                help  Print the help of a command or a topic

              Options:
                -h, --help
                        Print this help.

              Topics:
                config  The machine's configuration, pumpkin.toml

              `pumpkin help <command>` prints a command's help, as `pumpkin <command> --help`
              does, and `pumpkin help <topic>` a topic.
            $ pumpkin run --help
            exit status: Some(0)
            printed:
              Start a run of a workflow, and print its id once it has started.

              Usage: pumpkin run [OPTIONS] <ir>

              Arguments:
                <ir>
                        The workflow's IR, a JSON file.

              Options:
                    --runs <dir>
                        The directory runs are recorded in, each in <dir>/<id>.
                        Default: $HOME/.local/share/pumpkin/runs

                    --config <file>
                        The machine's configuration; see `pumpkin help config`.
                        Default: $HOME/.config/pumpkin/pumpkin.toml

                    --sandboxd <path>
                        The static sandboxd that every sandbox runs, as `cargo xtask sandboxd`
                        builds it.
                        Default: the one beside this pumpkin, where `cargo xtask install` puts
                        one

                    --wait
                        Follow the run to its end, as `pumpkin wait` does, and print its
                        outcome.json instead of its id.

                -h, --help
                        Print this help.

              Examples:
                pumpkin run workflow.json
                pumpkin run --wait workflow.json > outcome.json

              Exit status:
                0  The run has started.
                1  An error; a run that failed to start prints its outcome.json first.
                2  A usage error.
              With --wait, `run` exits as `pumpkin wait` does once the run has started. Every
              exit but 0 ends with a `pumpkin:` line on the standard error, which says why.

              The run runs on the engine of the Docker context that the configuration names;
              if that engine runs as root, `run` says so first. A driver process that `run`
              starts drives the run, and writes what it says after the start to driver.log in
              the record. Whatever ends `run` before the start ends the driver too; after it,
              the run goes on without `run`, and `pumpkin wait` follows it.

              The run outlives a logout unless its engine stops then, as rootless Docker does
              without `loginctl enable-linger`, or logind kills the session's processes
              (KillUserProcesses): then only a run under the user's manager outlives it, so
              enable lingering and start `run` under `systemd-run --user --scope`. Killing the
              driver leaves the run's containers, labelled pumpkin.run=<id>, its socket
              directory in the temporary directory, and its record without an outcome.json.
            $ pumpkin wait -h
            exit status: Some(0)
            printed:
              Wait until a run has ended, then print its outcome.json.

              Usage: pumpkin wait [OPTIONS] <run>

              Arguments:
                <run>
                        The run: its id; a prefix of its id, or of the 8 hexadecimal digits
                        that end it, that no other run in --runs shares; or its record's path,
                        which has a `/`.

              Options:
                    --runs <dir>
                        The directory that <run> is looked up in.
                        Default: $HOME/.local/share/pumpkin/runs

                -h, --help
                        Print this help.

              Examples:
                pumpkin wait 20261108-000905-126a21e9
                pumpkin wait 126a
                pumpkin wait runs/20261108-000905-126a21e9

              Exit status:
                0  The run finished with a score.
                3  The run finished without one.
                1  The run failed, or was lost as its driver ended without an outcome.json; or
                   another error.
                2  A usage error.
              Once the run has ended, `wait` ends with a `pumpkin:` line on the standard error
              that says how, and where its record is; any other exit but 0 ends with one that
              says why.
              If the final judge returned no result, its standard error is shown first, or,
              if it is long, named.

              Ending `pumpkin wait` doesn't stop the run. A record made before records were
              locked has no driver.pid, and is refused.
            $ pumpkin help --help
            exit status: Some(0)
            printed:
              Print the help of a command or a topic, or `pumpkin --help` without one.

              Usage: pumpkin help [topic]

              Arguments:
                [topic]
                        A command, or a topic: `config`, the machine's configuration.

              Options:
                -h, --help
                        Print this help.

              Examples:
                pumpkin help run
                pumpkin help config

              `pumpkin help <command>` prints what `pumpkin <command> --help` does.
            $ pumpkin help config
            exit status: Some(0)
            printed:
              pumpkin.toml, the machine's configuration, which every `pumpkin run` reads: the
              file that --config names, $HOME/.config/pumpkin/pumpkin.toml by default. It is
              TOML, with the tables below. Every key is required, but the [upstreams.<name>]
              tables, and any other key is refused.

              Example:
                [docker]
                context = "default"

                [upstreams.gateway]
                url = "https://gateway.example/v1"
                key_file = "gateway-key"
                models = [{ name = "model-a", upstream_name = "provider/model-a" }]

              [docker], the Docker engine that the machine's runs run on:
                context       The context that reaches it, as `docker context ls` lists it.

              [upstreams.<name>], for each upstream that a workflow's model route may reach:
                url           The base URL of its API: https, or http to a loopback IP
                              address, such as 127.0.0.1 but not localhost. The route reaches
                              /responses below it.
                key_file      The file that holds its key, below this file's directory unless
                              absolute.
                models        The models it serves, each with the `name` that a route allows
                              and the `upstream_name` that the upstream takes for it. No model
                              is listed twice, under one upstream or two.

              A workflow's model route reaches the one upstream that serves every model it
              allows.
            $ pumpkin help run
            does what `pumpkin run --help` does: true
            $ pumpkin help
            does what `pumpkin --help` does: true
            $ pumpkin -h
            does what `pumpkin --help` does: true
            $ pumpkin bogus --help
            does what `pumpkin --help` does: true
            $ pumpkin run --bogus -h extra
            does what `pumpkin run --help` does: true
            $ pumpkin
            exit status: Some(2)
            Run workflows: in each run, an agent works in a sandbox, and judges score what
            it submits.

            Usage: pumpkin <command>

            Commands:
              run   Start a run of a workflow
              wait  Wait until a run has ended, and print its outcome
              help  Print the help of a command or a topic

            Options:
              -h, --help  Print this help.

            Topics:
              config  The machine's configuration, pumpkin.toml

            `pumpkin help <command>` prints a command's help, as `pumpkin <command> --help`
            does, and `pumpkin help <topic>` a topic.
            pumpkin: to read how to use it: pumpkin help
            $ pumpkin bogus
            exit status: Some(2)
            error: unrecognized subcommand 'bogus'

            Usage: pumpkin <command>
            pumpkin: to read how to use it: pumpkin help
            $ pumpkin --version
            exit status: Some(2)
            error: unexpected argument '--version' found

            Usage: pumpkin <command>
            pumpkin: to read how to use it: pumpkin help
            $ pumpkin run --docker-context nowhere
            exit status: Some(2)
            error: unexpected argument '--docker-context' found

              tip: to pass '--docker-context' as a value, use '-- --docker-context'

            Usage: pumpkin run [OPTIONS] <ir>
            pumpkin: to read how to use it: pumpkin help run
            $ pumpkin run
            exit status: Some(2)
            error: the following required arguments were not provided:
              <ir>

            Usage: pumpkin run <ir>
            pumpkin: to read how to use it: pumpkin help run
            $ pumpkin run --runs=a --runs b a.json
            exit status: Some(2)
            error: the argument '--runs <dir>' cannot be used multiple times

            Usage: pumpkin run [OPTIONS] <ir>
            pumpkin: to read how to use it: pumpkin help run
            $ pumpkin wait $'\xff'
            exit status: Some(2)
            error: invalid value '�' for '<run>': it isn't UTF-8, as every id is: name an id, a prefix of one, or a record's path, which has a `/`
            pumpkin: to read how to use it: pumpkin help wait
            $ pumpkin help bogus
            exit status: Some(2)
            error: there is no help for `bogus`: expected a command (`run`, `wait`, `help`) or a topic (`config`)

            Usage: pumpkin help [topic]
            pumpkin: to read how to use it: pumpkin help help
            $ pumpkin wait --runs=nowhere 6a
            exit status: Some(1)
            pumpkin: nowhere: No such file or directory (os error 2): expected the directory that runs are recorded in, as `--runs` names it"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn unrunnable() {
    let scratch = Scratch::new("unrunnable");
    scratch.write("lwe.json", &serde_json::from_str::<Value>(LWE).unwrap());
    scratch.write("empty.json", &json!({}));
    scratch.write_sandboxd("private");
    fs::set_permissions(scratch.0.join("private"), Permissions::from_mode(0o750)).unwrap();
    scratch.write_executable("script", b"#!/bin/sh\n");
    // Copies of `pumpkin`: one alone, and one beside a `sandboxd` that is `pumpkin` too, linked
    // dynamically, as `cargo build` leaves one in a checkout.
    let (alone, checkout) = (scratch.0.join("alone"), scratch.0.join("checkout"));
    fs::create_dir(&alone).unwrap();
    fs::create_dir(&checkout).unwrap();
    fs::copy(PUMPKIN, alone.join("pumpkin")).unwrap();
    fs::hard_link(alone.join("pumpkin"), checkout.join("pumpkin")).unwrap();
    fs::hard_link(alone.join("pumpkin"), checkout.join("sandboxd")).unwrap();
    let runs = ["--runs", "runs"];
    let run = |ir, rest: &[&str]| scratch.pumpkin(&[&["run", ir], &runs[..], rest].concat());
    let dashed = scratch.pumpkin(&[&["run"], &runs[..], &["--", "-missing.json"]].concat());
    let beside = |dir: &PathBuf| {
        let args = [&["run", "lwe.json"], &runs[..]].concat();
        scratch.program(dir.join("pumpkin"), &args)
    };
    let mut unset = scratch.pumpkin(&["run", "lwe.json"]);
    unset.env_remove("HOME");
    let mut empty = scratch.pumpkin(&["run", "lwe.json"]);
    empty.env("HOME", "");
    let commands = [
        run("missing.json", &[]),
        dashed,
        run("empty.json", &[]),
        run("lwe.json", &["--sandboxd", "missing"]),
        run("lwe.json", &["--sandboxd", "private"]),
        run("lwe.json", &["--sandboxd", "script"]),
        beside(&alone),
        beside(&checkout),
        unset,
        empty,
    ];
    check(
        &scratch,
        commands,
        false,
        &[],
        &expect![[r#"
            $ pumpkin run missing.json --runs runs
            exit status: Some(1)
            pumpkin: the workflow's IR missing.json can't be read (No such file or directory (os error 2)): name the file that holds it
            $ pumpkin run --runs runs -- -missing.json
            exit status: Some(1)
            pumpkin: the workflow's IR -missing.json can't be read (No such file or directory (os error 2)): name the file that holds it
            $ pumpkin run empty.json --runs runs
            exit status: Some(1)
            pumpkin: the workflow's IR empty.json: missing field `ir_version` at line 1 column 2
            $ pumpkin run lwe.json --runs runs --sandboxd missing
            exit status: Some(1)
            pumpkin: the sandboxd missing can't be read (No such file or directory (os error 2)): name a static one, which `cargo xtask sandboxd` builds
            $ pumpkin run lwe.json --runs runs --sandboxd private
            exit status: Some(1)
            pumpkin: the sandboxd private can't be executed by every user, as its mode is 0750: make it executable by every user (`chmod a+x`)
            $ pumpkin run lwe.json --runs runs --sandboxd script
            exit status: Some(1)
            pumpkin: the sandboxd script isn't a 64-bit ELF executable: name a static one, which `cargo xtask sandboxd` builds
            $ <scratch>/alone/pumpkin run lwe.json --runs runs
            exit status: Some(1)
            pumpkin: the sandboxd beside `pumpkin`, <scratch>/alone/sandboxd, can't be read (No such file or directory (os error 2)): `cargo xtask install` installs `pumpkin` beside a static one, or name one with `--sandboxd`
            $ <scratch>/checkout/pumpkin run lwe.json --runs runs
            exit status: Some(1)
            pumpkin: the sandboxd beside `pumpkin`, <scratch>/checkout/sandboxd, is linked dynamically, and a sandbox runs only a static one: `cargo xtask install` installs `pumpkin` beside a static one, or name one with `--sandboxd`
            $ env -u HOME pumpkin run lwe.json
            exit status: Some(1)
            pumpkin: HOME is unset or empty: set it, or name a directory with `--runs`
            $ HOME= pumpkin run lwe.json
            exit status: Some(1)
            pumpkin: HOME is unset or empty: set it, or name a directory with `--runs`"#]],
    );
    assert!(!scratch.0.join("runs").exists(), "a refusal made a record");
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
            pumpkin: run <id> finished without a score: the agent submitted nothing, and the work phase ended when the agent exited with status 0; its record is <scratch>/runs/<id>
            printed outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            $ pumpkin wait <id> --runs runs
            exit status: Some(0)
            pumpkin: run <id> finished with a score of 3, after the work phase ended when the agent exited with status 0; its record is <scratch>/runs/<id>
            printed outcome.json: {"score":3,"state":"finished","verdict":{"kind":"judged","result":{"returned":{"raw_score":3},"score":3},"submission":{"finalized":true,"payload":{"digest":"sha256:44778d82365e4af681c40d5f0eef5cf6f5899d3f0ac335050a7ed6779cf3f674","size":3}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            $ pumpkin wait <id> --runs runs
            exit status: Some(3)
            pumpkin: the final judge's standard error, <scratch>/runs/<id>/jobs/0/log/command.stderr, holds:
              scoring failed
              \u{1b}[1mbold\u{1b}[0m
            pumpkin: run <id> finished without a score: the final judge returned no result (invalid_result: the judge wrote no result: write it at /pumpkin/out/result.json), after the work phase ended when the agent exited with status 0; its record is <scratch>/runs/<id>
            printed outcome.json: {"score":null,"state":"finished","verdict":{"failure":{"kind":"invalid_result","reason":"the judge wrote no result: write it at /pumpkin/out/result.json"},"kind":"no_result","submission":{"finalized":true,"payload":{"digest":"sha256:44778d82365e4af681c40d5f0eef5cf6f5899d3f0ac335050a7ed6779cf3f674","size":3}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}"#]],
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
    let stand_in = StandIn::new(&scratch);
    let mut large = workflow(BUSYBOX, &["true"]);
    large["roles"][0]["needs"]["cpu_millis"] = json!(4001);
    large["roles"][1]["needs"]["memory_bytes"] = json!((8u64 << 30) + 1);
    // All that the stand-in's host has, which fits.
    large["roles"][2]["needs"] = json!({ "cpu_millis": 4000, "memory_bytes": 8u64 << 30 });
    scratch.write("large.json", &large);
    let run = |ir, config: &str| {
        let mut command = scratch.pumpkin(&["run", ir, "--sandboxd", "sandboxd"]);
        command.args(["--runs", "runs", "--config", config]);
        command
    };
    let commands = [
        run("broken.json", "conf/served.toml"),
        run("lwe.json", "unserved.toml"),
        stand_in.run("large.json", &[]),
    ];
    check(
        &scratch,
        commands,
        false,
        &[],
        &expect![[r#"
            $ pumpkin run broken.json --sandboxd sandboxd --runs runs --config conf/served.toml
            exit status: Some(1)
            pumpkin: broken.json: /roles/1/name: `agent` also names /roles/0: give each a unique name
            pumpkin: broken.json: /roles/1/egress/model: `agent` is a judge, and only the agent has a model route: remove it
            pumpkin: broken.json: /main/solo_then_judge/grants/0/judge/role: no role is named `train_judge`: name one listed in `roles`
            pumpkin: broken.json: the workflow has 3 problems: fix them and run again
            $ pumpkin run lwe.json --sandboxd sandboxd --runs runs --config unserved.toml
            exit status: Some(1)
            pumpkin: unserved.toml: no upstream serves every model that the workflow's route allows, `gpt-6-astra`: list them all under one
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run large.json --sandboxd sandboxd --runs runs --config pumpkin.toml
            exit status: Some(1)
            pumpkin: large.json: /roles/0/needs: `agent` fits no class of sandbox, as `container` offers 4000 thousandths of a CPU, fewer than its 4001: ask for no more than a class offers
            pumpkin: large.json: /roles/1/needs: `train_judge` fits no class of sandbox, as `container` offers 8589934592 bytes of memory, fewer than its 8589934593: ask for no more than a class offers
            pumpkin: large.json: placing the workflow's roles in the engine's classes has 2 problems: fix them and run again"#]],
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
        let mut command = scratch.pumpkin(&["run", ir, "--sandboxd", "sandboxd"]);
        command.args(["--runs", "runs"]).args(config);
        command
    };
    let config = |name: &str| run("lwe.json", &["--config", &format!("{name}.toml")]);
    let mut home = run("lwe.json", &[]);
    home.env("HOME", scratch.0.join("home"));
    let mut homeless = run("lwe.json", &[]);
    homeless.env_remove("HOME");
    let mut served = config("conf/served");
    let mut unrouted = run("unrouted.json", &["--config", "docked.toml"]);
    // Without `docker` on `PATH`, `run` can't reach the engine past the configuration.
    for command in [&mut served, &mut unrouted] {
        command.env("PATH", scratch.0.join("nowhere"));
    }
    let refused = [
        "missing", "undocked", "nameless", "broken", "inline", "priced", "keyless", "invalid",
    ];
    let refused = (refused.into_iter()).chain([
        "twice",
        "again",
        "query",
        "fragment",
        "unserved",
        "docked",
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
            $ HOME=<scratch>/home pumpkin run lwe.json --sandboxd sandboxd --runs runs
            exit status: Some(1)
            pumpkin: the configuration <scratch>/home/.config/pumpkin/pumpkin.toml can't be read (No such file or directory (os error 2)): write one there, as `pumpkin help config` says, or name another file with `--config`
            $ env -u HOME pumpkin run lwe.json --sandboxd sandboxd --runs runs
            exit status: Some(1)
            pumpkin: HOME is unset or empty: set it, or name the configuration with `--config`
            $ pumpkin run lwe.json --sandboxd sandboxd --runs runs --config missing.toml
            exit status: Some(1)
            pumpkin: the configuration missing.toml can't be read (No such file or directory (os error 2)): write one there, as `pumpkin help config` says, or name another file with `--config`
            $ pumpkin run lwe.json --sandboxd sandboxd --runs runs --config undocked.toml
            exit status: Some(1)
            pumpkin: undocked.toml: TOML parse error at line 1, column 1
              |
            1 | [upstreams.gateway]
              | ^
            missing field `docker`
            $ pumpkin run lwe.json --sandboxd sandboxd --runs runs --config nameless.toml
            exit status: Some(1)
            pumpkin: nameless.toml: TOML parse error at line 2, column 11
              |
            2 | context = ""
              |           ^^
            the Docker context's name is empty: name one that `docker context ls` lists
            $ pumpkin run lwe.json --sandboxd sandboxd --runs runs --config broken.toml
            exit status: Some(1)
            pumpkin: broken.toml: TOML parse error at line 4, column 19
              |
            4 | [upstreams.gateway
              |                   ^
            unclosed table, expected `]`
            $ pumpkin run lwe.json --sandboxd sandboxd --runs runs --config inline.toml
            exit status: Some(1)
            pumpkin: inline.toml: TOML parse error at line 6, column 1
              |
            6 | key = "sk-1"
              | ^^^
            unknown field `key`, expected one of `url`, `key_file`, `models`
            $ pumpkin run lwe.json --sandboxd sandboxd --runs runs --config priced.toml
            exit status: Some(1)
            pumpkin: priced.toml: TOML parse error at line 7, column 73
              |
            7 | models = [{ name = "gpt-6-astra", upstream_name = "vendor/gpt-6-astra", price = 1 }]
              |                                                                         ^^^^^
            unknown field `price`, expected `name` or `upstream_name`
            $ pumpkin run lwe.json --sandboxd sandboxd --runs runs --config keyless.toml
            exit status: Some(1)
            pumpkin: keyless.toml: TOML parse error at line 4, column 1
              |
            4 | [upstreams.gateway]
              | ^^^^^^^^^^^^^^^^^^^
            missing field `key_file`
            $ pumpkin run lwe.json --sandboxd sandboxd --runs runs --config invalid.toml
            exit status: Some(1)
            pumpkin: invalid.toml: TOML parse error at line 4, column 12
              |
            4 | [upstreams.Gateway]
              |            ^^^^^^^
            `Gateway` is not a valid name: use 1 to 64 characters from a-z, 0-9, `_` and `-`, starting with a letter
            $ pumpkin run lwe.json --sandboxd sandboxd --runs runs --config twice.toml
            exit status: Some(1)
            pumpkin: twice.toml: TOML parse error at line 9, column 12
              |
            9 | [upstreams.gateway]
              |            ^^^^^^^
            duplicate key
            $ pumpkin run lwe.json --sandboxd sandboxd --runs runs --config again.toml
            exit status: Some(1)
            pumpkin: again.toml: `gpt-6-astra` is listed more than once: list each model once, under the upstream that serves it
            $ pumpkin run lwe.json --sandboxd sandboxd --runs runs --config query.toml
            exit status: Some(1)
            pumpkin: query.toml: TOML parse error at line 5, column 7
              |
            5 | url = "https://models.example.com/v1?project=1"
              |       ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
            the upstream's URL has a query or a fragment: name the API's base URL alone, below which the route reaches `/responses`
            $ pumpkin run lwe.json --sandboxd sandboxd --runs runs --config fragment.toml
            exit status: Some(1)
            pumpkin: fragment.toml: TOML parse error at line 5, column 7
              |
            5 | url = "https://models.example.com/v1#v2"
              |       ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
            the upstream's URL has a query or a fragment: name the API's base URL alone, below which the route reaches `/responses`
            $ pumpkin run lwe.json --sandboxd sandboxd --runs runs --config unserved.toml
            exit status: Some(1)
            pumpkin: unserved.toml: no upstream serves every model that the workflow's route allows, `gpt-6-astra`: list them all under one
            $ pumpkin run lwe.json --sandboxd sandboxd --runs runs --config docked.toml
            exit status: Some(1)
            pumpkin: docked.toml: no upstream serves every model that the workflow's route allows, `gpt-6-astra`: add an `[upstreams.<name>]` that lists them
            $ pumpkin run lwe.json --sandboxd sandboxd --runs runs --config conf/unkeyed.toml
            exit status: Some(1)
            pumpkin: conf/unkeyed.toml: the key file conf/missing can't be read (No such file or directory (os error 2)): name a file that holds the model provider's key
            $ PATH=<scratch>/nowhere pumpkin run lwe.json --sandboxd sandboxd --runs runs --config conf/served.toml
            exit status: Some(1)
            pumpkin: the Docker context `nowhere` can't be read: `docker` didn't run: No such file or directory (os error 2): install Docker's command line on `PATH`: name one that `docker context ls` lists
            $ PATH=<scratch>/nowhere pumpkin run unrouted.json --sandboxd sandboxd --runs runs --config docked.toml
            exit status: Some(1)
            pumpkin: the Docker context `nowhere` can't be read: `docker` didn't run: No such file or directory (os error 2): install Docker's command line on `PATH`: name one that `docker context ls` lists"#]],
    );
}

/// Writes into `scratch` the stand-in `sandboxd` that [`configurations`] and [`refusals`] give
/// `run`, and the machine's configurations, each named for what it holds.
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
    scratch.write_sandboxd("sandboxd");
    scratch.write("conf/key", "sk-test\n");
    let configs = [
        ("conf/served", format!("{gateway}\n{direct}")),
        ("docked", String::new()),
        ("nameless", gateway.clone()),
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
        let docker = match name {
            "nameless" => docker_table(""),
            _ => docker_table("nowhere"),
        };
        let config = format!("{docker}\n{config}");
        scratch.write(&format!("{name}.toml"), &config);
    }
    scratch.write("undocked.toml", &gateway);
}

/// Writes into `scratch` the workflow `asks.json`, [`workflow`]'s with the fixture's model route,
/// whose agent asks its model and keeps the answer in its `out`; and the configuration
/// `conf/pumpkin.toml`, of the Docker context `context`, whose one upstream, the stand-in at
/// `address`, serves the route's model as `vendor/gpt-6-astra`, with its key in `conf/key`.
fn write_asks(scratch: &Scratch, image: &str, context: &str, address: &str) {
    let config = format!(
        r#"{}
[upstreams.provider]
url = "http://{address}/v1/"
key_file = "key"
models = [{{ name = "gpt-6-astra", upstream_name = "vendor/gpt-6-astra" }}]
"#,
        docker_table(context)
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
struct Upstream {
    address: String,
    thread: JoinHandle<()>,
}

impl Upstream {
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
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn runs_on_docker() {
    let scratch = Scratch::new("docker");
    let sandboxd = sandboxd();
    let context = context();
    let engine = Engine::connect(ContextName::try_from(context.clone()).unwrap()).unwrap();
    let image = image(&engine);
    let upstream = Upstream::start();
    write_asks(&scratch, &image, &context, &upstream.address);
    scratch.write("home/.config/pumpkin/pumpkin.toml", &docker_table(&context));
    scratch.write("exits.json", &workflow(&image, &["true"]));
    let submit = format!("printf six > six && {SANDBOXCTL_PATH} submit submit six --finalize");
    scratch.write("submits.json", &workflow(&image, &["sh", "-c", &submit]));
    let mut fails = workflow(&image, &["sh", "-c", &submit]);
    let fail = r"printf 'scoring failed\n\033[1mbold\033[0m\n' >&2; exit 1";
    fails["roles"][2]["runtime"]["judge"]["command"]["argv"] = json!(["sh", "-c", fail]);
    scratch.write("fails.json", &fails);
    let run = |ir, rest: &[&str]| {
        let mut command = scratch.pumpkin(&["run", ir, "--sandboxd", &sandboxd, "--wait"]);
        command.args(rest);
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
    let config = ["--runs", "runs", "--config", "conf/pumpkin.toml"];
    let commands = [
        run("asks.json", &config),
        exits,
        run("submits.json", &config),
        run("fails.json", &config),
    ];
    let shown = [
        (&sandboxd[..], "<sandboxd>"),
        (&docker_config[..], "<docker config>"),
        (&upstream.address[..], "<stand-in>"),
    ];
    let rootful = !engine.report().security.rootless;
    check(
        &scratch,
        commands,
        rootful,
        &shown,
        &expect![[r#"
            $ pumpkin run asks.json --sandboxd <sandboxd> --wait --runs runs --config conf/pumpkin.toml
            exit status: Some(3)
            pumpkin: run <id> records into <scratch>/runs/<id>
            pumpkin: run <id> started
            pumpkin: following run <id>, recorded in <scratch>/runs/<id>: ending `pumpkin run` does not stop it
            pumpkin: run <id> finished without a score: the agent submitted nothing, and the work phase ended when the agent exited with status 0; its record is <scratch>/runs/<id>
            record: agent/log/command.stderr, agent/log/command.stdout, agent/out/answer, driver.log, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            the run log notes: the agent's model route goes to the upstream `provider`, which takes `gpt-6-astra` as `vendor/gpt-6-astra`
            the run log records: {"answer":{"kind":"forwarded"},"kind":"model_answered","request":0,"upstream_status":200}
            the agent's answer: data: POST /v1/responses HTTP/1.1 Some("Bearer sk-test") {"input":"hi","model":"vendor/gpt-6-astra","store":false}\n\n
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            printed outcome.json: true
            $ DOCKER_CONFIG=<docker config> HOME=<scratch>/home XDG_DATA_HOME=<scratch>/xdg pumpkin run exits.json --sandboxd <sandboxd> --wait
            exit status: Some(3)
            pumpkin: run <id> records into <scratch>/home/.local/share/pumpkin/runs/<id>
            pumpkin: run <id> started
            pumpkin: following run <id>, recorded in <scratch>/home/.local/share/pumpkin/runs/<id>: ending `pumpkin run` does not stop it
            pumpkin: run <id> finished without a score: the agent submitted nothing, and the work phase ended when the agent exited with status 0; its record is <scratch>/home/.local/share/pumpkin/runs/<id>
            record: agent/log/command.stderr, agent/log/command.stdout, agent/out/, driver.log, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            printed outcome.json: true
            $ pumpkin run submits.json --sandboxd <sandboxd> --wait --runs runs --config conf/pumpkin.toml
            exit status: Some(0)
            pumpkin: run <id> records into <scratch>/runs/<id>
            pumpkin: run <id> started
            pumpkin: following run <id>, recorded in <scratch>/runs/<id>: ending `pumpkin run` does not stop it
            pumpkin: run <id> finished with a score of 3, after the work phase ended when the agent exited with status 0; its record is <scratch>/runs/<id>
            record: agent/log/command.stderr, agent/log/command.stdout, agent/out/, driver.log, driver.pid, ir.json, jobs/0/log/command.stderr, jobs/0/log/command.stdout, jobs/0/out/evidence/, jobs/0/out/result.json, log.jsonl, outcome.json, payloads/44778d82365e4af681c40d5f0eef5cf6f5899d3f0ac335050a7ed6779cf3f674, provider.json, record.json
            outcome.json: {"score":3,"state":"finished","verdict":{"kind":"judged","result":{"returned":{"raw_score":3},"score":3},"submission":{"finalized":true,"payload":{"digest":"sha256:44778d82365e4af681c40d5f0eef5cf6f5899d3f0ac335050a7ed6779cf3f674","size":3}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            printed outcome.json: true
            $ pumpkin run fails.json --sandboxd <sandboxd> --wait --runs runs --config conf/pumpkin.toml
            exit status: Some(3)
            pumpkin: run <id> records into <scratch>/runs/<id>
            pumpkin: run <id> started
            pumpkin: following run <id>, recorded in <scratch>/runs/<id>: ending `pumpkin run` does not stop it
            pumpkin: the final judge's standard error, <scratch>/runs/<id>/jobs/0/log/command.stderr, holds:
              scoring failed
              \u{1b}[1mbold\u{1b}[0m
            pumpkin: run <id> finished without a score: the final judge returned no result (invalid_result: the judge wrote no result: write it at /pumpkin/out/result.json), after the work phase ended when the agent exited with status 0; its record is <scratch>/runs/<id>
            record: agent/log/command.stderr, agent/log/command.stdout, agent/out/, driver.log, driver.pid, ir.json, jobs/0/log/command.stderr, jobs/0/log/command.stdout, jobs/0/out/evidence/, log.jsonl, outcome.json, payloads/44778d82365e4af681c40d5f0eef5cf6f5899d3f0ac335050a7ed6779cf3f674, provider.json, record.json
            outcome.json: {"score":null,"state":"finished","verdict":{"failure":{"kind":"invalid_result","reason":"the judge wrote no result: write it at /pumpkin/out/result.json"},"kind":"no_result","submission":{"finalized":true,"payload":{"digest":"sha256:44778d82365e4af681c40d5f0eef5cf6f5899d3f0ac335050a7ed6779cf3f674","size":3}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            printed outcome.json: true"#]],
    );
    upstream.stop();
    wait_for_each_run(&scratch);
}
