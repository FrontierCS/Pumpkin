//! `build`: a task's source checked as `check` checks it, before the configuration and the
//! engine are reached, then its images built on the engine, and its IR printed alone on the
//! standard output.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::process::{Command, Output};

use expect_test::expect;
use schema::ir;
use schema::workflow::Workflow;
use test_support::{BUSYBOX, docker, docker_context};

use super::check::{task, write_repository, write_variants};
use super::{Scratch, Step, check, engine, invocation, machine, ran, report};

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn refusals() {
    let scratch = Scratch::new("build-refusals");
    write_repository(&scratch, "");
    scratch.write("tasks/broken/task.star", &task(0));
    // A `docker` that says whether it ran.
    fs::create_dir(scratch.0.join("bin")).unwrap();
    scratch.write_executable("bin/docker", b"#!/bin/sh\ntouch \"$0.ran\"\nexit 1\n");
    let alone = |args: &[&str]| {
        let mut command = scratch.pumpkin(&["build"]);
        command.args(args).args(["--arg", "model=model-a"]);
        command
            .env("PATH", scratch.0.join("bin"))
            .env_remove("HOME");
        command
    };
    let pumpkin = |args: &[&str]| scratch.pumpkin(args);
    let steps = [
        pumpkin(&["build", "--help"]),
        alone(&["tasks/broken/task.star"]),
        alone(&["tasks/bench/task.star"]),
        alone(&["tasks/bench/task.star", "--config", "absent.toml"]),
        pumpkin(&["build"]),
        pumpkin(&["build", "task.json", "--arg", "model", "--config"]),
    ];
    let ran: Vec<String> = (steps.into_iter())
        .map(|command| ran(command, &scratch.0, false))
        .collect();
    let docker = scratch.0.join("bin/docker.ran").exists();
    let ran = Step::Ran(format!("{}\ndocker was run: {docker}", ran.join("\n")));
    check(
        &scratch,
        [ran],
        false,
        &[],
        &expect![[r#"
            $ pumpkin build --help
            exit status: Some(0)
            printed:
              Build a task: check its source as `pumpkin check` does, then build each image
              that it declares on the machine's Docker engine, and print the IR of its
              workflow, which names each image by its ID.

              Usage: pumpkin build [OPTIONS] <entry>

              Arguments:
                <entry>
                        The task's entry file, as `pumpkin check` takes it.

              Options:
                    --arg <name>=<value>
                        The value of the arg that an arg() call declares as <name>, as
                        `pumpkin check` takes it.

                    --config <file>
                        The machine's configuration, whose [docker] names the engine; see
                        `pumpkin help config`.
                        Default: $HOME/.config/pumpkin/pumpkin.toml

                -h, --help
                        Print this help.

              Examples:
                pumpkin build tasks/bench/task.star --arg model=model-a > workflow.json
                pumpkin build tasks/bench/task.star --arg model=model-a --config pumpkin.toml

              Exit status:
                0  The images are built, and the IR printed.
                1  The task has problems, or an image's build failed, each said at its place;
                   or another error.
                2  A usage error.
              Every exit but 0 ends with a `pumpkin:` line on the standard error, which says
              why, and an exit of 1 then with the command that builds the task again, its
              paths made absolute. The standard output holds only the IR, as a run's record
              keeps it in ir.json; the builds' output goes to the standard error.

              Every problem that `check` finds is said before the configuration is read, and
              so before the engine is reached and any image is built. An image is built from
              a stage of its context, and tagged by the digest of the build's inputs, in
              pumpkin/<its recipe's directory below the root>, or, for a recipe of the
              standard library, in pumpkin-library/<its directory below @pumpkin//>; one that
              the engine has under its tag already is not built again. Images are built at
              once, as many as the CPUs of this machine that `pumpkin` may run on, as Cargo's
              jobs are, even if the engine is another machine's, each once those it is built
              on are built. All that each build's `docker build` wrote is said at once when it
              ends, as `make --output-sync` says a job's, then that it ended. A failed build
              is said at the image() call that declares its image; once one has failed, no
              other starts but those started with it, and each that runs is waited for, and
              said if it fails.

              With [controller] in the configuration, `build` sends the task as `pumpkin run`
              does, follows the build's job on the controller, and prints the IR it printed.
              A job there, of `run` or `build`, builds once it takes one of the controller's
              build slots, one for every two of its CPUs, waiting, if each is taken, until one
              is free; and, once a prune has removed what images no job holds, refuses to
              build while more than 85% of the controller's volume is used.
            $ env --unset=HOME PATH=<scratch>/bin pumpkin build tasks/broken/task.star --arg model=model-a
            exit status: Some(1)
            lib/bench.star:80:19: `tasks/broken/agent/Dockerfile` doesn't exist: name a regular file below the root that does
                80 |     agent_image = image("agent/Dockerfile", context = ".", mount = SHARED)
                   |                   ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `bench_task()`, called at tasks/broken/task.star:5:1
            lib/bench.star:33:18: `tasks/broken/judge/Dockerfile` doesn't exist: name a regular file below the root that does
                33 |         "image": image(recipe, mount = SHARED),
                   |                  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `_judge()`, called at lib/bench.star:82:9
                in `bench_task()`, called at tasks/broken/task.star:5:1
            tasks/broken/task.star:6:28: `cpu_millis` can't be `0`: expected a positive int
                6 |     agent = {"cpu_millis": 0, "memory_bytes": 32 * GiB},
                  |                            ^
                the IR's /roles/0/needs/cpu_millis
            pumpkin: tasks/broken/task.star: the task has 3 problems
            pumpkin: to build it again: pumpkin build --arg model=model-a <scratch>/tasks/broken/task.star
            $ env --unset=HOME PATH=<scratch>/bin pumpkin build tasks/bench/task.star --arg model=model-a
            exit status: Some(1)
            pumpkin: HOME is unset or empty: set it, or name the configuration with `--config`
            pumpkin: to build it again: pumpkin build --arg model=model-a <scratch>/tasks/bench/task.star
            $ env --unset=HOME PATH=<scratch>/bin pumpkin build tasks/bench/task.star --config absent.toml --arg model=model-a
            exit status: Some(1)
            pumpkin: the configuration absent.toml can't be read (No such file or directory (os error 2)): write one there, as `pumpkin help config` says, or name another file with `--config`
            pumpkin: to build it again: pumpkin build --arg model=model-a --config <scratch>/absent.toml <scratch>/tasks/bench/task.star
            $ pumpkin build
            exit status: Some(2)
            error: the following required arguments were not provided:
              <entry>

            Usage: pumpkin build <entry>
            pumpkin: to read how to use it: pumpkin help build
            $ pumpkin build task.json --arg model --config
            exit status: Some(2)
            error: invalid value 'task.json' for '<entry>': `task.json` names no `.star` file: name a task's entry file, whose name ends in `.star`
            pumpkin: to read how to use it: pumpkin help build
            docker was run: false"#]],
    );
}

#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn builds_on_docker() {
    let scratch = Scratch::new("build-docker");
    let label = format!("pumpkin.test=build-{}", std::process::id());
    let _images = Labelled(label.clone());
    write_repository(&scratch, "");
    // A repository whose judges' recipe is below two mounts, `shared/sub` inside `shared`, and
    // so is read from the deeper one, `//sub`, as `check` reads it; `//shared` holds none there.
    write_repository(&scratch, "nested/");
    let library = fs::read_to_string(scratch.0.join("nested/lib/bench.star")).unwrap();
    let library = library.replace(
        "SHARED = {\"shared\": \"//shared\"}",
        "SHARED = {\"shared\": \"//shared\", \"shared/sub\": \"//sub\"}",
    );
    scratch.write("nested/lib/bench.star", &library);
    let in_sub = task(8000).replace(
        "    metrics = [",
        "    judge_recipe = \"shared/sub/Dockerfile\",\n    metrics = [",
    );
    scratch.write("nested/tasks/bench/task.star", &in_sub);
    let in_sub =
        format!("FROM {BUSYBOX}\nCOPY shared/README /opt/shared/\nRUN echo in-sub && true\n");
    scratch.write("nested/sub/Dockerfile", &in_sub);
    // Labelled, so that the images are this process's own, and so that they can be removed;
    // a harness's layer has its base's labels.
    let recipes = [
        "tasks/bench/agent",
        "tasks/bench/judge",
        "nested/tasks/bench/agent",
    ];
    let recipes = (recipes.iter()).map(|dir| format!("{dir}/Dockerfile"));
    for recipe in recipes.chain(["nested/sub/Dockerfile".to_owned()]) {
        let mut file = OpenOptions::new()
            .append(true)
            .open(scratch.0.join(recipe))
            .unwrap();
        writeln!(file, "LABEL {label}").unwrap();
    }
    scratch.write("tasks/failing/task.star", &task(8000));
    let failing = format!(
        "FROM {BUSYBOX}\nLABEL {label}\nRUN echo failing && echo 'error: the step failed' >&2 && false\n"
    );
    scratch.write("tasks/failing/agent/Dockerfile", &failing);
    scratch.write("tasks/failing/judge/Dockerfile", &failing);
    let context = docker_context();
    scratch.write("pumpkin.toml", &machine(&context));
    let engine = engine(&scratch, &context);
    let rootful = !engine.report().security.rootless;
    let build = |entry: &str| {
        let mut command = scratch.pumpkin(&["build", entry, "--arg", "model=model-a"]);
        command.args(["--config", "pumpkin.toml"]);
        command
    };
    let (first, ir) = built(build("tasks/bench/task.star"), &scratch, rootful);
    let mut lines = vec![first];
    let labelled = images(&label);
    let mut shown = ir.clone();
    for (id, repository) in &labelled {
        shown = shown.replace(id, &format!("<{repository}>"));
    }
    let repositories: Vec<&str> = labelled
        .iter()
        .map(|(_, repository)| &repository[..])
        .collect();
    lines.push(format!("images labelled: {}", repositories.join(", ")));
    let parsed: ir::Workflow = serde_json::from_str(&ir).unwrap();
    let workflow = Workflow::parse(&parsed);
    lines.push(format!("the workflow parses: {}", workflow.is_ok()));
    lines.push(format!(
        "printed as a record keeps it: {}",
        workflow.is_ok_and(|workflow| ir == runtime::ir_json(&workflow))
    ));
    lines.push("printed:".to_owned());
    lines.extend(shown.lines().map(|line| format!("  {line}")));
    let (again, printed) = built(build("tasks/bench/task.star"), &scratch, rootful);
    lines.push(again);
    lines.push(format!("printed the same IR: {}", printed == ir));
    lines.push(format!(
        "the same images labelled after: {}",
        images(&label) == labelled
    ));
    let (failed, printed) = built(build("tasks/failing/task.star"), &scratch, rootful);
    lines.push(failed);
    lines.push(format!("printed nothing: {}", printed.is_empty()));
    let (nested, printed) = built(build("nested/tasks/bench/task.star"), &scratch, rootful);
    lines.push(nested);
    let judge = images(&label)
        .into_iter()
        .find(|(_, repository)| repository == "pumpkin/tasks/bench/shared/sub");
    lines.push(format!(
        "the judges' image is the deeper mount's recipe's: {}",
        judge.is_some_and(|(id, _)| printed.contains(&id))
    ));
    check(
        &scratch,
        [Step::Ran(lines.join("\n"))],
        rootful,
        &[],
        &expect![[r#"
            $ pumpkin build tasks/bench/task.star --arg model=model-a --config pumpkin.toml
            exit status: Some(0)
            builds' output shown: false, echoing: [], its steps' errors: []
            builds ended: ["pumpkin: built an image in pumpkin-library/harness/codex/layer", "pumpkin: built an image in pumpkin/tasks/bench/agent", "pumpkin: built an image in pumpkin/tasks/bench/judge"]
            pumpkin: building an image in pumpkin/tasks/bench/agent
            pumpkin: building an image in pumpkin/tasks/bench/judge
            pumpkin: building an image in pumpkin-library/harness/codex/layer
            images labelled: pumpkin-library/harness/codex/layer, pumpkin/tasks/bench/agent, pumpkin/tasks/bench/judge
            the workflow parses: true
            printed as a record keeps it: true
            printed:
              {
                "ir_version": "pumpkin.ir/v0",
                "name": "neutral/bench",
                "roles": [
                  {
                    "egress": {
                      "model": [
                        {
                          "api": "responses",
                          "models": [
                            "model-a"
                          ],
                          "request_headers": [
                            "originator",
                            "user-agent",
                            "session-id",
                            "thread-id",
                            "x-codex-beta-features",
                            "x-codex-window-id",
                            "x-codex-turn-metadata",
                            "x-openai-internal-codex-responses-lite"
                          ],
                          "response_headers": [
                            "openai-model",
                            "x-reasoning-included",
                            "x-models-etag",
                            "x-codex-turn-state",
                            "x-codex-safety-buffering-enabled",
                            "x-codex-safety-buffering-faster-model",
                            "x-codex-credits-has-credits",
                            "x-codex-credits-unlimited",
                            "x-codex-credits-balance",
                            "x-codex-promo-message",
                            "x-codex-rate-limit-reached-type",
                            "x-codex-active-limit",
                            "x-oai-request-id",
                            "cf-ray",
                            "x-openai-authorization-error",
                            "x-error-json",
                            "x-codex-primary-used-percent",
                            "x-codex-primary-window-minutes",
                            "x-codex-primary-reset-at",
                            "x-codex-secondary-used-percent",
                            "x-codex-secondary-window-minutes",
                            "x-codex-secondary-reset-at",
                            "x-codex-limit-name"
                          ]
                        }
                      ]
                    },
                    "grants": [
                      {
                        "judge": {
                          "call_timeout_seconds": 600,
                          "calls": 100,
                          "cumulative_seconds": 12000,
                          "name": "train",
                          "role": "train_judge",
                          "slot": "payload"
                        }
                      },
                      {
                        "submit": {
                          "name": "final"
                        }
                      }
                    ],
                    "image": "<pumpkin-library/harness/codex/layer>",
                    "name": "agent",
                    "needs": {
                      "cpu_millis": 8000,
                      "memory_bytes": 34359738368
                    },
                    "runtime": {
                      "agent": {
                        "command": {
                          "argv": [
                            "/harness/codex-0.167.0/bin/codex",
                            "exec",
                            "--ephemeral",
                            "--ignore-rules",
                            "--skip-git-repo-check",
                            "--json",
                            "-"
                          ],
                          "cwd": "/workspace",
                          "env": [
                            "CODEX_HOME=/workspace/.codex",
                            "TMPDIR=/workspace/.tmp"
                          ]
                        },
                        "deadline_seconds": 18000,
                        "dirs": [
                          {
                            "mode": "0700",
                            "path": "/workspace/.codex"
                          },
                          {
                            "mode": "0700",
                            "path": "/workspace/.tmp"
                          }
                        ],
                        "files": [
                          {
                            "content": "model = \"model-a\"\nmodel_reasoning_effort = \"high\"\nmodel_provider = \"pumpkin\"\n# Nobody is there to approve a command, and the agent's sandbox confines Codex, whose own\n# sandbox needs `bwrap`, which its pin leaves out.\napproval_policy = \"never\"\nsandbox_mode = \"danger-full-access\"\n# A tool's shell reads no login profile, which could change the environment the command sets.\nallow_login_shell = false\n# Nothing reaches past the model route, the only way out of the sandbox.\nweb_search = \"disabled\"\n\n# Codex's record of the run is the events it writes to its standard output, which the teardown\n# collects. The databases it makes in its home as it starts stay there, uncollected: `exec`\n# writes them no log, and its session, ephemeral, keeps nothing in them.\n\n# The model endpoint `sandboxd` serves, which needs no credential.\n[model_providers.pumpkin]\n# Any name but `OpenAI` keeps off the features Codex keeps for the provider of that name.\nname = \"Pumpkin\"\nbase_url = \"http://127.0.0.1:11235\"\nwire_api = \"responses\"\nrequires_openai_auth = false\n# A run of hours meets transient failures.\nrequest_max_retries = 100\nstream_max_retries = 100\n\n# The lead agent and as many spawned ones at most, each with one model request in flight at a\n# time: as many requests as the model route takes from a sandbox at once.\n[agents]\nmax_concurrent_threads_per_session = 6\n\n[analytics]\nenabled = false\n\n[features]\nplugins = false\napps = false\nimage_generation = false\n\n[plugins._default]\nenabled = false\n",
                            "mode": "0600",
                            "path": "/workspace/.codex/config.toml"
                          }
                        ],
                        "finalize_grace_seconds": 60,
                        "stdin": "Solve the task in /task/README.md, working in /workspace.\n\nSubmit a tar for a train call with `sandboxctl`, read its score and the calls left, and go on\nwhile it helps. Submit the final tar with `sandboxctl submit final <tar> --finalize` once, as\nyour last step, then exit.\n"
                      }
                    },
                    "user": {
                      "gid": 1000,
                      "uid": 1000
                    }
                  },
                  {
                    "image": "<pumpkin/tasks/bench/judge>",
                    "name": "train_judge",
                    "needs": {
                      "cpu_features": [
                        "avx2"
                      ],
                      "cpu_millis": 8000,
                      "memory_bytes": 2147483648
                    },
                    "runtime": {
                      "judge": {
                        "command": {
                          "argv": [
                            "/opt/judge/run"
                          ],
                          "cwd": "/pumpkin/home",
                          "env": [
                            "JUDGE_MODE=train",
                            "JUDGE_PAYLOAD=/pumpkin/in/payload/raw"
                          ]
                        },
                        "result": {
                          "returns": [
                            "/score",
                            "/metrics/instances",
                            "/metrics/solved",
                            "/metrics/submitted",
                            "/metrics/invalid",
                            "/metrics/duplicate",
                            "/metrics/unbounded",
                            "/metrics/rejected",
                            "/failure"
                          ],
                          "score": "/score"
                        },
                        "slots": [
                          {
                            "max_bytes": 4194304,
                            "name": "payload"
                          }
                        ]
                      }
                    },
                    "user": {
                      "gid": 65532,
                      "uid": 65532
                    }
                  },
                  {
                    "image": "<pumpkin/tasks/bench/judge>",
                    "name": "final_judge",
                    "needs": {
                      "cpu_features": [
                        "avx2"
                      ],
                      "cpu_millis": 8000,
                      "memory_bytes": 2147483648
                    },
                    "runtime": {
                      "judge": {
                        "command": {
                          "argv": [
                            "/opt/judge/run"
                          ],
                          "cwd": "/pumpkin/home",
                          "env": [
                            "JUDGE_MODE=final",
                            "JUDGE_PAYLOAD=/pumpkin/in/payload/raw"
                          ]
                        },
                        "result": {
                          "returns": [
                            "/score",
                            "/metrics/instances",
                            "/metrics/solved",
                            "/metrics/submitted",
                            "/metrics/invalid",
                            "/metrics/duplicate",
                            "/metrics/unbounded",
                            "/metrics/rejected",
                            "/failure"
                          ],
                          "score": "/score"
                        },
                        "slots": [
                          {
                            "max_bytes": 4194304,
                            "name": "payload"
                          }
                        ]
                      }
                    },
                    "user": {
                      "gid": 65532,
                      "uid": 65532
                    }
                  }
                ],
                "score": {
                  "role": "final_judge",
                  "slot": "payload",
                  "timeout_seconds": 120
                }
              }
            $ pumpkin build tasks/bench/task.star --arg model=model-a --config pumpkin.toml
            exit status: Some(0)
            builds' output shown: false, echoing: [], its steps' errors: []
            builds ended: []
            printed the same IR: true
            the same images labelled after: true
            $ pumpkin build tasks/failing/task.star --arg model=model-a --config pumpkin.toml
            exit status: Some(1)
            builds' output shown: true, echoing: ["failing"], its steps' errors: ["error: the step failed"]
            builds ended: ["pumpkin: the build of an image in pumpkin/tasks/failing/agent failed", "pumpkin: the build of an image in pumpkin/tasks/failing/judge failed"]
            pumpkin: building an image in pumpkin/tasks/failing/agent
            pumpkin: building an image in pumpkin/tasks/failing/judge
            lib/bench.star:80:19: building the image failed: `docker build` of `agent/Dockerfile` failed with exit status: 1: its output above says why: fix what Docker reports
                80 |     agent_image = image("agent/Dockerfile", context = ".", mount = SHARED)
                   |                   ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `bench_task()`, called at tasks/failing/task.star:5:1
            lib/bench.star:33:18: building the image failed: `docker build` of `judge/Dockerfile` failed with exit status: 1: its output above says why: fix what Docker reports
                33 |         "image": image(recipe, mount = SHARED),
                   |                  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `_judge()`, called at lib/bench.star:82:9
                in `bench_task()`, called at tasks/failing/task.star:5:1
            pumpkin: tasks/failing/task.star: the task has 2 problems
            pumpkin: to build it again: pumpkin build --arg model=model-a --config <scratch>/pumpkin.toml <scratch>/tasks/failing/task.star
            printed nothing: true
            $ pumpkin build nested/tasks/bench/task.star --arg model=model-a --config pumpkin.toml
            exit status: Some(0)
            builds' output shown: false, echoing: [], its steps' errors: []
            builds ended: ["pumpkin: built an image in pumpkin-library/harness/codex/layer", "pumpkin: built an image in pumpkin/tasks/bench/agent", "pumpkin: built an image in pumpkin/tasks/bench/shared/sub"]
            pumpkin: building an image in pumpkin/tasks/bench/agent
            pumpkin: building an image in pumpkin/tasks/bench/shared/sub
            pumpkin: building an image in pumpkin-library/harness/codex/layer
            the judges' image is the deeper mount's recipe's: true"#]],
    );
}

#[test]
#[ignore = "needs Docker: run cargo xtask live docker"]
fn variants_on_docker() {
    let scratch = Scratch::new("build-variants");
    let label = format!("pumpkin.test=variants-{}", std::process::id());
    let _images = Labelled(label.clone());
    write_variants(&scratch, &label);
    let context = docker_context();
    scratch.write("pumpkin.toml", &machine(&context));
    let engine = engine(&scratch, &context);
    let rootful = !engine.report().security.rootless;
    let build = |args: &[&str]| {
        let mut command = scratch.pumpkin(&["build", "tasks/bench/task.star"]);
        command.args(["--arg", "model=model-a", "--config", "pumpkin.toml"]);
        command.args(args);
        command
    };
    let mut lines = Vec::new();
    let mut seen = Vec::new();
    for args in [&[][..], &["--arg", "note=two"]] {
        let (ran, printed) = built(build(args), &scratch, rootful);
        lines.push(ran);
        // A build that failed has said why, and printed no IR.
        if !printed.is_empty() {
            lines.extend(roles(&printed, &mut seen));
        }
    }
    // Refused before the engine is reached, which then warns of nothing.
    let (refused, printed) = built(build(&["--arg", "base=busybox:1.37"]), &scratch, false);
    lines.push(refused);
    lines.push(format!("printed nothing: {}", printed.is_empty()));
    check(
        &scratch,
        [Step::Ran(lines.join("\n"))],
        rootful,
        &[],
        &expect![[r#"
            $ pumpkin build tasks/bench/task.star --arg model=model-a --config pumpkin.toml
            exit status: Some(0)
            builds' output shown: false, echoing: [], its steps' errors: []
            builds ended: ["pumpkin: built an image in pumpkin-library/harness/codex/layer", "pumpkin: built an image in pumpkin/tasks/bench/agent", "pumpkin: built an image in pumpkin/tasks/bench/judge, stage eval", "pumpkin: built an image in pumpkin/tasks/bench/judge, stage train", "pumpkin: built an image in pumpkin/tasks/bench/toolchain, stage go", "pumpkin: built an image in pumpkin/tasks/bench/tools"]
            pumpkin: building an image in pumpkin/tasks/bench/tools
            pumpkin: building an image in pumpkin/tasks/bench/toolchain
            pumpkin: building an image in pumpkin/tasks/bench/judge
            pumpkin: building an image in pumpkin/tasks/bench/agent
            pumpkin: building an image in pumpkin-library/harness/codex/layer
            agent: image 1, note=one
            train_judge: image 2, stage=train
            final_judge: image 3, stage=eval
            $ pumpkin build tasks/bench/task.star --arg model=model-a --config pumpkin.toml --arg note=two
            exit status: Some(0)
            builds' output shown: false, echoing: [], its steps' errors: []
            builds ended: ["pumpkin: built an image in pumpkin-library/harness/codex/layer", "pumpkin: built an image in pumpkin/tasks/bench/agent", "pumpkin: built an image in pumpkin/tasks/bench/tools"]
            pumpkin: building an image in pumpkin/tasks/bench/tools
            pumpkin: building an image in pumpkin/tasks/bench/agent
            pumpkin: building an image in pumpkin-library/harness/codex/layer
            agent: image 4, note=two
            train_judge: image 2, stage=train
            final_judge: image 3, stage=eval
            $ pumpkin build tasks/bench/task.star --arg model=model-a --config pumpkin.toml --arg base=busybox:1.37
            exit status: Some(1)
            builds' output shown: false, echoing: [], its steps' errors: []
            builds ended: []
            lib/bench.star:83:13: tasks/bench/tools/Dockerfile:2: `${BASE}` expands to the build argument's value: `busybox:1.37` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, `scratch`, an earlier stage or a named context the build is given
                83 |     tools = image("tools/Dockerfile", args = {"BASE": BASE, "NOTE": NOTE})
                   |             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `bench_task()`, called at tasks/bench/task.star:5:1
            pumpkin: tasks/bench/task.star: the task has 1 problem
            pumpkin: to build it again: pumpkin build --arg base=busybox:1.37 --arg model=model-a --config <scratch>/pumpkin.toml <scratch>/tasks/bench/task.star
            printed nothing: true"#]],
    );
}

/// A line for each role of `ir`, a workflow's, in their order: its name, its image, by the
/// number of its first showing, as `seen` counts them, and each of the image's labels that
/// starts `pumpkin.test.`, without that.
fn roles(ir: &str, seen: &mut Vec<String>) -> Vec<String> {
    let ir: serde_json::Value = serde_json::from_str(ir).unwrap();
    let roles = ir["roles"].as_array().unwrap().iter();
    roles
        .map(|role| {
            let image = role["image"].as_str().unwrap();
            let format = "{{json .Config.Labels}}";
            let inspect = ["image", "inspect", "--format", format, image];
            let labels = docker().args(inspect).output().unwrap();
            assert!(labels.status.success(), "{labels:?}");
            let labels: BTreeMap<String, String> = serde_json::from_slice(&labels.stdout).unwrap();
            let labels: Vec<String> = (labels.iter())
                .filter_map(|(key, value)| Some((key.strip_prefix("pumpkin.test.")?, value)))
                .map(|(key, value)| format!(" {key}={value}"))
                .collect();
            let labels = labels.concat();
            let number = 1 + first_seen(seen, image);
            format!(
                "{}: image {number},{labels}",
                role["name"].as_str().unwrap()
            )
        })
        .collect()
}

/// The index of `item` in `seen`, added at its end if it isn't there.
fn first_seen(seen: &mut Vec<String>, item: &str) -> usize {
    seen.iter()
        .position(|seen| seen == item)
        .unwrap_or_else(|| {
            seen.push(item.to_owned());
            seen.len() - 1
        })
}

/// What becomes of `command`, a `build` in `scratch`, as [`report`] shows it, but for what it
/// printed, for the output of `docker build`, and for the lines that say each build ended, as
/// builds end in any order. After its exit status it shows whether there was any output, which
/// steps it named that echo, and each line of a step's own output that starts with `error:`,
/// its step's number and time left out; then the lines that say the builds ended, sorted. And
/// it gives what it printed.
fn built(mut command: Command, scratch: &Scratch, rootful: bool) -> (String, String) {
    let invoked = invocation(&command);
    let output = command.output().unwrap();
    let printed = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    // `pumpkin`'s own lines, and the problems it says once the builds have stopped, the first
    // of which names a place in the library.
    let (mut said, mut progress, mut ended) = (Vec::new(), Vec::new(), Vec::new());
    let mut problems = false;
    for line in stderr.lines() {
        problems |= line.starts_with("lib/");
        let ending = [
            "pumpkin: built an image in ",
            "pumpkin: the build of an image in ",
        ];
        if ending.iter().any(|ending| line.starts_with(ending)) {
            ended.push(line);
        } else if problems || line.starts_with("pumpkin: ") {
            said.push(line);
        } else {
            progress.push(line);
        }
    }
    ended.sort_unstable();
    let mut echoed: Vec<&str> = (progress.iter())
        .filter_map(|line| line.split_once("RUN echo ")?.1.split_once(' '))
        .map(|(echoed, _)| echoed)
        .collect();
    echoed.sort_unstable();
    echoed.dedup();
    // BuildKit's plain progress starts a step's own line with its number and its seconds, as
    // `#6 0.170 `, and repeats it in the error it ends with without the number.
    let timed = |word: &str| {
        let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
        word.strip_prefix('#').is_some_and(digits)
            || word
                .split_once('.')
                .is_some_and(|(whole, part)| digits(whole) && digits(part))
    };
    let mut errors: Vec<&str> = (progress.iter())
        .filter_map(|line| {
            let mut rest = line.trim_start();
            while let Some((word, after)) = rest.split_once(' ')
                && timed(word)
            {
                rest = after;
            }
            rest.starts_with("error: ").then_some(rest)
        })
        .collect();
    errors.sort_unstable();
    errors.dedup();
    let output = Output {
        stdout: Vec::new(),
        stderr: said.join("\n").into(),
        ..output
    };
    let report = report(invoked, &output, &scratch.0, rootful);
    let mut lines: Vec<String> = report.lines().map(str::to_owned).collect();
    let shown = [
        format!(
            "builds' output shown: {}, echoing: {echoed:?}, its steps' errors: {errors:?}",
            !progress.is_empty()
        ),
        format!("builds ended: {ended:?}"),
    ];
    // After the command and its exit status.
    lines.splice(2..2, shown);
    (lines.join("\n"), printed)
}

/// The ID and the repository of each image that the engine has under a tag, labelled `label`,
/// sorted by repository.
fn images(label: &str) -> Vec<(String, String)> {
    let filter = format!("label={label}");
    let format = "{{.ID}} {{.Repository}}";
    let listed = docker()
        .args([
            "image",
            "ls",
            "--no-trunc",
            "--filter",
            &filter,
            "--format",
            format,
        ])
        .output()
        .unwrap();
    assert!(listed.status.success(), "{listed:?}");
    let listed = String::from_utf8(listed.stdout).unwrap();
    let mut images: Vec<(String, String)> = (listed.lines())
        .map(|line| {
            let (id, repository) = line.split_once(' ').unwrap();
            (id.to_owned(), repository.to_owned())
        })
        .collect();
    images.sort_by(|left, right| left.1.cmp(&right.1));
    images
}

/// The images labelled with this, which dropping it removes by each of their tags.
pub(super) struct Labelled(pub(super) String);

impl Drop for Labelled {
    // Nothing here panics, as a panic while a failed test unwinds would abort the test binary; a
    // tag that can't be removed is printed.
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
