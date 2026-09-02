#!/usr/bin/env bash
set -euo pipefail

usage() {
    printf 'usage: %s\n' "$0" >&2
}
if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then
    usage
    exit 0
fi
if [[ $# -ne 0 ]]; then
    usage
    exit 2
fi

repository_directory=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
script_directory="$repository_directory/scripts"
fixture_directory="$script_directory/fixtures/homebrew"
generator="$script_directory/generate-homebrew-formula.py"
for command in python3 ruby; do
    command -v "$command" >/dev/null 2>&1 || {
        printf 'required Homebrew formula test command is unavailable: %s\n' "$command" >&2
        exit 1
    }
done
for path in \
    "$generator" \
    "$fixture_directory/YGG_RELEASE_METADATA.json" \
    "$fixture_directory/expected-ygg.rb" \
    "$fixture_directory/assets/YGG_SHA256SUMS"; do
    [[ -f "$path" && ! -L "$path" ]] || {
        printf 'Homebrew formula fixture is missing or linked: %s\n' "$path" >&2
        exit 1
    }
done

work_directory=$(mktemp -d "${TMPDIR:-/tmp}/ygg-homebrew-test.XXXXXX")
trap 'rm -rf "$work_directory"' EXIT
formula="$work_directory/ygg.rb"
repeat="$work_directory/ygg-repeat.rb"

python3 "$generator" \
    "$fixture_directory/YGG_RELEASE_METADATA.json" \
    --assets-dir "$fixture_directory/assets" \
    --output "$formula"
python3 "$generator" \
    --metadata "$fixture_directory/YGG_RELEASE_METADATA.json" \
    --assets-dir "$fixture_directory/assets" \
    --output "$repeat"
cmp "$fixture_directory/expected-ygg.rb" "$formula"
cmp "$formula" "$repeat"
ruby -c "$formula"

python3 - "$formula" <<'PY'
import pathlib
import sys

formula = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")
required = (
    'class Ygg < Formula',
    'on_arm do',
    'on_intel do',
    'depends_on :macos',
    'depends_on "ripgrep"',
    'bin.install File.join(root, "ygg")',
    'bin.install File.join(root, "ygg-host")',
    'sha256 "',
)
for marker in required:
    if marker not in formula:
        raise SystemExit(f"formula is missing required Homebrew contract: {marker}")
if "Cargo.toml" in formula or "api.github.com" in formula:
    raise SystemExit("formula contains a mutable release source")
PY

# A metadata digest or local asset mismatch must stop formula generation before
# it can produce a formula that points at a different native release.
cp "$fixture_directory/YGG_RELEASE_METADATA.json" "$work_directory/bad.json"
python3 - "$work_directory/bad.json" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
value = json.loads(path.read_text(encoding="utf-8"))
value["assets"][1]["sha256"] = "0" * 64
path.write_text(json.dumps(value, sort_keys=True), encoding="utf-8")
PY
if python3 "$generator" "$work_directory/bad.json" --assets-dir "$fixture_directory/assets" --output "$work_directory/bad.rb"; then
    echo "formula generator accepted a mismatched immutable asset" >&2
    exit 1
fi

if rg -n 'Cargo\.toml|api\.github\.com|releases/latest|gh release|curl ' "$generator"; then
    echo "formula generator contains a mutable release lookup" >&2
    exit 1
fi

printf 'Homebrew formula generation and offline validation passed\n'
