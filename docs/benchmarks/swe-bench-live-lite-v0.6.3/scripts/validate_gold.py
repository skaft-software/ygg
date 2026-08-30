#!/usr/bin/env python3
"""Run the upstream gold procedure three complete times and freeze the denominator.

Gold patches are created only in the private evaluator input and are never
passed to ``run_agent.py`` or mounted into a task container.  A task is
``gold-valid`` only when the official evaluator reports it resolved on every
complete validation repetition.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
BENCHMARK_ROOT = SCRIPT_DIR.parent
sys.path.insert(0, str(SCRIPT_DIR))
from common import (  # noqa: E402
    DATASET_REVISION,
    EVALUATOR_COMMIT,
    ROOT,
    ensure_dataset,
    load_rows,
    rows_by_id,
    write_json,
)


def now() -> str:
    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--parquet", type=Path, default=BENCHMARK_ROOT / "data/lite.parquet")
    parser.add_argument("--evaluator-src", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--namespace", default="starryzhang")
    parser.add_argument("--image-tag", default="latest")
    parser.add_argument("--test-timeout-seconds", type=int, default=1800)
    parser.add_argument("--evaluator-workers", type=int, default=2)
    parser.add_argument("--repetitions", type=int, default=3)
    args = parser.parse_args()
    if args.repetitions < 1:
        parser.error("--repetitions must be positive")
    if args.evaluator_workers < 1:
        parser.error("--evaluator-workers must be positive")

    rows = load_rows(ensure_dataset(args.parquet.resolve()))
    by_id = rows_by_id(rows)
    output_dir = args.output_dir.resolve()
    if output_dir.exists() and any(output_dir.iterdir()):
        raise SystemExit(f"refusing to overwrite non-empty validation directory: {output_dir}")
    output_dir.mkdir(parents=True, exist_ok=True)
    validation_started_at = now()
    evaluator = SCRIPT_DIR / "evaluate.py"
    runs: list[dict] = []
    for number in range(1, args.repetitions + 1):
        run_dir = output_dir / f"repetition-{number}"
        command = [
            sys.executable,
            str(evaluator),
            "--parquet",
            str(args.parquet.resolve()),
            "--gold",
            "--evaluator-src",
            str(args.evaluator_src.resolve()),
            "--output-dir",
            str(run_dir),
            "--run-id",
            f"gold-validation-{number}",
            "--namespace",
            args.namespace,
            "--image-tag",
            args.image_tag,
            "--test-timeout-seconds",
            str(args.test_timeout_seconds),
            "--evaluator-workers",
            str(args.evaluator_workers),
        ]
        print(f"gold validation repetition {number}/{args.repetitions}", flush=True)
        completed = subprocess.run(command, cwd=BENCHMARK_ROOT, text=True)
        result_path = run_dir / "results.json"
        if result_path.is_file():
            try:
                result = json.loads(result_path.read_text(encoding="utf-8"))
            except json.JSONDecodeError:
                result = {}
        else:
            result = {}
        statuses = {
            item.get("instance_id"): item
            for item in result.get("instances", [])
            if isinstance(item, dict) and isinstance(item.get("instance_id"), str)
        }
        runs.append(
            {
                "repetition": number,
                "command": command,
                "process_return_code": completed.returncode,
                "results_path": str(result_path.relative_to(BENCHMARK_ROOT))
                if result_path.is_relative_to(BENCHMARK_ROOT)
                else str(result_path),
                "resolved_count": result.get("resolved_count", 0),
                "reported_count": result.get("reported_count", 0),
                "statuses": statuses,
            }
        )

    valid: list[dict] = []
    invalid: list[dict] = []
    for instance_id, row in by_id.items():
        observations = [run["statuses"].get(instance_id) for run in runs]
        resolved = [observation is not None and observation.get("resolved") is True for observation in observations]
        if all(resolved) and len(observations) == len(runs):
            valid.append(
                {
                    "instance_id": instance_id,
                    "repo": row["repo"],
                    "base_commit": row["base_commit"],
                    "validation": "resolved_on_every_repetition",
                }
            )
            continue
        values = [observation.get("resolved") if observation else None for observation in observations]
        if any(value is None for value in values):
            reason = "missing_or_evaluator_error"
        elif any(value is True for value in values) and any(value is False for value in values):
            reason = "flaky_gold_evaluation"
        else:
            reason = "gold_patch_unresolved"
        invalid.append(
            {
                "instance_id": instance_id,
                "repo": row["repo"],
                "base_commit": row["base_commit"],
                "reason": reason,
                "observations": observations,
                "resolved_values": values,
            }
        )

    summary = {
        "schema_version": "swebench-live-gold-validation-v1",
        "dataset_revision": DATASET_REVISION,
        "evaluator_commit": EVALUATOR_COMMIT,
        "validation_started_at": validation_started_at,
        "finished_at": now(),
        "policy": {
            "repetitions": args.repetitions,
            "evaluator_workers": args.evaluator_workers,
            "gold_valid_requires_resolved_on_every_repetition": True,
            "official_evaluator_only": True,
            "selective_retries": False,
            "gold_exposed_to_agent": False,
        },
        "nominal_count": len(rows),
        "gold_valid_count": len(valid),
        "invalid_count": len(invalid),
        "namespace": args.namespace,
        "image_tag": args.image_tag,
        "runs": runs,
        "valid_instances": valid,
        "invalid_instances": invalid,
    }
    # Keep the output files separately consumable as required by the protocol.
    write_json(output_dir / "gold-validation.json", summary)
    write_json(
        BENCHMARK_ROOT / "valid_instances.json",
        {
            "schema_version": "swebench-live-valid-instance-list-v1",
            "dataset_revision": DATASET_REVISION,
            "nominal_count": len(rows),
            "gold_valid_count": len(valid),
            "validation_repetitions": args.repetitions,
            "instances": valid,
        },
    )
    write_json(
        BENCHMARK_ROOT / "invalid_instances.json",
        {
            "schema_version": "swebench-live-invalid-instance-list-v1",
            "dataset_revision": DATASET_REVISION,
            "nominal_count": len(rows),
            "invalid_count": len(invalid),
            "validation_repetitions": args.repetitions,
            "instances": invalid,
        },
    )
    print(json.dumps({"nominal": len(rows), "gold_valid": len(valid), "invalid": len(invalid)}, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
