#!/usr/bin/env bash
set -euo pipefail

usage() {
    printf 'usage: %s VERSION NATIVE_ARCHIVE_DIRECTORY OUTPUT_DIRECTORY YGG_SHA256SUMS\n' "$0" >&2
    printf '\nPackage the three existing GitHub native release archives into four npm tarballs.\n' >&2
}

if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then
    usage >&2
    exit 0
fi
if [[ $# -ne 4 ]]; then
    usage
    exit 2
fi

version=$1
archive_directory=$2
output_directory=$3
checksums=$4
script_directory=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
template_directory="$script_directory/../packages/npm"

if [[ "$version" == v* ]]; then
    version=${version#v}
fi
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z-]+)*$ ]]; then
    printf 'version must be a canonical npm release version: %s\n' "$version" >&2
    exit 2
fi
for command in python3 npm; do
    if ! command -v "$command" >/dev/null 2>&1; then
        printf 'required npm packaging command is unavailable: %s\n' "$command" >&2
        exit 1
    fi
done
if [[ ! -d "$archive_directory" || -L "$archive_directory" ]]; then
    printf 'native archive directory must be a real directory: %s\n' "$archive_directory" >&2
    exit 1
fi
if [[ ! -f "$checksums" || -L "$checksums" ]]; then
    printf 'signed native release checksum metadata is missing: %s\n' "$checksums" >&2
    exit 1
fi
if [[ -e "$output_directory" && -L "$output_directory" ]]; then
    printf 'npm output directory must not be a symlink: %s\n' "$output_directory" >&2
    exit 1
fi
mkdir -p "$output_directory"
if [[ -n "$(find "$output_directory" -mindepth 1 -maxdepth 1 -print -quit)" ]]; then
    printf 'npm output directory must be empty; refusing to overwrite existing artifacts: %s\n' "$output_directory" >&2
    exit 1
fi

source_date_epoch=${SOURCE_DATE_EPOCH:-0}
if [[ ! "$source_date_epoch" =~ ^[0-9]+$ ]]; then
    printf 'SOURCE_DATE_EPOCH must be an unsigned integer: %s\n' "$source_date_epoch" >&2
    exit 1
fi

work_directory=$(mktemp -d "${TMPDIR:-/tmp}/ygg-npm.XXXXXX")
trap 'rm -rf "$work_directory"' EXIT
native_directory="$work_directory/native"
staging_directory="$work_directory/staged"
mkdir -p "$native_directory" "$staging_directory"

# This helper deliberately accepts only the already-produced native archives
# and their existing checksum asset. It never builds from a mutable checkout or
# downloads a replacement binary.
python3 - \
    "$archive_directory" \
    "$checksums" \
    "$version" \
    "$native_directory" \
    "$staging_directory" \
    "$template_directory" \
    "$source_date_epoch" <<'PY'
import hashlib
import os
import pathlib
import re
import shutil
import stat
import sys
import tarfile
import unicodedata

archive_directory = pathlib.Path(sys.argv[1])
checksums_path = pathlib.Path(sys.argv[2])
version = sys.argv[3]
native_directory = pathlib.Path(sys.argv[4])
staging_directory = pathlib.Path(sys.argv[5])
template_directory = pathlib.Path(sys.argv[6])
epoch = int(sys.argv[7])

MAX_ARCHIVE_BYTES = 128 * 1024 * 1024
MAX_EXPANDED_BYTES = 160 * 1024 * 1024
MAX_MEMBER_BYTES = 64 * 1024 * 1024
MAX_ENTRIES = 4096
WINDOWS_FORBIDDEN = set('<>:"|?*')
WINDOWS_DEVICES = {
    "CON",
    "PRN",
    "AUX",
    "NUL",
    *(f"COM{number}" for number in range(1, 10)),
    *(f"LPT{number}" for number in range(1, 10)),
}
TARGETS = {
    "aarch64-apple-darwin": (
        "@skaft-software/ygg-darwin-arm64",
        "darwin",
        "arm64",
        "ygg-darwin-arm64-" + version + ".tgz",
    ),
    "x86_64-apple-darwin": (
        "@skaft-software/ygg-darwin-x64",
        "darwin",
        "x64",
        "ygg-darwin-x64-" + version + ".tgz",
    ),
    "x86_64-unknown-linux-gnu": (
        "@skaft-software/ygg-linux-x64-gnu",
        "linux",
        "x64",
        "ygg-linux-x64-gnu-" + version + ".tgz",
    ),
}


def fail(message):
    raise SystemExit(message)


def regular_file(path, label):
    try:
        metadata = path.lstat()
    except FileNotFoundError:
        fail(f"{label} is missing: {path}")
    if not stat.S_ISREG(metadata.st_mode):
        fail(f"{label} must be a regular file: {path}")
    return metadata


