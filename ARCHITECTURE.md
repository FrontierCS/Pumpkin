# Architecture

A map of Pumpkin's code: where each thing lives, and what stays true across it. The rules for
changing the code are in [`AGENTS.md`](AGENTS.md). Names in backticks are for symbol search.

## Bird's-eye view

Pumpkin runs workflows, in which an agent works and judges score what it submits. A
workflow is data, JSON in Pumpkin's IR: one agent, its judges, and a pattern that grants the agent
judge calls and a submission, and ends the run with a verdict. Each instance of a role is a
sandbox, a judge's afresh for each call, whose first process, `sandboxd`, dials the host and serves
one session: the host puts and collects files, starts commands and freezes the sandbox, and code
in the sandbox makes ops with `sandboxctl`. On the host, the gateway holds each session's end, the
driver's decision core decides what a run does, and a provider, such as Docker's, makes sandboxes.
The session is the boundary that matters most: a sandbox may be buggy, and the agent is
opportunistic and goal-directed, using whatever it can reach to finish its task or raise its
score. So each guard either limits what the agent's code can reach, or keeps a buggy sandbox from
harming the host or the run.

## Entry points

The binaries are `pumpkin` (`crates/pumpkin-cli/src/main.rs`), which runs a workflow,
`sandboxd`, also `sandboxctl` (`crates/sandboxd/src/main.rs`), and `cargo xtask`
(`xtask/src/main.rs`). The key types: `ir::Workflow`, a workflow as written; `workflow::Workflow`,
parsed; `Core`, a run's decisions; `Session`, the host's end of a sandbox; `Provider`, a provider a
run has connected to; `Engine`, Docker's.

## Front ends

