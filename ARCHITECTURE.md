# Architecture

A map of Pumpkin's code: where each thing lives, and what stays true across it. The rules for
changing the code are in [`AGENTS.md`](AGENTS.md) and the guides it points to in `docs/dev/`.
Names in backticks are for symbol search.

## Bird's-eye view

Pumpkin runs workflows, in which an agent works and judges score what it submits. A workflow is
data, JSON in Pumpkin's IR: its roles, each an agent, a judge or a runner (one agent, its judges,
and the runners that they lease); the grants each role holds, the agent's judge calls, leases and
one submission, and a judge's leases; the model route of each role that has one, the agent or a
judge; and the score, the judge whose verdict ends the run. Each instance of a role is a sandbox,
a judge's afresh for each call and a runner's for each lease, whose supervisor, `sandboxd`, meets
the host, dialing it, or, where its provider exposes it, listening for the host to dial it, and
serves one session: the host puts and collects files, starts and kills commands and freezes the
sandbox, and code in the sandbox makes ops with `sandboxctl`, such as a lease's command or its
transfer of a tree of files. On the host, the gateway holds each session's end, the driver's
decision core decides what a run does, and a provider, such as Docker's, makes sandboxes.
The session is the boundary that matters most: a sandbox may be buggy, and the agent is
opportunistic and goal-directed, using whatever it can reach to finish its task or raise its
score. So each guard either limits what the agent's code can reach, or keeps a buggy sandbox from
harming the host or the run.

## Entry points

The binaries are `pumpkin` (`crates/pumpkin/src/main.rs`), which runs a workflow,
`sandboxd`, also `sandboxctl` (`crates/sandboxd/src/main.rs`), `ec2-boot`
(`providers/ec2/boot/src/main.rs`), which boots an EC2 sandbox's instance, and `cargo xtask`
(`xtask/src/main.rs`). The key types: `ir::Workflow`, a workflow as written; `workflow::Workflow`,
parsed; `Placed`, its roles placed on providers, each in the smallest class of sandbox whose
offers meet its needs (`Workflow::place`); `Core`, a run's decisions; `Session`, the host's end of
a sandbox; `Provider`, a provider a run has connected to; `Engine`, the operator's Docker
engine, on which the images that sandboxes run are built.

## Front ends

