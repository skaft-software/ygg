#!/usr/bin/env bash
set -euo pipefail

usage() {
    printf 'usage: %s PACKAGE_DIRECTORY VERSION\n' "$0" >&2
}
if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then
    usage
    exit 0
fi
if [[ $# -ne 2 ]]; then
    usage
    exit 2
fi

package_directory=$1
version=$2
repository_directory=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
if [[ ! -d "$package_directory" || -L "$package_directory" ]]; then
    printf 'npm package directory must be a real directory: %s\n' "$package_directory" >&2
    exit 1
fi
for command in npm uname; do
    command -v "$command" >/dev/null 2>&1 || {
        printf 'required npm install test command is unavailable: %s\n' "$command" >&2
        exit 1
    }
done

case "$(uname -s):$(uname -m)" in
    Darwin:arm64|Darwin:aarch64) platform_artifact="ygg-darwin-arm64-$version.tgz" ;;
    Darwin:x86_64)
        if command -v sysctl >/dev/null 2>&1 \
            && [[ "$(sysctl -in sysctl.proc_translated 2>/dev/null || true)" == 1 ]]; then
            platform_artifact="ygg-darwin-arm64-$version.tgz"
        else
            platform_artifact="ygg-darwin-x64-$version.tgz"
        fi
        ;;
    Linux:x86_64|Linux:amd64) platform_artifact="ygg-linux-x64-gnu-$version.tgz" ;;
    *)
        printf 'npm install smoke does not support this test host: %s %s\n' "$(uname -s)" "$(uname -m)" >&2
        exit 1
        ;;
esac

launcher="$package_directory/ygg-$version.tgz"
platform="$package_directory/$platform_artifact"
[[ -f "$launcher" && -f "$platform" ]] || {
    printf 'required local npm tarballs are missing\n' >&2
    exit 1
}

work_directory=$(mktemp -d "${TMPDIR:-/tmp}/ygg-npm-install-test.XXXXXX")
trap 'rm -rf "$work_directory"' EXIT
prefix="$work_directory/prefix"
cache="$work_directory/cache"
home="$work_directory/home"
probe_directory="$work_directory/probe"
sentinel="$home/user-data-sentinel"
mkdir -p "$prefix" "$cache" "$home" "$probe_directory"
printf '%s\n' 'must survive npm uninstall' > "$sentinel"

# Populate npm's local cache from the four produced files. The actual install
# remains offline, so optional dependency resolution cannot silently reach a
# registry and no lifecycle hook can become a network-running installer.
for artifact in "$package_directory"/*.tgz; do
    npm cache add --cache "$cache" --ignore-scripts "$artifact" >/dev/null
done
NPM_CONFIG_REGISTRY=http://127.0.0.1:9 \
NPM_CONFIG_CACHE="$cache" \
NPM_CONFIG_OFFLINE=true \
NPM_CONFIG_IGNORE_SCRIPTS=true \
NPM_CONFIG_AUDIT=false \
NPM_CONFIG_FUND=false \
HOME="$home" \
npm install \
    --global \
    --prefix "$prefix" \
    --cache "$cache" \
    --offline \
    --ignore-scripts \
    --no-audit \
    --no-fund \
    "$launcher" \
    "$platform" >/dev/null

bin_directory="$prefix/bin"
for command in ygg ygg-host; do
    [[ -x "$bin_directory/$command" ]] || {
        printf 'npm did not install %s\n' "$command" >&2
        exit 1
    }
done
PATH="$bin_directory:$PATH"
export PATH

[[ "$(HOME="$home" ygg --version)" == "ygg $version" ]]
HOME="$home" ygg --help >/dev/null
frame=$(HOME="$home" ygg-host)
[[ "$(printf '%s\n' "$frame" | awk 'END { print NR }')" == 1 ]]
printf '%s\n' "$frame" | grep -F '"type":"hello"' >/dev/null

probe_output=$(cd "$probe_directory" && HOME="$home" YGG_NPM_TEST_ENV=preserved ygg --probe argv-value)
expected_probe_directory=$(CDPATH= cd -P "$probe_directory" && pwd -P)
printf '%s\n' "$probe_output" | grep -F "cwd=$expected_probe_directory" >/dev/null
printf '%s\n' "$probe_output" | grep -F 'arg=argv-value' >/dev/null
printf '%s\n' "$probe_output" | grep -F 'env=preserved' >/dev/null

set +e
HOME="$home" ygg --exit 37
exit_status=$?
set -e
[[ "$exit_status" == 37 ]]

# Exercise the npm-created symlink and the alternate hoisted optional-package
# layout. Both paths must still reach the same native executable without a
# JavaScript process in the execution path.
ln -s "$bin_directory/ygg" "$work_directory/ygg-symlink"
[[ "$(HOME="$home" "$work_directory/ygg-symlink" --version)" == "ygg $version" ]]
public_root="$prefix/lib/node_modules/@skaft-software/ygg"
platform_name=${platform_artifact%-"$version".tgz}
platform_name=${platform_name#ygg-}
nested_root="$public_root/node_modules/@skaft-software/$platform_name"
hoisted_root="$prefix/lib/node_modules/@skaft-software/$platform_name"
if [[ -d "$nested_root" && ! -L "$nested_root" && ! -e "$hoisted_root" ]]; then
    mv "$nested_root" "$hoisted_root"
    [[ "$(HOME="$home" ygg --version)" == "ygg $version" ]]
fi

NPM_CONFIG_CACHE="$cache" HOME="$home" npm uninstall \
    --global \
    --prefix "$prefix" \
    --offline \
    --ignore-scripts \
    --no-audit \
    --no-fund \
    @skaft-software/ygg >/dev/null
[[ ! -e "$bin_directory/ygg" && ! -e "$bin_directory/ygg-host" ]]
[[ "$(cat "$sentinel")" == 'must survive npm uninstall' ]]
printf 'npm local install, launcher, and uninstall tests passed for %s\n' "$version"
