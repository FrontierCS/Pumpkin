# Architecture

Conventions are in [`AGENTS.md`](AGENTS.md). Names in backticks are for symbol search.

## Bird's-eye view

- A workflow is data, written as JSON in its IR: one agent, its judges, and a pattern that gives
  the agent grants and judges what it submits.
- `Workflow::parse` turns the IR into a `workflow::Workflow`, whose parts are known to fit.
- Each instance of a role runs in a sandbox whose first process is `sandboxd`, which dials the
  gateway and serves one session over it.
- The gateway holds the host's end of each session, a `Session`, whose owner puts and collects
  trees of files, starts commands and freezes the sandbox.
- Callers in a sandbox make ops with `sandboxctl`, which `sandboxd` carries over the session to
  the session's owner, who takes each with `Session::next_op`.
- The Docker provider starts a sandbox as a container that needs no root, with `sandboxd`
  mounted in and its session connected.
- The driver's decision core, `Core`, decides what a run of a workflow does: it takes each
  `Event` observed, such as an op from the agent, and returns the `Action`s to take, such as the
  op's reply.
- The driver's runtime, `Run`, runs a workflow on the Docker provider as the core decides, and
  keeps the run's record, whose run log holds every event the core took and the actions it
  returned.
- `xtask` runs contributors' tasks, among them the checks every change passes.

## Code map

Each crate depends only on crates listed above it.

**`crates/stdx`** (`stdx`). What the standard library lacks and several crates share: `Context`,
which prefixes an error with what failed, `abort_on_panic`, and `unpoisoned`, which takes a lock's
guard where no panic can leave it poisoned.

**`crates/pumpkin-schema`** (`pumpkin-schema`). Pumpkin's schemas, Rust types serialized as
JSON, and the sandbox protocol's wire format ([crate doc](crates/pumpkin-schema/src/lib.rs)).

- `ir`: the workflow IR, `ir::Workflow`, with each judge's result contract (`ResultSpec`). Its
  values check themselves as they deserialize (`ir::values`: `Name`, `Image`, `AbsolutePath`,
  `Argv`, `Env`, `JsonPointer`, `Returns`, ...).
- `workflow`: `Workflow::parse`, which resolves names to typed ids (`RoleId`, `JudgeId`, `SlotId`,
  `JudgeGrantId`), gives every time as a `Duration`, and reports the rules the IR breaks
  (`ParseErrors`).
- `sandbox`: the sandbox protocol (`ToSandbox`, `FromSandbox`, `Launch`, `Entry`), and the
  in-sandbox contract (`pumpkin_dir()`, `pumpkin_subdirs()`, `SANDBOXCTL_PATH`, `CONTROL_SOCKET`),
  with the paths and limits a role's commands and its session's owner share: a judge call's
  payload (`slot_payload()`), its result (`result_file()`, `RESULT_MAX_BYTES`) and evidence
  (`evidence_dir()`), and each command's environment, with its `HOME` (`command_env()`), and its
  output (`command_stdout()`, `command_stderr()`), relative to `pumpkin_dir()`, which
  `AbsolutePath::join` joins them to.
- `sandbox::frame` and `sandbox::tree`: what both ends of the protocol run, `frame::read`,
  `frame::write` and `tree::Writer`.
- `sandbox::op`: ops' calls (`OpCall`) and replies (`OpReply`), whose JSON `sandboxctl` prints.

**`crates/pumpkin-gateway`** (`pumpkin-gateway`). The host's end of sessions. `Listener::bind`
makes one sandbox's socket, and `Listener::accept` turns its one connection into a `Session`. A
`Session` puts trees (`Session::put`), starts commands (`Session::start`, `Process::wait`),
freezes (`Session::freeze`), collects trees (`Session::collect`), and hands out ops
(`Session::next_op`, `Arrival`) for its owner to answer (`Session::reply`). A reader thread per
session parses all that the sandbox sends (`route`, `Assembler`).

**`crates/sandboxd`** (`pumpkin-sandboxd`, binary `sandboxd`). The supervisor in every sandbox,
which `cargo xtask sandboxd` builds static. `session::serve` serves the session with `files`
(trees) and `commands` (starting, reaping, freezing), and, with `--ops`, `ops`, which carries
callers' ops from `CONTROL_SOCKET`. Run as `sandboxctl`, the same binary is the callers' shim
(`ctl`). `sys` holds the system calls `std` lacks.

**`providers/docker`** (`pumpkin-docker`). The Docker provider: a sandbox for a role as a
container that needs no root to start. `Engine::connect` reaches the engine of a named Docker
context (`ContextName`) and reports it (`Report`, `Security`). `Engine::start` binds a
`Listener`, creates and starts a container from a `Start` (its options in `create_args`), and
returns it as a `Container` once its `Session` is accepted. `Engine::destroy_all` removes a run's
containers by their `RunTag`. Every call to the `docker` command line goes through one function,
`docker`.

