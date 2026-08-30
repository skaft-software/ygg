#!/usr/bin/env python3
"""Run the pinned official Python Lite evaluator without changing its code.

On an Apple-silicon Docker host the upstream evaluator's architecture probe sees
arm64, while the published Python-only Lite images are x86_64.  The wrapper
patches only that host probe in the evaluator subprocess and records it; grading
logic and task/evaluator files are untouched.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
BENCHMARK_ROOT = SCRIPT_DIR.parent
sys.path.insert(0, str(SCRIPT_DIR))
from common import (  # noqa: E402
    EVALUATOR_COMMIT,
    EVALUATOR_REPOSITORY,
    EVALUATOR_VERSION,
    ROOT,
    ensure_dataset,
    load_rows,
    load_selection,
    rows_by_id,
    write_json,
    write_jsonl,
)


PYTHON_ARCH_WRAPPER = "exec(" + repr(
    """import platform, runpy, subprocess
platform.machine = lambda: "x86_64"
import docker.models.images as _images
_original_pull = _images.ImageCollection.pull

def _pull(self, repository, tag=None, **kwargs):
    ref = repository if tag is None else f"{repository}:{tag}"
    if ref.startswith("starryzhang/"):
        mirror = f"mirror.gcr.io/{ref}"
        subprocess.run(["docker", "pull", "--platform", "linux/amd64", mirror], check=True, timeout=300)
        subprocess.run(["docker", "tag", mirror, ref], check=True, timeout=30)
        subprocess.run(
            ["docker", "image", "rm", mirror],
            check=False,
            timeout=30,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        return self.get(ref)
    return _original_pull(self, repository, tag=tag, **kwargs)

_images.ImageCollection.pull = _pull
runpy.run_module("swebench.harness.run_evaluation", run_name="__main__")
"""
) + ")"


def full_dataset_json(rows: list[dict], output: Path) -> Path:
    output.parent.mkdir(parents=True, exist_ok=True)
    # This file contains gold fields and is used only by the evaluator process.
    # It is owner-only and should live below private/ outside the agent output.
    write_json(output, rows, private=True)
    return output


def find_reports(eval_root: Path) -> dict[str, tuple[Path, dict]]:
    reports: dict[str, tuple[Path, dict]] = {}
    for path in sorted((eval_root / "logs/run_evaluation").glob("**/report.json")):
        try:
            value = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, UnicodeError, json.JSONDecodeError):
            continue
        if not isinstance(value, dict):
            continue
        for instance_id, report in value.items():
            if isinstance(instance_id, str) and isinstance(report, dict):
                reports[instance_id] = (path, report)
    return reports


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--parquet", type=Path, default=BENCHMARK_ROOT / "data/lite.parquet")
    parser.add_argument("--predictions", type=Path)
    parser.add_argument("--gold", action="store_true", help="use upstream gold predictions for validation only")
    parser.add_argument("--selection", type=Path, help="selection manifest; omit for all 300 rows")
    parser.add_argument("--evaluator-src", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--namespace", default="starryzhang")
    parser.add_argument("--image-tag", default="latest")
    parser.add_argument("--test-timeout-seconds", type=int, default=1800)
    parser.add_argument("--evaluator-workers", type=int, default=1)
    args = parser.parse_args()
    if args.evaluator_workers < 1:
        parser.error("--evaluator-workers must be positive")
    if args.gold == (args.predictions is not None):
        parser.error("provide exactly one of --gold or --predictions")

    source = args.evaluator_src.resolve()
    if not (source / ".git").exists():
        raise SystemExit(f"evaluator source must be a git checkout: {source}")
    revision = subprocess.check_output(
        ["git", "-C", str(source), "rev-parse", "HEAD"], text=True
    ).strip()
    if revision != EVALUATOR_COMMIT:
        raise SystemExit(f"evaluator checkout is {revision}, expected {EVALUATOR_COMMIT}")
    rows = load_rows(ensure_dataset(args.parquet.resolve()))
    by_id = rows_by_id(rows)
    selected_ids = load_selection(args.selection.resolve()) if args.selection else [row["instance_id"] for row in rows]
    missing = sorted(set(selected_ids) - set(by_id))
    if missing:
        raise SystemExit(f"selection contains IDs outside pinned dataset: {missing}")

    output_dir = args.output_dir.resolve()
    if output_dir.exists() and any(output_dir.iterdir()):
        raise SystemExit(f"refusing to overwrite non-empty evaluator directory: {output_dir}")
    output_dir.mkdir(parents=True, exist_ok=True)
    private_dir = BENCHMARK_ROOT / "private" / "evaluator-inputs"
    full_json = full_dataset_json(rows, private_dir / "lite-full.json")
    command = [
        sys.executable,
        "-c",
        PYTHON_ARCH_WRAPPER,
        "--dataset_name",
        str(full_json),
        "--split",
        "lite",
        "--namespace",
        args.namespace,
        "--max_workers",
        str(args.evaluator_workers),
        "--cache_level",
        "env",
        "--instance_image_tag",
        args.image_tag,
        "--clean",
        "false",
        "--force_rebuild",
        "false",
        "--timeout",
        str(args.test_timeout_seconds),
        "--run_id",
        args.run_id,
    ]
    if args.gold:
        command.extend(["--predictions_path", "gold"])
    else:
        command.extend(["--predictions_path", str(args.predictions.resolve())])
    command.extend(["--instance_ids", *selected_ids])
    env = os.environ.copy()
    prior_pythonpath = env.get("PYTHONPATH")
    env["PYTHONPATH"] = str(source) + ((os.pathsep + prior_pythonpath) if prior_pythonpath else "")
    started = __import__("datetime").datetime.now(__import__("datetime").timezone.utc).isoformat().replace("+00:00", "Z")
    completed = subprocess.run(
        command,
        cwd=output_dir,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    (output_dir / "official-evaluator.stdout.txt").write_text(completed.stdout, encoding="utf-8")
    (output_dir / "official-evaluator.stderr.txt").write_text(completed.stderr, encoding="utf-8")
    reports = find_reports(output_dir)
    predictions: dict[str, dict] = {}
    if not args.gold:
        for line in args.predictions.resolve().read_text(encoding="utf-8").splitlines():
            if line.strip():
                value = json.loads(line)
                predictions[value["instance_id"]] = value

    instances: list[dict] = []
    for instance_id in selected_ids:
        report_item = reports.get(instance_id)
        pred = predictions.get(instance_id)
        if report_item:
            path, report = report_item
            item = {
                "instance_id": instance_id,
                "resolved": bool(report.get("resolved", False)),
                "patch_successfully_applied": report.get("patch_successfully_applied"),
                "patch_exists": report.get("patch_exists"),
                "report_path": str(path.relative_to(output_dir)),
                "evaluation_status": "reported",
            }
        elif pred is not None and pred.get("model_patch") in ("", None):
            item = {
                "instance_id": instance_id,
                "resolved": False,
                "evaluation_status": "empty_patch_not_submitted_to_upstream_harness",
            }
        else:
            item = {
                "instance_id": instance_id,
                "resolved": None,
                "evaluation_status": "missing_report_or_evaluator_error",
            }
        instances.append(item)

    result = {
        "schema_version": "swebench-live-official-evaluation-v1",
        "dataset_revision": "a637bd46829f3132e12938c8a0ca93173a977b8e",
        "evaluator_repository": EVALUATOR_REPOSITORY,
        "evaluator_commit": revision,
        "evaluator_version": EVALUATOR_VERSION,
        "architecture_probe_override": "platform.machine -> x86_64 only; official grading unchanged",
        "namespace": args.namespace,
        "image_tag": args.image_tag,
        "evaluator_workers": args.evaluator_workers,
        "cache_level": "env; newly pulled instance images are removed after each test",
        "run_id": args.run_id,
        "gold_validation": args.gold,
        "selection_count": len(selected_ids),
        "process_return_code": completed.returncode,
        "started_at": started,
        "finished_at": __import__("datetime").datetime.now(__import__("datetime").timezone.utc).isoformat().replace("+00:00", "Z"),
        "resolved_count": sum(item["resolved"] is True for item in instances),
        "reported_count": sum(item["evaluation_status"] == "reported" for item in instances),
        "instances": instances,
        "command_without_secrets": [arg for arg in command if arg != str(full_json)],
        "full_dataset_input": "private/evaluator-inputs/lite-full.json (gold-bearing; never mounted into agent containers)",
    }
    write_json(output_dir / "results.json", result)
    print(json.dumps({key: result[key] for key in ("selection_count", "resolved_count", "reported_count", "process_return_code")}, indent=2))
    return 0 if completed.returncode == 0 else completed.returncode


if __name__ == "__main__":
    raise SystemExit(main())
