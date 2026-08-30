#!/bin/sh
set -eu

repository="skaft-software/ygg"
version="0.6.4"
tag="v$version"
release_source_commit="__YGG_RELEASE_SOURCE_COMMIT__"
release_base="https://github.com/$repository/releases/download/$tag"
checksum_asset="YGG_SHA256SUMS"
checksum_bundle_asset="$checksum_asset.sigstore.json"
cosign_version="3.1.3"
cosign_linux_amd64_sha256="4629c757b7618056f8ddd7e2625ae9fdd94c0372a65049520bc7d9df9efc7f71"
cosign_darwin_amd64_sha256="2347488e5d5b25336644024dfeca5601b190e91197a71a917bda44744aff106c"
cosign_darwin_arm64_sha256="5cf948c2f4dfe59687bdd0b8523709067383e03982cc543475c8a7dc70e92a76"
mode="binary"

usage() {
    cat <<EOF
Install Ygg $version.

Usage:
  install-ygg.sh
  install-ygg.sh --from-source

The default installation downloads the binary matching this machine. Use
--from-source to build the pinned release with Cargo instead.

Environment:
  YGG_INSTALL_DIR     Binary directory (default: \$HOME/.local/bin)
  YGG_DATA_DIR        Packaged docs directory (default: sibling share/ygg)
  YGG_NO_MODIFY_PATH  Set to 1 to leave shell profiles unchanged
EOF
}

if [ "$#" -gt 1 ]; then
    usage >&2
    exit 2
fi
if [ "$#" -eq 1 ]; then
    case "$1" in
        --from-source) mode="source" ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            printf 'unknown installer option: %s\n' "$1" >&2
            usage >&2
            exit 2
            ;;
    esac
fi

if [ -z "${HOME:-}" ] && [ -z "${YGG_INSTALL_DIR:-}" ]; then
    printf 'HOME is not set; provide an absolute YGG_INSTALL_DIR\n' >&2
    exit 1
fi
install_directory=${YGG_INSTALL_DIR:-"$HOME/.local/bin"}
case "$install_directory" in
    /*) ;;
    *)
        printf 'YGG_INSTALL_DIR must be an absolute path: %s\n' "$install_directory" >&2
        exit 1
        ;;
esac
install_prefix=${install_directory%/*}
if [ -z "$install_prefix" ]; then
    install_prefix=/
fi
data_directory=${YGG_DATA_DIR:-"$install_prefix/share/ygg"}
case "$data_directory" in
    /*) ;;
    *)
        printf 'YGG_DATA_DIR must be an absolute path: %s\n' "$data_directory" >&2
        exit 1
        ;;
esac

work_directory=$(mktemp -d "${TMPDIR:-/tmp}/ygg-install.XXXXXX")
chmod 0700 "$work_directory"
install_temporary=
assets_temporary=
cleanup() {
    if [ -n "$install_temporary" ]; then
        rm -f "$install_temporary"
    fi
    if [ -n "$assets_temporary" ]; then
        rm -rf "$assets_temporary"
    fi
    rm -rf "$work_directory"
}
trap cleanup EXIT HUP INT TERM

trusted_release_url() {
    case "$1" in
        https://github.com/*|https://github.com:443/*|\
        https://codeload.github.com/*|https://codeload.github.com:443/*|\
        https://release-assets.githubusercontent.com/*|\
        https://release-assets.githubusercontent.com:443/*)
            return 0
            ;;
        *) return 1 ;;
    esac
}

download_release_file() {
    url=$1
    destination=$2
    headers="$destination.headers"
    locations="$destination.locations"

    if ! trusted_release_url "$url"; then
        printf 'refusing untrusted release URL\n' >&2
        return 1
    fi
    if ! effective_url=$(curl \
        --proto '=https' \
        --proto-redir '=https' \
        --tlsv1.2 \
        --location \
        --max-redirs 5 \
        --retry 3 \
        --retry-delay 1 \
        --connect-timeout 15 \
        --max-time 300 \
        --fail \
        --silent \
        --show-error \
        --dump-header "$headers" \
        --output "$destination" \
        --write-out '%{url_effective}' \
        "$url"); then
        printf 'could not download Ygg release asset\n' >&2
        return 1
    fi

    tr -d '\r' < "$headers" \
        | awk 'tolower(substr($0, 1, 9)) == "location:" {
            sub(/^[^:]*:[[:space:]]*/, ""); print
        }' > "$locations"
    while IFS= read -r redirected; do
        [ -z "$redirected" ] && continue
        if ! trusted_release_url "$redirected"; then
            printf 'refusing release download redirected to an untrusted host\n' >&2
            return 1
        fi
    done < "$locations"
    if ! trusted_release_url "$effective_url"; then
        printf 'refusing release download from an untrusted final host\n' >&2
        return 1
    fi
}

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        printf 'a SHA-256 utility (sha256sum or shasum) is required\n' >&2
        return 1
    fi
}