The workflow IR (`pumpkin-schema`'s `ir`) is the only input the runtime reads, and it holds no
defaults. Programs that write it are front ends: today, Rust factories, such as a task
repository's. A more general Pumpkin may offer its own workflow language for programmability, as
Bazel offers Starlark and Terraform HCL; like theirs, it would be a front end that evaluates to
the IR, so the runtime and the decision core never see it.

## Code map

Each crate depends only on crates above it. **Architecture Invariant** callouts often name absences.

**`crates/stdx`.** What `std` lacks and several crates share, such as `Context` for errors.
- **Architecture Invariant:** it knows nothing of Pumpkin: it depends on no Pumpkin crate.

**`crates/pumpkin-schema`.** The formats Pumpkin's programs share, never copied into a consumer:
`ir`, the workflow IR; `workflow`, a workflow parsed; `sandbox`, the sandbox protocol, its `frame`s,
`tree`s, ops (`sandbox::op`) and the in-sandbox contract: the paths, limits and environment a
role's commands and its session's owner share; `provider`, the values a run and its providers
exchange. Another program's format, such as `docker info`'s, stays with the code that reads it.
**API Boundary:** the IR, the in-sandbox contract, `sandboxctl`'s JSON.
- **Architecture Invariant:** outside its tests, it depends on `serde` and `serde_json` alone,
  because `sandboxd`, a small static binary, depends on it.
- **Architecture Invariant:** only parsing makes a `workflow::Workflow`, so code past parsing need
  not check again what parsing established.
- **Architecture Invariant:** the IR holds no defaults: an absent field means what its doc says,
  such as no limit; whatever writes the IR picks the defaults.

**`crates/pumpkin-gateway`.** The host's end of sessions: a sandbox's `Listener` makes its
`Session`, whose one owner moves trees, runs commands (`Process`) and answers ops, hearing of each
op, each model request, each command's exit and the session's end as an `Arrival` sent to it.
- **Architecture Invariant:** a sandbox can't harm the host through its session: anything out of
  protocol ends the session, and a session costs the host only bounded memory and bounded waits.
- **Architecture Invariant:** it knows no provider and no driver, so every provider's sandboxes
  have the same sessions: of Pumpkin's crates, it depends only on `pumpkin-schema` and `stdx`.

**`crates/sandboxd`.** The supervisor in every sandbox, built static by `cargo xtask sandboxd`:
`session` serves the gateway (`files`, `commands`), `ops` carries callers' ops, `model` serves
the model endpoint, `ctl` is `sandboxctl`, and `sys` holds the system calls `std` lacks.
- **Architecture Invariant:** the protocol's two ends share no Pumpkin crate but `pumpkin-schema`
  and `stdx`, so the protocol is all they agree on: `sandboxd` uses the gateway only in tests.
- **Architecture Invariant:** it has only its role's privileges, so it is no boundary within its
  sandbox: where the role runs the agent's code, that code can shape what `sandboxd` reports.

**`crates/pumpkin-provider`.** `Provider`, a run's one handle on a provider for every side effect
on it: it publishes what the run's sandboxes run, creates, lists and deletes them, and reads their
consoles.
- **Architecture Invariant:** it knows no provider: of Pumpkin's crates it depends only on
  `pumpkin-schema`.
- **Architecture Invariant:** every call on sandboxes names its run and keeps to that run's, by
  its tag, so runs that share a provider never reach each other's sandboxes.

**`providers/docker`.** `pumpkin-docker`, a sandbox as a container that needs no root to start:
`Engine` reaches the engine of a named Docker context (`ContextName`), reports it (`Report`), is
the `Provider` that makes each sandbox a container, and builds images, each tagged by its build's
inputs. A provider is `providers/<name>`, crate `pumpkin-<name>`, which holds its platform's
calls and its sandboxes' confinement.
- **Architecture Invariant:** a provider holds no session, and the run's times reach it as data:
  its sandboxes dial what the runtime gives them (`Dial`), the gateway accepts them, and a provider
  bounds only its own platform calls, so no provider depends on `pumpkin-gateway` outside its
  tests.
- **Architecture Invariant:** every provider starts `sandboxd` first in each sandbox, and presents
  `/pumpkin` with the directories `sandbox` says every sandbox has, so a role's commands and the
  session's owner find them anywhere.
- **Architecture Invariant:** production code reaches Docker only through `Engine`, which runs
  `docker` against the named context alone, so the operator's choice of engine holds. Tests run
  `docker` directly only to observe containers and images, pause containers, or build, tag and
  remove images of their own.
- **Architecture Invariant:** a container's only way out is its session's socket: it has no
  network but loopback, and the role's files move over the session, never through the daemon.

**`crates/pumpkin-decision`.** The driver's decision core: `Core` takes each `Event` a runtime
observes and returns the `Action`s the runtime is to perform, until one ends the run, with its
`Outcome`, or abandons it at its hard end; its `ops` rules on ops, `jobs` meters judge calls, and
`result` reads a call's `result.json` (`JudgeResult`).
- **Architecture Invariant:** the decision core does no I/O, reads no clock and uses no randomly
  seeded hash: time reaches it with each event, so the same events give the same actions, and it
  decides when time ends the work phase or the run, the hard end included. The
  crate is `#![no_std]`, so it can name none of `std`'s files, network, processes, environment,
  threads, clock or hashed collections, and it forbids `unsafe`, which `core::arch`'s clock
  needs. It depends on `pumpkin-schema` alone of Pumpkin's crates; the schema's I/O items
  (`tree::Writer`, `frame` over `std::io`, and `PathBuf` paths) stay reachable through it until the
  planned purification moves them out of `pumpkin-schema`.
- **Architecture Invariant:** only the decision core authorizes ops: `sandboxd` carries any op any
  code in the sandbox makes, and the gateway bounds only what ops cost the host.
- **Architecture Invariant:** the agent learns of a judge call only its state, what its grant has
  left, and the fields its judge declares (`ResultSpec`), so the workflow decides its feedback.

**`crates/pumpkin-model-route`.** The model route: `Route` closes the model requests that an
agent's sandbox makes over its session, and `Upstream` forwards a closed one to the model provider
and says how it answered (`Answer`); `Serving` is the upstream that serves a run's route, with the
model names it takes. The crate holds the route's rules and all that reaches the model provider,
so the TLS client (`ureq`, with `rustls` and `ring`) is in this crate alone.
- **Architecture Invariant:** the route stays closed: a request reaches the models its workflow
  allows and nothing else of the model provider's, so a response carries nothing but what its own
  request sent. The rules are in the crate's docs.
- **Architecture Invariant:** the route knows no run: of Pumpkin's crates it depends on
  `pumpkin-gateway`, `pumpkin-schema` and `stdx` only, and the runtime depends on it, never the
  reverse; what the run does with an answer, its health and fault rule, is the runtime's.

**`crates/pumpkin`.** The host side: the driver's runtime, `driver::run`, which prepares each
`Run`, then drives the decision core on the run's `Provider`, keeping the run's record, whose
files `record` lays out, and its run log, whose records `log` does. `model` serves the agent's
model requests through `pumpkin-model-route`'s `Route` and `Upstream`, and judges the model
provider's answers, until the agent finalizes, the work phase ends, or the provider refuses the
route's key.
- **Architecture Invariant:** the final judge starts only once the agent's sandbox and its jobs
  are gone, so nothing the agent started runs while its submission is judged.
- **Architecture Invariant:** the runtime is written once, against `Provider`: the library depends
  on no provider's crate, only its tests do, so a new provider changes no runtime code.
- **Architecture Invariant:** every sandbox starts on the runtime's one start path, `sandbox`, as
  the runtime reaches a run's provider only through `Sandboxes`; and a run's end deletes
  whatever its provider still lists of the run, so a run ends with none of its sandboxes left, or
  fails.

**`crates/pumpkin-cli`.** `pumpkin`, the operator's command line: `run` reaches the engine of a
named Docker context and runs a workflow on it with `Run`: it reaches a run's provider and hands
it to `Run` as a `Provider`. It reads the deployment's configuration (`config`, TOML): the model
upstreams that a workflow's model route may reach, of which it hands `Run` the one that serves
the route (`Serving`).

**`crates/pumpkin-harness`.** The harnesses agents run, such as Codex (`codex`), each pinned by a
`Pin`, whose recipe adds the harness to a task's agent image in a layer of its own, below the
directory the in-sandbox contract reserves for it, and each with an adapter, such as `Codex`,
that writes the agent's runtime that runs it.
- **Architecture Invariant:** it knows no task and no provider: of Pumpkin's crates, outside its
  tests it depends only on `pumpkin-schema`, and its Docker test also uses `stdx`. A harness
  therefore reaches an agent alike whichever factory builds its image, and any factory, in this
  repository or outside it, may use it.

**`xtask`.** `cargo xtask ci`, the checks every change passes (its doc length caps in `docs`, and
with `--docker` the tests that need Docker), and `cargo xtask sandboxd`. A check is a step of `ci`.

## Cross-cutting concerns

Errors, panics, `unsafe` and concurrency follow the rules in [`AGENTS.md`](AGENTS.md).
