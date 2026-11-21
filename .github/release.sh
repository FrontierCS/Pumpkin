#!/usr/bin/env bash
set -o errexit -o nounset -o pipefail
export LC_ALL=C
umask 022

if (( $# != 1 )); then
    echo "Usage: $0 TAG" >&2
    exit 1
fi
tag=$1
case "$tag" in
    ''|[!a-zA-Z0-9]*|*[!a-zA-Z0-9._-]*) echo "Unsupported release tag: $tag" >&2; exit 1 ;;
esac

cd -- "$(dirname -- "$0")/.."
output="$PWD/target/release-assets"
mkdir --parents "$output"
scratch=$(mktemp --directory "$output/.build.XXXXXXXX")
trap 'rm --recursive --force "$scratch"' EXIT
trap 'exit 1' HUP INT TERM

target=x86_64-unknown-linux-musl
cargo xtask install --root "$scratch/dist" --target "$target"
git rev-parse HEAD > "$scratch/dist/commit"
cp deploy/buildifier-LICENSE "$scratch/dist/"

cd -- "$scratch"
# The upstream formatter includes compressed Go debug sections in its release binary.
strip --strip-debug dist/bin/buildifier
archive="pumpkin-$tag-$target.tar.xz"
tar --create --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner \
    --file - --directory dist bin buildifier-LICENSE commit | xz --threads=1 --stdout > "$archive"
sha256sum "$archive" > "$archive.sha256"

mkdir unpacked
tar --extract --xz --file "$archive" --directory unpacked
cmp dist/commit unpacked/commit
for program in pumpkin sandboxd ec2-boot buildifier; do
    file --brief "unpacked/bin/$program" | grep --extended-regexp 'ELF 64-bit.*x86-64.*static(-pie|ally)? linked'
done
test -s unpacked/bin/pumpkin-modal.py
test -s unpacked/bin/pumpkin-modal.py.lock
unpacked/bin/buildifier --version
unpacked/bin/pumpkin --version | grep --fixed-strings "($(cut --characters=1-9 dist/commit) "
mv --target-directory "$output" "$archive" "$archive.sha256"
