#!/bin/sh
# Prints a pin, `<sha256> <url> <path>`, for each file that `apt-get install
# --no-install-recommends PACKAGE...` fetches in this container, and for each suite's InRelease
# and each index that apt reads, each path the file's below the archive's root, so that the
# files make a mirror of the part of the archive that the install reads. Run as root, online:
#
#     apt.sh PACKAGE...
#
# The sources must name one root, an `https://` URL.
set -o errexit -o nounset
apt-get update --quiet=2 --error-on=any >&2
root=$(sed --quiet 's|^URIs: *||p' /etc/apt/sources.list.d/*.sources \
    | tr ' ' '\n' | sed '/^$/d' | sort --unique)
case $(printf '%s\n' "$root" | wc --lines) in
1) ;;
*) echo "the sources name other roots than one: $root: name one archive" >&2; exit 1 ;;
esac
case $root in
https://*) ;;
*)
    echo "the sources' root, $root, isn't an \`https://\` URL: name the archive by one" >&2
    exit 1
    ;;
esac
root=${root%/}
# Each suite's InRelease, as apt keeps it, each suite as the indexes' URIs name it, as the sources
# do.
targets=$(apt-get indextargets --no-release-info --format '$(METAKEY) $(URI)')
suite() { sed "s|^$root/dists/||; s|/.*||"; }
for suite in $(printf '%s\n' "$targets" | cut --delimiter=' ' --fields=2 | suite \
    | sort --unique); do
    release=$(ls /var/lib/apt/lists/*_dists_"$suite"_InRelease)
    echo "$(sha256sum < "$release" | cut --delimiter=' ' --fields=1) $root/dists/$suite/InRelease dists/$suite/InRelease"
done
order=$(apt-config dump Acquire::CompressionTypes::Order \
    | sed --quiet 's/^Acquire::CompressionTypes::Order:: "\(.*\)";$/\1/p')
printf '%s\n' "$targets" | while read -r key uri; do
    suite=$(printf '%s\n' "$uri" | suite)
    release=$(ls /var/lib/apt/lists/*_dists_"$suite"_InRelease)
    # The compressed index that apt reads: the first of its order that the Release lists.
    sha256=
    for type in $order xz gz; do
        sha256=$(awk -v k="$key.$type" '/^SHA256:/{s=1;next} /^[^ ]/{s=0} s && $3==k {print $1; exit}' "$release")
        [ -z "$sha256" ] || break
    done
    # An index that the Release doesn't list, such as `binary-all`'s, apt doesn't read.
    [ -n "$sha256" ] || continue
    # Fetched by hash, where the file never changes, and kept by name, where apt's `file:`
    # method reads it.
    echo "$sha256 $root/dists/$suite/$(dirname "$key")/by-hash/SHA256/$sha256 dists/$suite/$key.$type"
done
apt-get install --yes --no-install-recommends --print-uris "$@" \
    | sed --quiet "s|^'\([^']*\)' .*|\1|p" | while read -r url; do
    # apt escapes a `+` or `~` as `%2b` or `%7e`, which an `ADD` source can't hold.
    url=$(printf '%s\n' "$url" | sed 's/%2[bB]/+/g; s/%7[eE]/~/g')
    path=${url#"$root/"}
    # Every version's record, as the install may take one that isn't the candidate.
    sha256=$(apt-cache show "$(basename "$path" | cut --delimiter=_ --fields=1)" \
        | awk -v f="$path" '/^Filename: /{m=($2==f)} m && /^SHA256: /{print $2; exit}')
    [ -n "$sha256" ] || { echo "apt-cache shows no SHA-256 for $path" >&2; exit 1; }
    echo "$sha256 $url $path"
done
