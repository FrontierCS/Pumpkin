//! `run`: what it finds wrong before it reaches the engine, and runs on Docker
//! followed to their ends with `--wait`, among them one whose agent makes ops through
//! `sandboxctl`.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::Permissions;
use std::io::{BufReader, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread::{self, JoinHandle};
use std::{env, fs};

use expect_test::{expect, expect_file};
use provider::{RunTag, Sweep as _};
use schema::sandbox::{MODEL_PORT, SANDBOXCTL_PATH};
use serde_json::{Value, json};
use test_support::http::{read_request, write_chunk};
use test_support::{BUSYBOX, docker, docker_context, files, sandboxd, standard_outputs};

use super::{
    EC2, LOCAL, PUMPKIN, RUNNER, Scratch, Step, check, containers, copy_tree, docker_table, engine,
    final_running, invocation, machine, ran, record, report, through_shell, with_room, workflow,
};

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn usage() {
    let scratch = Scratch::new();
    let same = |args: &[&str], like: &[&str]| {
        let ran = |args| scratch.pumpkin(args).output().unwrap();
        let same = ran(args) == ran(like);
        let (args, like) = (args.join(" "), like.join(" "));
        Step::Ran(format!(
            "$ pumpkin {args}\ndoes what `pumpkin {like}` does: {same}"
        ))
    };
    let pumpkin = |args: &[&str]| Step::from(scratch.pumpkin(args));
    let admin = |args: &[&str]| Step::from(scratch.admin(args));
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
        pumpkin(&["run", "--outcome", "a.star"]),
        pumpkin(&["cancel", "--any-member=yes", "6a"]),
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
        same(&["help", "controller"], &["controller", "--help"]),
        pumpkin(&["controller", "install", "--settings", "arn"]),
        admin(&["install", "ec2", "--settings", "/pumpkin-alice/settings"]),
        admin(&["install", "docker", "--settings", "settings.toml"]),
        admin(&["check", "--help"]),
        admin(&["run", "a.star"]),
    ];
    // What `--version` prints: this build, which names its commit if it was made in a git
    // checkout of Pumpkin's repository.
    let this = format!("pumpkin {}\n", runtime::BUILD);
    check(
        &scratch,
        steps,
        false,
        &[(&this, "pumpkin <this>\n")],
        &expect![[r#"
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
            Run workflows of agents in sandboxes

            Usage: pumpkin <command>

            Commands:
              check   Check tasks without building or running them
              fmt     Format `.star` files
              run     Build a task and start a run of it
              wait    Wait for a run to end and say how it ended
              ls      List runs and their states
              cancel  Stop a run and remove what it left
              view    Serve the runs to this machine's browser
              help    Print the help of a command or a topic

            Options:
              -V, --version  Print version
              -h, --help     Print help

            Rarely needed:
              reap        Remove what ended runs left
              controller  Run the controller's own commands

            Topics:
              config  The machine's configuration, pumpkin.toml

            Exit status:
              0  Success.
              1  An error. A `pumpkin:` line on the standard error says why.
              2  A usage error.
              3  An outcome short of success, which the command's help says.

            `pumpkin help <command>` prints a command's help, and `pumpkin help <topic>`
            prints a topic. The guide to running workflows is docs/running.md:
            https://github.com/FrontierCS/Pumpkin/blob/main/docs/running.md
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
            $ pumpkin run --outcome a.star
            exit status: Some(2)
            error: the following required arguments were not provided:
              --wait

            Usage: pumpkin run --wait --outcome <entry>
            pumpkin: to read how to use it: pumpkin help run
            $ pumpkin cancel --any-member=yes 6a
            exit status: Some(2)
            error: unexpected value 'yes' for '--any-member' found; no more were expected

            Usage: pumpkin cancel --any-member <run>
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
            error: there is no help for `bogus`: expected a command (`check`, `fmt`, `run`, `wait`, `ls`, `cancel`, `view`, `help`, `reap`, `controller`) or a topic (`config`)

            Usage: pumpkin help [topic]
            pumpkin: to read how to use it: pumpkin help help
            $ pumpkin help controller
            does what `pumpkin controller --help` does: true
            $ pumpkin controller install --settings arn
            exit status: Some(2)
            error: unrecognized subcommand 'install'

            Usage: pumpkin controller <action>
            pumpkin: to read how to use it: pumpkin help controller
            $ pumpkin-admin install ec2 --settings /pumpkin-alice/settings
            exit status: Some(2)
            error: invalid value for '--settings <settings>': `/pumpkin-alice/settings` is not an SSM parameter's ARN: use one such as `arn:aws:ssm:us-west-1:123456789012:parameter/pumpkin-alice/settings`
            pumpkin: to read how to use it: pumpkin-admin install --help
            $ pumpkin-admin install docker --settings settings.toml
            exit status: Some(2)
            error: invalid value for '--settings <settings>': `settings.toml` is no absolute path: name the file of the controller's settings by one
            pumpkin: to read how to use it: pumpkin-admin install --help
            $ pumpkin-admin check --help
            exit status: Some(0)
            printed:
              Check the controller's deployment, with read-only calls on AWS.

              `check` reads the settings that the ARN names, what they name, and the controller's instance. It
              asks IAM's policy simulator about the policy of each role that `pumpkin-admin policy` prints. It
              runs every check, and says each problem on a line of its own: the resource, what is wrong, and what
              to fix. It says nothing if there is none. It starts nothing, so it leaves unchecked what only a
              running controller shows, and says so.

              Usage: pumpkin-admin check --settings <arn>

              Options:
                    --settings <arn>
                        The ARN of the SSM parameter of the controller's settings

                -h, --help
                        Print help

              Examples:
                pumpkin-admin check --settings \
                  arn:aws:ssm:us-west-1:123456789012:parameter/pumpkin-alice/settings
                        Check the deployment whose settings the ARN names

              Exit status:
                0  `check` found no problem.
                1  `check` found a problem, or failed.
            $ pumpkin-admin run a.star
            exit status: Some(2)
            error: unrecognized subcommand 'run'

            Usage: pumpkin-admin <command>
            pumpkin: to read how to use it: pumpkin-admin --help"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn unrunnable() {
    let scratch = Scratch::new();
    scratch.write_task("workflow", &test_support::workflow());
    scratch.write("empty/task.star", "");
    scratch.write_sandboxd("sandboxd");
    scratch.write_sandboxd("private");
    fs::set_permissions(scratch.0.join("private"), Permissions::from_mode(0o750)).unwrap();
    scratch.write_executable("script", b"#!/bin/sh\n");
    // Links to `pumpkin`: one alone, and one beside a `sandboxd` that is `pumpkin` too, linked
    // dynamically, both in no `.pumpkin`, and one in a workspace whose path isn't UTF-8.
    let (alone, checkout) = (scratch.0.join("alone"), scratch.0.join("checkout"));
    let unnamed = scratch.0.join(OsStr::from_bytes(b"\xff/.pumpkin/pinned"));
    for dir in [&alone, &checkout, &unnamed] {
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
    let outside = |args: &[&str]| scratch.program(alone.join("pumpkin"), args);
    let not_utf8 = |args: &[&str]| scratch.program(unnamed.join("pumpkin"), args);
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
        outside(&["run", "workflow/task.star", "--sandboxd", "sandboxd"]),
        outside(&["wait", "6a"]),
        not_utf8(&["run", "workflow/task.star", "--sandboxd", "sandboxd"]),
        not_utf8(&["wait", "6a"]),
        not_utf8(&["cancel", "6a"]),
        not_utf8(&["reap"]),
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
            pumpkin: the sandboxd beside `pumpkin`, <scratch>/alone/sandboxd, can't be read (No such file or directory (os error 2)): `cargo xtask build` puts `pumpkin` beside a static one, or name one with `--sandboxd`
            $ <scratch>/checkout/pumpkin run workflow/task.star --runs runs
            exit status: Some(1)
            pumpkin: the sandboxd beside `pumpkin`, <scratch>/checkout/sandboxd, is linked dynamically, and a sandbox runs only a static one: `cargo xtask build` puts `pumpkin` beside a static one, or name one with `--sandboxd`
            $ <scratch>/alone/pumpkin run workflow/task.star --sandboxd sandboxd
            exit status: Some(1)
            pumpkin: this `pumpkin`, <scratch>/alone/pumpkin, is in no directory named `.pumpkin`, which would hold all it writes: run the one that a repository's `./pumpkin` launches, or that `cargo xtask build` puts in a checkout's `.pumpkin/bin`
            $ <scratch>/alone/pumpkin wait 6a
            exit status: Some(1)
            pumpkin: this `pumpkin`, <scratch>/alone/pumpkin, is in no directory named `.pumpkin`, which would hold all it writes: run the one that a repository's `./pumpkin` launches, or that `cargo xtask build` puts in a checkout's `.pumpkin/bin`
            $ <scratch>/�/.pumpkin/pinned/pumpkin run workflow/task.star --sandboxd sandboxd
            exit status: Some(1)
            pumpkin: the workspace's runs directory <scratch>/�/.pumpkin/runs isn't UTF-8, as the commands that `pumpkin` says to run next name it: move the workspace to a UTF-8 path, or name a UTF-8 directory with `--runs`
            $ <scratch>/�/.pumpkin/pinned/pumpkin wait 6a
            exit status: Some(1)
            pumpkin: the workspace's runs directory <scratch>/�/.pumpkin/runs isn't UTF-8, as the commands that `pumpkin` says to run next name it: move the workspace to a UTF-8 path, or name a UTF-8 directory with `--runs`
            $ <scratch>/�/.pumpkin/pinned/pumpkin cancel 6a
            exit status: Some(1)
            pumpkin: the workspace's runs directory <scratch>/�/.pumpkin/runs isn't UTF-8, as the commands that `pumpkin` says to run next name it: move the workspace to a UTF-8 path, or name a UTF-8 directory with `--runs`
            $ <scratch>/�/.pumpkin/pinned/pumpkin reap
            exit status: Some(1)
            pumpkin: the workspace's runs directory <scratch>/�/.pumpkin/runs isn't UTF-8, as the commands that `pumpkin` says to run next name it: move the workspace to a UTF-8 path, or name a UTF-8 directory with `--runs`
            $ env --unset=HOME pumpkin run workflow/task.star --sandboxd sandboxd
            exit status: Some(1)
            pumpkin: HOME is unset or empty: set it, or name the configuration with `--config`"#]],
    );
    assert!(!scratch.0.join("runs").exists(), "a refusal made a record");
}

/// Runs `wait` on each ended run recorded in `scratch`'s `runs`, which doesn't reach the engine,
/// and shows `sandboxd` as `<sandboxd>`.
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
            exit status: Some(0)
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container, started at <t>, exited 0 at <t>; its work ended at <t>
                        model: 1 request, all answered 200; dropped request headers: user-agent; no response reported its tokens; of 1 good answer, by the median, <t> to the end
                 <t> final call job 0 on local/container, started after <t>, ran <t>: score=1
                        model: 1 request, all answered 200; no response reported its tokens; of 1 good answer, by the median, <t> to the end
              model: 2 requests, all answered 200; dropped request headers: user-agent; no response reported its tokens; of 2 good answers, by the median, <t> to the end
              sandboxes: 2 on local/container, <t> in all, 2 created, all deleted
            pumpkin: run <id> finished with a result (score=1); its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/conf/pumpkin.toml --sandboxd <sandboxd> <scratch>/asks/task.star
            $ pumpkin wait <id> --runs runs
            exit status: Some(0)
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container, started at <t>, exited 0 at <t>; its work ended at <t>
                 <t> final call job 0 on local/container, started after <t>, ran <t>: score=3
              sandboxes: 2 on local/container, <t> in all, 2 created, all deleted
            pumpkin: run <id> finished with a result (score=3); its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/conf/pumpkin.toml --sandboxd <sandboxd> <scratch>/submits/task.star
            $ pumpkin wait <id> --runs runs
            exit status: Some(3)
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container, started at <t>, exited 0 at <t>; its work ended at <t>
                 <t> final call job 0 on local/container, started after <t>, ran <t>: failed: fails/final.star:18:34: getting /pumpkin/out/result.json failed: /pumpkin/out/result.json: No such file or directory (os error 2)\n    18 |     return {"score": json.decode(target.get("/pumpkin/out/result.json"))["raw_score"]}\n       |                                  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^\n    in `final_call()`, called at fails/final.star:3:5
              sandboxes: 2 on local/container, <t> in all, 2 created, all deleted
            pumpkin: run <id> finished without a result: the final call gave none (error: fails/final.star:18:34: getting /pumpkin/out/result.json failed: /pumpkin/out/result.json: No such file or directory (os error 2)
                18 |     return {"score": json.decode(target.get("/pumpkin/out/result.json"))["raw_score"]}
                   |                                  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `final_call()`, called at fails/final.star:3:5); its commands' logs are in <scratch>/runs/<id>/jobs/0/log; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/conf/pumpkin.toml --sandboxd <sandboxd> <scratch>/fails/task.star"#]],
    );
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
    let scratch = Scratch::new();
    let mut ir = test_support::workflow();
    scratch.write_task("workflow", &ir);
    let mut budgeted = ir.clone();
    budgeted["roles"][0]["runtime"]["agent"]["egress"]["model"][0]["budget"] =
        json!({ "microusd": 1_000_000 });
    scratch.write_task("budgeted", &budgeted);
    ir["roles"][1]["name"] = json!("agent");
    // In directories named so that the command that runs it again quotes it, for its `'`, or
    // `~` alone, each the root of its tasks.
    scratch.write_task_in("it's broken", "tasks", &ir);
    scratch.write_task_in("a~b", "tasks", &ir);
    write_configurations(&scratch);
    let uncosted = fs::read_to_string(scratch.0.join("conf/uncosted.toml")).unwrap();
    scratch.write("home/.config/pumpkin/pumpkin.toml", &uncosted);
    scratch.write("home/.config/pumpkin/key", "sk-test\n");
    let run = |entry, config: &str| {
        let mut command = scratch.pumpkin(&["run", entry, "--sandboxd", "sandboxd"]);
        command.args(["--runs", "runs", "--config", config]);
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
        // Refused before the engine is reached, as their Docker context names none.
        Step::from(run("budgeted/task.star", "conf/uncached.toml")),
        Step::from(run("budgeted/task.star", "conf/unwritten.toml")),
        Step::from(run("budgeted/task.star", "conf/uncosted.toml")),
        // `check` refuses what `run` does, by the configuration that `--config` names, or else
        // by the default one.
        Step::from(scratch.pumpkin(&["check", "budgeted/task.star", "--config", "unserved.toml"])),
        Step::from(scratch.pumpkin(&[
            "check",
            "budgeted/task.star",
            "--config",
            "conf/served.toml",
        ])),
        Step::from(scratch.pumpkin(&["check", "budgeted/task.star"])),
    ];
    check(
        &scratch,
        steps,
        false,
        &[],
        &expect![[r#"
            $ pumpkin run it's broken/tasks/task.star --sandboxd sandboxd --runs runs --config conf/served.toml
            exit status: Some(1)
            tasks/task.star:9:895: `agent` also names /roles/0: give each a unique name
                9 |     roles = [{"image": IMAGE, "name": "agent", "needs": {"cpu_millis": 8000, "memory_bytes": 34359738368}, "runtime": {"agent": {"command": {"argv": ["/usr/local/bin/pumpkin-fake-agent"], "cwd": "/workspace"}, "dirs": [{"mode": "0700", "path": "/workspace/.codex"}, {"mode": "0700", "path": "/workspace/.tmp"}], "egress": {"model": [{"api": "responses", "models": ["gpt-6-astra"]}]}, "files": [{"content": "model = \"gpt-6-astra\"\n", "mode": "0600", "path": "/workspace/.codex/config.toml"}], "finalize_grace_seconds": 60, "grants": [{"job": {"body": train, "call_timeout_seconds": 12000, "calls": 100, "cumulative_seconds": 12000, "payload_max_bytes": 4194304, "target": "trainer"}, "name": "train"}, {"name": "submit", "now": {"body": submit, "payload_max_bytes": 4194304}}], "stdin": "Solve the task in /task/README.md.\n"}}, "user": {"gid": 10001, "uid": 10001}}, {"image": IMAGE, "name": "agent", "needs": {"cpu_millis": 8000, "memory_bytes": 2147483648}, "runtime": {"target": {"cwd": "/workspace", "env": ["MODE=train", "DATASET=/data/train.json"]}}, "user": {"gid": 65532, "uid": 65532}}, {"image": IMAGE, "name": "final_judge", "needs": {"cpu_millis": 8000, "memory_bytes": 2147483648}, "runtime": {"target": {"cwd": "/pumpkin/home", "env": ["MODE=eval", "DATASET=/data/eval.json"]}}, "user": {"gid": 65532, "uid": 65532}}],
                  |                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               ^^^^^^^
                the IR's /roles/1/name
            tasks/task.star:9:674: no role is named `trainer`: name one listed in `roles`
                9 |     roles = [{"image": IMAGE, "name": "agent", "needs": {"cpu_millis": 8000, "memory_bytes": 34359738368}, "runtime": {"agent": {"command": {"argv": ["/usr/local/bin/pumpkin-fake-agent"], "cwd": "/workspace"}, "dirs": [{"mode": "0700", "path": "/workspace/.codex"}, {"mode": "0700", "path": "/workspace/.tmp"}], "egress": {"model": [{"api": "responses", "models": ["gpt-6-astra"]}]}, "files": [{"content": "model = \"gpt-6-astra\"\n", "mode": "0600", "path": "/workspace/.codex/config.toml"}], "finalize_grace_seconds": 60, "grants": [{"job": {"body": train, "call_timeout_seconds": 12000, "calls": 100, "cumulative_seconds": 12000, "payload_max_bytes": 4194304, "target": "trainer"}, "name": "train"}, {"name": "submit", "now": {"body": submit, "payload_max_bytes": 4194304}}], "stdin": "Solve the task in /task/README.md.\n"}}, "user": {"gid": 10001, "uid": 10001}}, {"image": IMAGE, "name": "agent", "needs": {"cpu_millis": 8000, "memory_bytes": 2147483648}, "runtime": {"target": {"cwd": "/workspace", "env": ["MODE=train", "DATASET=/data/train.json"]}}, "user": {"gid": 65532, "uid": 65532}}, {"image": IMAGE, "name": "final_judge", "needs": {"cpu_millis": 8000, "memory_bytes": 2147483648}, "runtime": {"target": {"cwd": "/pumpkin/home", "env": ["MODE=eval", "DATASET=/data/eval.json"]}}, "user": {"gid": 65532, "uid": 65532}}],
                  |                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  ^^^^^^^^^
                the IR's /roles/0/runtime/agent/grants/0/job/target
            tasks/task.star:9:870: `agent` is a target that no job function or final call names: name it from one
                9 |     roles = [{"image": IMAGE, "name": "agent", "needs": {"cpu_millis": 8000, "memory_bytes": 34359738368}, "runtime": {"agent": {"command": {"argv": ["/usr/local/bin/pumpkin-fake-agent"], "cwd": "/workspace"}, "dirs": [{"mode": "0700", "path": "/workspace/.codex"}, {"mode": "0700", "path": "/workspace/.tmp"}], "egress": {"model": [{"api": "responses", "models": ["gpt-6-astra"]}]}, "files": [{"content": "model = \"gpt-6-astra\"\n", "mode": "0600", "path": "/workspace/.codex/config.toml"}], "finalize_grace_seconds": 60, "grants": [{"job": {"body": train, "call_timeout_seconds": 12000, "calls": 100, "cumulative_seconds": 12000, "payload_max_bytes": 4194304, "target": "trainer"}, "name": "train"}, {"name": "submit", "now": {"body": submit, "payload_max_bytes": 4194304}}], "stdin": "Solve the task in /task/README.md.\n"}}, "user": {"gid": 10001, "uid": 10001}}, {"image": IMAGE, "name": "agent", "needs": {"cpu_millis": 8000, "memory_bytes": 2147483648}, "runtime": {"target": {"cwd": "/workspace", "env": ["MODE=train", "DATASET=/data/train.json"]}}, "user": {"gid": 65532, "uid": 65532}}, {"image": IMAGE, "name": "final_judge", "needs": {"cpu_millis": 8000, "memory_bytes": 2147483648}, "runtime": {"target": {"cwd": "/pumpkin/home", "env": ["MODE=eval", "DATASET=/data/eval.json"]}}, "user": {"gid": 65532, "uid": 65532}}],
                  |                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                the IR's /roles/1
            pumpkin: it's broken/tasks/task.star: the task has 3 problems
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/conf/served.toml --sandboxd <scratch>/sandboxd '<scratch>/it'\''s broken/tasks/task.star'
            $ pumpkin run a~b/tasks/task.star --sandboxd sandboxd --runs two
            lines --config my served.toml
            exit status: Some(1)
            tasks/task.star:9:895: `agent` also names /roles/0: give each a unique name
                9 |     roles = [{"image": IMAGE, "name": "agent", "needs": {"cpu_millis": 8000, "memory_bytes": 34359738368}, "runtime": {"agent": {"command": {"argv": ["/usr/local/bin/pumpkin-fake-agent"], "cwd": "/workspace"}, "dirs": [{"mode": "0700", "path": "/workspace/.codex"}, {"mode": "0700", "path": "/workspace/.tmp"}], "egress": {"model": [{"api": "responses", "models": ["gpt-6-astra"]}]}, "files": [{"content": "model = \"gpt-6-astra\"\n", "mode": "0600", "path": "/workspace/.codex/config.toml"}], "finalize_grace_seconds": 60, "grants": [{"job": {"body": train, "call_timeout_seconds": 12000, "calls": 100, "cumulative_seconds": 12000, "payload_max_bytes": 4194304, "target": "trainer"}, "name": "train"}, {"name": "submit", "now": {"body": submit, "payload_max_bytes": 4194304}}], "stdin": "Solve the task in /task/README.md.\n"}}, "user": {"gid": 10001, "uid": 10001}}, {"image": IMAGE, "name": "agent", "needs": {"cpu_millis": 8000, "memory_bytes": 2147483648}, "runtime": {"target": {"cwd": "/workspace", "env": ["MODE=train", "DATASET=/data/train.json"]}}, "user": {"gid": 65532, "uid": 65532}}, {"image": IMAGE, "name": "final_judge", "needs": {"cpu_millis": 8000, "memory_bytes": 2147483648}, "runtime": {"target": {"cwd": "/pumpkin/home", "env": ["MODE=eval", "DATASET=/data/eval.json"]}}, "user": {"gid": 65532, "uid": 65532}}],
                  |                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               ^^^^^^^
                the IR's /roles/1/name
            tasks/task.star:9:674: no role is named `trainer`: name one listed in `roles`
                9 |     roles = [{"image": IMAGE, "name": "agent", "needs": {"cpu_millis": 8000, "memory_bytes": 34359738368}, "runtime": {"agent": {"command": {"argv": ["/usr/local/bin/pumpkin-fake-agent"], "cwd": "/workspace"}, "dirs": [{"mode": "0700", "path": "/workspace/.codex"}, {"mode": "0700", "path": "/workspace/.tmp"}], "egress": {"model": [{"api": "responses", "models": ["gpt-6-astra"]}]}, "files": [{"content": "model = \"gpt-6-astra\"\n", "mode": "0600", "path": "/workspace/.codex/config.toml"}], "finalize_grace_seconds": 60, "grants": [{"job": {"body": train, "call_timeout_seconds": 12000, "calls": 100, "cumulative_seconds": 12000, "payload_max_bytes": 4194304, "target": "trainer"}, "name": "train"}, {"name": "submit", "now": {"body": submit, "payload_max_bytes": 4194304}}], "stdin": "Solve the task in /task/README.md.\n"}}, "user": {"gid": 10001, "uid": 10001}}, {"image": IMAGE, "name": "agent", "needs": {"cpu_millis": 8000, "memory_bytes": 2147483648}, "runtime": {"target": {"cwd": "/workspace", "env": ["MODE=train", "DATASET=/data/train.json"]}}, "user": {"gid": 65532, "uid": 65532}}, {"image": IMAGE, "name": "final_judge", "needs": {"cpu_millis": 8000, "memory_bytes": 2147483648}, "runtime": {"target": {"cwd": "/pumpkin/home", "env": ["MODE=eval", "DATASET=/data/eval.json"]}}, "user": {"gid": 65532, "uid": 65532}}],
                  |                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  ^^^^^^^^^
                the IR's /roles/0/runtime/agent/grants/0/job/target
            tasks/task.star:9:870: `agent` is a target that no job function or final call names: name it from one
                9 |     roles = [{"image": IMAGE, "name": "agent", "needs": {"cpu_millis": 8000, "memory_bytes": 34359738368}, "runtime": {"agent": {"command": {"argv": ["/usr/local/bin/pumpkin-fake-agent"], "cwd": "/workspace"}, "dirs": [{"mode": "0700", "path": "/workspace/.codex"}, {"mode": "0700", "path": "/workspace/.tmp"}], "egress": {"model": [{"api": "responses", "models": ["gpt-6-astra"]}]}, "files": [{"content": "model = \"gpt-6-astra\"\n", "mode": "0600", "path": "/workspace/.codex/config.toml"}], "finalize_grace_seconds": 60, "grants": [{"job": {"body": train, "call_timeout_seconds": 12000, "calls": 100, "cumulative_seconds": 12000, "payload_max_bytes": 4194304, "target": "trainer"}, "name": "train"}, {"name": "submit", "now": {"body": submit, "payload_max_bytes": 4194304}}], "stdin": "Solve the task in /task/README.md.\n"}}, "user": {"gid": 10001, "uid": 10001}}, {"image": IMAGE, "name": "agent", "needs": {"cpu_millis": 8000, "memory_bytes": 2147483648}, "runtime": {"target": {"cwd": "/workspace", "env": ["MODE=train", "DATASET=/data/train.json"]}}, "user": {"gid": 65532, "uid": 65532}}, {"image": IMAGE, "name": "final_judge", "needs": {"cpu_millis": 8000, "memory_bytes": 2147483648}, "runtime": {"target": {"cwd": "/pumpkin/home", "env": ["MODE=eval", "DATASET=/data/eval.json"]}}, "user": {"gid": 65532, "uid": 65532}}],
                  |                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                the IR's /roles/1
            pumpkin: a~b/tasks/task.star: the task has 3 problems
            pumpkin: to run it again: pumpkin run --runs '<scratch>/two
            lines' --config '<scratch>/my served.toml' --sandboxd <scratch>/sandboxd '<scratch>/a~b/tasks/task.star'
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config unserved.toml
            exit status: Some(1)
            pumpkin: unserved.toml: no upstream serves every model that the workflow's route allows, `gpt-6-astra`: list them all under one
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/unserved.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run budgeted/task.star --sandboxd sandboxd --runs runs --config conf/uncached.toml
            exit status: Some(1)
            pumpkin: conf/uncached.toml: `agent` has a model budget, but `gpt-6-astra` has no price for `cached` tokens: price them in its `usd_per_mtok` in the configuration, or remove the budget
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/conf/uncached.toml --sandboxd <scratch>/sandboxd <scratch>/budgeted/task.star
            $ pumpkin run budgeted/task.star --sandboxd sandboxd --runs runs --config conf/unwritten.toml
            exit status: Some(1)
            pumpkin: conf/unwritten.toml: `agent` has a model budget, but `gpt-6-astra` has no price for `cache_write` tokens: price them in its `usd_per_mtok` in the configuration, or remove the budget
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/conf/unwritten.toml --sandboxd <scratch>/sandboxd <scratch>/budgeted/task.star
            $ pumpkin run budgeted/task.star --sandboxd sandboxd --runs runs --config conf/uncosted.toml
            exit status: Some(1)
            pumpkin: conf/uncosted.toml: `agent` has a model budget, but `gpt-6-astra` has no price for `cached` or `cache_write` tokens: price them in its `usd_per_mtok` in the configuration, or remove the budget
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/conf/uncosted.toml --sandboxd <scratch>/sandboxd <scratch>/budgeted/task.star
            $ pumpkin check budgeted/task.star --config unserved.toml
            exit status: Some(1)
            budgeted/task.star:1:1: the file has no docstring: begin it with a string that says what it holds
                1 | load("//lib/train.star", "train")
                  | ^
            budgeted/submit.star:1:1: the file has no docstring: begin it with a string that says what it holds
                1 | def submit(run, payload, finalize: bool = False):
                  | ^
            pumpkin: budgeted/task.star: the task has 2 problems
            pumpkin: budgeted/task.star: unserved.toml: no upstream serves every model that the workflow's route allows, `gpt-6-astra`: list them all under one
            pumpkin: to check it again: pumpkin check --config <scratch>/unserved.toml <scratch>/budgeted/task.star
            $ pumpkin check budgeted/task.star --config conf/served.toml
            exit status: Some(1)
            budgeted/task.star:1:1: the file has no docstring: begin it with a string that says what it holds
                1 | load("//lib/train.star", "train")
                  | ^
            budgeted/submit.star:1:1: the file has no docstring: begin it with a string that says what it holds
                1 | def submit(run, payload, finalize: bool = False):
                  | ^
            pumpkin: budgeted/task.star: the task has 2 problems
            pumpkin: budgeted/task.star: conf/served.toml: `agent` has a model budget, but `gpt-6-astra` has no price for `input`, `cached`, `cache_write` or `output` tokens: price them in its `usd_per_mtok` in the configuration, or remove the budget
            pumpkin: to check it again: pumpkin check --config <scratch>/conf/served.toml <scratch>/budgeted/task.star
            $ pumpkin check budgeted/task.star
            exit status: Some(1)
            budgeted/task.star:1:1: the file has no docstring: begin it with a string that says what it holds
                1 | load("//lib/train.star", "train")
                  | ^
            budgeted/submit.star:1:1: the file has no docstring: begin it with a string that says what it holds
                1 | def submit(run, payload, finalize: bool = False):
                  | ^
            pumpkin: budgeted/task.star: the task has 2 problems
            pumpkin: budgeted/task.star: <scratch>/home/.config/pumpkin/pumpkin.toml: `agent` has a model budget, but `gpt-6-astra` has no price for `cached` or `cache_write` tokens: price them in its `usd_per_mtok` in the configuration, or remove the budget
            pumpkin: to check it again: pumpkin check <scratch>/budgeted/task.star"#]],
    );
    assert!(!scratch.0.join("runs").exists(), "a refusal made a record");
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn configurations() {
    let scratch = Scratch::new();
    scratch.write_task("workflow", &test_support::workflow());
    scratch.write_task("unrouted", &test_support::unrouted());
    write_configurations(&scratch);
    let run = |entry, config: &[&str]| {
        let mut command = scratch.pumpkin(&["run", entry, "--sandboxd", "sandboxd"]);
        command.args(["--runs", "runs"]).args(config);
        command
    };
    let config = |name: &str| run("workflow/task.star", &["--config", &format!("{name}.toml")]);
    let home = run("workflow/task.star", &[]);
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
        "negative",
        "outputless",
        "reasoned",
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
        "mixed",
        "bundled",
        "regional",
        "template-named",
        "group-slashed",
        "repository-cased",
        "unimaged",
        "unspecified",
        "portless",
        "public",
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
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs
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
            pumpkin: undocked.toml: `[docker]` is missing: name it, or `[controllers.<name>]`, as `pumpkin help config` says
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
            unknown field `price`, expected one of `name`, `upstream_name`, `usd_per_mtok`
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
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config negative.toml
            exit status: Some(1)
            pumpkin: negative.toml: TOML parse error at line 7, column 88
              |
            7 | models = [{ name = "gpt-6-astra", upstream_name = "vendor/gpt-6-astra", usd_per_mtok = { input = -1, output = 10 } }]
              |                                                                                        ^^^^^^^^^^^^^^^^^^^^^^^^^^^
            `input` is -1, not a price in US dollars a million tokens: give one from 0 to 1000000, such as 1.25
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/negative.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config outputless.toml
            exit status: Some(1)
            pumpkin: outputless.toml: TOML parse error at line 7, column 88
              |
            7 | models = [{ name = "gpt-6-astra", upstream_name = "vendor/gpt-6-astra", usd_per_mtok = { input = 1.25 } }]
              |                                                                                        ^^^^^^^^^^^^^^^^
            missing field `output`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/outputless.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config reasoned.toml
            exit status: Some(1)
            pumpkin: reasoned.toml: TOML parse error at line 7, column 117
              |
            7 | models = [{ name = "gpt-6-astra", upstream_name = "vendor/gpt-6-astra", usd_per_mtok = { input = 1.25, output = 10, reasoning = 10 } }]
              |                                                                                                                     ^^^^^^^^^
            unknown field `reasoning`, expected one of `input`, `cached`, `cache_write`, `output`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/reasoned.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
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
            the upstream's URL has a query or a fragment: name the API's base URL alone, below which the route reaches each API's endpoint
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/query.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config fragment.toml
            exit status: Some(1)
            pumpkin: fragment.toml: TOML parse error at line 5, column 7
              |
            5 | url = "https://models.example.com/v1#v2"
              |       ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
            the upstream's URL has a query or a fragment: name the API's base URL alone, below which the route reaches each API's endpoint
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/fragment.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config unserved.toml
            exit status: Some(1)
            pumpkin: unserved.toml: no upstream serves every model that the workflow's route allows, `gpt-6-astra`: list them all under one
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/unserved.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config placed.toml
            exit status: Some(1)
            pumpkin: placed.toml: no table of a configuration takes `placement`: remove it, as `pumpkin help config` says what each table takes
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/placed.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config podman.toml
            exit status: Some(1)
            pumpkin: podman.toml: TOML parse error at line 9, column 18
              |
            9 | [providers.local.podman]
              |                  ^^^^^^
            unknown variant `podman`, expected one of `docker`, `ec2`, `modal`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/podman.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config context.toml
            exit status: Some(1)
            pumpkin: context.toml: no table of a configuration takes `providers.local.docker.context`: remove it, as `pumpkin help config` says what each table takes
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
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config mixed.toml
            exit status: Some(1)
            pumpkin: mixed.toml: provider `local` is of kind `docker`, beside `aws`: a machine runs its roles either on its own engine or on other providers, so keep `local` alone, or remove it
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/mixed.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config bundled.toml
            exit status: Some(1)
            pumpkin: bundled.toml: no table of a configuration takes `providers.aws.ec2.repositories.bundles`: remove it, as `pumpkin help config` says what each table takes
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/bundled.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config regional.toml
            exit status: Some(1)
            pumpkin: regional.toml: TOML parse error at line 5, column 10
              |
            5 | region = "us_west_1"
              |          ^^^^^^^^^^^
            `us_west_1` is not an AWS region: use one such as `us-west-1`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/regional.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config template-named.toml
            exit status: Some(1)
            pumpkin: template-named.toml: TOML parse error at line 6, column 34
              |
            6 | classes = { agent = { template = "pumpkin-agent", version = 7, vcpus = 32, memory_mib = 262144 }, judge = { template = "lt-0bbbbbbbbbbbbbbbb", version = 2, vcpus = 8, memory_mib = 16384 } }
              |                                  ^^^^^^^^^^^^^^^
            `pumpkin-agent` is not a launch template's ID: use one such as `lt-0123456789abcdef0`, as `aws ec2 describe-launch-templates` lists it
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/template-named.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config group-slashed.toml
            exit status: Some(1)
            pumpkin: group-slashed.toml: TOML parse error at line 8, column 19
              |
            8 | timer = { group = "pumpkin/timer", role_arn = "arn:aws:iam::123456789012:role/pumpkin-timer" }
              |                   ^^^^^^^^^^^^^^^
            `pumpkin/timer` is not a schedule group's name: use one such as `pumpkin`, of 1 to 64 ASCII letters, digits, `-`, `_` and `.`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/group-slashed.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config repository-cased.toml
            exit status: Some(1)
            pumpkin: repository-cased.toml: TOML parse error at line 7, column 26
              |
            7 | repositories = { roles = "pumpkin/Roles", sandboxd = "pumpkin/sandboxd" }
              |                          ^^^^^^^^^^^^^^^
            `pumpkin/Roles` is not an ECR repository's name: use one such as `pumpkin/roles`, of 2 to 256 bytes: lowercase letters and digits, split by `/`, `.`, `_`, `__` or a run of `-`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/repository-cased.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config unimaged.toml
            exit status: Some(1)
            pumpkin: unimaged.toml: TOML parse error at line 4, column 1
              |
            4 | [providers.gpus.modal]
              | ^^^^^^^^^^^^^^^^^^^^^^
            missing field `images`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/unimaged.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config unspecified.toml
            exit status: Some(1)
            pumpkin: unspecified.toml: TOML parse error at line 5, column 11
              |
            5 | private = "0.0.0.0:7433"
              |           ^^^^^^^^^^^^^^
            the gateway's address 0.0.0.0:7433 is unspecified, which no sandbox can dial: name the address its sandboxes dial
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/unspecified.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config portless.toml
            exit status: Some(1)
            pumpkin: portless.toml: TOML parse error at line 5, column 11
              |
            5 | private = "172.31.0.10"
              |           ^^^^^^^^^^^^^
            `172.31.0.10` is not an IP address and a port: name one such as `172.31.0.10:7433`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/portless.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ pumpkin run workflow/task.star --sandboxd sandboxd --runs runs --config public.toml
            exit status: Some(1)
            pumpkin: public.toml: TOML parse error at line 5, column 1
              |
            5 | public = "203.0.113.1:7433"
              | ^^^^^^
            unknown field `public`, expected `private`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/public.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
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
            pumpkin: the Docker context `nowhere` can't be read: `docker context inspect` didn't start: No such file or directory (os error 2): install `docker` on `PATH`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/conf/served.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            $ PATH=<scratch>/nowhere pumpkin run unrouted/task.star --sandboxd sandboxd --runs runs --config docked.toml
            exit status: Some(1)
            pumpkin: the Docker context `nowhere` can't be read: `docker context inspect` didn't start: No such file or directory (os error 2): install `docker` on `PATH`
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/docked.toml --sandboxd <scratch>/sandboxd <scratch>/unrouted/task.star"#]],
    );
}

#[test]
fn open_files_raised() {
    let scratch = Scratch::new();
    scratch.write_task("workflow", &test_support::workflow());
    write_configurations(&scratch);
    // A `docker` that records whether the driver, which runs it, holds its soft limit at its hard.
    let docker = "#!/bin/sh\nwhile read -r a b c soft hard unit; do\n\
                  [ \"$a $b $c $soft\" = \"Max open files $hard\" ] && : > \"$0.raised\"\n\
                  done < /proc/$PPID/limits\nexit 1\n";
    scratch.write_executable("bin/docker", docker.as_bytes());
    // Started at the soft limit that systemd gives a login or a unit.
    let script = "ulimit -S -n 1024 && exec \"$0\" run workflow/task.star --sandboxd sandboxd \
                  --runs runs --config conf/served.toml";
    let pumpkin = scratch.0.join(".pumpkin/bin/pumpkin");
    let mut run = scratch.program("/bin/sh", &["-c", script, pumpkin.to_str().unwrap()]);
    run.env("PATH", scratch.0.join("bin"));
    let ran = ran(run, &scratch.0, false);
    let raised = scratch.0.join("bin/docker.raised").exists();
    let raised = format!("the driver's soft limit is its hard limit: {raised}");
    check(
        &scratch,
        [ran, raised].map(Step::Ran),
        false,
        &[],
        &expect![[r#"
            $ PATH=<scratch>/bin /bin/sh -c ulimit -S -n 1024 && exec "$0" run workflow/task.star --sandboxd sandboxd --runs runs --config conf/served.toml <scratch>/.pumpkin/bin/pumpkin
            exit status: Some(1)
            pumpkin: the Docker context `nowhere` can't be read: `docker context inspect` failed with exit status: 1 and wrote nothing to standard error: name one that `docker context ls` lists
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/conf/served.toml --sandboxd <scratch>/sandboxd <scratch>/workflow/task.star
            the driver's soft limit is its hard limit: true"#]],
    );
}

/// Writes into `scratch` the `sandboxd` that [`configurations`] and [`refusals`] give `run`, the
/// key `conf/key`, and a machine configuration `<name>.toml` for each name they run: `conf/served`
/// and `docked`, which are accepted, and each other, which breaks the rule that its name says.
/// Each but `undocked`, which names no Docker context, ends with [`LOCAL`]'s tables, changed as
/// its name says.
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the configurations, a line or a few each"
)]
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
    let priced =
        |prices: &str| gateway.replace(" }", &format!(", usd_per_mtok = {{ {prices} }} }}"));
    scratch.write_sandboxd("sandboxd");
    scratch.write("conf/key", "sk-test\n");
    let configs = [
        (
            "conf/served",
            format!(
                "{gateway}\n{}",
                direct.replace(" }", ", usd_per_mtok = { input = 0.3, output = 2.5 } }")
            ),
        ),
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
        ("negative", priced("input = -1, output = 10")),
        ("outputless", priced("input = 1.25")),
        (
            "conf/uncached",
            priced("input = 1.25, cache_write = 1.5, output = 10"),
        ),
        (
            "conf/unwritten",
            priced("input = 1.25, cached = 0.125, output = 10"),
        ),
        ("conf/uncosted", priced("input = 1.25, output = 10")),
        (
            "reasoned",
            priced("input = 1.25, output = 10, reasoning = 10"),
        ),
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
        ("mixed", EC2.to_owned()),
        (
            "bundled",
            EC2.replace(
                " sandboxd = \"pumpkin/sandboxd\" }",
                " sandboxd = \"pumpkin/sandboxd\", bundles = \"pumpkin/bundles\" }",
            ),
        ),
        ("regional", EC2.replace("\"us-west-1\"", "\"us_west_1\"")),
        (
            "template-named",
            EC2.replace("\"lt-0aaaaaaaaaaaaaaaa\"", "\"pumpkin-agent\""),
        ),
        (
            "group-slashed",
            EC2.replace("group = \"pumpkin\"", "group = \"pumpkin/timer\""),
        ),
        (
            "repository-cased",
            EC2.replace("\"pumpkin/roles\"", "\"pumpkin/Roles\""),
        ),
        (
            "unimaged",
            "[providers.gpus.modal]\nprofile = \"pumpkin\"\ndials_from = \"203.0.113.7\"\n\n\
             [providers.gpus.modal.classes.h100]\ngpu = \"H100!\"\n\
             gpu_model = \"NVIDIA H100 80GB HBM3\"\ncpu_millis = 4000\n\
             memory_bytes = 17179869184\nstartup_seconds = 1800\n"
                .to_owned(),
        ),
        (
            "unspecified",
            "[gateway]\nprivate = \"0.0.0.0:7433\"\n".to_owned(),
        ),
        (
            "portless",
            "[gateway]\nprivate = \"172.31.0.10\"\n".to_owned(),
        ),
        (
            "public",
            "[gateway]\npublic = \"203.0.113.1:7433\"\n".to_owned(),
        ),
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
/// `vendor/gpt-6-astra`, with a price, and with its key in `conf/key`.
fn write_asks(scratch: &Scratch, context: &str, address: &str) {
    let config = format!(
        r#"{}
[upstreams.provider]
url = "http://{address}/v1/"
key_file = "key"
models = [{{ name = "gpt-6-astra", upstream_name = "vendor/gpt-6-astra", usd_per_mtok = {{ input = 1.25, output = 10 }} }}]
"#,
        machine(context)
    );
    scratch.write("conf/pumpkin.toml", &config);
    fs::write(scratch.0.join("conf/key"), "sk-test\n").unwrap();
    let body = r#"{"model": "gpt-6-astra", "store": false, "input": "hi"}"#;
    let ask = format!(
        "wget --quiet --output-document=/pumpkin/out/answer --header 'content-type: application/json' \
         --post-data '{body}' http://127.0.0.1:{MODEL_PORT}/responses && printf six > six && \
         {SANDBOXCTL_PATH} call submit six --arg finalize=true"
    );
    let mut asks = workflow("", &["sh", "-c", &ask]);
    asks["roles"][0]["runtime"]["agent"]["egress"] =
        test_support::workflow()["roles"][0]["runtime"]["agent"]["egress"].clone();
    let body = r#"{"model": "gpt-6-astra", "messages": [{"role": "user", "content": "grade"}]}"#;
    let grade = format!(
        "wget --quiet --output-document=/pumpkin/out/answer \
         --header 'content-type: application/json' --post-data '{body}' \
         http://127.0.0.1:{MODEL_PORT}/v1/chat/completions && \
         printf '{{\"raw_score\": 1}}' > /pumpkin/out/result.json"
    );
    asks["roles"][2]["runtime"]["target"]["egress"] =
        json!({ "model": [{ "api": "chat", "models": ["gpt-6-astra"] }] });
    scratch.write_task("asks", &asks);
    scratch.write("asks/final.star", &final_running(&grade));
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
                let (line, headers, body) = read_request(&mut BufReader::new(&connection));
                // The connection that `stop` makes holds no request.
                if line.is_empty() {
                    return;
                }
                let authorization = headers.iter().find(|(name, _)| name == "authorization");
                let authorization = authorization.map(|(_, value)| value);
                let body = String::from_utf8(body).unwrap();
                let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                            transfer-encoding: chunked\r\nconnection: close\r\n\r\n";
                connection.write_all(head.as_bytes()).unwrap();
                write_chunk(
                    &mut connection,
                    &format!("data: {line} {authorization:?} {body}\n\n"),
                );
                write_chunk(&mut connection, "");
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
#[ignore = "needs Docker: run cargo xtask live docker"]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn runs_on_docker() {
    let scratch = Scratch::new();
    let sandboxd = sandboxd().into_os_string().into_string().unwrap();
    let context = docker_context();
    let engine = engine(&scratch, &context);
    let upstream = Upstream::start();
    write_asks(&scratch, &context, &upstream.address);
    scratch.write("home/.config/pumpkin/pumpkin.toml", &machine(&context));
    scratch.write_task("exits", &workflow("", &["true"]));
    let submit =
        format!("printf six > six && {SANDBOXCTL_PATH} call submit six --arg finalize=true");
    scratch.write_task("submits", &workflow("", &["sh", "-c", &submit]));
    scratch.write_task("fails", &workflow("", &["sh", "-c", &submit]));
    scratch.write("fails/final.star", &final_running("exit 1"));
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
    exits.env("DOCKER_CONFIG", &docker_config);
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
            exit status: Some(0)
            pumpkin: run <id> records into <scratch>/runs/<id>
            pumpkin: run <id> started
            pumpkin: following run <id>, recorded in <scratch>/runs/<id>: ending `pumpkin run` does not stop it
            pumpkin:    <t> agent started on local/container
            pumpkin:    <t> the work phase ended
            pumpkin:    <t> final call job 0 asked on local/container
            pumpkin:    <t> final call job 0 started
            pumpkin:    <t> final call job 0 ended: score=1
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container, started at <t>, exited 0 at <t>; its work ended at <t>
                        model: 1 request, all answered 200; dropped request headers: user-agent; no response reported its tokens; of 1 good answer, by the median, <t> to the end
                 <t> final call job 0 on local/container, started after <t>, ran <t>: score=1
                        model: 1 request, all answered 200; no response reported its tokens; of 1 good answer, by the median, <t> to the end
              model: 2 requests, all answered 200; dropped request headers: user-agent; no response reported its tokens; of 2 good answers, by the median, <t> to the end
              sandboxes: 2 on local/container, <t> in all, 2 created, all deleted
            pumpkin: run <id> finished with a result (score=1); its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/conf/pumpkin.toml --sandboxd <sandboxd> <scratch>/asks/task.star
            record: agents/agent/log/command.stderr, agents/agent/log/command.stdout, agents/agent/out/answer, agents/agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/got/1/result.json, jobs/0/log/0/command.stderr, jobs/0/log/0/command.stdout, jobs/0/out/answer, jobs/0/out/result.json, jobs/0/sandbox.json, log.jsonl, model/0.request, model/0.response, model/1.request, model/1.response, model/items/06651be039f9e5a47490f8ea118fd0931e7824702c693557a77a7df2d5114aa4, outcome.json, payloads/44778d82365e4af681c40d5f0eef5cf6f5899d3f0ac335050a7ed6779cf3f674, provider.json, record.json, sources/task/asks/final.star, sources/task/asks/submit.star, sources/task/asks/task.star, sources/task/lib/train.star
            agent `agent`'s sandbox, agents/agent/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local"}
            job 0's sandbox, jobs/0/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local"}
            provider.json routes: {"agent":[{"api":"responses","models":{"gpt-6-astra":{"upstream_name":"vendor/gpt-6-astra","usd_per_mtok":{"input":1.25,"output":10.0}}},"upstream":"provider"}],"final_judge":[{"api":"chat","models":{"gpt-6-astra":{"upstream_name":"vendor/gpt-6-astra","usd_per_mtok":{"input":1.25,"output":10.0}}},"upstream":"provider"}]}
            the run log records: {"answer":{"kind":"forwarded"},"api":"responses","caller":"agent-0","first_byte_ns":"<ns>","kept":{"request":{"body_bytes":57,"body_sha256":"8e1b803c11e5437c46a8ea56e92d591b284778ce8209d56a347f8c8fdc0ca260","bytes":57,"dropped_headers":["user-agent"]},"response":{"bytes":116}},"kind":"model_answered","model":"gpt-6-astra","request":0,"stream":false,"upstream":"provider","upstream_status":200}
            the run log records: {"answer":{"kind":"forwarded"},"api":"chat","caller":"job-0","first_byte_ns":"<ns>","kept":{"request":{"body_bytes":77,"body_sha256":"018fa791e56a2db8c8c065da57294d744c30e14181cf2fdbfd272d3e8f009efd","bytes":117,"items":{"count":1,"field":"messages"}},"response":{"bytes":143}},"kind":"model_answered","model":"gpt-6-astra","request":1,"stream":false,"upstream":"provider","upstream_status":200}
            the agent's answer: data: POST /v1/responses HTTP/1.1 Some("Bearer sk-test") {"input":"hi","model":"vendor/gpt-6-astra","store":false}\n\n
            the final call's answer: data: POST /v1/chat/completions HTTP/1.1 Some("Bearer sk-test") {"messages":[{"content":"grade","role":"user"}],"model":"vendor/gpt-6-astra"}\n\n
            outcome.json: {"agents":[{"role":"agent","submission":{"finalized":true,"payload":{"digest":"sha256:44778d82365e4af681c40d5f0eef5cf6f5899d3f0ac335050a7ed6779cf3f674","size":3}},"work_end":{"kind":"agent_exited","status":{"code":0}}}],"state":"finished","usage":{"calls":[{"grant":null,"job":0,"model_requests":1,"tokens":{"input":0,"output":0,"responses":0}}],"model_requests":2,"ran_ns":"<ns>","sandboxes":[{"class":"container","count":2,"lasted_ns":"<ns>","provider":"local"}],"tokens":{"input":0,"output":0,"responses":0}},"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":1}]}}}
            $ DOCKER_CONFIG=<docker config> pumpkin run exits/task.star --sandboxd <sandboxd> --wait
            exit status: Some(0)
            pumpkin: run <id> records into <scratch>/.pumpkin/runs/<id>
            pumpkin: run <id> started
            pumpkin: following run <id>, recorded in <scratch>/.pumpkin/runs/<id>: ending `pumpkin run` does not stop it
            pumpkin:    <t> agent started on local/container
            pumpkin:    <t> the work phase ended
            pumpkin:    <t> final call job 0 asked on local/container
            pumpkin:    <t> final call job 0 started
            pumpkin:    <t> final call job 0 ended: score=0
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container, started at <t>, exited 0 at <t>; its work ended at <t>
                 <t> final call job 0 on local/container, started after <t>, ran <t>: score=0
              sandboxes: 2 on local/container, <t> in all, 2 created, all deleted
            pumpkin: run <id> finished with a result (score=0); its record is <scratch>/.pumpkin/runs/<id>
            pumpkin: to run it again: pumpkin run --sandboxd <sandboxd> <scratch>/exits/task.star
            record: agents/agent/log/command.stderr, agents/agent/log/command.stdout, agents/agent/out/, agents/agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/out/, jobs/0/sandbox.json, log.jsonl, outcome.json, payloads/, provider.json, record.json, sources/task/exits/final.star, sources/task/exits/submit.star, sources/task/exits/task.star, sources/task/lib/train.star
            agent `agent`'s sandbox, agents/agent/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local"}
            job 0's sandbox, jobs/0/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local"}
            outcome.json: {"agents":[{"role":"agent","submission":null,"work_end":{"kind":"agent_exited","status":{"code":0}}}],"state":"finished","usage":{"model_requests":0,"ran_ns":"<ns>","sandboxes":[{"class":"container","count":2,"lasted_ns":"<ns>","provider":"local"}],"tokens":{"input":0,"output":0,"responses":0}},"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":0}]}}}
            $ pumpkin run submits/task.star --sandboxd <sandboxd> --wait --runs runs --config conf/pumpkin.toml
            exit status: Some(0)
            pumpkin: run <id> records into <scratch>/runs/<id>
            pumpkin: run <id> started
            pumpkin: following run <id>, recorded in <scratch>/runs/<id>: ending `pumpkin run` does not stop it
            pumpkin:    <t> agent started on local/container
            pumpkin:    <t> the work phase ended
            pumpkin:    <t> final call job 0 asked on local/container
            pumpkin:    <t> final call job 0 started
            pumpkin:    <t> final call job 0 ended: score=3
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container, started at <t>, exited 0 at <t>; its work ended at <t>
                 <t> final call job 0 on local/container, started after <t>, ran <t>: score=3
              sandboxes: 2 on local/container, <t> in all, 2 created, all deleted
            pumpkin: run <id> finished with a result (score=3); its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/conf/pumpkin.toml --sandboxd <sandboxd> <scratch>/submits/task.star
            record: agents/agent/log/command.stderr, agents/agent/log/command.stdout, agents/agent/out/, agents/agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/got/1/result.json, jobs/0/log/0/command.stderr, jobs/0/log/0/command.stdout, jobs/0/out/result.json, jobs/0/sandbox.json, log.jsonl, outcome.json, payloads/44778d82365e4af681c40d5f0eef5cf6f5899d3f0ac335050a7ed6779cf3f674, provider.json, record.json, sources/task/lib/train.star, sources/task/submits/final.star, sources/task/submits/submit.star, sources/task/submits/task.star
            agent `agent`'s sandbox, agents/agent/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local"}
            job 0's sandbox, jobs/0/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local"}
            outcome.json: {"agents":[{"role":"agent","submission":{"finalized":true,"payload":{"digest":"sha256:44778d82365e4af681c40d5f0eef5cf6f5899d3f0ac335050a7ed6779cf3f674","size":3}},"work_end":{"kind":"agent_exited","status":{"code":0}}}],"state":"finished","usage":{"model_requests":0,"ran_ns":"<ns>","sandboxes":[{"class":"container","count":2,"lasted_ns":"<ns>","provider":"local"}],"tokens":{"input":0,"output":0,"responses":0}},"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":3}]}}}
            $ pumpkin run fails/task.star --sandboxd <sandboxd> --wait --runs runs --config conf/pumpkin.toml
            exit status: Some(3)
            pumpkin: run <id> records into <scratch>/runs/<id>
            pumpkin: run <id> started
            pumpkin: following run <id>, recorded in <scratch>/runs/<id>: ending `pumpkin run` does not stop it
            pumpkin:    <t> agent started on local/container
            pumpkin:    <t> the work phase ended
            pumpkin:    <t> final call job 0 asked on local/container
            pumpkin:    <t> final call job 0 started
            pumpkin:    <t> final call job 0 ended: failed: fails/final.star:18:34: getting /pumpkin/out/result.json failed: /pumpkin/out/result.json: No such file or directory (os error 2)\n    18 |     return {"score": json.decode(target.get("/pumpkin/out/result.json"))["raw_score"]}\n       |                                  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^\n    in `final_call()`, called at fails/final.star:3:5
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container, started at <t>, exited 0 at <t>; its work ended at <t>
                 <t> final call job 0 on local/container, started after <t>, ran <t>: failed: fails/final.star:18:34: getting /pumpkin/out/result.json failed: /pumpkin/out/result.json: No such file or directory (os error 2)\n    18 |     return {"score": json.decode(target.get("/pumpkin/out/result.json"))["raw_score"]}\n       |                                  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^\n    in `final_call()`, called at fails/final.star:3:5
              sandboxes: 2 on local/container, <t> in all, 2 created, all deleted
            pumpkin: run <id> finished without a result: the final call gave none (error: fails/final.star:18:34: getting /pumpkin/out/result.json failed: /pumpkin/out/result.json: No such file or directory (os error 2)
                18 |     return {"score": json.decode(target.get("/pumpkin/out/result.json"))["raw_score"]}
                   |                                  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `final_call()`, called at fails/final.star:3:5); its commands' logs are in <scratch>/runs/<id>/jobs/0/log; its record is <scratch>/runs/<id>
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/conf/pumpkin.toml --sandboxd <sandboxd> <scratch>/fails/task.star
            record: agents/agent/log/command.stderr, agents/agent/log/command.stdout, agents/agent/out/, agents/agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/got/1/, jobs/0/log/0/command.stderr, jobs/0/log/0/command.stdout, jobs/0/out/, jobs/0/sandbox.json, log.jsonl, outcome.json, payloads/44778d82365e4af681c40d5f0eef5cf6f5899d3f0ac335050a7ed6779cf3f674, provider.json, record.json, sources/task/fails/final.star, sources/task/fails/submit.star, sources/task/fails/task.star, sources/task/lib/train.star
            agent `agent`'s sandbox, agents/agent/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local"}
            job 0's sandbox, jobs/0/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local"}
            outcome.json: {"agents":[{"role":"agent","submission":{"finalized":true,"payload":{"digest":"sha256:44778d82365e4af681c40d5f0eef5cf6f5899d3f0ac335050a7ed6779cf3f674","size":3}},"work_end":{"kind":"agent_exited","status":{"code":0}}}],"state":"finished","usage":{"model_requests":0,"ran_ns":"<ns>","sandboxes":[{"class":"container","count":2,"lasted_ns":"<ns>","provider":"local"}],"tokens":{"input":0,"output":0,"responses":0}},"verdict":{"failure":{"kind":"error","reason":"fails/final.star:18:34: getting /pumpkin/out/result.json failed: /pumpkin/out/result.json: No such file or directory (os error 2)\n    18 |     return {\"score\": json.decode(target.get(\"/pumpkin/out/result.json\"))[\"raw_score\"]}\n       |                                  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^\n    in `final_call()`, called at fails/final.star:3:5"},"kind":"no_result"}}"#]],
    );
    upstream.stop();
    wait_for_each_run(&scratch, &sandboxd);
}