**`crates/pumpkin`** (`pumpkin`). Pumpkin's host side: the driver, which runs a workflow. The
driver's decision core is `driver::core` ([module doc](crates/pumpkin/src/driver/core.rs)).
`Core::start` begins a run of a `Workflow`, and `Core::handle` takes each `Event` with the time
since the run started; both return `Action`s, and `Core::tick_due` says when time alone next
calls for some. Its `ops` module authorizes the agent's ops (`Request`) and fills in their
replies, `jobs` meters the judge calls they start (`Jobs::usage`), and `JudgeResult::parse`
reads a judge's `result.json` per its `ResultSpec`. A run ends with `Action::Finish`, which
carries its `Outcome` (`WorkEnd`, `Verdict`).

The driver's runtime is `driver::run` ([module doc](crates/pumpkin/src/driver/run.rs)).
`Run::prepare` parses a workflow's IR and begins its record, and `Run::execute` drives the run
to its `Outcome`, unless it fails. One loop, `Runtime::drive`, turns the agent's ops and its end
(`agent`, `Agent::watch`), the reports of judge calls (`judge`, a thread per call) and ticks
into `Event`s, appends each with the actions the core returns to the run log (`log`, `Log`), and
performs them in sandboxes that `Sandboxes` starts (`sandbox`). `record` lays out the run's
record, and stores the payloads of the calls the core accepts by digest (`Payloads`).

**`xtask`**. Contributor tasks: `cargo xtask ci`, the checks every change passes, among them
`docs::check`, the doc length caps; and `cargo xtask sandboxd`.

## Boundaries and invariants

### Workspace

- A panic ends the whole process ([`Cargo.toml`](Cargo.toml),
  [`abort_on_panic`](crates/stdx/src/lib.rs)).
- Only `sandboxd`'s `sys` uses `unsafe`: the workspace warns on `unsafe_code`
  ([`Cargo.toml`](Cargo.toml)), `cargo xtask ci` denies warnings ([`ci`](xtask/src/main.rs)), and
  only `sys` expects the lint ([`sys`](crates/sandboxd/src/sys.rs)).

### `pumpkin-schema`

- Outside its tests, it depends on `serde` and `serde_json` alone
  ([crate doc](crates/pumpkin-schema/src/lib.rs),
  [`Cargo.toml`](crates/pumpkin-schema/Cargo.toml)).
- The IR states every value; defaults belong to whatever writes it
  ([`ir`](crates/pumpkin-schema/src/ir.rs)).
- Only `Workflow::parse` makes a `workflow::Workflow`, so code past it doesn't check again
  ([`workflow`](crates/pumpkin-schema/src/workflow.rs)).
- A judge grant runs one call at a time, and names a judge no other grant names, so its judge
  does too ([`ir::JudgeGrant`](crates/pumpkin-schema/src/ir.rs)).
- Every provider presents the in-sandbox contract in each sandbox it starts
  ([`sandbox`](crates/pumpkin-schema/src/sandbox.rs)).
- A passive sandbox makes no op: its session takes none without an `OpPayloadLimit`
  ([`sandbox`](crates/pumpkin-schema/src/sandbox.rs),
  [`Listener::accept`](crates/pumpkin-gateway/src/lib.rs)).
- A frame's message and body are capped ([`frame`](crates/pumpkin-schema/src/sandbox/frame.rs)).
- A tree holds only directories and regular files at `RelativePath`s, written within `Limits`
  ([`sandbox`](crates/pumpkin-schema/src/sandbox.rs),
  [`tree`](crates/pumpkin-schema/src/sandbox/tree.rs)).

### `pumpkin-gateway`

- The socket, not a secret, tells which sandbox is talking: each has a `Listener` of its own,
  which takes one connection ([`pumpkin-gateway`](crates/pumpkin-gateway/src/lib.rs)).
- Everything a session receives is untrusted, and whatever breaks the protocol ends the session
  ([`pumpkin-gateway`](crates/pumpkin-gateway/src/lib.rs)).
- The gateway authorizes no op, but bounds what ops cost the host
  ([`pumpkin-gateway`](crates/pumpkin-gateway/src/lib.rs)).

### `sandboxd`

- It depends on `pumpkin-gateway` only in its tests ([`Cargo.toml`](crates/sandboxd/Cargo.toml)),
  so the protocol's two ends share only [`pumpkin-schema`](crates/pumpkin-schema/src/lib.rs).
- `sandboxd` has only the role's privileges, so its reports are untrusted wherever the role runs
  untrusted code ([`sandboxd`](crates/sandboxd/src/main.rs)).
- `sandboxd` authorizes no op: any code in the sandbox may make one, and the session's owner
  decides what each may do ([`ops`](crates/sandboxd/src/ops.rs)).

### `pumpkin-docker`

- A container's only way out is its session's socket: it has no network but loopback
  ([`pumpkin-docker`](providers/docker/src/lib.rs)).
- Files move only over the session, never through the Docker daemon
  ([`pumpkin-docker`](providers/docker/src/lib.rs)).
- Each container carries its run's `RunTag`, so `Engine::destroy_all` finds a run's containers
  after the process that started them is gone ([`pumpkin-docker`](providers/docker/src/lib.rs)).
