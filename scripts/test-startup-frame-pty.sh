#!/usr/bin/env bash
# Run the deterministic Unix PTY/frame startup regression lane.
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

exec cargo test --locked -p ygg-coding-agent --test startup_frame_pty -- "$@"
