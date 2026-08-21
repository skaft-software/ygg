#!/usr/bin/env bash
set -euo pipefail

# Rebuild the first-party worker bundle from this checkout and atomically install
# it into the same ~/.ygg/extensions location used by cargo run. This is
# intentionally independent of the release packaging clean-tree gate.
repository_directory=$(cd "$(dirname "$0")/.." && pwd)
version=$(python3 - "$repository_directory/Cargo.toml" <<'PY'
import sys
import tomllib
with open(sys.argv[1], "rb") as handle:
    print(tomllib.load(handle)["workspace"]["package"]["version"])
PY
)
staging_directory=$(mktemp -d "${TMPDIR:-/tmp}/ygg-subagents-reinstall.XXXXXX")
trap 'rm -rf "$staging_directory"' EXIT

"$repository_directory/scripts/package-ygg-extension-release.sh" \
    ygg-subagents "$staging_directory" "v$version" \
    "$repository_directory/extensions/ygg-subagents"

command=(extension install)
if [[ -f "${HOME}/.ygg/extensions/ygg-subagents/extension.toml" ]]; then
    command=(extension update)
fi
cargo run --quiet --manifest-path "$repository_directory/Cargo.toml" \
    -p ygg-coding-agent -- "${command[@]}" \
    --path "$staging_directory/ygg-subagents-$version.tar.gz"

printf 'Reinstalled ygg-subagents from %s\n' "$repository_directory/extensions/ygg-subagents"
