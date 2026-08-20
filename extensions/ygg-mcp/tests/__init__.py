"""Package-owned tests for ygg-mcp."""

from pathlib import Path
import sys

_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(_ROOT / "vendor"))
sys.path.insert(0, str(_ROOT))
