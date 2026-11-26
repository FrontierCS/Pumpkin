# Running workflows

This page is for the operator: whoever runs workflows with `pumpkin` and holds the machine's
configuration and keys. It says how the commands fit together. `pumpkin --help` lists the
commands and the exit statuses. Each command's `--help` gives its arguments, with examples.

## Installing

A task repository's launcher, `./pumpkin`, runs the release that the repository's
`.pumpkinversion` pins. The README says how to
[install or upgrade](../README.md#installing-or-upgrading-a-task-repository) it. The launcher
installs a release into the cache that every repository shares. It names each tool that the
download needs and the machine lacks. To build a checkout for development, run
`cargo xtask build` there and use its `.pumpkin/bin/pumpkin`.

The repository's `.pumpkin/` directory ignores itself in Git. It holds the repository's runs in
`.pumpkin/runs/`, the copies of each controller's runs in `.pumpkin/runs/<name>/`, and the state
of its builds and providers. The release cache, the Docker engine's images and a `--runs`
directory live elsewhere. So do the controller's client keys, in `~/.config/pumpkin/ssh/`.

## The machine's configuration

`~/.config/pumpkin/pumpkin.toml`, or the file that `--config` names, says what a machine's runs
may use. It names the Docker engine and the providers that sandboxes are placed on. It names the
upstreams that model routes reach, with each model's prices, and the files that hold the
upstreams' keys. A machine whose runs go to controllers names each controller in
`[controllers.<name>]`, instead of the engine and the providers.
`pumpkin help config` is the reference, with examples. The [README](../README.md#quickstart) gives
the configuration of a machine whose runs stay on its own engine.

## Checking a task

A task is an entry file below its root. The root is the nearest directory, at or above the entry
file, that holds a `PUMPKIN.star`. `pumpkin check` evaluates the task, checks its images' recipes
and its workflow, and lints the files it read. It reaches no Docker engine and no network.
`pumpkin fmt` formats the files as the lint expects
([the language](language.md#formatting-and-the-lint)).

## Running

`pumpkin run` starts a driver. The driver checks the task and places each role
([providers](providers.md#placing-a-run)). Then it reaches each provider that it placed a role
on, builds the task's images and runs the run. `run` prints the run's id once the run has
started. The run then goes on without `run`. Whatever ends `run` before the start ends the driver
too. The driver writes what it says after the start to the record's `driver.log`. On a machine
with a controller, the controller builds and runs the task. There, `run` prints the id at once.

A run's id is the UTC time it was asked for and eight random hexadecimal digits, such as
`20261116-124728-51da8048`. A command that takes a run takes its id. It also takes a prefix of
the id, or of its eight digits, that no other run shares. A run takes no seed: its sources and
its `--arg`s fix its workflow.

A run outlives a logout unless its engine stops then, as rootless Docker does without
`loginctl enable-linger`, or logind kills the session's processes (`KillUserProcesses`). Then only
a run under the user's manager outlives it: enable lingering, and start `run` under
`systemd-run --user --scope`.

## Following and listing runs

`pumpkin wait` follows a run to its end and says its summary. `run --wait` starts a run and
follows it as `wait` does. `pumpkin ls` lists the runs that are live or ended within the last day,
the newest first. `ls --json` lists them for a program to read. `pumpkin view` serves them to this
machine's browser ([records](record.md#the-page)). `wait`, `ls` and `view` read only the record, so
ending one never stops a run.

A live run's phase is `publishing` what its sandboxes run, `starting` its agents, `work`, `ending`
the work phase, or `final call`. Under the phase, `ls` names each call and each lease that hasn't
ended. It names a call by its function, and a lease by its grant and state. A phase past its bound
is a bug, which `ls` says. An ended run's state is `finished`, `failed`, `interrupted` by a
signal, or `lost` if its driver ended without an `outcome.json`. Its task is its workflow's name,
or `-` if its record has no `ir.json` yet.

## Stopping runs and cleaning up

`pumpkin cancel` stops a run and removes what it left. `pumpkin reap` removes what ended runs
left. SIGTERM, SIGINT or SIGHUP to the driver stops the run, and the run deletes its sandboxes.
The driver's pid is in the record's `driver.pid`. A signal that was ignored as `run` started
stays ignored. SIGKILL to the driver leaves the run lost, and leaves its sandboxes behind. A
create that the driver gave up on can still make a sandbox after that. Every sandbox also ends by
its run's hard end, even if nothing reaps it.

A reap deletes each sandbox that a provider of the run lists under the run's tag. It also deletes
each sandbox that the record holds undeleted. It notes each sandbox that it deletes in
`reaps.jsonl`. It reaches the providers through the configuration that ran the run: the
`--config` that its `record.json` keeps, or else the default. To find by hand what a run left,
look for the containers labelled `pumpkin.run=<id>`, the EC2 instances tagged `pumpkin:run=<id>`
and the Modal sandboxes tagged `pumpkin-run=<id>`. On a controller, `reap` reaps again if the
reply is cut. The ids that it then prints may miss runs that were cleaned before the cut.
