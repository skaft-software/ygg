#!/usr/bin/env bash
set -euo pipefail

script_directory=$(cd "$(dirname "$0")" && pwd)
installer="$script_directory/install.sh"
work_directory=$(mktemp -d "${TMPDIR:-/tmp}/ygg-installer-test.XXXXXX")
trap 'rm -rf "$work_directory"' EXIT
assets="$work_directory/assets"
fake_bin="$work_directory/fake-bin"
package="ygg-0.3.3-alpha-aarch64-apple-darwin"
archive_name="$package.tar.gz"
mkdir -p "$assets" "$fake_bin"

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

make_assets() {
    staging="$work_directory/staging"
    rm -rf "$staging" "$assets"
    mkdir -p "$staging/$package" "$assets"
    cat > "$staging/$package/ygg" <<'EOF'
#!/bin/sh
case "${1:-}" in
    --version) printf '%s\n' 'ygg 0.3.3-alpha' ;;
    --help) printf '%s\n' 'fake Ygg help' ;;
    *) exit 0 ;;
esac
EOF
    chmod 0755 "$staging/$package/ygg"
    printf '%s\n' 'test license' > "$staging/$package/LICENSE"
    COPYFILE_DISABLE=1 tar -C "$staging" -czf "$assets/$archive_name" "$package"
    printf '%s  ./%s\n' "$(sha256_file "$assets/$archive_name")" "$archive_name" \
        > "$assets/YGG_SHA256SUMS"
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
cp "$YGG_TEST_ASSETS/$name" "$output"
if [ "${YGG_TEST_TAMPER_ARCHIVE:-0}" = 1 ] && [ "$name" != YGG_SHA256SUMS ]; then
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
        "$@" \
        sh "$installer"
}

make_assets
positive_home="$work_directory/positive-home"
run_installer "$positive_home" > "$work_directory/positive.out"
test -x "$positive_home/bin/ygg"
test "$("$positive_home/bin/ygg" --version)" = 'ygg 0.3.3-alpha'

after_redirect="$work_directory/untrusted-home"
if run_installer "$after_redirect" \
    YGG_TEST_REDIRECT_HOST=example.com \
    > "$work_directory/untrusted.out" 2> "$work_directory/untrusted.err"; then
    echo 'installer accepted an untrusted redirect' >&2
    exit 1
fi
grep -F 'redirected to an untrusted host' "$work_directory/untrusted.err" >/dev/null
test ! -e "$after_redirect/bin/ygg"

tamper_home="$work_directory/tamper-home"
if run_installer "$tamper_home" \
    YGG_TEST_TAMPER_ARCHIVE=1 \
    > "$work_directory/tamper.out" 2> "$work_directory/tamper.err"; then
    echo 'installer accepted a checksum mismatch' >&2
    exit 1
fi
grep -F 'checksum mismatch' "$work_directory/tamper.err" >/dev/null
test ! -e "$tamper_home/bin/ygg"

rm -f "$work_directory/staging/$package/ygg"
ln -s LICENSE "$work_directory/staging/$package/ygg"
COPYFILE_DISABLE=1 tar -C "$work_directory/staging" \
    -czf "$assets/$archive_name" "$package"
printf '%s  ./%s\n' "$(sha256_file "$assets/$archive_name")" "$archive_name" \
    > "$assets/YGG_SHA256SUMS"
link_home="$work_directory/link-home"
if run_installer "$link_home" \
    > "$work_directory/link.out" 2> "$work_directory/link.err"; then
    echo 'installer accepted a linked archive entry' >&2
    exit 1
fi
grep -F 'links or unexpected entry types' "$work_directory/link.err" >/dev/null
test ! -e "$link_home/bin/ygg"

printf '%s\n' 'binary installer tests passed'
