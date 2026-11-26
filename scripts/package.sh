#!/usr/bin/env bash
# Pack the release binary, LICENSE, README.md and the third-party notices into
# copper-dictate-<tag>-x86_64-linux.tar.gz and its .sha256 under <out-dir>, then
# check the pair: the checksum verifies, and the binary unpacked from the
# archive reports the version Cargo.toml holds.
#
#   scripts/package.sh <out-dir> [<tag>]
#
# <tag> defaults to v<version>. A tag given must be exactly that: the release
# names its assets after the tag release-please made, and a tag that disagrees
# with Cargo.toml means the archive would carry a binary of another version.
set -euo pipefail

if [[ $# -lt 1 ]] || [[ $# -gt 2 ]]; then
    echo "usage: $0 <out-dir> [<tag>]" >&2
    exit 2
fi
out=$1
pkgid=$(cargo pkgid --quiet)
version=${pkgid##*[#@]}
tag=${2:-v$version}
if [[ "$tag" != "v$version" ]]; then
    echo "package: tag $tag does not match Cargo.toml's version $version" >&2
    exit 1
fi

triple=x86_64-unknown-linux-gnu
binary=target/$triple/release/copper-dictate
if [[ ! -x "$binary" ]]; then
    echo "package: $binary is missing; build it with cargo build --release --locked --target $triple first" >&2
    exit 1
fi

name=copper-dictate-$tag-x86_64-linux
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

mkdir "$stage/$name"
cp "$binary" LICENSE README.md "$stage/$name/"
if ! cargo about generate --locked --fail about.hbs -o "$stage/$name/THIRD-PARTY-NOTICES.txt" ||
    ! [[ -s "$stage/$name/THIRD-PARTY-NOTICES.txt" ]]; then
    echo "package: cargo about could not write the third-party notices" >&2
    exit 1
fi
mkdir -p "$out"
tar -C "$stage" --owner=0 --group=0 --numeric-owner -czf "$out/$name.tar.gz" "$name"
(cd "$out" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256" && sha256sum --check --quiet "$name.tar.gz.sha256")

mkdir "$stage/unpacked"
tar -C "$stage/unpacked" -xzf "$out/$name.tar.gz"
for file in copper-dictate LICENSE README.md THIRD-PARTY-NOTICES.txt; do
    if ! [[ -s "$stage/unpacked/$name/$file" ]]; then
        echo "package: the archive has no $file" >&2
        exit 1
    fi
done
reported=$("$stage/unpacked/$name/copper-dictate" --version)
if [[ "$reported" != "copper-dictate $version" ]]; then
    echo "package: the packed binary reports '$reported', expected 'copper-dictate $version'" >&2
    exit 1
fi
echo "package: $out/$name.tar.gz ($reported)"
