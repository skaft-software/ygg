#!/usr/bin/env bash
# Sanitized reconstruction of the canonical campaign invocation. It contains no
# credential values. Run only after following README.md and staging a disposable
# owner-private Codex credential directory.
set -euo pipefail

: "${YGG_REPO:?checkout frozen at 61677754bf69833a384bee2b29ef8eff29f37fc1}"
: "${YGG_BINARY:?Linux amd64 Ygg 0.6.2 binary}"
: "${CODEX_STAGE:?disposable root-owned directory containing .ygg/credentials/codex.json}"
: "${HARBOR:?Harbor 0.22.0 executable at commit 6ecebe4ae9910ee0b28a2e6e8fa30934c0b41dfa}"
: "${JOBS_DIR:?output directory}"

expected_sha=16036929493fb12ffc4d8a553cdfcb642c3c983fb469877403808e5aabbd5f07
actual_sha=$(sha256sum "$YGG_BINARY" | awk '{print $1}')
test "$actual_sha" = "$expected_sha"
test "$($YGG_BINARY --version)" = "ygg 0.6.2"
test "$(stat -c '%u' "$CODEX_STAGE/.ygg")" = 0
test "$(stat -c '%a' "$CODEX_STAGE/.ygg/credentials/codex.json")" = 600
unset OPENAI_API_KEY OPENROUTER_API_KEY

mounts=$(python3 - "$YGG_BINARY" "$CODEX_STAGE/.ygg" <<'PY'
import json
import os
import sys

binary, credential_root = map(os.path.abspath, sys.argv[1:])
print(json.dumps([
    {
        "type": "bind",
        "source": binary,
        "target": "/usr/local/bin/ygg",
        "read_only": True,
    },
    {
        "type": "bind",
        "source": credential_root,
        "target": "/root/.ygg",
        "read_only": True,
    },
]))
PY
)

PYTHONPATH="$YGG_REPO" "$HARBOR" run \
  -d 'terminal-bench/terminal-bench-2-1@6' \
  -a evaluation.harbor.ygg_agent:Ygg \
  -m gpt-5.6-sol \
  -e docker \
  -k 5 \
  -n 20 \
  --mounts "$mounts" \
  --agent-env HOME=/root \
  --agent-kwarg reasoning=max \
  --agent-kwarg ygg_binary_sha256="$expected_sha" \
  --jobs-dir "$JOBS_DIR" \
  --job-name ygg-tb21-sol-max-k5-n20