bounded_file() {
    path=$1
    maximum=$2
    size=$(wc -c < "$path" | tr -d '[:space:]')
    case "$size" in
        ''|*[!0-9]*)
            printf 'could not determine downloaded file size\n' >&2
            return 1
            ;;
    esac
    if [ "$size" -gt "$maximum" ]; then
        printf 'downloaded release asset exceeds its size limit\n' >&2
        return 1
    fi
}

validate_release_source_commit() {
    case "$release_source_commit" in
        [0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]) ;;
        *)
            printf 'installer is not bound to an immutable Ygg release commit\n' >&2
            return 1
            ;;
    esac
}

install_pinned_cosign() {
    operating_system=$(uname -s)
    machine=$(uname -m)
    case "$operating_system:$machine" in
        Linux:x86_64|Linux:amd64)
            cosign_asset=cosign-linux-amd64
            cosign_sha256=$cosign_linux_amd64_sha256
            ;;
        Darwin:x86_64)
            if [ "$(sysctl -in sysctl.proc_translated 2>/dev/null || true)" = "1" ]; then
                cosign_asset=cosign-darwin-arm64
                cosign_sha256=$cosign_darwin_arm64_sha256
            else
                cosign_asset=cosign-darwin-amd64
                cosign_sha256=$cosign_darwin_amd64_sha256
            fi
            ;;
        Darwin:arm64|Darwin:aarch64)
            cosign_asset=cosign-darwin-arm64
            cosign_sha256=$cosign_darwin_arm64_sha256
            ;;
        *)
            printf 'no pinned cosign verifier is available for %s %s\n' "$operating_system" "$machine" >&2
            return 1
            ;;
    esac

    cosign_path="$work_directory/cosign"
    download_release_file \
        "https://github.com/sigstore/cosign/releases/download/v$cosign_version/$cosign_asset" \
        "$cosign_path"
    bounded_file "$cosign_path" 167772160
    actual_cosign_sha256=$(sha256_file "$cosign_path")
    if [ "$actual_cosign_sha256" != "$cosign_sha256" ]; then
        printf 'checksum mismatch for the pinned cosign verifier\n' >&2
        return 1
    fi
    chmod 0700 "$cosign_path"
}

