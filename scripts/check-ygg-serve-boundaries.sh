#!/bin/sh
set -eu

base_ref=${1:-c6ec60f}

if ! git rev-parse --verify "${base_ref}^{commit}" >/dev/null 2>&1; then
  echo "unknown ygg serve boundary base: ${base_ref}" >&2
  exit 2
fi

changed_files=$(
  {
    git diff --name-only "${base_ref}...HEAD"
    git diff --name-only
    git diff --name-only --cached
    git ls-files --others --exclude-standard
  } | sort -u
)

violations=""

for changed_file in $changed_files; do
  case "$changed_file" in
    apps/* \
      | extensions/ygg-serve/* \
      | docs/experimental/ygg-serve/* \
      | scripts/check-ygg-serve-boundaries.sh \
      | Cargo.toml \
      | Cargo.lock \
      | .gitignore)
      ;;
    crates/ygg-coding-agent/Cargo.toml \
      | crates/ygg-coding-agent/src/cli.rs \
      | crates/ygg-coding-agent/src/main.rs \
      | crates/ygg-coding-agent/src/extensions.rs \
      | crates/ygg-coding-agent/src/extensions/serve.rs)
      ;;
    *)
      violations="${violations}${changed_file}
"
      ;;
  esac
done

if [ -n "$violations" ]; then
  echo "ygg serve crossed its optional extension boundary:" >&2
  printf '%s' "$violations" >&2
  exit 1
fi

echo "ygg serve changes stay within the optional extension/application boundary"
