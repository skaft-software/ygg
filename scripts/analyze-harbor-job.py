#!/usr/bin/env python3
"""Audit token accounting and trajectory shape for one Harbor job directory.

The report keeps provider input, cache-hit input, output, and Harbor's included
cache subset separate. It never reads task prompts or emits tool arguments or
results. Only the Python standard library is required.
"""

from __future__ import annotations

import argparse
import collections
import datetime as dt
import glob
import json
import math
import os
import statistics
from pathlib import Path
from typing import Any

PERCENTILES = (0.5, 0.75, 0.9, 0.95)


def quantile(values: list[float], fraction: float) -> float | None:
    ordered = sorted(values)
    if not ordered:
        return None
    position = (len(ordered) - 1) * fraction
    lower = math.floor(position)
    upper = math.ceil(position)
    if lower == upper:
        return ordered[lower]
    return ordered[lower] * (upper - position) + ordered[upper] * (position - lower)


def distribution(rows: list[dict[str, Any]], key: str) -> dict[str, int | float | None]:
    values = [float(row[key]) for row in rows if isinstance(row.get(key), (int, float))]
    if not values:
        return {"count": 0}
    result: dict[str, int | float | None] = {
        "count": len(values),
        "sum": sum(values),
        "mean": statistics.mean(values),
        "max": max(values),
    }
    for fraction in PERCENTILES:
        label = "median" if fraction == 0.5 else f"p{int(fraction * 100)}"
        result[label] = quantile(values, fraction)
    return result


def parse_timestamp(value: Any) -> dt.datetime | None:
    if not isinstance(value, str):
        return None
    return dt.datetime.fromisoformat(value.replace("Z", "+00:00"))


def read_json(path: Path) -> dict[str, Any] | None:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError):
        return None
    return value if isinstance(value, dict) else None


def native_usage(trial: Path) -> tuple[collections.Counter[str], list[int], int]:
    totals: collections.Counter[str] = collections.Counter()
    contexts: list[int] = []
    records = 0
    for path_string in glob.glob(str(trial / "agent" / "sessions" / "*" / "*.jsonl")):
        try:
            lines = Path(path_string).read_text(encoding="utf-8").splitlines()
        except (OSError, UnicodeError):
            continue
        for line in lines:
            try:
                value = json.loads(line)
            except json.JSONDecodeError:
                continue
            if not isinstance(value, dict) or value.get("type") != "usage":
                continue
            record = value.get("record")
            usage = record.get("usage") if isinstance(record, dict) else None
            if not isinstance(usage, dict):
                continue
            records += 1
            for key, number in usage.items():
                if isinstance(number, int) and not isinstance(number, bool):
                    totals[key] += number
            contexts.append(
                int(usage.get("input_tokens", 0))
                + int(usage.get("cache_read_tokens", 0))
                + int(usage.get("cache_write_tokens", 0))
            )
    return totals, contexts, records


def trajectory_shape(trial: Path) -> dict[str, int | float | None]:
    value = read_json(trial / "agent" / "trajectory.json") or {}
    calls: list[tuple[str, str]] = []
    observation_bytes = 0
    truncated_observations = 0
    for step in value.get("steps", []):
        if not isinstance(step, dict):
            continue
        for call in step.get("tool_calls") or []:
            if not isinstance(call, dict):
                continue
            arguments = json.dumps(
                call.get("arguments") or {}, sort_keys=True, separators=(",", ":")
            )
            calls.append((str(call.get("function_name")), arguments))
        observation = step.get("observation")
        results = observation.get("results") if isinstance(observation, dict) else []
        for result in results or []:
            if not isinstance(result, dict):
                continue
            content = result.get("content")
            if isinstance(content, str):
                encoded = content.encode("utf-8")
                if "truncated_stdout=" in content or "truncated_stderr=" in content:
                    truncated_observations += 1
            elif content is None:
                encoded = b""
            else:
                encoded = json.dumps(content, separators=(",", ":")).encode("utf-8")
            observation_bytes += len(encoded)
    counts = collections.Counter(calls)
    repeated = sum(count - 1 for count in counts.values() if count > 1)
    consecutive = sum(current == previous for previous, current in zip(calls, calls[1:]))
    return {
        "tool_calls": len(calls),
        "repeated_tool_calls": repeated,
        "consecutive_repeated_tool_calls": consecutive,
        "tool_observation_bytes": observation_bytes,
        "truncated_tool_observations": truncated_observations,
    }


def manifest_drift(trial: Path) -> tuple[int, int]:
    manifest = read_json(trial / "agent" / "native-session-manifest.json")
    if manifest is None:
        return 0, 0
    files = manifest.get("files") or []
    declared = 0
    actual = 0
    agent_root = (trial / "agent").resolve()
    for item in files:
        if not isinstance(item, dict):
            continue
        declared += int(item.get("bytes", 0))
        relative = Path(str(item.get("path", "")))
        if relative.is_absolute() or ".." in relative.parts:
            continue
        try:
            candidate = (agent_root / relative).resolve(strict=True)
        except OSError:
            continue
        if not candidate.is_relative_to(agent_root) or not candidate.is_file():
            continue
        try:
            actual += candidate.stat().st_size
        except OSError:
            pass
    return declared, actual


