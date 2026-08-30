#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 3 || $# -gt 4 ]]; then
    printf 'usage: %s ID OUTPUT_DIRECTORY VERSION [SOURCE_DIRECTORY]\n' "$0" >&2
    exit 2
fi

package_id=$1
output_directory=$2
version=$3
source_override=${4:-}

if [[ ! "$package_id" =~ ^[a-z][a-z0-9-]{0,63}$ ]]; then
    printf 'invalid extension bundle ID: %s\n' "$package_id" >&2
    exit 2
fi
if [[ ! "$version" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$ ]]; then
    printf 'version must be a canonical Ygg version tag such as v0.6.5: %s\n' "$version" >&2
    exit 2
fi
for command in git python3; do
    if ! command -v "$command" >/dev/null 2>&1; then
        printf 'required release command is unavailable: %s\n' "$command" >&2
        exit 1
    fi
done

script_directory=$(cd "$(dirname "$0")" && pwd)
repository_directory=$(cd "$script_directory/.." && pwd)
package_version=${version#v}
tracked_manifest=

if [[ -z "$source_override" ]]; then
    if ! grep -Ev '^[[:space:]]*(#|$)' "$repository_directory/extensions/release-catalog.txt" \
        | grep -Fx "$package_id" >/dev/null; then
        printf 'extension is not in the official release catalog: %s\n' "$package_id" >&2
        exit 2
    fi
    if ! git -C "$repository_directory" diff-index --quiet HEAD --; then
        printf 'release source has tracked changes; package an immutable clean commit\n' >&2
        exit 1
    fi
    source_directory="$repository_directory/extensions/$package_id"
    tracked_manifest=$(mktemp "${TMPDIR:-/tmp}/ygg-extension-files.XXXXXX")
    trap 'rm -f "$tracked_manifest"' EXIT
    git -C "$repository_directory" ls-files -z -- "extensions/$package_id" >"$tracked_manifest"
    if [[ ! -s "$tracked_manifest" ]]; then
        printf 'official extension has no tracked source files: %s\n' "$package_id" >&2
        exit 1
    fi
else
    source_directory=$(cd "$source_override" 2>/dev/null && pwd) || {
        printf 'cannot resolve extension source directory: %s\n' "$source_override" >&2
        exit 1
    }
fi

if [[ ! -d "$source_directory" || -L "$source_directory" ]]; then
    printf 'extension source is missing, linked, or not a directory: %s\n' "$source_directory" >&2
    exit 1
fi
if [[ ! -f "$source_directory/extension.toml" || -L "$source_directory/extension.toml" ]]; then
    printf 'extension source has no regular extension.toml: %s\n' "$source_directory" >&2
    exit 1
fi
if [[ -e "$source_directory/package.toml" || -e "$source_directory/install.json" ]]; then
    printf 'executable-extension sources cannot contain application/install manifests\n' >&2
    exit 1
fi

source_date_epoch=${SOURCE_DATE_EPOCH:-$(git -C "$repository_directory" log -1 --format=%ct HEAD)}
case "$source_date_epoch" in
    ''|*[!0-9]*)
        printf 'SOURCE_DATE_EPOCH must be an unsigned integer: %s\n' "$source_date_epoch" >&2
        exit 1
        ;;
esac

mkdir -p "$output_directory"
archive="$output_directory/$package_id-$package_version.tar.gz"
python3 - \
    "$repository_directory" \
    "$source_directory" \
    "$package_id" \
    "$package_version" \
    "$archive" \
    "$source_date_epoch" \
    "$tracked_manifest" <<'PY'
import gzip
import os
import pathlib
import re
import stat
import sys
import tarfile
import tomllib

repository, source, package_id, ygg_version, archive, epoch, tracked_manifest = sys.argv[1:8]
repository = pathlib.Path(repository)
source = pathlib.Path(source)
archive = pathlib.Path(archive)
epoch = int(epoch)

with (source / "extension.toml").open("rb") as handle:
    manifest = tomllib.load(handle)
for field in ("name", "version", "api_version", "requires_ygg", "entrypoint"):
    if field not in manifest:
        raise SystemExit(f"extension.toml is missing {field}")
if manifest["name"] != package_id:
    raise SystemExit(
        f"extension.toml name {manifest['name']!r} does not match {package_id!r}"
    )
if not isinstance(manifest["version"], str) or re.fullmatch(
    r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)(?:[-+][0-9A-Za-z.-]+)?",
    manifest["version"],
) is None:
    raise SystemExit("extension.toml version is not semantic versioning")
if manifest["api_version"] != "0.2":
    raise SystemExit("release bundles must declare api_version = '0.2'")
expected_ygg = f"={ygg_version}"
if manifest["requires_ygg"] != expected_ygg:
    raise SystemExit(
        f"release bundle must declare requires_ygg = {expected_ygg!r}"
    )
