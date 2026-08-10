#!/usr/bin/env bash
set -euo pipefail

script_directory=$(cd "$(dirname "$0")" && pwd)
source_installer="$script_directory/install.sh"
work_directory=$(mktemp -d "${TMPDIR:-/tmp}/ygg-installer-test.XXXXXX")
trap 'rm -rf "$work_directory"' EXIT
assets="$work_directory/assets"
fake_bin="$work_directory/fake-bin"
installer="$work_directory/install-ygg.sh"
package="ygg-0.3.3-aarch64-apple-darwin"
archive_name="$package.tar.gz"
release_commit=0123456789abcdef0123456789abcdef01234567
mkdir -p "$assets" "$fake_bin"

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

cat > "$fake_bin/cosign-template" <<'EOF'
#!/bin/sh
set -eu
if [ "${YGG_TEST_BAD_SIGNATURE:-0}" = 1 ]; then
    exit 1
fi
[ "${1:-}" = verify-blob ] || exit 2
shift
identity='^https://github\.com/skaft-software/ygg/\.github/workflows/release-ygg\.yml@refs/tags/(v0\.3\.3|ygg-binaries-v0\.3\.3)$'
expected_sha=0123456789abcdef0123456789abcdef01234567
saw_bundle=false
saw_identity=false
saw_issuer=false
saw_name=false
saw_repository=false
saw_sha=false
blob=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --bundle)
            [ -f "$2" ] || exit 2
            saw_bundle=true
            shift 2
            ;;
        --certificate-identity-regexp)
            [ "$2" = "$identity" ] || exit 2
            saw_identity=true
            shift 2
            ;;
        --certificate-oidc-issuer)
            [ "$2" = https://token.actions.githubusercontent.com ] || exit 2
            saw_issuer=true
            shift 2
            ;;
        --certificate-github-workflow-name)
            [ "$2" = 'Ygg binary release' ] || exit 2
            saw_name=true
            shift 2
            ;;
        --certificate-github-workflow-repository)
            [ "$2" = skaft-software/ygg ] || exit 2
            saw_repository=true
            shift 2
            ;;
        --certificate-github-workflow-sha)
            [ "$2" = "$expected_sha" ] || exit 2
            saw_sha=true
            shift 2
            ;;
        --*) exit 2 ;;
        *)
            [ -z "$blob" ] || exit 2
            blob=$1
            shift
            ;;
    esac
done
[ "$saw_bundle" = true ]
[ "$saw_identity" = true ]
[ "$saw_issuer" = true ]
[ "$saw_name" = true ]
[ "$saw_repository" = true ]
[ "$saw_sha" = true ]
[ -f "$blob" ]
printf '%s\n' verified >> "$YGG_TEST_COSIGN_LOG"
EOF
chmod 0755 "$fake_bin/cosign-template"

cosign_sha256=$(sha256_file "$fake_bin/cosign-template")
python3 - "$source_installer" "$installer" "$release_commit" "$cosign_sha256" <<'PY'
import pathlib
import sys

source = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")
commit = sys.argv[3]
digest = sys.argv[4]
placeholder = 'release_source_commit="__YGG_RELEASE_SOURCE_COMMIT__"'
cosign = 'cosign_darwin_arm64_sha256="5cf948c2f4dfe59687bdd0b8523709067383e03982cc543475c8a7dc70e92a76"'
if source.count(placeholder) != 1 or source.count(cosign) != 1:
    raise SystemExit("installer release placeholders changed unexpectedly")
source = source.replace(placeholder, f'release_source_commit="{commit}"')
source = source.replace(cosign, f'cosign_darwin_arm64_sha256="{digest}"')
pathlib.Path(sys.argv[2]).write_text(source, encoding="utf-8")
PY
chmod 0755 "$installer"

