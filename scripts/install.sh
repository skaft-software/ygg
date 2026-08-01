#!/bin/sh
set -eu

repository="skaft-software/ygg"
version="0.3.2-alpha"
tag="v$version"
release_base="https://github.com/$repository/releases/download/$tag"
checksum_asset="YGG_SHA256SUMS"
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

work_directory=$(mktemp -d "${TMPDIR:-/tmp}/ygg-install.XXXXXX")
install_temporary=
cleanup() {
    if [ -n "$install_temporary" ]; then
        rm -f "$install_temporary"
    fi
    rm -rf "$work_directory"
}
trap cleanup EXIT HUP INT TERM

trusted_release_url() {
    case "$1" in
        https://github.com/*|https://github.com:443/*|\
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

install_binary() {
    source_binary=$1
    expected_version=$2
    if [ ! -f "$source_binary" ] || [ -L "$source_binary" ]; then
        printf 'Ygg release binary is not a regular file\n' >&2
        return 1
    fi
    chmod 0755 "$source_binary"
    binary_version=$("$source_binary" --version)
    if [ "$binary_version" != "ygg $expected_version" ]; then
        printf 'Ygg binary version mismatch: %s\n' "$binary_version" >&2
        return 1
    fi

    if [ -e "$install_directory" ] && [ ! -d "$install_directory" ]; then
        printf 'installation path is not a directory: %s\n' "$install_directory" >&2
        return 1
    fi
    mkdir -p "$install_directory"
    destination="$install_directory/ygg"
    if { [ -e "$destination" ] || [ -L "$destination" ]; } \
        && [ ! -f "$destination" ]; then
        printf 'Ygg destination is not a regular file: %s\n' "$destination" >&2
        return 1
    fi

    install_temporary=$(mktemp "$install_directory/.ygg.XXXXXX")
    cp "$source_binary" "$install_temporary"
    chmod 0755 "$install_temporary"
    mv -f "$install_temporary" "$destination"
    install_temporary=
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
    if ! command -v cargo >/dev/null 2>&1; then
        printf '%s\n' \
            "Source installation requires Rust 1.86 or newer." \
            "Install Rust from https://rustup.rs/ and run this installer again." >&2
        exit 1
    fi
    printf 'Building Ygg %s from source\n' "$tag"
    source_root="$work_directory/source-root"
    cargo install \
        --locked \
        --git "https://github.com/$repository" \
        --tag "$tag" \
        --bin ygg \
        --root "$source_root" \
        ygg-coding-agent
    install_binary "$source_root/bin/ygg" "$version"
else
    for command in curl tar uname awk; do
        if ! command -v "$command" >/dev/null 2>&1; then
            printf 'required installer command is unavailable: %s\n' "$command" >&2
            exit 1
        fi
    done

    target=$(resolve_target)
    archive_name="ygg-$version-$target.tar.gz"
    archive="$work_directory/$archive_name"
    checksums="$work_directory/$checksum_asset"
    printf 'Downloading Ygg %s for %s\n' "$version" "$target"
    download_release_file "$release_base/$checksum_asset" "$checksums"
    bounded_file "$checksums" 1048576
    download_release_file "$release_base/$archive_name" "$archive"
    bounded_file "$archive" 134217728

    if ! expected_sha256=$(awk -v name="$archive_name" '
        $2 == name || $2 == "./" name { value = $1; count += 1 }
        END { if (count != 1) exit 1; print value }
    ' "$checksums"); then
        printf 'release checksum manifest does not contain exactly one entry for %s\n' "$archive_name" >&2
        exit 1
    fi
    expected_sha256=$(printf '%s' "$expected_sha256" | tr 'A-F' 'a-f')
    case "$expected_sha256" in
        *[!0-9a-f]*|'')
            printf 'release checksum is malformed for %s\n' "$archive_name" >&2
            exit 1
            ;;
    esac
    if [ "${#expected_sha256}" -ne 64 ]; then
        printf 'release checksum is malformed for %s\n' "$archive_name" >&2
        exit 1
    fi
    actual_sha256=$(sha256_file "$archive")
    if [ "$actual_sha256" != "$expected_sha256" ]; then
        printf 'checksum mismatch for %s\n' "$archive_name" >&2
        exit 1
    fi

    package="ygg-$version-$target"
    entries="$work_directory/archive-entries"
    expected_entries="$work_directory/expected-entries"
    tar -tzf "$archive" | LC_ALL=C sort > "$entries"
    printf '%s\n' \
        "$package/" \
        "$package/LICENSE" \
        "$package/ygg" \
        | LC_ALL=C sort > "$expected_entries"
    if ! cmp -s "$expected_entries" "$entries"; then
        printf 'release archive has an unexpected layout\n' >&2
        exit 1
    fi
    types="$work_directory/archive-types"
    expected_types="$work_directory/expected-types"
    tar -tvzf "$archive" | awk '{print substr($1, 1, 1)}' | LC_ALL=C sort > "$types"
    printf '%s\n' - - d | LC_ALL=C sort > "$expected_types"
    if ! cmp -s "$expected_types" "$types"; then
        printf 'release archive contains links or unexpected entry types\n' >&2
        exit 1
    fi

    extraction="$work_directory/extracted"
    mkdir -p "$extraction"
    tar -xzf "$archive" -C "$extraction"
    install_binary "$extraction/$package/ygg" "$version"
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
