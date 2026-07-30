#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
    echo "usage: scripts/smoke-ygg-serve-installed.sh /absolute/path/to/ygg" >&2
    exit 2
fi

binary=$1
case "$binary" in
    /*) ;;
    *)
        binary_directory=$(cd "$(dirname "$binary")" && pwd)
        binary="$binary_directory/$(basename "$binary")"
        ;;
esac
if [ ! -x "$binary" ]; then
    echo "installed ygg binary is not executable: $binary" >&2
    exit 2
fi

script_directory=$(cd "$(dirname "$0")" && pwd)
repository_directory=$(cd "$script_directory/.." && pwd)
expected_directory="$repository_directory/extensions/ygg-serve/web"
work_directory=$(mktemp -d "${TMPDIR:-/tmp}/ygg-serve-smoke.XXXXXX")
server_pid=

cleanup() {
    if [ -n "$server_pid" ]; then
        kill "$server_pid" >/dev/null 2>&1 || true
        wait "$server_pid" >/dev/null 2>&1 || true
    fi
    rm -rf "$work_directory"
}
trap cleanup EXIT HUP INT TERM

mkdir -p \
    "$work_directory/config" \
    "$work_directory/run" \
    "$work_directory/download/assets"
server_log="$work_directory/server.log"
cookie_jar="$work_directory/cookies"
: >"$cookie_jar"

(
    cd "$work_directory/run"
    exec env -i \
        PATH="$PATH" \
        XDG_CONFIG_HOME="$work_directory/config" \
        OPENAI_API_KEY=ygg-serve-smoke-not-a-secret \
        YGG_MODEL=gpt-5.4 \
        YGG_SESSION_DIR="$work_directory/sessions" \
        YGG_OFFLINE=true \
        YGG_EXTENSIONS= \
        YGG_TRUSTED_EXTENSIONS= \
        "$binary" serve --no-open --port 0
) >"$server_log" 2>&1 &
server_pid=$!

launch_url=
attempt=0
while [ "$attempt" -lt 100 ]; do
    launch_url=$(sed -n 's/^Open ygg once: //p' "$server_log" | sed -n '1p')
    if [ -n "$launch_url" ]; then
        break
    fi
    if ! kill -0 "$server_pid" >/dev/null 2>&1; then
        echo "installed ygg exited before publishing its launch URL" >&2
        sed -E 's#(/__ygg/launch/)[0-9a-f]{64}#\1<redacted>#g' "$server_log" >&2
        exit 1
    fi
    attempt=$((attempt + 1))
    sleep 0.1
done
if [ -z "$launch_url" ]; then
    echo "installed ygg did not publish a launch URL in time" >&2
    exit 1
fi
if ! printf '%s\n' "$launch_url" \
    | grep -Eq '^http://127\.0\.0\.1:[0-9]+/__ygg/launch/[0-9a-f]{64}$'; then
    echo "installed ygg published a malformed or non-loopback launch URL" >&2
    exit 1
fi

origin=${launch_url%%/__ygg/launch/*}
curl -fsS \
    --noproxy '*' \
    --proto '=http' \
    --connect-timeout 2 \
    --max-time 10 \
    --cookie "$cookie_jar" \
    --cookie-jar "$cookie_jar" \
    --dump-header "$work_directory/launch.headers" \
    --output "$work_directory/launch.body" \
    "$launch_url"
tr -d '\r' <"$work_directory/launch.headers" >"$work_directory/launch.headers.clean"
grep -Eq '^HTTP/[0-9.]+ 303 ' "$work_directory/launch.headers.clean"
grep -Fxi "location: /" "$work_directory/launch.headers.clean" >/dev/null
grep -Fi "set-cookie: " "$work_directory/launch.headers.clean" \
    | grep -Fi "HttpOnly" \
    | grep -Fi "SameSite=Strict" >/dev/null

curl -fsS \
    --noproxy '*' \
    --proto '=http' \
    --connect-timeout 2 \
    --max-time 10 \
    --cookie "$cookie_jar" \
    --dump-header "$work_directory/index.headers" \
    --output "$work_directory/download/index.html" \
    "$origin/"
curl -fsS \
    --noproxy '*' \
    --proto '=http' \
    --connect-timeout 2 \
    --max-time 10 \
    --cookie "$cookie_jar" \
    --dump-header "$work_directory/app-js.headers" \
    --output "$work_directory/download/assets/app.js" \
    "$origin/assets/app.js"
curl -fsS \
    --noproxy '*' \
    --proto '=http' \
    --connect-timeout 2 \
    --max-time 10 \
    --cookie "$cookie_jar" \
    --dump-header "$work_directory/app-css.headers" \
    --output "$work_directory/download/assets/app.css" \
    "$origin/assets/app.css"
curl -fsS \
    --noproxy '*' \
    --proto '=http' \
    --connect-timeout 2 \
    --max-time 10 \
    --cookie "$cookie_jar" \
    --dump-header "$work_directory/files-panel-chunk.headers" \
    --output "$work_directory/download/assets/chunk-FilesPanel.js" \
    "$origin/assets/chunk-FilesPanel.js"
curl -fsS \
    --noproxy '*' \
    --proto '=http' \
    --connect-timeout 2 \
    --max-time 10 \
    --cookie "$cookie_jar" \
    --dump-header "$work_directory/jsx-runtime-chunk.headers" \
    --output "$work_directory/download/assets/chunk-jsx-runtime.js" \
    "$origin/assets/chunk-jsx-runtime.js"
curl -fsS \
    --noproxy '*' \
    --proto '=http' \
    --connect-timeout 2 \
    --max-time 10 \
    --cookie "$cookie_jar" \
    --dump-header "$work_directory/markdown-chunk.headers" \
    --output "$work_directory/download/assets/chunk-MarkdownMessage.js" \
    "$origin/assets/chunk-MarkdownMessage.js"

cmp "$work_directory/download/index.html" "$expected_directory/index.html"
cmp "$work_directory/download/assets/app.js" "$expected_directory/assets/app.js"
cmp "$work_directory/download/assets/app.css" "$expected_directory/assets/app.css"
cmp \
    "$work_directory/download/assets/chunk-FilesPanel.js" \
    "$expected_directory/assets/chunk-FilesPanel.js"
cmp \
    "$work_directory/download/assets/chunk-jsx-runtime.js" \
    "$expected_directory/assets/chunk-jsx-runtime.js"
cmp \
    "$work_directory/download/assets/chunk-MarkdownMessage.js" \
    "$expected_directory/assets/chunk-MarkdownMessage.js"

(
    cd "$work_directory/download"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum -c "$expected_directory/SHA256SUMS"
    else
        shasum -a 256 -c "$expected_directory/SHA256SUMS"
    fi
)

tr -d '\r' <"$work_directory/index.headers" >"$work_directory/index.headers.clean"
tr -d '\r' <"$work_directory/app-js.headers" >"$work_directory/app-js.headers.clean"
tr -d '\r' <"$work_directory/app-css.headers" >"$work_directory/app-css.headers.clean"
tr -d '\r' \
    <"$work_directory/files-panel-chunk.headers" \
    >"$work_directory/files-panel-chunk.headers.clean"
tr -d '\r' \
    <"$work_directory/jsx-runtime-chunk.headers" \
    >"$work_directory/jsx-runtime-chunk.headers.clean"
tr -d '\r' \
    <"$work_directory/markdown-chunk.headers" \
    >"$work_directory/markdown-chunk.headers.clean"
bundle_sha256=$(cat "$expected_directory/bundle.sha256")
expected_csp="content-security-policy: default-src 'self'; base-uri 'none'; connect-src 'self'; font-src 'self' data:; form-action 'none'; frame-ancestors 'none'; img-src 'self' data: blob:; object-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'"

grep -Fxi "content-type: text/html; charset=utf-8" "$work_directory/index.headers.clean" >/dev/null
grep -Fxi "content-type: text/javascript; charset=utf-8" "$work_directory/app-js.headers.clean" >/dev/null
grep -Fxi "content-type: text/css; charset=utf-8" "$work_directory/app-css.headers.clean" >/dev/null
grep -Fxi "content-type: text/javascript; charset=utf-8" "$work_directory/files-panel-chunk.headers.clean" >/dev/null
grep -Fxi "content-type: text/javascript; charset=utf-8" "$work_directory/jsx-runtime-chunk.headers.clean" >/dev/null
grep -Fxi "content-type: text/javascript; charset=utf-8" "$work_directory/markdown-chunk.headers.clean" >/dev/null
for headers in \
    "$work_directory/index.headers.clean" \
    "$work_directory/app-js.headers.clean" \
    "$work_directory/app-css.headers.clean" \
    "$work_directory/files-panel-chunk.headers.clean" \
    "$work_directory/jsx-runtime-chunk.headers.clean" \
    "$work_directory/markdown-chunk.headers.clean"
do
    grep -Fxi "cache-control: no-store" "$headers" >/dev/null
    grep -Fxi "x-content-type-options: nosniff" "$headers" >/dev/null
    grep -Fxi "referrer-policy: no-referrer" "$headers" >/dev/null
    grep -Fxi "$expected_csp" "$headers" >/dev/null
    grep -Fxi "x-ygg-web-bundle: $bundle_sha256" "$headers" >/dev/null
done

if grep -Fq "graphical shell is not bundled" "$work_directory/download/index.html"; then
    echo "installed ygg served the retired placeholder shell" >&2
    exit 1
fi

printf 'installed ygg served the checked web bundle %s\n' "$bundle_sha256"
