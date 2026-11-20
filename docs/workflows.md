# Writing workflows

This page says what each key of a workflow means, what bounds it, and how a wrong value is refused;
what a role's code sees when it calls a function; and what the record keeps of a call. The
README's [workflow](../README.md#a-workflow) introduces the parts with `examples/hello`, which this
page quotes; [the language](language.md) says how the files themselves are written.

The IR's types in the `schema` crate's `ir` module are the reference for every key: a key that the
IR doesn't take is refused, and an absent key means what its doc there says, never a default
chosen elsewhere.

## The entry file

A task is an entry file below a root, the nearest directory at or above it that holds a
`PUMPKIN.star`. The entry file makes one `workflow()` call, with these keyword arguments:

| Key | What it is |
| --- | --- |
| `name` | what `pumpkin ls` and the page show: 1 to 128 characters from A-Z, a-z, 0-9, `.`, `_`, `-` and `/` |
| `roles` | the run's roles in declaration order, including at least one agent |
| `deadline_seconds` | how long all agents may work, from their start or, at the latest, when the longest agent startup bound has passed since the run started |
| `final` | the final call, which gives the run its result |

`workflow()` is called once, while the entry file's own statements run; the files it loads give it
values and bodies. A missing key is refused at the call:

```text
text.star:1:1: `workflow` lacks `roles`, `final`: give them
    1 | workflow(name = "")
      | ^^^^^^^^^^^^^^^^^^^
```

## Roles

Every role has these keys:

| Key | What it is |
| --- | --- |
| `name` | unique among the roles: 1 to 64 characters from a-z, 0-9, `_` and `-`, starting with a letter |
| `image` | what `image()` returns, which `pumpkin run` builds |
| `user` | `{"uid": …, "gid": …}`, the user its commands run as, neither of them 0 |
| `needs` | what each of its sandboxes needs, from which the run picks its class of sandbox |
| `runtime` | its kind and what the kind holds: `{"agent": {…}}`, `{"target": {…}}` or `{"runner": {…}}` |

`needs` holds `cpu_millis`, thousandths of a CPU, and `memory_bytes`, both required; `gpu`,
`{"count": …, "model": …}` with the model as `nvidia-smi` names it; `cpu_features`, flags as
`/proc/cpuinfo` names them; and `cpu_model`. The run places each role in the smallest class of
sandbox, of every provider's, that offers them, and refuses one that fits none, saying why of each
class, before anything starts ([providers](providers.md)).

A value that the IR's type refuses is said at the place in the source that made it, with its place
in the IR:

```text
bad/zero/task.star:3:14: `cpu_millis` can't be `0`: expected a positive int
    3 | CPU_MILLIS = 0
      |              ^
    the IR's /roles/1/needs/cpu_millis
```

### Agents

Each agent works in its own sandbox and runs its command once. Agents may differ in resources,
command, model route and grants. They work under one deadline.

| Key | What it is |
| --- | --- |
| `command` | `{"argv": [...], "cwd": …, "env": [...]}`: the whole command line, as the image's entrypoint is not used; the directory it starts in; and `NAME=value` entries added to the image's environment, which may be left out |
| `deadline_seconds` | a single agent may give the work deadline here when the workflow omits it |
| `finalize_grace_seconds` | how long the agent may keep running once a call has ended its work, its ops refused meanwhile; 0 ends the work in that call's step |
| `dirs`, `files` | directories (`{"path", "mode"}`) and text files (`{"path", "mode", "content"}`) made before the command starts, as the role's user, with modes written as `"0644"` |
| `stdin` | the text the command reads on its standard input; absent, it reads nothing |
| `egress` | its routes out: `{"model": [...]}`, a model route ([models](models.md)) |
| `grants` | the functions it may call |

Each agent ends when its command exits, when its grace after an `end_work()` runs out, or at
the shared deadline, whichever comes first. Its peers can keep working. Like every sandbox, the agent's has `/pumpkin`, which holds
Pumpkin's own files: `in/`, which holds its `stdin`, `out/`, which is collected into the record
when the agent ends, `home/`, its commands' `HOME`, and `log/`, their output. A path of `dirs` or
`files` below `/pumpkin` or `/.pumpkin` is refused.

### Targets

A role that a function's body drives: each call of the job function that names it, and the final
call if it names it, starts a fresh sandbox of it, which runs no command of its own and ends with
the call. Each agent may name a given target from one job function. A target used by the final
call is exclusive to it. Every target must be reached.

