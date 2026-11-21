# Running workflows

This page is for the operator: whoever runs workflows with `pumpkin` and holds the machine's
configuration and keys. It goes through how to configure the machine, check a task, run it,
and follow, list, stop and clean up its runs. Each command's `--help` lists its arguments, output
and exit status. This page says what each command is for and how they fit together.

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
Docker engine's images, records written to an explicit `--runs` directory, files `fmt` and
`lock` rewrite, and the controller's client SSH keys in `~/.config/pumpkin/ssh/` live elsewhere.

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

Each problem is said at its file, line and column, with the source line under it. A recipe must
pin what it fetches: an image by its digest, a file by its checksum, a Git repository by its full
commit. `pumpkin lock` writes the fetch stage that pins a step's downloads, and `pumpkin fmt`
formats `.star` files as the lint expects.

## Running

`pumpkin run` builds the task's images on the Docker engine, then starts a driver process that
places each role, reserves what the run needs on its providers, and runs it. It prints the run's
id once the run has started, then returns; the run goes on without it. On a machine with a
controller, the controller builds the images and starts the driver.

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

## What a command prints next

Every ending that `pumpkin` reports, of a run or of a refusal, ends with the commands to run next,
each a line that a shell runs as it is: how to follow the run or stop it, how to run the task again
with the arguments its record keeps, or how to remove what a lost run left.
