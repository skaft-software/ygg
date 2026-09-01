#!/usr/bin/env python3
"""Fetch and freeze the public, redacted Lite task manifest.

The parquet file is public and contains evaluator-only columns.  It is never
mounted into an agent container.  ``--full-output`` is intentionally required
only by the evaluator scripts and is written owner-only.
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from common import (  # noqa: E402
    DATASET_COLUMNS,
    DATASET_REPOSITORY,
    DATASET_REVISION,
    DATASET_ROWS,
    DATASET_SHA256,
    DATASET_URL,
    ROOT,
    ensure_dataset,
    load_rows,
    public_task,
    sha256_file,
    write_json,
    write_jsonl,
)

PUBLIC_ROOT = Path("/workspace/ygg/docs/benchmarks/swe-bench-live-lite-v0.6.3")


def public_path(path: Path) -> str:
    """Represent a local input path without publishing its machine root."""
    resolved = path.resolve()
    try:
        relative = resolved.relative_to(ROOT)
    except ValueError:
        return "/external/artifact"
    return str(PUBLIC_ROOT / relative)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--parquet", type=Path, default=ROOT / "data/lite.parquet")
    parser.add_argument(
        "--public-output",
        type=Path,
        default=ROOT / "data/agent_tasks.jsonl",
        help="redacted task manifest safe to inspect (not mounted by the runner)",
    )
    parser.add_argument(
        "--full-output",
        type=Path,
        help="owner-only full JSONL for official evaluator use; never pass to Ygg",
    )
    parser.add_argument(
        "--manifest",
        type=Path,
        default=ROOT / "data/dataset-manifest.json",
    )
    args = parser.parse_args()

    parquet = args.parquet.resolve()
    if not parquet.is_file():
        from common import fetch_dataset

        fetch_dataset(parquet)
    ensure_dataset(parquet)
    rows = load_rows(parquet)
    write_jsonl(args.public_output, (public_task(row) for row in rows))
    if args.full_output:
        write_jsonl(args.full_output, rows, private=True)

    write_json(
        args.manifest,
        {
            "schema_version": "swebench-live-lite-dataset-v1",
            "dataset": DATASET_REPOSITORY,
            "revision": DATASET_REVISION,
            "parquet_path": public_path(parquet),
            "parquet_sha256": sha256_file(parquet),
            "expected_parquet_sha256": DATASET_SHA256,
            "source_url": DATASET_URL,
            "split": "lite",
            "row_count": len(rows),
            "expected_row_count": DATASET_ROWS,
            "columns": list(DATASET_COLUMNS),
            "agent_manifest_path": public_path(args.public_output),
            "full_manifest_path": public_path(args.full_output) if args.full_output else None,
            "local_paths_are_public_placeholders": True,
            "privileged_columns_are_excluded_from_agent_manifest": True,
        },
    )
    print(f"frozen {len(rows)} rows: {parquet}")
    print(f"public manifest: {args.public_output}")
    if args.full_output:
        print(f"private evaluator manifest: {args.full_output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
