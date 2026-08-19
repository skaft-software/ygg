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
if [[ ! "$version" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    printf 'version must be a canonical release tag such as v0.5.0: %s\n' "$version" >&2
    exit 2
fi
case "$target" in
    x86_64-unknown-linux-gnu|x86_64-apple-darwin|aarch64-apple-darwin) ;;
    *)
        printf 'unsupported Ygg release target: %s\n' "$target" >&2
        exit 2
        ;;
esac
for command in git python3; do
    if ! command -v "$command" >/dev/null 2>&1; then
        printf 'required release command is unavailable: %s\n' "$command" >&2
        exit 1
    fi
done

source_directory=$(cd "$source_directory" && pwd)
if ! git -C "$source_directory" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    printf 'release source must be a Git checkout: %s\n' "$source_directory" >&2
    exit 1
fi
if ! git -C "$source_directory" diff-index --quiet HEAD --; then
    printf 'release source has tracked changes; package an immutable clean commit\n' >&2
    exit 1
fi
binary="$source_directory/target/$target/release/ygg"
host_binary="$source_directory/target/$target/release/ygg-host"
package_version=${version#v}
artifact_name="ygg-${package_version}-${target}"
staging_directory=$(mktemp -d "${TMPDIR:-/tmp}/ygg-release.XXXXXX")
package_directory="$staging_directory/$artifact_name"
asset_manifest="$staging_directory/tracked-assets"
trap 'rm -rf "$staging_directory"' EXIT

for required_binary in "$binary" "$host_binary"; do
    if [[ ! -f "$required_binary" || -L "$required_binary" || ! -x "$required_binary" ]]; then
        printf 'release binary is missing, linked, or not executable: %s\n' "$required_binary" >&2
        printf 'build both with: cargo build --release --locked --target %s -p ygg-coding-agent --bins\n' "$target" >&2
        exit 1
    fi
done

binary_version=$("$binary" --version)
if [[ "$binary_version" != "ygg $package_version" ]]; then
    printf 'binary version does not match release tag: %s (%s)\n' "$binary_version" "$version" >&2
    exit 1
fi
printf '%s\n' '{"protocol_version":1,"request_id":"release-probe","command":"hello"}' \
    | "$host_binary" > "$staging_directory/host-hello"
python3 - "$staging_directory/host-hello" "$package_version" <<'PY'
import json
import pathlib
import sys

lines = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8").splitlines()
if len(lines) != 1:
    raise SystemExit("ygg-host release probe did not emit exactly one frame")
message = json.loads(lines[0])
if (
    message.get("protocol_version") != 1
    or message.get("request_id") != "release-probe"
    or message.get("seq") != 1
    or message.get("type") != "hello"
    or message.get("data", {}).get("sdk_version") != sys.argv[2]
):
    raise SystemExit("ygg-host release probe returned an invalid handshake")
PY

mkdir -p "$output_directory" "$package_directory"
cp "$binary" "$package_directory/ygg"
cp "$host_binary" "$package_directory/ygg-host"
chmod 0755 "$package_directory/ygg" "$package_directory/ygg-host"

# Copy only paths known to Git under the public package roots. This excludes
# ignored and untracked workstation files regardless of their location.
git -C "$source_directory" ls-files -z -- LICENSE README.md docs examples sdk > "$asset_manifest"
python3 - "$source_directory" "$package_directory" "$asset_manifest" <<'PY'
import os
import pathlib
import shutil
import stat
import sys

source = pathlib.Path(sys.argv[1])
package = pathlib.Path(sys.argv[2])
entries = pathlib.Path(sys.argv[3]).read_bytes().split(b"\0")
allowed_roots = {"LICENSE", "README.md", "docs", "examples", "sdk"}
copied = set()
for raw in entries:
    if not raw:
        continue
    relative = pathlib.Path(os.fsdecode(raw))
    if relative.is_absolute() or ".." in relative.parts or not relative.parts:
        raise SystemExit(f"unsafe tracked release path: {relative}")
    if relative.parts[0] not in allowed_roots:
        raise SystemExit(f"unexpected tracked release path: {relative}")
    source_path = source / relative
    metadata = source_path.lstat()
    if not stat.S_ISREG(metadata.st_mode):
        raise SystemExit(f"release assets must be regular files: {relative}")
    destination = package / relative
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source_path, destination)
    destination.chmod(0o755 if metadata.st_mode & 0o111 else 0o644)
    copied.add(relative.as_posix())

for required_file in ("LICENSE", "README.md"):
    if required_file not in copied:
        raise SystemExit(f"tracked {required_file} is missing from release assets")
for required in ("docs", "examples", "sdk"):
    if not any(path.startswith(required + "/") for path in copied):
        raise SystemExit(f"tracked {required}/ assets are missing from the release package")
PY

source_date_epoch=${SOURCE_DATE_EPOCH:-$(git -C "$source_directory" log -1 --format=%ct HEAD)}
case "$source_date_epoch" in
    ''|*[!0-9]*)
        printf 'SOURCE_DATE_EPOCH must be an unsigned integer: %s\n' "$source_date_epoch" >&2
        exit 1
        ;;
esac
archive="$output_directory/$artifact_name.tar.gz"
python3 - "$package_directory" "$archive" "$artifact_name" "$source_date_epoch" <<'PY'
import gzip
import pathlib
import stat
import sys
import tarfile

package = pathlib.Path(sys.argv[1])
archive = pathlib.Path(sys.argv[2])
artifact_name = sys.argv[3]
epoch = int(sys.argv[4])
paths = [package, *sorted(package.rglob("*"), key=lambda path: path.relative_to(package).as_posix())]
with archive.open("wb") as raw:
    with gzip.GzipFile(filename="", mode="wb", fileobj=raw, compresslevel=9, mtime=epoch) as compressed:
        with tarfile.open(fileobj=compressed, mode="w", format=tarfile.GNU_FORMAT) as output:
            for path in paths:
                metadata = path.lstat()
                if not (stat.S_ISDIR(metadata.st_mode) or stat.S_ISREG(metadata.st_mode)):
                    raise SystemExit(f"release archive cannot contain links or special files: {path}")
                relative = path.relative_to(package)
                name = artifact_name if not relative.parts else f"{artifact_name}/{relative.as_posix()}"
                info = output.gettarinfo(str(path), arcname=name)
                info.uid = 0
                info.gid = 0
                info.uname = ""
                info.gname = ""
                info.mtime = epoch
                info.mode = 0o755 if info.isdir() or metadata.st_mode & 0o111 else 0o644
                if info.isfile():
                    with path.open("rb") as contents:
                        output.addfile(info, contents)
                else:
                    output.addfile(info)
PY

entries="$staging_directory/archive-entries"
expected_entries="$staging_directory/expected-entries"
tar -tzf "$archive" | sed 's#/$##' | LC_ALL=C sort > "$entries"
(
    cd "$staging_directory"
    find "$artifact_name" -print | LC_ALL=C sort
) > "$expected_entries"
if ! cmp -s "$expected_entries" "$entries"; then
    printf 'release archive has an unexpected layout: %s\n' "$archive" >&2
    diff -u "$expected_entries" "$entries" >&2 || true
    exit 1
fi
if ! tar -tvzf "$archive" | awk '
    { type = substr($1, 1, 1); if (type != "-" && type != "d") exit 1 }
'; then
    printf 'release archive contains links or unexpected entry types\n' >&2
    exit 1
fi

printf 'created %s\n' "$archive"