def trial_row(trial: Path) -> dict[str, Any] | None:
    result = read_json(trial / "result.json")
    if result is None:
        return None
    agent_result = result.get("agent_result") or {}
    verifier = result.get("verifier_result") or {}
    rewards = verifier.get("rewards") if isinstance(verifier, dict) else None
    reward = rewards.get("reward") if isinstance(rewards, dict) else None
    classification = None
    try:
        classification = (trial / "agent" / "failure-classification.txt").read_text().strip()
    except OSError:
        pass

    usage, contexts, requests = native_usage(trial)
    fresh = usage["input_tokens"]
    cached = usage["cache_read_tokens"]
    cache_write = usage["cache_write_tokens"]
    provider_input = fresh + cached + cache_write
    output = usage["output_tokens"]
    start = parse_timestamp((result.get("agent_execution") or {}).get("started_at"))
    finish = parse_timestamp((result.get("agent_execution") or {}).get("finished_at"))
    declared_bytes, actual_bytes = manifest_drift(trial)
    row = {
        "trial": result.get("trial_name", trial.name),
        "task": result.get("task_name"),
        "reward": reward,
        "classification": classification,
        "provider_input_tokens": provider_input,
        "uncached_input_tokens": fresh,
        "cache_read_tokens": cached,
        "cache_write_tokens": cache_write,
        "output_tokens": output,
        "reasoning_tokens": usage["reasoning_tokens"],
        "processed_tokens": provider_input + output,
        "model_requests": requests,
        "average_context_tokens": statistics.mean(contexts) if contexts else None,
        "maximum_context_tokens": max(contexts) if contexts else None,
        "agent_duration_seconds": (finish - start).total_seconds() if start and finish else None,
        "harbor_input_tokens": agent_result.get("n_input_tokens"),
        "harbor_cache_tokens": agent_result.get("n_cache_tokens"),
        "harbor_output_tokens": agent_result.get("n_output_tokens"),
        "native_minus_harbor_input_tokens": (
            provider_input - int(agent_result["n_input_tokens"])
            if isinstance(agent_result.get("n_input_tokens"), int)
            else None
        ),
        "manifest_byte_drift": actual_bytes - declared_bytes,
    }
    row.update(trajectory_shape(trial))
    return row


def summarize(rows: list[dict[str, Any]]) -> dict[str, Any]:
    metric_keys = (
        "provider_input_tokens",
        "uncached_input_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
        "output_tokens",
        "reasoning_tokens",
        "processed_tokens",
        "model_requests",
        "average_context_tokens",
        "maximum_context_tokens",
        "agent_duration_seconds",
        "tool_calls",
        "repeated_tool_calls",
        "tool_observation_bytes",
    )
    return {key: distribution(rows, key) for key in metric_keys}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("job", type=Path, help="Harbor job directory containing trial/result.json files")
    parser.add_argument("--top", type=int, default=20, help="number of highest-input trials to retain")
    parser.add_argument("--output", type=Path, help="write JSON here instead of stdout")
    args = parser.parse_args()
    rows = [row for trial in sorted(args.job.iterdir()) if trial.is_dir() if (row := trial_row(trial))]
    if not rows:
        parser.error("no Harbor trial results found")

    classes = {
        "all": rows,
        "verifier_success": [row for row in rows if row["reward"] == 1],
        "verifier_failure": [row for row in rows if row["reward"] == 0],
        "benchmark_timeout": [row for row in rows if row["classification"] == "benchmark_timeout"],
        "provider_failure": [row for row in rows if row["classification"] == "provider_failure"],
    }
    by_task: dict[str, list[dict[str, Any]]] = collections.defaultdict(list)
    for row in rows:
        by_task[str(row["task"])].append(row)
    task_rows = [
        {
            "task": task,
            "provider_input_tokens": sum(row["provider_input_tokens"] for row in task_trials),
            "uncached_input_tokens": sum(row["uncached_input_tokens"] for row in task_trials),
            "processed_tokens": sum(row["processed_tokens"] for row in task_trials),
            "successes": sum(row["reward"] == 1 for row in task_trials),
        }
        for task, task_trials in by_task.items()
    ]
    total_input = sum(row["provider_input_tokens"] for row in rows)
    total_cache = sum(row["cache_read_tokens"] for row in rows)
    report = {
        "schema": "ygg.harbor-usage-audit.v1",
        "job": str(args.job),
        "equations": {
            "provider_input_tokens": "uncached_input_tokens + cache_read_tokens + cache_write_tokens",
            "processed_tokens": "provider_input_tokens + output_tokens",
            "reasoning_tokens": "subset of output_tokens",
            "harbor_cache_tokens": "included subset of harbor_input_tokens; never add it again",
        },
        "counts": {
            "trials": len(rows),
            "tasks": len(by_task),
            "rewards": dict(collections.Counter(str(row["reward"]) for row in rows)),
            "classifications": dict(collections.Counter(str(row["classification"]) for row in rows)),
        },
        "cache_read_ratio_of_provider_input": total_cache / total_input if total_input else None,
        "classes": {name: {"count": len(group), "distribution": summarize(group)} for name, group in classes.items()},
        "task_distribution": summarize(task_rows),
        "reconciliation": {
            "native_provider_input_tokens": total_input,
            "harbor_input_tokens": sum(int(row["harbor_input_tokens"] or 0) for row in rows),
            "native_minus_harbor_input_tokens": sum(int(row["native_minus_harbor_input_tokens"] or 0) for row in rows),
            "trials_with_manifest_drift": sum(row["manifest_byte_drift"] != 0 for row in rows),
            "manifest_byte_drift": sum(row["manifest_byte_drift"] for row in rows),
        },
        "highest_input_trials": sorted(rows, key=lambda row: row["provider_input_tokens"], reverse=True)[: args.top],
    }
    encoded = json.dumps(report, indent=2, sort_keys=True) + "\n"
    if args.output:
        args.output.write_text(encoded, encoding="utf-8")
    else:
        print(encoded, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
