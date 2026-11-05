# Architecture

A map of Pumpkin's code: where each thing lives, and what stays true across it. The rules for
changing the code are in [`AGENTS.md`](AGENTS.md). Names in backticks are for symbol search.

## Bird's-eye view

Pumpkin runs workflows, in which an untrusted agent works and judges score what it submits. A
workflow is data, JSON in Pumpkin's IR: one agent, its judges, and a pattern that grants the agent
judge calls and a submission, and ends the run with a verdict. Each instance of a role is a
sandbox, a judge's afresh for each call, whose first process, `sandboxd`, dials the host and serves
one session: the host puts and collects files, starts commands and freezes the sandbox, and code
in the sandbox makes ops with `sandboxctl`. On the host, the gateway holds each session's end, the
driver's decision core decides what a run does, and a provider, such as Docker's, makes sandboxes.
The session is the boundary that matters most: anything past it may be the agent's.

## Entry points

The binaries are `sandboxd`, also `sandboxctl` (`crates/sandboxd/src/main.rs`), and `cargo xtask`
(`xtask/src/main.rs`). The key types: `ir::Workflow`, a workflow as written; `workflow::Workflow`,
parsed; `Core`, a run's decisions; `Session`, the host's end of a sandbox; `Engine`, Docker's.

## Code map

Each crate depends only on crates above it. **Architecture Invariant** callouts often name absences.

**`crates/stdx`.** What `std` lacks and several crates share, such as `Context` for errors.
- **Architecture Invariant:** it knows nothing of Pumpkin: it depends on no Pumpkin crate.

**`crates/pumpkin-schema`.** The formats Pumpkin's programs share, never copied into a consumer:
`ir`, the workflow IR; `workflow`, a workflow parsed; `sandbox`, the sandbox protocol, its `frame`s,
`tree`s, ops (`sandbox::op`) and the paths every sandbox has. Another program's format, such as
`docker info`'s, stays with the code that reads it. **API Boundary:** the IR, `sandboxctl`'s JSON.
- **Architecture Invariant:** outside its tests, it depends on `serde` and `serde_json` alone,
  because `sandboxd`, a small static binary, depends on it.
- **Architecture Invariant:** only parsing makes a `workflow::Workflow`, so code past parsing need
  not check again what parsing established.
- **Architecture Invariant:** the IR holds no defaults: an absent field means what its doc says,
  such as no limit; whatever writes the IR picks the defaults.
- **Architecture Invariant:** no grant names the judge that scores, so the agent never calls it.
- **Architecture Invariant:** a tree holds only directories and regular files, below the directory
  it is written to: no link, device or `..` crosses a session.

**`crates/pumpkin-gateway`.** The host's end of sessions: a sandbox's `Listener` makes its
`Session`, whose one owner moves trees, runs commands (`Process`) and answers ops (`Arrival`).
- **Architecture Invariant:** the socket, not a secret, tells which sandbox is talking: each has
  its own `Listener`, which takes one connection.
- **Architecture Invariant:** everything a session receives is untrusted: a protocol break ends
  the session, and a sandbox costs the host only bounded memory and bounded waits.
- **Architecture Invariant:** a passive sandbox, whose role makes no ops, can make none.
- **Architecture Invariant:** it knows no provider and no driver, so every provider's sandboxes
  have the same sessions: of Pumpkin's crates, it depends only on `pumpkin-schema` and `stdx`.

**`crates/sandboxd`.** The supervisor in every sandbox, built static by `cargo xtask sandboxd`:
`session` serves the gateway (`files`, `commands`), `ops` carries callers' ops, `ctl` is
`sandboxctl`, and `sys` holds the system calls `std` lacks.
- **Architecture Invariant:** the protocol's two ends share no Pumpkin crate but `pumpkin-schema`
  and `stdx`, so the protocol is all they agree on: `sandboxd` uses the gateway only in tests.
- **Architecture Invariant:** it has only its role's privileges, so its reports are untrusted
  wherever the role runs untrusted code.

**`providers/docker`.** `pumpkin-docker`, a sandbox as a container that needs no root to start:
`Engine` reaches the engine of a named Docker context (`ContextName`), reports it (`Report`),
starts `Container`s and removes a run's by their `RunTag`. A provider is `providers/<name>`,
crate `pumpkin-<name>`, which holds its platform's calls and its sandboxes' confinement.
- **Architecture Invariant:** every provider starts `sandboxd` first in each sandbox, and presents
  the paths `sandbox` names, so a role's commands and the session's owner find them anywhere.
- **Architecture Invariant:** each container carries its run's `RunTag`, so a run's containers are
  found and removed after the process that started them is gone.
- **Architecture Invariant:** production code reaches Docker only through `Engine`, which runs
  `docker` against the named context alone, so the operator's choice of engine holds. Tests run
  `docker` directly only to observe containers or build images.
- **Architecture Invariant:** a container's only way out is its session's socket: it has no
  network but loopback, and the role's files move over the session, never through the daemon.

**`crates/pumpkin`.** The host side: the driver. Its decision core, `driver::core`, takes each
`Event` a runtime observes and returns the `Action`s the runtime is to perform, until one ends the
run with its `Outcome`; its `ops` rules on ops, `jobs` meters judge calls, and `result` reads a
call's `result.json` (`JudgeResult`).
- **Architecture Invariant:** the decision core does no I/O and reads no clock: time reaches it with
  each event, so the same events give the same actions.
- **Architecture Invariant:** only the decision core authorizes ops: `sandboxd` carries any op any
  code in the sandbox makes, and the gateway bounds only what ops cost the host.
- **Architecture Invariant:** the agent learns of a judge call only its state, what its grant has
  left, and the fields its judge declares (`ResultSpec`), so the workflow decides its feedback.
- **Architecture Invariant:** what a grant has used is counted from its jobs, never in a ledger.
- **Architecture Invariant:** the final judge starts only once the agent's sandbox and its jobs
  are gone, so nothing the agent started runs while its submission is judged.

**`xtask`.** `cargo xtask ci`, the checks every change passes (its doc length caps in `docs`, and
with `--docker` the tests that need Docker), and `cargo xtask sandboxd`. A check is a step of `ci`.

## Cross-cutting concerns

**Errors.** What the world can cause is returned or handled (`AGENTS.md`). **Architecture
Invariant:** a panic is a bug, and outside tests it ends the whole process: no code outside tests
catches one, since what a panic leaves behind can't be trusted. Release builds abort, and each
binary that runs threads installs `stdx`'s hook, which aborts in dev builds too.

**Unsafe.** **Architecture Invariant:** `unsafe` is only for a system call `std` lacks, each use
under `#[expect(unsafe_code, reason = "…")]`, as the workspace's lints and `ci` require.

**Concurrency.** Ownership is single, as `AGENTS.md` says. **Architecture Invariant:** threads
exist only where I/O blocks, and a value they share says beside it who shares it.

**Command lines.** A binary's grammar is an `xflags` `flags` module beside its `main`.