- `docker` reaches only the engine of the context named, whatever the environment
  ([`pumpkin-docker`](providers/docker/src/lib.rs): its envelope, and `docker`).

### `pumpkin`

- The decision core does no I/O and reads no clock, so the same events give the same actions
  ([`core`](crates/pumpkin/src/driver/core.rs)).
- The core authorizes every op the agent makes ([`ops`](crates/pumpkin/src/driver/core/ops.rs)),
  and answers exactly once each op it is given ([`Core`](crates/pumpkin/src/driver/core.rs)).
- What a judge grant has used is counted from its jobs, so no separate ledger can disagree with
  them ([`jobs`](crates/pumpkin/src/driver/core/jobs.rs)).
- The agent learns of a judge call only its state and the fields its judge declares
  ([`ResultSpec`](crates/pumpkin-schema/src/ir.rs),
  [`Failure`](crates/pumpkin/src/driver/core/jobs.rs)).
- One loop drives a run and owns its state; each judge call runs on a thread of its own, which
  reports to the loop ([`run`](crates/pumpkin/src/driver/run.rs)).
- The run log holds every event the core takes, with the actions it returns, each written and
  synced before they are performed, and every op the agent's session hands out, those the
  gateway refuses itself included ([`log`](crates/pumpkin/src/driver/run/log.rs)).
- A call's payload is stored only once the core accepts the call, so a refused one leaves only
  its size and digest in the log ([`Runtime::reply`](crates/pumpkin/src/driver/run.rs)).
- Of a judge's `out`, the record keeps only its `result_file()`, up to `RESULT_MAX_BYTES`, and
  its `evidence_dir()`, up to the judge's `evidence_max_bytes`; a call over either limit has no
  valid result, so every valid result has its evidence in the record
  ([`judge`](crates/pumpkin/src/driver/run/judge.rs)).
- A run leaves no container behind: a judge call's end is reported once its sandbox is
  destroyed, a sandbox that can't be destroyed fails the run, and a run, however it ends,
  removes every container that carries its `RunTag` once its judge calls' threads have ended
  ([`judge`](crates/pumpkin/src/driver/run/judge.rs),
  [`Run::execute`](crates/pumpkin/src/driver/run.rs)). Whatever ends the process at once, such
  as a signal or a panic, leaves the run's containers, for `Engine::destroy_all` to remove by
  its `RunTag`, and its socket directory `pumpkin-<id>`.

## Where new code goes

- **A Pumpkin schema** (an IR field, a protocol message, an op's call or reply): in
  `pumpkin-schema`, in the module of its format, never a copy in a consumer. A format another
  program defines, as `docker info` defines its JSON (`InfoJson`), stays with the code that reads
  it.
- **An IR field or rule:** its type in `ir`, and a check on one value in `ir::values` or, where
  it needs a protocol's limit, in `Workflow::parse`; its parsed form in `workflow`, and a rule
  across parts in `Workflow::parse`; and the field in the fixture
  the crate's tests parse ([`tests/ir.rs`](crates/pumpkin-schema/tests/ir.rs)).
- **A request from the gateway to `sandboxd`:** a `ToSandbox` message and its `FromSandbox`
  replies; a `Session` method in the gateway; its handling in `sandboxd`'s `session`, by the
  module that serves it (`files`, `commands`).
- **An op:** its call and reply in `sandbox::op`, with a row in its table; its command in
  `sandboxctl`'s grammar (`ctl`); its `Op` in the gateway; its `Request` in the decision core.
- **A path or limit every sandbox relies on, whatever its provider:** `pumpkin_schema::sandbox`,
  beside `pumpkin_dir()`.
- **A system call `std` lacks, for `sandboxd`:** `sys`.
- **A decision of a run** (when its work phase ends, whether an op is allowed, what its reply
  says): `driver::core`, an op's rule in `ops` and judge calls' metering in `jobs`. What the core
  must know comes in as an `Event`, and what must be done goes out as an `Action`.
- **What a run does in a sandbox** (starting it, putting files in, collecting what it wrote,
  destroying it): `driver::run`, the agent's sandbox in `agent`, a judge call's in `judge`, and
  what both share in `sandbox`; the loop performs each `Action` in `Runtime::perform`.
- **A file of a run's record, or a record of its run log:** `record`, whose module doc lists the
  record's files, and `log`, whose module doc lists the log's records and which gives each
  `Event` and `Action` its JSON form.
- **A provider:** `providers/<name>/`, crate `pumpkin-<name>`, which the workspace's
  `providers/*` glob takes in. Its platform's calls and its sandboxes' confinement stay in its
  crate, as `docker` and `create_args` stay in `pumpkin-docker`.
- **A command:** its grammar in an `xflags` `flags` module beside the binary's `main`, as
  `sandboxd`, `sandboxctl` and `xtask` have. A contributor task is an `xtask` command; a check is a
  step of `cargo xtask ci`.
- **A helper `std` lacks that several crates need:** `stdx`.
