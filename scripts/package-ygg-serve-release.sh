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

if [[ ! "$version" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$ ]]; then
    printf 'version must be a canonical Ygg version tag such as v0.6.3: %s\n' "$version" >&2
    exit 2
fi

case "$target" in
    x86_64-unknown-linux-gnu|x86_64-apple-darwin|aarch64-apple-darwin) ;;
    *)
        printf 'unsupported ygg-serve release target: %s\n' "$target" >&2
        exit 2
        ;;
esac
for command in git python3; do
    if ! command -v "$command" >/dev/null 2>&1; then
        printf 'required release command is unavailable: %s\n' "$command" >&2
        exit 1
    fi
done

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

script_directory=$(cd "$(dirname "$0")" && pwd)
repository_directory=$(cd "$script_directory/.." && pwd)
if ! git -C "$repository_directory" diff-index --quiet HEAD --; then
    printf 'release source has tracked changes; package an immutable clean commit\n' >&2
    exit 1
fi
binary="$repository_directory/target/$target/release/ygg"
package_version=${version#v}
artifact_name="ygg-serve-${package_version}-${target}"
staging_directory=$(mktemp -d "${TMPDIR:-/tmp}/ygg-serve-release.XXXXXX")
package_directory="$staging_directory/ygg-serve"
trap 'rm -rf "$staging_directory"' EXIT

if [[ ! -f "$binary" || -L "$binary" || ! -x "$binary" ]]; then
    printf 'release binary is missing, linked, or not executable: %s\n' "$binary" >&2
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

source_date_epoch=${SOURCE_DATE_EPOCH:-$(git -C "$repository_directory" log -1 --format=%ct HEAD)}
case "$source_date_epoch" in
    ''|*[!0-9]*)
        printf 'SOURCE_DATE_EPOCH must be an unsigned integer: %s\n' "$source_date_epoch" >&2
        exit 1
        ;;
esac

archive="$output_directory/$artifact_name.tar.gz"
python3 - "$package_directory" "$archive" "$source_date_epoch" <<'PY'
import gzip
import pathlib
import stat
import sys
import tarfile

package = pathlib.Path(sys.argv[1])
archive = pathlib.Path(sys.argv[2])
epoch = int(sys.argv[3])
paths = [package, *sorted(package.rglob("*"), key=lambda path: path.relative_to(package).as_posix())]
with archive.open("wb") as raw:
    with gzip.GzipFile(filename="", mode="wb", fileobj=raw, compresslevel=9, mtime=epoch) as compressed:
        with tarfile.open(fileobj=compressed, mode="w", format=tarfile.GNU_FORMAT) as output:
            for path in paths:
                metadata = path.lstat()
                if not (stat.S_ISDIR(metadata.st_mode) or stat.S_ISREG(metadata.st_mode)):
                    raise SystemExit(f"release archive cannot contain links or special files: {path}")
                relative = path.relative_to(package)
                name = "ygg-serve" if not relative.parts else f"ygg-serve/{relative.as_posix()}"
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

expected = ["ygg-serve", "ygg-serve/bin", "ygg-serve/bin/ygg-serve-runtime", "ygg-serve/package.toml"]
with tarfile.open(archive, mode="r:gz") as packaged:
    members = packaged.getmembers()
    names = [member.name.rstrip("/") for member in members]
    if names != expected:
        raise SystemExit(f"release archive has an unexpected layout: {names!r}")
    if any(not (member.isdir() or member.isfile()) for member in members):
        raise SystemExit("release archive contains links or unexpected entry types")
PY
printf 'created %s\n' "$archive"
