# Writing workflows

A workflow is data, the workflow IR, which a task's entry file makes with one `workflow()` call.
The IR's types in the `schema` crate, [`ir.rs`](../crates/schema/src/ir.rs) and
[`ir/`](../crates/schema/src/ir/), are the reference for every key: what it means, what bounds it,
and what an absent one means. This page says how the parts fit, as
[`examples/hello`](../examples/hello/task.star) shows them without a model.
[The language](language.md) says how the files are written.

## Roles

A role is a template of sandboxes: an image, a user that isn't root, and what each sandbox needs,
by which the run places it ([providers](providers.md#placing-a-run)). Its `runtime` gives its
kind:

- An **agent** works on the task. Its command runs once, until it exits, its work ends and its
  grace runs out, or the work deadline, `deadline_seconds`, passes. The agents share that
  deadline, and each ends on its own. An agent holds `grants`, the functions it may call, and
  `egress`, its model route ([models](models.md)).
- A **target** runs the commands that a call's body sends. With the `call` lifetime, the default,
  each call gets a fresh sandbox, which ends with it. With `work`, one sandbox serves every call
  of the work phase, one whole body at a time. A work target's commands make no ops and no model
  requests: a process there can outlive its call, so nothing could tell its requests apart by
  job.
- A **runner** is a sandbox that a session leases, and that the lease's holder drives. It holds
  neither egress nor grants.

Every sandbox has `/pumpkin`, which holds Pumpkin's own files, as the `schema` crate's
[`sandbox`](../crates/schema/src/sandbox.rs) module lays them out: `in/`, the agent's standard
input, `out/`, which the record collects, `home/`, the commands' `HOME`, and `log/`, their output.

## Grants

A grant is a name and one function, under its kind's key: `{"name": "count", "job": {…}}`. Its
holder calls it by name, and the run checks each call against the grant before anything runs.

- A **job function** runs its body on a sandbox of its `target`, one call at a time. The call
  answers at once with the job's number, which `sandboxctl status` follows to what the body
  returned. The agent never reaches the target: it learns only the job's state, what the grant has
  left, and what the body returned.
- A **now function** answers in the call's reply, with no sandbox. Its body proposes changes to
  the run's state: `run.set_submission(payload)` replaces the caller's submission, and
  `run.end_work()` locks it and ends the caller's work once its `finalize_grace_seconds` pass. The
  run applies them once the body returns, all of them or none.
- A **session** leases runners. Its holder opens a lease, runs commands and copies trees in it one
  at a time, and closes it. A target's sessions are its body's, afresh for each call, and a call
  target's commands' too.

The parts name each other by role. An agent names a call target from one of its job functions
at most, though other agents may name it too. The final call's target is a call target that no
job names. A target's session names a runner that no other session names. An agent's runner has
an image that no target has, so the agent never runs what a target's image holds. Each target
and runner is named.

## The final call

Once every agent's work has ended, and every call, lease and work sandbox is gone, the final call
runs its body on a fresh sandbox of its `target`. The body gets each agent role's last submission,
a `Payload`, or `None`. What it returns is the run's result, whose entries the run shows in order,
as `score=6 lines=1`. A failed final call leaves the run without a result.

## Bodies

A body is a top-level `def` of a file that the entry file loads. The record keeps the text of each
file that the evaluation read, and each call evaluates its body from that text, so a call runs the
body that was checked as the run started. Its parameters, in order:

1. The handle: a `Target` for a job's body and the final call's, a `Run` for a now function's.
2. The caller's bytes, a `Payload`, or, for the final call, the submissions by role, unannotated.
3. The caller's parameters, each annotated `str`, `int` or `bool`, which the caller gives as
   `--arg <name>=<value>`, read as their type before the body runs. The final call has none.
4. After `*`, the keyword-only parameters, which the grant's or the final call's `keywords` give.

| Method | What it does |
| --- | --- |
| `target.put(path, payload)` | writes the payload as a file at `path`, its parents made |
| `target.run(argv)` | runs `argv` in the target's `cwd`, and waits for it, whatever its exit status |
| `target.capture(argv, stdin = payload)` | runs `argv`, and returns its standard output if it exits 0 |
| `target.get(path)` | returns the text of the file at `path` |
| `target.caller()` | returns the calling agent's role, in a job's body |
| `target.with_runner(session, callback, ...)` | opens a lease of the target's session, calls `callback(runner, ...)`, closes the lease even if that fails, and returns what it returned once the runner is deleted |
| `runner.put(tree, to)` | copies the target's tree to a new path in the runner |
| `runner.get(from, to)` | copies the runner's file or tree into a new directory on the target, a file under its own name |
| `runner.run(argv, timeout_seconds = seconds)` | runs `argv` in the runner, and returns a [`RunnerOutput`](../crates/schema/src/sandbox/op.rs) |
| `run.set_submission(payload)`, `run.end_work()` | as a now function's body proposes them |

`get` and `capture` return at most 1 MiB of UTF-8 text. A runner handle works only in its
callback. A refused op, a failed copy, a command that doesn't start or whose output isn't
collected whole, or a lease that runs out fails the body. A runner that can't be deleted fails the
run.

A body returns a dict whose keys are names and whose values JSON can hold, at most 1 MiB of JSON.
It fails if its evaluation fails, as on `fail()` or a `get` of a missing file, or if it returns
anything else. The record keeps why, and the caller hears only that the body failed.

## From inside a sandbox

An agent, or a call target's commands, make ops with `sandboxctl`, which comes first on `PATH`.
`sandboxctl --help` gives each command's arguments and exit statuses, and the `schema` crate's
[`sandbox::op`](../crates/schema/src/sandbox/op.rs) module each reply's fields. `sandboxctl ops`
lists the role's grants with their `help`, limits and parameters, and `sandboxctl ops --text`
prints their usage for a shell.

![One job function's call, end to end: in the agent's sandbox, sandboxctl call sends the payload over the session; the driver's core checks it against the grant and answers with the job's number; the runtime starts a fresh sandbox of the target and evaluates the body, from the sources the record keeps, whose put, run and get drive the target; the target ends, and the dict the body returned goes to the agent's sandboxctl status and to the record.](figures/call.svg)

## What `pumpkin check` refuses

`pumpkin check` checks how the parts fit, and reads each body against its kind without calling
it. A body's own statements run only when it is called, so a misspelt method fails the call that
reaches it, not the check.
