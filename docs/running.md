# Running workflows

This page is for the operator: whoever runs workflows with `pumpkin` and holds the machine's
configuration and keys. It goes through how to configure the machine, check a task, run it,
and follow, list, stop and clean up its runs. Each command's `--help` lists its arguments, with
examples, and `pumpkin --help` gives the exit statuses. This page says what each command is for
and how they fit together.

## Installing

The task maintainer follows the README's
[install or upgrade instructions](../README.md#installing-or-upgrading-a-task-repository).
Each release provides `pumpkin`, the launcher, and `pumpkinversion`, which the repository keeps as
`.pumpkinversion`. Its one line pins the tag, full commit and SHA-256 of the release archive.

The first run downloads the archive, verifies its checksum and unpacks the release under
`${XDG_CACHE_HOME:-$HOME/.cache}/pumpkin/`. Every repository that pins the same release runs it
from that cache. Installing an uncached release on Linux x86-64 needs curl or wget, `sha256sum`,
tar, xz, `flock` from util-linux and TLS certificates.

To build a checkout for development, run `cargo xtask build` from it and use its
`.pumpkin/bin/pumpkin`.

The repository's `.pumpkin/` beside `./pumpkin` holds its runs in `.pumpkin/runs/` and the state
its builds and providers keep. The directory ignores itself in Git. The shared release cache,
Docker engine's images, records written to an explicit `--runs` directory, files `fmt`
rewrites, and the controller's client SSH keys in `~/.config/pumpkin/ssh/` live elsewhere.

## The machine's configuration

`~/.config/pumpkin/pumpkin.toml` (or the file that `--config` names) says what a machine's runs
may use: its Docker engine, the providers that sandboxes may be placed on, the upstreams that
model routes reach, with each model's prices, and the files that hold their keys. A machine whose
runs go to a controller names `[controller]` in place of the engine and the providers.
`pumpkin help config` is the reference, with an example of a machine whose runs go to EC2 and
Modal and of one whose runs go to a controller; the [README](../README.md#run-it) gives one whose
runs stay on its own Docker engine.

## Checking a task

A task is an entry file below a root: the nearest directory, at or above it, that holds a
`PUMPKIN.star`, where the task's `//` labels start. `pumpkin check` evaluates the entry file and
the files it loads, with the `--arg`s given, then checks each image's recipe and the workflow, then
lints the files it read. It reaches no Docker engine, no network and no configuration, and prints
nothing when it finds no problem.

```console
$ ./pumpkin check tasks/bench/task.star --arg model=model-a
$ ./pumpkin check tasks/*/task.star --arg model=model-a
```

Each problem is said at its file's path below the root, its line and its column, then its source
line, with carets under what it is about. A recipe's problem is said at the `image()` call that
declares it. Past 20 problems, `check` counts the rest. It reads only files below the root, with
no symbolic link on the way to them. An image's build can still fail after a clean check, as when
a recipe's step fails.

A recipe must pin what it fetches: an image by its digest, a file by its checksum, a Git
repository by its full commit; a stage that pins a step's downloads by checksum, as
`ADD --checksum` does, satisfies it. `pumpkin fmt` formats `.star` files as the lint expects, and
[the language](language.md#formatting-and-the-lint) lists the lint's rules.

## Running

`pumpkin run` starts a driver process, which builds the task's images on the Docker engine,
places each role, reserves what the run needs on its providers, and runs it. `run` prints the
run's id once the run has started, then returns; the run goes on without it. On a machine with a
controller, the controller builds the images and starts the driver. `run` sends it the files below
the paths that the task reaches in its Git repository, whether Git tracks them, ignores them or
neither. `run` then prints the id at once, before the build, and
`pumpkin wait` follows the build and the run.

```console
$ ./pumpkin run tasks/bench/task.star --arg model=model-a
$ ./pumpkin run tasks/bench/task.star --arg model=model-a --wait
```

The blocks below show what to expect from Pumpkin's own `examples/hello`, which needs no model,
on a machine whose task repository is `/home/alice/tasks`. `run` says each step on its standard error, such
as `pumpkin: building an image in pumpkin/hello` when the engine lacks an image, and prints the
run's id on its standard output:

```console
$ ./pumpkin run examples/hello/task.star
pumpkin: the Docker engine that context `default` reaches runs as root, so its `docker` group is root-equivalent: rootless Docker is recommended
pumpkin: run 20261116-195228-234cd166 records into /home/alice/tasks/.pumpkin/runs/20261116-195228-234cd166
pumpkin: run 20261116-195228-234cd166 started
pumpkin: to follow it: pumpkin wait 20261116-195228-234cd166
pumpkin: to stop it: pumpkin cancel 20261116-195228-234cd166
20261116-195228-234cd166
```

Before it reaches the Docker engine, the driver checks the task and reads the configuration. It warns if the engine runs as root. It
places each role, then reaches each provider that it places a role on, then builds the task's
images on the engine. [Providers](providers.md#placing-a-run) says how
it places each role. The driver writes what it says after the start to `driver.log` in the
record. Whatever ends `run` before the start ends the driver too. After the start, the run goes on
without `run`.

The driver tags each image by the digest of the build's inputs. The image goes in `pumpkin/<recipe's
directory below the root>`, or for a recipe of the standard library in `pumpkin-library/<directory
below @pumpkin//>`. An image that the engine already has under its tag isn't built again. As many
builds run at once as the CPUs that `pumpkin` may run on, as Cargo's jobs do. Each build starts once
the images it builds on are built. A failed build's output is said whole as it ends, as `make
--output-sync` does. A build that succeeds says only that it ended. A failed build is said at the
`image()` call that declares its image. Once one has failed, no other build starts, and those
already running are waited for.

A run outlives a logout unless its engine stops then, as rootless Docker does without
`loginctl enable-linger`, or logind kills the session's processes (`KillUserProcesses`). Then only
a run under the user's manager outlives it: enable lingering, and start `run` under
`systemd-run --user --scope`.

A run's id is the UTC time it was asked for and eight random hexadecimal digits, such as
`20261116-124728-51da8048`; any command that takes a run takes its id, or a prefix of the id or of
its eight digits, that no other run shares. A run takes no seed: its workflow is fixed by its sources and its `--arg`s, and
the random digits of its id are fresh each time.

## Following and listing runs

`pumpkin wait <run>` says each milestone of the run as its record logs it, and once it has ended,
its summary: how long each sandbox and call took, the model requests and their tokens, what each
budget spent, the cost, and the values its result shows. It exits 0 if the run finished with a
result, 3 if it finished without one, and 1 if it failed, was interrupted or was lost. Ending
`wait` does not stop the run. `run --wait` starts a run and then follows it as `wait` does. Both
print the milestones and the summary on their standard error, and nothing on their standard output
unless `--outcome` asks for the run's `outcome.json`, which [records](record.md) shows:
`pumpkin wait --outcome 234cd166 > outcome.json`. A run that finishes without a result says in its
last lines why; when its final call gave none, it also says where that call's commands' logs
are, or that it ran no command.

```console
$ ./pumpkin wait 234cd166
pumpkin: following run 20261116-195228-234cd166, recorded in /home/alice/tasks/.pumpkin/runs/20261116-195228-234cd166: ending `pumpkin wait` does not stop it
pumpkin:    0:00 agent started on local/container
pumpkin:    0:00 count job 0 of agent 0 asked on local/container
pumpkin:    0:00 count job 0 of agent 0 started
pumpkin:    0:01 count job 0 of agent 0 ended: lines=1 bytes=6
pumpkin:    0:01 the work phase ended
pumpkin:    0:01 final call job 1 asked on local/container
pumpkin:    0:01 final call job 1 started
pumpkin:    0:02 final call job 1 ended: score=6 lines=1
pumpkin: run 20261116-195228-234cd166 started 1s after it was asked for, and ran 2s:
     0:00 agent on local/container, started at 0:00, exited 0 at 0:01; its work ended at 0:01
     0:00 count job 0 of agent 0 on local/container, started after 0s, ran 0s: lines=1 bytes=6
     0:01 final call job 1 on local/container, started after 0s, ran 0s: score=6 lines=1
  sandboxes: 3 on local/container, 3s in all, 3 created, all deleted
pumpkin: run 20261116-195228-234cd166 finished with a result (score=6 lines=1); its record is /home/alice/tasks/.pumpkin/runs/20261116-195228-234cd166
pumpkin: to run it again: pumpkin run /home/alice/tasks/examples/hello/task.star
```

`pumpkin ls` lists the runs that are live or ended within the last day, a line each, then their
totals: a live run's phase and the bound at which Pumpkin ends it, an ended run's state, how long
it ran, its cost and its result's values. `pumpkin ls 20261116` lists a day's runs.

A live run's phase is `publishing` what its sandboxes run, `starting` its agents, `work`, `ending`
the work phase, or `final call`. Under it, a line names each call, by function, and each lease, by
its grant and state, that hasn't ended. A phase past its bound is a bug, which `ls` says. An ended
run's state is `finished`, `failed`, `interrupted` by a signal, or `lost` if its driver ended
without an `outcome.json`. Its task is its workflow's name, or `-` if its record has no `ir.json`
yet. The default runs directory lists no runs until a run makes it.

```console
$ ./pumpkin ls
20261116-195228-234cd166 finished         2s  -               -  examples/hello: score=6 lines=1
1 run: 1 finished
```

`pumpkin view` serves the same runs to this machine's browser; [records](record.md) says what it
shows.

## Stopping runs and cleaning up

`pumpkin cancel <run>` stops a run: the driver deletes the run's sandboxes, and the run's outcome
says which signal ended it. A run whose driver died is lost; `cancel` removes what it left, and
`pumpkin reap` does so for ended runs. On a controller it reaps the runs made by this build
and names those left to other builds. Every sandbox also ends by its run's hard end, which the
workflow's bounds give, even if nothing reaps it.

SIGTERM, SIGINT and SIGHUP to the driver stop the run. Its pid is in the record's `driver.pid`.
The run removes its sandboxes, and its `outcome.json` says which signal interrupted it. A signal
that was ignored as `run` started stays ignored. SIGKILL to the driver, as `cancel --force` sends,
leaves the run lost with its sandboxes. A create that the driver gave up on can still make a
sandbox after that.

`reap`, and `cancel` after the stop, delete what a run left on each provider it ran on. On the
Docker engine of the context that the record's `provider.json` names, they delete the containers
labelled `pumpkin.run=<id>`. On a provider of kind ec2, they delete the EC2 instances tagged
`pumpkin:run=<id>`, and on one of kind modal, the Modal sandboxes tagged `pumpkin-run=<id>`. They
read such a provider from the configuration that ran the run: the `--config` that its
`record.json` keeps, or else the default. A reap also removes the run's socket directories in
`sockets/` and records a lost run as failed. It notes each sandbox it removes in `reaps.jsonl`,
with the time. A provider may no longer list a sandbox that the record holds undeleted, as when
the configuration now points to another region or profile. The reap then says so and notes the
sandbox there as gone. On a controller, `reap` reaps again if the reply is cut. The ids it prints may then omit
runs that were cleaned before the cut.

## What a command prints next

Every ending that `pumpkin` reports, of a run or of a refusal, ends with the commands to run next,
each a line that a shell runs as it is: how to follow the run or stop it, how to run the task again
with the arguments its record keeps, or how to remove what a lost run left. Their paths are made
absolute. The command that runs a task again builds it as its source is then. A failed `check`
or `fmt` ends with the command that retries the entries or files that failed.