verified_archive_sha256() {
    checksums=$1
    bundle=$2
    archive_name=$3
    validate_release_source_commit
    install_pinned_cosign

    identity_version=$(printf '%s' "$version" | sed 's/\./\\./g')
    identity="^https://github\\.com/skaft-software/ygg/\\.github/workflows/release-ygg\\.yml@refs/tags/(v${identity_version}|ygg-binaries-v${identity_version})$"
    python3 - \
        "$checksums" \
        "$bundle" \
        "$cosign_path" \
        "$identity" \
        "$repository" \
        "$release_source_commit" \
        "$archive_name" <<'PY'
import os
import re
import stat
import subprocess
import sys

manifest_path, bundle_path, cosign_path = sys.argv[1:4]
identity, repository, source_commit, archive_name = sys.argv[4:8]
expected_names = {
    "install-ygg.sh",
    "ygg-0.6.4-aarch64-apple-darwin.tar.gz",
    "ygg-0.6.4-x86_64-apple-darwin.tar.gz",
    "ygg-0.6.4-x86_64-unknown-linux-gnu.tar.gz",
}
line_pattern = re.compile(r"^([0-9A-Fa-f]{64})  (?:\./)?([A-Za-z0-9_.-]+)$")


def open_private_regular(path, maximum):
    if not hasattr(os, "O_NOFOLLOW"):
        raise RuntimeError("descriptor-relative verification is unavailable")
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    metadata = os.fstat(descriptor)
    if (
        not stat.S_ISREG(metadata.st_mode)
        or metadata.st_nlink != 1
        or metadata.st_size <= 0
        or metadata.st_size > maximum
    ):
        os.close(descriptor)
        raise RuntimeError("release provenance object is unsafe")
    return descriptor


manifest = bundle = None
try:
    manifest = open_private_regular(manifest_path, 1024 * 1024)
    bundle = open_private_regular(bundle_path, 1024 * 1024)
    manifest_reference = f"/dev/fd/{manifest}"
    # Cosign's Darwin build cannot reliably reopen a Sigstore bundle through a
    # shared /dev/fd descriptor. The bundle lives in the installer's private
    # work directory and was opened above with O_NOFOLLOW and a single-link
    # regular-file check, so pass that private path while retaining the validated
    # descriptor until verification completes.
    bundle_reference = bundle_path
    if not os.path.exists(manifest_reference):
        raise RuntimeError("descriptor-relative verification is unavailable")
    command = [
        cosign_path,
        "verify-blob",
        "--bundle", bundle_reference,
        "--certificate-identity-regexp", identity,
        "--certificate-oidc-issuer", "https://token.actions.githubusercontent.com",
        "--certificate-github-workflow-name", "Ygg binary release",
        "--certificate-github-workflow-repository", repository,
        "--certificate-github-workflow-sha", source_commit,
        manifest_reference,
    ]
    result = subprocess.run(
        command,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        pass_fds=(manifest, bundle),
        timeout=120,
        check=False,
    )
    if result.returncode != 0:
        raise RuntimeError("release checksum provenance verification failed")

    os.lseek(manifest, 0, os.SEEK_SET)
    with os.fdopen(os.dup(manifest), "rb") as source:
        contents = source.read(1024 * 1024 + 1)
    if len(contents) > 1024 * 1024:
        raise RuntimeError("release checksum manifest exceeds its size limit")
    try:
        lines = contents.decode("ascii").splitlines()
    except UnicodeDecodeError as error:
        raise RuntimeError("release checksum manifest is malformed") from error
    entries = {}
    for line in lines:
        match = line_pattern.fullmatch(line)
        if match is None:
            raise RuntimeError("release checksum manifest is malformed")
        digest, name = match.groups()
        if name in entries:
            raise RuntimeError("release checksum manifest contains duplicate entries")
        entries[name] = digest.lower()
    if set(entries) != expected_names:
        raise RuntimeError("release checksum manifest has an unexpected asset set")
    print(entries[archive_name])
except (KeyError, OSError, subprocess.SubprocessError, RuntimeError) as error:
    message = str(error)
    allowed = {
        "descriptor-relative verification is unavailable",
        "release provenance object is unsafe",
        "release checksum provenance verification failed",
        "release checksum manifest exceeds its size limit",
        "release checksum manifest is malformed",
        "release checksum manifest contains duplicate entries",
        "release checksum manifest has an unexpected asset set",
    }
    print(message if message in allowed else "release checksum provenance verification failed", file=sys.stderr)
    raise SystemExit(1)
finally:
    if manifest is not None:
        os.close(manifest)
    if bundle is not None:
        os.close(bundle)
PY
}

