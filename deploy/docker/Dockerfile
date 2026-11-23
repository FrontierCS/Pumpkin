# syntax=docker/dockerfile:1
# A Pumpkin controller in one container: Ubuntu 24.04 with systemd as PID 1, `sshd`, a Docker
# engine of its own, and the release that `PUMPKIN_RELEASE` pins. At every start, a unit runs the
# release's `pumpkin-admin install docker`, which sets up what Pumpkin relies on there, as
# deploy/aws's first boot does. kind runs a node so, and GitLab's image reconfigures at every start:
# https://github.com/kubernetes-sigs/kind/blob/b5bed7e9f115a19e8d9f300be66c462039937d57/images/base/Dockerfile#L211-L218
# https://gitlab.com/gitlab-org/omnibus-gitlab/-/blob/9b9a2e883910b2f996fd23f4845435e0cdc71550/docker/assets/wrapper#L142-L156

# Ubuntu 24.04 with CA certificates and curl, which apt and the release's download use over HTTPS,
# and the sources that every controller's install takes, from deploy/controller.
FROM buildpack-deps:noble-curl@sha256:6b03a2e5a50d3b482003a040045952cfbd7ed8081e1d6ac85f283b6e44fbff27 AS ubuntu
COPY --from=controller install.sh docker.asc /usr/local/lib/pumpkin-controller/
RUN /usr/local/lib/pumpkin-controller/install.sh sources

# The release, as the `.pumpkinversion` line `<tag> <commit> <sha256>` pins it, checked by its
# SHA-256.
FROM ubuntu AS download
ARG PUMPKIN_RELEASE
RUN <<EOF
set -o errexit
if [ -z "$PUMPKIN_RELEASE" ]; then
    echo "PUMPKIN_RELEASE is unset: set it in .env to the release's .pumpkinversion line" >&2
    exit 1
fi
apt-get update
DEBIAN_FRONTEND=noninteractive apt-get install --yes --no-install-recommends xz-utils
/usr/local/lib/pumpkin-controller/install.sh release "$PUMPKIN_RELEASE" /pumpkin
EOF

# The install alone, so that `--build-context pumpkin=<dir>` puts another in its place.
# https://github.com/docker/buildx/blob/876462897612d36679153c3414f7689626251501/docs/reference/buildx_build.md#L116-L117
FROM scratch AS pumpkin
COPY --from=download /pumpkin/ /

# systemd and `sshd`, then the Docker engine that every controller's install takes.
FROM ubuntu
RUN <<EOF
set -o errexit
apt-get update
DEBIAN_FRONTEND=noninteractive apt-get install --yes --no-install-recommends \
    systemd systemd-sysv dbus libpam-systemd openssh-server
/usr/local/lib/pumpkin-controller/install.sh docker
rm --recursive --force /var/lib/apt/lists/*
# The package's host keys would be every build's. The volume keeps the container's own.
rm --force /etc/ssh/ssh_host_*
EOF

# The host key, in the volume at /etc/ssh/keys, made at the first start, so that it outlives each
# container and no client is refused after an upgrade. GitLab's image makes its host keys so:
# https://gitlab.com/gitlab-org/omnibus-gitlab/-/blob/9b9a2e883910b2f996fd23f4845435e0cdc71550/docker/assets/wrapper#L125-L140
COPY <<EOF /etc/ssh/sshd_config.d/05-pumpkin-host-key.conf
HostKey /etc/ssh/keys/ssh_host_ed25519_key
EOF
COPY <<EOF /etc/systemd/system/pumpkin-host-key.service
[Unit]
Description=Make the Pumpkin controller's host key if it has none
ConditionPathExists=!/etc/ssh/keys/ssh_host_ed25519_key
Before=ssh.service

[Service]
Type=oneshot
ExecStart=/usr/bin/ssh-keygen -q -t ed25519 -N "" -f /etc/ssh/keys/ssh_host_ed25519_key

[Install]
RequiredBy=ssh.service
EOF
RUN systemctl enable pumpkin-host-key.service

# The install, at every start, from the settings in the directory that compose mounts. It enables
# the boot's reap, which then runs after it. `sshd` starts only once it has succeeded, as a new
# container has none of `sshd`'s settings and the members' keys until it has run, and an `sshd`
# without them would admit the keys that the kept home's `authorized_keys` holds. It stays active
# once it has succeeded, so that the start of `sshd` that the first connection makes doesn't run it
# again, as systemd's manual says of a oneshot that a dependency pulls in:
# https://github.com/systemd/systemd/blob/db11bab38ccf1ed257f310d29070843d4c58ea01/man/systemd.service.xml#L1602-L1613
COPY <<EOF /etc/systemd/system/pumpkin-install.service
[Unit]
Description=Install the Pumpkin controller's host software from /opt/pumpkin
Before=pumpkin-boot.service ssh.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/opt/pumpkin/bin/pumpkin-admin install docker --settings /etc/pumpkin/conf/settings.toml
ExecStartPost=/usr/bin/systemctl start --no-block pumpkin-boot.service

[Install]
WantedBy=multi-user.target
RequiredBy=ssh.service
EOF
RUN systemctl enable pumpkin-install.service

COPY --from=pumpkin / /opt/pumpkin/

# Before systemd starts, every process of the container's cgroup moves into `init.scope`, and the
# cgroup gives its children every controller. systemd moves the processes there only once, as it
# starts. A `docker exec` that came after would stay in the cgroup, and the kernel would refuse
# each controller to its children while it ran, so the engine would start without the memory
# controller, and say for good that it can't limit swap. Once the cgroup gives its children
# controllers, a `docker exec` joins systemd's own cgroup instead. moby's dind enables them so, in a
# loop for `docker exec`'s race, on cgroup v2 alone; on a host without it, which the README asks
# for, this stops with why. systemd takes `init.scope`'s parent as its root:
# https://github.com/moby/moby/blob/1f98bfc8473f1c9753cbc80bdc30c04dfea266e3/hack/dind#L61-L80
# https://github.com/systemd/systemd/blob/db11bab38ccf1ed257f310d29070843d4c58ea01/src/core/cgroup.c#L3698-L3699
# https://github.com/systemd/systemd/blob/db11bab38ccf1ed257f310d29070843d4c58ea01/src/core/cgroup.c#L3799-L3805
# https://github.com/opencontainers/runc/blob/48dfb3a2c0bc9967074e04609493a500120ba018/libcontainer/process_linux.go#L308-L318
COPY --chmod=0755 <<EOF /usr/local/sbin/pumpkin-init
#!/bin/sh
if [ ! -f /sys/fs/cgroup/cgroup.controllers ]; then
    echo "pumpkin-init: /sys/fs/cgroup is no cgroup v2 hierarchy: run the controller on a host with cgroup v2" >&2
    exit 1
fi
mkdir -p /sys/fs/cgroup/init.scope
while ! {
    xargs -rn1 < /sys/fs/cgroup/cgroup.procs > /sys/fs/cgroup/init.scope/cgroup.procs || :
    sed -e 's/ / +/g' -e 's/^/+/' < /sys/fs/cgroup/cgroup.controllers > /sys/fs/cgroup/cgroup.subtree_control
}; do :; done
exec /sbin/init
EOF

# systemd reads that it runs in a container from `container`, and stops on SIGRTMIN+3:
# https://github.com/kubernetes-sigs/kind/blob/b5bed7e9f115a19e8d9f300be66c462039937d57/images/base/Dockerfile#L211-L216
ENV container=docker
STOPSIGNAL SIGRTMIN+3
ENTRYPOINT ["/usr/local/sbin/pumpkin-init"]