make_archive() {
    variant=$1
    rm -rf "$assets"
    mkdir -p "$assets"
    cp "$fake_bin/cosign-template" "$assets/cosign-darwin-arm64"
    printf '%s\n' 'test sigstore bundle' > "$assets/YGG_SHA256SUMS.sigstore.json"
    python3 - "$assets/$archive_name" "$package" "$variant" <<'PY'
import gzip
import io
import pathlib
import sys
import tarfile

archive = pathlib.Path(sys.argv[1])
package = sys.argv[2]
variant = sys.argv[3]

files = {
    "LICENSE": b"test license\n",
    "README.md": b"# Ygg\n",
    "ygg": b'''#!/bin/sh
case "${1:-}" in
    --version) printf '%s\\n' 'ygg 0.3.3' ;;
    --help) printf '%s\\n' 'fake Ygg help' ;;
    *) exit 0 ;;
esac
''',
    "ygg-host": b'''#!/bin/sh
IFS= read -r request
case "$request" in
    *'"request_id":"installer-probe"'*)
        printf '%s\\n' '{"protocol_version":1,"request_id":"installer-probe","seq":1,"type":"hello","data":{"sdk_version":"0.3.3"}}'
        ;;
    *) exit 2 ;;
esac
''',
    "docs/index.md": b"# Docs\n",
    "examples/README.md": b"# Example\n",
    "sdk/README.md": b"# SDK\n",
}
directories = [package, f"{package}/docs", f"{package}/examples", f"{package}/sdk"]

class Zeros(io.RawIOBase):
    def __init__(self, size):
        self.remaining = size
    def readable(self):
        return True
    def readinto(self, target):
        count = min(len(target), self.remaining)
        if count == 0:
            return 0
        target[:count] = b"\0" * count
        self.remaining -= count
        return count


def add_directory(output, name):
    info = tarfile.TarInfo(name)
    info.type = tarfile.DIRTYPE
    info.mode = 0o755
    output.addfile(info)


def add_file(output, name, data, mode=0o644):
    info = tarfile.TarInfo(name)
    info.size = len(data)
    info.mode = mode
    output.addfile(info, io.BytesIO(data))

with archive.open("wb") as raw:
    with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as compressed:
        with tarfile.open(fileobj=compressed, mode="w", format=tarfile.GNU_FORMAT) as output:
            for directory in directories:
                add_directory(output, directory)
            for name, data in files.items():
                if variant == "link" and name == "ygg":
                    info = tarfile.TarInfo(f"{package}/ygg")
                    info.type = tarfile.SYMTYPE
                    info.linkname = "LICENSE"
                    output.addfile(info)
                else:
                    add_file(
                        output,
                        f"{package}/{name}",
                        data,
                        0o755 if name in {"ygg", "ygg-host"} else 0o644,
                    )
            if variant == "duplicate":
                add_file(output, f"{package}/README.md", b"duplicate\n")
            elif variant == "portable-collision":
                add_file(output, f"{package}/docs/INDEX.md", b"collision\n")
            elif variant == "traversal":
                add_file(output, f"{package}/docs/../escape", b"escape\n")
            elif variant == "device-name":
                add_file(output, f"{package}/docs/CON.txt", b"device\n")
            elif variant == "unexpected":
                add_file(output, f"{package}/private.txt", b"private\n")
            elif variant == "special":
                info = tarfile.TarInfo(f"{package}/docs/device")
                info.type = tarfile.CHRTYPE
                info.devmajor = 1
                info.devminor = 3
                output.addfile(info)
            elif variant == "many":
                for index in range(4096):
                    add_file(output, f"{package}/docs/member-{index:04d}", b"")
            elif variant == "expanded":
                for index in range(3):
                    size = 44 * 1024 * 1024
                    info = tarfile.TarInfo(f"{package}/docs/large-{index}")
                    info.size = size
                    info.mode = 0o644
                    output.addfile(info, io.BufferedReader(Zeros(size), buffer_size=1024 * 1024))

if variant == "concatenated":
    with archive.open("ab") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode="w") as output:
                add_file(output, "second-archive", b"unexpected\n")
PY
    {
        printf '%064d  ./install-ygg.sh\n' 0
        printf '%s  ./%s\n' "$(sha256_file "$assets/$archive_name")" "$archive_name"
        printf '%064d  ./ygg-0.3.3-x86_64-apple-darwin.tar.gz\n' 0
        printf '%064d  ./ygg-0.3.3-x86_64-unknown-linux-gnu.tar.gz\n' 0
    } > "$assets/YGG_SHA256SUMS"
}

cat > "$fake_bin/uname" <<'EOF'
#!/bin/sh
case "${1:-}" in
    -s) printf '%s\n' Darwin ;;
    -m) printf '%s\n' arm64 ;;
    *) exit 2 ;;
esac
EOF
cat > "$fake_bin/cargo" <<'EOF'
#!/bin/sh
echo 'binary installer unexpectedly invoked Cargo' >&2
exit 99
EOF
cat > "$fake_bin/curl" <<'EOF'
#!/bin/sh
set -eu
output=
headers=
url=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --proto|--proto-redir|--max-redirs|--retry|--retry-delay|--connect-timeout|--max-time|--write-out)
            shift 2
            ;;
        --dump-header)
            headers=$2
            shift 2
            ;;
        --output)
            output=$2
            shift 2
            ;;
        --tlsv1.2|--location|--fail|--silent|--show-error)
            shift
            ;;
        https://*)
            url=$1
            shift
            ;;
        *)
            printf 'unexpected curl argument: %s\n' "$1" >&2
            exit 2
            ;;
    esac