entrypoint = manifest["entrypoint"]
if not isinstance(entrypoint, dict) or not isinstance(entrypoint.get("command"), str):
    raise SystemExit("extension.toml entrypoint.command must be a string")
command = pathlib.PurePosixPath(entrypoint["command"])
if not command.is_absolute() and (not command.parts or ".." in command.parts or "." in command.parts):
    raise SystemExit("relative entrypoint.command is not portable")
if not command.is_absolute():
    local_command = source.joinpath(*command.parts)
    if local_command.exists():
        command_metadata = local_command.lstat()
        if not stat.S_ISREG(command_metadata.st_mode) or not command_metadata.st_mode & 0o111:
            raise SystemExit("local entrypoint.command must be a regular executable file")
    elif len(command.parts) > 1:
        raise SystemExit("relative entrypoint.command is missing from the bundle")

if tracked_manifest:
    raw = pathlib.Path(tracked_manifest).read_bytes().split(b"\0")
    files = []
    prefix = pathlib.PurePosixPath("extensions") / package_id
    for encoded in raw:
        if not encoded:
            continue
        try:
            relative_to_repo = pathlib.PurePosixPath(encoded.decode("utf-8"))
        except UnicodeDecodeError as error:
            raise SystemExit("tracked extension path is not UTF-8") from error
        try:
            relative = relative_to_repo.relative_to(prefix)
        except ValueError as error:
            raise SystemExit(f"tracked path escaped extension root: {relative_to_repo}") from error
        files.append(source.joinpath(*relative.parts))
else:
    files = [path for path in source.rglob("*") if not path.is_dir()]

files = [
    path
    for path in files
    if "__pycache__" not in path.relative_to(source).parts
    and path.suffix not in {".pyc", ".pyo"}
]

if not files:
    raise SystemExit("extension source has no files")
relative_files = []
for path in files:
    metadata = path.lstat()
    if not stat.S_ISREG(metadata.st_mode):
        raise SystemExit(f"release bundle cannot contain links or special files: {path}")
    try:
        relative = path.relative_to(source)
    except ValueError as error:
        raise SystemExit(f"extension file escaped source root: {path}") from error
    if not relative.parts or any(part in ("", ".", "..") for part in relative.parts):
        raise SystemExit(f"extension path is not portable: {relative}")
    relative_text = relative.as_posix()
    relative_text.encode("utf-8")
    if any(ord(character) < 32 or ord(character) == 127 for character in relative_text):
        raise SystemExit(f"extension path contains a control character: {relative!r}")
    relative_files.append((relative, path, metadata))

relative_files.sort(key=lambda item: item[0].as_posix())
if [relative.as_posix() for relative, _, _ in relative_files].count("extension.toml") != 1:
    raise SystemExit("release bundle must contain exactly one extension.toml")

directories = {pathlib.PurePosixPath()}
for relative, _, _ in relative_files:
    parent = pathlib.PurePosixPath(relative.as_posix()).parent
    while parent != pathlib.PurePosixPath("."):
        directories.add(parent)
        parent = parent.parent

with archive.open("wb") as raw:
    with gzip.GzipFile(filename="", mode="wb", fileobj=raw, compresslevel=9, mtime=epoch) as compressed:
        with tarfile.open(fileobj=compressed, mode="w", format=tarfile.GNU_FORMAT) as output:
            root_info = tarfile.TarInfo(package_id)
            root_info.type = tarfile.DIRTYPE
            root_info.mode = 0o755
            root_info.mtime = epoch
            root_info.uid = root_info.gid = 0
            root_info.uname = root_info.gname = ""
            output.addfile(root_info)

            for relative in sorted(directories, key=lambda path: path.as_posix()):
                if not relative.parts:
                    continue
                info = tarfile.TarInfo(f"{package_id}/{relative.as_posix()}")
                info.type = tarfile.DIRTYPE
                info.mode = 0o755
                info.mtime = epoch
                info.uid = info.gid = 0
                info.uname = info.gname = ""
                output.addfile(info)

            for relative, path, metadata in relative_files:
                info = tarfile.TarInfo(f"{package_id}/{relative.as_posix()}")
                info.size = metadata.st_size
                info.mode = 0o755 if metadata.st_mode & 0o111 else 0o644
                info.mtime = epoch
                info.uid = info.gid = 0
                info.uname = info.gname = ""
                with path.open("rb") as contents:
                    output.addfile(info, contents)

with tarfile.open(archive, mode="r:gz") as packaged:
    members = packaged.getmembers()
    if not members or members[0].name.rstrip("/") != package_id or not members[0].isdir():
        raise SystemExit("release bundle has no canonical root directory")
    if any(not (member.isdir() or member.isfile()) for member in members):
        raise SystemExit("release bundle contains a link or special entry")
    names = [member.name.rstrip("/") for member in members]
    if names.count(f"{package_id}/extension.toml") != 1:
        raise SystemExit("release bundle has no unique extension.toml")
PY
printf 'created %s\n' "$archive"