#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the steps and their snapshot"
)]
fn writes_only_in_its_workspace() {
    let scratch = Scratch::new();
    let label = format!("pumpkin.test=workspace-{}", std::process::id());
    let _images = Labelled(label.clone());
    write_hello(&scratch, &label);
    fs::hard_link(sandboxd(), scratch.0.join(".pumpkin/bin/sandboxd")).unwrap();
    let context = docker_context();
    scratch.write("pumpkin.toml", &machine(&context));
    let home = test_home(&scratch, &context);
    let temp = scratch.0.join("tmp");
    fs::create_dir(&temp).unwrap();
    let before = files(&home);
    let config = ["--config", "pumpkin.toml"];
    let mut run = at_home(
        &scratch,
        &home,
        &temp,
        &[&["run", HELLO, "--wait"], &config[..]].concat(),
    );
    let invoked = invocation(&run);
    let rootful = !engine(&scratch, &context).report().security.rootless;
    let output = with_room(&mut run);
    let written: Vec<String> = files(&home)
        .into_iter()
        .filter(|file| !before.contains(file))
        .collect();
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    let (_, dir) = record(&stderr).unwrap();
    let mut lines = vec![
        report(invoked, &output, &scratch.0, rootful),
        format!("HOME's new files: {written:?}"),
        format!("TMPDIR holds: [{}]", held(&temp)),
        format!(".pumpkin holds: {}", held(&scratch.0.join(".pumpkin"))),
        format!(
            ".pumpkin/docker holds: {}",
            held(&scratch.0.join(".pumpkin/docker"))
        ),
    ];
    lines.extend(
        standard_outputs(&scratch.0.join(dir))
            .into_iter()
            .map(remaining),
    );
    // So that the scratch directory can be removed.
    fs::set_permissions(home.join(".docker"), Permissions::from_mode(0o755)).unwrap();
    let shown = lines
        .join("\n")
        .replace(scratch.0.to_str().unwrap(), "<scratch>");
    expect![[r#"
        $ env --unset=DOCKER_CONFIG TMPDIR=<scratch>/tmp env --unset=XDG_CACHE_HOME env --unset=XDG_CONFIG_HOME env --unset=XDG_DATA_HOME pumpkin run examples/hello/task.star --wait --config pumpkin.toml
        exit status: Some(0)
        pumpkin: run <id> records into <scratch>/.pumpkin/runs/<id>
        pumpkin: run <id> started
        pumpkin: following run <id>, recorded in <scratch>/.pumpkin/runs/<id>: ending `pumpkin run` does not stop it
        pumpkin:    <t> agent started on local/container
        pumpkin:    <t> count job 0 of agent 0 asked on local/container
        pumpkin:    <t> count job 0 of agent 0 started
        pumpkin:    <t> count job 0 of agent 0 ended: lines=1 bytes=6
        pumpkin:    <t> the work phase ended
        pumpkin:    <t> final call job 1 asked on local/container
        pumpkin:    <t> final call job 1 started
        pumpkin:    <t> final call job 1 ended: score=6 lines=1
        pumpkin: run <id> started <t> after it was asked for, and ran <t>:
             <t> agent on local/container, started at <t>, exited 0 at <t>; its work ended at <t>
             <t> count job 0 of agent 0 on local/container, started after <t>, ran <t>: lines=1 bytes=6
             <t> final call job 1 on local/container, started after <t>, ran <t>: score=6 lines=1
          sandboxes: 3 on local/container, <t> in all, 3 created, all deleted
        pumpkin: run <id> finished with a result (score=6 lines=1); its record is <scratch>/.pumpkin/runs/<id>
        pumpkin: to run it again: pumpkin run --config <scratch>/pumpkin.toml <scratch>/examples/hello/task.star
        record: agents/agent/log/command.stderr, agents/agent/log/command.stdout, agents/agent/out/, agents/agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/got/1/counts.json, jobs/0/log/0/command.stderr, jobs/0/log/0/command.stdout, jobs/0/out/, jobs/0/sandbox.json, jobs/1/got/1/result.json, jobs/1/log/0/command.stderr, jobs/1/log/0/command.stdout, jobs/1/out/result.json, jobs/1/sandbox.json, log.jsonl, outcome.json, payloads/5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03, provider.json, record.json, sources/task/hello/count.star, sources/task/hello/final.star, sources/task/hello/task.star, sources/task/submit.star
        agent `agent`'s sandbox, agents/agent/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local"}
        job 0's sandbox, jobs/0/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local"}
        job 1's sandbox, jobs/1/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local"}
        outcome.json: {"agents":[{"role":"agent","submission":{"finalized":true,"payload":{"digest":"sha256:5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03","size":6}},"work_end":{"kind":"agent_exited","status":{"code":0}}}],"state":"finished","usage":{"model_requests":0,"ran_ns":"<ns>","sandboxes":[{"class":"container","count":3,"lasted_ns":"<ns>","provider":"local"}],"tokens":{"input":0,"output":0,"responses":0}},"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":6},{"name":"lines","value":1}]}}}
        HOME's new files: []
        TMPDIR holds: []
        .pumpkin holds: bin, docker, runs
        .pumpkin/docker holds: buildx, publish-slots
        agent `agent`'s standard output:
        {"kind":"call","job":0,"remaining":{"calls":null,"seconds":null}}
        {"kind":"status","job":0,"state":"completed","returned":{"bytes":6,"lines":1},"remaining":{"calls":null,"seconds":null}}
        {"kind":"call","returned":{"state":"accepted"},"remaining_seconds":"<s>"}
        job 0's command 0's standard output:
        job 1's command 0's standard output:"#]].assert_eq(&shown);
}

