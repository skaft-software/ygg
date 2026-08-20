#!/usr/bin/env python3
"""Self-contained package entrypoint; stdout belongs to JSON-RPC in run mode."""

import os
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parent
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))


def _configured_python(argv):
    """Return the interpreter from one fully validated inert configuration.

    This is an exec bootstrap, not an installer: the configured environment
    must already exist. Configuration ownership, permissions, bounds, schema,
    and duplicate keys are validated before any executable path is honored.
    """

    if any(flag in argv for flag in ("--check-config", "--contract")):
        return None
    config = None
    for index, argument in enumerate(argv):
        if argument == "--config" and index + 1 < len(argv):
            config = Path(argv[index + 1])
            break
        if argument.startswith("--config="):
            config = Path(argument.split("=", 1)[1])
            break

    try:
        from ygg_hermes_memory.config import ConfigError, load_config

        loaded = load_config(config)
        selected = loaded.environment.python if loaded.environment is not None else None
        if selected is None:
            return None
        target = Path(os.path.abspath(os.fspath(selected.expanduser())))
        resolved_target = target.resolve(strict=True)
        if not resolved_target.is_file() or not os.access(target, os.X_OK):
            return None
        return target
    except (ConfigError, OSError, ValueError):
        return None


target_python = _configured_python(sys.argv[1:])
if target_python is not None:
    current_python = Path(os.path.abspath(sys.executable))
    if target_python != current_python:
        os.execv(
            str(target_python),
            [str(target_python), str(Path(__file__).resolve()), *sys.argv[1:]],
        )

VENDOR = ROOT / "vendor"
if str(VENDOR) not in sys.path:
    sys.path.insert(0, str(VENDOR))

from ygg_hermes_memory.runtime import main


if __name__ == "__main__":
    raise SystemExit(main())
