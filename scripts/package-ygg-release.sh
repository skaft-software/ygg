#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 4 ]]; then
    printf 'usage: %s TARGET OUTPUT_DIRECTORY VERSION SOURCE_DIRECTORY\n' "$0" >&2
    exit 2
fi

target=$1
output_directory=$2
version=$3
source_directory=$4

if [[ -z "$target" || -z "$version" || -z "$source_directory" ]]; then
    printf 'target, version, and source directory must not be empty\n' >&2
    exit 2
fi
if [[ ! "$version" =~ ^v[0-9]+\.[0-9]+\.[0-9]+-alpha[0-9A-Za-z.-]*$ ]]; then
    printf 'version must be an alpha release tag such as v0.3.3-alpha: %s\n' "$version" >&2
    exit 2
fi
case "$target" in
    x86_64-unknown-linux-gnu|x86_64-apple-darwin|aarch64-apple-darwin) ;;
    *)
        printf 'unsupported Ygg release target: %s\n' "$target" >&2
        exit 2
        ;;
esac

source_directory=$(cd "$source_directory" && pwd)
binary="$source_directory/target/$target/release/ygg"
license="$source_directory/LICENSE"
package_version=${version#v}
artifact_name="ygg-${package_version}-${target}"
staging_directory=$(mktemp -d "${TMPDIR:-/tmp}/ygg-release.XXXXXX")
package_directory="$staging_directory/$artifact_name"
trap 'rm -rf "$staging_directory"' EXIT

if [[ ! -x "$binary" ]]; then
    printf 'release binary not found: %s\n' "$binary" >&2
    printf 'build it with: cargo build --release --locked --target %s -p ygg-coding-agent --bin ygg\n' "$target" >&2
    exit 1
fi
if [[ ! -f "$license" || -L "$license" ]]; then
    printf 'release license is not a regular file: %s\n' "$license" >&2
    exit 1
fi

binary_version=$("$binary" --version)
if [[ "$binary_version" != "ygg $package_version" ]]; then
    printf 'binary version does not match release tag: %s (%s)\n' "$binary_version" "$version" >&2
    exit 1
fi

mkdir -p "$output_directory" "$package_directory"
cp "$binary" "$package_directory/ygg"
cp "$license" "$package_directory/LICENSE"
chmod 0755 "$package_directory/ygg"
chmod 0644 "$package_directory/LICENSE"

archive="$output_directory/$artifact_name.tar.gz"
COPYFILE_DISABLE=1 tar -C "$staging_directory" -czf "$archive" "$artifact_name"

entries="$staging_directory/archive-entries"
expected_entries="$staging_directory/expected-entries"
tar -tzf "$archive" | LC_ALL=C sort > "$entries"
printf '%s\n' \
    "$artifact_name/" \
    "$artifact_name/LICENSE" \
    "$artifact_name/ygg" \
    | LC_ALL=C sort > "$expected_entries"
if ! cmp -s "$expected_entries" "$entries"; then
    printf 'release archive has an unexpected layout: %s\n' "$archive" >&2
    diff -u "$expected_entries" "$entries" >&2 || true
    exit 1
fi

printf 'created %s\n' "$archive"
