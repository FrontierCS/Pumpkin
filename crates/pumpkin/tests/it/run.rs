//! `run`: what it finds wrong before it reaches the engine, and once it has, and runs on Docker
//! followed to their ends with `--wait`, among them one whose agent makes ops through
//! `sandboxctl`.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::Permissions;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::{self, JoinHandle};
use std::{env, fs};

use expect_test::{Expect, expect};
use provider::{RunTag, Sweep as _};
use schema::sandbox::{MODEL_PORT, SANDBOXCTL_PATH};
use serde_json::{Value, json};
use test_support::{WORKFLOW, docker_context, files, sandboxd, standard_outputs};

use super::{
    LOCAL, PUMPKIN, RUNNER, Scratch, StandIn, Step, build_first, check, containers, cpu_list,
    docker_table, engine, invocation, ledger, machine, machine_cores, machine_cpus, ran, record,
    report, siblings, through_shell, with_room, workflow,
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
    // Each path that `wait`, `cancel` and `reap` are given is UTF-8 too, as the commands that
    // `pumpkin` says to run next name them, and each that `run` is given, as its record keeps it.
    let not_utf8 = |shown: &str, args: &[&[u8]]| {
        let mut command = scratch.pumpkin(&[]);
        command.args(args.iter().map(|arg| OsStr::from_bytes(arg)));
        let output = command.output().unwrap();
        Step::Ran(report(
            format!("$ pumpkin {shown}"),
            &output,
            &scratch.0,
            false,
        ))
    };
    let steps = [
        pumpkin(&["--help"]),
        pumpkin(&["run", "--help"]),
        pumpkin(&["wait", "-h"]),
        pumpkin(&["cancel", "--help"]),
        pumpkin(&["reap", "--help"]),
        pumpkin(&["help", "--help"]),
        pumpkin(&["help", "config"]),
        pumpkin(&["--version"]),
        same(&["help", "run"], &["run", "--help"]),
        same(&["help"], &["--help"]),
        same(&["-h"], &["--help"]),
        same(&["-V"], &["--version"]),
        same(&["bogus", "--help"], &["--help"]),
        same(&["run", "--bogus", "-h", "extra"], &["run", "--help"]),
        pumpkin(&[]),
        pumpkin(&["bogus"]),
        pumpkin(&["--bogus", "run"]),
        pumpkin(&["--version=x", "run", "a.star"]),
        pumpkin(&["run", "--docker-context", "nowhere"]),
        pumpkin(&["run"]),
        pumpkin(&["run", "--runs=a", "--runs", "b", "a.star"]),
        pumpkin(&["run", "a.json"]),
        pumpkin(&["run", "a.star", "--arg", "model"]),
        pumpkin(&["run", "a.star", "--arg", "a=1", "--arg", "a=2"]),
        pumpkin(&["cancel", "--force=yes", "6a"]),
        pumpkin(&["reap", "6a"]),
        not_utf8("wait $'\\xff'", &[b"wait", b"\xff"]),
        not_utf8("wait $'\\xff/x'", &[b"wait", b"\xff/x"]),
        not_utf8(
            "cancel --runs $'\\xff' 6a",
            &[b"cancel", b"--runs", b"\xff", b"6a"],
        ),
        not_utf8("reap --runs $'\\xff'", &[b"reap", b"--runs", b"\xff"]),
        not_utf8(
            "run --config $'\\xff' a.star",
            &[b"run", b"--config", b"\xff", b"a.star"],
        ),
        pumpkin(&["help", "bogus"]),
    ];
    // What `--version` prints: this build, which names its commit if it was made in a git
    // checkout of Pumpkin's repository.
    let this = format!("pumpkin {}\n", runtime::BUILD);
    // Filled in by `UPDATE_EXPECT=1` once the crate builds.
    check(
        &scratch,
        steps,
        false,
        &[(&this, "pumpkin <this>\n")],
        &expect![[r#"
            $ pumpkin --help
            exit status: Some(0)
            printed:
              Run workflows: in each run, an agent works in a sandbox, and judges score what
              it submits.

              Usage: pumpkin <command>

              Commands:
                check   Check a task's source, building and running nothing
                fmt     Format the workflow language's `.star` files
                run     Build a task's images, and start a run of it
                wait    Wait until a run has ended, and print its outcome
                cancel  Stop a run, and remove what it left
                help    Print the help of a command or a topic

              Options:
                -V, --version
                        Print the version.

                -h, --help
                        Print this help.

              Rarely needed:
                build   Build a task's images, and print its workflow's IR
                reap    Remove what the runs that have ended left

              Topics:
                config  The machine's configuration, pumpkin.toml

              `pumpkin help <command>` prints a command's help, as `pumpkin <command> --help`
              does, and `pumpkin help <topic>` a topic.
            $ pumpkin run --help
            exit status: Some(0)
            printed:
              Build a task as `pumpkin build` does, then start a run of its workflow, and print
              the run's id once it has started.

              Usage: pumpkin run [OPTIONS] <entry>

              Arguments:
                <entry>
                        The task's entry file, as `pumpkin check` takes it.

              Options:
                    --arg <name>=<value>
                        The value of the arg that an arg() call declares as <name>, as
                        `pumpkin check` takes it.

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
                pumpkin run tasks/bench/task.star --arg model=model-a
                pumpkin run tasks/bench/task.star --arg model=model-a --wait > outcome.json

              Exit status:
                0  The run has started.
                1  An error; a run that failed to start prints its outcome.json first.
                2  A usage error.
              With --wait, `run` exits as `pumpkin wait` does once the run has started. Every
              exit but 0 ends with a `pumpkin:` line on the standard error, which says why.

              Every ending of a run ends with the command that runs it again: `pumpkin run`
              of the task's entry, with the args and the flags that started it, their paths
              made absolute. It builds the task as its source is then. A run takes no seed:
              its id's random digits, and the grants by which its sandboxes reach it, are
              fresh in every run, so that no two runs share them.

              The task is checked, and the configuration read, before the machine's Docker
              engine is reached, warning if that engine runs as root; the task's images are
              built there; and then each provider that the configuration names is reached,
              before anything starts. The run places each role in the smallest class of
              sandbox that offers what the role needs, of every provider's, and refuses a
              role that fits none, saying why of each class, before anything is reserved.
              A driver process that `run` starts does all of that, and drives the run, and
              writes what it says after the start to driver.log in the record. Whatever ends
              `run` before the start ends the driver too; after it, the run goes on without
              `run`, and `pumpkin wait` follows it.

              On the Docker engine, each run holds whole cores of its own until it ends, which
              no other run's containers are given, each role as many as hold its CPU limit
              rounded up. A run whose roles need more cores at once than other runs leave is
              refused before its record is made: run it again once they have ended.

              The run outlives a logout unless its engine stops then, as rootless Docker does
              without `loginctl enable-linger`, or logind kills the session's processes
              (KillUserProcesses): then only a run under the user's manager outlives it, so
              enable lingering and start `run` under `systemd-run --user --scope`.

              `pumpkin cancel` stops the run by SIGTERM to the driver, whose pid is in the
              record's driver.pid, as SIGINT and SIGHUP do too: the run removes its
              sandboxes, and its outcome.json says which signal interrupted it. A signal
              ignored as `run` starts stays ignored. SIGKILL to the driver leaves the run
              lost, with its sandboxes, which `pumpkin cancel` removes: its containers,
              labelled pumpkin.run=<id>, and its socket directories in the record's sockets/,
              which `cancel` removes too.
            $ pumpkin wait -h
            exit status: Some(0)
            printed:
              Wait until a run has ended, then print its outcome.json.

              Usage: pumpkin wait [OPTIONS] <run>

              Arguments:
                <run>
                        The run: its id; a prefix of its id, or of the 8 hexadecimal digits that
                        end it, that no other run in --runs shares; or its record's path, which
                        has a `/`.

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
                1  The run failed, was interrupted, or was lost as its driver ended without
                   an outcome.json, which `pumpkin cancel` then records; or another error.
                2  A usage error.
              Once the run has ended, `wait` says on the standard error how it ended, and
              where its record is, then the command that runs it again, as `pumpkin run
              --help` says, and of a lost run, the commands that remove what it left; any
              other exit but 0 ends with a `pumpkin:` line that says why.
              If the final judge returned no result, its standard error is shown first, or,
              if it is long, named.

              Ending `pumpkin wait` doesn't stop the run: `pumpkin cancel` does.
            $ pumpkin cancel --help
            exit status: Some(0)
            printed:
              Stop a run, wait until it has ended, remove what it left, and print its
              outcome.json.

              Usage: pumpkin cancel [OPTIONS] <run>

              Arguments:
                <run>
                        The run: its id; a prefix of its id, or of the 8 hexadecimal digits that
                        end it, that no other run in --runs shares; or its record's path, which
                        has a `/`.

              Options:
                    --runs <dir>
                        The directory that <run> is looked up in.
                        Default: $HOME/.local/share/pumpkin/runs

                    --force
                        End the run's driver at once, by SIGKILL, rather than stop the run by
                        SIGTERM; the run is then lost, and recorded as failed. A create that the
                        driver had given up on may still make a sandbox after `cancel` has
                        ended, which `pumpkin reap` removes.

                -h, --help
                        Print this help.

              Examples:
                pumpkin cancel 126a
                pumpkin cancel --force 126a

              Exit status:
                0  The run has ended, and nothing that it left remains.
                1  An error.
                2  A usage error.
                3  The run has ended, but a provider of it can't be reached, so what it left
                   there may remain.

              A live run's driver, whose pid is in the record's driver.pid, gets SIGTERM, and
              the run stops as `pumpkin run --help` says, which `cancel` waits for; ending
              `pumpkin cancel` doesn't end the stop. A run that has ended, or is lost, gets no
              signal. Then `cancel` removes what `pumpkin reap` would of the run, on each
              provider that it ran on, as `pumpkin reap --help` says,
              and prints its outcome.json, then says how the run ended, as `pumpkin wait`
              does.
            $ pumpkin reap --help
            exit status: Some(0)
            printed:
              Remove what each run in --runs that has ended, or is lost, left, and print the
              id of each run that left something.

              Usage: pumpkin reap [OPTIONS]

              Options:
                    --runs <dir>
                        The directory of the runs to reap.
                        Default: $HOME/.local/share/pumpkin/runs

                -h, --help
                        Print this help.

              Examples:
                pumpkin reap
                pumpkin reap --runs runs

              Exit status:
                0  Nothing that a run which has ended left remains.
                1  A run can't be reaped, which `reap` says, or another error; it reaps the
                   others all the same.
                2  A usage error.
                3  Only that a provider of a run, which `reap` names, can't be reached, as
                   when its Docker context is gone, so what the run left there may remain.

              Of each run whose driver has ended, `reap` deletes the sandboxes of the run that
              remain on each provider that it ran on, such as one that a create given up at
              its deadline made: on the Docker engine of the context that the record's
              provider.json names, the containers labelled pumpkin.run=<id>. It removes the
              run's socket directories, in the record's sockets/, and a lost run's record it
              marks failed, as its driver ended without an outcome.json. It leaves a live run
              to its driver: `pumpkin cancel` stops one.

              Each sandbox that `reap` removes it notes in the record's reaps.jsonl, with the
              time; and each that the record holds undeleted, but that its provider no longer
              lists, it says, and notes there as gone.
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
              pumpkin.toml, the machine's configuration, which every `pumpkin run` and
              `pumpkin build` reads: the file that --config names,
              $HOME/.config/pumpkin/pumpkin.toml by default. It is TOML, with the tables below.
              Every key is required, but those of the [upstreams.<name>] tables, and any
              other key is refused.

              Example:
                [docker]
                context = "default"

                [providers.local.docker]

                [upstreams.gateway]
                url = "https://gateway.example/v1"
                key_file = "gateway-key"
                models = [{ name = "model-a", upstream_name = "provider/model-a" }]

              [docker], the machine's Docker engine:
                context       The context that reaches it, as `docker context ls` lists it.
                cpus          The CPUs that its sandboxes may be given, each run whole cores
                              of its own, as the kernel lists CPUs, such as "1-7,9-15";
                              absent, all that are online.

              [providers.<name>.<kind>], for each provider that the machine's runs may place
              roles on, as provider <name>, of kind <kind>, whose table holds that kind's own
              settings:
                docker        Sandboxes on the engine of [docker]; it takes no settings. A
                              provider of this kind is the configuration's one provider. Its
                              one class, `container`, offers the CPUs that sandboxes may be
                              given, the machine's memory, the flags that /proc/cpuinfo lists
                              for every CPU, no CPU model and no GPU.

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
            $ pumpkin --version
            exit status: Some(0)
            printed:
              pumpkin <this>
            $ pumpkin help run
            does what `pumpkin run --help` does: true
            $ pumpkin help
            does what `pumpkin --help` does: true
            $ pumpkin -h
            does what `pumpkin --help` does: true
            $ pumpkin -V
            does what `pumpkin --version` does: true
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
              check   Check a task's source, building and running nothing
              fmt     Format the workflow language's `.star` files
              run     Build a task's images, and start a run of it
              wait    Wait until a run has ended, and print its outcome
              cancel  Stop a run, and remove what it left
              help    Print the help of a command or a topic

            Options:
              -V, --version  Print the version.
              -h, --help     Print this help.

            Rarely needed:
              build   Build a task's images, and print its workflow's IR
              reap    Remove what the runs that have ended left

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
            $ pumpkin --bogus run
            exit status: Some(2)
            error: unexpected argument '--bogus' found

            Usage: pumpkin <command>
            pumpkin: to read how to use it: pumpkin help run
            $ pumpkin --version=x run a.star
            exit status: Some(2)
            error: unexpected value 'x' for '--version' found; no more were expected

            Usage: pumpkin --version <command>
            pumpkin: to read how to use it: pumpkin help run
            $ pumpkin run --docker-context nowhere
            exit status: Some(2)
            error: unexpected argument '--docker-context' found

              tip: to pass '--docker-context' as a value, use '-- --docker-context'

            Usage: pumpkin run [OPTIONS] <entry>
            pumpkin: to read how to use it: pumpkin help run
            $ pumpkin run
            exit status: Some(2)
            error: the following required arguments were not provided:
              <entry>

            Usage: pumpkin run <entry>
            pumpkin: to read how to use it: pumpkin help run
            $ pumpkin run --runs=a --runs b a.star
            exit status: Some(2)
            error: the argument '--runs <dir>' cannot be used multiple times

            Usage: pumpkin run [OPTIONS] <entry>
            pumpkin: to read how to use it: pumpkin help run
            $ pumpkin run a.json
            exit status: Some(2)
            error: invalid value 'a.json' for '<entry>': `a.json` names no `.star` file: name a task's entry file, whose name ends in `.star`
            pumpkin: to read how to use it: pumpkin help run
            $ pumpkin run a.star --arg model
            exit status: Some(2)
            error: `--arg model` has no `=`: give it as `--arg <name>=<value>`

            Usage: pumpkin run [OPTIONS] <entry>
            pumpkin: to read how to use it: pumpkin help run
            $ pumpkin run a.star --arg a=1 --arg a=2
            exit status: Some(2)
            error: `--arg a=2` names an arg given already: give each arg once

            Usage: pumpkin run [OPTIONS] <entry>
            pumpkin: to read how to use it: pumpkin help run
            $ pumpkin cancel --force=yes 6a
            exit status: Some(2)
            error: unexpected value 'yes' for '--force' found; no more were expected

            Usage: pumpkin cancel --force <run>
            pumpkin: to read how to use it: pumpkin help cancel
            $ pumpkin reap 6a
            exit status: Some(2)
            error: unexpected argument '6a' found

            Usage: pumpkin reap [OPTIONS]
            pumpkin: to read how to use it: pumpkin help reap
            $ pumpkin wait $'\xff'
            exit status: Some(2)
            error: invalid value '�' for '<run>': it isn't UTF-8, as every id is: name an id, a prefix of one, or a record's path, which has a `/`
            pumpkin: to read how to use it: pumpkin help wait
            $ pumpkin wait $'\xff/x'
            exit status: Some(2)
            error: invalid value '�/x' for '<run>': it isn't UTF-8, as the commands that `pumpkin` says to run next name it: name a record with a UTF-8 path
            pumpkin: to read how to use it: pumpkin help wait
            $ pumpkin cancel --runs $'\xff' 6a
            exit status: Some(2)
            error: invalid value '�' for '--runs <dir>': it isn't UTF-8, as the commands that `pumpkin` says to run next name it: name a directory with a UTF-8 path
            pumpkin: to read how to use it: pumpkin help cancel
            $ pumpkin reap --runs $'\xff'
            exit status: Some(2)
            error: invalid value '�' for '--runs <dir>': it isn't UTF-8, as the commands that `pumpkin` says to run next name it: name a directory with a UTF-8 path
            pumpkin: to read how to use it: pumpkin help reap
            $ pumpkin run --config $'\xff' a.star
            exit status: Some(2)
            error: invalid value '�' for '--config <file>': it isn't UTF-8, as a run's record keeps each path that `run` is given: name one that is
            pumpkin: to read how to use it: pumpkin help run
            $ pumpkin help bogus
            exit status: Some(2)
            error: there is no help for `bogus`: expected a command (`check`, `fmt`, `run`, `wait`, `cancel`, `help`, `build`, `reap`) or a topic (`config`)

            Usage: pumpkin help [topic]
            pumpkin: to read how to use it: pumpkin help help"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn unrunnable() {
    let scratch = Scratch::new("unrunnable");
    scratch.write_task(
        "workflow",
        &serde_json::from_str::<Value>(WORKFLOW).unwrap(),
    );
    scratch.write("empty/task.star", "");
    scratch.write_sandboxd("sandboxd");
    scratch.write_sandboxd("private");
    fs::set_permissions(scratch.0.join("private"), Permissions::from_mode(0o750)).unwrap();
    scratch.write_executable("script", b"#!/bin/sh\n");
    // Links to `pumpkin`: one alone, and one beside a `sandboxd` that is `pumpkin` too, linked
    // dynamically.
    let (alone, checkout) = (scratch.0.join("alone"), scratch.0.join("checkout"));
    for dir in [&alone, &checkout] {
        fs::create_dir_all(dir).unwrap();
        fs::hard_link(PUMPKIN, dir.join("pumpkin")).unwrap();
    }
    fs::hard_link(PUMPKIN, checkout.join("sandboxd")).unwrap();
    let runs = ["--runs", "runs"];
    let run = |entry, rest: &[&str]| scratch.pumpkin(&[&["run", entry], &runs[..], rest].concat());
    let dashed = [
        &["run"],
        &runs[..],
        &["--sandboxd", "sandboxd", "--", "-missing.star"],
    ];
    let dashed = scratch.pumpkin(&dashed.concat());
    let beside = |dir: &PathBuf| {
        let args = [&["run", "workflow/task.star"], &runs[..]].concat();
        scratch.program(dir.join("pumpkin"), &args)
    };
    let mut unset = scratch.pumpkin(&["run", "workflow/task.star", "--sandboxd", "sandboxd"]);
    unset.env_remove("HOME");
    let commands = [
        run("missing/task.star", &["--sandboxd", "sandboxd"]),
        dashed,
        run("empty/task.star", &["--sandboxd", "sandboxd"]),
        run("workflow/task.star", &["--sandboxd", "missing"]),
        run("workflow/task.star", &["--sandboxd", "private"]),
        run("workflow/task.star", &["--sandboxd", "script"]),
        beside(&alone),
        beside(&checkout),
        unset,
    ];
    check(
        &scratch,
        commands,
        false,
        &[],
        &expect![[r#"
            $ pumpkin run missing/task.star --runs runs --sandboxd sandboxd
            exit status: Some(1)
            pumpkin: the entry's directory missing can't be found (No such file or directory (os error 2)): name the entry by a path that exists
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --sandboxd <scratch>/sandboxd <scratch>/missing/task.star
            $ pumpkin run --runs runs --sandboxd sandboxd -- -missing.star
            exit status: Some(1)
            -missing.star: `-missing.star` doesn't exist: name a regular file below the root that does
            pumpkin: -missing.star: the task has 1 problem
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --sandboxd <scratch>/sandboxd <scratch>/-missing.star
            $ pumpkin run empty/task.star --runs runs --sandboxd sandboxd
            exit status: Some(1)
            empty/task.star: this file calls no `workflow()`: call it once, with the workflow
            pumpkin: empty/task.star: the task has 1 problem
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --sandboxd <scratch>/sandboxd <scratch>/empty/task.star
            $ pumpkin run workflow/task.star --runs runs --sandboxd missing
            exit status: Some(1)
            pumpkin: the sandboxd missing can't be read (No such file or directory (os error 2)): name a static one, which `cargo xtask sandboxd` builds
            $ pumpkin run workflow/task.star --runs runs --sandboxd private
            exit status: Some(1)
            pumpkin: the sandboxd private can't be executed by every user, as its mode is 0750: make it executable by every user (`chmod a+x`)
            $ pumpkin run workflow/task.star --runs runs --sandboxd script
            exit status: Some(1)
            pumpkin: the sandboxd script isn't a 64-bit ELF executable: name a static one, which `cargo xtask sandboxd` builds
            $ <scratch>/alone/pumpkin run workflow/task.star --runs runs
            exit status: Some(1)
            pumpkin: the sandboxd beside `pumpkin`, <scratch>/alone/sandboxd, can't be read (No such file or directory (os error 2)): `cargo xtask install` installs `pumpkin` beside a static one, or name one with `--sandboxd`
            $ <scratch>/checkout/pumpkin run workflow/task.star --runs runs
            exit status: Some(1)
            pumpkin: the sandboxd beside `pumpkin`, <scratch>/checkout/sandboxd, is linked dynamically, and a sandbox runs only a static one: `cargo xtask install` installs `pumpkin` beside a static one, or name one with `--sandboxd`
            $ env --unset=HOME pumpkin run workflow/task.star --sandboxd sandboxd
            exit status: Some(1)
            pumpkin: HOME is unset or empty: set it, or name a directory with `--runs`"#]],
    );
    assert!(!scratch.0.join("runs").exists(), "a refusal made a record");
}

/// `wait` finds the `outcome.json` of each run recorded in `scratch`'s `runs`, which have
/// ended, and with `sandboxd`, shown as `<sandboxd>`: the engine is not reached.
fn wait_for_each_run(scratch: &Scratch, sandboxd: &str) {
    let mut ids: Vec<String> = (fs::read_dir(scratch.0.join("runs")).unwrap())
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    ids.sort();
    let waits: Vec<Command> = (ids.iter())
        .map(|id| scratch.pumpkin(&["wait", id, "--runs", "runs"]))
        .collect();
    let mut shown: Vec<(&str, &str)> = ids.iter().map(|id| (&id[..], "<id>")).collect();
    shown.push((sandboxd, "<sandboxd>"));
    check(
        scratch,
        waits,
        false,
        &shown,
        &expect![[r#"
            $ pumpkin wait <id> --runs runs
            exit status: Some(3)
            pumpkin: run <id> finished without a score: the agent submitted nothing, and the work phase ended when the agent exited with status 0; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/conf/pumpkin.toml --sandboxd <sandboxd> <scratch>/asks/task.star
            printed outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            $ pumpkin wait <id> --runs runs
            exit status: Some(0)
            pumpkin: run <id> finished with a score of 3, after the work phase ended when the agent exited with status 0; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/conf/pumpkin.toml --sandboxd <sandboxd> <scratch>/submits/task.star
            printed outcome.json: {"score":3,"state":"finished","verdict":{"kind":"judged","result":{"returned":{"raw_score":3},"score":3},"submission":{"finalized":true,"payload":{"digest":"sha256:44778d82365e4af681c40d5f0eef5cf6f5899d3f0ac335050a7ed6779cf3f674","size":3}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            $ pumpkin wait <id> --runs runs
            exit status: Some(3)
            pumpkin: the final judge's standard error, <scratch>/runs/<id>/jobs/0/log/command.stderr, holds:
              scoring failed
              \u{1b}[1mbold\u{1b}[0m
            pumpkin: run <id> finished without a score: the final judge returned no result (invalid_result: the judge wrote no result: write it at /pumpkin/out/result.json), after the work phase ended when the agent exited with status 0; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/conf/pumpkin.toml --sandboxd <sandboxd> <scratch>/fails/task.star
            printed outcome.json: {"score":null,"state":"finished","verdict":{"failure":{"kind":"invalid_result","reason":"the judge wrote no result: write it at /pumpkin/out/result.json"},"kind":"no_result","submission":{"finalized":true,"payload":{"digest":"sha256:44778d82365e4af681c40d5f0eef5cf6f5899d3f0ac335050a7ed6779cf3f674","size":3}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}"#]],
    );
}

#[test]
fn modes() {
    modes_under(
        0o002,
        &expect![[r#"
            $ umask 002 && PATH=<scratch>/bin:/usr/bin:/bin pumpkin run lacking/task.star --sandboxd sandboxd --runs shared/runs --config pumpkin.toml
            exit status: Some(1)
            pumpkin: run <id> records into <scratch>/shared/runs/<id>
            pumpkin: run <id> failed: publishing role `agent`'s image on provider `local` failed: the engine of context `default` has no image sha256:1111111111111111111111111111111111111111111111111111111111111111: build or load it there first, as the provider pulls none; its record is <scratch>/shared/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/shared/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/lacking/task.star
            record: driver.log, driver.pid, ir.json, outcome.json, provider.json, record.json
            outcome.json: {"reason":"publishing role `agent`'s image on provider `local` failed: the engine of context `default` has no image sha256:1111111111111111111111111111111111111111111111111111111111111111: build or load it there first, as the provider pulls none","state":"failed"}
            printed outcome.json: true
            shared: 755
            shared/runs: 755
            the record: 755"#]],
    );
}

#[test]
fn modes_under_a_stricter_umask() {
    modes_under(
        0o077,
        &expect![[r#"
            $ umask 077 && PATH=<scratch>/bin:/usr/bin:/bin pumpkin run lacking/task.star --sandboxd sandboxd --runs shared/runs --config pumpkin.toml
            exit status: Some(1)
            pumpkin: run <id> records into <scratch>/shared/runs/<id>
            pumpkin: run <id> failed: publishing role `agent`'s image on provider `local` failed: the engine of context `default` has no image sha256:1111111111111111111111111111111111111111111111111111111111111111: build or load it there first, as the provider pulls none; its record is <scratch>/shared/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/shared/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/lacking/task.star
            record: driver.log, driver.pid, ir.json, outcome.json, provider.json, record.json
            outcome.json: {"reason":"publishing role `agent`'s image on provider `local` failed: the engine of context `default` has no image sha256:1111111111111111111111111111111111111111111111111111111111111111: build or load it there first, as the provider pulls none","state":"failed"}
            printed outcome.json: true
            shared: 700
            shared/runs: 700
            the record: 700"#]],
    );
}

/// Runs `pumpkin run` under `umask`, with a runs directory whose parent is missing too, and
/// checks the report, with the modes of the directories it made, against `expected`.
fn modes_under(umask: libc::mode_t, expected: &Expect) {
    let scratch = Scratch::new(&format!("modes-{umask:03o}"));
    let _stand_in = StandIn::new(&scratch);
    let mut run = scratch.pumpkin(&["run", "lacking/task.star", "--sandboxd", "sandboxd"]);
    run.args(["--runs", "shared/runs", "--config", "pumpkin.toml"]);
    let bin = scratch.0.join("bin");
    run.env("PATH", format!("{}:/usr/bin:/bin", bin.display()));
    let invoked = under_umask(&mut run, umask);
    let output = run.output().unwrap();
    let mut lines = vec![report(invoked, &output, &scratch.0, false)];
    let stderr = String::from_utf8(output.stderr).unwrap();
    let (_, record) = record(&stderr).unwrap();
    for (name, path) in [
        ("shared", scratch.0.join("shared")),
        ("shared/runs", scratch.0.join("shared/runs")),
        ("the record", PathBuf::from(record)),
    ] {
        let mode = fs::metadata(path).unwrap().permissions().mode() & 0o7777;
        lines.push(format!("{name}: {mode:o}"));
    }
    check(
        &scratch,
        [Step::Ran(lines.join("\n"))],
        false,
        &[],
        expected,
    );
}

#[test]
fn ancestors() {
    let scratch = Scratch::new("ancestors");
    let dirs = [
        // Not writable by its group, which may or may not be the user's private group.
        ("shared", 0o757),
        ("shared/sealed", 0o555),
        ("sticky", 0o1777),
        ("own", 0o755),
    ];
    for (dir, mode) in dirs {
        fs::create_dir(scratch.0.join(dir)).unwrap();
        fs::set_permissions(scratch.0.join(dir), Permissions::from_mode(mode)).unwrap();
    }
    symlink("own", scratch.0.join("link")).unwrap();
    let _stand_in = StandIn::new(&scratch);
    check(
        &scratch,
        [
            run_into(&scratch, "shared/runs"),
            run_into(&scratch, "shared/sealed"),
            run_into(&scratch, "sticky/runs"),
            run_into(&scratch, "link/runs"),
        ],
        false,
        &[],
        &expect![[r#"
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run lacking/task.star --sandboxd sandboxd --runs shared/runs --config pumpkin.toml
            exit status: Some(1)
            pumpkin: `<scratch>/shared` is writable by others: expected a directory that only its owner, or a group that holds only this process's user, may write, or a sticky one, as `chmod o-w <scratch>/shared` makes it
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run lacking/task.star --sandboxd sandboxd --runs shared/sealed --config pumpkin.toml
            exit status: Some(1)
            pumpkin: `<scratch>/shared` is writable by others: expected a directory that only its owner, or a group that holds only this process's user, may write, or a sticky one, as `chmod o-w <scratch>/shared` makes it
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run lacking/task.star --sandboxd sandboxd --runs sticky/runs --config pumpkin.toml
            exit status: Some(1)
            pumpkin: run <id> records into <scratch>/sticky/runs/<id>
            pumpkin: run <id> failed: publishing role `agent`'s image on provider `local` failed: the engine of context `default` has no image sha256:1111111111111111111111111111111111111111111111111111111111111111: build or load it there first, as the provider pulls none; its record is <scratch>/sticky/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/sticky/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/lacking/task.star
            record: driver.log, driver.pid, ir.json, outcome.json, provider.json, record.json
            outcome.json: {"reason":"publishing role `agent`'s image on provider `local` failed: the engine of context `default` has no image sha256:1111111111111111111111111111111111111111111111111111111111111111: build or load it there first, as the provider pulls none","state":"failed"}
            printed outcome.json: true
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run lacking/task.star --sandboxd sandboxd --runs link/runs --config pumpkin.toml
            exit status: Some(1)
            pumpkin: run <id> records into <scratch>/own/runs/<id>
            pumpkin: run <id> failed: publishing role `agent`'s image on provider `local` failed: the engine of context `default` has no image sha256:1111111111111111111111111111111111111111111111111111111111111111: build or load it there first, as the provider pulls none; its record is <scratch>/own/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/link/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/lacking/task.star
            record: driver.log, driver.pid, ir.json, outcome.json, provider.json, record.json
            outcome.json: {"reason":"publishing role `agent`'s image on provider `local` failed: the engine of context `default` has no image sha256:1111111111111111111111111111111111111111111111111111111111111111: build or load it there first, as the provider pulls none","state":"failed"}
            printed outcome.json: true"#]],
    );
    for runs in ["shared/runs", "shared/sealed"] {
        let records = fs::read_dir(scratch.0.join(runs)).unwrap().count();
        assert_eq!(records, 0, "a refusal left a record in {runs}");
    }
}

#[test]
fn ancestors_writable_by_their_group() {
    let scratch = Scratch::new("group");
    let private = scratch.0.join("private");
    fs::create_dir(&private).unwrap();
    fs::set_permissions(&private, Permissions::from_mode(0o775)).unwrap();
    let _stand_in = StandIn::new(&scratch);
    let expected = if is_private_group(&private) {
        expect![[r#"
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run lacking/task.star --sandboxd sandboxd --runs private/runs --config pumpkin.toml
            exit status: Some(1)
            pumpkin: run <id> records into <scratch>/private/runs/<id>
            pumpkin: run <id> failed: publishing role `agent`'s image on provider `local` failed: the engine of context `default` has no image sha256:1111111111111111111111111111111111111111111111111111111111111111: build or load it there first, as the provider pulls none; its record is <scratch>/private/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/private/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/lacking/task.star
            record: driver.log, driver.pid, ir.json, outcome.json, provider.json, record.json
            outcome.json: {"reason":"publishing role `agent`'s image on provider `local` failed: the engine of context `default` has no image sha256:1111111111111111111111111111111111111111111111111111111111111111: build or load it there first, as the provider pulls none","state":"failed"}
            printed outcome.json: true"#]]
    } else {
        expect![[r#"
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run lacking/task.star --sandboxd sandboxd --runs private/runs --config pumpkin.toml
            exit status: Some(1)
            pumpkin: `<scratch>/private` is writable by its group: expected a directory that only its owner, or a group that holds only this process's user, may write, or a sticky one, as `chmod g-w <scratch>/private` makes it"#]]
    };
    check(
        &scratch,
        [run_into(&scratch, "private/runs")],
        false,
        &[],
        &expected,
    );
}

/// `pumpkin run` in `scratch`, recording into `runs`, with the stand-in's `docker` first on the
/// `PATH`.
fn run_into(scratch: &Scratch, runs: &str) -> Command {
    let mut command = scratch.pumpkin(&["run", "lacking/task.star", "--sandboxd", "sandboxd"]);
    command.args(["--runs", runs, "--config", "pumpkin.toml"]);
    let bin = scratch.0.join("bin");
    command.env("PATH", format!("{}:/usr/bin:/bin", bin.display()));
    command
}

/// Whether the group of `dir` is the private group of its owner, the test's user, by
/// `/etc/passwd` and `/etc/group`: that user's primary group and no other user's, which
/// `/etc/group` lists with no member but that user.
fn is_private_group(dir: &Path) -> bool {
    // Field `n` of `entry`: its name first, its id third, and fourth a user's primary group, or
    // a group's members.
    fn field(entry: &[String], n: usize) -> Option<&str> {
        entry.get(n).map(String::as_str)
    }
    let entries = |file| -> Vec<Vec<String>> {
        let text = fs::read_to_string(file).unwrap();
        (text.lines())
            .map(|line| line.split(':').map(str::to_owned).collect())
            .collect()
    };
    let metadata = fs::metadata(dir).unwrap();
    let (uid, gid) = (metadata.uid().to_string(), metadata.gid().to_string());
    let (uid, gid) = (Some(uid.as_str()), Some(gid.as_str()));
    let (users, groups) = (entries("/etc/passwd"), entries("/etc/group"));
    let Some(user) = (users.iter()).find(|user| field(user, 2) == uid) else {
        return false;
    };
    let others = (users.iter()).any(|other| field(other, 2) != uid && field(other, 3) == gid);
    let members: Vec<&str> = (groups.iter())
        .filter(|group| field(group, 2) == gid)
        .map(|group| field(group, 3).unwrap_or(""))
        .collect();
    field(user, 3) == gid
        && !others
        && !members.is_empty()
        && (members.iter().flat_map(|members| members.split(',')))
            .all(|member| member.is_empty() || member == user[0])
}

/// Has `command` run under `umask` ([`through_shell`]), and returns its invocation, as
/// [`invocation`] shows it, after `umask` and the mask.
fn under_umask(command: &mut Command, umask: libc::mode_t) -> String {
    let invoked = format!("$ umask {umask:03o} && {}", &invocation(command)[2..]);
    *command = through_shell(command, &format!("umask {umask:03o}"));
    invoked
}

/// Each mode that the directories below `dir`, and the files, have, in octal.
fn modes_below(dir: &Path) -> String {
    let (mut dirs, mut files) = (BTreeSet::new(), BTreeSet::new());
    let mut pending = vec![dir.to_owned()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            let mode = format!("{:o}", metadata.permissions().mode() & 0o7777);
            if metadata.is_dir() {
                dirs.insert(mode);
                pending.push(path);
            } else {
                files.insert(mode);
            }
        }
    }
    let join = |modes: BTreeSet<String>| modes.into_iter().collect::<Vec<_>>().join(", ");
    format!("directories {}; files {}", join(dirs), join(files))
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn refusals() {
    let scratch = Scratch::new("refusals");
    let mut ir: Value = serde_json::from_str(WORKFLOW).unwrap();
    scratch.write_task("workflow", &ir);
    ir["roles"][1]["name"] = json!("agent");
    ir["roles"][1]["grants"] = json!([{ "submit": { "name": "resubmit" } }]);
    // In directories named so that the command that runs it again quotes it, for its `'`, or
    // `~` alone, each the root of its tasks.
    scratch.write_task_in("it's broken", "tasks", &ir);
    scratch.write_task_in("a~b", "tasks", &ir);
    write_configurations(&scratch);
    let stand_in = StandIn::new(&scratch);
    let mut large = workflow(&format!("sha256:{}", "0".repeat(64)), &["true"]);
    large["roles"][1]["needs"]["cpu_millis"] = json!(i64::MAX);
    large["roles"][2]["needs"]["gpu"] = json!({ "count": 1, "model": "NVIDIA H100 80GB HBM3" });
    scratch.write_task("large", &large);
    let mut wide = workflow(&format!("sha256:{}", "0".repeat(64)), &["true"]);
    let cpus = machine_cpus();
    wide["roles"][0]["needs"]["cpu_millis"] = json!(cpus * 1000);
    wide["roles"][1]["needs"]["cpu_millis"] = json!(1000);
    scratch.write_task("wide", &wide);
    scratch.write("plain.toml", &machine("default"));
    // Two CPUs given, of which the agent's take both.
    wide["roles"][0]["needs"]["cpu_millis"] = json!(2000);
    scratch.write_task("narrow", &wide);
    let given =
        |cpus: &str| format!("[docker]\ncontext = \"default\"\ncpus = \"{cpus}\"\n\n{LOCAL}");
    scratch.write("offline.toml", &given("0,4096-4097"));
    scratch.write("narrow.toml", &given("0-1"));
    scratch.write("backward.toml", &given("3-1"));
    scratch.write("empty.toml", &given(""));
    let run = |entry, config: &str| {
        let mut command = scratch.pumpkin(&["run", entry, "--sandboxd", "sandboxd"]);
        command.args(["--runs", "runs", "--config", config]);
        command
    };
    let on_stand_in = |entry, config: &str| {
        let mut command = run(entry, config);
        let bin = scratch.0.join("bin");
        command.env("PATH", format!("{}:/usr/bin:/bin", bin.display()));
        command
    };
    // Paths that need quoting for a space alone, and for a newline, which makes the command
    // span two lines.
    let mut quoted = scratch.pumpkin(&["run", "a~b/tasks/task.star", "--sandboxd", "sandboxd"]);
    quoted.args(["--runs", "two\nlines", "--config", "my served.toml"]);
    let steps = [
        Step::from(run("it's broken/tasks/task.star", "conf/served.toml")),
        Step::from(quoted),
        Step::from(run("workflow/task.star", "unserved.toml")),
        Step::from(stand_in.run("large/task.star", &[])),
        Step::from(on_stand_in("wide/task.star", "plain.toml")),
        Step::from(on_stand_in("wide/task.star", "backward.toml")),
        Step::from(on_stand_in("wide/task.star", "empty.toml")),
        Step::from(on_stand_in("wide/task.star", "offline.toml")),
        Step::from(on_stand_in("narrow/task.star", "narrow.toml")),
    ];
    let cores = machine_cores();
    let shown = [
        (format!("offers {cpus}000 "), "offers <cpus>000 ".to_owned()),
        (format!("`agent` {cpus},"), "`agent` <cpus>,".to_owned()),
        (format!("the {cores} that"), "the <cores> that".to_owned()),
        (
            format!("CPUs of 0-{}", cpus - 1),
            "CPUs of 0-<cpus - 1>".to_owned(),
        ),
    ];
    let shown: Vec<(&str, &str)> = (shown.iter())
        .map(|(from, to)| (from.as_str(), to.as_str()))
        .collect();
    check(
        &scratch,
        steps,
        false,
        &shown,
        &expect![[r#"
            $ pumpkin run it's broken/tasks/task.star --sandboxd sandboxd --runs runs --config conf/served.toml
            exit status: Some(1)
            tasks/task.star:5:881: `agent` also names /roles/0: give each a unique name
                5 |     roles = [{"egress": {"model": {"models": ["gpt-6-astra"]}}, "grants": [{"judge": {"call_timeout_seconds": 12000, "calls": 100, "cumulative_seconds": 12000, "name": "train", "role": "train_judge", "slot": "payload"}}, {"submit": {"name": "submit"}}], "image": IMAGE, "name": "agent", "needs": {"cpu_millis": 8000, "memory_bytes": 34359738368}, "runtime": {"agent": {"command": {"argv": ["/usr/local/bin/pumpkin-fake-agent"], "cwd": "/workspace"}, "deadline_seconds": 18000, "dirs": [{"mode": "0700", "path": "/workspace/.codex"}, {"mode": "0700", "path": "/workspace/.tmp"}], "files": [{"content": "model = \"gpt-6-astra\"\n", "mode": "0600", "path": "/workspace/.codex/config.toml"}], "finalize_grace_seconds": 60, "stdin": "Solve the task in /task/README.md.\n"}}, "user": {"gid": 10001, "uid": 10001}}, {"grants": [{"submit": {"name": "resubmit"}}], "image": IMAGE, "name": "agent", "needs": {"cpu_millis": 8000, "memory_bytes": 2147483648}, "runtime": {"judge": {"command": {"argv": ["/usr/local/bin/judge"], "cwd": "/pumpkin/home", "env": ["MODE=train", "DATASET=/data/train.json"]}, "result": {"returns": ["/raw_score", "/metrics", "/failure"], "score": "/raw_score"}, "slots": [{"max_bytes": 4194304, "name": "payload"}]}}, "user": {"gid": 65532, "uid": 65532}}, {"image": IMAGE, "name": "final_judge", "needs": {"cpu_millis": 8000, "memory_bytes": 2147483648}, "runtime": {"judge": {"command": {"argv": ["/usr/local/bin/judge"], "cwd": "/pumpkin/home", "env": ["MODE=eval", "DATASET=/data/eval.json"]}, "result": {"returns": ["/raw_score", "/metrics", "/failure"], "score": "/raw_score"}, "slots": [{"max_bytes": 4194304, "name": "payload"}]}}, "user": {"gid": 65532, "uid": 65532}}],
                  |                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 ^^^^^^^
                the IR's /roles/1/name
            tasks/task.star:5:821: `agent` is a judge, and judges hold no grants yet: remove them
                5 |     roles = [{"egress": {"model": {"models": ["gpt-6-astra"]}}, "grants": [{"judge": {"call_timeout_seconds": 12000, "calls": 100, "cumulative_seconds": 12000, "name": "train", "role": "train_judge", "slot": "payload"}}, {"submit": {"name": "submit"}}], "image": IMAGE, "name": "agent", "needs": {"cpu_millis": 8000, "memory_bytes": 34359738368}, "runtime": {"agent": {"command": {"argv": ["/usr/local/bin/pumpkin-fake-agent"], "cwd": "/workspace"}, "deadline_seconds": 18000, "dirs": [{"mode": "0700", "path": "/workspace/.codex"}, {"mode": "0700", "path": "/workspace/.tmp"}], "files": [{"content": "model = \"gpt-6-astra\"\n", "mode": "0600", "path": "/workspace/.codex/config.toml"}], "finalize_grace_seconds": 60, "stdin": "Solve the task in /task/README.md.\n"}}, "user": {"gid": 10001, "uid": 10001}}, {"grants": [{"submit": {"name": "resubmit"}}], "image": IMAGE, "name": "agent", "needs": {"cpu_millis": 8000, "memory_bytes": 2147483648}, "runtime": {"judge": {"command": {"argv": ["/usr/local/bin/judge"], "cwd": "/pumpkin/home", "env": ["MODE=train", "DATASET=/data/train.json"]}, "result": {"returns": ["/raw_score", "/metrics", "/failure"], "score": "/raw_score"}, "slots": [{"max_bytes": 4194304, "name": "payload"}]}}, "user": {"gid": 65532, "uid": 65532}}, {"image": IMAGE, "name": "final_judge", "needs": {"cpu_millis": 8000, "memory_bytes": 2147483648}, "runtime": {"judge": {"command": {"argv": ["/usr/local/bin/judge"], "cwd": "/pumpkin/home", "env": ["MODE=eval", "DATASET=/data/eval.json"]}, "result": {"returns": ["/raw_score", "/metrics", "/failure"], "score": "/raw_score"}, "slots": [{"max_bytes": 4194304, "name": "payload"}]}}, "user": {"gid": 65532, "uid": 65532}}],
                  |                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                the IR's /roles/1/grants
            tasks/task.star:5:186: no role is named `train_judge`: name one listed in `roles`
                5 |     roles = [{"egress": {"model": {"models": ["gpt-6-astra"]}}, "grants": [{"judge": {"call_timeout_seconds": 12000, "calls": 100, "cumulative_seconds": 12000, "name": "train", "role": "train_judge", "slot": "payload"}}, {"submit": {"name": "submit"}}], "image": IMAGE, "name": "agent", "needs": {"cpu_millis": 8000, "memory_bytes": 34359738368}, "runtime": {"agent": {"command": {"argv": ["/usr/local/bin/pumpkin-fake-agent"], "cwd": "/workspace"}, "deadline_seconds": 18000, "dirs": [{"mode": "0700", "path": "/workspace/.codex"}, {"mode": "0700", "path": "/workspace/.tmp"}], "files": [{"content": "model = \"gpt-6-astra\"\n", "mode": "0600", "path": "/workspace/.codex/config.toml"}], "finalize_grace_seconds": 60, "stdin": "Solve the task in /task/README.md.\n"}}, "user": {"gid": 10001, "uid": 10001}}, {"grants": [{"submit": {"name": "resubmit"}}], "image": IMAGE, "name": "agent", "needs": {"cpu_millis": 8000, "memory_bytes": 2147483648}, "runtime": {"judge": {"command": {"argv": ["/usr/local/bin/judge"], "cwd": "/pumpkin/home", "env": ["MODE=train", "DATASET=/data/train.json"]}, "result": {"returns": ["/raw_score", "/metrics", "/failure"], "score": "/raw_score"}, "slots": [{"max_bytes": 4194304, "name": "payload"}]}}, "user": {"gid": 65532, "uid": 65532}}, {"image": IMAGE, "name": "final_judge", "needs": {"cpu_millis": 8000, "memory_bytes": 2147483648}, "runtime": {"judge": {"command": {"argv": ["/usr/local/bin/judge"], "cwd": "/pumpkin/home", "env": ["MODE=eval", "DATASET=/data/eval.json"]}, "result": {"returns": ["/raw_score", "/metrics", "/failure"], "score": "/raw_score"}, "slots": [{"max_bytes": 4194304, "name": "payload"}]}}, "user": {"gid": 65532, "uid": 65532}}],
                  |                                                                                                                                                                                          ^^^^^^^^^^^^^
                the IR's /roles/0/grants/0/judge/role
            pumpkin: it's broken/tasks/task.star: the task has 3 problems
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/conf/served.toml --sandboxd <scratch>/sandboxd '<scratch>/it'\''s broken/tasks/task.star'
            $ pumpkin run a~b/tasks/task.star --sandboxd sandboxd --runs two
            lines --config my served.toml
            exit status: Some(1)
            tasks/task.star:5:881: `agent` also names /roles/0: give each a unique name
                5 |     roles = [{"egress": {"model": {"models": ["gpt-6-astra"]}}, "grants": [{"judge": {"call_timeout_seconds": 12000, "calls": 100, "cumulative_seconds": 12000, "name": "train", "role": "train_judge", "slot": "payload"}}, {"submit": {"name": "submit"}}], "image": IMAGE, "name": "agent", "needs": {"cpu_millis": 8000, "memory_bytes": 34359738368}, "runtime": {"agent": {"command": {"argv": ["/usr/local/bin/pumpkin-fake-agent"], "cwd": "/workspace"}, "deadline_seconds": 18000, "dirs": [{"mode": "0700", "path": "/workspace/.codex"}, {"mode": "0700", "path": "/workspace/.tmp"}], "files": [{"content": "model = \"gpt-6-astra\"\n", "mode": "0600", "path": "/workspace/.codex/config.toml"}], "finalize_grace_seconds": 60, "stdin": "Solve the task in /task/README.md.\n"}}, "user": {"gid": 10001, "uid": 10001}}, {"grants": [{"submit": {"name": "resubmit"}}], "image": IMAGE, "name": "agent", "needs": {"cpu_millis": 8000, "memory_bytes": 2147483648}, "runtime": {"judge": {"command": {"argv": ["/usr/local/bin/judge"], "cwd": "/pumpkin/home", "env": ["MODE=train", "DATASET=/data/train.json"]}, "result": {"returns": ["/raw_score", "/metrics", "/failure"], "score": "/raw_score"}, "slots": [{"max_bytes": 4194304, "name": "payload"}]}}, "user": {"gid": 65532, "uid": 65532}}, {"image": IMAGE, "name": "final_judge", "needs": {"cpu_millis": 8000, "memory_bytes": 2147483648}, "runtime": {"judge": {"command": {"argv": ["/usr/local/bin/judge"], "cwd": "/pumpkin/home", "env": ["MODE=eval", "DATASET=/data/eval.json"]}, "result": {"returns": ["/raw_score", "/metrics", "/failure"], "score": "/raw_score"}, "slots": [{"max_bytes": 4194304, "name": "payload"}]}}, "user": {"gid": 65532, "uid": 65532}}],
                  |                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 ^^^^^^^
                the IR's /roles/1/name
            tasks/task.star:5:821: `agent` is a judge, and judges hold no grants yet: remove them
                5 |     roles = [{"egress": {"model": {"models": ["gpt-6-astra"]}}, "grants": [{"judge": {"call_timeout_seconds": 12000, "calls": 100, "cumulative_seconds": 12000, "name": "train", "role": "train_judge", "slot": "payload"}}, {"submit": {"name": "submit"}}], "image": IMAGE, "name": "agent", "needs": {"cpu_millis": 8000, "memory_bytes": 34359738368}, "runtime": {"agent": {"command": {"argv": ["/usr/local/bin/pumpkin-fake-agent"], "cwd": "/workspace"}, "deadline_seconds": 18000, "dirs": [{"mode": "0700", "path": "/workspace/.codex"}, {"mode": "0700", "path": "/workspace/.tmp"}], "files": [{"content": "model = \"gpt-6-astra\"\n", "mode": "0600", "path": "/workspace/.codex/config.toml"}], "finalize_grace_seconds": 60, "stdin": "Solve the task in /task/README.md.\n"}}, "user": {"gid": 10001, "uid": 10001}}, {"grants": [{"submit": {"name": "resubmit"}}], "image": IMAGE, "name": "agent", "needs": {"cpu_millis": 8000, "memory_bytes": 2147483648}, "runtime": {"judge": {"command": {"argv": ["/usr/local/bin/judge"], "cwd": "/pumpkin/home", "env": ["MODE=train", "DATASET=/data/train.json"]}, "result": {"returns": ["/raw_score", "/metrics", "/failure"], "score": "/raw_score"}, "slots": [{"max_bytes": 4194304, "name": "payload"}]}}, "user": {"gid": 65532, "uid": 65532}}, {"image": IMAGE, "name": "final_judge", "needs": {"cpu_millis": 8000, "memory_bytes": 2147483648}, "runtime": {"judge": {"command": {"argv": ["/usr/local/bin/judge"], "cwd": "/pumpkin/home", "env": ["MODE=eval", "DATASET=/data/eval.json"]}, "result": {"returns": ["/raw_score", "/metrics", "/failure"], "score": "/raw_score"}, "slots": [{"max_bytes": 4194304, "name": "payload"}]}}, "user": {"gid": 65532, "uid": 65532}}],
                  |                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                the IR's /roles/1/grants
            tasks/task.star:5:186: no role is named `train_judge`: name one listed in `roles`
                5 |     roles = [{"egress": {"model": {"models": ["gpt-6-astra"]}}, "grants": [{"judge": {"call_timeout_seconds": 12000, "calls": 100, "cumulative_seconds": 12000, "name": "train", "role": "train_judge", "slot": "payload"}}, {"submit": {"name": "submit"}}], "image": IMAGE, "name": "agent", "needs": {"cpu_millis": 8000, "memory_bytes": 34359738368}, "runtime": {"agent": {"command": {"argv": ["/usr/local/bin/pumpkin-fake-agent"], "cwd": "/workspace"}, "deadline_seconds": 18000, "dirs": [{"mode": "0700", "path": "/workspace/.codex"}, {"mode": "0700", "path": "/workspace/.tmp"}], "files": [{"content": "model = \"gpt-6-astra\"\n", "mode": "0600", "path": "/workspace/.codex/config.toml"}], "finalize_grace_seconds": 60, "stdin": "Solve the task in /task/README.md.\n"}}, "user": {"gid": 10001, "uid": 10001}}, {"grants": [{"submit": {"name": "resubmit"}}], "image": IMAGE, "name": "agent", "needs": {"cpu_millis": 8000, "memory_bytes": 2147483648}, "runtime": {"judge": {"command": {"argv": ["/usr/local/bin/judge"], "cwd": "/pumpkin/home", "env": ["MODE=train", "DATASET=/data/train.json"]}, "result": {"returns": ["/raw_score", "/metrics", "/failure"], "score": "/raw_score"}, "slots": [{"max_bytes": 4194304, "name": "payload"}]}}, "user": {"gid": 65532, "uid": 65532}}, {"image": IMAGE, "name": "final_judge", "needs": {"cpu_millis": 8000, "memory_bytes": 2147483648}, "runtime": {"judge": {"command": {"argv": ["/usr/local/bin/judge"], "cwd": "/pumpkin/home", "env": ["MODE=eval", "DATASET=/data/eval.json"]}, "result": {"returns": ["/raw_score", "/metrics", "/failure"], "score": "/raw_score"}, "slots": [{"max_bytes": 4194304, "name": "payload"}]}}, "user": {"gid": 65532, "uid": 65532}}],
                  |                                                                                                                                                                                          ^^^^^^^^^^^^^
                the IR's /roles/0/grants/0/judge/role
            pumpkin: a~b/tasks/task.star: the task has 3 problems
            pumpkin: to run it again: pumpkin run --runs '<scratch>/two
            lines' --config '<scratch>/my served.toml' --sandboxd <scratch>/sandboxd '<scratch>/a~b/tasks/task.star'
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config unserved.toml
            exit status: Some(1)
            pumpkin: unserved.toml: no upstream serves every model that the workflow's route allows, `gpt-6-astra`: list them all under one
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/unserved.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run large/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
            exit status: Some(1)
            pumpkin: large/task.star: /roles/1/needs: `train_judge` fits no class of sandbox, as `local`'s `container` offers <cpus>000 thousandths of a CPU, fewer than its 9223372036854775807: offer a class that meets its needs, or change its needs
            pumpkin: large/task.star: /roles/2/needs: `final_judge` fits no class of sandbox, as `local`'s `container` has no GPU, and it needs 1 × `NVIDIA H100 80GB HBM3`: offer a class that meets its needs, or change its needs
            pumpkin: large/task.star: placing the workflow's roles in the classes that pumpkin.toml's providers offer has 2 problems
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <scratch>/sandboxd <scratch>/large/task.star
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run wide/task.star --sandboxd sandboxd --runs runs --config plain.toml
            exit status: Some(1)
            pumpkin: reserving on provider `local`: the run's roles need more whole cores at once than the <cores> that this machine gives sandboxes, to hold their CPUs (`agent` <cpus>, `train_judge` 1, `final_judge` 1): give them fewer CPUs
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run wide/task.star --sandboxd sandboxd --runs runs --config backward.toml
            exit status: Some(1)
            pumpkin: backward.toml: TOML parse error at line 3, column 8
              |
            3 | cpus = "3-1"
              |        ^^^^^
            `3-1` is no CPU list: expected one such as `0-3,8`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/backward.toml --sandboxd <scratch>/sandboxd <scratch>/wide/task.star
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run wide/task.star --sandboxd sandboxd --runs runs --config empty.toml
            exit status: Some(1)
            pumpkin: empty.toml: TOML parse error at line 3, column 8
              |
            3 | cpus = ""
              |        ^^
            the CPU list is empty: name one CPU at least, such as `0`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/empty.toml --sandboxd <scratch>/sandboxd <scratch>/wide/task.star
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run wide/task.star --sandboxd sandboxd --runs runs --config offline.toml
            exit status: Some(1)
            pumpkin: reaching provider `local` failed: the CPUs to give sandboxes, 0,4096-4097, name 4096-4097, which aren't online: name only CPUs of 0-<cpus - 1>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/offline.toml --sandboxd <scratch>/sandboxd <scratch>/wide/task.star
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run narrow/task.star --sandboxd sandboxd --runs runs --config narrow.toml
            exit status: Some(1)
            pumpkin: reserving on provider `local`: the run's roles need more whole cores at once than the 2 that this machine gives sandboxes, to hold their CPUs (`agent` 2, `train_judge` 1, `final_judge` 1): give them fewer CPUs"#]],
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
    let mut ir: Value = serde_json::from_str(WORKFLOW).unwrap();
    scratch.write_task("workflow", &ir);
    ir["roles"][0].as_object_mut().unwrap().remove("egress");
    scratch.write_task("unrouted", &ir);
    write_configurations(&scratch);
    let run = |entry, config: &[&str]| {
        let mut command = scratch.pumpkin(&["run", entry, "--sandboxd", "sandboxd"]);
        command.args(["--runs", "runs"]).args(config);
        command
    };
    let config = |name: &str| run("workflow/task.star", &["--config", &format!("{name}.toml")]);
    let mut home = run("workflow/task.star", &[]);
    home.env("HOME", scratch.0.join("home"));
    let mut homeless = run("workflow/task.star", &[]);
    homeless.env_remove("HOME");
    let mut served = config("conf/served");
    let mut unrouted = run("unrouted/task.star", &["--config", "docked.toml"]);
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
        "placed",
        "podman",
        "context",
        "valued",
        "second",
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
            $ HOME=<scratch>/home pumpkin run workflow/task.star --sandboxd sandboxd --runs runs
            exit status: Some(1)
            pumpkin: the configuration <scratch>/home/.config/pumpkin/pumpkin.toml can't be read (No such file or directory (os error 2)): write one there, as `pumpkin help config` says, or name another file with `--config`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ env --unset=HOME pumpkin run workflow/task.star --sandboxd sandboxd --runs runs
            exit status: Some(1)
            pumpkin: HOME is unset or empty: set it, or name the configuration with `--config`
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config missing.toml
            exit status: Some(1)
            pumpkin: the configuration missing.toml can't be read (No such file or directory (os error 2)): write one there, as `pumpkin help config` says, or name another file with `--config`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/missing.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config undocked.toml
            exit status: Some(1)
            pumpkin: undocked.toml: `[docker]` is missing: name it, as `pumpkin help config` says
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/undocked.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config nameless.toml
            exit status: Some(1)
            pumpkin: nameless.toml: TOML parse error at line 2, column 11
              |
            2 | context = ""
              |           ^^
            the Docker context's name is empty: name one that `docker context ls` lists
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/nameless.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config broken.toml
            exit status: Some(1)
            pumpkin: broken.toml: TOML parse error at line 4, column 19
              |
            4 | [upstreams.gateway
              |                   ^
            unclosed table, expected `]`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/broken.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config inline.toml
            exit status: Some(1)
            pumpkin: inline.toml: TOML parse error at line 6, column 1
              |
            6 | key = "sk-1"
              | ^^^
            unknown field `key`, expected one of `url`, `key_file`, `models`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/inline.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config priced.toml
            exit status: Some(1)
            pumpkin: priced.toml: TOML parse error at line 7, column 73
              |
            7 | models = [{ name = "gpt-6-astra", upstream_name = "vendor/gpt-6-astra", price = 1 }]
              |                                                                         ^^^^^
            unknown field `price`, expected `name` or `upstream_name`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/priced.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config keyless.toml
            exit status: Some(1)
            pumpkin: keyless.toml: TOML parse error at line 4, column 1
              |
            4 | [upstreams.gateway]
              | ^^^^^^^^^^^^^^^^^^^
            missing field `key_file`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/keyless.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config invalid.toml
            exit status: Some(1)
            pumpkin: invalid.toml: TOML parse error at line 4, column 12
              |
            4 | [upstreams.Gateway]
              |            ^^^^^^^
            `Gateway` is not a valid name: use 1 to 64 characters from a-z, 0-9, `_` and `-`, starting with a letter
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/invalid.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config twice.toml
            exit status: Some(1)
            pumpkin: twice.toml: TOML parse error at line 9, column 12
              |
            9 | [upstreams.gateway]
              |            ^^^^^^^
            duplicate key
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/twice.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config again.toml
            exit status: Some(1)
            pumpkin: again.toml: `gpt-6-astra` is listed more than once: list each model once, under the upstream that serves it
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/again.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config query.toml
            exit status: Some(1)
            pumpkin: query.toml: TOML parse error at line 5, column 7
              |
            5 | url = "https://models.example.com/v1?project=1"
              |       ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
            the upstream's URL has a query or a fragment: name the API's base URL alone, below which the route reaches `/responses`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/query.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config fragment.toml
            exit status: Some(1)
            pumpkin: fragment.toml: TOML parse error at line 5, column 7
              |
            5 | url = "https://models.example.com/v1#v2"
              |       ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
            the upstream's URL has a query or a fragment: name the API's base URL alone, below which the route reaches `/responses`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/fragment.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config unserved.toml
            exit status: Some(1)
            pumpkin: unserved.toml: no upstream serves every model that the workflow's route allows, `gpt-6-astra`: list them all under one
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/unserved.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config placed.toml
            exit status: Some(1)
            pumpkin: placed.toml: TOML parse error at line 11, column 2
               |
            11 | [placement.agent]
               |  ^^^^^^^^^
            unknown field `placement`, expected one of `docker`, `providers`, `upstreams`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/placed.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config podman.toml
            exit status: Some(1)
            pumpkin: podman.toml: TOML parse error at line 9, column 18
              |
            9 | [providers.local.podman]
              |                  ^^^^^^
            unknown variant `podman`, expected `docker`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/podman.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config context.toml
            exit status: Some(1)
            pumpkin: context.toml: TOML parse error at line 10, column 1
               |
            10 | context = "remote"
               | ^^^^^^^
            unknown field `context`, there are no fields
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/context.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config valued.toml
            exit status: Some(1)
            pumpkin: valued.toml: TOML parse error at line 10, column 10
               |
            10 | docker = true
               |          ^^^^
            invalid type: boolean `true`, expected an empty table, as the provider's kind takes no settings
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/valued.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config second.toml
            exit status: Some(1)
            pumpkin: second.toml: providers `local` and `remote` are each of kind `docker`, so each would be the machine's one Docker engine, `[docker]`'s: keep one of them
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/second.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config docked.toml
            exit status: Some(1)
            pumpkin: docked.toml: no upstream serves every model that the workflow's route allows, `gpt-6-astra`: add an `[upstreams.<name>]` that lists them
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/docked.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config conf/unkeyed.toml
            exit status: Some(1)
            pumpkin: conf/unkeyed.toml: the key file conf/missing can't be read (No such file or directory (os error 2)): name a file that holds the model provider's key
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/conf/unkeyed.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ PATH=<scratch>/nowhere pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config conf/served.toml
            exit status: Some(1)
            pumpkin: the Docker context `nowhere` can't be read: `docker` didn't run: No such file or directory (os error 2): install Docker's command line on `PATH`: name one that `docker context ls` lists
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/conf/served.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ PATH=<scratch>/nowhere pumpkin run unrouted/task.star --sandboxd sandboxd --runs runs --config docked.toml
            exit status: Some(1)
            pumpkin: the Docker context `nowhere` can't be read: `docker` didn't run: No such file or directory (os error 2): install Docker's command line on `PATH`: name one that `docker context ls` lists
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/docked.toml --sandboxd <scratch>/sandboxd <scratch>/unrouted/task.star"#]],
    );
}

/// Writes into `scratch` the stand-in `sandboxd` that [`configurations`] and [`refusals`] give
/// `run`, and the machine's configurations, each named for what it holds: `conf/served`, of the
/// context `nowhere` and two upstreams, its key in `conf/key`; `docked`, without upstreams; and,
/// to be refused, one for each rule a configuration breaks, as its name says (a missing context,
/// TOML errors, keys, upstreams, URLs, unserved models, `[placement]`, provider kinds and their
/// settings); and `conf/unkeyed`, whose key file is missing. Each but `undocked` ends with
/// [`LOCAL`]'s tables, changed as its name says.
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
        ("placed", gateway.clone()),
        ("podman", gateway.clone()),
        ("context", gateway.clone()),
        ("valued", gateway.clone()),
        ("second", gateway.clone()),
        ("conf/unkeyed", gateway.replace("\"key\"", "\"missing\"")),
    ];
    for (name, config) in configs {
        let docker = match name {
            "nameless" => docker_table(""),
            _ => docker_table("nowhere"),
        };
        let placed = match name {
            "placed" => {
                format!("{LOCAL}\n[placement.agent]\nprovider = \"local\"\nclass = \"container\"\n")
            }
            "podman" => LOCAL.replace(".docker]", ".podman]"),
            "context" => LOCAL.replace(".docker]\n", ".docker]\ncontext = \"remote\"\n"),
            "valued" => LOCAL.replace("local.docker]\n", "local]\ndocker = true\n"),
            "second" => format!("[providers.remote.docker]\n\n{LOCAL}"),
            _ => LOCAL.to_owned(),
        };
        let config = format!("{docker}\n{config}\n{placed}");
        scratch.write(&format!("{name}.toml"), &config);
    }
    scratch.write("undocked.toml", &gateway);
}

/// Writes into `scratch` the task `asks`, [`workflow`]'s with the fixture's model route,
/// whose agent asks its model and keeps the answer in its `out`; and the configuration
/// `conf/pumpkin.toml`, of the Docker context `context`, which places the roles as [`LOCAL`]
/// does, and whose one upstream, the stand-in at `address`, serves the route's model as
/// `vendor/gpt-6-astra`, with its key in `conf/key`.
fn write_asks(scratch: &Scratch, context: &str, address: &str) {
    let config = format!(
        r#"{}
[upstreams.provider]
url = "http://{address}/v1/"
key_file = "key"
models = [{{ name = "gpt-6-astra", upstream_name = "vendor/gpt-6-astra" }}]
"#,
        machine(context)
    );
    scratch.write("conf/pumpkin.toml", &config);
    fs::write(scratch.0.join("conf/key"), "sk-test\n").unwrap();
    let body = r#"{"model": "gpt-6-astra", "store": false, "input": "hi"}"#;
    let ask = format!(
        "wget --quiet --output-document=/pumpkin/out/answer --header 'content-type: application/json' \
         --post-data '{body}' http://127.0.0.1:{MODEL_PORT}/responses"
    );
    let mut asks = workflow("", &["sh", "-c", &ask]);
    let fixture: Value = serde_json::from_str(WORKFLOW).unwrap();
    asks["roles"][0]["egress"] = fixture["roles"][0]["egress"].clone();
    scratch.write_task("asks", &asks);
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
    let sandboxd = sandboxd().into_os_string().into_string().unwrap();
    let context = docker_context();
    let engine = engine(&context);
    let upstream = Upstream::start();
    write_asks(&scratch, &context, &upstream.address);
    scratch.write("home/.config/pumpkin/pumpkin.toml", &machine(&context));
    scratch.write_task("exits", &workflow("", &["true"]));
    let submit = format!("printf six > six && {SANDBOXCTL_PATH} submit submit six --finalize");
    scratch.write_task("submits", &workflow("", &["sh", "-c", &submit]));
    let mut fails = workflow("", &["sh", "-c", &submit]);
    let fail = r"printf 'scoring failed\n\033[1mbold\033[0m\n' >&2; exit 1";
    fails["roles"][2]["runtime"]["judge"]["command"]["argv"] = json!(["sh", "-c", fail]);
    scratch.write_task("fails", &fails);
    for task in ["asks", "exits", "submits", "fails"] {
        build_first(&scratch, task, "conf/pumpkin.toml");
    }
    let run = |entry, rest: &[&str]| {
        let mut command = scratch.pumpkin(&["run", entry, "--sandboxd", &sandboxd, "--wait"]);
        command.args(rest);
        command
    };
    // `docker` finds its contexts below `HOME` unless `DOCKER_CONFIG` says where.
    let home = PathBuf::from(env::var_os("HOME").unwrap());
    let docker_config = env::var_os("DOCKER_CONFIG").map_or(home.join(".docker"), PathBuf::from);
    let docker_config = docker_config.into_os_string().into_string().unwrap();
    let mut exits = run("exits/task.star", &[]);
    (exits.env("HOME", scratch.0.join("home"))).env("DOCKER_CONFIG", &docker_config);
    let config = ["--runs", "runs", "--config", "conf/pumpkin.toml"];
    let commands = [
        run("asks/task.star", &config),
        exits,
        run("submits/task.star", &config),
        run("fails/task.star", &config),
    ];
    let shown = [
        (&sandboxd[..], "<sandboxd>"),
        (&docker_config[..], "<docker config>"),
        (&upstream.address[..], "<stand-in>"),
    ];
    let rootful = !engine.report().security.rootless;
    check(
        &scratch,
        commands.map(Step::OnEngine),
        rootful,
        &shown,
        &expect![[r#"
            $ pumpkin run asks/task.star --sandboxd <sandboxd> --wait --runs runs --config conf/pumpkin.toml
            exit status: Some(3)
            pumpkin: run <id> records into <scratch>/runs/<id>
            pumpkin: run <id> started
            pumpkin: following run <id>, recorded in <scratch>/runs/<id>: ending `pumpkin run` does not stop it
            pumpkin: run <id> finished without a score: the agent submitted nothing, and the work phase ended when the agent exited with status 0; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/conf/pumpkin.toml --sandboxd <sandboxd> <scratch>/asks/task.star
            record: agent/console.txt, agent/log/command.stderr, agent/log/command.stdout, agent/out/answer, agent/sandbox.json, driver.log, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local","report":null}
            the run log notes: the agent's model route goes to the upstream `provider`, which takes `gpt-6-astra` as `vendor/gpt-6-astra`
            the run log records: {"answer":{"kind":"forwarded"},"kind":"model_answered","request":0,"upstream_status":200}
            the agent's answer: data: POST /v1/responses HTTP/1.1 Some("Bearer sk-test") {"input":"hi","model":"vendor/gpt-6-astra","store":false}\n\n
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            printed outcome.json: true
            $ DOCKER_CONFIG=<docker config> HOME=<scratch>/home pumpkin run exits/task.star --sandboxd <sandboxd> --wait
            exit status: Some(3)
            pumpkin: run <id> records into <scratch>/home/.local/share/pumpkin/runs/<id>
            pumpkin: run <id> started
            pumpkin: following run <id>, recorded in <scratch>/home/.local/share/pumpkin/runs/<id>: ending `pumpkin run` does not stop it
            pumpkin: run <id> finished without a score: the agent submitted nothing, and the work phase ended when the agent exited with status 0; its record is <scratch>/home/.local/share/pumpkin/runs/<id>
            pumpkin: to run it again: pumpkin run --sandboxd <sandboxd> <scratch>/exits/task.star
            record: agent/console.txt, agent/log/command.stderr, agent/log/command.stdout, agent/out/, agent/sandbox.json, driver.log, driver.pid, ir.json, log.jsonl, outcome.json, payloads/, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local","report":null}
            outcome.json: {"score":null,"state":"finished","verdict":{"kind":"no_submission"},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            printed outcome.json: true
            $ pumpkin run submits/task.star --sandboxd <sandboxd> --wait --runs runs --config conf/pumpkin.toml
            exit status: Some(0)
            pumpkin: run <id> records into <scratch>/runs/<id>
            pumpkin: run <id> started
            pumpkin: following run <id>, recorded in <scratch>/runs/<id>: ending `pumpkin run` does not stop it
            pumpkin: run <id> finished with a score of 3, after the work phase ended when the agent exited with status 0; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/conf/pumpkin.toml --sandboxd <sandboxd> <scratch>/submits/task.star
            record: agent/console.txt, agent/log/command.stderr, agent/log/command.stdout, agent/out/, agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/console.txt, jobs/0/log/command.stderr, jobs/0/log/command.stdout, jobs/0/out/evidence/, jobs/0/out/result.json, jobs/0/sandbox.json, log.jsonl, outcome.json, payloads/44778d82365e4af681c40d5f0eef5cf6f5899d3f0ac335050a7ed6779cf3f674, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local","report":null}
            job 0's sandbox, jobs/0/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local","report":null}
            outcome.json: {"score":3,"state":"finished","verdict":{"kind":"judged","result":{"returned":{"raw_score":3},"score":3},"submission":{"finalized":true,"payload":{"digest":"sha256:44778d82365e4af681c40d5f0eef5cf6f5899d3f0ac335050a7ed6779cf3f674","size":3}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            printed outcome.json: true
            $ pumpkin run fails/task.star --sandboxd <sandboxd> --wait --runs runs --config conf/pumpkin.toml
            exit status: Some(3)
            pumpkin: run <id> records into <scratch>/runs/<id>
            pumpkin: run <id> started
            pumpkin: following run <id>, recorded in <scratch>/runs/<id>: ending `pumpkin run` does not stop it
            pumpkin: the final judge's standard error, <scratch>/runs/<id>/jobs/0/log/command.stderr, holds:
              scoring failed
              \u{1b}[1mbold\u{1b}[0m
            pumpkin: run <id> finished without a score: the final judge returned no result (invalid_result: the judge wrote no result: write it at /pumpkin/out/result.json), after the work phase ended when the agent exited with status 0; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/conf/pumpkin.toml --sandboxd <sandboxd> <scratch>/fails/task.star
            record: agent/console.txt, agent/log/command.stderr, agent/log/command.stdout, agent/out/, agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/console.txt, jobs/0/log/command.stderr, jobs/0/log/command.stdout, jobs/0/out/evidence/, jobs/0/sandbox.json, log.jsonl, outcome.json, payloads/44778d82365e4af681c40d5f0eef5cf6f5899d3f0ac335050a7ed6779cf3f674, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local","report":null}
            job 0's sandbox, jobs/0/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local","report":null}
            outcome.json: {"score":null,"state":"finished","verdict":{"failure":{"kind":"invalid_result","reason":"the judge wrote no result: write it at /pumpkin/out/result.json"},"kind":"no_result","submission":{"finalized":true,"payload":{"digest":"sha256:44778d82365e4af681c40d5f0eef5cf6f5899d3f0ac335050a7ed6779cf3f674","size":3}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            printed outcome.json: true"#]],
    );
    upstream.stop();
    wait_for_each_run(&scratch, &sandboxd);
}

/// What the judges of the run whose agent makes ops do with the payload in their slot: for
/// `slow`, run past any call's timeout; for any other, write a result whose `raw_score` is the
/// payload's size, and evidence, and print who judged it, and where.
const JUDGE: &str = r#"
    payload=/pumpkin/in/payload/raw
    if [ "$(cat $payload)" = slow ]; then sleep 300; fi
    bytes=$(wc -c < $payload)
    printf '{"raw_score": %s}\n' $bytes > /pumpkin/out/result.json
    echo kept > /pumpkin/out/evidence/evidence.txt
    echo "judged $bytes bytes as $(id -u), with HOME=$HOME, in $(pwd)""#;

/// What the agent of the run that makes ops does: shows the modes of the directories and files
/// the run put in its sandbox, and its standard input; then makes ops through `sandboxctl`,
/// whose path stands as `SANDBOXCTL`, printing the reply each got: one the core refuses, for
/// each of a job and a grant it doesn't have and a payload over its grant's limit, and one that
/// the gateway refuses, without the core, for a payload over every grant's; a judge call, which
/// it awaits; one that runs past its timeout; and a submission, which it finalizes.
/// `sandboxctl` exits with 1 for a refusal and 2 for a job that has no score.
const AGENT: &str = r#"#!/bin/sh
    cd /workspace && stat -c '%a %n' .codex .codex/skills .codex/config.toml .tmp agent
    echo "standard input: $(cat)"
    x() { head -c "$1" /dev/zero | tr '\0' x > "$2"; }
    x 6 six && x 9 nine && x 17 seventeen && printf slow > slow
    ctl() {
        out=$(SANDBOXCTL "$@" 2>&1)
        echo "sandboxctl $*: exit $?: $out" | sed 's/"remaining_seconds":[0-9]*/"remaining_seconds":"<s>"/'
    }
    # Waits while job $1 is queued or running, then shows its status.
    await() {
        while SANDBOXCTL status "$1" | grep -qE '"state":"(queued|running)"'; do
            sleep 1
        done
        ctl status "$1"
    }
    ctl ops
    ctl status 0
    ctl judge nope six
    ctl judge train nine
    ctl judge train seventeen
    ctl judge train six
    await 0
    ctl judge train slow
    await 1
    ctl submit submit six --finalize"#;

/// The workflow of the run whose agent makes ops: [`workflow`]'s, whose agent runs
/// [`AGENT`], which the run puts in its sandbox with the fixture's directories, files and
/// standard input, and a directory listed before the one that holds it; whose judges run
/// [`JUDGE`], with room in the train judge's slot for 8 bytes and in the score's for 16, the
/// most an op of the agent's may carry; and whose calls have a timeout of 1 s, without a
/// cumulative limit, so that it is each call's timeout, and what a reply has left depends on no
/// command's time.
fn ops_workflow() -> Value {
    // Spliced in, since `format!` would need each of the shell's braces doubled.
    let script = AGENT.replace("SANDBOXCTL", SANDBOXCTL_PATH);
    let mut ir = workflow("", &["/workspace/agent"]);
    let agent = &mut ir["roles"][0]["runtime"]["agent"];
    let skills = json!({ "path": "/workspace/.codex/skills", "mode": "0750" });
    agent["dirs"].as_array_mut().unwrap().insert(0, skills);
    let launcher = json!({ "path": "/workspace/agent", "mode": "0755", "content": script });
    agent["files"].as_array_mut().unwrap().push(launcher);
    let grant = &mut ir["roles"][0]["grants"][0]["judge"];
    grant["call_timeout_seconds"] = json!(1);
    grant.as_object_mut().unwrap().remove("cumulative_seconds");
    for (role, max_bytes) in [(1, 8), (2, 16)] {
        let judge = &mut ir["roles"][role]["runtime"]["judge"];
        judge["command"]["argv"] = json!(["sh", "-c", JUDGE]);
        judge["slots"][0]["max_bytes"] = json!(max_bytes);
    }
    ir
}

/// What the agent of the run that leases a runner does: opens a lease, waits for its runner, puts
/// a tree of its own there, runs a command that shows where and how it runs and writes a tree from
/// the one put, gets that tree back, puts the first again, which fails as its destination exists,
/// runs one that runs past its timeout, reading the lease's status after each, closes the lease,
/// shows what it got, and finalizes a submission. Each reply's times stand as `<s>`.
const LEASING_AGENT: &str = r#"#!/bin/sh
    printf xxxxxx > /workspace/six
    mkdir --parents /workspace/src/deep && printf hello > /workspace/src/a && printf deep > /workspace/src/deep/b
    ctl() {
        out=$(SANDBOXCTL "$@" 2>&1)
        echo "sandboxctl $*: exit $?: $out" | sed --regexp-extended 's/"(remaining_seconds|elapsed_seconds|busy_seconds|idle_seconds|instance_seconds|cumulative_seconds)":[0-9]+/"\1":"<s>"/g'
    }
    # Waits until lease $1 is ready, then shows its status.
    ready() {
        until SANDBOXCTL lease status "$1" | grep -q '"state":"ready"'; do sleep 1; done
        ctl lease status "$1"
    }
    ctl lease open gpu
    ready 0
    ctl lease put 0 /workspace/src /work/src
    ready 0
    ctl lease run 0 -- sh -c 'echo "in $(pwd) as $(id -u), with CUDA=$CUDA"; echo to stderr >&2; mkdir out; tr a-z A-Z < src/a > out/A; cp --recursive src/deep out/'
    ready 0
    ctl lease get 0 /work/out /workspace/got
    ready 0
    ctl lease put 0 /workspace/src /work/src
    ready 0
    ctl lease run 0 --timeout 1 -- sleep 60
    ready 0
    ctl lease close 0
    echo "got: $(find /workspace/got | sort | tr '\n' ' '), A: $(cat /workspace/got/A), deep/b: $(cat /workspace/got/deep/b)"
    ctl submit submit /workspace/six --finalize"#;

#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
#[expect(clippy::too_many_lines, reason = "most of it is the run's snapshot")]
fn leases_on_docker() {
    let scratch = Scratch::new("leases");
    let sandboxd = sandboxd().to_str().unwrap().to_owned();
    let context = docker_context();
    let engine = engine(&context);
    let script = LEASING_AGENT.replace("SANDBOXCTL", SANDBOXCTL_PATH);
    let mut ir = workflow("", &["/workspace/agent"]);
    let launcher = json!({ "path": "/workspace/agent", "mode": "0755", "content": script });
    let agent = &mut ir["roles"][0]["runtime"]["agent"];
    agent["files"].as_array_mut().unwrap().push(launcher);
    let mut runner = ir["roles"][1].clone();
    runner["name"] = json!("gpu_runner");
    runner["image"] = json!(RUNNER);
    runner["runtime"] = json!({ "runner": { "cwd": "/work", "env": ["CUDA=7"] } });
    ir["roles"].as_array_mut().unwrap().push(runner);
    let grant = json!({ "lease": {
        "name": "gpu", "role": "gpu_runner", "instances": 1, "instance_seconds": 120,
        "commands": 5, "transfer_bytes": 1024,
    } });
    ir["roles"][0]["grants"]
        .as_array_mut()
        .unwrap()
        .insert(1, grant);
    scratch.write_task("leases", &ir);
    scratch.write("pumpkin.toml", &machine(&context));
    build_first(&scratch, "leases", "pumpkin.toml");
    let mut run = scratch.pumpkin(&["run", "leases/task.star", "--sandboxd", &sandboxd, "--wait"]);
    run.args(["--config", "pumpkin.toml"]);
    let output = with_room(&mut run);
    let rootful = !engine.report().security.rootless;
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    let (id, dir) = record(&stderr).unwrap();
    let record = scratch.0.join(dir);
    let mut lines = vec![format!("exit status: {:?}", output.status.code())];
    let listed = containers(&context).list(&RunTag::try_from(id.to_owned()).unwrap());
    lines.push(format!("listed once it ended: {:?}", listed.unwrap()));
    let leases = record.join("leases");
    lines.push(format!("the lease's record: {}", files(&leases).join(", ")));
    for command in ["1", "4"] {
        for stream in ["stdout", "stderr"] {
            let path = leases.join(format!("0/log/{command}/command.{stream}"));
            let text = fs::read_to_string(path).unwrap_or_default();
            lines.push(format!("command {command}'s {stream}: {text:?}"));
        }
    }
    lines.extend(
        standard_outputs(&record)
            .into_iter()
            .take_while(|line| !line.starts_with("job ")),
    );
    let shown = [(&sandboxd[..], "<sandboxd>")];
    check(
        &scratch,
        [Step::Ran(lines.join("\n"))],
        rootful,
        &shown,
        &expect![[r#"
            exit status: Some(0)
            listed once it ended: []
            the lease's record: 0/console.txt, 0/log/1/command.stderr, 0/log/1/command.stdout, 0/log/4/command.stderr, 0/log/4/command.stdout, 0/out/, 0/sandbox.json
            command 1's stdout: "in /work as 65532, with CUDA=7\n"
            command 1's stderr: "to stderr\n"
            command 4's stdout: ""
            command 4's stderr: ""
            the agent's standard output:
            sandboxctl lease open gpu: exit 0: {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":5}}
            sandboxctl lease status 0: exit 0: {"kind":"lease_status","lease":0,"state":"ready","elapsed_seconds":"<s>","busy_seconds":"<s>","idle_seconds":"<s>","failure":null,"last":null,"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":5}}
            sandboxctl lease put 0 /workspace/src /work/src: exit 0: {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":4}}
            sandboxctl lease status 0: exit 0: {"kind":"lease_status","lease":0,"state":"ready","elapsed_seconds":"<s>","busy_seconds":"<s>","idle_seconds":"<s>","failure":null,"last":{"kind":"transfer","transfer":0,"end":{"moved":{"entries":3,"bytes":9}}},"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":4}}
            sandboxctl lease run 0 -- sh -c echo "in $(pwd) as $(id -u), with CUDA=$CUDA"; echo to stderr >&2; mkdir out; tr a-z A-Z < src/a > out/A; cp --recursive src/deep out/: exit 0: {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":3}}
            sandboxctl lease status 0: exit 0: {"kind":"lease_status","lease":0,"state":"ready","elapsed_seconds":"<s>","busy_seconds":"<s>","idle_seconds":"<s>","failure":null,"last":{"kind":"command","command":1,"end":{"exited":{"code":0}},"stdout":"in /work as 65532, with CUDA=7\n","stderr":"to stderr\n"},"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":3}}
            sandboxctl lease get 0 /work/out /workspace/got: exit 0: {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":2}}
            sandboxctl lease status 0: exit 0: {"kind":"lease_status","lease":0,"state":"ready","elapsed_seconds":"<s>","busy_seconds":"<s>","idle_seconds":"<s>","failure":null,"last":{"kind":"transfer","transfer":2,"end":{"moved":{"entries":3,"bytes":9}}},"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":2}}
            sandboxctl lease put 0 /workspace/src /work/src: exit 0: {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":1}}
            sandboxctl lease status 0: exit 0: {"kind":"lease_status","lease":0,"state":"ready","elapsed_seconds":"<s>","busy_seconds":"<s>","idle_seconds":"<s>","failure":null,"last":{"kind":"transfer","transfer":3,"end":{"failed":"putting the tree at /work/src in the runner failed: /work/src: File exists (os error 17): the tree's root becomes /work/src, which must not exist: give `--replace` to replace it"}},"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":1}}
            sandboxctl lease run 0 --timeout 1 -- sleep 60: exit 0: {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":0}}
            sandboxctl lease status 0: exit 0: {"kind":"lease_status","lease":0,"state":"ready","elapsed_seconds":"<s>","busy_seconds":"<s>","idle_seconds":"<s>","failure":null,"last":{"kind":"command","command":4,"end":"timed_out","stdout":"","stderr":""},"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":0}}
            sandboxctl lease close 0: exit 0: {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":0}}
            got: /workspace/got /workspace/got/A /workspace/got/deep /workspace/got/deep/b , A: HELLO, deep/b: deep
            sandboxctl submit submit /workspace/six --finalize: exit 0: {"kind":"submit","state":"accepted","remaining_seconds":"<s>"}"#]],
    );
}

#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
#[expect(clippy::too_many_lines, reason = "most of it is the run's snapshot")]
fn ops_on_docker() {
    let scratch = Scratch::new("ops");
    let sandboxd = sandboxd().to_str().unwrap().to_owned();
    let context = docker_context();
    let engine = engine(&context);
    scratch.write_task("ops", &ops_workflow());
    scratch.write("pumpkin.toml", &machine(&context));
    build_first(&scratch, "ops", "pumpkin.toml");
    let runs = format!("runs/{},\"quoted\"", "long".repeat(30));
    let mut run = scratch.pumpkin(&["run", "ops/task.star", "--sandboxd", &sandboxd, "--wait"]);
    run.args(["--runs", &runs, "--config", "pumpkin.toml"]);
    let invoked = under_umask(&mut run, 0o002);
    let output = with_room(&mut run);
    let rootful = !engine.report().security.rootless;
    let mut lines = vec![report(invoked, &output, &scratch.0, rootful)];
    let stderr = String::from_utf8(output.stderr).unwrap();
    let (id, dir) = record(&stderr).unwrap();
    let listed = containers(&context).list(&RunTag::try_from(id.to_owned()).unwrap());
    lines.push(format!("listed once it ended: {:?}", listed.unwrap()));
    let sockets = scratch.0.join(dir).join("sockets");
    let over = sockets.join("agent/s").into_os_string().len() > 107;
    lines.push(format!(
        "the agent's socket's path is over 107 bytes: {over}"
    ));
    lines.push(format!("socket directory left: {}", sockets.exists()));
    let below = modes_below(&scratch.0.join(dir));
    lines.push(format!("modes in the record: {below}"));
    lines.extend(standard_outputs(&scratch.0.join(dir)));
    let shown = [(&sandboxd[..], "<sandboxd>"), (&runs[..], "<runs>")];
    check(
        &scratch,
        [Step::Ran(lines.join("\n"))],
        rootful,
        &shown,
        &expect![[r#"
            $ umask 002 && pumpkin run ops/task.star --sandboxd <sandboxd> --wait --runs <runs> --config pumpkin.toml
            exit status: Some(0)
            pumpkin: run <id> records into <scratch>/<runs>/<id>
            pumpkin: run <id> started
            pumpkin: following run <id>, recorded in <scratch>/<runs>/<id>: ending `pumpkin run` does not stop it
            pumpkin: run <id> finished with a score of 6, after the work phase ended when the agent exited with status 0; its record is <scratch>/<runs>/<id>
            pumpkin: to run it again: pumpkin run --runs '<scratch>/<runs>' --config <scratch>/pumpkin.toml --sandboxd <sandboxd> <scratch>/ops/task.star
            record: agent/console.txt, agent/log/command.stderr, agent/log/command.stdout, agent/out/, agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/console.txt, jobs/0/log/command.stderr, jobs/0/log/command.stdout, jobs/0/out/evidence/evidence.txt, jobs/0/out/result.json, jobs/0/sandbox.json, jobs/1/console.txt, jobs/1/log/command.stderr, jobs/1/log/command.stdout, jobs/1/out/evidence/, jobs/1/sandbox.json, jobs/2/console.txt, jobs/2/log/command.stderr, jobs/2/log/command.stdout, jobs/2/out/evidence/evidence.txt, jobs/2/out/result.json, jobs/2/sandbox.json, log.jsonl, outcome.json, payloads/5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6, payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd, provider.json, record.json
            the agent's sandbox, agent/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local","report":null}
            job 0's sandbox, jobs/0/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local","report":null}
            job 1's sandbox, jobs/1/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local","report":null}
            job 2's sandbox, jobs/2/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local","report":null}
            outcome.json: {"score":6,"state":"finished","verdict":{"kind":"judged","result":{"returned":{"raw_score":6},"score":6},"submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}}},"work_end":{"kind":"agent_exited","status":{"code":0}}}
            printed outcome.json: true
            listed once it ended: []
            the agent's socket's path is over 107 bytes: true
            socket directory left: false
            modes in the record: directories 755; files 644
            the agent's standard output:
            700 .codex
            750 .codex/skills
            600 .codex/config.toml
            700 .tmp
            755 agent
            standard input: Solve the task in /task/README.md.
            sandboxctl ops: exit 0: {"kind":"ops","remaining_seconds":"<s>","resources":{"cpu_millis":500,"memory_bytes":268435456},"grants":[{"kind":"judge","name":"train","max_bytes":8,"calls":100,"cumulative_seconds":null,"call_timeout_seconds":1},{"kind":"submit","name":"submit","max_bytes":16}]}
            sandboxctl status 0: exit 1: {"kind":"refused","message":"you have no job 0: ask about a job that one of your calls returned"}
            sandboxctl judge nope six: exit 1: {"kind":"refused","message":"you hold no grant named `nope`: call one that `ops` lists"}
            sandboxctl judge train nine: exit 1: {"kind":"refused","message":"the payload is 9 bytes, over `train`'s limit of 8: send a smaller one"}
            sandboxctl judge train seventeen: exit 1: {"kind":"refused","message":"the payload is 17 bytes, more than the limit of 16: send at most 16"}
            sandboxctl judge train six: exit 0: {"kind":"judge","job":0,"remaining":{"calls":99,"seconds":null}}
            sandboxctl status 0: exit 0: {"kind":"status","job":0,"state":"completed","returned":{"raw_score":6},"scored":true,"remaining":{"calls":99,"seconds":null}}
            sandboxctl judge train slow: exit 0: {"kind":"judge","job":1,"remaining":{"calls":98,"seconds":null}}
            sandboxctl status 1: exit 2: {"kind":"status","job":1,"state":"errored","failure":{"code":"judge_timeout","message":"the judge ran past its timeout"},"remaining":{"calls":98,"seconds":null}}
            sandboxctl submit submit six --finalize: exit 0: {"kind":"submit","state":"accepted","remaining_seconds":"<s>"}
            job 0's standard output:
            judged 6 bytes as 65532, with HOME=/pumpkin/home, in /pumpkin/home
            job 1's standard output:
            job 2's standard output:
            judged 6 bytes as 65532, with HOME=/pumpkin/home, in /pumpkin/home"#]],
    );
}

/// How many records `runs`, the runs directory in `scratch`, holds.
fn runs_made(scratch: &Scratch) -> usize {
    fs::read_dir(scratch.0.join("runs")).map_or(0, Iterator::count)
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is what each share shows, and the snapshot"
)]
fn cpus() {
    let scratch = Scratch::new("cpus");
    let stand_in = StandIn::new(&scratch);
    let cores = machine_cores();
    assert!(
        cores >= 5,
        "the machine has {cores} cores: run the test on one of 5 at least"
    );
    let threads = machine_cpus() / cores;
    let sized = |name: &str, [agent, train, last]: [u64; 3]| {
        let mut ir = workflow("", &["true"]);
        for (role, count) in [agent, train, last].into_iter().enumerate() {
            ir["roles"][role]["needs"]["cpu_millis"] = json!(count * 1000 - 500);
        }
        scratch.write_task(name, &ir);
    };
    sized("first", [1, 2, 1]);
    sized("second", [2, 1, 1]);
    // A core fewer than the two runs leave, then one for the train judge.
    sized("third", [threads * (cores - 4), 1, 1]);
    stand_in.hold("create");
    let started = |task: &str| {
        let output = stand_in
            .run(&format!("{task}/task.star"), &[])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    let (first, second) = (started("first"), started("second"));
    let shares = |id: &str| -> BTreeMap<String, BTreeSet<u32>> {
        let entry = fs::read_to_string(ledger(&scratch).join(format!("{id}.json"))).unwrap();
        let entry: Value = serde_json::from_str(&entry).unwrap();
        // Each role runs one sandbox at a time, instance 0.
        let roles = entry["roles"].as_object().unwrap().iter();
        let role = |key: &str| key.strip_suffix("/0").unwrap().to_owned();
        roles
            .map(|(key, cpus)| (role(key), cpu_list(cpus.as_str().unwrap())))
            .collect()
    };
    let (ours, theirs) = (shares(&first), shares(&second));
    // Whole cores, and none more than hold `needed` CPUs.
    let fitted = |cpus: &BTreeSet<u32>, needed: usize| {
        let whole = cpus.iter().all(|&cpu| siblings(cpu).is_subset(cpus));
        let core = cpus.first().map_or(0, |&cpu| siblings(cpu).len());
        whole && cpus.len() >= needed && cpus.len() - core < needed
    };
    let all = |shares: &BTreeMap<String, BTreeSet<u32>>| -> BTreeSet<u32> {
        shares.values().flatten().copied().collect()
    };
    let mut lines = vec![
        format!(
            "the first's roles on whole cores, as few as hold their CPUs: {}",
            fitted(&ours["agent"], 1)
                && fitted(&ours["train_judge"], 2)
                && fitted(&ours["final_judge"], 1)
        ),
        format!(
            "its agent and train judge apart: {}",
            ours["agent"].is_disjoint(&ours["train_judge"])
        ),
        format!(
            "its final judge on its agent's: {}",
            ours["final_judge"] == ours["agent"]
        ),
        format!(
            "the second's on whole cores, as few as hold their CPUs, apart: {}",
            fitted(&theirs["agent"], 2)
                && fitted(&theirs["train_judge"], 1)
                && theirs["agent"].is_disjoint(&theirs["train_judge"])
        ),
        format!(
            "the second's apart from the first's: {}",
            all(&ours).is_disjoint(&all(&theirs))
        ),
    ];
    let refused = ran(stand_in.run("third/task.star", &[]), &scratch.0, false);
    let refused = (refused.replace(&first, "<run>").replace(&second, "<run>"))
        .replace(
            &format!("need {} whole", cores - 3),
            "need <cores - 3> whole",
        )
        .replace(
            &format!("`agent` {}", threads * (cores - 4)),
            "`agent` <threads × (cores - 4)>",
        )
        .replace(&format!("the {cores} that"), "the <cores> that");
    lines.push(format!("{refused}\nrecords made: {}", runs_made(&scratch)));
    stand_in.release("create");
    for id in [&first, &second] {
        let wait = scratch.pumpkin(&["wait", id, "--runs", "runs"]).output();
        lines.push(format!(
            "the run ended: {}",
            wait.unwrap().status.code() == Some(3)
        ));
    }
    let third = stand_in.run("third/task.star", &[]).output().unwrap();
    lines.push(format!("then the third starts: {}", third.status.success()));
    check(
        &scratch,
        lines.into_iter().map(Step::Ran),
        false,
        &[],
        &expect![[r#"
            the first's roles on whole cores, as few as hold their CPUs: true
            its agent and train judge apart: true
            its final judge on its agent's: true
            the second's on whole cores, as few as hold their CPUs, apart: true
            the second's apart from the first's: true
            $ PATH=<scratch>/bin:/usr/bin:/bin pumpkin run third/task.star --sandboxd sandboxd --runs runs --config pumpkin.toml
            exit status: Some(1)
            pumpkin: reserving on provider `local`: the run's roles need <cores - 3> whole cores at once, to hold their CPUs (`agent` <threads × (cores - 4)>, `train_judge` 1, `final_judge` 1), and runs <run>, <run> hold 4 of the <cores> that this machine gives sandboxes: run it once they end, or give its roles fewer CPUs
            records made: 2
            the run ended: true
            the run ended: true
            then the third starts: true"#]],
    );
}