| Key | What it is |
| --- | --- |
| `cwd` | the directory each command of the body's starts in |
| `env` | `NAME=value` entries added to each command's environment |
| `egress` | its model route, open while a call's body runs |
| `sessions` | the sessions it may call while a call's body runs, written as a grant of one is; each call uses them afresh |

### Runners

A sandbox that a session leases for its holder, which sends it commands and copies. Distinct
agents may lease the same runner role. Every lease starts its own sandbox. A target's session
names a runner no other session names. It holds `cwd` and `env`, as a target does, and neither egress nor grants. A
runner of an agent's session has an image that no target has, so that the agent never runs what a
target's image holds.

## Grants

A grant is `{"name": …, <kind>: {…}}`: a name, unique among the role's grants, which the role calls
it by, and one function under its kind's key, `job`, `now` or `session`. A second key after the kind
is refused, and so is a grant with none.

### A job function

| Key | What it is |
| --- | --- |
| `target` | the target that each call starts afresh |
| `body` | a `def` of a loaded file, which drives the target ([bodies](#bodies)) |
| `keywords` | the values of the body's keyword-only parameters, as JSON; may be left out |
| `payload_max_bytes` | the most bytes a call's payload may hold, at most 64 MiB |
| `calls` | the most calls the agent may make; left out, no bound |
| `cumulative_seconds` | the most time its calls may take in all, each counting once it has ended; left out, no bound |
| `call_timeout_seconds` | each call's timeout, counted from its target's start |

An agent's job grant runs one call at a time. Each agent has its own call and time limits. A call ends with its timeout, with the function's
cumulative time, or with the work phase, whichever comes first.

### A now function

| Key | What it is |
| --- | --- |
| `body` | a `def` of a loaded file, which answers on the run's state ([bodies](#bodies)) |
| `payload_max_bytes` | the most bytes a call's payload may hold, at most 64 MiB |

Its call is answered at once, with what the body returned, after the run has applied what the body
asked of it.

### A session

| Key | What it is |
| --- | --- |
| `runner` | the runner that each lease starts afresh |
| `instances` | the most leases open at once |
| `instance_seconds` | the most time each lease may last, from when its runner is ready |
| `cumulative_seconds` | the most time its leases may last in all, each open lease counting; left out, no bound |
| `commands` | the most commands its leases may run in all, each copy counting as one; left out, no bound |
| `transfer_bytes` | the most bytes of files each copy may move, at most 1 GiB |

A lease runs one command or copy at a time, until its holder closes it, its time runs out, or its
holder's work ends: the work phase, for the agent; its call, for a target. A target's session
bounds each call afresh.

## The final call

`final` holds `target`, which no job function names, `body`, `keywords`, as a job function's, which
may be left out, `payload_max_bytes`, at most 64 MiB, and `timeout_seconds`, counted from its
target's start. The body drives a fresh sandbox once all agents, calls and leases have ended.
The dict it returns is the run's result. Each entry is a value the run shows in the body's order,
as `pumpkin wait`, `pumpkin ls` and the page show `score=6 lines=1`.

A single agent may omit `final.input`. Its last selected payload then goes directly to the body.
If it selects nothing the run skips the final call and `pumpkin wait` exits 3.

Set `"input": "selections"` for a team. A single agent may use it too. The body receives a UTF-8
JSON array in agent declaration order. Each entry names the role and its selection. For example:

```json
[{"role":"alice","selection":{"path":"/pumpkin/in/selections/alice","bytes":2,"digest":"sha256:8f434346648f6b96df89dda901c5176b10a6d83961dd3c1ac88b59b2dc327aa4"}},{"role":"bob","selection":null}]
```

A present selection is staged at its path with its original bytes. The digest is its SHA-256
and `payload_max_bytes` bounds each selection. A missing selection has no file. The final body
runs even when all selections are missing and supplies the workflow's scoring rule. The selected files and the judge's other
`/pumpkin` files share that filesystem's 1 GiB capacity and the target's memory limit.
Size the total inputs accordingly.

## Bodies

A body is a top-level `def` of a file that the entry loads; an entry file defines no function. The
IR names a body by its file's label and its name, and each call evaluates that file afresh, from
the text the record keeps. Its parameters, in order:

1. The handle, which only the host makes: a `Target` for a job function's body and the final
   call's, a `Run` for a now function's.
2. The payload, a `Payload`: the bytes the caller sent, which the body can only pass on, to
   `put` or `select`.
3. The caller's parameters, if any: each annotated `str`, `int` or `bool`, with or without a
   default. The final call's body has none, as no caller gives it an argument.
4. After `*`, the keyword-only parameters, which a job function's `keywords` or the final call's
   give, each a JSON value that must fit its annotation. One that they don't give needs a default.

The two handles' methods:

| Method | What it does |
| --- | --- |
| `target.put(path, payload)` | writes the payload as one file at `path`, an absolute path, its parents made |
| `target.run(argv)` | runs `argv`, a list of strings, in the target's `cwd`, and waits for it to end, whatever its exit status |
| `target.get(path)` | the text of the file at `path`, which must be UTF-8 and at most 1 MiB |
| `run.select(payload)` | makes the payload the selection, the one the final call gets, replacing the last |
| `run.end_work()` | ends the caller's work, its selection locked, once its grace has run out |

A now function's body only proposes: the run applies its `select` and `end_work`, in order, once
the body has returned, each checked against the state the ones before it leave, and applies none
if one is refused. `end_work()` needs a selection, and must come last.

A body returns a dict whose keys are names, as a role's are, and whose values JSON can hold; it is
encoded as `json.encode` would, and takes at most 1 MiB as JSON. Within a body, annotations are checked on entry and on return of
every function it calls, and its evaluation is bounded as every evaluation is
([the language](language.md#bounds)).

A body fails if its evaluation fails, as on `fail()` or a method that fails, such as a `get` of a
file that is not there, or if it returns anything but such a dict, or one over that size. The
record keeps the reason; the caller hears only `the call's body failed: the run's record says why`.

### The caller's arguments

A parameter after the payload is given by the caller as `--arg <name>=<value>`, and read as its
type before the body runs: a `str` as it is, an `int` in decimal digits, a `bool` as `true` or
`false`. One with no default must be given. A call whose arguments don't read is refused, with the
body unrun, as `examples/hello`'s `final` answers:

```json
{"kind":"refused","message":"`--arg finalize` is `\"yes\"`, which is no bool: give it `true` or `false`"}
{"kind":"refused","message":"`--arg mode` is given, and `final` takes no such argument: give only those that `ops` lists"}
```

## From inside a sandbox

A role's code makes ops with `sandboxctl`, at `/.pumpkin/bin/sandboxctl` in the agent's sandbox,
and in a target's that holds sessions. It prints the reply as one line of JSON, whose `kind` names it, and
exits 0, 1 for a refused op, or 2 for the status of a job that gave no result; `sandboxctl --help`
gives each command's arguments, and the `schema` crate's `sandbox::op` module each reply's fields.
The replies below come from `examples/hello` with its agent changed to try each, and its `count`
grant given `"calls": 2` and `"cumulative_seconds": 120`.

`sandboxctl ops` gives the time left to work, the sandbox's resources, and each grant with its
bounds and its caller's parameters:

```json
{"kind":"ops","remaining_seconds":299,"resources":{"cpu_millis":500,"memory_bytes":268435456},"grants":[{"kind":"job","name":"count","payload_max_bytes":1024,"calls":2,"cumulative_seconds":120,"call_timeout_seconds":60},{"kind":"now","name":"final","payload_max_bytes":1024,"arguments":{"finalize":{"type":"bool","default":false}}}]}
```

`sandboxctl call <function> <file> [--arg <name>=<value>]...` calls a function with the file's
bytes as the payload. A job function's call answers at once with the job's number and what the
function has left, `null` for a bound it doesn't set:

```json
{"kind":"call","job":0,"remaining":{"calls":1,"seconds":120}}
```

`sandboxctl status <job>` follows the job: its `state` is `queued`, `running`, `completed`, with
what the body `returned`, or `errored`, with a `failure` whose `code` is `call_timeout` or
`call_error`:

```json
{"kind":"status","job":0,"state":"completed","returned":{"bytes":6,"lines":1},"remaining":{"calls":1,"seconds":119}}
```

Once a job has ended, `model_requests_over_budget` says how many of its target's model requests a
spent model budget barred, if any. `hello`'s target makes none; a job whose target's budget ran out
shows:

```json
{"kind":"status","job":1,"state":"errored","failure":{"code":"call_error","message":"the call's body failed: the run's record says why"},"model_requests_over_budget":3,"remaining":{"calls":0,"seconds":80}}
```

A now function's call answers with what the body returned and the time left to work, which, once
the call has ended the work, is at most the grace:

```json
{"kind":"call","returned":{"state":"stored"},"remaining_seconds":298}
{"kind":"call","returned":{"state":"accepted"},"remaining_seconds":30}
```

A session's functions are called as `<session>.<op>`: `open [--seconds <n>]` answers with the
lease's number; `run <lease> [--timeout <seconds>] -- <program> [<arg>...]`, `put <lease> <tree>
<to> [--replace]` and `get <lease> <from> <to>` start a command or a copy of a directory tree and
answer at once; `status <lease>` says how the lease and its last command or copy are doing, with
the ends of the command's output; and `close <lease>` ends it.

A refusal says why, and what to do instead. Each of these is a real reply:

```json
{"kind":"refused","message":"`count` runs one call at a time, and already has one queued or running: call again once it has finished; this call is not charged"}
{"kind":"refused","message":"you have no job 7: ask about a job that one of your calls returned"}
{"kind":"refused","message":"the work was ended: make no more ops"}
```

## How a call is bounded

Before a call runs, the run checks its payload against the function's `payload_max_bytes` and
refuses an empty one, and checks the function's `calls` and `cumulative_seconds`. A job's body may
drive its target until `call_timeout_seconds` have passed since the target started, or less if the
function's cumulative time or the work phase ends first; then the command the body runs is ended,
or its next method fails, and the job is `errored` with `call_timeout`. Each file a body gets
holds at most 1 MiB, and each evaluation of a body is bounded in steps and bytes, as every
evaluation is. The final call is bounded so by `timeout_seconds`.

![One job function's call, end to end: in the agent's sandbox, sandboxctl call sends the payload over the session; the driver's core checks it against the grant and answers with the job's number; the runtime starts a fresh sandbox of the target and evaluates the body, from the sources the record keeps, whose put, run and get drive the target; the target ends, and the dict the body returned goes to the agent's sandboxctl status and to the record.](figures/call.svg)

## What the record keeps of a call

A call's payload is kept once, in the record's `payloads/`, by the SHA-256 of its bytes. The run
log keeps the op that made the call and the reply, then the call's start, each `put` with the
path and the payload's digest, each `run` with its argv and how it ended, each `get` with the path
and the digest of what it got, and what the body returned, or why it failed. Each call's target,
the final call's included, keeps in a directory of its own, `jobs/<n>/`, the output of its commands
(`log/`), the files its body got (`got/`) and, as the call ends, what the target holds in
`/pumpkin/out` (`out/`); [records](record.md) lays out the rest of the record.

## What `pumpkin check` refuses

`pumpkin check` evaluates the files, reads the workflow into the IR, checks how its parts fit, and
reads every body against its kind, without calling it. It says each problem at its file, line and
column; a problem in how the parts fit is said with its place in the IR. Among them:

- a key the IR doesn't take, or a value its type refuses, as above, a kind that holds what it
  can't among them;
- a function defined in the entry file;
- a workflow with no agent, conflicting deadlines, or several agents and a scalar final;
- a name that names no role, or a role of another kind than its place takes;
- a target that two grants of one agent name or that a job and the final call both name;
- a runner that two sessions name unless their holders are distinct agents;
- a target or a runner that nothing names;
- an agent's runner whose image a target has;
- a payload or transfer bound past the protocol's.

Each in the words `check` says it:

```text
`egress` is no field of `runner`: expected one of `cwd`, `env`
a function defined in an entry file is not in Pumpkin's Starlark: define it in a library, and load it
the final call has several agents' selections: give it `input = "selections"`
`agent` is an agent: name a target as the final call's target
`final_judge` is the final call's target and `check`'s: give the final call a role no job names
`gpu` is a runner that no session names: name it from one
`gpu` runs target `trainer`'s image, sha256:…: give the runner an image no target has
the final call's `payload_max_bytes` is 68157440, over the op payload limit of 67108864: give at most that
```

It also reads each body against its kind: a body that is no `def` of its file, or whose
parameters don't fit its kind, is refused at the parameter:

```text
lib/submit.star:1:12: `submit`'s first parameter is a `Target`, and a now function's body takes its `Run`: write `def submit(target: Run, …)`
lib/submit.star:1:47: `submit`'s `finalize` follows the payload, so its caller gives it: annotate it `str`, `int` or `bool`
lib/final.star:1:38: `final_call`'s `level` follows the payload, and the final call has no caller to give it: remove it
lib/final.star:1:65: `final_call` takes the keyword `metric`, which the final call's `keywords` does not give: give it, or a default
```

A body's own statements run only when it is called, so a misspelt method fails the call that
reaches it, not the check:

```text
lib/verify.star:2:19: `pt` is no method of a `Target`: call one of `get`, `put`, `run`
```
