//! `check`: a task's source, read below the root of its repository, evaluated and checked with
//! no engine and no configuration, then linted by the formatter beside `pumpkin`, the real one;
//! nothing said on success, and else every problem at its place.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::symlink;
use std::path::Path;

use expect_test::expect;
use test_support::BUSYBOX;

use super::{Scratch, Step, check, copy_tree, ran};

/// A neutral library, which writes a task's workflow from the task's own values: Codex works
/// as the agent, on the image that the package's agent recipe builds, and a trainer and the final
/// call's target, on the image of its judge recipe, judge the agent's train calls and its final
/// submission.
const LIBRARY: &str = r##""""A neutral library: `bench_task` writes the workflow of the task in the entry's package,
`tasks/<id>`, from its own values."""

load("@pumpkin//harness/codex/codex.star", "codex")

# The run's choices, each given as `--arg <name>=<value>`, or else its default.
MODEL = arg("model", str)
EFFORT = arg("effort", str, default = "high")
DEADLINE = arg("deadline", int, default = 5 * 3600)
TRAIN_TIMEOUT = arg("train_timeout", int, default = 600)

# The metrics that every judge returns after the task's own.
JUDGE_METRICS = ["unbounded", "rejected"]

# The directories that each recipe copies from, beside its package's.
SHARED = {"shared": "//shared"}

PROMPT = """Solve the task in /task/README.md, working in /workspace.

Submit a tar for a train call with `sandboxctl`, read its score and the calls left, and go on
while it helps. Submit the final tar with `sandboxctl call final <tar> --arg finalize=true` once, as
your last step, then exit.
"""

def bench_task(
        *,
        agent,
        train_calls,
        train_seconds,
        final_timeout,
        submission_bytes,
        judge,
        metrics,
        judge_recipe = "judge/Dockerfile"):
    """The workflow of the task in the entry's package.

    Args:
      agent: the agent's needs.
      train_calls: how many train calls the agent may make.
      train_seconds: how many seconds the train calls may take in all.
      final_timeout: how long the final call may take.
      submission_bytes: the most that a submission may hold.
      judge: the needs of the trainer and of the final call's target.
      metrics: the task's own metrics, which each train call returns.
      judge_recipe: the recipe of the image of the trainer and the target, built in the package's
        directory with `//shared` mounted in it.

    Returns:
      The workflow.
    """
    task = package_name().split("/")[-1]
    agent_image = image("agent/Dockerfile", context = ".", mount = SHARED)
    judges = [
        _target("trainer", "train", recipe = judge_recipe, needs = judge),
        _target("final_judge", "final", recipe = judge_recipe, needs = judge),
    ]
    codex_agent = codex(
        agent_image,
        model = MODEL,
        effort = EFFORT,
        tools = "native",
        workspace = "/workspace",
        prompt = PROMPT,
        finalize_grace_seconds = 60,
    )
    workflow(
        name = "neutral/" + task.replace("_", "-"),
        deadline_seconds = DEADLINE,
        roles = [
            {
                "name": "agent",
                "image": codex_agent["image"],
                "user": {"uid": 1000, "gid": 1000},
                "needs": agent,
                "runtime": {"agent": codex_agent["runtime"]["agent"] | {
                    "grants": [
                        {"name": "train", "job": {
                            "target": "trainer",
                            "body": train,
                            "keywords": {"metrics": metrics + JUDGE_METRICS},
                            "payload_max_bytes": submission_bytes,
                            "calls": train_calls,
                            "cumulative_seconds": train_seconds,
                            "call_timeout_seconds": TRAIN_TIMEOUT,
                        }},
                        {"name": "final", "now": {"body": submit, "payload_max_bytes": MiB}},
                    ],
                }},
            },
        ] + judges,
        final = {
            "target": "final_judge",
            "body": final_call,
            "payload_max_bytes": submission_bytes,
            "timeout_seconds": final_timeout,
        },
    )

def _env(mode):
    """The environment of a judge that judges in `mode`."""
    return ["JUDGE_MODE=" + mode, "JUDGE_PAYLOAD=/pumpkin/in/payload/raw"]

def _target(name, mode, *, recipe, needs):
    """The target role `name`, which judges in `mode`.

    It runs on the image that `recipe` builds.
    """
    return {
        "name": name,
        "image": image(recipe, mount = SHARED),
        "user": {"uid": 65532, "gid": 65532},
        "needs": needs,
        "runtime": {"target": {"cwd": "/pumpkin/home", "env": _env(mode)}},
    }

def train(target: Target, payload: Payload, *, metrics: list[str]):
    """Judges the payload in train mode, in a fresh sandbox of the trainer.

    Args:
      target: the trainer's sandbox.
      payload: the tar to judge.
      metrics: the judge's metrics to return.

    Returns:
      The judge's score and the metrics that `metrics` names.
    """
    target.put("/pumpkin/in/payload/raw", payload)
    target.run(["/opt/judge/run"])
    found = json.decode(target.get("/pumpkin/out/result.json"))
    return {"score": found["score"]} | {name: found["metrics"][name] for name in metrics}

def submit(run: Run, payload: Payload, finalize: bool = False):
    run.set_submission(payload)
    if finalize:
        run.end_work()
    return {}

def final_call(target: Target, submissions):
    """Judges the submission in its target, and shows its score."""
    target.put("/pumpkin/in/payload/raw", submissions["agent"])
    target.run(["/opt/judge/run"])
    return {"score": json.decode(target.get("/pumpkin/out/result.json"))["score"]}
"##;

/// A task's entry file, which the library writes the workflow of, with `cpu_millis` for its
/// agent.
pub(super) fn task(cpu_millis: u32) -> String {
    format!(
        r#""""A neutral task, whose workflow the library writes from its values."""

load("//lib/bench.star", "bench_task")

bench_task(
    agent = {{"cpu_millis": {cpu_millis}, "memory_bytes": 32 * GiB}},
    train_calls = 100,
    train_seconds = 12000,
    final_timeout = 120,
    submission_bytes = 4 * MiB,
    judge = {{"cpu_millis": 8000, "memory_bytes": 2 * GiB, "cpu_features": ["avx2"]}},
    metrics = ["instances", "solved", "submitted", "invalid", "duplicate"],
)
"#
    )
}

/// Writes into `scratch`'s directory `dir` a repository of tasks: its root's marker, the
/// library, the directory that every recipe copies, and the task `tasks/bench`, with its
/// recipes.
pub(super) fn write_repository(scratch: &Scratch, dir: &str) {
    let write = |name: &str, contents: &str| scratch.write(&format!("{dir}{name}"), contents);
    write(
        "PUMPKIN.star",
        "# The root of a repository of tasks, where `//` labels start.\n",
    );
    write("lib/bench.star", LIBRARY);
    write("shared/README", "What every image of the tasks copies.\n");
    write("tasks/bench/task.star", &task(8000));
    let agent = format!("FROM {BUSYBOX}\nCOPY shared/ /opt/shared/\nWORKDIR /workspace\n");
    write("tasks/bench/agent/Dockerfile", &agent);
    let judge = format!("FROM {BUSYBOX}\nCOPY shared/ /opt/shared/\nCOPY judge/run /opt/judge/\n");
    write("tasks/bench/judge/Dockerfile", &judge);
    write("tasks/bench/judge/run", "#!/bin/sh\n");
}