def parse_checksums():
    pattern = re.compile(r"^([0-9a-f]{64})  \./([A-Za-z0-9_.-]+)$")
    entries = {}
    lines = checksums_path.read_text(encoding="ascii").splitlines()
    if not lines:
        fail("native release checksum metadata is empty")
    for line in lines:
        match = pattern.fullmatch(line)
        if match is None:
            fail("native release checksum metadata is malformed")
        name, digest = match.group(2), match.group(1)
        if name in entries:
            fail(f"native release checksum metadata repeats {name}")
        entries[name] = digest
    expected = {f"ygg-{version}-{target}.tar.gz" for target in TARGETS}
    if set(entries) not in (expected, expected | {"install-ygg.sh"}):
        fail("native release checksum metadata does not contain exactly the three target archives")
    return entries


def validate_archive(target, archive, expected_digest):
    metadata = regular_file(archive, "native release archive")
    if metadata.st_size > MAX_ARCHIVE_BYTES:
        fail(f"native release archive exceeds {MAX_ARCHIVE_BYTES} bytes: {archive}")
    digest = hashlib.sha256()
    with archive.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    if digest.hexdigest() != expected_digest:
        fail(f"native release archive checksum does not match signed metadata: {archive.name}")

    root_name = f"ygg-{version}-{target}"
    destination = native_directory / target
    destination.mkdir(parents=True, exist_ok=False)
    seen = set()
    expanded = 0
    required = {"ygg", "ygg-host", "LICENSE", "README.md"}
    with tarfile.open(archive, mode="r:gz") as source:
        members = source.getmembers()
        if len(members) > MAX_ENTRIES:
            fail(f"native release archive has too many entries: {archive.name}")
        for member in members:
            name = member.name
            if "\\" in name or name.startswith("/"):
                fail(f"native release archive has an unsafe path: {name}")
            parts = pathlib.PurePosixPath(name).parts
            if not parts or parts[0] != root_name or any(part in ("", ".", "..") for part in parts):
                fail(f"native release archive has an unsafe path: {name}")
            if name in seen:
                fail(f"native release archive repeats an entry: {name}")
            seen.add(name)
            relative = "/".join(parts[1:])
            if relative and not (
                relative in required
                or relative in {"docs", "examples", "sdk"}
                or relative.startswith("docs/")
                or relative.startswith("examples/")
                or relative.startswith("sdk/")
            ):
                fail(f"native release archive has an unexpected member: {name}")
            if member.size > MAX_MEMBER_BYTES:
                fail(f"native release archive member exceeds {MAX_MEMBER_BYTES} bytes: {name}")
            if not (member.isdir() or member.isreg()):
                fail(f"native release archive contains a link or special file: {name}")
            expanded += member.size
            if expanded > MAX_EXPANDED_BYTES:
                fail(f"native release archive expands beyond {MAX_EXPANDED_BYTES} bytes: {archive.name}")
            destination_path = destination / pathlib.PurePosixPath(relative)
            if member.isdir():
                destination_path.mkdir(parents=True, exist_ok=True)
                continue
            destination_path.parent.mkdir(parents=True, exist_ok=True)
            stream = source.extractfile(member)
            if stream is None:
                fail(f"native release archive member cannot be read: {name}")
            with destination_path.open("wb") as output:
                shutil.copyfileobj(stream, output, length=1024 * 1024)
            destination_path.chmod(0o755 if member.mode & 0o111 else 0o644)
    extracted_members = {
        candidate.relative_to(destination).as_posix()
        for candidate in destination.rglob("*")
    }
    if not required.issubset(extracted_members):
        fail(f"native release archive is missing a required native or documentation root: {archive.name}")
    for binary_name in ("ygg", "ygg-host"):
        binary = destination / binary_name
        if not binary.is_file() or binary.is_symlink() or not os.access(binary, os.X_OK):
            fail(f"native release binary is missing or not executable: {archive.name}/{binary_name}")
    for directory in ("docs", "examples", "sdk"):
        path = destination / directory
        if not path.is_dir() or path.is_symlink():
            fail(f"native release documentation root is missing: {archive.name}/{directory}")
    return destination


def render(template, replacements):
    path = template_directory / template
    regular_file(path, "npm package template")
    text = path.read_text(encoding="utf-8")
    for marker, value in replacements.items():
        if marker not in text:
            fail(f"npm package template is missing {marker}: {path}")
        text = text.replace(marker, value)
    if "__" in text:
        fail(f"npm package template has an unresolved placeholder: {path}")
    return text


def copy_file(source, destination, executable=False):
    metadata = regular_file(source, "native release asset")
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, destination)
    destination.chmod(0o755 if executable else (0o755 if metadata.st_mode & 0o111 else 0o644))


