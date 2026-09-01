#!/usr/bin/env python3
"""Write the benchmark's frozen identity manifest without recording secrets."""

from __future__ import annotations

import argparse
import json
import os
import platform
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

SCRIPT_DIR = Path(__file__).resolve().parent
BENCHMARK_ROOT = SCRIPT_DIR.parent
sys.path.insert(0, str(SCRIPT_DIR))
from common import (  # noqa: E402
    DATASET_REVISION,
    DATASET_SHA256,
    EVALUATOR_COMMIT,
    EVALUATOR_REPOSITORY,
    IMAGE_ARCH,
    IMAGE_NAMESPACE,
    IMAGE_TAG,
    YGG_COMMIT,
    YGG_RELEASE_REF,
    YGG_REPOSITORY,
    YGG_VERSION,
    ensure_dataset,
    sha256_file,
    system_prompt_identity,
    tool_schema_identity,
    write_json,
)


def now() -> str:
    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")


def command_output(command: list[str]) -> str | None:
    try:
        return subprocess.check_output(command, text=True, stderr=subprocess.STDOUT).strip()
    except (OSError, subprocess.CalledProcessError):
        return None


def git_identity(path: Path) -> dict[str, Any]:
    return {
        "path": str(path),
        "commit": command_output(["git", "-C", str(path), "rev-parse", "HEAD"]),
        "status": command_output(["git", "-C", str(path), "status", "--porcelain"]),
        "dirty": bool(command_output(["git", "-C", str(path), "status", "--porcelain"])),
    }


def image_digests(phase_dir: Path) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for path in sorted((phase_dir / "instances").glob("*/metadata.json")):
        try:
            value = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, UnicodeError, json.JSONDecodeError):
            continue
        instance_id = value.get("instance_id")
        image = value.get("image")
        if isinstance(instance_id, str) and isinstance(image, dict):
            result[instance_id] = {
                "reference": image.get("reference"),
                "resolved_digest": image.get("resolved_digest"),
                "image_id": image.get("image_id"),
                "architecture": image.get("architecture"),
                "os": image.get("os"),
            }
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=BENCHMARK_ROOT / "manifest.json")
    parser.add_argument("--parquet", type=Path, default=BENCHMARK_ROOT / "data/lite.parquet")
    parser.add_argument("--selection", type=Path)
    parser.add_argument("--phase-dir", type=Path)
    parser.add_argument("--ygg-source", type=Path)
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--phase", default="in_progress")
    parser.add_argument("--status", default="in_progress")
    parser.add_argument("--model", default="gpt-5.6-sol")
    parser.add_argument("--provider", default="codex-oauth")
    parser.add_argument("--reasoning", default="max")
    parser.add_argument("--timeout-seconds", type=int, default=1800)
    parser.add_argument("--k", type=int, default=1)
    parser.add_argument("--invalid-list", type=Path)
    parser.add_argument("--start-time")
    parser.add_argument("--finish-time")
    args = parser.parse_args()

    parquet = ensure_dataset(args.parquet.resolve())
    source = args.ygg_source.resolve() if args.ygg_source else None
    binary = args.binary.resolve() if args.binary else None
    selection = args.selection.resolve() if args.selection else None
    invalid = args.invalid_list.resolve() if args.invalid_list else None
    env_names = {}
    for name in sorted(os.environ):
        if any(token in name.casefold() for token in ("key", "token", "secret", "password", "credential", "auth")):
            env_names[name] = "REDACTED" if os.environ.get(name) else "UNSET"
    manifest: dict[str, Any] = {
        "schema_version": "swebench-live-ygg-frozen-manifest-v1",
        "status": args.status,
        "phase": args.phase,
        "created_at": now(),
        "start_time": args.start_time,
        "finish_time": args.finish_time,
        "ygg": {
            "repository": YGG_REPOSITORY,
            "commit": YGG_COMMIT,
            "release_ref": YGG_RELEASE_REF,
            "version": YGG_VERSION,
            "source_checkout": git_identity(source) if source else None,
            "benchmark_checkout": git_identity(BENCHMARK_ROOT.parents[2]),
            "binary_path": str(binary) if binary else None,
            "binary_sha256": sha256_file(binary) if binary and binary.is_file() else None,
            "binary_version": command_output([str(binary), "--version"]) if binary and binary.is_file() else None,
            "target": "x86_64-unknown-linux-musl",
        },
        "toolchain": {
            "rustc": command_output(["rustc", "--version"]),
            "cargo": command_output(["cargo", "--version"]),
            "python": sys.version,
        },
        "host": {
            "platform": platform.platform(),
            "system": platform.system(),
            "release": platform.release(),
            "machine": platform.machine(),
            "processor": platform.processor(),
            "cpu_count": os.cpu_count(),
            "uname": command_output(["uname", "-a"]),
            "docker_info": command_output(["docker", "info", "--format", "os={{.OSType}} arch={{.Architecture}} cpus={{.NCPU}} mem={{.MemTotal}}"]),
        },
        "model": {
            "model": args.model,
            "provider": args.provider,
            "reasoning": args.reasoning,
            "k": args.k,
        },
        "dataset": {
            "repository": "SWE-bench-Live/SWE-bench-Live",
            "revision": DATASET_REVISION,
            "split": "lite",
            "nominal_count": 300,
            "parquet_path": str(parquet),
            "parquet_sha256": sha256_file(parquet),
            "expected_parquet_sha256": DATASET_SHA256,
        },
        "evaluator": {
            "repository": EVALUATOR_REPOSITORY,
            "commit": EVALUATOR_COMMIT,
            "version": "4.0.3",
            "python_module": "swebench.harness.run_evaluation",
            "architecture_probe_override": "platform.machine -> x86_64; grading code unchanged",
        },
        "configuration": {
            "task_timeout_seconds": args.timeout_seconds,
            "evaluator_test_timeout_seconds": 1800,
            "timeout_grace_seconds": 15,
            "image_namespace": IMAGE_NAMESPACE,
            "image_arch": IMAGE_ARCH,
            "image_tag": IMAGE_TAG,
            "docker_platform": "linux/amd64",
            "task_concurrency": 1,
            "selective_retries": False,
            "extensions": [],
            "context_files": True,
            "workspace_trusted": True,
            "agent_prompt": "exact problem_statement only",
        },
        "system_prompt": system_prompt_identity(source),
        "tool_schema": tool_schema_identity(),
        "environment_variables": env_names,
        "selection": {
            "path": str(selection) if selection else None,
            "sha256": sha256_file(selection) if selection and selection.is_file() else None,
        },
        "invalid_instance_list": {
            "path": str(invalid) if invalid else None,
            "sha256": sha256_file(invalid) if invalid and invalid.is_file() else None,
        },
        "image_digests": image_digests(args.phase_dir.resolve()) if args.phase_dir else {},
        "secrets": "all credential values omitted; Codex credential is copied per task and never returned",
    }
    write_json(args.output.resolve(), manifest)
    print(args.output.resolve())
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
