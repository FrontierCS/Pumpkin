# Architecture

This is a map of Pumpkin's code: what each part is for, and the few facts that hold across parts.
It holds what changes slowly. A module's doc says how the module works, and a type, a check or a
test holds what the code relies on. The rules for changing the code are in [`AGENTS.md`](AGENTS.md)
and `docs/dev/`. Names in backticks are for symbol search.

## Bird's-eye view

Pumpkin runs workflows in which agents work on a task, and a final call judges what they submit.
The agent's code is untrusted: it may be buggy, and it uses whatever it can reach to finish its
task or raise its result. Pumpkin gives each part of a run exactly what the workflow grants it,
bounds what a run spends in time and money, and keeps a record of the run that no sandbox can
alter.

A workflow is data, the workflow IR. It declares the agent roles, the targets that calls run in,
the runners that leases hold, each role's grants and model route, and the final call. A task's
source, in a subset of Starlark, evaluates to that IR and to the images its roles run. The runtime
reads only the IR, and the driver hands it the bodies of the IR's functions, so the runtime depends
on no language.

Each instance of a role is a sandbox, which a provider makes: a container on the operator's Docker
engine, a container on an EC2 instance of its own, or a Modal sandbox. A sandbox holds the
container `main` and, for a runner, its companions. In each container, `sandboxd` serves one
session, its only way to the host. Over it the host moves files and runs commands, and code in the
sandbox makes ops, such as a call of a function, and model requests. On the host, the gateway holds
each session's end, the decision core decides what the run does next, and the runtime carries out
those decisions on the providers and keeps the run's record. The model route sends each model
request that it admits to the model provider, with the provider's key.

A run is one process, the driver, on one machine: the operator's own, or a team's controller, an
EC2 instance or a container on a Docker host, which runs the runs a machine sends it.

