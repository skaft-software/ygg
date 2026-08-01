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

case "$target" in
    x86_64-unknown-linux-gnu|x86_64-apple-darwin|aarch64-apple-darwin) ;;
    *)
        printf 'unsupported ygg-serve release target: %s\n' "$target" >&2
        exit 2
        ;;
esac

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

script_directory=$(cd "$(dirname "$0")" && pwd)
repository_directory=$(cd "$script_directory/.." && pwd)
binary="$repository_directory/target/$target/release/ygg"
package_version=${version#v}
artifact_name="ygg-serve-${package_version}-${target}"
staging_directory=$(mktemp -d "${TMPDIR:-/tmp}/ygg-serve-release.XXXXXX")
package_directory="$staging_directory/ygg-serve"
trap 'rm -rf "$staging_directory"' EXIT

if [[ ! -x "$binary" ]]; then
    printf 'release binary not found: %s\n' "$binary" >&2
    printf 'build it with: cargo build --release --locked --target %s -p ygg-coding-agent --features serve\n' "$target" >&2
    exit 1
fi

binary_version=$("$binary" --version)
if [[ "$binary_version" != "ygg $package_version" ]]; then
    printf 'binary version does not match release tag: %s (%s)\n' "$binary_version" "$version" >&2
    exit 1
fi

mkdir -p "$output_directory" "$package_directory/bin"
cp "$binary" "$package_directory/bin/ygg-serve-runtime"
chmod 0755 "$package_directory/bin/ygg-serve-runtime"
binary_sha256=$(sha256_file "$package_directory/bin/ygg-serve-runtime")

cat >"$package_directory/package.toml" <<EOF
schema_version = 1
id = "ygg-serve"
version = "$package_version"
requires_ygg = "=$package_version"
target = "$target"

[entrypoint]
path = "bin/ygg-serve-runtime"
args = ["serve"]
sha256 = "$binary_sha256"

[capabilities]
network = "loopback"
process = true
filesystem = "workspace"
EOF

archive="$output_directory/$artifact_name.tar.gz"
COPYFILE_DISABLE=1 tar -C "$staging_directory" -czf "$archive" ygg-serve
printf 'created %s\n' "$archive"