extract_validated_archive() {
    archive=$1
    extraction=$2
    expected_root=$3
    archive_kind=$4
    expected_sha256=$5

    python3 - "$archive" "$extraction" "$expected_root" "$archive_kind" "$expected_sha256" <<'PY'
import gzip
import hashlib
import os
import pathlib
import stat
import sys
import tarfile
import tempfile
import unicodedata

MAX_ARCHIVE_BYTES = 128 * 1024 * 1024
MAX_TAR_BYTES = 160 * 1024 * 1024
MAX_ENTRIES = 4096
MAX_MEMBER_BYTES = 64 * 1024 * 1024
MAX_EXPANDED_BYTES = 128 * 1024 * 1024
# A tar stream is zero-padded to a 10240-byte record boundary, and some
# producers emit one additional 512-byte block after the last record. Allow
# up to a record plus one block of zero padding; anything non-zero, or more
# padding than that, means appended or concatenated data.
MAX_PADDING_BYTES = tarfile.RECORDSIZE + tarfile.BLOCKSIZE
WINDOWS_FORBIDDEN = set('<>:"\\|?*')
WINDOWS_DEVICES = {
    "CON", "PRN", "AUX", "NUL",
    *(f"COM{number}" for number in range(1, 10)),
    *(f"LPT{number}" for number in range(1, 10)),
}


class ArchiveError(Exception):
    pass


def fail(message):
    raise ArchiveError(message)


def safe_parts(name, expected_root):
    if not isinstance(name, str) or not name or name.startswith(("/", "\\")):
        fail("release archive contains an unsafe path")
    if len(name.encode("utf-8")) > 4096:
        fail("release archive contains an unsafe path")
    parts = name.rstrip("/").split("/")
    if not parts or parts[0] != expected_root:
        fail("release archive has an unexpected layout")
    for component in parts:
        if (
            not component
            or component in (".", "..")
            or component != unicodedata.normalize("NFC", component)
            or component.endswith((".", " "))
            or len(component.encode("utf-8")) > 255
            or any(ord(character) < 32 or ord(character) == 127 for character in component)
            or any(character in WINDOWS_FORBIDDEN for character in component)
        ):
            fail("release archive contains an unsafe or non-portable path")
        stem = component.split(".", 1)[0].upper()
        if stem in WINDOWS_DEVICES:
            fail("release archive contains an unsafe or non-portable path")
    return parts


def validate_layout(parts, member, kind):
    if len(parts) == 1:
        if not member.isdir():
            fail("release archive has an unexpected layout")
        return
    if kind == "source":
        return
    top = parts[1]
    if top in {"LICENSE", "README.md", "ygg", "ygg-host"}:
        if len(parts) != 2 or not member.isfile():
            fail("release archive has an unexpected layout")
    elif top in {"docs", "examples", "sdk"}:
        if len(parts) == 2 and not member.isdir():
            fail("release archive has an unexpected layout")
    else:
        fail("release archive has an unexpected layout")


def ensure_parent_directories(root, parts, path_types):
    current = root
    for index, component in enumerate(parts[:-1]):
        logical = "/".join(parts[: index + 1])
        if path_types.get(logical) == "file":
            fail("release archive path traverses a file")
        current = current / component
        try:
            current.mkdir(mode=0o755)
        except FileExistsError:
            if current.is_symlink() or not current.is_dir():
                fail("release archive path traverses an unsafe object")


def extract_member(packaged, member, destination, mode):
    source = packaged.extractfile(member)
    if source is None:
        fail("release archive contains an unreadable file")
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    descriptor = os.open(destination, flags, mode)
    try:
        os.fchmod(descriptor, mode)
        remaining = member.size
        with os.fdopen(descriptor, "wb", closefd=False) as output:
            while remaining:
                chunk = source.read(min(1024 * 1024, remaining))
                if not chunk:
                    fail("release archive member ended unexpectedly")
                output.write(chunk)
                remaining -= len(chunk)
            if source.read(1):
                fail("release archive member exceeds its declared size")
            output.flush()
            os.fsync(output.fileno())
    finally:
        os.close(descriptor)
        source.close()


def run():
    archive_path = pathlib.Path(sys.argv[1])
    extraction = pathlib.Path(sys.argv[2])
    expected_root = sys.argv[3]
    kind = sys.argv[4]
    expected_sha256 = sys.argv[5]
    if kind not in {"release", "source"}:
        fail("invalid archive validation mode")

    nofollow = getattr(os, "O_NOFOLLOW", 0)
    descriptor = os.open(archive_path, os.O_RDONLY | nofollow)
    try:
        metadata = os.fstat(descriptor)
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1:
            fail("downloaded archive is not a private regular file")
        if metadata.st_size <= 0 or metadata.st_size > MAX_ARCHIVE_BYTES:
            fail("downloaded release archive exceeds its size limit")

        digest = hashlib.sha256()
        with os.fdopen(os.dup(descriptor), "rb") as source:
            for chunk in iter(lambda: source.read(1024 * 1024), b""):
                digest.update(chunk)
        if expected_sha256 != "-" and digest.hexdigest() != expected_sha256:
            fail("checksum mismatch for release archive")

        os.lseek(descriptor, 0, os.SEEK_SET)
        with os.fdopen(os.dup(descriptor), "rb") as compressed, tempfile.TemporaryFile() as tar_bytes:
            total_tar_bytes = 0
            try:
                with gzip.GzipFile(fileobj=compressed, mode="rb") as uncompressed:
                    while True:
                        chunk = uncompressed.read(1024 * 1024)
                        if not chunk:
                            break
                        total_tar_bytes += len(chunk)
                        if total_tar_bytes > MAX_TAR_BYTES:
                            fail("release archive exceeds its decompression limit")
                        tar_bytes.write(chunk)
            except (EOFError, gzip.BadGzipFile) as error:
                raise ArchiveError("release archive has invalid gzip data") from error

            tar_bytes.seek(0)
            seen = set()
            portable_seen = set()
            path_types = {}
            expanded = 0
            count = 0
            required = {
                expected_root: "directory",
                f"{expected_root}/README.md": "file",
                f"{expected_root}/docs": "directory",
                f"{expected_root}/examples": "directory",
                f"{expected_root}/sdk": "directory",
            }
            if kind == "release":
                required.update({
                    f"{expected_root}/LICENSE": "file",
                    f"{expected_root}/ygg": "file",
                    f"{expected_root}/ygg-host": "file",
                })

            with tarfile.open(fileobj=tar_bytes, mode="r|", bufsize=512) as packaged:
                for member in packaged:
                    count += 1
                    if count > MAX_ENTRIES:
                        fail("release archive contains too many members")
                    if member.issym() or member.islnk() or not (member.isdir() or member.isfile()):
                        fail("release archive contains links or unexpected entry types")
                    if getattr(member, "sparse", None):
                        fail("release archive contains a sparse file")
                    if member.size < 0 or member.size > MAX_MEMBER_BYTES:
                        fail("release archive member exceeds its size limit")
                    expanded += member.size
                    if expanded > MAX_EXPANDED_BYTES:
                        fail("release archive exceeds its expanded-size limit")

                    parts = safe_parts(member.name, expected_root)
                    logical = "/".join(parts)
                    portable = "/".join(
                        unicodedata.normalize("NFC", component).casefold() for component in parts
                    )
                    if logical in seen:
                        fail("release archive contains a duplicate member")
                    if portable in portable_seen:
                        fail("release archive contains colliding portable paths")
                    seen.add(logical)
                    portable_seen.add(portable)
                    validate_layout(parts, member, kind)

                    ensure_parent_directories(extraction, parts, path_types)
                    destination = extraction.joinpath(*parts)
                    if member.isdir():
                        try:
                            destination.mkdir(mode=0o755)
                        except FileExistsError:
                            if destination.is_symlink() or not destination.is_dir():
                                fail("release archive directory collides with an unsafe object")
                        os.chmod(destination, 0o755, follow_symlinks=False)
                        path_types[logical] = "directory"
                    else:
                        mode = 0o755 if kind == "release" and logical in {
                            f"{expected_root}/ygg", f"{expected_root}/ygg-host"
                        } else 0o644
                        extract_member(packaged, member, destination, mode)
                        path_types[logical] = "file"

                logical_end = packaged.offset

            if logical_end > total_tar_bytes:
                fail("release archive has an invalid end marker")
            tar_bytes.seek(logical_end)
            padding = tar_bytes.read()
            if len(padding) > MAX_PADDING_BYTES or any(padding):
                fail("release archive contains trailing or concatenated data")
            for name, expected_type in required.items():
                if path_types.get(name) != expected_type:
                    fail("release archive is missing required members")
    finally:
        os.close(descriptor)


try:
    run()
except ArchiveError as error:
    print(str(error), file=sys.stderr)
    raise SystemExit(1)
except (OSError, tarfile.TarError, UnicodeError, ValueError):
    print("release archive is malformed or could not be extracted", file=sys.stderr)
    raise SystemExit(1)
PY
}

