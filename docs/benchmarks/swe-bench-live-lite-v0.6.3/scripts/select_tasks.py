#!/usr/bin/env python3
"""Select a deterministic Lite subset without looking at gold fields."""

from __future__ import annotations

import argparse
import hashlib
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from common import ROOT, ensure_dataset, load_rows, public_task, write_json  # noqa: E402


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--parquet", type=Path, default=ROOT / "data/lite.parquet")
    parser.add_argument("--size", type=int, required=True)
    parser.add_argument("--seed", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.size < 1:
        parser.error("--size must be positive")

    rows = load_rows(ensure_dataset(args.parquet.resolve()))
    ordered = sorted(
        rows,
        key=lambda row: hashlib.sha256(
            f"{args.seed}:{row['instance_id']}".encode("utf-8")
        ).hexdigest(),
    )
    selected = ordered[: args.size]
    if len(selected) != args.size:
        raise ValueError("selection is larger than the dataset")
    write_json(
        args.output,
        {
            "schema_version": "swebench-live-selection-v1",
            "seed": args.seed,
            "method": "sort by SHA-256(seed + ':' + instance_id), take first N",
            "size": len(selected),
            "dataset_revision": "a637bd46829f3132e12938c8a0ca93173a977b8e",
            "instances": [public_task(row) for row in selected],
        },
    )
    print(f"selected {len(selected)} instances into {args.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
