#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 3 ]]; then
    printf 'usage: %s TARGET OUTPUT_DIRECTORY VERSION\n' "$0" >&2
    exit 2
fi

target=$1
output_directory=$2
version=$3

if [[ -z "$target" || -z "$version" ]]; then
    printf 'target and version must not be empty\n' >&2
    exit 2
fi

if [[ ! "$version" =~ ^v[0-9]+\.[0-9]+\.[0-9]+-alpha[0-9A-Za-z.-]*$ ]]; then
    printf 'version must be an alpha release tag such as v0.3.2-alpha: %s\n' "$version" >&2
    exit 2
fi

script_directory=$(cd "$(dirname "$0")" && pwd)
repository_directory=$(cd "$script_directory/.." && pwd)
binary="$repository_directory/target/$target/release/ygg"
artifact_name="ygg-${version#v}-${target}"
staging_directory=$(mktemp -d "${TMPDIR:-/tmp}/ygg-release.XXXXXX")
trap 'rm -rf "$staging_directory"' EXIT

if [[ ! -x "$binary" ]]; then
    printf 'release binary not found: %s\n' "$binary" >&2
    printf 'build it with: cargo build --release --locked --target %s -p ygg-coding-agent --features serve\n' "$target" >&2
    exit 1
fi

binary_version=$("$binary" --version)
if [[ "$binary_version" != *" ${version#v}"* ]]; then
    printf 'binary version does not match release tag: %s (%s)\n' "$binary_version" "$version" >&2
    exit 1
fi

mkdir -p "$output_directory"
mkdir "$staging_directory/$artifact_name"
cp "$binary" "$staging_directory/$artifact_name/ygg"
cat >"$staging_directory/$artifact_name/RELEASE.txt" <<EOF
Ygg ${version} (${target})

This artifact was built with the optional 'serve' feature. The default Ygg
build remains feature-disabled; 'ygg serve' is a local loopback-only surface.

Binary version:
$binary_version
EOF

tar -C "$staging_directory" -czf "$output_directory/$artifact_name.tar.gz" "$artifact_name"
printf 'created %s\n' "$output_directory/$artifact_name.tar.gz"
