# syntax=docker/dockerfile:1
# A Pumpkin controller in one container: Ubuntu 24.04 with systemd as PID 1, `sshd`, a Docker
# engine of its own, and the release that `PUMPKIN_RELEASE` pins. At every start, a unit runs the
# release's `pumpkin-admin install docker`, which sets up what Pumpkin relies on there, as
# deploy/aws's first boot does. kind runs a node so, and GitLab's image reconfigures at every start:
# https://github.com/kubernetes-sigs/kind/blob/b5bed7e9f115a19e8d9f300be66c462039937d57/images/base/Dockerfile#L211-L218
# https://gitlab.com/gitlab-org/omnibus-gitlab/-/blob/9b9a2e883910b2f996fd23f4845435e0cdc71550/docker/assets/wrapper#L142-L156

# Ubuntu 24.04 with CA certificates and curl, which apt and the release's download use over HTTPS.
# Docker's packages at exact versions, and the rest from a snapshot of Ubuntu's archive, as
# deploy/aws's controller.tf pins them.
FROM buildpack-deps:noble-curl@sha256:6b03a2e5a50d3b482003a040045952cfbd7ed8081e1d6ac85f283b6e44fbff27 AS ubuntu
COPY --from=aws docker.asc /etc/apt/keyrings/docker.asc
RUN <<EOF
set -o errexit
sed --in-place 's|http://[a-z.]*ubuntu.com/ubuntu/|https://snapshot.ubuntu.com/ubuntu/20261101T000000Z/|' \
    /etc/apt/sources.list.d/ubuntu.sources
echo 'deb [arch=amd64 signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu noble stable' \
    > /etc/apt/sources.list.d/docker.list
EOF

# The release, as the `.pumpkinversion` line `<tag> <commit> <sha256>` pins it, checked by its
# SHA-256, as deploy/aws's controller.yaml checks it.
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
set -- $PUMPKIN_RELEASE
cd "$(mktemp --directory)"
curl --fail --silent --show-error --location --proto '=https' --output pumpkin.tar.xz \
    "https://github.com/FrontierCS/Pumpkin/releases/download/$1/pumpkin-$1-x86_64-unknown-linux-musl.tar.xz"
echo "$3  pumpkin.tar.xz" | sha256sum --check --strict
mkdir /pumpkin
tar --extract --xz --file=pumpkin.tar.xz --directory=/pumpkin
EOF

# The install alone, so that `--build-context pumpkin=<dir>` puts another in its place.
# https://github.com/docker/buildx/blob/876462897612d36679153c3414f7689626251501/docs/reference/buildx_build.md#L116-L117
FROM scratch AS pumpkin
COPY --from=download /pumpkin/ /

FROM ubuntu
RUN <<EOF
set -o errexit
apt-get update
DEBIAN_FRONTEND=noninteractive apt-get install --yes --no-install-recommends \
    systemd systemd-sysv dbus libpam-systemd openssh-server \
    docker-ce=5:29.9.2-1~ubuntu.24.04~noble \
    docker-ce-cli=5:29.9.2-1~ubuntu.24.04~noble \
    containerd.io=2.4.2-3~ubuntu.24.04~noble \
    docker-buildx-plugin=0.38.1-1~ubuntu.24.04~noble
rm --recursive --force /var/lib/apt/lists/*
# The package's host keys would be every build's. The volume keeps the container's own.
rm --force /etc/ssh/ssh_host_*
# The user that the install makes, at an ID that every build gives it, as the volume at /home
# keeps its files from one container to the next. Its home is copied into an empty volume.
useradd --uid=1001 --user-group --create-home --shell=/bin/sh --groups=docker pumpkin
EOF

# Builds find images by their manifests' digests, which only the containerd image store keeps.
COPY <<EOF /etc/docker/daemon.json
{"features": {"containerd-snapshotter": true}}
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
# the boot's reap, which then runs after it. `sshd` starts after it, as a new container has none
# of `sshd`'s settings and the members' keys until it has run.
COPY <<EOF /etc/systemd/system/pumpkin-install.service
[Unit]
Description=Install the Pumpkin controller's host software from /opt/pumpkin
Before=pumpkin-boot.service ssh.service

[Service]
Type=oneshot
ExecStart=/opt/pumpkin/bin/pumpkin-admin install docker --settings /etc/pumpkin/conf/settings.toml
ExecStartPost=/usr/bin/systemctl start --no-block pumpkin-boot.service

[Install]
WantedBy=multi-user.target
EOF
RUN systemctl enable pumpkin-install.service

COPY --from=pumpkin / /opt/pumpkin/

# systemd reads that it runs in a container from `container`, and stops on SIGRTMIN+3:
# https://github.com/kubernetes-sigs/kind/blob/b5bed7e9f115a19e8d9f300be66c462039937d57/images/base/Dockerfile#L211-L216
ENV container=docker
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
