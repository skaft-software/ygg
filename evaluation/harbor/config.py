"""Pinned inputs used by the Harbor smoke and benchmark workflow."""

YGG_REPOSITORY = "https://github.com/skaft-software/ygg.git"
PINNED_YGG_COMMIT = "61677754bf69833a384bee2b29ef8eff29f37fc1"
PINNED_YGG_VERSION = "0.6.2"

HARBOR_REPOSITORY = "https://github.com/harbor-framework/harbor.git"
PINNED_HARBOR_COMMIT = "6ecebe4ae9910ee0b28a2e6e8fa30934c0b41dfa"

# Keep the first Terminal-Bench 2.1 dataset pinned. The registry's @latest alias
# is useful for smoke testing but must not silently change a reported result.
TERMINAL_BENCH_DATASET = "terminal-bench/terminal-bench-2-1@6"

# This is the subscription route used by the v0.6.2 Codex login flow. A caller
# can still pass an API-backed model explicitly with Harbor's -m option.
DEFAULT_MODEL = "gpt-5.6-sol"
DEFAULT_REASONING = "max"
DEFAULT_PROVIDER_ENV = ("OPENAI_API_KEY",)

DEFAULT_BINARY_SOURCE = "/usr/local/bin/ygg"
DEFAULT_BINARY_PATH = "/tmp/ygg"
DEFAULT_SESSION_DIR = "/logs/agent/sessions"
