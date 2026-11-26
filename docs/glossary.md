# Glossary

This page defines each concept of Pumpkin's code, docs and help once. An entry first says what kind
of thing the term names, and what it is for. It uses plain words or linked terms, never the term or
a synonym, then the code's names and a link to their home. **Not** names a word to avoid. **Open**
marks a word with several meanings, whose rename is undecided. `cargo xtask ci` refuses each word
of the `retired` table in [`.config/tidy.toml`](../.config/tidy.toml), and names the word to write
instead.

### Action

An instruction that the [decision core](#decision-core) gives the [driver](#driver), such as to
start a [call](#call) (`decision::Action`).

### Agent

A [role](#role) that works on the [task](#task), such as an AI coding agent, in its own
[sandbox](#sandbox). It runs a [harness](#harness), and holds [grants](#grant) and a
[model route](#model-route) ([roles](workflows.md#roles)).

### Asset

Files that each [sandbox](#sandbox) of a [role](#role) holds read-only at a path, such as a model's
weights. Each file is pinned by its SHA-256 and fetched from an `https` URL. A digest over the files
names them, so a [provider](#provider) keeps them once. The role's `assets` key lists them
(`ir::Asset`).

### Body

A function, a `def` in a [task](#task)'s file, that a [call](#call) runs with a handle on its
[target](#target)'s sandbox or the run's state ([bodies](workflows.md#bodies)).

### Budget

A spending limit in US dollars on one entry of a [model route](#model-route) (`ir::ModelBudget`).
**Open:** `lang`'s `Budget` is the bytes that evaluating a [task](#task) may allocate.

### Call

A request to run a function: a role's use of a [grant](#grant), or the [final call](#final-call). A
grant's call is checked against the grant first ([grants](workflows.md#grants)).

### Class

A kind of [sandbox](#sandbox) that a [provider](#provider) offers, such as a machine size, with its
resources, startup bound and price (`workflow::Class`).

### Companion

A [container](#container) that a [runner](#runner) declares beside [`main`](#main) in each of its
[sandboxes](#sandbox), with its own image and user. A [holder](#holder) names it to run or copy
there: `container =` in a [body](#body), `--container` to `sandboxctl`. The runner's `companions`
key lists them (`ir::Companion`).

### Container

An isolated set of processes in a [sandbox](#sandbox), started from an [image](#image), with its
own files, user and address: [`main`](#main), or a [companion](#companion) (`ir::Container`).
**Open:** also a Docker container, in which the Docker and EC2 providers run a sandbox's `main`.
**Not** for a sandbox as a whole.

### Controller

A [team](#team)'s shared computer, an EC2 instance or a Docker container, that runs its members'
[runs](#run) through `pumpkin controller serve` ([the controller](providers.md#the-controller)).

### Decision core

The part of the [driver](#driver) that decides a run's next steps from each [event](#event), with
no I/O or clock (`decision::Core`). Also **core**.

### Driver

The one process that carries out a [run](#run): it feeds the [decision core](#decision-core), acts
on [providers](#provider) and writes the [record](#record) (the `runtime` crate).

### Engine

The Docker daemon on which Pumpkin builds every [image](#image): that of the [machine](#machine)
that runs the driver (`engine::Engine`).

### Event

A fact that the [driver](#driver) observed, such as a call's end, which it logs and hands the
[decision core](#decision-core) (`decision::Event`).

### Final call

The [call](#call) that examines the agents' [submissions](#submission) once their work ends. Its
[body](#body) returns the run's [result](#result) ([the final call](workflows.md#the-final-call)).
**Not** judge.

### Gateway

The part of the [driver](#driver) that every [sandbox](#sandbox) connects to, holding its end of
each [session](#session) (the `gateway` crate).

### Grant

A role's permission to call a function by a name: a [job function](#job-function), a
[now function](#now-function), or a [session](#session)'s six functions (`ir::Grant`).

### Harness

The program that an [agent](#agent) runs, such as Codex, with its files (`ir::AgentRuntime`).
Pumpkin's library holds several, under `@pumpkin//harness/`.

### Hard end

A [run](#run)'s time limit, after the work deadline, by which every sandbox ends and the core
abandons it (`decision::lasts`).

### Holder

The role that a [lease](#lease) is lent to: an [agent](#agent), or a [target](#target) in each of
its calls (`workflow::Holder`).

### Host

**Open.** The machine that a run's driver runs on (`runtime::Host`), or a session's side opposite
the sandbox. Also a Docker controller's host, and what `pumpkin-admin install` puts on a
controller (`pumpkin::host`). Also a provider's shared machine, a network host name, and what a
body's handle reaches (`lang`'s `Host`).

### Image

A packaged file system, in Docker's format, from which a [role](#role)'s sandboxes start, built
from a [recipe](#recipe) on the [engine](#engine).

### Instance

**Open.** An EC2 instance, an AWS virtual machine. Also a role's [sandbox](#sandbox), and a
[lease](#lease), as a session's `instances` (most open at once) and `instance_seconds` (longest
life) count them.

### IR

The intermediate representation of a [workflow](#workflow): JSON data, which evaluating a
[task](#task) writes. The runtime reads nothing else of the task (`schema::ir`).

### Job

**Open.** A [call](#call) of a [job function](#job-function) or the final call, numbered by `JobId`
and kept in the record's `jobs/<n>/`. Also a [controller](#controller)'s run, in a systemd unit of
its own (`pumpkin::job`).

### Job function

A [grant](#grant)'s function whose [body](#body) runs on a sandbox of its [target](#target), one
call at a time, while the caller goes on (`ir::JobFunction`).

### Lease

The loan of a fresh [runner](#runner) sandbox to an agent or a target's call, until it closes or
times out (`ir::Session`). A [session](#session)'s `<name>.open` opens it.

### Lost

Said of a [run](#run) whose [driver](#driver) ended before writing its [outcome](#outcome). A
[reap](#reap) records it as failed ([records](record.md#what-a-record-holds)).

### Machine

A computer that runs `pumpkin`: the operator's own, or a [controller](#controller). **Not** host.

### `main`

The [container](#container) of each [sandbox](#sandbox) that runs its [role](#role)'s
[image](#image) (`Container::Main`). A command or a copy goes there unless it names a
[companion](#companion).

### Meeting

How a [container](#container) first reaches the [gateway](#gateway), such as by a local socket,
and sets up its [session](#session) (`provider::Meeting`). Then it has met, and its sandbox once
all have (`container_met`, `sandbox_met`).

### Member

A person who may send runs to a [controller](#controller), which admits their ssh key by its list
(the `members` crate).

### Model provider

The company that answers [model requests](#model-request), such as OpenAI, with its key. **Not**
provider alone, which is a sandbox's.

### Model request

A request to an AI model, from code in a [sandbox](#sandbox), which the [model route](#model-route)
admits or refuses ([models](models.md)).

### Model route

A [role](#role)'s list of the model APIs its code may use, each with the models it allows and any
[budget](#budget) (`ir::ModelRoute`). Also **route**.

### Now function

A [grant](#grant)'s function whose [body](#body) answers at once, with no sandbox. It may set the
caller's [submission](#submission) or end its work (`ir::NowFunction`).

### Op

A request, such as a [call](#call), that code in a [sandbox](#sandbox) makes with the `sandboxctl`
command. The [decision core](#decision-core) answers each (`schema::sandbox::op`). **Not**
operation.

### Operator

The person who runs `pumpkin` on their [machine](#machine). **Not** user, a role's numeric Unix
user (`ir::User`).

### Outcome

The account of how a [run](#run) ended, in `outcome.json`: each agent's [submission](#submission)
and the final call's verdict, if it finished (`decision::Outcome`).

### Placing

Choosing for each [role](#role) the [provider](#provider) and the smallest [class](#class) that
fits its needs (`Workflow::place`, [placing a run](providers.md#placing-a-run)).

### Provider

A service that creates, lists and deletes [sandboxes](#sandbox): Docker, EC2 or Modal
(`provider::Provider`, [providers](providers.md)).

### Prune

A [controller](#controller)'s removal of old [records](#record), installs, job files and
[images](#image), so its disk doesn't fill (the `retention` crate, `engine::prune`).

### Reap

The cleanup of what an ended [run](#run) left on its [providers](#provider), found by its
[record](#record) and tag ([stopping runs](running.md#stopping-runs-and-cleaning-up)).

### Record

A [run](#run)'s directory, which its [driver](#driver) writes and no sandbox can reach: its log,
outputs and [outcome](#outcome) ([records](record.md)).

### Recipe

A Dockerfile for an [image](#image), whose build may fetch only what a digest, checksum or commit
pins (the `recipe` crate).

### Relay

The part of [`main`](#main)'s `sandboxd` that carries each of the [gateway](#gateway)'s
connections to the [companion](#companion) whose name it starts with (`sandbox::route`). Only a
[sandbox](#sandbox) with companions that the gateway dials has one. Its create gives the relay's
address (`Listened::relay`), and each companion's `sandbox::Relayed`. **Open:** also
`runtime::relay`'s verb, what a call's thread does between its target's session and the loop.

### Result

The values, such as a score, that the [final call](#final-call)'s [body](#body) returns
([the final call](workflows.md#the-final-call)).

### Role

A template for [sandboxes](#sandbox): an [image](#image), a Unix user and what each sandbox needs.
Its `runtime` key gives its kind: [agent](#agent), [target](#target) or [runner](#runner)
(`ir::Role`).

### Run

One execution of a [workflow](#workflow), from its start by `pumpkin run` to its
[outcome](#outcome), with its [record](#record).

### Runner

A [role](#role) whose sandboxes are lent, each fresh, to an agent or a target's call to run
commands in (`ir::RunnerRuntime`).

### Runtime

**Open.** The `runtime` crate, the [driver](#driver)'s code. Also a role's `runtime` key, its kind
(`ir::Runtime`).

### Sandbox

An isolated environment, such as a Docker container, that a [provider](#provider) creates for one
instance of a [role](#role). `sandboxd`, its supervisor, serves its [session](#session)
(`schema::sandbox`).

### Session

**Open.** A connection between the [gateway](#gateway) and a [container](#container)'s `sandboxd`,
its only way out (`gateway::Session`). A sandbox has one for each container. Also a
[grant](#grant)'s kind that lends [runners](#runner) (`ir::Session`).

### Submission

The bytes that an [agent](#agent) hands in for the [final call](#final-call), as a
[now function](#now-function)'s `run.set_submission` last set them. **Open:** the core's
`Action::StartFinalCall` names them `selections`.

### Sweep

The part of a [provider](#provider) that lists and deletes a [run](#run)'s sandboxes by its tag,
all that a [reap](#reap) needs (`provider::Sweep`).

### Target

A [role](#role) whose sandboxes run the commands that a [call](#call)'s [body](#body) sends, out of
the calling agent's reach ([roles](workflows.md#roles)). **Open:** `host::Target` is what a
controller runs on. `config::Target` is where a machine's runs go.

### Task

A piece of work for agents, such as a benchmark problem, in Starlark, a Python-like language. Its
entry file evaluates to a [workflow](#workflow) ([the language](language.md)).

### Team

The people who share a [controller](#controller), its [members](#member), and its deployment
([`deploy/aws`](../deploy/aws/README.md)).

### Work

The first of a run's two phases, in which [agents](#agent) run and submit, until each finishes or
the work deadline, `deadline_seconds`, passes.

### Work target

A [target](#target) whose one [sandbox](#sandbox) lasts the whole [work](#work) phase, and serves
each of its [calls](#call) in turn, keeping the files that each call leaves. A target's `lifetime`
key chooses this: `work_phase` for one sandbox for the work phase, or `per_call`, the default, for
a fresh sandbox per call (`ir::TargetLifetime`). **Open:** it also goes by shared sandbox,
`work_phase` target and work sandbox, and by the record's `shared/<role>/`.

### Workflow

The data that sets out a [run](#run): its [roles](#role), [grants](#grant),
[model routes](#model-route), work deadline and [final call](#final-call) (`ir::Workflow`,
[workflows](workflows.md)).
