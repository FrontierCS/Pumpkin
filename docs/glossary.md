# Glossary

This page defines each concept of Pumpkin's code, docs and help once, in the code's words. An
entry says what the word means and links the home of its details: a type's doc, a command's help
or a guide. **Not** names a word to avoid for the concept. **Open** marks a word with more than
one meaning, whose rename is undecided. `cargo xtask ci` refuses each word of the `retired` table
in [`.config/tidy.toml`](../.config/tidy.toml), and names the word to write instead.

### Action

What the decision core returns for the runtime to perform, such as a call's start
(`decision::Action`).

### Agent

A role whose harness works on the task, in sandboxes that run its command once
([roles](workflows.md#roles)). It holds grants and a model route.

### Body

A `def` of a workflow's file that a call runs, with a handle to its sandbox
([bodies](workflows.md#bodies)).

### Budget

The most that one entry of a role's model route may spend, in US dollars (`ir::ModelBudget`).
**Open:** `lang`'s `Budget` is the bytes that an evaluation may allocate.

### Call

One use of a grant's function by its holder. Each call is checked against the grant before
anything runs ([grants](workflows.md#grants)).

### Class

A kind of sandbox that a provider offers, such as a size of machine, with what it offers and its
startup bound (`workflow::Class`).

### Container

A Docker container. The Docker and EC2 providers run each sandbox in one, and `deploy/docker`
runs a controller in one. **Not** for a sandbox in general.

### Controller

A team's machine that runs the runs which members' machines send it, through
`pumpkin controller serve` ([the controller](providers.md#the-controller)).

### Decision core

The part of the driver that decides, from each event, the run's actions, with no I/O or clock
(`decision::Core`). Also **core**.

### Driver

The one process that carries out a run. It drives the decision core, acts on the providers and
writes the record (the `runtime` crate).

### Engine

A Docker engine, which builds every image. The operator's is `engine::Engine`.

### Event

What the runtime observed and hands the decision core, which the run log records
(`decision::Event`).

### Final call

The call that judges the agents' submissions once their work has ended. What its body returns is
the run's result ([the final call](workflows.md#the-final-call)). **Not** judge.

### Gateway

The host's end of every sandbox's session (the `gateway` crate).

### Grant

A name and one function, which a role holds and calls by that name (`ir::Grant`). Its kind is a
job function, a now function or a session.

### Harness

The program that an agent runs, with its files: the IR's `AgentRuntime`, and a directory of the
standard library, such as `harness/codex/`.

### Hard end

The latest that a run may last, by which the core abandons it (`decision::lasts`).

### Holder

The role that holds a session, and drives its leases: an agent, or a target in each of its calls
(`workflow::Holder`).

### Host

**Open.** It names the machine that a run's driver runs on (`runtime::Host`), and the side of a
session opposite the sandbox. It also names a Docker controller's host, what `pumpkin-admin`
installs on a controller (`pumpkin::host`), the machine that a provider's sandboxes share, a
network's host name, and what a body reaches through `lang`'s `Host`.

### Image

What a role's sandboxes run, built from its recipe on an engine, and tagged by the digest of its
build's inputs.

### Instance

**Open.** An EC2 instance. Also a sandbox, as an instance of its role, and a lease, as a session
grant's `instances` and `instance_seconds` count it.

### IR

The workflow IR: a workflow as JSON data, which the runtime alone reads (`schema::ir`).

### Job

**Open.** A job function's call, which the core follows by its `JobId` and the record keeps in
`jobs/<n>/`. Also a run that a controller holds, from its send to its end, in a systemd unit of
its own (`pumpkin::job`).

### Job function

A grant's function that runs its body on a sandbox of its target, one call at a time
(`ir::JobFunction`).

### Lease

A holder's use of a fresh sandbox of a runner, from `.open` to its end (`ir::Session`).

### Lost

Said of a run whose driver ended before it wrote the run's outcome. A reap records it as failed
([records](record.md#what-a-record-holds)).

### Machine

A computer that runs `pumpkin`: the operator's own, or a controller. **Not** host.

### Meeting

How a provider's sandboxes meet the gateway, which sets up each session (`provider::Meeting`).
A sandbox has met once its session is set up.

### Member

A person whose key a team's controller admits, by its list of members (the `members` crate).

### Model provider

The service that answers model requests, with its key. **Not** provider alone, which is a
sandbox's.

### Model request

A request that code in a sandbox writes in a model's API, which the model route answers
([models](models.md)).

### Model route

A role's entries for the APIs it may write model requests in, with the models that each allows
(`ir::ModelRoute`). Also **route**.

### Now function

A grant's function whose body answers in the call's reply, with no sandbox (`ir::NowFunction`).

### Op

A request that code in a sandbox makes through `sandboxctl`, such as a call. The decision core
authorizes each (`schema::sandbox::op`). **Not** operation.

### Operator

The person who runs `pumpkin` on their machine. **Not** user, which is a role's numeric user
(`ir::User`).

### Outcome

How a run ended: each agent's submission and the final call's verdict (`outcome.json`,
`decision::Outcome`).

### Placing

Choosing for each role the provider and the class that its sandboxes are created in
(`Workflow::place`, [placing a run](providers.md#placing-a-run)).

### Provider

What creates, lists and deletes sandboxes: Docker, EC2 or Modal (`provider::Provider`,
[providers](providers.md)).

### Prune

A controller's removal of images and records that its rules no longer keep (the `retention`
crate).

### Reap

The deletion of what an ended run left on its providers, by its record alone
([stopping runs](running.md#stopping-runs-and-cleaning-up)).

### Record

A run's directory, which its driver writes and no sandbox can reach ([records](record.md)).

### Recipe

A Dockerfile that builds an image, which may fetch only what a digest pins (the `recipe` crate).

### Result

The values that the final call's body returns ([the final call](workflows.md#the-final-call)).

### Role

A template of sandboxes: an image, a user and what each sandbox needs. Its kind is agent, target
or runner (`ir::Role`).

### Run

One execution of a workflow, from its start by `pumpkin run` to its outcome, with its record.

### Runner

A role whose sandboxes sessions lease, for each lease's holder to drive (`ir::RunnerRuntime`).

### Runtime

**Open.** The `runtime` crate, the driver's side of a run. Also a role's `runtime` key, which
gives its kind (`ir::Runtime`).

### Sandbox

One instance of a role: what Pumpkin isolates, places, bounds and deletes. A provider creates it,
and `sandboxd` serves its session (`schema::sandbox`).

### Session

**Open.** The one connection between the gateway and a sandbox's `sandboxd`, its only way out
(`gateway::Session`). Also a grant's kind that leases runners (`ir::Session`).

### Submission

The payload that an agent submits for the final call, which its last `run.set_submission` set.
**Open:** the core's `Action::StartFinalCall` names them `selections`.

### Sweep

The part of a provider that lists and deletes a run's sandboxes by its tag, which is all a reap
needs (`provider::Sweep`).

### Target

A role whose sandboxes run the commands that a call's body sends ([roles](workflows.md#roles)).
**Open:** a controller's `host::Target` is EC2 or Docker, and `config::Target` is the machine or
its controllers.

### Task

A repository's sources, whose entry file evaluates to a workflow and its images
([the language](language.md)).

### Team

The members who share a controller, and its deployment ([`deploy/aws`](../deploy/aws/README.md)).

### Work

What agents do until each one's work ends, at the latest by the work deadline,
`deadline_seconds`. The final call follows it.

### Work target

A target whose `lifetime` is `work`, whose one sandbox serves every call of the work phase
(`ir::TargetLifetime`). **Open:** it is also a shared sandbox, a work-lifetime target and a work
sandbox, and the record's `shared/<role>/`.

### Workflow

The agents, targets and runners of a run, their grants and model routes, and its final call
(`ir::Workflow`, [workflows](workflows.md)).