validate_release_binaries() {
    source_root=$1
    expected_version=$2
    source_binary="$source_root/ygg"
    source_host="$source_root/ygg-host"

    for executable in "$source_binary" "$source_host"; do
        if [ ! -f "$executable" ] || [ -L "$executable" ]; then
            printf 'Ygg release executable is not a regular file: %s\n' "$executable" >&2
            return 1
        fi
        chmod 0755 "$executable"
    done

    binary_version=$("$source_binary" --version)
    if [ "$binary_version" != "ygg $expected_version" ]; then
        printf 'Ygg binary version mismatch: %s\n' "$binary_version" >&2
        return 1
    fi

    host_probe="$work_directory/host-probe"
    printf '%s\n' '{"protocol_version":1,"request_id":"installer-probe","command":"hello"}' \
        | "$source_host" > "$host_probe"
    bounded_file "$host_probe" 1048576
    if [ "$(wc -l < "$host_probe" | tr -d '[:space:]')" != 1 ] \
        || ! grep -F '"request_id":"installer-probe"' "$host_probe" >/dev/null 2>&1 \
        || ! grep -F '"type":"hello"' "$host_probe" >/dev/null 2>&1 \
        || ! grep -F "\"sdk_version\":\"$expected_version\"" "$host_probe" >/dev/null 2>&1; then
        printf 'ygg-host did not return a valid protocol handshake\n' >&2
        return 1
    fi
}

