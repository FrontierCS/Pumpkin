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
    let scratch = Scratch::new();
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
              Build a task's images and print its workflow's IR.

              `build` checks the task as `pumpkin check` does. It then builds each image the task declares on the
              machine's Docker engine. It prints the workflow's IR, which names each image by its ID, as a run's
              record keeps it in `ir.json`. The standard output holds only the IR, and the builds' output goes to
              the standard error.

              With `[controller]` in the configuration, the controller builds the task, and `build` prints the IR.

              Usage: pumpkin build [OPTIONS] <entry>

              Arguments:
                <entry>
                        A task's entry file, below the nearest directory that holds PUMPKIN.star

              Options:
                    --arg <name>=<value>
                        Set the arg that an `arg()` call declares as <name>

                    --config <file>
                        The machine's configuration, which `pumpkin help config` describes (default:
                        `$HOME/.config/pumpkin/pumpkin.toml`)

                -h, --help
                        Print help

              Examples:
                pumpkin build tasks/bench/task.star --arg model=model-a > workflow.json
                pumpkin build tasks/bench/task.star --arg model=model-a \
                  --config pumpkin.toml
            $ env --unset=HOME PATH=<scratch>/bin pumpkin build tasks/broken/task.star --arg model=model-a
            exit status: Some(1)
            lib/bench.star:52:19: `tasks/broken/agent/Dockerfile` doesn't exist: name a regular file below the root that does
                52 |     agent_image = image("agent/Dockerfile", context = ".", mount = SHARED)
                   |                   ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `bench_task()`, called at tasks/broken/task.star:5:1
            lib/bench.star:110:18: `tasks/broken/judge/Dockerfile` doesn't exist: name a regular file below the root that does
                110 |         "image": image(recipe, mount = SHARED),
                    |                  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `_target()`, called at lib/bench.star:54:9
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
    let scratch = Scratch::new();
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
                "ir_version": "pumpkin.ir/v1",
                "name": "neutral/bench",
                "deadline_seconds": 18000,
                "roles": [
                  {
                    "name": "agent",
                    "image": "<pumpkin-library/harness/codex/layer>",
                    "user": {
                      "uid": 1000,
                      "gid": 1000
                    },
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
                            "-"
                          ],
                          "env": [
                            "CODEX_HOME=/workspace/.codex",
                            "TMPDIR=/workspace/.tmp"
                          ],
                          "cwd": "/workspace"
                        },
                        "dirs": [
                          {
                            "path": "/workspace/.codex",
                            "mode": "0700"
                          },
                          {
                            "path": "/workspace/.tmp",
                            "mode": "0700"
                          }
                        ],
                        "files": [
                          {
                            "path": "/workspace/.codex/config.toml",
                            "mode": "0600",
                            "content": "model = \"model-a\"\nmodel_reasoning_effort = \"high\"\nmodel_provider = \"pumpkin\"\n# Nobody is there to approve a command, and the agent's sandbox confines Codex, whose own\n# sandbox needs `bwrap`, which its pin leaves out.\napproval_policy = \"never\"\nsandbox_mode = \"danger-full-access\"\n# A tool's shell reads no login profile, which could change the environment the command sets.\nallow_login_shell = false\n# Nothing reaches past the model route, the only way out of the sandbox.\nweb_search = \"disabled\"\n\n# No reasoning summary in the progress on the standard error: the model's answers hold them.\nhide_agent_reasoning = true\n\n# The databases Codex makes in its home as it starts stay there, uncollected: `exec` writes them\n# no log, and its session, ephemeral, keeps nothing in them.\n\n# The model endpoint `sandboxd` serves, which needs no credential.\n[model_providers.pumpkin]\n# Any name but `OpenAI` keeps off the features Codex keeps for the provider of that name.\nname = \"Pumpkin\"\nbase_url = \"http://127.0.0.1:11235\"\nwire_api = \"responses\"\nrequires_openai_auth = false\n# A run of hours meets transient failures.\nrequest_max_retries = 100\nstream_max_retries = 100\n\n# The lead agent and as many spawned ones at most, each with one model request in flight at a\n# time: as many requests as the model route takes from a sandbox at once.\n[agents]\nmax_concurrent_threads_per_session = 6\n\n[analytics]\nenabled = false\n\n[features]\nplugins = false\napps = false\nimage_generation = false\n\n[plugins._default]\nenabled = false\n"
                          }
                        ],
                        "prepend_grant_help": true,
                        "stdin": "Solve the task in /task/README.md, working in /workspace.\n\nSubmit a tar for a train call with `sandboxctl`, read its score and the calls left, and go on\nwhile it helps. Submit the final tar with `sandboxctl call final <tar> --arg finalize=true` once, as\nyour last step, then exit.\n",
                        "finalize_grace_seconds": 60,
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
                            "name": "train",
                            "job": {
                              "target": "trainer",
                              "body": {
                                "file": "//lib/bench.star",
                                "def": "train"
                              },
                              "keywords": {
                                "metrics": [
                                  "instances",
                                  "solved",
                                  "submitted",
                                  "invalid",
                                  "duplicate",
                                  "unbounded",
                                  "rejected"
                                ]
                              },
                              "payload_max_bytes": 4194304,
                              "calls": 100,
                              "cumulative_seconds": 12000,
                              "call_timeout_seconds": 600
                            }
                          },
                          {
                            "name": "final",
                            "now": {
                              "body": {
                                "file": "//lib/bench.star",
                                "def": "submit",
                                "arguments": {
                                  "finalize": {
                                    "type": "bool",
                                    "default": false
                                  }
                                }
                              },
                              "payload_max_bytes": 1048576
                            }
                          }
                        ]
                      }
                    }
                  },
                  {
                    "name": "trainer",
                    "image": "<pumpkin/tasks/bench/judge>",
                    "user": {
                      "uid": 65532,
                      "gid": 65532
                    },
                    "needs": {
                      "cpu_millis": 8000,
                      "memory_bytes": 2147483648,
                      "cpu_features": [
                        "avx2"
                      ]
                    },
                    "runtime": {
                      "target": {
                        "cwd": "/pumpkin/home",
                        "env": [
                          "JUDGE_MODE=train",
                          "JUDGE_PAYLOAD=/pumpkin/in/payload/raw"
                        ]
                      }
                    }
                  },
                  {
                    "name": "final_judge",
                    "image": "<pumpkin/tasks/bench/judge>",
                    "user": {
                      "uid": 65532,
                      "gid": 65532
                    },
                    "needs": {
                      "cpu_millis": 8000,
                      "memory_bytes": 2147483648,
                      "cpu_features": [
                        "avx2"
                      ]
                    },
                    "runtime": {
                      "target": {
                        "cwd": "/pumpkin/home",
                        "env": [
                          "JUDGE_MODE=final",
                          "JUDGE_PAYLOAD=/pumpkin/in/payload/raw"
                        ]
                      }
                    }
                  }
                ],
                "final": {
                  "target": "final_judge",
                  "body": {
                    "file": "//lib/bench.star",
                    "def": "final_call"
                  },
                  "payload_max_bytes": 4194304,
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
            lib/bench.star:52:19: building the image failed: `docker build` of `agent/Dockerfile` failed with exit status: 1: its output above says why: fix what Docker reports
                52 |     agent_image = image("agent/Dockerfile", context = ".", mount = SHARED)
                   |                   ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `bench_task()`, called at tasks/failing/task.star:5:1
            lib/bench.star:110:18: building the image failed: `docker build` of `judge/Dockerfile` failed with exit status: 1: its output above says why: fix what Docker reports
                110 |         "image": image(recipe, mount = SHARED),
                    |                  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `_target()`, called at lib/bench.star:54:9
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
    let scratch = Scratch::new();
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
            trainer: image 2, stage=train
            final_judge: image 3, stage=eval
            $ pumpkin build tasks/bench/task.star --arg model=model-a --config pumpkin.toml --arg note=two
            exit status: Some(0)
            builds' output shown: false, echoing: [], its steps' errors: []
            builds ended: ["pumpkin: built an image in pumpkin-library/harness/codex/layer", "pumpkin: built an image in pumpkin/tasks/bench/agent", "pumpkin: built an image in pumpkin/tasks/bench/tools"]
            pumpkin: building an image in pumpkin/tasks/bench/tools
            pumpkin: building an image in pumpkin/tasks/bench/agent
            pumpkin: building an image in pumpkin-library/harness/codex/layer
            agent: image 4, note=two
            trainer: image 2, stage=train
            final_judge: image 3, stage=eval
            $ pumpkin build tasks/bench/task.star --arg model=model-a --config pumpkin.toml --arg base=busybox:1.37
            exit status: Some(1)
            builds' output shown: false, echoing: [], its steps' errors: []
            builds ended: []
            lib/bench.star:55:13: tasks/bench/tools/Dockerfile:2: `${BASE}` expands to the build argument's value: `busybox:1.37` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, `scratch`, an earlier stage or a named context the build is given
                55 |     tools = image("tools/Dockerfile", args = {"BASE": BASE, "NOTE": NOTE})
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
