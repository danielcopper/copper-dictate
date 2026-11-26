#!/usr/bin/env bash
# Pack the release binary into copper-dictate-<tag>-x86_64-linux.tar.gz and its
# .sha256 under <out-dir>, then check the pair: the checksum verifies, and the
# binary unpacked from the archive reports the version Cargo.toml holds.
#
#   scripts/package.sh <out-dir> [<tag>]
#
# <tag> defaults to v<version>. A tag given must be exactly that: the release
# names its assets after the tag release-please made, and a tag that disagrees
# with Cargo.toml means the archive would carry a binary of another version.
set -euo pipefail

if [ $# -lt 1 ] || [ $# -gt 2 ]; then
    echo "usage: $0 <out-dir> [<tag>]" >&2
    exit 2
fi
out=$1
pkgid=$(cargo pkgid --quiet)
version=${pkgid##*[#@]}
tag=${2:-v$version}
if [ "$tag" != "v$version" ]; then
    echo "package: tag $tag does not match Cargo.toml's version $version" >&2
    exit 1
fi

binary=target/release/copper-dictate
if [ ! -x "$binary" ]; then
    echo "package: $binary is missing; build it with cargo build --release --locked first" >&2
    exit 1
fi

name=copper-dictate-$tag-x86_64-linux
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

mkdir "$stage/$name"
cp "$binary" LICENSE README.md "$stage/$name/"
mkdir -p "$out"
tar -C "$stage" --owner=0 --group=0 --numeric-owner -czf "$out/$name.tar.gz" "$name"
(cd "$out" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256" && sha256sum --check --quiet "$name.tar.gz.sha256")

mkdir "$stage/unpacked"
tar -C "$stage/unpacked" -xzf "$out/$name.tar.gz"
reported=$("$stage/unpacked/$name/copper-dictate" --version)
if [ "$reported" != "copper-dictate $version" ]; then
    echo "package: the packed binary reports '$reported', expected 'copper-dictate $version'" >&2
    exit 1
fi
echo "package: $out/$name.tar.gz ($reported)"
