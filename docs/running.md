# Running workflows

This page is for the operator: whoever runs workflows with `pumpkin` and holds the machine's
configuration and keys. It says how the commands fit together. `pumpkin --help` lists them, with
the exit statuses, and each command's `--help` gives its arguments, with examples.

## Installing

A task repository runs the release that its `.pumpkinversion` pins, through its launcher,
`./pumpkin`, which the README says how to
[install or upgrade](../README.md#installing-or-upgrading-a-task-repository). The launcher
installs a release into the cache that every repository shares, and names the tools it lacks. To
build a checkout for development, run `cargo xtask build` there and use its `.pumpkin/bin/pumpkin`.

The repository's `.pumpkin/`, which ignores itself in Git, holds its runs in `.pumpkin/runs/`, the
copies of each controller's runs in `.pumpkin/runs/<name>/`, and the state of its builds and
providers. The release cache, the Docker engine's images, a `--runs` directory and the controller's
client keys, in `~/.config/pumpkin/ssh/`, live elsewhere.

## The machine's configuration

`~/.config/pumpkin/pumpkin.toml`, or the file that `--config` names, says what a machine's runs
may use: its Docker engine, the providers that sandboxes are placed on, the upstreams that model
routes reach, with each model's prices, and the files that hold their keys. A machine whose runs
go to controllers names each in `[controllers.<name>]` instead of the engine and the providers.
`pumpkin help config` is the reference, with examples. The [README](../README.md#quickstart) gives
the configuration of a machine whose runs stay on its own engine.

## Checking a task

A task is an entry file below its root, the nearest directory at or above it that holds a
`PUMPKIN.star`. `pumpkin check` evaluates it, checks its images' recipes and its workflow, and
lints the files it read, without reaching a Docker engine or the network. `pumpkin fmt` formats
the files as the lint expects ([the language](language.md#formatting-and-the-lint)).

## Running

`pumpkin run` starts a driver, which checks the task, places each role
([providers](providers.md#placing-a-run)), reaches each provider that it places a role on, builds
the task's images and runs the run. `run` prints the run's id once the run has started, and the
run goes on without it. Whatever ends `run` before then ends the driver too. The driver writes what
it says after the start to the record's `driver.log`. On a machine with a controller, the
controller builds and runs the task, and `run` prints the id at once.

A run's id is the UTC time it was asked for and eight random hexadecimal digits, such as
`20261116-124728-51da8048`. A command that takes a run takes its id, or a prefix of the id or of
its eight digits that no other run shares. A run takes no seed: its sources and its `--arg`s fix
its workflow.

A run outlives a logout unless its engine stops then, as rootless Docker does without
`loginctl enable-linger`, or logind kills the session's processes (`KillUserProcesses`). Then only
a run under the user's manager outlives it: enable lingering, and start `run` under
`systemd-run --user --scope`.

## Following and listing runs

`pumpkin wait` follows a run to its end and says its summary, and `run --wait` starts a run and
follows it. `pumpkin ls` lists the runs that are live or ended within the last day, and
`pumpkin view` serves them to this machine's browser ([records](record.md#the-page)). `wait`,
`ls` and `view` read only the record, so ending one never stops a run.

A live run's phase is `publishing` what its sandboxes run, `starting` its agents, `work`, `ending`
the work phase, or `final call`. Under it, `ls` names each call that hasn't ended, by its
function, and each lease, by its grant and state. A phase past its bound is a bug, which `ls`
says. An ended run's state is `finished`, `failed`, `interrupted` by a signal, or `lost` if its
driver ended without an `outcome.json`. Its task is its workflow's name, or `-` if its record has
no `ir.json` yet.

## Stopping runs and cleaning up

`pumpkin cancel` stops a run and removes what it left, and `pumpkin reap` removes what ended runs
left. SIGTERM, SIGINT or SIGHUP to the driver, whose pid is in the record's `driver.pid`, stops the
run, and the run deletes its sandboxes. A signal that was ignored as `run` started stays ignored.
SIGKILL leaves the run lost with its sandboxes, and a create that the driver gave up on can still
make one after that. Every sandbox also ends by its run's hard end, even if nothing reaps it.

A reap deletes each sandbox that a provider of the run lists under the run's tag, and each that
the record holds undeleted, and notes it in `reaps.jsonl`. It reaches the providers through the
configuration that ran the run: the `--config` that its `record.json` keeps, or else the default.
To find what a run left by hand, look for the containers labelled `pumpkin.run=<id>`, the EC2
instances tagged `pumpkin:run=<id>` and the Modal sandboxes tagged `pumpkin-run=<id>`. On a
controller, `reap` reaps again if the reply is cut, and the ids it prints may then miss runs
cleaned before the cut.
