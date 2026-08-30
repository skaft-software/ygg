#!/usr/bin/env python3
"""Run isolated SWE-bench agent containers concurrently.

Each task is still executed exactly once by a separate ``run_agent.py``
process.  The launcher only changes inter-task scheduling; it does not share a
workspace, credentials copy, container, or session directory between tasks.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import os
import subprocess
import sys
from pathlib import Path
from typing import Any

SCRIPT_DIR = Path(__file__).resolve().parent
BENCHMARK_ROOT = SCRIPT_DIR.parent
sys.path.insert(0, str(SCRIPT_DIR))
from common import (  # noqa: E402
    DATASET_REVISION,
    DATASET_ROWS,
    DOCKER_PLATFORM,
    IMAGE_ARCH,
    IMAGE_NAMESPACE,
    IMAGE_TAG,
    TASK_WORKSPACE,
    YGG_BINARY_TARGET,
    YGG_COMMIT,
    YGG_RELEASE_REF,
    YGG_VERSION,
    ensure_dataset,
    load_rows,
    load_selection,
    public_task,
    rows_by_id,
    sha256_file,
    system_prompt_identity,
    tool_schema_identity,
    write_json,
    write_jsonl,
)


def safe_name(value: str) -> str:
    import re

    return re.sub(r"[^a-zA-Z0-9_.-]+", "-", value).strip("-")[:80] or "task"


def now_iso() -> str:
    from datetime import datetime, timezone

    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")


def child_command(
    *,
    index: int,
    instance_id: str,
    args: argparse.Namespace,
    selection_path: Path,
    instance_dir: Path,
) -> list[str]:
    command = [
        args.python,
        str(SCRIPT_DIR / "run_agent.py"),
        "--parquet",
        str(args.parquet.resolve()),
        "--selection",
        str(selection_path),
        "--binary",
        str(args.binary.resolve()),
        "--credential-dir",
        str(args.credential_dir.resolve()),
        "--output-dir",
        str(instance_dir),
        "--run-id",
        f"{args.run_id}-{index:03d}-{safe_name(instance_id)}",
        "--model",
        args.model,
        "--reasoning",
        args.reasoning,
        "--timeout-seconds",
        str(args.timeout_seconds),
        "--image-arch",
        args.image_arch,
        "--image-tag",
        args.image_tag,
        "--workers",
        "1",
    ]
    if args.ygg_source:
        command.extend(["--ygg-source", str(args.ygg_source.resolve())])
    if args.keep_images:
        command.append("--keep-images")
    return command


def run_child(
    *,
    index: int,
    row: dict[str, Any],
    args: argparse.Namespace,
    selection_path: Path,
    instance_dir: Path,
    log_path: Path,
) -> dict[str, Any]:
    command = child_command(
        index=index,
        instance_id=row["instance_id"],
        args=args,
        selection_path=selection_path,
        instance_dir=instance_dir,
    )
    started = now_iso()
    return_code: int | None = None
    error: str | None = None
    try:
        with log_path.open("w", encoding="utf-8") as log:
            log.write(json.dumps({"started": started, "command": command}, sort_keys=True) + "\n")
            log.flush()
            completed = subprocess.run(
                command,
                cwd=BENCHMARK_ROOT,
                stdout=log,
                stderr=subprocess.STDOUT,
                text=True,
                check=False,
            )
            return_code = completed.returncode
    except Exception as caught:  # preserve a launcher failure as task evidence
        error = f"{type(caught).__name__}: {caught}"
        log_path.write_text(error + "\n", encoding="utf-8")

    summary_path = instance_dir / "run-summary.json"
    child_summary: dict[str, Any] = {}
    if summary_path.is_file():
        try:
            value = json.loads(summary_path.read_text(encoding="utf-8"))
            if isinstance(value, dict):
                child_summary = value
        except (OSError, UnicodeError, json.JSONDecodeError):
            pass
    summaries = child_summary.get("summaries")
    summary = summaries[0] if isinstance(summaries, list) and summaries else None
    if not isinstance(summary, dict):
        patch_path = instance_dir / "final_patch.diff"
        patch = patch_path.read_text(encoding="utf-8") if patch_path.is_file() else ""
        summary = {
            "instance_id": row["instance_id"],
            "repo": row["repo"],
            "base_commit": row["base_commit"],
            "termination_reason": "launcher_failure" if error else "child_process_failure",
            "process_kind": "launcher_failure" if error else "child_process_failure",
            "patch_bytes": len(patch.encode("utf-8")),
            "patch_lines": len(patch.splitlines()),
            "has_patch": bool(patch.strip()),
            "wall_seconds": None,
            "instance_dir": str(instance_dir),
        }
    return {
        "index": index,
        "instance_id": row["instance_id"],
        "return_code": return_code,
        "error": error,
        "summary": summary,
        "instance_dir": str(instance_dir),
        "log_path": str(log_path),
        "finished": now_iso(),
    }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--parquet", type=Path, default=BENCHMARK_ROOT / "data/lite.parquet")
    parser.add_argument("--selection", type=Path, required=True)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--credential-dir", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--model", default="gpt-5.6-sol")
    parser.add_argument("--reasoning", default="max")
    parser.add_argument("--timeout-seconds", type=int, default=1800)
    parser.add_argument("--image-arch", default=IMAGE_ARCH, choices=["x86_64", "arm64"])
    parser.add_argument("--image-tag", default=IMAGE_TAG)
    parser.add_argument("--ygg-source", type=Path)
    parser.add_argument("--concurrency", type=int, default=20)
    parser.add_argument("--python", default=sys.executable, help="Python executable for child runners")
    parser.add_argument("--keep-images", action="store_true")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.concurrency < 1:
        raise SystemExit("--concurrency must be positive")
    if args.timeout_seconds < 1:
        raise SystemExit("--timeout-seconds must be positive")
    parquet = ensure_dataset(args.parquet.resolve())
    rows = load_rows(parquet)
    by_id = rows_by_id(rows)
    selected_ids = load_selection(args.selection.resolve())
    missing = sorted(set(selected_ids) - set(by_id))
    if missing:
        raise SystemExit(f"selection contains IDs outside the pinned dataset: {missing}")
    binary = args.binary.resolve()
    credential_dir = args.credential_dir.resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise SystemExit(f"Ygg binary is not executable: {binary}")
    if not credential_dir.is_dir():
        raise SystemExit(f"credential directory is not a directory: {credential_dir}")

    output_dir = args.output_dir.resolve()
    if output_dir.exists() and any(output_dir.iterdir()):
        raise SystemExit(f"refusing to overwrite non-empty run directory: {output_dir}")
    instances_dir = output_dir / "instances"
    selections_dir = output_dir / "work" / "selections"
    logs_dir = output_dir / "launcher-logs"
    instances_dir.mkdir(parents=True, exist_ok=True)
    selections_dir.mkdir(parents=True, exist_ok=True)
    logs_dir.mkdir(parents=True, exist_ok=True)

    start = now_iso()
    config = {
        "schema_version": "swebench-live-parallel-run-v1",
        "phase": args.run_id,
        "run_id": args.run_id,
        "start_timestamp": start,
        "dataset": {
            "repository": "SWE-bench-Live/SWE-bench-Live",
            "revision": DATASET_REVISION,
            "parquet_sha256": sha256_file(parquet),
            "split": "lite",
            "nominal_count": DATASET_ROWS,
        },
        "selection": {
            "path": str(args.selection.resolve()),
            "sha256": sha256_file(args.selection.resolve()),
            "count": len(selected_ids),
        },
        "ygg": {
            "repository": "https://github.com/skaft-software/ygg.git",
            "commit": YGG_COMMIT,
            "release_ref": YGG_RELEASE_REF,
            "version": YGG_VERSION,
            "binary": str(binary),
            "binary_sha256": sha256_file(binary),
            "target": YGG_BINARY_TARGET,
            "source_checkout": str(args.ygg_source.resolve()) if args.ygg_source else None,
        },
        "model": args.model,
        "provider": "Codex OAuth credential copied into disposable task containers",
        "reasoning": args.reasoning,
        "k": 1,
        "task_concurrency": args.concurrency,
        "launcher_workers": args.concurrency,
        "child_task_workers": 1,
        "timeout_seconds": args.timeout_seconds,
        "remove_images_after_task": not args.keep_images,
        "image_namespace": IMAGE_NAMESPACE,
        "image_arch": args.image_arch,
        "image_tag": args.image_tag,
        "docker_platform": DOCKER_PLATFORM,
        "task_workspace": TASK_WORKSPACE,
        "agent_prompt": "exact pinned problem_statement field; no hints/gold/evaluator fields; raw prompt retained only in native trajectory",
        "system_prompt": system_prompt_identity(args.ygg_source.resolve() if args.ygg_source else None),
        "tool_schema": tool_schema_identity(),
        "parallelism_policy": {
            "each_task_once": True,
            "selective_retries": False,
            "isolated_child_output_directories": True,
            "shared_host_dataset_not_mounted": True,
            "shared_provider_concurrency_effects_must_be_reported": True,
        },
        "status": "running",
    }
    write_json(output_dir / "manifest.json", config)

    public_selections: dict[int, Path] = {}
    for index, instance_id in enumerate(selected_ids, start=1):
        row = by_id[instance_id]
        selection_path = selections_dir / f"{index:03d}-{safe_name(instance_id)}.json"
        write_json(
            selection_path,
            {
                "dataset_revision": DATASET_REVISION,
                "instances": [public_task(row)],
                "method": "parallel-launcher-single-task-subselection",
                "schema_version": "swebench-live-selection-v1",
                "size": 1,
                "source_selection": str(args.selection.resolve()),
            },
        )
        public_selections[index] = selection_path

    results: list[dict[str, Any]] = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.concurrency) as executor:
        futures = {
            executor.submit(
                run_child,
                index=index,
                row=by_id[instance_id],
                args=args,
                selection_path=public_selections[index],
                instance_dir=instances_dir / safe_name(instance_id),
                log_path=logs_dir / f"{index:03d}-{safe_name(instance_id)}.log",
            ): (index, instance_id)
            for index, instance_id in enumerate(selected_ids, start=1)
        }
        for completed_count, future in enumerate(concurrent.futures.as_completed(futures), start=1):
            result = future.result()
            results.append(result)
            write_json(
                output_dir / "progress.json",
                {
                    "completed": completed_count,
                    "total": len(selected_ids),
                    "task_concurrency": args.concurrency,
                    "last": result,
                },
            )
            print(
                f"[{completed_count}/{len(selected_ids)}] {result['instance_id']} "
                f"return_code={result['return_code']}",
                flush=True,
            )

    results.sort(key=lambda value: value["index"])
    summaries = [result["summary"] for result in results]
    predictions: list[dict[str, Any]] = []
    for result in results:
        instance_dir = Path(result["instance_dir"])
        prediction_path = instance_dir / "predictions.jsonl"
        if prediction_path.is_file():
            for line in prediction_path.read_text(encoding="utf-8").splitlines():
                if line.strip():
                    predictions.append(json.loads(line))
                    break
        if not any(item.get("instance_id") == result["instance_id"] for item in predictions):
            patch_path = instance_dir / "final_patch.diff"
            predictions.append(
                {
                    "instance_id": result["instance_id"],
                    "model_name_or_path": f"ygg-{args.model}-{args.reasoning}",
                    "model_patch": patch_path.read_text(encoding="utf-8") if patch_path.is_file() else "",
                }
            )
    predictions.sort(key=lambda value: selected_ids.index(value["instance_id"]))
    write_jsonl(output_dir / "predictions.jsonl", predictions)
    aggregate = {
        "schema_version": "swebench-live-run-summary-v1",
        "run_id": args.run_id,
        "start_timestamp": start,
        "finish_timestamp": now_iso(),
        "nominal_selection_count": len(selected_ids),
        "prediction_count": len(predictions),
        "task_concurrency": args.concurrency,
        "summaries": summaries,
        "launcher_results": results,
    }
    write_json(output_dir / "run-summary.json", aggregate)
    config["status"] = "complete"
    config["finish_timestamp"] = aggregate["finish_timestamp"]
    config["prediction_path"] = str(output_dir / "predictions.jsonl")
    write_json(output_dir / "manifest.json", config)
    print(f"completed {len(summaries)} tasks; predictions: {output_dir / 'predictions.jsonl'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