def copy_tree(source, destination):
    if not source.is_dir() or source.is_symlink():
        fail(f"native release documentation root is invalid: {source}")
    shutil.copytree(source, destination, symlinks=False)


def set_mtimes(root):
    paths = sorted(root.rglob("*"), key=lambda item: len(item.parts), reverse=True)
    for path in paths + [root]:
        os.utime(path, (epoch, epoch), follow_symlinks=False)


checksum_entries = parse_checksums()
native = {}
licenses = None
for target in TARGETS:
    archive_name = f"ygg-{version}-{target}.tar.gz"
    archive = archive_directory / archive_name
    digest = checksum_entries.get(archive_name)
    if digest is None:
        fail(f"native release checksum is missing for {archive_name}")
    extracted = validate_archive(target, archive, digest)
    native[target] = extracted
    license_bytes = (extracted / "LICENSE").read_bytes()
    if licenses is None:
        licenses = license_bytes
    elif license_bytes != licenses:
        fail("native release archives do not share the same LICENSE")

if licenses is None:
    fail("native release archives did not provide a LICENSE")

launcher = staging_directory / "launcher"
launcher.mkdir()
(launcher / "package.json").write_text(
    render("launcher/package.json.in", {"__VERSION__": version}), encoding="utf-8"
)
(launcher / "README.md").write_text(
    render("launcher/README.md.in", {"VERSION": version}), encoding="utf-8"
)
(launcher / "LICENSE").write_bytes(licenses)
copy_file(template_directory / "launcher/bin/ygg", launcher / "bin/ygg", executable=True)
copy_file(template_directory / "launcher/bin/ygg-host", launcher / "bin/ygg-host", executable=True)
(launcher / "lib").mkdir()
(launcher / "lib/launch.sh").write_text(
    render("launcher/lib/launch.sh.in", {"__VERSION__": version}), encoding="utf-8"
)
(launcher / "lib/launch.sh").chmod(0o755)
set_mtimes(launcher)

for target, (package_name, operating_system, cpu, _) in TARGETS.items():
    platform = staging_directory / target
    platform.mkdir()
    (platform / "package.json").write_text(
        render(
            "platform/package.json.in",
            {
                "__PACKAGE_NAME__": package_name,
                "__VERSION__": version,
                "__TARGET__": target,
                "__OS__": operating_system,
                "__CPU__": cpu,
            },
        ),
        encoding="utf-8",
    )
    (platform / "README.md").write_text(
        render("platform/README.md.in", {"__TARGET__": target, "VERSION": version}),
        encoding="utf-8",
    )
    (platform / "LICENSE").write_bytes(licenses)
    source = native[target]
    copy_file(source / "ygg", platform / "bin/ygg", executable=True)
    copy_file(source / "ygg-host", platform / "bin/ygg-host", executable=True)
    (platform / "share/ygg/.ygg-version").parent.mkdir(parents=True, exist_ok=True)
    (platform / "share/ygg/.ygg-version").write_text(version + "\n", encoding="utf-8")
    copy_file(source / "README.md", platform / "share/ygg/README.md")
    copy_tree(source / "docs", platform / "share/ygg/docs")
    copy_tree(source / "examples", platform / "share/ygg/examples")
    copy_tree(source / "sdk", platform / "share/ygg/sdk")
    set_mtimes(platform)
PY

# npm pack is intentionally run from each staged directory with scripts
# disabled. The resulting names are normalized so artifact ordering and
# downstream verification do not depend on npm's display name formatting.
pack_directory="$work_directory/packed"
mkdir -p "$pack_directory"
for descriptor in \
    "launcher|$staging_directory/launcher|ygg-$version.tgz" \
    "aarch64-apple-darwin|$staging_directory/aarch64-apple-darwin|ygg-darwin-arm64-$version.tgz" \
    "x86_64-apple-darwin|$staging_directory/x86_64-apple-darwin|ygg-darwin-x64-$version.tgz" \
    "x86_64-unknown-linux-gnu|$staging_directory/x86_64-unknown-linux-gnu|ygg-linux-x64-gnu-$version.tgz"; do
    IFS='|' read -r label stage expected_name <<EOF
$descriptor
EOF
    destination="$pack_directory/$label"
    mkdir -p "$destination"
    (
        cd "$stage"
        npm pack --ignore-scripts --pack-destination "$destination" >/dev/null
    )
    shopt -s nullglob
    packed_files=("$destination"/*.tgz)
    shopt -u nullglob
    if [[ ${#packed_files[@]} -ne 1 ]]; then
        printf 'npm pack did not produce exactly one tarball for %s\n' "$label" >&2
        exit 1
    fi
    mv "${packed_files[0]}" "$output_directory/$expected_name"
done

python3 "$script_directory/verify-ygg-npm.py" "$version" "$output_directory"
printf 'created deterministic npm packages in %s\n' "$output_directory"