done
name=${url##*/}
source="$YGG_TEST_ASSETS/$name"
if [ "${YGG_TEST_HARDLINK_ARCHIVE:-0}" = 1 ] && [ "$name" = ygg-0.3.3-aarch64-apple-darwin.tar.gz ]; then
    ln "$source" "$output"
else
    cp "$source" "$output"
fi
if [ "${YGG_TEST_TAMPER_ARCHIVE:-0}" = 1 ] && [ "$name" = ygg-0.3.3-aarch64-apple-darwin.tar.gz ]; then
    printf 'tampered' >> "$output"
fi
if [ "${YGG_TEST_TAMPER_COSIGN:-0}" = 1 ] && [ "$name" = cosign-darwin-arm64 ]; then
    printf 'tampered' >> "$output"
fi
host=${YGG_TEST_REDIRECT_HOST:-release-assets.githubusercontent.com}
effective="https://$host/test/$name"
printf 'HTTP/1.1 302 Found\r\nLocation: %s\r\n\r\nHTTP/1.1 200 OK\r\n\r\n' \
    "$effective" > "$headers"
printf '%s' "$effective"
EOF
chmod 0755 "$fake_bin/uname" "$fake_bin/cargo" "$fake_bin/curl"

run_installer() {
    test_home=$1
    shift
    mkdir -p "$test_home"
    env \
        HOME="$test_home" \
        SHELL=/bin/sh \
        PATH="$fake_bin:$PATH" \
        YGG_INSTALL_DIR="$test_home/bin" \
        YGG_NO_MODIFY_PATH=1 \
        YGG_TEST_ASSETS="$assets" \
        YGG_TEST_COSIGN_LOG="$work_directory/cosign.log" \
        "$@" \
        sh "$installer"
}

expect_failure() {
    label=$1
    expected=$2
    shift 2
    test_home="$work_directory/$label-home"
    if run_installer "$test_home" "$@" \
        > "$work_directory/$label.out" 2> "$work_directory/$label.err"; then
        printf 'installer accepted invalid input: %s\n' "$label" >&2
        exit 1
    fi
    grep -F "$expected" "$work_directory/$label.err" >/dev/null
    test ! -e "$test_home/bin/ygg"
}

make_archive valid
positive_home="$work_directory/positive-home"
run_installer "$positive_home" > "$work_directory/positive.out"
test -x "$positive_home/bin/ygg"
test -x "$positive_home/bin/ygg-host"
printf '%s\n' '{"protocol_version":1,"request_id":"installer-probe","command":"hello"}' \
    | "$positive_home/bin/ygg-host" \
    | grep -F '"sdk_version":"0.3.3"' >/dev/null
test "$("$positive_home/bin/ygg" --version)" = 'ygg 0.3.3'
test -f "$positive_home/share/ygg/README.md"
test -f "$positive_home/share/ygg/docs/index.md"
test -f "$positive_home/share/ygg/examples/README.md"
test -f "$positive_home/share/ygg/sdk/README.md"
grep -Fx verified "$work_directory/cosign.log" >/dev/null

override_home="$work_directory/override-home"
override_data="$work_directory/override-data"
run_installer "$override_home" YGG_DATA_DIR="$override_data" > "$work_directory/override.out"
test -x "$override_home/bin/ygg"
test -x "$override_home/bin/ygg-host"
test -f "$override_data/README.md"
test -f "$override_data/docs/index.md"
test -f "$override_data/examples/README.md"
test -f "$override_data/sdk/README.md"
test ! -e "$override_home/share/ygg"

expect_failure untrusted 'redirected to an untrusted host' YGG_TEST_REDIRECT_HOST=example.com
expect_failure signature 'release checksum provenance verification failed' YGG_TEST_BAD_SIGNATURE=1
expect_failure cosign-tamper 'checksum mismatch for the pinned cosign verifier' YGG_TEST_TAMPER_COSIGN=1
expect_failure archive-tamper 'checksum mismatch for release archive' YGG_TEST_TAMPER_ARCHIVE=1
expect_failure hardlink 'downloaded archive is not a private regular file' YGG_TEST_HARDLINK_ARCHIVE=1

printf '%s  ./%s\n' "$(sha256_file "$assets/$archive_name")" "$archive_name" \
    >> "$assets/YGG_SHA256SUMS"
expect_failure duplicate-checksum 'release checksum manifest contains duplicate entries'

for case_spec in \
    'link|links or unexpected entry types' \
    'duplicate|duplicate member' \
    'portable-collision|colliding portable paths' \
    'traversal|unsafe or non-portable path' \
    'device-name|unsafe or non-portable path' \
    'unexpected|unexpected layout' \
    'special|links or unexpected entry types' \
    'many|too many members' \
    'expanded|expanded-size limit' \
    'concatenated|trailing or concatenated data'; do
    variant=${case_spec%%|*}
    expected=${case_spec#*|}
    make_archive "$variant"
    expect_failure "$variant" "$expected"
done

printf '%s\n' 'binary installer tests passed'