![The programs and machines of a run: the operator's pumpkin starts the driver on the operator's machine or on the controller; the driver's parts, the sandboxes that the providers create and the model provider that the route reaches.](docs/figures/run-machines.svg)

## Entry points

`pumpkin` (`crates/pumpkin`) is the operator's command line, and its `main.rs` dispatches the
commands. `run` builds a task's images and starts its driver. Read its path through the driver
first. A machine whose configuration names a controller sends each run over `ssh` to the same
executable there, whose `controller serve` starts it as a job. Under the name `pumpkin-admin`, the
executable installs and checks the controller's host.

`sandboxd` (`crates/sandboxd`) is the supervisor in every sandbox, a static binary. Under the name
`sandboxctl`, it is the shim through which code in the sandbox makes ops. `ec2-boot`
(`providers/ec2/boot`) boots each instance of the EC2 provider. `cargo xtask` (`xtask`) holds the
contributor tasks.

The key types are `ir::Workflow`, a workflow as written; `workflow::Workflow`, parsed, and
`Placed`, its roles placed in the classes that providers offer; `Core`, a run's decisions, with its
`Event`s and `Action`s; `Run`, a run on its `Host`; `Provider`; `Session`, the host's end of a
sandbox; `Engine`, the operator's Docker engine; and `Record`, a run's record.

## Code map

The figure shows how the crates depend on one another, as `cargo xtask figures` draws it. Pay
attention to the **Architecture Invariant** notes: they often name something deliberately absent.

![How the workspace's crates depend on one another: a box for each crate, above the crates it depends on, with an arrow to each that no other of its arrows already leads to; the crates that build a binary are shaded.](docs/figures/crates.svg)

### `crates/stdx`

What `std` lacks and several crates share, such as errors with context (`Context`), deadlines
(`Deadline`), a clock that a test can script (`time::Clock`) and child processes (`process`).

**Architecture Invariant:** it knows nothing of Pumpkin.

**Architecture Invariant:** a child process gets only the environment that its caller states, and
`process::run` ends it at a bound that its caller states. Clippy's configuration bans std's ways
around `process`.

### `crates/schema`

The formats that Pumpkin's programs share: the workflow IR (`ir`), a workflow parsed and placed
(`workflow`), the frames of its protocols (`frame`), the sandbox protocol (`sandbox`), and what the
EC2 provider hands `ec2-boot` (`ec2_boot`).

**Architecture Invariant:** it depends on `serde` and `serde_json` alone, as `sandboxd` and
`ec2-boot`, small static binaries, depend on it.

### `crates/decision`

The decision core. `Core` takes each `Event` that the runtime observed and returns the `Action`s to
perform, until one ends the run.

**Architecture Invariant:** the core does no I/O, reads no clock and draws no randomness, so the
same events give the same actions. Time reaches it only with each event.

**Architecture Invariant:** only the core authorizes ops. `sandboxd` and the gateway carry any op
that code in a sandbox makes, and the runtime names its caller by the session it came on.

**Architecture Invariant:** the final call starts only once every agent, call and lease has ended
and the deletion of every work sandbox that the run knows of is confirmed.

### `crates/lang`, `crates/library`, `crates/recipe`

`lang` is the workflow language. `evaluate` runs a task's entry file to a `Plan` of its images and
workflow, or to its `Problems`, and `call` calls a body during a run. `library` holds the standard
library's files, which `pumpkin` serves below `@pumpkin//`. A harness is a directory there.
`recipe` checks image recipes, Dockerfiles, for a fetch that no digest pins.

**Architecture Invariant:** evaluation is hermetic. It reads source only through its `Loader`, by
`Label`s that stay below their root. It only declares images, and reaches a host only through the
handle that `call` gives a body. It ends within its `Bounds`.

**Architecture Invariant:** only `pumpkin` depends on `lang`, and `library`'s tests, so the
runtime, the core and the providers never see the language. Only `library` knows a harness.

### `crates/gateway`, `crates/tls`, `crates/sandboxd`

The two ends of a session. The `gateway` is the host's end: the `Session`'s one owner hears of each
op, model request and exit as an `Arrival`. `sandboxd` serves the sandbox's end and freezes it.
`tls` carries a session that crosses the Internet.

**Architecture Invariant:** a sandbox can't harm the host through its session. Anything out of
protocol ends the session, and a session costs the host only bounded memory and bounded waits.

**Architecture Invariant:** the two ends share no Pumpkin crate but `schema`, `stdx` and `tls`, so
the protocol is all they agree on. `tls` carries bytes below the protocol and knows none of its
messages. The gateway knows no provider, so every provider's sandboxes have the same sessions.

**Architecture Invariant:** `sandboxd` is no boundary within its container. It has its role's
privileges, so what it does the role's own code could do too. The session is the boundary. Neither
is the relay in `main`: a companion's own TLS is the boundary.

### `crates/route`

The model route. `serve` sends each model request that a `Route` admits upstream (`Upstream`), and
streams the response back.

**Architecture Invariant:** the route stays closed. A request reaches the models that its workflow
allows and nothing else of the model provider's: none of the state it keeps, the tools it runs or
the URLs it fetches. Only the body that a `Route` admitted goes upstream, with the headers of its
API and those its entry lists.

**Architecture Invariant:** it holds all that reaches a model provider, and it knows no run.

### `crates/provider`, `crates/engine`, `crates/aws`

`provider` is the interface that every provider implements. `Provider` reserves what a run's roles
need, publishes their images, says how the sandboxes meet the gateway (`Meeting`), and creates
them. `Sweep`, its part that lists and deletes a run's sandboxes, is all a reap needs. `engine` is
the operator's Docker engine, which builds every image, whichever provider runs it. `aws` is AWS's
command line as Pumpkin runs it (`Cli`).

**Architecture Invariant:** runs that share a provider never reach each other's sandboxes. Every
sandbox carries its run's `RunTag`, and a provider lists and deletes only the sandboxes that carry
the tag it is given.

**Architecture Invariant:** a provider's call rests only on what it is given and what the platform
holds, never on what an earlier call left in the instance, so a reap works from a record alone.

**Architecture Invariant:** no provider depends on another's crate, or on `gateway` outside its
tests. What two providers share is a crate of its own, as `engine` and `aws` are.

**Architecture Invariant:** a principal's IAM policy is what the calls of the shipped binaries
state. `Cli` runs only a `Call`, which names what IAM must allow of it, and `pumpkin-admin policy`
prints each principal's policy from them.

### `providers/docker`, `providers/ec2`, `providers/modal`

`Docker` runs each sandbox as a container on the operator's engine, on whole cores of its run's own.
`Ec2` runs each as a container on an EC2 instance of its own, in the envelope that `deploy/aws`
makes, and `ec2-boot` locks the instance's network down before the container starts. `Modal` runs
each as a sandbox in a Modal workspace, under gVisor, through its helper, `pumpkin-modal.py`.

### `crates/runtime`

The host side. `Run::prepare` reserves on each provider and begins the record, and `Run::execute`
drives the `Core` on the run's providers to its outcome, or until a signal stops it. One thread
drives the core, and each call and lease is a child on a thread of its own. A start meets each
companion on one more. `record` and `log` lay out the record, and `Record` reads it back.

**Architecture Invariant:** the runtime is written once, against `Provider`. It depends on no
provider's crate, not even in its tests, which run on a simulated provider.

**Architecture Invariant:** every sandbox starts on one path (`Sandboxes::start`), and a run's end
deletes whatever its providers still list of it, so a run ends with none of its sandboxes left, or
fails.

**Architecture Invariant:** every deadline of a run is a time on its `Host`'s clock, so a
simulated host decides every outcome that time decides.

### `crates/pumpkin`, `crates/install`, `crates/members`, `crates/retention`

`pumpkin` is the command line and the controller's side. `controller` is a machine's end of its
controller, and `serve` the controller's end. `job` runs the controller's jobs, and `host` is what
`pumpkin-admin install` installs. `install` names the files of an install, `members` parses a
team's list of members' keys, and `retention` decides what the controller's prune removes.

**Architecture Invariant:** `install`, `members` and `retention` do no I/O, so their tests pin each
rule on values.

**Architecture Invariant:** a record is read only through `runtime`'s readers. `view` writes none,
and says of a run what `ls` says.

**Architecture Invariant:** each key that the controller admits runs only the forced command that
names its member, and `serve` acts for that member. This prevents accidents, not members: each
member's build runs as the controller's one user.

### `crates/test-support`, `xtask`, `dist`, `deploy`, `examples`

`test-support` holds what the tests of several crates share, such as the conformance suite of the
`Provider` trait. Only tests use it. `xtask` holds the contributor tasks, such as `ci` and `live`.
`dist` holds the files that a release ships as they are, such as `./pumpkin`, a task repository's
launcher. `deploy/aws` is the Terraform module that makes a team's Pumpkin on AWS, as
[its README](deploy/aws/README.md) says. `deploy/docker` makes a team's controller in a privileged
container on a Docker host, as [its README](deploy/docker/README.md) says. Both install a
controller by `deploy/controller`'s one script. `examples` holds tasks.

**Architecture Invariant:** a Docker controller's jobs reach the engine inside its container alone,
never the host's, so the paths that the Docker provider mounts, its ledger of CPUs and the prune of
images mean there what they mean on an EC2 controller.

## Cross-cutting concerns

### Security model

The agent's code, and whatever a target or a runner runs, is untrusted, and so is every sandbox.
Each guard either limits what that code can reach, or keeps a sandbox from harming the host or
the run.

A sandbox reaches the host only over its session, and nothing else of the network: a Docker sandbox
has loopback alone, an EC2 sandbox's instance drops all but the gateway's address and port, and a
Modal sandbox's containers open no connection but to one another and DNS. Nothing separates them,
so a task authenticates its own services. Through the session, ops go to the core, which authorizes
each against the caller's grants, and model requests go through the closed route. The agent never
reaches the final call's target.

A sandbox proves which it is by how it meets the gateway. On the host, it dials a socket that only
it can reach. On a private network, its hello presents an `Attestation` that its provider checks,
such as an EC2 instance's identity document. Across the Internet, the gateway dials the endpoint
that the sandbox's create gave, and answers the hello with a `RegistrationGrant` that only that
`sandboxd` was given. A companion meets the gateway through a relay in `main`, under TLS that ends
in the companion, with a certificate that the gateway issued for it alone.

No credential is reachable from a sandbox. The model provider's key stays in the host's `Upstream`,
and the record keeps no header's value. Images are private: an EC2 instance pulls them with its
instance profile, and Modal with a role that it assumes by its own identity. The driver reads each
of the operator's key files itself, so no such key crosses argv or the environment. A machine sends
its keys to its controller only on the connection's standard input.

### Deadlines and spending bounds

Every wait of a run is bounded. A run's bounds are the workflow's, such as the work deadline and
each grant's calls, seconds and bytes. The core meters each, and `lasts` gives the run's hard end,
by which the core abandons the run if nothing has ended it. The runtime meters each route's model
budgets. Every sandbox ends by its run's hard end even if its driver dies. A run's spend is in its
record.

### The run record

`record` lays out each run's own directory. The run log (`log.jsonl`) holds each event that the
core decided on, with its actions, and what the runtime saw of the run's sandboxes, so a run can be
read back event by event. The log only grows, a whole line at a time. `ir.json`, `sources/` and
`provider.json` are written before it and never again, so a live run's reader reads only what the
log gained.

A record appears only once its driver holds its lock, which it holds until the run's last write.
Only the lock's holder writes the record. Once it has let go, only `Record::reap` writes it, and
only `Record::remove` removes it. The record keeps every model exchange as the route saw it pass,
which no sandbox can alter. `ls`, `wait` and `view` read only the record, so a machine shows a run
the same way whatever ran it.

A run whose driver died is lost. A reap deletes, through `Sweep` alone, what the run left on each
provider that its record names, and records the lost run as failed.

### Determinism and pinning

The core and the language read no clock and draw no randomness, and the clippy configuration bans
hash maps whose order follows a seed. What Pumpkin builds or runs is pinned: an image's tag is the
digest of its build's inputs, a recipe's instructions fetch only what a digest pins, and each
tool and release that Pumpkin fetches is pinned by its checksum.

### Interfaces

Task authors rely on the IR, the paths and limits inside a sandbox, and `sandboxctl`'s JSON, which
`schema` holds. Other builds of Pumpkin rely on the record, whose `record_version` a reader refuses
if it isn't its own, and on the controller's settings, of which a job takes only its own envelope
major. A task repository runs the release that its `.pumpkinversion` pins.

### Errors

Errors follow [`docs/dev/style.md`](docs/dev/style.md). What `pumpkin` says of a run's end or of a
refusal ends with the commands to run next (`next`).

### Testing

[`docs/dev/testing.md`](docs/dev/testing.md) holds the rules. `cargo xtask ci` reaches no Docker
engine, network or cloud, so it tests the core, the gateway, the route and the runtime with
Pumpkin's own processes, loopback sockets and a simulated host and provider. `cargo xtask live`
tests the real providers.
