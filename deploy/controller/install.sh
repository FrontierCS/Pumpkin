#!/bin/sh
# What every Pumpkin controller installs on Ubuntu 24.04, whichever its target: Docker's packages
# from Docker's repository, at the versions pinned here, with the containerd image store; the rest
# from the snapshot of Ubuntu's archive pinned here; and a Pumpkin release, checked by its SHA-256.
# deploy/aws's first boot and deploy/docker's image run it, as root, beside `docker.asc`.
#
#   install.sh sources                 point apt at the snapshot and at Docker's repository
#   install.sh docker [<package>...]   install Docker's packages, and each <package>
#   install.sh release <line> <dir>    unpack into <dir> the release that <line>, a
#                                      `.pumpkinversion` line, pins
set -o errexit -o nounset

SNAPSHOT=20261101T000000Z
DOCKER=5:29.9.2-1~ubuntu.24.04~noble
CONTAINERD=2.4.2-3~ubuntu.24.04~noble
BUILDX=0.38.1-1~ubuntu.24.04~noble

here=$(dirname "$0")

# `apt-get` with the arguments given, tried again 5 s apart while it fails, 6 times in all: a first
# boot's apt-daily may hold a lock of apt's, and apt waits for none but dpkg's frontend lock, as it
# takes the others with a lock that doesn't wait. So cloud-init tries an apt command again, for up
# to 30 s, while another apt holds apt's locks:
# https://salsa.debian.org/apt-team/apt/-/blob/db797312548f6e7d6d1d1faa9f1e48c1e521d290/apt-pkg/contrib/fileutl.cc#L279
# https://github.com/canonical/cloud-init/blob/905d10891051496bd196798b2a8e0d77f161db74/cloudinit/distros/debian.py#L202-L243
apt_get() {
    tries=1
    until DEBIAN_FRONTEND=noninteractive apt-get "$@"; do
        [ "$tries" -lt 6 ] || return 1
        tries=$((tries + 1))
        sleep 5
    done
}

case "${1-}" in
sources)
    install -D --mode=0644 "$here/docker.asc" /etc/apt/keyrings/docker.asc
    # Over HTTPS, for every suite, as the image's sources name the archive's and the security
    # pocket's.
    cat > /etc/apt/sources.list.d/ubuntu.sources <<SOURCES
Types: deb
URIs: https://snapshot.ubuntu.com/ubuntu/$SNAPSHOT/
Suites: noble noble-updates noble-backports noble-security
Components: main restricted universe multiverse
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg
SOURCES
    echo 'deb [arch=amd64 signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu noble stable' \
        > /etc/apt/sources.list.d/docker.list
    ;;
docker)
    shift
    # Before the engine's first start: builds find images by their manifests' digests, which only
    # the containerd image store keeps.
    mkdir --parents /etc/docker
    cat > /etc/docker/daemon.json <<DAEMON
{"features": {"containerd-snapshotter": true}}
DAEMON
    apt_get update
    apt_get install --yes "docker-ce=$DOCKER" "docker-ce-cli=$DOCKER" \
        "containerd.io=$CONTAINERD" "docker-buildx-plugin=$BUILDX" "$@"
    ;;
release)
    dir=$(realpath --canonicalize-missing "$3")
    set -- $2
    cd "$(mktemp --directory)"
    curl --fail --silent --show-error --location --proto '=https' --output pumpkin.tar.xz \
        "https://github.com/FrontierCS/Pumpkin/releases/download/$1/pumpkin-$1-x86_64-unknown-linux-musl.tar.xz"
    echo "$3  pumpkin.tar.xz" | sha256sum --check --strict
    mkdir --parents "$dir"
    tar --extract --xz --file=pumpkin.tar.xz --directory="$dir"
    ;;
*)
    echo "usage: $0 sources | docker [<package>...] | release <line> <dir>" >&2
    exit 2
    ;;
esac