install_executable() {
    source_binary=$1
    name=$2
    if [ -e "$install_directory" ] && [ ! -d "$install_directory" ]; then
        printf 'installation path is not a directory: %s\n' "$install_directory" >&2
        return 1
    fi
    mkdir -p "$install_directory"
    destination="$install_directory/$name"
    if [ -L "$destination" ] \
        || { [ -e "$destination" ] && [ ! -f "$destination" ]; }; then
        printf 'Ygg destination is linked or not a regular file: %s\n' "$destination" >&2
        return 1
    fi

    install_temporary=$(mktemp "$install_directory/.$name.XXXXXX")
    cp "$source_binary" "$install_temporary"
    chmod 0755 "$install_temporary"
    mv -f "$install_temporary" "$destination"
    install_temporary=
}

install_release_binaries() {
    source_root=$1
    expected_version=$2
    validate_release_binaries "$source_root" "$expected_version"
    install_executable "$source_root/ygg" ygg
    install_executable "$source_root/ygg-host" ygg-host
}

install_assets() {
    source_root=$1
    if [ ! -f "$source_root/README.md" ] || [ -L "$source_root/README.md" ] \
        || [ ! -d "$source_root/docs" ] || [ -L "$source_root/docs" ] \
        || [ ! -d "$source_root/examples" ] || [ -L "$source_root/examples" ] \
        || [ ! -d "$source_root/sdk" ] || [ -L "$source_root/sdk" ]; then
        printf 'Ygg documentation assets are missing from the release package\n' >&2
        return 1
    fi

    data_parent=${data_directory%/*}
    if [ -z "$data_parent" ]; then
        data_parent=/
    fi
    mkdir -p "$data_parent"
    assets_temporary=$(mktemp -d "$data_parent/.ygg-docs.XXXXXX")
    cp "$source_root/README.md" "$assets_temporary/README.md"
    cp -R "$source_root/docs" "$assets_temporary/docs"
    cp -R "$source_root/examples" "$assets_temporary/examples"
    cp -R "$source_root/sdk" "$assets_temporary/sdk"
    printf '%s\n' "$version" > "$assets_temporary/.ygg-version"

    if { [ -e "$data_directory" ] || [ -L "$data_directory" ]; } \
        && [ ! -d "$data_directory" ]; then
        printf 'Ygg documentation path is not a directory: %s\n' "$data_directory" >&2
        return 1
    fi
    previous_directory="$data_directory.previous.$$"
    if [ -e "$data_directory" ] || [ -L "$data_directory" ]; then
        if ! mv "$data_directory" "$previous_directory"; then
            printf 'could not stage the existing Ygg documentation directory\n' >&2
            return 1
        fi
    fi
    if ! mv "$assets_temporary" "$data_directory"; then
        if [ -e "$previous_directory" ]; then
            mv "$previous_directory" "$data_directory" || true
        fi
        printf 'could not install Ygg documentation assets\n' >&2
        return 1
    fi
    assets_temporary=
    rm -rf "$previous_directory"
}

resolve_target() {
    operating_system=$(uname -s)
    machine=$(uname -m)
    case "$operating_system" in
        Darwin)
            if [ "$machine" = "x86_64" ] \
                && [ "$(sysctl -in sysctl.proc_translated 2>/dev/null || true)" = "1" ]; then
                machine="arm64"
            fi
            case "$machine" in
                arm64|aarch64) printf '%s\n' "aarch64-apple-darwin" ;;
                x86_64) printf '%s\n' "x86_64-apple-darwin" ;;
                *)
                    printf 'unsupported macOS architecture: %s\n' "$machine" >&2
                    return 1
                    ;;
            esac
            ;;
        Linux)
            case "$machine" in
                x86_64|amd64) ;;
                *)
                    printf 'unsupported Linux architecture: %s\n' "$machine" >&2
                    return 1
                    ;;
            esac
            if command -v getconf >/dev/null 2>&1 \
                && getconf GNU_LIBC_VERSION >/dev/null 2>&1; then
                :
            elif command -v ldd >/dev/null 2>&1 \
                && ldd --version 2>&1 | grep -Eiq 'glibc|gnu libc'; then
                :
            else
                printf 'the prebuilt Linux release requires GNU libc; Linux musl is not supported\n' >&2
                return 1
            fi
            printf '%s\n' "x86_64-unknown-linux-gnu"
            ;;
        *)
            printf 'unsupported operating system: %s\n' "$operating_system" >&2
            return 1
            ;;
    esac
}

if [ "$mode" = "source" ]; then
    for command in cargo curl python3; do
        if ! command -v "$command" >/dev/null 2>&1; then
            printf 'required source-installer command is unavailable: %s\n' "$command" >&2
            exit 1
        fi
    done
    validate_release_source_commit
    printf 'Building Ygg %s from immutable source %s\n' "$tag" "$release_source_commit"
    source_root="$work_directory/source-root"
    cargo install \
        --locked \
        --git "https://github.com/$repository" \
        --rev "$release_source_commit" \
        --bin ygg \
        --bin ygg-host \
        --root "$source_root" \
        ygg-coding-agent

    source_archive="$work_directory/ygg-source.tar.gz"
    source_extraction="$work_directory/source-extraction"
    source_package="ygg-$release_source_commit"
    download_release_file \
        "https://github.com/$repository/archive/$release_source_commit.tar.gz" \
        "$source_archive"
    bounded_file "$source_archive" 134217728
    mkdir -m 0700 "$source_extraction"
    extract_validated_archive \
        "$source_archive" \
        "$source_extraction" \
        "$source_package" \
        source \
        -
    install_release_binaries "$source_root/bin" "$version"
    install_assets "$source_extraction/$source_package"
else
    for command in curl python3 uname; do
        if ! command -v "$command" >/dev/null 2>&1; then
            printf 'required installer command is unavailable: %s\n' "$command" >&2
            exit 1
        fi
    done

    target=$(resolve_target)
    archive_name="ygg-$version-$target.tar.gz"
    archive="$work_directory/$archive_name"
    checksums="$work_directory/$checksum_asset"
    checksum_bundle="$work_directory/$checksum_bundle_asset"
    printf 'Downloading Ygg %s for %s\n' "$version" "$target"
    download_release_file "$release_base/$checksum_asset" "$checksums"
    bounded_file "$checksums" 1048576
    download_release_file "$release_base/$checksum_bundle_asset" "$checksum_bundle"
    bounded_file "$checksum_bundle" 1048576
    if ! expected_sha256=$(verified_archive_sha256 \
        "$checksums" \
        "$checksum_bundle" \
        "$archive_name"); then
        exit 1
    fi

    download_release_file "$release_base/$archive_name" "$archive"
    bounded_file "$archive" 134217728
    package="ygg-$version-$target"
    extraction="$work_directory/extracted"
    mkdir -m 0700 "$extraction"
    extract_validated_archive \
        "$archive" \
        "$extraction" \
        "$package" \
        release \
        "$expected_sha256"
    install_release_binaries "$extraction/$package" "$version"
    install_assets "$extraction/$package"
fi

path_present=false
case ":${PATH:-}:" in
    *":$install_directory:"*) path_present=true ;;
esac
if [ "$path_present" = false ] && [ "${YGG_NO_MODIFY_PATH:-0}" != "1" ]; then
    profile=
    path_line=
    if [ "$install_directory" = "${HOME:-}/.local/bin" ]; then
        path_line='export PATH="$HOME/.local/bin:$PATH"'
        case "${SHELL:-}" in
            */zsh) profile="$HOME/.zshrc" ;;
            */bash)
                if [ -f "$HOME/.bash_profile" ]; then
                    profile="$HOME/.bash_profile"
                else
                    profile="$HOME/.bashrc"
                fi
                ;;
            */sh|*/dash|*/ksh) profile="$HOME/.profile" ;;
        esac
    fi
    marker="# Added by the Ygg installer"
    if [ -n "$profile" ] && ! grep -F "$marker" "$profile" >/dev/null 2>&1; then
        printf '\n%s\n%s\n' "$marker" "$path_line" >> "$profile"
        printf 'Added %s to PATH in %s\n' "$install_directory" "$profile"
    fi
fi

"$install_directory/ygg" --version
if ! command -v rg >/dev/null 2>&1; then
    printf '%s\n' \
        "Note: Ygg also requires ripgrep (rg)." \
        "Install it with 'brew install ripgrep' on macOS or your Linux package manager."
fi
if [ "$path_present" = false ]; then
    printf 'Restart your shell, or run:\n  export PATH="%s:$PATH"\n' "$install_directory"
fi
printf '%s\n' "Ygg is installed. Run 'ygg --help' to get started."
