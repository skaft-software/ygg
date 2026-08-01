#!/bin/sh
set -eu

# Forward-enforcement checkpoint after the shipped Serve source, canonical release
# tooling, and then-current main were reconciled. Their independent histories
# include unrelated core/TUI work, so auditing from the earlier stacked-branch
# checkpoint would turn this gate into a blanket core allowlist. A caller may
# still pass an older ancestor to audit a wider range explicitly.
default_base_ref=63f73d655c4fe27faeae4579f379726b093ef827
base_ref=${1:-$default_base_ref}

if ! git rev-parse --verify "${base_ref}^{commit}" >/dev/null 2>&1; then
  echo "unknown ygg serve boundary base: ${base_ref}" >&2
  exit 2
fi
if ! git merge-base --is-ancestor "$base_ref" HEAD; then
  echo "ygg serve boundary base is not an ancestor of HEAD: ${base_ref}" >&2
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
    apps/web/* \
      | extensions/ygg-serve/* \
      | docs/experimental/ygg-serve/* \
      | docs/design/config-diagnostics.md \
      | docs/design/serve-lifecycle-safety.md \
      | SECURITY.md \
      | scripts/check-ygg-serve-boundaries.sh \
      | scripts/package-ygg-serve-release.sh \
      | scripts/smoke-ygg-serve-installed.sh \
      | .github/workflows/ci.yml \
      | .github/workflows/release-serve.yml \
      | .gitattributes \
      | deny.toml \
      | Cargo.toml \
      | Cargo.lock \
      | .gitignore)
      ;;
    crates/ygg-coding-agent/Cargo.toml \
      | crates/ygg-coding-agent/src/cli.rs \
      | crates/ygg-coding-agent/src/extension_package.rs \
      | crates/ygg-coding-agent/src/main.rs \
      | crates/ygg-coding-agent/src/commands.rs \
      | crates/ygg-coding-agent/src/extensions.rs \
      | crates/ygg-coding-agent/src/extensions/serve.rs)
      ;;
    # Generic agent-owned context accounting. These files do not depend on
    # Serve; the adapter only projects their public snapshots.
    crates/ygg-agent/src/agent.rs \
      | crates/ygg-agent/src/context.rs \
      | crates/ygg-agent/src/lib.rs \
      | crates/ygg-agent/tests/agent_run.rs)
      ;;
    # Generic coding-agent hardening shared by every frontend: RPC framing,
    # queue delivery, usage projection, and the explicit dispatch borrows,
    # alongside source-aware configuration and session deletion primitives.
    crates/ygg-coding-agent/src/modes/rpc.rs \
      | crates/ygg-coding-agent/src/config.rs \
      | crates/ygg-coding-agent/src/resource_resolver.rs \
      | crates/ygg-coding-agent/src/resources.rs \
      | crates/ygg-coding-agent/src/session_store.rs)
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
