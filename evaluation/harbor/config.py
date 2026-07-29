"""Pinned inputs used by the Harbor smoke and benchmark workflow."""

YGG_REPOSITORY = "https://github.com/skaft-software/ygg.git"
PINNED_YGG_COMMIT = "06cc784ef52a60b173f6d04bd90d8d30954e7501"
PINNED_YGG_VERSION = "0.3.2-alpha"

HARBOR_REPOSITORY = "https://github.com/harbor-framework/harbor.git"
PINNED_HARBOR_COMMIT = "e76f7e32f5644fb9f648cd23151aac5c67492ea0"

# Harbor's published Terminal-Bench 2 package is the benchmark input. Keep the
# package reference in one place so a result can be reproduced from a job's
# resolved lock file instead of silently following the registry head.
TERMINAL_BENCH_DATASET = "terminal-bench/terminal-bench-2@2.0"

DEFAULT_MODEL = "openai/gpt-5.4"
DEFAULT_REASONING = "medium"
DEFAULT_PROVIDER_ENV = ("OPENAI_API_KEY",)

DEFAULT_BINARY_SOURCE = "/usr/local/bin/ygg"
DEFAULT_BINARY_PATH = "/tmp/ygg"
DEFAULT_SESSION_DIR = "/logs/agent/sessions"
