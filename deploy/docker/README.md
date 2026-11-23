# deploy/docker

A team's Pumpkin controller in one container, on any Linux x86-64 host with a Docker engine. It
needs no cloud. The members run `pumpkin run`, `wait`, `ls`, `cancel`, `view` and `reap` through it,
as through the controller that [`deploy/aws`](../aws/README.md) makes. Its runs place their roles
on the Docker provider alone, on the container's own engine.

| What | Why |
| --- | --- |
| one privileged container, `pumpkin-controller-1`, with a private cgroup namespace and tmpfs at `/run`, `/run/lock` and `/tmp`, as kind runs a node | systemd as its PID 1, which runs the jobs' units and the engine |
| Ubuntu 24.04 with systemd, `sshd` and what every controller installs by `deploy/controller/install.sh`: Docker's packages at exact versions, and the rest from a snapshot of Ubuntu's archive | builds with the containerd image store |
| a Docker engine of the container's own, never the host's | the sandboxes' mounts, the ledger of CPUs and the prune of images mean what they mean on EC2, and touch nothing of the host's |
| the release that `PUMPKIN_RELEASE` pins, checked by its SHA-256, whose `pumpkin-admin install docker` runs at every start, before `sshd` (`pumpkin-admin install --help` says what it does) | the user `pumpkin`, lingering, `sshd`'s settings, the members' keys and the boot's reap; no idle stop, as nothing would start the container again |
| the volumes `pumpkin_docker`, `pumpkin_containerd`, `pumpkin_home` and `pumpkin_host-key` | the engine's images and build cache, the store, the trees, the installs and the records, and the host key, kept from one container to the next |
| `conf/`, mounted read-only at `/etc/pumpkin/conf` | the settings and the members' list, which the admin edits in place |
| port 22 of the container, published at `PUMPKIN_PORT`, 2222 by default | `sshd`, which admits for `pumpkin` only the keys that the members' list holds |

## Trust

The container is privileged, so whatever is root in it is root on the host. Each member is root in
it, as each member's build runs as `pumpkin`, whose `docker` group is root, as on EC2. A sandbox
that escapes its container on the container's engine is root in it too, and so on the host: on EC2
such an escape reaches one instance, here the whole host. Run it on a machine or a VM that is the
team's alone, and holds nothing else.

## Set up

Docker Engine with Compose v2 and Buildx, on a host with cgroup v2. Check out this directory at the
release's tag, and in it write:

- `.env`, with the release's `.pumpkinversion` line, and the address to publish `sshd` at if not
  port 2222 of every address:

  ```sh
  PUMPKIN_RELEASE="<tag> <commit> <sha256>"
  PUMPKIN_PORT=192.0.2.10:2222
  ```

  Docker writes its own rules for a published port, ahead of a firewall such as ufw, which then
  doesn't block it
  ([docker run](https://github.com/docker/cli/blob/7287ab389066e1e8eefa94e240d013fd93d37ea0/docs/reference/commandline/run.md#L349-L352)).
  So on a host with a public address, name the address of the network that the members reach it
  on, as GitLab's README shows
  ([README](https://gitlab.com/gitlab-org/omnibus-gitlab/-/blob/9b9a2e883910b2f996fd23f4845435e0cdc71550/doc/docker/README.md#L339-L353)).

- `conf/settings.toml`, the jobs' settings, of envelope 1.1, readable by every user, as each job
  reads it as `pumpkin`, in a `conf/` that every user may enter. `[deployment]` is the host's: the controller's `name`, 1 to 16 of a-z,
  0-9 and `-`, and the path of the members' list in the container. The rest is a configuration
  with a Docker engine and one provider of kind `docker`, as `pumpkin help config` says:

  ```toml
  envelope = "1.1"

  [deployment]
  name = "lab"
  members = "/etc/pumpkin/conf/members"

  [docker]
  context = "default"

  [providers.local.docker]
  ```

- `conf/members`, a line `<member> <key>` for each member's key, as `pumpkin-admin install
  --help` says. It may be empty at first.

Then run `docker compose up --detach --build`. `docker compose exec controller journalctl
--unit=pumpkin-install` shows what the install said. Once `conf/` is fixed after an install that
failed, run `docker compose restart`.

Each member names the controller in `~/.config/pumpkin/pumpkin.toml`, with no `modal`, which needs
an EC2 controller, and runs a task repository whose `.pumpkinversion` pins this release or a later
one:

```toml
[controllers.lab]
address = "lab.example.org:2222"
```

## Members

A member's first command prints the line to add to `conf/members`. The controller copies the list
within a minute. Give each member the host key's fingerprint, which `docker compose exec controller
ssh-keygen -l -f /etc/ssh/keys/ssh_host_ed25519_key.pub` prints, to compare with the one their
client pinned: `ssh-keygen -l -F '[lab.example.org]:2222' -f ~/.config/pumpkin/ssh/known_hosts`.
Taking a key off the list refuses it within a minute. A member was root on the host, so make the
host anew once one is no longer trusted.

## Upgrade

1. Set `draining = true` in `conf/settings.toml`. Each job that starts is refused, and the jobs
   that run go on to their ends. `pumpkin ls` shows them.
2. Once none runs, check out the new release's tag, set its line in `.env`, and run
   `docker compose up --detach --build`. The volumes keep the runs' records and the host key. A job
   that still runs is lost, and the next start's boot reaps it.
3. Set `draining = false`.

The volume `pumpkin_home` holds the runs' records. Back it up with the container stopped, as
Docker's documentation backs up a volume
([volumes](https://docs.docker.com/engine/storage/volumes/#back-up-restore-or-migrate-data-volumes)).

## Teardown

`docker compose down --volumes` removes the container and its volumes, the records with them.
