#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage:
  scripts/build-ygg-image.sh [IMAGE]
  scripts/build-ygg-image.sh --context OUTPUT_DIRECTORY

Build the pinned linux/amd64 Ygg image (default: ygg:0.6.2), or materialize
the exact clean tracked Docker context for inspection.
EOF
}

mode=build
image=ygg:0.6.2
output_directory=
case $# in
    0) ;;
    1)
        case "$1" in
            -h|--help)
                usage
                exit 0
                ;;
            --context)
                usage >&2
                exit 2
                ;;
            *) image=$1 ;;
        esac
        ;;
    2)
        if [[ "$1" != --context ]]; then
            usage >&2
            exit 2
        fi
        mode=context
        output_directory=$2
        ;;
    *)
        usage >&2
        exit 2
        ;;
esac

for command in git python3 tar; do
    if ! command -v "$command" >/dev/null 2>&1; then
        printf 'required container-build command is unavailable: %s\n' "$command" >&2
        exit 1
    fi
done
if [[ "$mode" == build ]] && ! command -v docker >/dev/null 2>&1; then
    printf 'Docker is required to build the Ygg image\n' >&2
    exit 1
fi

script_directory=$(cd "$(dirname "$0")" && pwd)
repository_directory=$(cd "$script_directory/.." && pwd)
if ! git -C "$repository_directory" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    printf 'Ygg container source must be a Git checkout\n' >&2
    exit 1
fi
git -C "$repository_directory" update-index -q --refresh
if ! git -C "$repository_directory" diff-index --quiet HEAD --; then
    printf 'Ygg container source has tracked changes; build an immutable clean commit\n' >&2
    exit 1
fi
source_commit=$(git -C "$repository_directory" rev-parse 'HEAD^{commit}')
if [[ ! "$source_commit" =~ ^[0-9a-f]{40}$ ]]; then
    printf 'could not resolve an immutable Ygg source commit\n' >&2
    exit 1
fi

index_manifest=$(mktemp "${TMPDIR:-/tmp}/ygg-container-index.XXXXXX")
temporary_context=
cleanup() {
    rm -f "$index_manifest"
    if [[ -n "$temporary_context" ]]; then
        rm -rf "$temporary_context"
    fi
}
trap cleanup EXIT HUP INT TERM

git -C "$repository_directory" ls-files -s -z > "$index_manifest"
python3 - "$index_manifest" <<'PY'
import pathlib
import sys

entries = pathlib.Path(sys.argv[1]).read_bytes().split(b"\0")
for entry in entries:
    if not entry:
        continue
    metadata, separator, path = entry.partition(b"\t")
    fields = metadata.split()
    if not separator or len(fields) != 3 or fields[0] not in {b"100644", b"100755"}:
        display = path.decode("utf-8", "replace") if path else "<unknown>"
        raise SystemExit(f"container context rejects links, submodules, or special entries: {display}")
PY

if [[ "$mode" == context ]]; then
    if [[ -e "$output_directory" || -L "$output_directory" ]]; then
        printf 'container context output already exists: %s\n' "$output_directory" >&2
        exit 1
    fi
    mkdir -m 0700 "$output_directory"
    context=$output_directory
else
    temporary_context=$(mktemp -d "${TMPDIR:-/tmp}/ygg-container-context.XXXXXX")
    chmod 0700 "$temporary_context"
    context=$temporary_context
fi

git -C "$repository_directory" archive --format=tar "$source_commit" \
    | tar -xf - -C "$context"
printf '%s\n' "$source_commit" > "$context/.ygg-container-source"
chmod 0644 "$context/.ygg-container-source"

if [[ "$mode" == context ]]; then
    printf 'created clean Ygg container context at %s (%s)\n' "$context" "$source_commit"
else
    docker build \
        --file "$context/deploy/Dockerfile.ygg" \
        --tag "$image" \
        "$context"
fi
