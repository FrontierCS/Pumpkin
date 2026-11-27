# Providers and the controller

A provider makes a run's sandboxes: containers on the machine's own Docker engine, containers on
EC2 instances, or Modal sandboxes. A controller is an EC2 instance that runs the runs that the
operator's machine sends it. The operator is whoever runs `pumpkin` and holds the machine's
configuration and keys. This page says what each is, what it needs, and where each is set up:
`pumpkin help config` holds every key of the machine's configuration, and
[`deploy/aws`](../deploy/aws/README.md) the AWS side.

![On the operator's machine, pumpkin builds images on the Docker engine, which also runs the Docker provider's containers. A machine with a controller sends it its runs over ssh. An EC2 controller pushes its images to ECR, launches EC2 instances that pull them, and creates Modal sandboxes, from images that Modal imports. A Modal sandbox may hold companions as Modal's sidecars. A Docker controller, a privileged container, builds and runs on an engine of its own, never the host's. Docker mounts assets read-only from its cache, and Modal from an image filled once per digest.](figures/providers.svg)

## Placing a run

Every image is built on the Docker engine of the machine that runs the driver, the controller's
own when runs go to a controller, whichever provider runs it, and tagged by the digest of its
inputs. A role's `needs` says what one sandbox of it needs: its CPUs, its memory, and
a GPU or a CPU feature if any. The run places each role in the smallest class of sandbox, of every
provider's in the configuration, that offers that, and refuses a role that fits none, saying why
of each class, before any image is built. Of the providers but the engine's own, it reaches only
those that it places a role on. A provider whose sandboxes pull their images, EC2 or Modal, first
pushes them to where they pull from. Every sandbox carries its run's tag, so runs
that share a provider never reach each other's sandboxes.

## The Docker provider

Its sandboxes are containers on the machine's own engine, each confined, with no network but
loopback, on whole cores that no other run's containers get while the run lasts. A run whose roles
need more cores than other runs leave is refused, and can be run again once they end. It has one
class, `container`, that offers the machine's CPUs and memory and no GPU. The class holds assets,
mounted read-only from `.pumpkin/assets/` on the driver's machine. A machine that uses it uses no
other provider, so where a run's roles are placed never depends on how much memory the machine
has. Rootless Docker is the recommended engine: `pumpkin run` warns of one that runs as root.

## The EC2 provider

Its sandboxes are containers on EC2 instances of their own, one each, launched from the launch
templates of the envelope that `deploy/aws` makes, which the configuration's classes name. The
images go to the envelope's private ECR repositories, and each instance pulls its role's image with
its own role, through ECR's credential helper, which keeps no login. An instance's firewall lets its
container reach the gateway's address and port and nothing else, not the instance nor its metadata.
Each instance's termination is scheduled at its sandbox's hard end, outside the instance, so it ends
even if the driver dies. Its user data holds no secret: a sandbox proves which it is by its
instance's identity document, which AWS signs, and the gateway admits it once, for the instance
that the run launched, from that instance's address. The provider holds AWS's certificate of the
signing key for each region that AWS lists, and refuses a region it holds none for as it connects.

## The Modal provider

Its sandboxes are sandboxes in a Modal workspace, under gVisor, that run images which Modal
imports from the envelope's ECR repositories with a role that it assumes by its own identity, so
no AWS credential of the operator's reaches Modal. A sandbox's containers connect only to one
another and to Modal's resolver. It answers for public names, so code in any container can send
data out in DNS queries. The driver dials each sandbox through Modal's TLS tunnel, which takes
connections from `dials_from` alone. Modal ends each sandbox at its hard end. Its classes hold
assets, outside `/__modal`.

Its classes hold a runner's [companions](glossary.md#companion), each as a
[sidecar of Modal's](https://modal.com/docs/guide/sandbox-sidecars), which has no GPU. A sandbox
at the least needs that hold its companions may not [meet](glossary.md#meeting) the gateway
within its class's `startup_seconds`.

Each container has its own files, processes, address and loopback. Any can connect to the others,
by the name `main` or a companion's `name`. A service sees every such caller at one address. So a
task authenticates its services, starting each with a secret before the agent's code runs. It
binds each to `0.0.0.0`, and polls each until it answers.

The containers share the sandbox's memory, so one that exhausts it can end a companion alone or
the whole sandbox. Code in `main` can end the [relay](glossary.md#relay) that carries each
companion's session, and so fail the [lease](glossary.md#lease).

Pumpkin's `sandboxd` listens on port 7434 in each container, and on 7435 in `main` if the runner
has companions. A task's own services take other ports.

## The controller

The controller is the same `pumpkin`, under `pumpkin controller`, on an EC2 instance that
`deploy/aws` makes or in the container that `deploy/docker` makes. On EC2, its jobs read their
settings, such as the EC2 provider's launch templates, from the module's SSM parameter, and the
machine finds and starts it through EC2. The machine reaches it at its private address from the
controller's VPC, or through a VPN that translates its addresses into the VPC's. A machine
elsewhere reaches it at its public address, from a block that the module admits. On Docker, its
jobs read their settings from a file that the admin writes, and run on the Docker provider alone,
on the container's own engine. The machine reaches it at the address that its configuration
names, and it never stops. Either admits only the keys that its members' list holds, and names
each member's jobs.
A machine whose configuration names
`[controllers.<name>]` sends it each run, over ssh: `pumpkin run` sends the files below the paths that
the task reaches in its repository, whether git tracks or ignores them, then starts the run's job, which builds the images and runs the driver there, in a
systemd unit of its own. `wait`, `ls`, `cancel`, `view` and `reap` work on the controller's runs
with the same arguments, and `wait` copies an ended run's record back to the machine, into
`.pumpkin/runs/<name>/`. A run whose copy there has ended needs no controller. A job refuses a run
that never fits the controller before it waits for a build slot.

A job builds once it takes one of the controller's build slots, one for every two of its CPUs. While
every slot is taken, it waits. Once a prune has removed the images that no job holds, a job refuses
to build while more than 85% of the controller's volume is used. `ls` says first how long the
controller has been up (on Docker, its host), its jobs, how full its disk is, and whether its boot failed. It lists a job
that has yet to start its run as `waiting` for a build slot or `building`, or as `unstarted` once it
has ended without one. It lists this machine's copies of the controller's runs beside the controller's. `cancel`
stops a job whose run has yet to start. `cancel` and `reap` send this machine's Modal configuration
file, which the boot lacks.

`view` sends each of the page's requests for the runs' data over one `ssh` to
`pumpkin controller serve`, a copy of this `pumpkin` that it installs on the controller if needed.
It starts the controller if it is stopped. Once the connection has ended, the next request opens
it again. While that fails, the page says so and asks again after 5 s, then after twice the wait
before, up to a minute. When an EC2 controller has stopped, as an idle one does, the page offers a
"Start it" button (`aws ec2 start-instances`). Until the controller answers, each request asks EC2
whether it runs (`aws ec2 describe-instances`), or probes the `address` of one on Docker.
`view --runs .pumpkin/runs/<name>` serves the copies that `wait` made.

An EC2 controller starts when a command needs it and stops itself once it has been idle a while,
with no job and no ssh session. At boot, each controller reaps what lost runs left. A machine's
keys go to it only with a job, into the job's directory in memory, which the job's unit removes as it ends. A job
runs the same Pumpkin and helper programs as the machine, which uploads its install when
the controller lacks it. At boot, each installed build reaps its own records and appends its
output to `~/boot.log`.
