<p align="center">
  <img src="docs/figures/logo.svg" width="160" alt="Pumpkin's logo: an open ring with an orange tile, turned 45 degrees, in the break at its top.">
</p>

<h1 align="center">Pumpkin</h1>

<p align="center">
  Pumpkin is an orchestration layer that handles multi-agent multi-sandbox workflows. It provides optimized communication abstractions for easy management and error handling.
</p>

<p align="center">
  <a href="docs/workflows.md">Workflows</a> ·
  <a href="docs/language.md">Language</a> ·
  <a href="docs/running.md">Running</a> ·
  <a href="docs/record.md">Records</a> ·
  <a href="docs/providers.md">Providers</a> ·
  <a href="docs/models.md">Models</a> ·
  <a href="ARCHITECTURE.md">Architecture</a>
</p>


## Why Pumpkin?

Running a multi-agent multi-sandbox flow requires extensive communication and error handling.
Naively setting up such a workflow can result in increased obscure failures and unnecessary context overhead for agents.
We built Pumpkin to provide clean abstractions so people can easily do error handling.

| | Without Pumpkin | With Pumpkin |
| --- | --- | --- |
| Agent | A container you start. It has the network and the mounts you give it. | A sandbox the workflow names. Its only way out is its session with the driver. |
| Model key | In the container's environment, or behind a proxy you run. | On the host. The route adds it to requests the workflow allows. |
| Judge | Another container or script, wired up per task. | A fresh sandbox of the final-call role, after the agent's work has ended. |
| Limits | Timeouts and spend caps in each launcher. | The workflow's deadline, call bounds, and model budget, on every run. |
| Record | The logs you saved. | One directory per run: the workflow, the log, sandbox output, model exchanges, and the result. `wait`, `ls`, and `view` read it. |
| Where it runs | Local Docker, or another launcher for EC2 or Modal. | The same `pumpkin run`, on this machine's Docker, on EC2, or on Modal. |

## How it works

`pumpkin` evaluates a task to its workflow IR. Each run's driver carries out the IR in sandboxes, and writes the record that `wait`, `ls` and `view` read.

![A task's sources go to pumpkin, which evaluates them to the workflow IR and builds the images; the driver, one process for each run, reads the IR, calls the functions' bodies from the sources the evaluation read, and has a provider create a sandbox for each instance of a role; it writes the record, which pumpkin wait, pumpkin ls and the page read.](docs/figures/overview.svg)

Each run's sandboxes come from Docker, EC2 or Modal. Each run's driver runs on this machine, or on a team's controller, which this machine reaches over `ssh`.

![On the operator's machine, pumpkin builds images on the Docker engine, where the Docker provider's containers also run. A machine with a controller sends its runs over ssh to the controller. The controller is an EC2 instance or a privileged container on a Docker host. An EC2 controller builds the images on its own engine and pushes them to ECR. It launches EC2 instances that pull the images, and creates Modal sandboxes, which Modal imports from the same repositories. A Docker controller builds the images on an engine inside its own container, never on the host's. The Docker provider's containers also run on that engine.](docs/figures/providers.svg)

[`ARCHITECTURE.md`](ARCHITECTURE.md) maps the code. [Providers](docs/providers.md) says what each provider and the controller need.

## Quickstart

On Linux x86-64, run `./pumpkin` from the root of the task repository. The `.pumpkinversion` file
beside the launcher selects the release. If that release is missing from your cache, the launcher
downloads it, verifies it and unpacks it there. Every repository that pins the release uses that
install and keeps its own run state in `.pumpkin/`. Docker runs the sandboxes.
For a new task repository, [install the launcher and pin](#installing-or-upgrading-a-task-repository).

One file configures the machine for every task: `~/.config/pumpkin/pumpkin.toml`. A machine whose
runs stay on its own Docker engine needs only this:

```toml
[docker]
context = "default"

[providers.local.docker]
```

Each task is an entry file, `task.star`. The [examples](examples/README.md) run in place from a
Pumpkin checkout, once `cargo xtask build` has built `pumpkin` into its `.pumpkin/bin`. They start
with `examples/hello`, which needs no model. From the checkout's root:

```console
$ .pumpkin/bin/pumpkin check examples/hello/task.star
$ .pumpkin/bin/pumpkin run examples/hello/task.star
20261116-195228-234cd166
$ .pumpkin/bin/pumpkin wait 234c
pumpkin: run 20261116-195228-234cd166 finished with a result (score=6 lines=1), ...
$ .pumpkin/bin/pumpkin ls
20261116-195228-234cd166 finished         2s  -               -  examples/hello: score=6 lines=1
$ .pumpkin/bin/pumpkin view
```

A task repository's `./pumpkin` runs its own tasks with the same commands. `pumpkin --help` lists
them. [Writing workflows](docs/workflows.md) says how a workflow's parts fit.
[Running workflows](docs/running.md) says how the commands fit together.

### Installing or upgrading a task repository

For a first install or an upgrade, the task maintainer chooses a tag from the
[published Pumpkin releases](https://github.com/FrontierCS/Pumpkin/releases).
Replace `<tag>` below with your chosen tag and run the commands from the task repository root on
Linux x86-64. Staging the files is optional. This recipe creates and removes its own temporary
directory (`install_dir`) so a failed download or version check leaves the working pair in place.

```sh
(
    set -o errexit
    tag='<tag>'
    url="https://github.com/FrontierCS/Pumpkin/releases/download/$tag"
    install_dir=$(mktemp --directory "$PWD/.pumpkin-install.XXXXXXXX")
    trap 'rm --recursive --force "$install_dir"' EXIT
    cd "$install_dir"
    curl --fail --location --proto '=https' --output pumpkin "$url/pumpkin"
    curl --fail --location --proto '=https' --output .pumpkinversion "$url/pumpkinversion"
    chmod +x pumpkin
    ./pumpkin --version
    mv --target-directory .. pumpkin .pumpkinversion
)
```

Commit `pumpkin` and `.pumpkinversion` together, along with any workflow changes the release
requires.

## Contributing

[`ARCHITECTURE.md`](ARCHITECTURE.md) is the map of the code. [`docs/dev/`](docs/dev/) holds the
rules for changing it. `cargo xtask ci` runs the checks that every change must pass.
`cargo xtask release --help` says how a release is made.