#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
#[expect(clippy::too_many_lines, reason = "most of it is the snapshot")]
fn unbuilt_on_docker() {
    let scratch = Scratch::new();
    let sandboxd = sandboxd().into_os_string().into_string().unwrap();
    let label = format!("pumpkin.test=unbuilt-{}", std::process::id());
    let _images = Labelled(label.clone());
    let mut ir = workflow("", &["true"]);
    ir["roles"][2]["image"] = json!(RUNNER);
    scratch.write_task("unbuilt", &ir);
    let failing = format!(
        "FROM {BUSYBOX}\nLABEL {label}\nRUN echo failing && echo 'error: the step failed' >&2 && false\n"
    );
    for recipe in ["unbuilt/Dockerfile", "unbuilt/runner/Dockerfile"] {
        scratch.write(recipe, &failing);
    }
    let context = docker_context();
    scratch.write("pumpkin.toml", &machine(&context));
    let rootful = !engine(&scratch, &context).report().security.rootless;
    let mut run = scratch.pumpkin(&["run", "unbuilt/task.star", "--sandboxd", &sandboxd]);
    run.args(["--runs", "runs", "--config", "pumpkin.toml"]);
    let invoked = invocation(&run);
    let output = run.output().unwrap();
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    // Three kinds of line: `pumpkin`'s and the problems said once the builds stopped, the lines
    // that say each build started or ended, in any order, and what each `docker build` wrote.
    let (mut said, mut builds, mut written) = (Vec::new(), Vec::new(), Vec::new());
    let mut problems = false;
    for line in stderr.lines() {
        problems |= line.starts_with("unbuilt/");
        let build = [
            "pumpkin: building an image in ",
            "pumpkin: the build of an image in ",
        ];
        if build.iter().any(|build| line.starts_with(build)) {
            builds.push(line);
        } else if problems || line.starts_with("pumpkin: ") {
            said.push(line);
        } else {
            written.push(line);
        }
    }
    builds.sort_unstable();
    let output = Output {
        stderr: said.join("\n").into(),
        ..output
    };
    let echoed = written.iter().any(|line| line.contains("RUN echo failing"));
    let failed = (written.iter()).any(|line| line.ends_with("error: the step failed"));
    let lines = [
        report(invoked, &output, &scratch.0, rootful),
        format!("the builds said: {builds:#?}"),
        format!("what `docker build` wrote is said: its step {echoed}, its error {failed}"),
        format!("a record was made: {}", scratch.0.join("runs").exists()),
    ];
    check(
        &scratch,
        lines.map(Step::Ran),
        rootful,
        &[(&sandboxd[..], "<sandboxd>")],
        &expect![[r#"
            $ pumpkin run unbuilt/task.star --sandboxd <sandboxd> --runs runs --config pumpkin.toml
            exit status: Some(1)
            unbuilt/task.star:5:9: building the image failed: `docker build` of `Dockerfile` failed with exit status: 1: its output above says why: fix what Docker reports
                5 | IMAGE = image("Dockerfile")
                  |         ^^^^^^^^^^^^^^^^^^^
            unbuilt/task.star:6:10: building the image failed: `docker build` of `runner/Dockerfile` failed with exit status: 1: its output above says why: fix what Docker reports
                6 | RUNNER = image("runner/Dockerfile")
                  |          ^^^^^^^^^^^^^^^^^^^^^^^^^^
            pumpkin: unbuilt/task.star: the task has 2 problems
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <sandboxd> <scratch>/unbuilt/task.star
            the builds said: [
                "pumpkin: building an image in pumpkin/unbuilt",
                "pumpkin: building an image in pumpkin/unbuilt/runner",
                "pumpkin: the build of an image in pumpkin/unbuilt failed",
                "pumpkin: the build of an image in pumpkin/unbuilt/runner failed",
            ]
            what `docker build` wrote is said: its step true, its error true
            a record was made: false"#]],
    );
}

#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
fn unplaced_provider_unreached_on_docker() {
    let scratch = Scratch::new();
    let sandboxd = sandboxd().into_os_string().into_string().unwrap();
    let label = format!("pumpkin.test=unplaced-{}", std::process::id());
    let _images = Labelled(label.clone());
    scratch.write_task("unbuilt", &workflow("", &["true"]));
    let failing = format!("FROM {BUSYBOX}\nLABEL {label}\nRUN false\n");
    scratch.write("unbuilt/Dockerfile", &failing);
    let context = docker_context();
    // `a100`'s one class has GPUs, which no role needs, and it sorts before `aws`, which the AWS
    // guard keeps unreachable, so a run that reached every provider would reach `a100` first.
    let config = format!("{}\n{EC2}\n{A100}", docker_table(&context));
    scratch.write("pumpkin.toml", &config);
    let rootful = !engine(&scratch, &context).report().security.rootless;
    let mut run = scratch.pumpkin(&["run", "unbuilt/task.star", "--sandboxd", &sandboxd]);
    run.args(["--runs", "runs", "--config", "pumpkin.toml"]);
    let lines = [
        ran(run, &scratch.0, rootful),
        format!("a record was made: {}", scratch.0.join("runs").exists()),
    ];
    check(
        &scratch,
        lines.map(Step::Ran),
        rootful,
        &[(&sandboxd[..], "<sandboxd>")],
        &expect![[r#"
            $ pumpkin run unbuilt/task.star --sandboxd <sandboxd> --runs runs --config pumpkin.toml
            exit status: Some(1)
            pumpkin: reaching provider `aws` failed: `aws --version` failed with exit status: 1 and wrote nothing to standard error: expected it to exit with 0
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <sandboxd> <scratch>/unbuilt/task.star
            a record was made: false"#]],
    );
}

// Each role that fits no class is refused before any build, with what each class lacks and the
// changes that would place it. A pipe gets it plain, and a terminal styled.
#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
fn misplaced_on_docker() {
    let scratch = Scratch::new();
    let sandboxd = sandboxd().into_os_string().into_string().unwrap();
    let mut ir = workflow("", &["true"]);
    let h100 = json!({ "count": 1, "model": "NVIDIA H100 80GB HBM3" });
    ir["roles"][0]["needs"] =
        json!({ "cpu_millis": 8000, "memory_bytes": 32_u64 << 30, "gpu": h100 });
    ir["roles"][1]["needs"] = json!({ "cpu_millis": 64_000, "memory_bytes": 2_u64 << 30 });
    scratch.write_task("misfit", &ir);
    let context = docker_context();
    scratch.write(
        "pumpkin.toml",
        &format!("{}\n{EC2}\n{A100}", docker_table(&context)),
    );
    let rootful = !engine(&scratch, &context).report().security.rootless;
    let run = || {
        let mut run = scratch.pumpkin(&["run", "misfit/task.star", "--sandboxd", &sandboxd]);
        run.args(["--runs", "runs", "--config", "pumpkin.toml"]);
        run
    };
    let shown = [(&sandboxd[..], "<sandboxd>")];
    check(
        &scratch,
        [run()],
        rootful,
        &shown,
        &expect![[r#"
            $ pumpkin run misfit/task.star --sandboxd <sandboxd> --runs runs --config pumpkin.toml
            exit status: Some(1)
            pumpkin: error: role `agent` fits no class of sandbox
              --> misfit/task.star:9:57: roles[0].needs: 8 CPUs, 32.0 GiB, 1 × `NVIDIA H100 80GB HBM3`
              = `a100/a100`: 1 × `NVIDIA A100-SXM4-80GB`
              = `aws/agent`: no GPU
              = `aws/judge`: 13.4 GiB, no GPU
              = help: add a class with 1 × `NVIDIA H100 80GB HBM3`, and at least 8 CPUs and 32.0 GiB,
                      or change the role's `gpu` to 1 × `NVIDIA A100-SXM4-80GB`, which `a100/a100` has
            pumpkin: error: role `trainer` fits no class of sandbox
              --> misfit/task.star:9:869: roles[1].needs: 64 CPUs, 2.0 GiB
              = `a100/a100`: 8 CPUs, 1 × `NVIDIA A100-SXM4-80GB` (only for roles that need it)
              = `aws/agent`: 32 CPUs
              = `aws/judge`: 8 CPUs
              = help: add a class with at least 64 CPUs and 2.0 GiB,
                      or lower the role's `cpu_millis` to 32000, which `aws/agent` offers
            pumpkin: misfit/task.star: placing the workflow's roles in the classes that pumpkin.toml's providers offer has 2 problems
            pumpkin: to read how a configuration offers a class: pumpkin help config
            pumpkin: to run it again: pumpkin run --runs <scratch>/runs --config <scratch>/pumpkin.toml --sandboxd <sandboxd> <scratch>/misfit/task.star"#]],
    );
    let mut styled = run();
    styled.env("CLICOLOR_FORCE", "1");
    let output = styled.output().unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    let said = (stderr.lines()).filter(|line| !line.contains(" runs as root, "));
    let said: String = said.flat_map(|line| [line, "\n"]).collect();
    let said =
        (said.replace(scratch.0.to_str().unwrap(), "<scratch>")).replace(&sandboxd, "<sandboxd>");
    let svg = anstyle_svg::Term::new().render_svg(&said);
    expect_file!["run/misplaced.svg"].assert_eq(&svg);
}

/// A Modal provider whose one class has an A100 GPU.
const A100: &str = r#"[providers.a100.modal]
profile = "pumpkin"
images = { region = "us-west-1", repository = "pumpkin/roles", role_arn = "arn:aws:iam::123456789012:role/pumpkin-modal-pull" }
dials_from = "203.0.113.7"
classes.a100 = { gpu = "A100-80GB", gpu_model = "NVIDIA A100-SXM4-80GB", cpu_millis = 8000, memory_bytes = 85899345920, startup_seconds = 1800 }
"#;

/// Writes `examples/hello` into `scratch`, with the marker of its root, and with `label` on its
/// image.
fn write_hello(scratch: &Scratch, label: &str) {
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    for file in [
        "PUMPKIN.star",
        "submit.star",
        "hello/task.star",
        "hello/count.star",
        "hello/final.star",
        "hello/Dockerfile",
    ] {
        let text = fs::read_to_string(examples.join(file)).unwrap();
        scratch.write(&format!("examples/{file}"), &text);
    }
    let recipe = scratch.0.join("examples/hello/Dockerfile");
    let mut labelled = fs::OpenOptions::new().append(true).open(recipe).unwrap();
    writeln!(labelled, "LABEL {label}").unwrap();
}

/// The entry of the task that [`write_hello`] writes.
const HELLO: &str = "examples/hello/task.star";

/// `scratch`'s `pumpkin` with `args`, with `home` as `HOME` and `temp` as `TMPDIR`, and none of
/// the variables that would name other directories for them.
fn at_home(scratch: &Scratch, home: &Path, temp: &Path, args: &[&str]) -> Command {
    let mut run = scratch.pumpkin(args);
    run.env("HOME", home).env("TMPDIR", temp);
    for name in [
        "DOCKER_CONFIG",
        "XDG_CACHE_HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
    ] {
        run.env_remove(name);
    }
    run
}

/// A home of `scratch`'s, whose Docker configuration, `.docker`, is read-only, and holds, for a
/// Docker context but `default`, `context`, a copy of the contexts of this process's, which
/// `docker` reads it from.
fn test_home(scratch: &Scratch, context: &str) -> PathBuf {
    let home = scratch.0.join("home");
    let docker = home.join(".docker");
    fs::create_dir_all(&docker).unwrap();
    if context != "default" {
        let real = env::var_os("DOCKER_CONFIG").map_or_else(
            || PathBuf::from(env::var_os("HOME").unwrap()).join(".docker"),
            PathBuf::from,
        );
        copy_tree(&real.join("contexts"), &docker.join("contexts"));
    }
    fs::set_permissions(&docker, Permissions::from_mode(0o555)).unwrap();
    home
}

/// The names in `dir`, sorted, between commas.
fn held(dir: &Path) -> String {
    let mut names: Vec<String> = (fs::read_dir(dir).unwrap())
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names.join(", ")
}

const OPS_FINAL: &str = r#"
    payload=/pumpkin/in/payload/raw
    bytes=$(wc -c < $payload)
    printf '{"raw_score": %s}\n' $bytes > /pumpkin/out/result.json
    echo kept > /pumpkin/out/evidence.txt
    echo "judged $bytes bytes as $(id -u), with HOME=$HOME, in $(pwd)""#;

const AGENT: &str = r#"#!/bin/sh
    cd /workspace && stat -c '%a %n' .codex .codex/skills .codex/config.toml .tmp agent
    echo "standard input: $(cat)"
    x() { head -c "$1" /dev/zero | tr '\0' x > "$2"; }
    x 6 six && x 9 nine && x 17 seventeen && printf slow > slow
    ctl() {
        out=$(SANDBOXCTL "$@" 2>&1)
        echo "sandboxctl $*: exit $?: $out" | sed 's/"remaining_seconds":[0-9]*/"remaining_seconds":"<s>"/'
    }
    ctl ops
    ctl status 0
    ctl call nope six --wait 15
    ctl call train nine
    ctl call train seventeen
    ctl call train slow --wait 1
    ctl call train six --wait 60
    ctl status 0
    ctl call submit six --arg finalize=true --wait 15"#;

/// The workflow of the run whose agent makes ops: [`workflow`]'s, whose agent runs [`AGENT`] and
/// whose final call runs [`OPS_FINAL`].
fn ops_workflow() -> Value {
    // Spliced in, since `format!` would need each of the shell's braces doubled.
    let script = AGENT.replace("SANDBOXCTL", SANDBOXCTL_PATH);
    let mut ir = workflow("", &["/workspace/agent"]);
    let agent = &mut ir["roles"][0]["runtime"]["agent"];
    agent["prepend_grant_help"] = json!(true);
    // Listed before the directory that holds it.
    let skills = json!({ "path": "/workspace/.codex/skills", "mode": "0750" });
    agent["dirs"].as_array_mut().unwrap().insert(0, skills);
    let launcher = json!({ "path": "/workspace/agent", "mode": "0755", "content": script });
    agent["files"].as_array_mut().unwrap().push(launcher);
    let train = &mut ir["roles"][0]["runtime"]["agent"]["grants"][0]["job"];
    train["payload_max_bytes"] = json!(8);
    // Room for the body's put and command on a loaded engine.
    train["call_timeout_seconds"] = json!(5);
    // With no cumulative limit, the timeout is each call's alone, and what a reply has left
    // depends on no command's time.
    train.as_object_mut().unwrap().remove("cumulative_seconds");
    // The most that an op of the agent's may carry.
    ir["final"]["payload_max_bytes"] = json!(16);
    ir["roles"][0]["runtime"]["agent"]["grants"][1]["now"]["payload_max_bytes"] = json!(16);
    ir
}

const LEASING_AGENT: &str = r#"#!/bin/sh
    printf xxxxxx > /workspace/six
    mkdir --parents /workspace/src/deep && printf hello > /workspace/src/a && printf deep > /workspace/src/deep/b
    ctl() {
        out=$(SANDBOXCTL "$@" 2>&1)
        echo "sandboxctl $*: exit $?: $out" | sed --regexp-extended 's/"(remaining_seconds|elapsed_seconds|busy_seconds|idle_seconds|instance_seconds|cumulative_seconds)":[0-9]+/"\1":"<s>"/g'
    }
    # Waits until lease $1 is ready, then shows its status.
    ready() {
        until SANDBOXCTL call gpu.status "$1" | grep -q '"state":"ready"'; do sleep 1; done
        ctl call gpu.status "$1"
    }
    ctl call gpu.open
    ready 0
    ctl call gpu.put 0 /workspace/src /work/src
    ready 0
    ctl call gpu.run 0 -- sh -c 'echo "in $(pwd) as $(id -u), with CUDA=$CUDA"; echo to stderr >&2; mkdir out; tr a-z A-Z < src/a > out/A; cp --recursive src/deep out/'
    ready 0
    ctl call gpu.get 0 /work/out /workspace/got
    ready 0
    ctl call gpu.put 0 /workspace/src /work/src
    ready 0
    ctl call gpu.run 0 --timeout 1 -- sleep 60
    ready 0
    ctl call gpu.close 0
    echo "got: $(find /workspace/got | sort | tr '\n' ' '), A: $(cat /workspace/got/A), deep/b: $(cat /workspace/got/deep/b)"
    ctl call submit /workspace/six --arg finalize=true"#;

#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
#[expect(clippy::too_many_lines, reason = "most of it is the run's snapshot")]
fn leases_on_docker() {
    let scratch = Scratch::new();
    let sandboxd = sandboxd().to_str().unwrap().to_owned();
    let context = docker_context();
    let engine = engine(&scratch, &context);
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
    let grant = json!({ "name": "gpu", "session": {
        "runner": "gpu_runner", "instances": 1, "instance_seconds": 120,
        "commands": 5, "transfer_bytes": 1024,
    } });
    ir["roles"][0]["runtime"]["agent"]["grants"]
        .as_array_mut()
        .unwrap()
        .insert(1, grant);
    scratch.write_task("leases", &ir);
    scratch.write("pumpkin.toml", &machine(&context));
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
            the lease's record: 0/log/1/command.stderr, 0/log/1/command.stdout, 0/log/4/command.stderr, 0/log/4/command.stdout, 0/out/, 0/sandbox.json
            command 1's stdout: "in /work as 65532, with CUDA=7\n"
            command 1's stderr: "to stderr\n"
            command 4's stdout: ""
            command 4's stderr: ""
            agent `agent`'s standard output:
            sandboxctl call gpu.open: exit 0: {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":5}}
            sandboxctl call gpu.status 0: exit 0: {"kind":"lease_status","lease":0,"state":"ready","elapsed_seconds":"<s>","busy_seconds":"<s>","idle_seconds":"<s>","failure":null,"last":null,"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":5}}
            sandboxctl call gpu.put 0 /workspace/src /work/src: exit 0: {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":4}}
            sandboxctl call gpu.status 0: exit 0: {"kind":"lease_status","lease":0,"state":"ready","elapsed_seconds":"<s>","busy_seconds":"<s>","idle_seconds":"<s>","failure":null,"last":{"kind":"transfer","transfer":0,"end":{"moved":{"entries":3,"bytes":9}}},"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":4}}
            sandboxctl call gpu.run 0 -- sh -c echo "in $(pwd) as $(id -u), with CUDA=$CUDA"; echo to stderr >&2; mkdir out; tr a-z A-Z < src/a > out/A; cp --recursive src/deep out/: exit 0: {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":3}}
            sandboxctl call gpu.status 0: exit 0: {"kind":"lease_status","lease":0,"state":"ready","elapsed_seconds":"<s>","busy_seconds":"<s>","idle_seconds":"<s>","failure":null,"last":{"kind":"command","command":1,"end":{"exited":{"code":0}},"stdout":"in /work as 65532, with CUDA=7\n","stderr":"to stderr\n"},"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":3}}
            sandboxctl call gpu.get 0 /work/out /workspace/got: exit 0: {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":2}}
            sandboxctl call gpu.status 0: exit 0: {"kind":"lease_status","lease":0,"state":"ready","elapsed_seconds":"<s>","busy_seconds":"<s>","idle_seconds":"<s>","failure":null,"last":{"kind":"transfer","transfer":2,"end":{"moved":{"entries":3,"bytes":9}}},"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":2}}
            sandboxctl call gpu.put 0 /workspace/src /work/src: exit 0: {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":1}}
            sandboxctl call gpu.status 0: exit 0: {"kind":"lease_status","lease":0,"state":"ready","elapsed_seconds":"<s>","busy_seconds":"<s>","idle_seconds":"<s>","failure":null,"last":{"kind":"transfer","transfer":3,"end":{"failed":"putting the tree at /work/src in the runner failed: /work/src: File exists (os error 17): the tree's root becomes /work/src, which must not exist: give `--replace` to replace it"}},"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":1}}
            sandboxctl call gpu.run 0 --timeout 1 -- sleep 60: exit 0: {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":0}}
            sandboxctl call gpu.status 0: exit 0: {"kind":"lease_status","lease":0,"state":"ready","elapsed_seconds":"<s>","busy_seconds":"<s>","idle_seconds":"<s>","failure":null,"last":{"kind":"command","command":4,"end":"timed_out","stdout":"","stderr":""},"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":0}}
            sandboxctl call gpu.close 0: exit 0: {"kind":"lease","lease":0,"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":0}}
            got: /workspace/got /workspace/got/A /workspace/got/deep /workspace/got/deep/b , A: HELLO, deep/b: deep
            sandboxctl call submit /workspace/six --arg finalize=true: exit 0: {"kind":"call","returned":{},"remaining_seconds":"<s>"}"#]],
    );
}

/// What the agent of the run whose final call's target leases a runner submits: a program that
/// prints `/answers`, a file of the target's image, or, if it can't read it, 7.
const PROGRAM: &str = "cat /answers || echo 7";

const LEASING_FINAL: &str = r#"
    echo "the judge reads /answers: $(cat /answers)"
    ready() { until SANDBOXCTL call box.status 0 | grep -q '"state":"ready"'; do sleep 1; done; }
    SANDBOXCTL call box.open > /dev/null
    ready
    SANDBOXCTL call box.put 0 /pumpkin/in/payload /work/submission > /dev/null
    ready
    SANDBOXCTL call box.run 0 -- sh /work/submission/raw > /dev/null
    ready
    status=$(SANDBOXCTL call box.status 0)
    echo "$status" | sed --regexp-extended 's/"(elapsed_seconds|busy_seconds|idle_seconds|instance_seconds)":[0-9]+/"\1":"<s>"/g'
    score=$(echo "$status" | sed --quiet 's/.*"stdout":"\([0-9]*\)\\n".*/\1/p')
    printf '{"raw_score": %s}' "$score" > /pumpkin/out/result.json"#;

#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
fn final_call_leases_on_docker() {
    let scratch = Scratch::new();
    let sandboxd = sandboxd().to_str().unwrap().to_owned();
    let context = docker_context();
    let engine = engine(&scratch, &context);
    let agent = format!(
        "#!/bin/sh\nprintf '%s\\n' '{PROGRAM}' > /workspace/program\n\
         {SANDBOXCTL_PATH} call submit /workspace/program --arg finalize=true\n"
    );
    let mut ir = workflow("", &["/workspace/agent"]);
    let launcher = json!({ "path": "/workspace/agent", "mode": "0755", "content": agent });
    let runtime = &mut ir["roles"][0]["runtime"]["agent"];
    runtime["files"].as_array_mut().unwrap().push(launcher);
    ir["roles"][2]["runtime"]["target"]["sessions"] = json!([{ "name": "box", "session": {
        "runner": "box", "instances": 1, "instance_seconds": 120, "transfer_bytes": 1_048_576,
    } }]);
    let mut runner = ir["roles"][1].clone();
    runner["name"] = json!("box");
    runner["image"] = json!(RUNNER);
    runner["runtime"] = json!({ "runner": { "cwd": "/work" } });
    ir["roles"].as_array_mut().unwrap().push(runner);
    scratch.write_task("isolation", &ir);
    let judge = LEASING_FINAL.replace("SANDBOXCTL", SANDBOXCTL_PATH);
    scratch.write("isolation/final.star", &final_running(&judge));
    // The target's, and the agent's, image holds `/answers`; the runner's doesn't.
    let recipe =
        format!("FROM {BUSYBOX}\nRUN mkdir --mode=1777 /workspace && echo 42 > /answers\n");
    scratch.write("isolation/Dockerfile", &recipe);
    scratch.write("pumpkin.toml", &machine(&context));
    let mut run = scratch.pumpkin(&["run", "isolation/task.star", "--sandboxd", &sandboxd]);
    run.args(["--wait", "--config", "pumpkin.toml"]);
    let output = with_room(&mut run);
    let rootful = !engine.report().security.rootless;
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    let (id, dir) = record(&stderr).unwrap();
    let record = scratch.0.join(dir);
    let mut lines = vec![format!("exit status: {:?}", output.status.code())];
    let listed = containers(&context).list(&RunTag::try_from(id.to_owned()).unwrap());
    lines.push(format!("listed once it ended: {:?}", listed.unwrap()));
    let outcome = fs::read_to_string(record.join("outcome.json")).unwrap();
    let outcome: Value = serde_json::from_str(&outcome).unwrap();
    lines.push(format!(
        "values: {}",
        outcome["verdict"]["result"]["values"]
    ));
    for stream in ["stdout", "stderr"] {
        // The lease's second work, after the put.
        let path = record.join(format!("leases/0/log/1/command.{stream}"));
        let text = fs::read_to_string(path).unwrap_or_default();
        lines.push(format!("the program's {stream}: {text:?}"));
    }
    lines.extend(standard_outputs(&record).into_iter().map(remaining));
    let shown = [(&sandboxd[..], "<sandboxd>")];
    check(
        &scratch,
        [Step::Ran(lines.join("\n"))],
        rootful,
        &shown,
        &expect![[r#"
            exit status: Some(0)
            listed once it ended: []
            values: [{"name":"score","value":7}]
            the program's stdout: "7\n"
            the program's stderr: "cat: can't open '/answers': No such file or directory\n"
            agent `agent`'s standard output:
            {"kind":"call","returned":{},"remaining_seconds":"<s>"}
            job 0's command 0's standard output:
            the judge reads /answers: 42
            {"kind":"lease_status","lease":0,"state":"ready","elapsed_seconds":"<s>","busy_seconds":"<s>","idle_seconds":"<s>","failure":null,"last":{"kind":"command","command":1,"end":{"exited":{"code":0}},"stdout":"7\n","stderr":"cat: can't open '/answers': No such file or directory\n"},"remaining":{"instances":0,"instance_seconds":"<s>","cumulative_seconds":null,"commands":null}}"#]],
    );
}

#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
#[expect(clippy::too_many_lines, reason = "most of it is the run's snapshot")]
fn ops_on_docker() {
    let scratch = Scratch::new();
    let sandboxd = sandboxd().to_str().unwrap().to_owned();
    let context = docker_context();
    let engine = engine(&scratch, &context);
    scratch.write_task("ops", &ops_workflow());
    scratch.write("ops/final.star", &final_running(OPS_FINAL));
    scratch.write("pumpkin.toml", &machine(&context));
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
    let over = sockets.join("agent-0/s").into_os_string().len() > 107;
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
            pumpkin:    <t> agent started on local/container
            pumpkin:    <t> train job 0 of agent 0 asked on local/container
            pumpkin:    <t> train job 0 of agent 0 started
            pumpkin:    <t> train job 0 of agent 0 ended: failed: the call's timeout passed
            pumpkin:    <t> train job 1 of agent 0 asked on local/container
            pumpkin:    <t> train job 1 of agent 0 started
            pumpkin:    <t> train job 1 of agent 0 ended: bytes=6
            pumpkin:    <t> the work phase ended
            pumpkin:    <t> final call job 2 asked on local/container
            pumpkin:    <t> final call job 2 started
            pumpkin:    <t> final call job 2 ended: score=6
            pumpkin: run <id> started <t> after it was asked for, and ran <t>:
                 <t> agent on local/container, started at <t>, exited 0 at <t>; its work ended at <t>
                 <t> train job 0 of agent 0 on local/container, started after <t>, ran <t>: failed: the call's timeout passed (timed out)
                 <t> train job 1 of agent 0 on local/container, started after <t>, ran <t>: bytes=6
                 <t> final call job 2 on local/container, started after <t>, ran <t>: score=6
              sandboxes: 4 on local/container, <t> in all, 4 created, all deleted
            pumpkin: run <id> finished with a result (score=6); its record is <scratch>/<runs>/<id>
            pumpkin: to run it again: pumpkin run --runs '<scratch>/<runs>' --config <scratch>/pumpkin.toml --sandboxd <sandboxd> <scratch>/ops/task.star
            record: agents/agent/log/command.stderr, agents/agent/log/command.stdout, agents/agent/out/, agents/agent/sandbox.json, driver.log, driver.pid, ir.json, jobs/0/log/0/command.stderr, jobs/0/log/0/command.stdout, jobs/0/out/, jobs/0/sandbox.json, jobs/1/got/1/result.json, jobs/1/log/0/command.stderr, jobs/1/log/0/command.stdout, jobs/1/out/, jobs/1/sandbox.json, jobs/2/got/1/result.json, jobs/2/log/0/command.stderr, jobs/2/log/0/command.stdout, jobs/2/out/evidence.txt, jobs/2/out/result.json, jobs/2/sandbox.json, log.jsonl, outcome.json, payloads/5e0cf7bd1dfa3831788b0cf6dedcdd228fba6f34dc238d371e746567e80bc7b6, payloads/b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd, provider.json, record.json, sources/task/lib/train.star, sources/task/ops/final.star, sources/task/ops/submit.star, sources/task/ops/task.star
            agent `agent`'s sandbox, agents/agent/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local"}
            job 0's sandbox, jobs/0/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local"}
            job 1's sandbox, jobs/1/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local"}
            job 2's sandbox, jobs/2/sandbox.json: {"delete":{"state":"deleted"},"id":"<container>","provider":"local"}
            outcome.json: {"agents":[{"role":"agent","submission":{"finalized":true,"payload":{"digest":"sha256:b7fb217694ae2d305e766608d250f797daa984e4ac4b5fa638a729be352f2fcd","size":6}},"work_end":{"kind":"agent_exited","status":{"code":0}}}],"state":"finished","usage":{"model_requests":0,"ran_ns":"<ns>","sandboxes":[{"class":"container","count":4,"lasted_ns":"<ns>","provider":"local"}],"tokens":{"input":0,"output":0,"responses":0}},"verdict":{"kind":"returned","result":{"values":[{"name":"score","value":6}]}}}
            listed once it ended: []
            the agent's socket's path is over 107 bytes: true
            socket directory left: false
            modes in the record: directories 755; files 644
            agent `agent`'s standard output:
            700 .codex
            750 .codex/skills
            600 .codex/config.toml
            700 .tmp
            755 agent
            standard input: Sandbox commands for role agent
            Work time upper bound: 300 seconds. Run `sandboxctl ops` for current limits.

            sandboxctl call train [FILE] [--wait SECONDS]
            Payload limit: 8 bytes. Timeout: 5 seconds per call. Call limit: 100.

            sandboxctl call submit [FILE] [--arg finalize=BOOL] [--wait SECONDS]
            Default for finalize: false.
            Payload limit: 16 bytes. Returns immediately.

            FILE is a regular file containing the payload. Omit FILE for an empty payload.
            Use --wait SECONDS to wait for the job. A completed job has a returned field. Without --wait, follow the job number with sandboxctl status JOB.

            ---

            Task

            Solve the task in /task/README.md.
            sandboxctl ops: exit 0: {"kind":"ops","role":"agent","remaining_seconds":"<s>","resources":{"cpu_millis":500,"memory_bytes":268435456},"grants":[{"kind":"job","name":"train","payload_max_bytes":8,"calls":100,"cumulative_seconds":null,"call_timeout_seconds":5},{"kind":"now","name":"submit","payload_max_bytes":16,"arguments":{"finalize":{"type":"bool","default":false}}}]}
            sandboxctl status 0: exit 1: {"kind":"refused","message":"you have no job 0: ask about a job that one of your calls returned"}
            sandboxctl call nope six --wait 15: exit 1: {"kind":"refused","message":"you hold no grant named `nope`: call one that `ops` lists"}
            sandboxctl call train nine: exit 1: {"kind":"refused","message":"the payload is 9 bytes, over `train`'s limit of 8: send a smaller one"}
            sandboxctl call train seventeen: exit 1: {"kind":"refused","message":"the payload is 17 bytes, more than the limit of 16: send at most 16"}
            sandboxctl call train slow --wait 1: exit 124: sandboxctl: wait expired after job 0 was admitted: inspect it with `sandboxctl status 0`
            sandboxctl call train six --wait 60: exit 0: {"kind":"status","job":1,"state":"completed","returned":{"bytes":6},"remaining":{"calls":98,"seconds":null}}
            sandboxctl status 0: exit 2: {"kind":"status","job":0,"state":"errored","failure":{"code":"call_timeout","message":"the call ran past its timeout"},"remaining":{"calls":98,"seconds":null}}
            sandboxctl call submit six --arg finalize=true --wait 15: exit 0: {"kind":"call","returned":{},"remaining_seconds":"<s>"}
            job 0's command 0's standard output:
            job 1's command 0's standard output:
            job 2's command 0's standard output:
            judged 6 bytes as 65532, with HOME=/pumpkin/home, in /pumpkin/home"#]],
    );
}

/// The images labelled with this, which dropping it removes by each of their tags.
struct Labelled(String);

impl Drop for Labelled {
    // Nothing here panics: a panic while a failed test unwinds would abort the test binary. It
    // prints each tag that it can't remove.
    fn drop(&mut self) {
        let filter = format!("label={}", self.0);
        let format = "{{.Repository}}:{{.Tag}}";
        let listed = docker()
            .args(["image", "ls", "--filter", &filter, "--format", format])
            .output();
        let tags = listed.map(|listed| String::from_utf8_lossy(&listed.stdout).into_owned());
        for tag in tags.unwrap_or_default().lines() {
            let removed = docker().args(["image", "rm", "--", tag]).output();
            if !removed.is_ok_and(|removed| removed.status.success()) {
                eprintln!("the test's image tag {tag} can't be removed");
            }
        }
    }
}

/// `line` with the seconds left of the run's deadline that a call's answer gives masked, as the
/// run's start moves them.
fn remaining(line: String) -> String {
    match line.split_once(r#""remaining_seconds":"#) {
        Some((before, after)) => {
            let after = after.trim_start_matches(|c: char| c.is_ascii_digit());
            format!(r#"{before}"remaining_seconds":"<s>"{after}"#)
        }
        None => line,
    }
}
