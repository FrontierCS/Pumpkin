# Writing workflows

A workflow is data: the workflow IR. A task's entry file makes it with one `workflow()` call.
The IR's types in the `schema` crate are the reference for every key. They live in
[`ir.rs`](../crates/schema/src/ir.rs) and [`ir/`](../crates/schema/src/ir/), and say what each
key means, what bounds it, and what an absent one means. This page says how the parts fit.
[`examples/hello`](../examples/hello/task.star) shows them in a workflow that needs no model.
[The language](language.md) says how the files are written.

## Roles

A role is a template of sandboxes: an image, a user that isn't root, and what each sandbox needs.
The run places the role by those needs ([providers](providers.md#placing-a-run)). The role's
`runtime` gives its kind:

- An **agent** works on the task. Its command runs once. It runs until it exits, until its work
  ends and its grace runs out, or until the work deadline passes. The workflow's
  `deadline_seconds` sets that deadline. The agents share the deadline, and each ends on its own.
  An agent holds `grants`, the functions that it may call. It also holds `egress`, its model
  route ([models](models.md)).
- A **target** runs the commands that a call's body sends. With the `call` lifetime, which is the
  default, each call gets a fresh sandbox that ends with the call. With the `work` lifetime, one
  sandbox serves every call of the work phase. It serves one whole body at a time. A work
  target's commands make no ops and no model requests. A process there can outlive its call, so
  nothing could tell its requests apart by job.
- A **runner** is a sandbox that a session leases. The lease's holder drives it. A runner holds no
  egress and no grants.

Every sandbox has `/pumpkin`, which holds Pumpkin's own files. The `schema` crate's
[`sandbox`](../crates/schema/src/sandbox.rs) module lays them out:

- `in/` holds the agent's standard input.
- `out/` holds what the record collects.
- `home/` is the commands' `HOME`.
- `log/` holds the commands' output.

## Grants

A grant is a name and one function, under its kind's key: `{"name": "count", "job": {…}}`. Its
holder calls it by name. The run checks each call against the grant before anything runs.

- A **job function** runs its body on a sandbox of its `target`, one call at a time. The call
  answers at once with the job's number. `sandboxctl status` follows the job to what the body
  returned. The agent never reaches the target. It learns only the job's state, what the grant has
  left, and what the body returned.
- A **now function** answers in the call's reply, with no sandbox. Its body proposes changes to
  the run's state. `run.set_submission(payload)` replaces the caller's submission.
  `run.end_work()` locks the submission and ends the caller's work. The caller may keep running
  for its `finalize_grace_seconds`, with its ops refused. The run applies the changes once the body returns, all of them
  or none.
- A **session** leases runners. Its holder opens a lease, runs commands and copies trees in it one
  at a time, then closes the lease. A target's body holds the target's sessions, and each call gets
  them afresh. A call target's commands hold them too.

The parts name each other by role. An agent may name a call target in at most one of its job
functions. Other agents may name the same target too. The final call's target is a call target
that no job function names. A target's session names a runner that no other session names. An
agent's runner has an image that no target has, so the agent never runs what a target's image
holds. Another part names each target and each runner.

## The final call

The final call waits until every agent's work has ended, and every call, lease and work sandbox is
gone. Then it runs its body on a fresh sandbox of its `target`. The body gets each agent role's
last submission, as a `Payload` or `None`. What the body returns is the run's result. The run
shows the result's entries in order, as `score=6 lines=1`. A failed final call leaves the run
without a result.

## Bodies

A body is a top-level `def` of a file that the entry file loads. The record keeps the text of each
file that the evaluation read. Each call evaluates its body from that text. So a call runs the
body that was checked as the run started. A body's parameters, in order:

1. The handle: a `Target` for a job's body or the final call's body, and a `Run` for a now
   function's body.
2. The caller's bytes, as a `Payload`. In the final call's body, this parameter holds the
   submissions by role and has no annotation.
3. The caller's parameters, each annotated `str`, `int` or `bool`. The caller gives each as
   `--arg <name>=<value>`. Each is read as its type before the body runs. The final call has none.
4. After `*`, the keyword-only parameters. The `keywords` of the grant or of the final call give
   them.

| Method | What it does |
| --- | --- |
| `target.put(path, payload)` | writes the payload as a file at `path`, its parents made |
| `target.run(argv)` | runs `argv` in the target's `cwd`, and waits for it, whatever its exit status |
| `target.capture(argv, stdin = payload)` | runs `argv`, and returns its standard output if it exits 0 |
| `target.get(path)` | returns the text of the file at `path` |
| `target.caller()` | returns the calling agent's role, in a job's body |
| `target.with_runner(session, callback, ...)` | opens a lease of the target's session, calls `callback(runner, ...)`, closes the lease even if the callback fails, and returns what the callback returned once the runner is deleted |
| `runner.put(tree, to)` | copies the target's tree to a new path in the runner |
| `runner.get(from, to)` | copies the runner's file or tree into a new directory on the target, where a file keeps its name |
| `runner.run(argv, timeout_seconds = seconds)` | runs `argv` in the runner, and returns a [`RunnerOutput`](../crates/schema/src/sandbox/op.rs) |
| `run.set_submission(payload)`, `run.end_work()` | propose a now function's changes, as above |

`get` and `capture` return at most 1 MiB of UTF-8 text. A runner handle works only in its
callback. Any of these fails the body: a refused op, a failed copy, a command that doesn't start
or whose output isn't collected whole, or a lease that runs out. A runner that can't be deleted
fails the run.

A body returns a dict of at most 1 MiB of JSON. Its keys are names, and JSON can hold its values.
The body fails if its evaluation fails, as on `fail()` or a `get` of a missing file. It also fails
if it returns anything else. The record keeps why it failed. The caller hears only that it failed.

## From inside a sandbox

An agent and a call target's commands make ops with `sandboxctl`, which comes first on `PATH`.
`sandboxctl --help` gives each command's arguments and exit statuses. The `schema` crate's
[`sandbox::op`](../crates/schema/src/sandbox/op.rs) module gives each reply's fields.
`sandboxctl ops` lists the role's grants with their `help`, limits and parameters.
`sandboxctl ops --text` prints their usage for a shell.

![One job function's call, end to end: in the agent's sandbox, sandboxctl call sends the payload over the session; the driver's core checks it against the grant and answers with the job's number; the runtime starts a fresh sandbox of the target and evaluates the body, from the sources the record keeps, whose put, run and get drive the target; the target ends, and the dict the body returned goes to the agent's sandboxctl status and to the record.](figures/call.svg)

## What `pumpkin check` refuses

`pumpkin check` checks how the parts fit. It reads each body against its kind without calling
it. A body's own statements run only when it is called, so a misspelt method fails the call that
reaches it, not the check.
