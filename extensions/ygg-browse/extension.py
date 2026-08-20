#!/usr/bin/env python3
"""Self-contained API 0.2 entrypoint for the official Ygg Browse bundle."""

from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(ROOT / "vendor"))
sys.path.insert(0, str(ROOT))

from ygg_browse.runtime import main  # noqa: E402


if __name__ == "__main__":
    main()