The workflow IR (`schema`'s `ir`) is the only input the runtime reads, and it holds no
defaults. Programs that write it are front ends. Pumpkin's own front end is its workflow
language, a subset of Starlark that `lang` evaluates to the IR; any other program may still write
the IR, as a task repository's Rust factories do.

## Code map

Each crate depends only on crates above it, but `test-support`, which only tests use.
**Architecture Invariant** callouts often name absences.

**`crates/stdx`.** What `std` lacks and several crates share, such as `Context` for errors, and
the signals a process notes rather than acts on (`signal`).
- **Architecture Invariant:** it knows nothing of Pumpkin: it depends on no Pumpkin crate.
- **Architecture Invariant:** it holds the workspace's one `unsafe`, the `sigaction` that
  `signal::catch` makes: the workspace denies `unsafe_code`, and that function alone expects it.

**`crates/test-support`.** What the tests of more than one crate share, such as the conformance
suite every provider passes (`suite`), a complete workflow (`WORKFLOW`), and what the tests that
need Docker reach it with; a helper that one crate's tests use stays in that crate's tests.
- **Architecture Invariant:** only tests use it: each crate that does names it under
  `[dev-dependencies]`.

**`crates/tls`.** TLS 1.3 below a session whose connection crosses the Internet, relayed to a
`Stream`, a Unix socket of the session's own: a `Client`, which trusts Web PKI's roots
(`Client::web_pki`), and a `Server`, the stand-in's end in tests.
- **Architecture Invariant:** a session reads and writes its socket as any other: TLS stays below
  it, so the session's protocol and bounds are the same over TLS as without it.

**`crates/schema`.** The formats Pumpkin's programs share, never copied into a consumer:
`ir`, the workflow IR; `workflow`, a workflow parsed, and placed in the classes of sandbox that
providers offer (`Class`, `Offers`); `sandbox`, the sandbox protocol, its `frame`s, `tree`s, ops
(`sandbox::op`), how a sandbox meets the gateway (`SocketDial`, `TcpDial`, `Listening`) and the
in-sandbox contract: the paths, limits and environment a role's commands and its session's owner
share; `ec2_boot`, what the EC2 provider hands `ec2-boot` (`Start`).
Another program's format, such as `docker info`'s, stays with the code that reads it.
**API Boundary:** the IR, the in-sandbox contract, `sandboxctl`'s JSON.
- **Architecture Invariant:** outside its tests, it depends on `serde` and `serde_json` alone,
  because `sandboxd`, a small static binary, depends on it.
- **Architecture Invariant:** only parsing makes a `workflow::Workflow`, and only placing one
  makes a `Placed`, so code past them need not check again what they established.
- **Architecture Invariant:** the IR holds no defaults: an absent field means what its doc says,
  such as no limit; whatever writes the IR picks the defaults.
- **Architecture Invariant:** the agent never calls the judge that scores it: parsing
  (`Parser::score`) refuses a workflow whose score judge is the role of any of the agent's judge
  grants, and a judge holds only lease grants (`Parser::roles`); nor does it run what a judge
  runs: parsing (`Parser::agents_runner`) refuses an agent's lease of a runner whose image is a
  judge's.
- **Architecture Invariant:** a passive sandbox, one whose role makes no ops (`Create::ops`), has
  no `sandboxctl`, so it can make none.

**`crates/recipe`.** The image recipes Pumpkin builds, Dockerfiles: `check` refuses one that may
have Docker fetch what no digest pins, and `check_source` an `ADD` source Docker may take for a
Git repository's URL; `fetch` reads a recipe's fetch stage (`FetchStage`), which pins each file
a step would fetch, and writes the pins into it.
- **Architecture Invariant:** it does no I/O and knows no Pumpkin crate: it depends on none, and
  it is `#![no_std]`, so it reads only the text it is given, and a harness and a provider share
  one check without either depending on the other.

**`crates/engine`.** The operator's Docker engine, on which Pumpkin builds the images that sandboxes
run, whichever provider runs them, and from which it publishes them: `Engine` reaches the engine of
a named Docker context (`ContextName`), reports it (`Report`), and builds images, each tagged by its
build's inputs, from a recipe (`Recipe`) that `recipe::check` passes and a context of directories
(`BuildContext`) that it stages; it writes a recipe's fetch stage from what a resolver prints, run
online in a container of the image that the recipe builds up to that step (`Engine::lock`). For the
providers whose sandboxes pull what they run from a registry, it checks what `Platform` an image is
for (`Engine::check_platform`), loads the image of a `sandboxd` (`Engine::load_sandboxd`) and
pushes, with a `Login` that reaches that push alone (`Pusher::push`); and `confinement` confines a
role's container wherever a provider's sandbox is one.
- **Architecture Invariant:** it knows no provider: of Pumpkin's crates it depends only on
  `recipe`, `schema` and `stdx`, so a crate that only builds or publishes images depends on it,
  and on no provider.
- **Architecture Invariant:** production code reaches the operator's Docker engine only through
  it, whose `docker` runs against the named context alone, so the operator's choice of engine
  holds; the boot of an EC2 instance, its entry and `ec2-boot`, runs the `docker` of that
  instance. Tests run `docker` directly only to observe containers and images, to create, run,
  pause and remove containers of their own, or to build, tag and remove images of their own.
- **Architecture Invariant:** what the engine fetches for a build is pinned: an image by digest,
  a file by `ADD --checksum`, or a Git tree by its full commit (`recipe::check`). What a step's
  own command fetches is outside this; a recipe may take it from a fetch stage instead, which
  pins each file (`recipe::fetch`). A base's `ONBUILD` triggers, which the check doesn't read,
  are outside it too. A lock's resolver (`Engine::lock`) runs online, outside any build or run,
  and only writes pins into a recipe.

**`crates/gateway`.** The host's end of sessions: where a sandbox meets the gateway (`meeting`),
a `Registration` that its run's `Sockets` or `GrantListener` issues, or its run's `Dialer` makes
of the endpoint the sandbox's create gives, which it dials over TLS (`tls`), makes the sandbox's
`Session`, whose one owner moves trees, runs commands (`Process`) and answers ops, hearing of each
op, each model request, each command's exit and the session's end as an `Arrival` sent to it, and
through whose `Trees` another thread moves trees beside the owner, as a lease's does between its
holder and its runner.
- **Architecture Invariant:** a sandbox can't harm the host through its session: anything out of
  protocol ends the session, and a session costs the host only bounded memory and bounded waits,
  as does a peer yet to say which sandbox it is. So a session accepted with no op payload limit
  (`Registration::accept`) ends at its first op (`Assembler::open`). The gateway makes each
  meeting, the private directory of a run's sockets included, so no caller can make one that
  breaks this, given a parent where no other user may rename that directory (`Sockets::create`).
- **Architecture Invariant:** it knows no provider and no driver, so every provider's sandboxes
  have the same sessions: of Pumpkin's crates, it depends only on `schema`, `stdx` and `tls`.

**`crates/sandboxd`.** The supervisor in every sandbox, built static by `cargo xtask sandboxd`:
`session` serves the gateway (`files`, `commands`), `ops` carries callers' ops, `model` serves
the model endpoint, `ctl` is `sandboxctl`, and `sys` holds the system calls `std` lacks.
- **Architecture Invariant:** the protocol's two ends share no Pumpkin crate but `schema`
  and `stdx`, so the protocol is all they agree on: `sandboxd` uses the gateway only in tests.
- **Architecture Invariant:** it has only its role's privileges, from the start, or, started as
  root, from before it serves anything (`--user`), so it is no boundary within its sandbox:
  where the role runs the agent's code, that code can shape what `sandboxd` reports.

**`crates/provider`.** `Provider`, a run's one handle on a provider for every side effect
on it: it reserves what the run's roles need (`Reservation`), publishes what the run's
sandboxes run, creates, lists and deletes them, and reads their consoles; `Sweep`, the part
of it that lists and deletes, which a reap needs alone; and the values they take and give, such
as `Create`. How a provider's sandboxes meet the gateway is its `Meeting`, each way of which
holds the create, a `Creator`, that takes what that way gives a sandbox: a socket's dial, a TCP
address's with a grant, or, for one that listens, a grant, the create then giving its
`Endpoint`. A provider is `providers/<name>`, crate `<name>`, which holds its platform's calls,
but those that a crate of its own holds for every provider that makes them, as `aws` does AWS's,
and `engine` the operator's Docker engine's.
- **Architecture Invariant:** it knows no provider: of Pumpkin's crates it depends only on
  `schema`.
- **Architecture Invariant:** every call on sandboxes names its run and keeps to that run's, by
  its tag, so runs that share a provider never reach each other's sandboxes.
- **Architecture Invariant:** a provider holds no session, and the run's times reach it as data: its
  sandboxes meet the gateway where the runtime gives them, dialing it, or listening at the endpoint
  the create returns, which the gateway dials, and a provider bounds only its own platform calls,
  so no provider depends on `gateway` outside its tests.
- **Architecture Invariant:** a create gets only what its provider's `Meeting` takes, and gives an
  endpoint only where its sandboxes listen, so no provider and no run handles a dial or an
  endpoint of another way.
- **Architecture Invariant:** no provider depends on another's crate: what two providers share is
  in a crate of its own, as `engine` and `aws` are.
- **Architecture Invariant:** every provider starts `sandboxd` first in each sandbox, or, where
  its platform puts a process of its own first, as Modal does, as the one command of that
  process, as root with `--user`, so that a freeze leaves none of the role's processes; and
  presents `/pumpkin` with the directories `sandbox` says every sandbox has, so a role's commands
  and the session's owner find them anywhere.
- **Architecture Invariant:** a sandbox that no start claims ends itself, so one that a provider
  makes after the run gave up its create runs nothing: a `sandboxd` that dials finds its
  registration gone with the start that issued it, and one that listens gives up a minute after
  its start's deadline, which its provider passes it (`sandboxd --by`).

**`providers/docker`.** `docker`, a sandbox as a container that needs no root to start, on the
operator's engine: `Docker`, made from an `Engine` and the CPUs it may give, is the `Provider`
that makes each sandbox a container under the engine's `confinement`, on CPUs of its run's own
(`cpus`), once it has checked that the engine enforces each of a container's limits
(`Docker::connect`). For a reap, it reaches the engine without checking that it is fit to run on
(`Docker::reach`), as `Reached`, which is only a `Sweep`.
- **Architecture Invariant:** a container's only way out is its session's socket: it has no
  network but loopback, and the role's files move over the session, never through the daemon.
- **Architecture Invariant:** a run's containers run on CPUs that no other run's are given, from
  its reservation to its end: the machine's ledger (`cpus`) holds each run's, locked by the
  process that holds the reservation, so no process keeps it, and a reserve reads every run's
  and writes only its own.

**`crates/aws`.** AWS's command line as a provider runs it, `Cli`, in one `Region`, and ECR's
repositories on it: each described by its name (`Cli::repositories`), and an image pushed to one
from the operator's Docker engine, through `engine`'s `Pusher`, named there by its digest
(`Cli::publish`), so that every provider whose sandboxes pull from ECR pushes alike, and none
depends on another's crate; and the IAM roles that providers' settings name (`RoleArn`).
- **Architecture Invariant:** production code reaches AWS only through `Cli`, but for the login to
  ECR that an EC2 sandbox's entry makes with its instance's own `aws`; and every call `Cli` makes
  is in its region, with no variable of this process's environment but those that find `aws`'s
  own configuration.

**`providers/ec2`.** `ec2`, a sandbox as a container on an EC2 instance of its own: `Ec2`
reaches the envelope the operator made on AWS, which its `Settings` name, reports it, and is the
`Provider` that pushes what its sandboxes pull from the operator's Docker engine to the envelope's
repositories (`aws`), which they pull it from by digest, and launches each sandbox's instance,
whose boot runs the role's container under the engine's confinement: for those two it depends
on `engine`. For a reap, it reaches the envelope only as far as AWS answering to the
credentials (`Ec2::reach`), as `Reached`, which is only a `Sweep`.
- **Architecture Invariant:** `ec2` reaches AWS only through `aws`'s `Cli`, in the settings'
  region, but for the login to ECR that its instances' entry makes.
- **Architecture Invariant:** a container's only way out is the gateway's address and port: its
  instance drops whatever else it sends, to the instance itself and its metadata included.

**`providers/ec2/boot`.** `ec2-boot`, the boot of each EC2 sandbox's instance, a static binary
that `sandboxd`'s image holds beside it (`Engine::load_sandboxd`), which the instance's user data,
a thin shell entry, copies out and runs with the `Start` that the provider wrote: it locks the
instance down, checks its Docker, pulls the role's image, and runs the role's container.
- **Architecture Invariant:** of Pumpkin's crates it depends on `schema` and `stdx` alone, so it
  is small and static, and the provider's values reach it only as a `Start`.

**`providers/modal`.** `modal`, a sandbox as a sandbox in a Modal workspace: `Modal` reaches the
workspace that a profile of Modal's configuration reaches, and the ECR repository that its
`Settings` name, reports both, and is the `Provider` that publishes a role's image as Modal
imports it from there by its digest, with an AWS role that Modal assumes by its own identity, once
it has pushed the image there from the operator's Docker engine (`aws`), for which it depends on
`engine`, and creates each sandbox, tagged with its run, in Pumpkin's one App, which it never
stops, with `sandboxd` attached from the operator's machine, listening behind Modal's TLS tunnel;
for a reap, it reaches the workspace alone (`Modal::reach`), as `Reached`, which is only a
`Sweep`. It reaches Modal through its helper, `pumpkin-modal.py`, a Python program on Modal's SDK
that `uv` runs once for each call, with the packages its lock pins.
- **Architecture Invariant:** `modal` reaches Modal only through its helper, which gets, of
  this process's environment, `HOME` and `PATH` alone, with the settings' profile and
  configuration file, and AWS only through `aws`'s `Cli`, in the settings' region.
- **Architecture Invariant:** a sandbox opens no connection, but DNS to Modal's resolver, and its
  only way in is its tunnel, which takes connections from the settings' `dials_from` address
  alone.

**`crates/decision`.** The driver's decision core: `Core` takes each `Event` a runtime
observes and returns the `Action`s the runtime is to perform, until one ends the run, with its
`Outcome`, or abandons it at its hard end; its `ops` rules on ops, `jobs` meters judge calls,
`leases` meters and ends leases, and `result` reads a call's `result.json` (`JudgeResult`).
- **Architecture Invariant:** the decision core does no I/O, reads no clock and uses no randomly
  seeded hash: time reaches it with each event, so the same events give the same actions, and it
  decides when time ends the work phase, a lease or the run, the hard end included. The
  crate is `#![no_std]`, so it can name none of `std`'s files, network, processes, environment,
  threads, clock or hashed collections, and the workspace denies `unsafe`, which `core::arch`'s
  clock needs. It depends on `schema` alone of Pumpkin's crates, through which the schema's I/O
  items (`tree::Writer`, `frame` over `std::io`, and `PathBuf` paths) remain reachable.
- **Architecture Invariant:** only the decision core authorizes ops: `sandboxd` carries any op any
  code in the sandbox makes, and the gateway bounds only what ops cost the host.
- **Architecture Invariant:** the agent learns of a judge call only its state, what its grant has
  left, and the fields its judge declares (`ResultSpec`), so the workflow decides its feedback.

**`crates/route`.** The model route: `Route` closes the model requests that the sandbox of a role
with a route makes over its session, an entry's for each API, and `Upstream` forwards a closed one
to the model provider and says how it answered (`Answer`), as `serve` does for each request;
`Serving` is the upstream that serves an entry of a run's route, its endpoint and key, of which
the run makes an `Upstream` for each sandbox it serves, with the model names it takes. The crate
holds the route's rules and all that reaches the model provider, so the model provider's TLS
client (`ureq`, with `rustls` and `ring`) is in this crate alone.
- **Architecture Invariant:** the route stays closed: a request reaches the models its workflow
  allows and nothing else of the model provider's, so a response carries nothing but what its own
  request sent. The rules of each API are the `closure` module's.
- **Architecture Invariant:** the route knows no run: of Pumpkin's crates it depends on
  `gateway`, `schema` and `stdx` only, and the runtime depends on it, never the
  reverse; what the run does with an answer, its health and fault rule, is the runtime's.

**`crates/runtime`.** The host side: the driver's runtime, whose `Run` prepares each run, then
drives the decision core on the run's `Provider`s, each of whose sandboxes meets the gateway where
its provider's `Meeting` says, timed by its `Host`, whose clock makes the inboxes the run's threads
wait on (`System`, the machine's), keeping the run's record, whose files `record` lays out, and
its run log, whose records `log` does, each synced through the host. A judge call or a lease is a
child of the run (`child`), on a thread of its own, which `judge` or `lease` drives. `model`
serves the model requests of each sandbox whose role has a route through `route`'s `Route` and an
`Upstream` of the sandbox's own, and judges the model provider's answers: the agent's until the
work phase ends, a judge call's while its command runs (`model_requests_allowed`), or until the
provider refuses an entry's key, which fails the run as the deployment's fault, but for one heard
after the agent's final submission locked the selection, on a route that served before it.
`Record` also reads a record, for `pumpkin`: a live run's phase and bounds (`Progress`), the
arguments that started it (`Record::args`), and each model request it kept
(`Record::model_request`).
- **Architecture Invariant:** the final judge starts only once the agent's sandbox, its jobs, and
  every lease it or they held, are gone, so nothing the agent started runs while its submission
  is judged; and a run ends only once the final judge's leases are gone too.
- **Architecture Invariant:** the record keeps every model exchange of every caller as the route
  saw it pass, which no sandbox can alter, written as it passes (`route::Keep`,
  `model::Exchange`), in `model/` of the record, numbered as the run log numbers requests: the
  request's body as it went upstream, or as its client sent it if the route refused it, each item
  of a conversation the route read kept once, by its digest, from which `Record::model_request`
  rebuilds the body byte for byte; and the answer's body as it came back from the upstream, a
  stream's as its events, even where the route answered its client with a refusal instead, or
  else the route's refusal. The deployment's key, a header, is never written.
- **Architecture Invariant:** a judge's evidence (`evidence_dir`) goes to the run's record and
  nowhere else: no op reply carries a file collected from a judge's sandbox, so only the operator
  sees it; a lease's status carries the ends of its own commands' output to its holder alone, and
  a lease's transfer moves trees between its holder's sandbox and its runner alone, through the
  host, which holds each on its disk only while it moves it.
- **Architecture Invariant:** a run's record is made only once each provider that it places a
  role on holds what its roles need at once (`Provider::reserve`), so a run that doesn't fit is
  refused with no record; the run holds it until its end has deleted its sandboxes.
- **Architecture Invariant:** a record appears in the runs directory only once its `driver.pid`
  is locked (`Lock`), and the lock is held until the run's last write, so a record is ended, live
  or lost, by `outcome.json` and then the lock, and a record without `driver.pid` is not one.
  Only the lock's holder writes the record, and once it has let go only `Record::reap` does,
  which records a lost run as failed, and appends each sandbox it removed, or found gone, to
  `reaps.jsonl`, so that a reap is auditable from the record.
- **Architecture Invariant:** a run takes its workflow placed (`Placed`), and makes from it the
  providers it runs on (`ByProvider`), one for each provider that it places a role on, so each
  role's sandboxes are created on its provider, in the class of sandbox it is placed in, which it
  fits, and each is given as long to start as that class takes.
- **Architecture Invariant:** the runtime is written once, against `Provider`: it depends on no
  provider's crate, not even in its tests, which run on the simulated provider (`Sim`), so a new
  provider changes no runtime code.
- **Architecture Invariant:** every sandbox starts on the runtime's one start path, `sandbox`, as
  the runtime calls a run's providers, but for what each declared (`Provider::ready`), only
  there: through `Sandboxes` for a run, and for a reap through `Record::reap`, which only lists
  and deletes (`Sweep`); and a run's end deletes whatever each of its providers still lists of the
  run, so a run ends with none of its sandboxes left, or fails, but for one that a create that
  gave up makes only after that list (`Creator::create`), which `Record::reap` deletes.

**`crates/harness`.** The harnesses agents run, such as Codex (`codex`), each pinned by a
`Pin`, whose recipe adds the harness to a task's agent image in a layer of its own, below the
directory the in-sandbox contract reserves for it, and each with an adapter, such as
`codex::runtime`, that writes the agent's runtime that runs it.
- **Architecture Invariant:** it knows no task and no provider: of Pumpkin's crates, outside its
  tests it depends only on `schema` and `recipe`, and its Docker test also uses `test-support`. A
  harness therefore reaches an agent alike whichever factory builds its image, and any factory,
  in this repository or outside it, may use it.
- **Architecture Invariant:** what an adapter writes depends on its arguments alone: the crate is
  `#![no_std]`, and the workspace denies `unsafe`, so it can name no file, network, process,
  environment, thread or clock, though `schema`'s I/O items stay reachable through it.

**`crates/lang`.** Pumpkin's workflow language, a subset of Starlark: `evaluate` runs an entry
file, and the files it loads, given a run's `Args`, to a `Plan` of the images it declares and of
the workflow that names them, which its caller checks and builds, or to its `Problems`, each at
its file, line and column, as a linter's findings in the files it read are too (`Plan::lint`).
- **Architecture Invariant:** evaluation is hermetic, so the same sources and args always give
  the same plan: it reads source only through its `Loader`, by `Label`s, which stay below the
  root, it only declares the images its caller builds, and the crate is `#![no_std]`, so its own
  code can name no file, clock, environment, process or randomly seeded hash. Of Pumpkin's
  crates it depends on `schema`, whose I/O items stay reachable through it, as they do through
  the decision core, on `recipe`, and on `harness`, whose adapter writes the runtime its
  `codex()` gives from its arguments alone, as `harness`'s invariant says.
- **Architecture Invariant:** no crate but `pumpkin` depends on it, so the runtime, the decision
  core and the providers never see the language.

**`crates/pumpkin`.** `pumpkin`, the operator's command line, which `cli` parses. `check`
evaluates a task's source with `lang`, from the files below the root of its repository (`star`'s
`Root`), and checks its images' recipes and its workflow, reaching no engine and reading no
configuration, then lints the files it read (`Plan::lint`) with the formatter beside `pumpkin`
(`style`), with which `fmt` formats `.star` files. `build` takes the same path, then builds the
images on the engine of the configuration's Docker context (`Engine::start`) and prints the IR
that names them; `lock` takes it to a recipe's fetch stage, which it writes (`Engine::lock`).
`run` starts the run of a task in a driver process (`driver`), which it executes as itself, and
returns once the run has started: the driver connects each provider that the configuration
names, as its kind says (`Connected`), then builds the task's images as `build` does, places the
workflow's roles in the classes that they offer before the record is begun, and hands `Run` each
provider that a role is placed on, with the static `sandboxd` it has found (`Sandboxd`), the
`Dialer` for the sandboxes that listen, and, for a run that places a role on a private provider,
the `GrantListener` it binds where that provider's sandboxes dial (`listen`). `wait` follows a run's
record (`Record`) to its end, `ls` lists the records (`progress`), `cancel` stops a run, and
`cancel` and `reap` remove what a run that has ended left (`Record::reap`), on each provider that
its record names (`Reaching`); what a command says of an ending or a refusal ends with the
commands to run next (`next`). It reads the machine's configuration (`config`, TOML): the Docker
context; the providers, each by name and kind, `docker`, `ec2` or `modal`; the gateway's address
for each reach over a network; and the model upstreams that a workflow's model route may reach,
of which it hands `Run` the one that serves each entry of a route (`Serving`).
- **Architecture Invariant:** only `style` runs the formatter, buildifier, and Pumpkin's interface
  holds none of its flags or forms: each file's text goes to it, and `style` reads what it gives
  back, so another formatter can take its place.
- **Architecture Invariant:** what a task's source names stays below its root on disk, as its
  labels keep it there in name: `Root`, the one `Loader`, reaches each file and directory that
  an evaluation or its images name from the root, and refuses a symbolic link on the way.
- **Architecture Invariant:** a machine runs its roles either on its own engine or on other
  providers: a configuration that names a provider of kind `docker` names no other
  (`Providers::new`), so the class a role is placed in never turns on this machine's memory.
- **Architecture Invariant:** all that Pumpkin writes on the operator's machine is below the
  workspace's `.pumpkin` directory, the one that holds the `pumpkin` that runs (`Workspace`): the
  runs' records, unless `--runs` names another directory; and a directory of each provider kind's
  own, which it hands the provider as it connects (`Engine::connect`, `Ec2::connect`,
  `Modal::connect`), as no provider writes on this machine but there. Outside it are only what it
  reads, the machine's configuration and each tool's credentials, in `~/.aws`, `~/.modal.toml` and
  Docker's configuration, with what those tools keep there of their own, as the AWS command line's
  cache and BuildKit's token seed; the Docker engine's images; the files that `fmt` and `lock`
  rewrite, which their command lines name; and the engine's ledger of CPUs (`cpus`), which every
  run on the engine shares, whatever its workspace.
- **Architecture Invariant:** each `pumpkin` process adds 022 to its umask first (`main`), so
  nothing it makes, a run's record included, is writable by another user, whatever the
  operator's umask.

**`xtask`.** `cargo xtask ci`, the checks every change passes (its checks of Rust files in `tidy`,
the check that the repository's `.star` files are formatted in `star`, and with `--docker` the
tests that need Docker), `cargo xtask fmt`, which formats the workspace with the pinned nightly
rustfmt, `cargo xtask sandboxd`, `cargo xtask install`, which installs `pumpkin` beside a static
`sandboxd` and `ec2-boot`, the Modal provider's helper and the formatter, `cargo xtask mutants`,
which runs cargo-mutants on the working tree's changes (`diff`), `cargo xtask refill`, which
rewrites a package's snapshots, and `cargo xtask install-tools`, which installs the contributor
tools that `tools` pins. The formatter is buildifier, pinned by its release's SHA-256, which `star`
fetches once into the target directory. A check is a step of `ci`.

## Cross-cutting concerns

Errors follow the rules in [`docs/dev/style.md`](docs/dev/style.md); panics, `unsafe` and
concurrency follow those in [`AGENTS.md`](AGENTS.md).