/// Writes into `scratch` the repository of [`write_repository`], with its trainer's and judge's
/// images two stages of one recipe, `train` and `eval`, and its agent's image built on the
/// tools' image, from a recipe whose `FROM` is the arg `base`, and which keeps the arg `note` as
/// a label, with the toolchain's `/usr/local/go` copied in, a stage `go` of its recipe, which a
/// later stage that fails follows, as the arg `toolchain` names it; each image labelled `label`.
pub(super) fn write_variants(scratch: &Scratch, label: &str) {
    write_repository(scratch, "");
    let library = fs::read_to_string(scratch.0.join("lib/bench.star")).unwrap();
    let library = library
        .replace(
            "TRAIN_TIMEOUT = arg(\"train_timeout\", int, default = 600)\n",
            &format!(
                "TRAIN_TIMEOUT = arg(\"train_timeout\", int, default = 600)\n\
                 BASE = arg(\"base\", str, default = \"{BUSYBOX}\")\n\
                 NOTE = arg(\"note\", str, default = \"one\")\n\
                 TOOLCHAIN = arg(\"toolchain\", str, default = \"Go\")\n"
            ),
        )
        .replace(
            "\"image\": image(recipe, mount = SHARED),",
            "\"image\": image(recipe, mount = SHARED, target = mode if mode == \"train\" else \
             \"eval\"),",
        )
        .replace(
            "    agent_image = image(\"agent/Dockerfile\", context = \".\", mount = SHARED)",
            "    tools = image(\"tools/Dockerfile\", args = {\"BASE\": BASE, \"NOTE\": NOTE})\n    \
             toolchain = image(\"toolchain/Dockerfile\", target = TOOLCHAIN)\n    \
             built_on = {\"build-tools\": tools, \"toolchain\": toolchain}\n    \
             agent_image = image(\"agent/Dockerfile\", context = \".\", mount = SHARED, \
             contexts = built_on)",
        );
    scratch.write("lib/bench.star", &library);
    let tools = format!(
        "ARG BASE\nFROM ${{BASE}}\nARG NOTE\nRUN echo tools && true\nLABEL {label} \
         pumpkin.test.note=$NOTE\n"
    );
    scratch.write("tasks/bench/tools/Dockerfile", &tools);
    let toolchain = format!(
        "FROM {BUSYBOX} AS go\nRUN echo toolchain && mkdir --parents /usr/local/go \
         && echo go1.24 > /usr/local/go/VERSION\nLABEL {label}\nFROM go\nRUN false\n"
    );
    scratch.write("tasks/bench/toolchain/Dockerfile", &toolchain);
    let agent = format!(
        "FROM build-tools\nCOPY --from=toolchain /usr/local/go /usr/local/go\n\
         COPY shared/ /opt/shared/\nRUN echo agent && test -f /usr/local/go/VERSION\n\
         LABEL {label}\n"
    );
    scratch.write("tasks/bench/agent/Dockerfile", &agent);
    let judge = format!(
        "FROM {BUSYBOX} AS train\nCOPY judge/run /opt/judge/\nRUN echo train && true\n\
         LABEL {label} pumpkin.test.stage=train\n\
         FROM train AS eval\nRUN echo eval && true\nLABEL pumpkin.test.stage=eval\n"
    );
    scratch.write("tasks/bench/judge/Dockerfile", &judge);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn tasks() {
    let scratch = Scratch::new();
    write_repository(&scratch, "");
    scratch.write("tasks/broken/task.star", &task(0));
    let unpinned = "FROM busybox:1.37\nCOPY judge/run /opt/judge/\n";
    scratch.write("tasks/broken/judge/Dockerfile", unpinned);
    scratch.write("tasks/other/task.star", &task(8000));
    // A `docker` that says whether it ran, which a check never does.
    fs::create_dir(scratch.0.join("bin")).unwrap();
    scratch.write_executable("bin/docker", b"#!/bin/sh\ntouch \"$0.ran\"\nexit 1\n");
    let mut alone = scratch.pumpkin(&["check", "tasks/bench/task.star", "--arg", "model=model-a"]);
    alone.env("PATH", scratch.0.join("bin")).env_remove("HOME");
    let alone = ran(alone, &scratch.0, false);
    let docker = scratch.0.join("bin/docker.ran").exists();
    let alone = Step::Ran(format!("{alone}\ndocker was run: {docker}"));
    let pumpkin = |args: &[&str]| Step::from(scratch.pumpkin(args));
    let bench = |args: &[&str]| {
        let mut command = scratch.pumpkin(&["check", "tasks/bench/task.star"]);
        command.args(args);
        Step::from(command)
    };
    // An entry and an `--arg`'s value that each hold a byte that isn't UTF-8: usage problems,
    // not an arg dropped.
    let mut not_utf8 = scratch.pumpkin(&["check"]);
    not_utf8.arg(OsStr::from_bytes(b"tasks/\xff/task.star"));
    not_utf8.arg("--arg").arg(OsStr::from_bytes(b"model=\xff"));
    let unused: Vec<String> = ('a'..='u').map(|name| format!("--arg={name}=1")).collect();
    let mut many = vec!["--arg", "model=model-a"];
    many.extend(unused.iter().map(String::as_str));
    let steps = [
        pumpkin(&["check", "--help"]),
        alone,
        bench(&[
            "--arg=model=model-a",
            "--arg",
            "effort=low",
            "--arg",
            "deadline=3600",
        ]),
        pumpkin(&["check", "tasks/broken/task.star", "--arg", "model=model-a"]),
        pumpkin(&[
            "check",
            "tasks/broken/task.star",
            "tasks/bench/task.star",
            "tasks/other/task.star",
            "nowhere/task.star",
            "--arg",
            "model=model-a",
        ]),
        bench(&["--arg", "mdoel=model-a"]),
        bench(&["--arg", "model=model-a", "--arg", "deadline=1h"]),
        bench(&[
            "--arg",
            "model=model-a",
            "--arg",
            "mdoel=x",
            "--arg",
            "effrot=low",
        ]),
        bench(&many),
        pumpkin(&["check"]),
        pumpkin(&[
            "check",
            "task.json",
            "notes.md",
            "--arg",
            "model",
            "--arg",
            "=x",
            "--arg",
            "a=1",
            "--arg",
            "a=2",
            "--arg",
        ]),
        Step::from(not_utf8),
    ];
    check(
        &scratch,
        steps,
        false,
        &[],
        &expect![[r#"
            $ pumpkin check --help
            exit status: Some(0)
            printed:
              Check each task's source: evaluate its entry file, and the files that it loads,
              with the args given, then check the recipe of each image that it declares, and
              its workflow, then lint those files, and say every problem found, in all the
              tasks.

              Usage: pumpkin check [OPTIONS] <entry>...

              Arguments:
                <entry>...
                        A task's entry file, a .star file below the root of its repository: the
                        nearest directory, at or above the entry's, that holds PUMPKIN.star.
                        Each task is checked with the same args.

              Options:
                    --arg <name>=<value>
                        The value of the arg that an arg() call declares as <name>, read as the
                        type that it declares. Each arg is given at most once, and one with no
                        default must be, for each task.

                -h, --help
                        Print this help.

              Examples:
                pumpkin check tasks/bench/task.star --arg model=model-a
                pumpkin check tasks/bench/task.star --arg model=model-a --arg deadline=3600
                pumpkin check tasks/*/task.star --arg model=model-a

              Exit status:
                0  The check found no problem.
                1  A task has problems, each said at its place; or another error. Every
                   task is checked all the same.
                2  A usage error.
              Every exit but 0 ends with a `pumpkin:` line on the standard error, which says
              why, and an exit of 1 then with the command that checks again the tasks that
              failed, their entries' paths made absolute. On success, `check` prints nothing.

              A problem is said at its file's path below the root, its line and its column,
              then its source line, with carets under what it is about; a recipe's, at the
              image() call that declares it. Past 20 problems, the rest are counted.

              The lint, once a task has no other problem, finds in the files that it read
              mistakes that evaluating them doesn't, as they may lie on paths that the args
              given don't run: a name loaded but never used; a name bound but never used, as
              a function's local, or a function or a global whose name starts with `_`; an
              expression whose value is never used; a name used where a path to the use may
              not have bound it; a statement never reached; a function that returns a value
              on some paths but not on others; and what the language refuses once run, as
              iterating over a string. It holds each file to one convention too: a docstring
              for the file and for each public function, which names its parameters under
              `Args:` and says what it returns under `Returns:`; and names in
              lower_snake_case, constants in UPPER_SNAKE_CASE, and none of `l`, `I` or `O`.
              It runs the formatter beside `pumpkin`, as `pumpkin fmt` does.

              `check` reads only files below the root, with no symbolic link on the way to
              them, and PUMPKIN.star holds only comments. It reaches no Docker engine, no
              network and no configuration, so an image's build may still fail, as on a
              recipe's step that fails.
            $ env --unset=HOME PATH=<scratch>/bin pumpkin check tasks/bench/task.star --arg model=model-a
            exit status: Some(0)
            docker was run: false
            $ pumpkin check tasks/bench/task.star --arg=model=model-a --arg effort=low --arg deadline=3600
            exit status: Some(0)
            $ pumpkin check tasks/broken/task.star --arg model=model-a
            exit status: Some(1)
            lib/bench.star:52:19: `tasks/broken/agent/Dockerfile` doesn't exist: name a regular file below the root that does
                52 |     agent_image = image("agent/Dockerfile", context = ".", mount = SHARED)
                   |                   ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `bench_task()`, called at tasks/broken/task.star:5:1
            lib/bench.star:110:18: tasks/broken/judge/Dockerfile:1: `busybox:1.37` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, `scratch`, an earlier stage or a named context the build is given
                110 |         "image": image(recipe, mount = SHARED),
                    |                  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `_target()`, called at lib/bench.star:54:9
                in `bench_task()`, called at tasks/broken/task.star:5:1
            tasks/broken/task.star:6:28: `cpu_millis` can't be `0`: expected a positive int
                6 |     agent = {"cpu_millis": 0, "memory_bytes": 32 * GiB},
                  |                            ^
                the IR's /roles/0/needs/cpu_millis
            pumpkin: tasks/broken/task.star: the task has 3 problems
            pumpkin: to check it again: pumpkin check --arg model=model-a <scratch>/tasks/broken/task.star
            $ pumpkin check tasks/broken/task.star tasks/bench/task.star tasks/other/task.star nowhere/task.star --arg model=model-a
            exit status: Some(1)
            lib/bench.star:52:19: `tasks/broken/agent/Dockerfile` doesn't exist: name a regular file below the root that does
                52 |     agent_image = image("agent/Dockerfile", context = ".", mount = SHARED)
                   |                   ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `bench_task()`, called at tasks/broken/task.star:5:1
            lib/bench.star:110:18: tasks/broken/judge/Dockerfile:1: `busybox:1.37` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, `scratch`, an earlier stage or a named context the build is given
                110 |         "image": image(recipe, mount = SHARED),
                    |                  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `_target()`, called at lib/bench.star:54:9
                in `bench_task()`, called at tasks/broken/task.star:5:1
            tasks/broken/task.star:6:28: `cpu_millis` can't be `0`: expected a positive int
                6 |     agent = {"cpu_millis": 0, "memory_bytes": 32 * GiB},
                  |                            ^
                the IR's /roles/0/needs/cpu_millis
            pumpkin: tasks/broken/task.star: the task has 3 problems
            lib/bench.star:52:19: `tasks/other/agent/Dockerfile` doesn't exist: name a regular file below the root that does
                52 |     agent_image = image("agent/Dockerfile", context = ".", mount = SHARED)
                   |                   ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `bench_task()`, called at tasks/other/task.star:5:1
            lib/bench.star:110:18: `tasks/other/judge/Dockerfile` doesn't exist: name a regular file below the root that does
                110 |         "image": image(recipe, mount = SHARED),
                    |                  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `_target()`, called at lib/bench.star:54:9
                in `bench_task()`, called at tasks/other/task.star:5:1
            pumpkin: tasks/other/task.star: the task has 2 problems
            pumpkin: nowhere/task.star: the entry's directory nowhere can't be found (No such file or directory (os error 2)): name the entry by a path that exists
            pumpkin: to check them again: pumpkin check --arg model=model-a <scratch>/tasks/broken/task.star <scratch>/tasks/other/task.star <scratch>/nowhere/task.star
            $ pumpkin check tasks/bench/task.star --arg mdoel=model-a
            exit status: Some(1)
            lib/bench.star:7:9: `--arg model` is required, as this `arg()` gives it no default: give it as `--arg model=<value>`
                7 | MODEL = arg("model", str)
                  |         ^^^^^^^^^^^^^^^^^
                `--arg mdoel` is given, and no `arg()` call before this one declares it
            pumpkin: tasks/bench/task.star: the task has 1 problem
            pumpkin: to check it again: pumpkin check --arg mdoel=model-a <scratch>/tasks/bench/task.star
            $ pumpkin check tasks/bench/task.star --arg model=model-a --arg deadline=1h
            exit status: Some(1)
            lib/bench.star:9:12: `--arg deadline` is `"1h"`, which `int` can't read: give it decimal digits, after `-` or `+` or neither, from -2^63 to 2^63 - 1
                9 | DEADLINE = arg("deadline", int, default = 5 * 3600)
                  |            ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
            pumpkin: tasks/bench/task.star: the task has 1 problem
            pumpkin: to check it again: pumpkin check --arg deadline=1h --arg model=model-a <scratch>/tasks/bench/task.star
            $ pumpkin check tasks/bench/task.star --arg model=model-a --arg mdoel=x --arg effrot=low
            exit status: Some(1)
            tasks/bench/task.star: `--arg effrot` is declared by no `arg()` call: give one of `deadline`, `effort`, `model`, `train_timeout`
            tasks/bench/task.star: `--arg mdoel` is declared by no `arg()` call: give one of `deadline`, `effort`, `model`, `train_timeout`
            pumpkin: tasks/bench/task.star: the task has 2 problems
            pumpkin: to check it again: pumpkin check --arg effrot=low --arg mdoel=x --arg model=model-a <scratch>/tasks/bench/task.star
            $ pumpkin check tasks/bench/task.star --arg model=model-a --arg=a=1 --arg=b=1 --arg=c=1 --arg=d=1 --arg=e=1 --arg=f=1 --arg=g=1 --arg=h=1 --arg=i=1 --arg=j=1 --arg=k=1 --arg=l=1 --arg=m=1 --arg=n=1 --arg=o=1 --arg=p=1 --arg=q=1 --arg=r=1 --arg=s=1 --arg=t=1 --arg=u=1
            exit status: Some(1)
            tasks/bench/task.star: `--arg a` is declared by no `arg()` call: give one of `deadline`, `effort`, `model`, `train_timeout`
            tasks/bench/task.star: `--arg b` is declared by no `arg()` call: give one of `deadline`, `effort`, `model`, `train_timeout`
            tasks/bench/task.star: `--arg c` is declared by no `arg()` call: give one of `deadline`, `effort`, `model`, `train_timeout`
            tasks/bench/task.star: `--arg d` is declared by no `arg()` call: give one of `deadline`, `effort`, `model`, `train_timeout`
            tasks/bench/task.star: `--arg e` is declared by no `arg()` call: give one of `deadline`, `effort`, `model`, `train_timeout`
            tasks/bench/task.star: `--arg f` is declared by no `arg()` call: give one of `deadline`, `effort`, `model`, `train_timeout`
            tasks/bench/task.star: `--arg g` is declared by no `arg()` call: give one of `deadline`, `effort`, `model`, `train_timeout`
            tasks/bench/task.star: `--arg h` is declared by no `arg()` call: give one of `deadline`, `effort`, `model`, `train_timeout`
            tasks/bench/task.star: `--arg i` is declared by no `arg()` call: give one of `deadline`, `effort`, `model`, `train_timeout`
            tasks/bench/task.star: `--arg j` is declared by no `arg()` call: give one of `deadline`, `effort`, `model`, `train_timeout`
            tasks/bench/task.star: `--arg k` is declared by no `arg()` call: give one of `deadline`, `effort`, `model`, `train_timeout`
            tasks/bench/task.star: `--arg l` is declared by no `arg()` call: give one of `deadline`, `effort`, `model`, `train_timeout`
            tasks/bench/task.star: `--arg m` is declared by no `arg()` call: give one of `deadline`, `effort`, `model`, `train_timeout`
            tasks/bench/task.star: `--arg n` is declared by no `arg()` call: give one of `deadline`, `effort`, `model`, `train_timeout`
            tasks/bench/task.star: `--arg o` is declared by no `arg()` call: give one of `deadline`, `effort`, `model`, `train_timeout`
            tasks/bench/task.star: `--arg p` is declared by no `arg()` call: give one of `deadline`, `effort`, `model`, `train_timeout`
            tasks/bench/task.star: `--arg q` is declared by no `arg()` call: give one of `deadline`, `effort`, `model`, `train_timeout`
            tasks/bench/task.star: `--arg r` is declared by no `arg()` call: give one of `deadline`, `effort`, `model`, `train_timeout`
            tasks/bench/task.star: `--arg s` is declared by no `arg()` call: give one of `deadline`, `effort`, `model`, `train_timeout`
            tasks/bench/task.star: `--arg t` is declared by no `arg()` call: give one of `deadline`, `effort`, `model`, `train_timeout`
            and 1 more problem: fix these first, then run again
            pumpkin: tasks/bench/task.star: the task has 21 problems
            pumpkin: to check it again: pumpkin check --arg a=1 --arg b=1 --arg c=1 --arg d=1 --arg e=1 --arg f=1 --arg g=1 --arg h=1 --arg i=1 --arg j=1 --arg k=1 --arg l=1 --arg m=1 --arg model=model-a --arg n=1 --arg o=1 --arg p=1 --arg q=1 --arg r=1 --arg s=1 --arg t=1 --arg u=1 <scratch>/tasks/bench/task.star
            $ pumpkin check
            exit status: Some(2)
            error: the following required arguments were not provided:
              <entry>...

            Usage: pumpkin check <entry>...
            pumpkin: to read how to use it: pumpkin help check
            $ pumpkin check task.json notes.md --arg model --arg =x --arg a=1 --arg a=2 --arg
            exit status: Some(2)
            error: invalid value 'task.json' for '<entry>...': `task.json` names no `.star` file: name a task's entry file, whose name ends in `.star`
            pumpkin: to read how to use it: pumpkin help check
            $ pumpkin check tasks/�/task.star --arg model=�
            exit status: Some(2)
            error: invalid value 'tasks/�/task.star' for '<entry>...': it isn't UTF-8, as the commands that `pumpkin` says to run next name it: name one that is
            pumpkin: to read how to use it: pumpkin help check"#]],
    );
}

#[test]
fn lints() {
    let scratch = Scratch::new();
    write_repository(&scratch, "");
    let library = fs::read_to_string(scratch.0.join("lib/bench.star")).unwrap();
    let sloppy = r#"
def sometimes(x):
    """1 if `x` is true."""
    if x:
        return 1

def sloppy(calls):
    """`calls`, as a binding that a path may leave unbound."""
    unused = calls
    calls == 100
    if calls:
        bound = calls
    return bound
    fail("never")

def _helper():
    pass
"#;
    scratch.write("lib/bench.star", &format!("{library}{sloppy}"));
    let entry = fs::read_to_string(scratch.0.join("tasks/bench/task.star")).unwrap();
    let entry = entry.replace("\"bench_task\")", "\"bench_task\", \"sometimes\")");
    scratch.write("tasks/bench/task.star", &entry);
    let steps = [scratch.pumpkin(&["check", "tasks/bench/task.star", "--arg", "model=model-a"])];
    check(
        &scratch,
        steps,
        false,
        &[],
        &expect![[r#"
            $ pumpkin check tasks/bench/task.star --arg model=model-a
            exit status: Some(1)
            tasks/bench/task.star:3:41: this name is loaded, but never used: drop it from the `load`
                3 | load("//lib/bench.star", "bench_task", "sometimes")
                  |                                         ^^^^^^^^^
            lib/bench.star:144:1: this function returns a value on some paths, but not on others: return one on every path, or on none
                144 | def sometimes(x):
                    | ^^^^^^^^^^^^^^^^^
            lib/bench.star:150:5: this docstring's `Args:` doesn't name the function's parameters: give each a line, and no other name
                150 |     """`calls`, as a binding that a path may leave unbound."""
                    |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
            lib/bench.star:150:5: this docstring doesn't say what the function returns: say it under `Returns:`
                150 |     """`calls`, as a binding that a path may leave unbound."""
                    |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
            lib/bench.star:151:5: this is bound, but never used: use it, or drop it
                151 |     unused = calls
                    |     ^^^^^^
            lib/bench.star:152:5: this expression's value is never used: use it, or drop the statement
                152 |     calls == 100
                    |     ^^^^^^^^^^^^
            lib/bench.star:155:12: this name may be unbound here, as a path to this use doesn't bind it: bind it on every path to this use
                155 |     return bound
                    |            ^^^^^
            lib/bench.star:156:5: this statement is never reached, as one before it ends its block: drop it, or move it before that one
                156 |     fail("never")
                    |     ^^^^^^^^^^^^^
            lib/bench.star:158:1: this is bound, but never used: use it, or drop it
                158 | def _helper():
                    | ^^^^^^^^^^^^^^
            pumpkin: tasks/bench/task.star: the task has 9 problems
            pumpkin: to check it again: pumpkin check --arg model=model-a <scratch>/tasks/bench/task.star"#]],
    );
}

#[test]
fn variants() {
    let scratch = Scratch::new();
    write_variants(&scratch, "pumpkin.test=unbuilt");
    let bench = |args: &[&str]| {
        let mut command = scratch.pumpkin(&["check", "tasks/bench/task.star", "--arg", "model=m"]);
        command.args(args);
        Step::from(command)
    };
    let steps = [
        bench(&[]),
        bench(&["--arg", "base=busybox:1.37"]),
        bench(&["--arg", "toolchain=rust"]),
    ];
    check(
        &scratch,
        steps,
        false,
        &[],
        &expect![[r#"
            $ pumpkin check tasks/bench/task.star --arg model=m
            exit status: Some(0)
            $ pumpkin check tasks/bench/task.star --arg model=m --arg base=busybox:1.37
            exit status: Some(1)
            lib/bench.star:55:13: tasks/bench/tools/Dockerfile:2: `${BASE}` expands to the build argument's value: `busybox:1.37` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, `scratch`, an earlier stage or a named context the build is given
                55 |     tools = image("tools/Dockerfile", args = {"BASE": BASE, "NOTE": NOTE})
                   |             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `bench_task()`, called at tasks/bench/task.star:5:1
            pumpkin: tasks/bench/task.star: the task has 1 problem
            pumpkin: to check it again: pumpkin check --arg base=busybox:1.37 --arg model=m <scratch>/tasks/bench/task.star
            $ pumpkin check tasks/bench/task.star --arg model=m --arg toolchain=rust
            exit status: Some(1)
            lib/bench.star:56:17: tasks/bench/toolchain/Dockerfile: the build's target, `rust`, names no stage of the recipe: name one that a `FROM <image> AS <name>` begins
                56 |     toolchain = image("toolchain/Dockerfile", target = TOOLCHAIN)
                   |                 ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `bench_task()`, called at tasks/bench/task.star:5:1
            pumpkin: tasks/bench/task.star: the task has 1 problem
            pumpkin: to check it again: pumpkin check --arg model=m --arg toolchain=rust <scratch>/tasks/bench/task.star"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn roots() {
    let scratch = Scratch::new();
    write_repository(&scratch, "repo/");
    write_repository(&scratch, "unshared/");
    fs::remove_dir_all(scratch.0.join("unshared/shared")).unwrap();
    // Repositories for the nested mounts, and for the nested roots.
    write_repository(&scratch, "nested/");
    write_repository(&scratch, "outer/");
    // Mounts that nest: `shared/sub` is inside `shared`.
    let nested = LIBRARY.replace(
        "SHARED = {\"shared\": \"//shared\"}",
        "SHARED = {\"shared\": \"//shared\", \"shared/sub\": \"//sub\"}",
    );
    let (plain, pinned) = (task(8000), format!("FROM {BUSYBOX}\n"));
    let loads = plain.replace("//lib/bench.star", "//escape/bench.star");
    let mounted = plain.replace(
        "    metrics = [",
        "    judge_recipe = \"shared/judge.Dockerfile\",\n    metrics = [",
    );
    let in_sub = plain.replace(
        "    metrics = [",
        "    judge_recipe = \"shared/sub/Dockerfile\",\n    metrics = [",
    );
    let files = [
        ("outside/bench.star", LIBRARY),
        ("outside/Dockerfile", &pinned),
        ("outside/task.star", &plain),
        ("repo/tasks/loads/task.star", &loads),
        ("repo/tasks/linked/task.star", &plain),
        ("repo/tasks/linked/judge/run", "#!/bin/sh\n"),
        // The judge's recipe is in the directory mounted at `shared`.
        ("repo/tasks/mounted/task.star", &mounted),
        ("repo/tasks/mounted/agent/Dockerfile", &pinned),
        ("repo/shared/judge.Dockerfile", "FROM busybox\n"),
        ("unshared/tasks/mounted/task.star", &mounted),
        ("unshared/tasks/mounted/agent/Dockerfile", &pinned),
        (
            "repo/tasks/marked/PUMPKIN.star/README",
            "No marker, as a directory.\n",
        ),
        (
            "repo/tasks/marked/task.star",
            "load(\"//lib/dir.star\", \"x\")\n",
        ),
        (
            "repo/lib/dir.star/README",
            "A directory, which no `load` reads.\n",
        ),
        ("repo/tasks/my task/task.star", &plain),
        (
            "newer/PUMPKIN.star",
            "# The root.\r\n\r\n  pumpkin = \"1.0\"\r\n",
        ),
        ("newer/task.star", &plain),
        // The judge's recipe is below both mounts, so it is read from the deeper one, `//sub`;
        // `//shared` holds none at `sub/Dockerfile`.
        ("nested/lib/bench.star", &nested),
        ("nested/tasks/bench/task.star", &in_sub),
        ("nested/sub/Dockerfile", "FROM busybox\n"),
        // A repository inside a repository: the nearest root is the entry's, as Bazel's nearest
        // boundary file and Cargo's nearest ancestor are. The library is outside it.
        ("outer/inner/PUMPKIN.star", "# The inner root.\n"),
        ("outer/inner/task.star", &plain),
        ("linkmark/PUMPKIN.real", "# Comments, behind a link.\n"),
        ("linkmark/task.star", &plain),
    ];
    for (name, contents) in files {
        scratch.write(name, contents);
    }
    let links = [
        // A marker that is a link marks the root, and is then refused.
        ("PUMPKIN.real", "linkmark/PUMPKIN.star"),
        ("../outside", "repo/escape"),
        ("../outside", "unshared/shared"),
        ("../../../outside", "repo/tasks/linked/agent"),
        (
            "../../../../outside/Dockerfile",
            "repo/tasks/linked/judge/Dockerfile",
        ),
    ];
    for (target, at) in links {
        symlink(target, scratch.0.join(at)).unwrap();
    }
    let pumpkin = |entry: &str| Step::from(scratch.pumpkin(&["check", entry, "--arg=model=m"]));
    let steps = [
        pumpkin("repo/tasks/bench/task.star"),
        pumpkin("repo/tasks/loads/task.star"),
        pumpkin("repo/tasks/linked/task.star"),
        pumpkin("repo/tasks/mounted/task.star"),
        pumpkin("unshared/tasks/mounted/task.star"),
        pumpkin("repo/tasks/none.star"),
        pumpkin("repo/tasks/marked/task.star"),
        pumpkin("repo/tasks/my task/task.star"),
        pumpkin("outside/task.star"),
        pumpkin("newer/task.star"),
        pumpkin("nested/tasks/bench/task.star"),
        pumpkin("outer/inner/task.star"),
        pumpkin("linkmark/task.star"),
    ];
    check(
        &scratch,
        steps,
        false,
        &[],
        &expect![[r#"
            $ pumpkin check repo/tasks/bench/task.star --arg=model=m
            exit status: Some(0)
            $ pumpkin check repo/tasks/loads/task.star --arg=model=m
            exit status: Some(1)
            tasks/loads/task.star:3:6: loading `//escape/bench.star`: `escape` is a symbolic link, which could lead out of the root: put what it links to there instead
                3 | load("//escape/bench.star", "bench_task")
                  |      ^^^^^^^^^^^^^^^^^^^^^
            pumpkin: repo/tasks/loads/task.star: the task has 1 problem
            pumpkin: to check it again: pumpkin check --arg model=m <scratch>/repo/tasks/loads/task.star
            $ pumpkin check repo/tasks/linked/task.star --arg=model=m
            exit status: Some(1)
            lib/bench.star:52:19: `tasks/linked/agent` is a symbolic link, which could lead out of the root: put what it links to there instead
                52 |     agent_image = image("agent/Dockerfile", context = ".", mount = SHARED)
                   |                   ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `bench_task()`, called at tasks/linked/task.star:5:1
            lib/bench.star:110:18: `tasks/linked/judge/Dockerfile` is a symbolic link, which could lead out of the root: put what it links to there instead
                110 |         "image": image(recipe, mount = SHARED),
                    |                  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `_target()`, called at lib/bench.star:54:9
                in `bench_task()`, called at tasks/linked/task.star:5:1
            pumpkin: repo/tasks/linked/task.star: the task has 2 problems
            pumpkin: to check it again: pumpkin check --arg model=m <scratch>/repo/tasks/linked/task.star
            $ pumpkin check repo/tasks/mounted/task.star --arg=model=m
            exit status: Some(1)
            lib/bench.star:110:18: shared/judge.Dockerfile:1: `busybox` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, `scratch`, an earlier stage or a named context the build is given
                110 |         "image": image(recipe, mount = SHARED),
                    |                  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `_target()`, called at lib/bench.star:54:9
                in `bench_task()`, called at tasks/mounted/task.star:5:1
            pumpkin: repo/tasks/mounted/task.star: the task has 1 problem
            pumpkin: to check it again: pumpkin check --arg model=m <scratch>/repo/tasks/mounted/task.star
            $ pumpkin check unshared/tasks/mounted/task.star --arg=model=m
            exit status: Some(1)
            lib/bench.star:52:19: `shared` is a symbolic link, which could lead out of the root: put what it links to there instead
                52 |     agent_image = image("agent/Dockerfile", context = ".", mount = SHARED)
                   |                   ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `bench_task()`, called at tasks/mounted/task.star:5:1
            lib/bench.star:110:18: `shared` is a symbolic link, which could lead out of the root: put what it links to there instead
                110 |         "image": image(recipe, mount = SHARED),
                    |                  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `_target()`, called at lib/bench.star:54:9
                in `bench_task()`, called at tasks/mounted/task.star:5:1
            pumpkin: unshared/tasks/mounted/task.star: the task has 2 problems
            pumpkin: to check it again: pumpkin check --arg model=m <scratch>/unshared/tasks/mounted/task.star
            $ pumpkin check repo/tasks/none.star --arg=model=m
            exit status: Some(1)
            tasks/none.star: `tasks/none.star` doesn't exist: name a regular file below the root that does
            pumpkin: repo/tasks/none.star: the task has 1 problem
            pumpkin: to check it again: pumpkin check --arg model=m <scratch>/repo/tasks/none.star
            $ pumpkin check repo/tasks/marked/task.star --arg=model=m
            exit status: Some(1)
            tasks/marked/task.star:1:6: loading `//lib/dir.star`: `lib/dir.star` is no regular file: name a regular file below the root
                1 | load("//lib/dir.star", "x")
                  |      ^^^^^^^^^^^^^^^^
            pumpkin: repo/tasks/marked/task.star: the task has 1 problem
            pumpkin: to check it again: pumpkin check --arg model=m <scratch>/repo/tasks/marked/task.star
            $ pumpkin check repo/tasks/my task/task.star --arg=model=m
            exit status: Some(1)
            pumpkin: repo/tasks/my task/task.star: the entry, below the root <scratch>/repo: `//tasks/my task/task.star` is no label: after `//`, write `/`-separated components of ASCII letters, digits, `.`, `_` and `-`, none of them empty, `.` or `..`
            pumpkin: to check it again: pumpkin check --arg model=m '<scratch>/repo/tasks/my task/task.star'
            $ pumpkin check outside/task.star --arg=model=m
            exit status: Some(1)
            pumpkin: outside/task.star: no directory at or above <scratch>/outside holds `PUMPKIN.star`: put one at the root of the task's repository, where its `//` labels start
            pumpkin: to check it again: pumpkin check --arg model=m <scratch>/outside/task.star
            $ pumpkin check newer/task.star --arg=model=m
            exit status: Some(1)
            pumpkin: newer/task.star: <scratch>/newer/PUMPKIN.star:3:3: this pumpkin reads only comments in `PUMPKIN.star`: the repository may need a newer pumpkin
            pumpkin: to check it again: pumpkin check --arg model=m <scratch>/newer/task.star
            $ pumpkin check nested/tasks/bench/task.star --arg=model=m
            exit status: Some(1)
            lib/bench.star:110:18: sub/Dockerfile:1: `busybox` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, `scratch`, an earlier stage or a named context the build is given
                110 |         "image": image(recipe, mount = SHARED),
                    |                  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `_target()`, called at lib/bench.star:54:9
                in `bench_task()`, called at tasks/bench/task.star:5:1
            pumpkin: nested/tasks/bench/task.star: the task has 1 problem
            pumpkin: to check it again: pumpkin check --arg model=m <scratch>/nested/tasks/bench/task.star
            $ pumpkin check outer/inner/task.star --arg=model=m
            exit status: Some(1)
            task.star:3:6: loading `//lib/bench.star`: `lib/bench.star` doesn't exist: name a regular file below the root that does
                3 | load("//lib/bench.star", "bench_task")
                  |      ^^^^^^^^^^^^^^^^^^
            pumpkin: outer/inner/task.star: the task has 1 problem
            pumpkin: to check it again: pumpkin check --arg model=m <scratch>/outer/inner/task.star
            $ pumpkin check linkmark/task.star --arg=model=m
            exit status: Some(1)
            pumpkin: linkmark/task.star: <scratch>/linkmark/PUMPKIN.star: `PUMPKIN.star` is a symbolic link, which could lead out of the root: put what it links to there instead
            pumpkin: to check it again: pumpkin check --arg model=m <scratch>/linkmark/task.star"#]],
    );
}

#[test]
fn library() {
    let scratch = Scratch::new();
    write_repository(&scratch, "");
    write_repository(&scratch, "images/");
    let library = LIBRARY.replace(
        "    agent_image = image(\"agent/Dockerfile\", context = \".\", mount = SHARED)",
        "    agent_image = image(\n        \"Dockerfile\",\n        context = \"@pumpkin//harness/one\",\n        \
         mount = {\"m\": \"@pumpkin//harness/two\"},\n    )",
    );
    scratch.write("images/lib/bench.star", &library);
    let loads = "load(\"@pumpkin//harness/codex.star\", \"codex_agent\")\n";
    scratch.write("tasks/loads/task.star", loads);
    // The library's `codex`, whose recipe's path a task's recipe that fails the check shares.
    write_repository(&scratch, "served/");
    scratch.write("served/harness/codex/layer/Dockerfile", "FROM busybox\n");
    let pumpkin = |entry: &str| Step::from(scratch.pumpkin(&["check", entry, "--arg=model=m"]));
    let steps = [
        pumpkin("tasks/loads/task.star"),
        pumpkin("images/tasks/bench/task.star"),
        pumpkin("served/tasks/bench/task.star"),
    ];
    check(
        &scratch,
        steps,
        false,
        &[],
        &expect![[r#"
            $ pumpkin check tasks/loads/task.star --arg=model=m
            exit status: Some(1)
            tasks/loads/task.star:1:6: loading `@pumpkin//harness/codex.star`: `@pumpkin//harness/codex.star` names no regular file of the standard library: name one that it holds
                1 | load("@pumpkin//harness/codex.star", "codex_agent")
                  |      ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
            pumpkin: tasks/loads/task.star: the task has 1 problem
            pumpkin: to check it again: pumpkin check --arg model=m <scratch>/tasks/loads/task.star
            $ pumpkin check images/tasks/bench/task.star --arg=model=m
            exit status: Some(1)
            lib/bench.star:52:19: `@pumpkin//harness/one` names no directory of the standard library: name one that it holds
                52 |     agent_image = image(
                   |                   ^^^^^^
                in `bench_task()`, called at tasks/bench/task.star:5:1
            lib/bench.star:52:19: `@pumpkin//harness/two` names no directory of the standard library: name one that it holds
                52 |     agent_image = image(
                   |                   ^^^^^^
                in `bench_task()`, called at tasks/bench/task.star:5:1
            pumpkin: images/tasks/bench/task.star: the task has 2 problems
            pumpkin: to check it again: pumpkin check --arg model=m <scratch>/images/tasks/bench/task.star
            $ pumpkin check served/tasks/bench/task.star --arg=model=m
            exit status: Some(0)"#]],
    );
}

#[test]
fn library_lints() {
    // The standard library's files as a task's, below `//`, so that `check` lints
    // them, as it doesn't the library's own.
    let scratch = Scratch::new();
    write_repository(&scratch, "");
    copy(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../library"),
        "",
        &scratch,
    );
    scratch.write("lib/bench.star", &LIBRARY.replace("\"@pumpkin//", "\"//"));
    let pumpkin = scratch.pumpkin(&["check", "tasks/bench/task.star", "--arg=model=m"]);
    check(
        &scratch,
        [Step::from(pumpkin)],
        false,
        &[],
        &expect![[r#"
            $ pumpkin check tasks/bench/task.star --arg=model=m
            exit status: Some(0)"#]],
    );
}

/// The `--arg`s that [`examples`] checks an example with, by its entry file below `examples/`.
/// An entry file not listed here needs no argument.
const EXAMPLE_ARGS: &[(&str, &[&str])] = &[
    ("codex/task.star", &["--arg", "model=model-a"]),
    ("hypergraph/task.star", &["--arg", "model=model-a"]),
    ("team/codex.star", &["--arg", "model=model-a"]),
    // So that the GPU runner, which the smoke adds only for a GPU's model, is checked too.
    ("smoke/task.star", &["--arg", "gpu_model=gpu-a"]),
];

fn write_benchmark_inputs(scratch: &Scratch) {
    // Preparation supplies the upstream recipes, source trees and replay selections.
    for context in [
        "agent-base",
        "verifier-base",
        "native-agent",
        "native-verifier",
    ] {
        scratch.write(
            &format!("examples/benchmark-input/{context}/Dockerfile"),
            "FROM scratch\n",
        );
    }
    for mount in [
        "benchmark-input/generation",
        "benchmark-input/agent",
        "benchmark-input/judge",
        "benchmark-replay",
    ] {
        fs::create_dir_all(scratch.0.join("examples").join(mount)).unwrap();
    }
}

#[test]
fn examples() {
    let scratch = Scratch::new();
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    let mut names = Vec::new();
    let root = scratch.0.join("examples");
    fs::create_dir(&root).unwrap();
    for entry in fs::read_dir(examples).unwrap() {
        let entry = entry.unwrap();
        let dest = root.join(entry.file_name());
        if entry.path().join("task.star").is_file() {
            copy_tree(&entry.path(), &dest);
            names.push(format!(
                "{}/task.star",
                entry.file_name().into_string().unwrap()
            ));
        } else if entry.path().is_file() {
            fs::copy(entry.path(), dest).unwrap();
        }
    }
    write_benchmark_inputs(&scratch);
    names.extend([
        "team/codex.star".into(),
        "hypergraph-benchmark/replay/task.star".into(),
    ]);
    names.sort();
    let steps = names.iter().map(|name| {
        let args = EXAMPLE_ARGS.iter().find(|(example, _)| example == name);
        let mut command = scratch.pumpkin(&["check", name]);
        command.args(args.map_or(&[][..], |(_, args)| args));
        command.current_dir(&root);
        Step::from(command)
    });
    check(
        &scratch,
        steps,
        false,
        &[],
        &expect![[r#"
            $ pumpkin check codex/task.star --arg model=model-a
            exit status: Some(0)
            $ pumpkin check hello/task.star
            exit status: Some(0)
            $ pumpkin check hypergraph-benchmark/replay/task.star
            exit status: Some(0)
            $ pumpkin check hypergraph-benchmark/task.star
            exit status: Some(0)
            $ pumpkin check hypergraph/task.star --arg model=model-a
            exit status: Some(0)
            $ pumpkin check smoke/task.star --arg gpu_model=gpu-a
            exit status: Some(0)
            $ pumpkin check team/codex.star --arg model=model-a
            exit status: Some(0)
            $ pumpkin check team/task.star
            exit status: Some(0)"#]],
    );
}

/// Writes each file below `dir` into `scratch`, at its path below `dir` after `below`, with each
/// `@pumpkin//` label in quotes a `//` one.
fn copy(dir: &Path, below: &str, scratch: &Scratch) {
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = format!("{below}{}", entry.file_name().to_str().unwrap());
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &format!("{path}/"), scratch);
            continue;
        }
        let text = fs::read_to_string(entry.path()).unwrap();
        scratch.write(&path, &text.replace("\"@pumpkin//", "\"//"));
    }
}

/// `lib/verify.star`, which defines a job function's body that a check accepts.
const VERIFY: &str = r#""""The body of the agent's check calls."""

def verify(target: Target, payload: Payload, *, metric: str = "score"):
    """Puts the payload in the target, runs the judge on it, and returns its metric.

    Args:
      target: the call's target.
      payload: the agent's submission.
      metric: the metric of the judge's result that the call returns.

    Returns:
      The metric, by its name.
    """
    target.put("/pumpkin/in/payload", payload)
    target.run(["/opt/judge/run"])
    return {metric: json.decode(target.get("/pumpkin/out/result.json"))[metric]}
"#;

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the library's and the snapshot's text"
)]
fn bodies() {
    // The `def` line of `LIBRARY`'s now function's body.
    const SUBMIT: &str = "def submit(run: Run, payload: Payload, finalize: bool = False):";
    // The `def` line of `LIBRARY`'s final call's body.
    const FINAL: &str = "def final_call(target: Target, submissions):";
    let scratch = Scratch::new();
    // A task below `dir` whose library gives the agent a job function, `check`, whose body is
    // `body`, a value of `verify`, the text of `lib/verify.star`, on a target of the judges' image,
    // and a now function, `final`, whose body's `def` line is `submit`, and a final call whose
    // body's `def` line is `final_call`.
    let write = |dir: &str, verify: &str, body: &str, (submit, final_call): (&str, &str)| {
        write_repository(&scratch, dir);
        let target = r#"{
            "name": "checker",
            "image": image(judge_recipe, mount = SHARED),
            "user": {"uid": 65532, "gid": 65532},
            "needs": judge,
            "runtime": {"target": {"cwd": "/pumpkin/home"}},
        }"#;
        let function = format!(
            r#"{{"name": "check", "job": {{
                        "target": "checker",
                        "body": {body},
                        "payload_max_bytes": submission_bytes,
                        "call_timeout_seconds": TRAIN_TIMEOUT,
                    }}}},
                    {{"name": "final", "now": {{"body": submit, "payload_max_bytes": MiB}}}},"#
        );
        let library = LIBRARY
            .replace(
                "load(\"@pumpkin//harness/codex/codex.star\", \"codex\")\n",
                "load(\"@pumpkin//harness/codex/codex.star\", \"codex\")\n\
                 load(\"//lib/verify.star\", \"verify\")\n",
            )
            .replace(
                r#"{"name": "final", "now": {"body": submit, "payload_max_bytes": MiB}},"#,
                &function,
            )
            .replace("] + judges,", &format!("] + judges + [{target}],"))
            .replace(SUBMIT, submit)
            .replace(FINAL, final_call);
        scratch.write(&format!("{dir}lib/bench.star"), &library);
        scratch.write(&format!("{dir}lib/verify.star"), verify);
    };
    write("", VERIFY, "verify", (SUBMIT, FINAL));
    write(
        "one/",
        "def verify(target):\n    pass\n",
        "verify",
        (SUBMIT, FINAL),
    );
    write("int/", VERIFY, "3", (SUBMIT, FINAL));
    let submit = "def submit(run: Run, payload: Payload, finalize: float = 0.0):";
    write("now/", VERIFY, "verify", (submit, FINAL));
    let final_call = "def final_call(target: Run, submissions):";
    write("final/", VERIFY, "verify", (SUBMIT, final_call));
    let final_call = "def final_call(target: Target, submissions, level: int = 1):";
    write("after/", VERIFY, "verify", (SUBMIT, final_call));
    let final_call = "def final_call(target: Target, submissions, *, mode: str, extra = 0):";
    write("keywords/", VERIFY, "verify", (SUBMIT, final_call));
    let bench = scratch.0.join("keywords/lib/bench.star");
    let library = fs::read_to_string(&bench).unwrap().replace(
        r#""body": final_call,"#,
        r#""body": final_call, "keywords": {"mode": 3, "extra": 1, "level": 2},"#,
    );
    fs::write(bench, library).unwrap();
    let pumpkin = |dir: &str| {
        let entry = format!("{dir}tasks/bench/task.star");
        Step::from(scratch.pumpkin(&["check", &entry, "--arg=model=m"]))
    };
    check(
        &scratch,
        [
            pumpkin(""),
            pumpkin("one/"),
            pumpkin("int/"),
            pumpkin("now/"),
            pumpkin("final/"),
            pumpkin("after/"),
            pumpkin("keywords/"),
        ],
        false,
        &[],
        &expect![[r#"
            $ pumpkin check tasks/bench/task.star --arg=model=m
            exit status: Some(0)
            $ pumpkin check one/tasks/bench/task.star --arg=model=m
            exit status: Some(1)
            lib/verify.star:1:5: `verify` takes 1 positional parameter, and a job's body takes its `Target` and the payload first: write `def verify(target: Target, payload: Payload, …)`
                1 | def verify(target):
                  |     ^^^^^^
            pumpkin: one/tasks/bench/task.star: the task has 1 problem
            pumpkin: to check it again: pumpkin check --arg model=m <scratch>/one/tasks/bench/task.star
            $ pumpkin check int/tasks/bench/task.star --arg=model=m
            exit status: Some(1)
            lib/bench.star:89:33: `body` can't be an int: expected a dict
                89 |                         "body": 3,
                   |                                 ^
                the IR's /roles/0/runtime/agent/grants/1/job/body
            pumpkin: int/tasks/bench/task.star: the task has 1 problem
            pumpkin: to check it again: pumpkin check --arg model=m <scratch>/int/tasks/bench/task.star
            $ pumpkin check now/tasks/bench/task.star --arg=model=m
            exit status: Some(1)
            lib/bench.star:145:40: `submit`'s `finalize` follows the payload, so its caller gives it: annotate it `str`, `int` or `bool`
                145 | def submit(run: Run, payload: Payload, finalize: float = 0.0):
                    |                                        ^^^^^^^^
            pumpkin: now/tasks/bench/task.star: the task has 1 problem
            pumpkin: to check it again: pumpkin check --arg model=m <scratch>/now/tasks/bench/task.star
            $ pumpkin check final/tasks/bench/task.star --arg=model=m
            exit status: Some(1)
            lib/bench.star:151:16: `final_call`'s first parameter is a `Run`, and the final call's body takes its `Target`: write `def final_call(target: Target, …)`
                151 | def final_call(target: Run, submissions):
                    |                ^^^^^^
            pumpkin: final/tasks/bench/task.star: the task has 1 problem
            pumpkin: to check it again: pumpkin check --arg model=m <scratch>/final/tasks/bench/task.star
            $ pumpkin check after/tasks/bench/task.star --arg=model=m
            exit status: Some(1)
            lib/bench.star:151:45: `final_call`'s `level` follows the submissions, and the final call has no caller to give it: remove it
                151 | def final_call(target: Target, submissions, level: int = 1):
                    |                                             ^^^^^
            pumpkin: after/tasks/bench/task.star: the task has 1 problem
            pumpkin: to check it again: pumpkin check --arg model=m <scratch>/after/tasks/bench/task.star
            $ pumpkin check keywords/tasks/bench/task.star --arg=model=m
            exit status: Some(1)
            lib/bench.star:151:5: the final call's `keywords` gives `level`, which is no keyword-only parameter of `final_call`: remove it
                151 | def final_call(target: Target, submissions, *, mode: str, extra = 0):
                    |     ^^^^^^^^^^
            lib/bench.star:151:48: the final call's keyword `mode` can't be `3`: `final_call` takes a string
                151 | def final_call(target: Target, submissions, *, mode: str, extra = 0):
                    |                                                ^^^^
            pumpkin: keywords/tasks/bench/task.star: the task has 2 problems
            pumpkin: to check it again: pumpkin check --arg model=m <scratch>/keywords/tasks/bench/task.star"#]],
    );
}
