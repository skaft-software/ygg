#!/usr/bin/env python3
"""Aggregate Ygg telemetry, trajectories, timings, memory, and evaluator results."""

from __future__ import annotations

import argparse
import json
import math
import re
import sys
from collections import Counter, defaultdict
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterable

SCRIPT_DIR = Path(__file__).resolve().parent
BENCHMARK_ROOT = SCRIPT_DIR.parent
sys.path.insert(0, str(SCRIPT_DIR))
from common import write_json  # noqa: E402


def read_json(path: Path) -> dict[str, Any] | None:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError):
        return None
    return value if isinstance(value, dict) else None


def read_jsonl(path: Path) -> list[dict[str, Any]]:
    result: list[dict[str, Any]] = []
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeError):
        return result
    for line in lines:
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict):
            result.append(value)
    return result


def numeric(value: Any) -> float | None:
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        return float(value)
    return None


def integer(value: Any) -> int | None:
    value = numeric(value)
    return int(value) if value is not None else None


def median(values: Iterable[float | int]) -> float | None:
    data = sorted(float(value) for value in values)
    if not data:
        return None
    middle = len(data) // 2
    if len(data) % 2:
        return data[middle]
    return (data[middle - 1] + data[middle]) / 2


def percentile(values: Iterable[float | int], percentile_value: float) -> float | None:
    data = sorted(float(value) for value in values)
    if not data:
        return None
    if len(data) == 1:
        return data[0]
    rank = (len(data) - 1) * percentile_value / 100.0
    lower = math.floor(rank)
    upper = math.ceil(rank)
    if lower == upper:
        return data[lower]
    return data[lower] + (data[upper] - data[lower]) * (rank - lower)


def normalize_key(name: str) -> str:
    return name.casefold().replace("_", "").replace("-", "")


def variant(value: Any, *names: str) -> Any:
    if not isinstance(value, dict):
        return None
    wanted = {normalize_key(name) for name in names}
    for key, candidate in value.items():
        if isinstance(key, str) and normalize_key(key) in wanted:
            return candidate
    return None


def active_entries(records: list[dict[str, Any]]) -> list[dict[str, Any]]:
    entries = {
        record.get("id"): record
        for record in records
        if record.get("type") == "entry" and isinstance(record.get("id"), str)
    }
    heads = [
        record.get("id")
        for record in records
        if record.get("type") == "head" and isinstance(record.get("id"), str)
    ]
    if not heads:
        return [record for record in records if record.get("type") == "entry"]
    active: set[str] = set()
    current = heads[-1]
    while isinstance(current, str) and current not in active:
        active.add(current)
        entry = entries.get(current)
        current = entry.get("parent") if isinstance(entry, dict) else None
    return [
        record
        for record in records
        if record.get("type") == "entry" and record.get("id") in active
    ]


def assistant_tool_data(session_files: list[Path]) -> dict[str, Any]:
    """Recover tool batches and shell-shape signals from native sessions.

    This is intentionally a conservative parser.  If a future session variant
    is not recognized it returns no claim rather than guessing from stdout.
    """

    assistant_messages = 0
    tool_turns = 0
    multiple_tool_turns = 0
    tool_calls: list[dict[str, Any]] = []
    for path in session_files:
        records = read_jsonl(path)
        for entry in active_entries(records):
            value = entry.get("value")
            if not isinstance(value, dict) or str(value.get("type", "")).casefold() != "message":
                continue
            message = variant(value, "message")
            if message is None:
                message = {key: item for key, item in value.items() if key != "type"}
            assistant = variant(message, "assistant")
            if not isinstance(assistant, dict):
                continue
            assistant_messages += 1
            parts = assistant.get("content")
            if not isinstance(parts, list):
                parts = []
            calls: list[dict[str, Any]] = []
            for part in parts:
                call = variant(part, "tool_call")
                if not isinstance(call, dict):
                    continue
                name = call.get("name")
                if not isinstance(name, str):
                    continue
                arguments = call.get("arguments")
                if not isinstance(arguments, dict):
                    raw = call.get("arguments_json")
                    if isinstance(raw, str):
                        try:
                            parsed = json.loads(raw)
                        except json.JSONDecodeError:
                            parsed = {}
                        arguments = parsed if isinstance(parsed, dict) else {}
                    else:
                        arguments = {}
                calls.append({"name": name, "arguments": arguments})
            if calls:
                tool_turns += 1
                if len(calls) > 1:
                    multiple_tool_turns += 1
                tool_calls.extend(calls)
    bash_commands: list[str] = []
    for call in tool_calls:
        if call["name"] != "bash":
            continue
        arguments = call["arguments"]
        command = arguments.get("command") if isinstance(arguments, dict) else None
        if isinstance(command, str):
            bash_commands.append(command)
    compound = sum(bool(re.search(r"&&|\|\||(?<!\|);", command)) for command in bash_commands)
    return {
        "session_files": [str(path) for path in session_files],
        "assistant_messages": assistant_messages,
        "tool_turns": tool_turns,
        "multiple_tool_turns": multiple_tool_turns,
        "tool_calls_from_native_session": len(tool_calls),
        "bash_commands_from_native_session": len(bash_commands),
        "compound_bash_commands": compound,
        "compound_bash_ratio": (compound / len(bash_commands)) if bash_commands else None,
        "native_parser_confident": bool(assistant_messages or session_files == []),
    }


def interval_union(intervals: list[tuple[float, float]]) -> float:
    if not intervals:
        return 0.0
    ordered = sorted((start, max(start, end)) for start, end in intervals)
    total = 0.0
    current_start, current_end = ordered[0]
    for start, end in ordered[1:]:
        if start <= current_end:
            current_end = max(current_end, end)
        else:
            total += current_end - current_start
            current_start, current_end = start, end
    return total + current_end - current_start


def concurrency(intervals: list[tuple[float, float]]) -> dict[str, Any]:
    if not intervals:
        return {
            "max_concurrent_tool_count": 0,
            "tool_calls_with_overlap": 0,
            "concurrent_tool_calls": 0,
            "tool_active_union_seconds": 0.0,
        }
    events: list[tuple[float, int, int]] = []
    for index, (start, end) in enumerate(intervals):
        events.append((start, 1, index))
        events.append((max(start, end), -1, index))
    # End before start at an equal timestamp means touching intervals are not
    # treated as concurrent.
    events.sort(key=lambda item: (item[0], item[1]))
    active: set[int] = set()
    overlap: set[int] = set()
    maximum = 0
    for _timestamp, delta, index in events:
        if delta > 0:
            if active:
                overlap.add(index)
                overlap.update(active)
            active.add(index)
            maximum = max(maximum, len(active))
        else:
            active.discard(index)
    return {
        "max_concurrent_tool_count": maximum,
        "tool_calls_with_overlap": len(overlap),
        "concurrent_tool_calls": sum(
            1
            for index, (start, end) in enumerate(intervals)
            if any(
                other_index != index
                and other_start < end
                and other_end > start
                for other_index, (other_start, other_end) in enumerate(intervals)
            )
        ),
        "tool_active_union_seconds": round(interval_union(intervals), 6),
    }


def telemetry_data(path: Path) -> dict[str, Any]:
    records = [
        record
        for record in read_jsonl(path)
        if record.get("schema") == "ygg.telemetry.v1"
    ]
    started_requests = [record for record in records if record.get("record") == "model_request_started"]
    finished_requests = [record for record in records if record.get("record") == "model_request_finished"]
    tool_started = [record for record in records if record.get("record") == "tool_started"]
    tool_finished = [record for record in records if record.get("record") == "tool_finished"]
    tool_starts: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for record in tool_started:
        call_id = record.get("tool_call_id_sha256")
        if isinstance(call_id, str):
            tool_starts[call_id].append(record)
    tool_intervals: list[tuple[float, float]] = []
    for record in tool_finished:
        call_id = record.get("tool_call_id_sha256")
        start = tool_starts.get(call_id, []).pop(0) if isinstance(call_id, str) and tool_starts.get(call_id) else None
        finish_ms = numeric(record.get("timestamp_unix_ms"))
        elapsed_ms = numeric(record.get("elapsed_ms")) or 0.0
        start_ms = numeric(start.get("timestamp_unix_ms")) if start else None
        if start_ms is None and finish_ms is not None:
            start_ms = finish_ms - elapsed_ms
        if start_ms is not None and finish_ms is not None:
            tool_intervals.append((start_ms / 1000.0, finish_ms / 1000.0))
    request_intervals: list[tuple[float, float]] = []
    for record in finished_requests:
        finish_ms = numeric(record.get("timestamp_unix_ms"))
        elapsed_ms = numeric(record.get("elapsed_ms")) or 0.0
        if finish_ms is not None:
            request_intervals.append(((finish_ms - elapsed_ms) / 1000.0, finish_ms / 1000.0))
    usage = {
        "provider_input_tokens": 0,
        "uncached_input_tokens": 0,
        "cache_read_tokens": 0,
        "cache_write_tokens": 0,
        "output_tokens": 0,
        "reasoning_tokens": 0,
        "total_tokens": 0,
        "cost_microdollars": 0,
        "saw_cost": False,
        "usage_records": 0,
    }
    for record in records:
        if record.get("record") not in {"model_request_finished", "compaction_finished"}:
            continue
        if record.get("usage_scope") not in {"request", "operation"}:
            continue
        usage["usage_records"] += 1
        for key in (
            "provider_input_tokens",
            "uncached_input_tokens",
            "cache_read_tokens",
            "cache_write_tokens",
            "output_tokens",
            "reasoning_tokens",
            "total_tokens",
        ):
            value = integer(record.get(key))
            if value is not None:
                usage[key] += value
        cost = integer(record.get("cost_microdollars"))
        if cost is not None:
            usage["cost_microdollars"] += cost
            usage["saw_cost"] = True
    tools = Counter(str(record.get("tool")) for record in tool_started if record.get("tool"))
    request_elapsed = [numeric(record.get("elapsed_ms")) for record in finished_requests]
    request_elapsed = [value for value in request_elapsed if value is not None]
    tool_elapsed = [numeric(record.get("elapsed_ms")) for record in tool_finished]
    tool_elapsed = [value for value in tool_elapsed if value is not None]
    logical_turns = {
        integer(record.get("logical_turn"))
        for record in started_requests + finished_requests
        if integer(record.get("logical_turn")) is not None
    }
    return {
        "telemetry_file": str(path),
        "record_count": len(records),
        "model_call_attempts": len(started_requests),
        "model_calls_completed": len(finished_requests),
        "model_turns_from_telemetry": len(logical_turns),
        "tool_calls": len(tool_started),
        "tool_executions": len(tool_finished),
        "tool_counts": dict(sorted(tools.items())),
        "bash_calls": tools.get("bash", 0),
        "read_calls": tools.get("read", 0),
        "search_calls": tools.get("search", 0),
        "edit_calls": tools.get("edit", 0),
        "write_calls": tools.get("write", 0),
        "multiple_tool_call_records": None,
        "request_time_seconds": round(sum(request_elapsed) / 1000.0, 6),
        "tool_time_seconds": round(sum(tool_elapsed) / 1000.0, 6),
        "active_request_union_seconds": round(interval_union(request_intervals), 6),
        "active_tool_union_seconds": round(interval_union(tool_intervals), 6),
        "concurrency": concurrency(tool_intervals),
        "usage": usage,
        "request_intervals": request_intervals,
        "tool_intervals": tool_intervals,
        "run_finished": next(
            (record for record in reversed(records) if record.get("record") == "run_finished"),
            None,
        ),
    }


def native_usage(session_files: list[Path]) -> dict[str, Any]:
    result = {
        "input_tokens": 0,
        "cache_read_tokens": 0,
        "cache_write_tokens": 0,
        "output_tokens": 0,
        "reasoning_tokens": 0,
        "total_tokens": 0,
        "cost_microdollars": 0,
        "saw_usage": False,
        "usage_records": 0,
    }
    for path in session_files:
        for record in read_jsonl(path):
            if record.get("type") != "usage" or not isinstance(record.get("record"), dict):
                continue
            operation = record["record"]
            values = operation.get("usage")
            if not isinstance(values, dict):
                continue
            parsed_any = False
            for key in result:
                if key in {"saw_usage", "usage_records", "cost_microdollars"}:
                    continue
                value = integer(values.get(key))
                if value is not None:
                    result[key] += value
                    parsed_any = True
            cost = operation.get("cost", operation.get("cost_microdollars"))
            if isinstance(cost, dict):
                cost = cost.get("total")
            cost_value = integer(cost)
            if cost_value is not None:
                result["cost_microdollars"] += cost_value
                parsed_any = True
            if parsed_any:
                result["saw_usage"] = True
                result["usage_records"] += 1
    return result


def parse_timestamp(value: Any) -> datetime | None:
    if not isinstance(value, str):
        return None
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None


def task_result_map(path: Path) -> dict[str, dict[str, Any]]:
    result = read_json(path)
    if not result:
        return {}
    return {
        item["instance_id"]: item
        for item in result.get("instances", [])
        if isinstance(item, dict) and isinstance(item.get("instance_id"), str)
    }


def task_record(task_dir: Path, eval_results: dict[str, dict[str, Any]]) -> dict[str, Any]:
    metadata = read_json(task_dir / "metadata.json") or {}
    telemetry_path = task_dir / "trajectory/ygg-telemetry.jsonl"
    telemetry = telemetry_data(telemetry_path) if telemetry_path.is_file() else {
        "telemetry_file": str(telemetry_path),
        "record_count": 0,
        "model_call_attempts": None,
        "model_calls_completed": None,
        "model_turns_from_telemetry": None,
        "tool_calls": None,
        "tool_executions": None,
        "tool_counts": {},
        "bash_calls": None,
        "read_calls": None,
        "search_calls": None,
        "edit_calls": None,
        "write_calls": None,
        "multiple_tool_call_records": None,
        "request_time_seconds": None,
        "tool_time_seconds": None,
        "active_request_union_seconds": None,
        "active_tool_union_seconds": None,
        "concurrency": {},
        "usage": {},
        "request_intervals": [],
        "tool_intervals": [],
        "run_finished": None,
    }
    session_files = sorted((task_dir / "trajectory/sessions").rglob("*.jsonl"))
    native = assistant_tool_data(session_files)
    native_tokens = native_usage(session_files)
    usage = telemetry.get("usage", {})
    if not usage.get("usage_records") and native_tokens.get("saw_usage"):
        usage = {
            "provider_input_tokens": native_tokens["input_tokens"] + native_tokens["cache_read_tokens"] + native_tokens["cache_write_tokens"],
            "uncached_input_tokens": native_tokens["input_tokens"],
            "cache_read_tokens": native_tokens["cache_read_tokens"],
            "cache_write_tokens": native_tokens["cache_write_tokens"],
            "output_tokens": native_tokens["output_tokens"],
            "reasoning_tokens": native_tokens["reasoning_tokens"],
            "total_tokens": native_tokens["total_tokens"],
            "cost_microdollars": native_tokens["cost_microdollars"],
            "saw_cost": native_tokens["cost_microdollars"] != 0,
            "usage_records": native_tokens["usage_records"],
            "source": "native-session-fallback",
        }
    eval_item = eval_results.get(metadata.get("instance_id", ""), {})
    wall = numeric(metadata.get("wall_seconds"))
    agent_wall = numeric(metadata.get("agent_seconds"))
    active = interval_union(
        [*telemetry.get("request_intervals", []), *telemetry.get("tool_intervals", [])]
    )
    if agent_wall is not None:
        unattributed = max(0.0, agent_wall - active)
    else:
        unattributed = None
    return {
        "instance_id": metadata.get("instance_id"),
        "repo": metadata.get("repo"),
        "base_commit": metadata.get("base_commit"),
        "termination_reason": metadata.get("termination_reason"),
        "process_kind": metadata.get("process_kind"),
        "resolved": eval_item.get("resolved"),
        "evaluation_status": eval_item.get("evaluation_status"),
        "wall_seconds": wall,
        "agent_seconds": agent_wall,
        "setup_seconds": numeric(metadata.get("setup_seconds")),
        "model_inference_or_provider_seconds": telemetry.get("request_time_seconds"),
        "tool_execution_seconds": telemetry.get("tool_time_seconds"),
        "active_observed_seconds": round(active, 6) if active else 0.0,
        "unattributed_agent_seconds": round(unattributed, 6) if unattributed is not None else None,
        "model_call_attempts": telemetry.get("model_call_attempts"),
        "model_calls_completed": telemetry.get("model_calls_completed"),
        "model_turns": native.get("assistant_messages") or telemetry.get("model_turns_from_telemetry"),
        "tool_calls": telemetry.get("tool_calls"),
        "tool_executions": telemetry.get("tool_executions"),
        "tool_counts": telemetry.get("tool_counts", {}),
        "bash_calls": telemetry.get("bash_calls"),
        "read_calls": telemetry.get("read_calls"),
        "search_calls": telemetry.get("search_calls"),
        "edit_calls": telemetry.get("edit_calls"),
        "write_calls": telemetry.get("write_calls"),
        "multiple_tool_turns": native.get("multiple_tool_turns"),
        "tool_turns": native.get("tool_turns"),
        "compound_bash_commands": native.get("compound_bash_commands"),
        "compound_bash_ratio": native.get("compound_bash_ratio"),
        "max_concurrent_tool_count": telemetry.get("concurrency", {}).get("max_concurrent_tool_count"),
        "concurrent_tool_calls": telemetry.get("concurrency", {}).get("concurrent_tool_calls"),
        "tool_calls_with_overlap": telemetry.get("concurrency", {}).get("tool_calls_with_overlap"),
        "tokens": usage,
        "memory": metadata.get("memory", {}),
        "patch_bytes": metadata.get("patch_capture", {}).get("patch_bytes"),
        "patch_lines": metadata.get("patch_capture", {}).get("patch_lines"),
        "has_patch": metadata.get("patch_capture", {}).get("has_patch", False),
        "telemetry_records": telemetry.get("record_count"),
        "trajectory_conversion": metadata.get("trajectory_conversion"),
        "evidence_dir": str(task_dir),
    }


def aggregate(records: list[dict[str, Any]], manifest: dict[str, Any], result_path: Path | None) -> dict[str, Any]:
    resolved = sum(record.get("resolved") is True for record in records)
    wall = [record["wall_seconds"] for record in records if numeric(record.get("wall_seconds")) is not None]
    agent_wall = [record["agent_seconds"] for record in records if numeric(record.get("agent_seconds")) is not None]
    calls = [record["model_call_attempts"] for record in records if numeric(record.get("model_call_attempts")) is not None]
    resolved_calls = [record["model_call_attempts"] for record in records if record.get("resolved") is True and numeric(record.get("model_call_attempts")) is not None]
    tools = [record["tool_calls"] for record in records if numeric(record.get("tool_calls")) is not None]
    turns = [record["model_turns"] for record in records if numeric(record.get("model_turns")) is not None]
    multiple_denominator = sum(record.get("model_turns", 0) or 0 for record in records if numeric(record.get("model_turns")) is not None)
    multiple_numerator = sum(record.get("multiple_tool_turns", 0) or 0 for record in records if numeric(record.get("multiple_tool_turns")) is not None)
    token_keys = ["provider_input_tokens", "uncached_input_tokens", "cache_read_tokens", "cache_write_tokens", "output_tokens", "reasoning_tokens", "total_tokens"]
    token_totals = {key: sum(integer(record.get("tokens", {}).get(key)) or 0 for record in records) for key in token_keys}
    token_totals["processed_tokens"] = token_totals["provider_input_tokens"] + token_totals["output_tokens"]
    cost_values = [integer(record.get("tokens", {}).get("cost_microdollars")) for record in records if record.get("tokens", {}).get("saw_cost")]
    cost_microdollars = sum(value or 0 for value in cost_values) if cost_values else None
    ygg_memory = [numeric(record.get("memory", {}).get("peak_ygg_rss_kib")) for record in records]
    ygg_memory = [value for value in ygg_memory if value is not None]
    tree_memory = [numeric(record.get("memory", {}).get("peak_process_tree_rss_kib")) for record in records]
    tree_memory = [value for value in tree_memory if value is not None]
    container_memory = [numeric(record.get("memory", {}).get("peak_container_memory_bytes")) for record in records]
    container_memory = [value for value in container_memory if value is not None]
    termination = Counter(str(record.get("termination_reason")) for record in records)
    result_status = Counter(str(record.get("evaluation_status")) for record in records)
    start = parse_timestamp(manifest.get("start_timestamp"))
    finish = parse_timestamp(manifest.get("finish_timestamp"))
    campaign_wall = (finish - start).total_seconds() if start and finish else sum(wall) if wall else None
    total_tokens = token_totals["processed_tokens"]
    cost_per_resolved = (cost_microdollars / 1_000_000 / resolved) if cost_microdollars is not None and resolved else None
    return {
        "schema_version": "swebench-live-aggregate-telemetry-v1",
        "run_id": manifest.get("run_id"),
        "task_count": len(records),
        "resolved_count": resolved,
        "resolution_rate": resolved / len(records) if records else None,
        "timing": {
            "total_campaign_wall_seconds": campaign_wall,
            "sum_task_wall_seconds": sum(wall) if wall else None,
            "median_task_wall_seconds": median(wall),
            "p90_task_wall_seconds": percentile(wall, 90),
            "p95_task_wall_seconds": percentile(wall, 95),
            "successful_tasks_per_wall_clock_hour": (resolved / campaign_wall * 3600) if campaign_wall and campaign_wall > 0 else None,
            "median_agent_process_seconds": median(agent_wall),
            "model_inference_or_provider_time_seconds": sum(record.get("model_inference_or_provider_seconds") or 0 for record in records),
            "tool_execution_time_seconds": sum(record.get("tool_execution_seconds") or 0 for record in records),
            "unattributed_agent_time_seconds": sum(record.get("unattributed_agent_seconds") or 0 for record in records),
            "measurement_note": "model time is Ygg's provider-request elapsed time, not provider GPU inference time",
        },
        "agent_loop": {
            "median_model_calls_per_task": median(calls),
            "median_model_calls_per_resolved_task": median(resolved_calls),
            "median_model_turns_per_task": median(turns),
            "median_tool_calls_per_task": median(tools),
            "multiple_tool_turn_percentage": (multiple_numerator / multiple_denominator) if multiple_denominator else None,
            "turns_with_multiple_tool_calls": multiple_numerator,
            "turns_observed": multiple_denominator,
            "median_max_concurrent_tool_count": median(record["max_concurrent_tool_count"] for record in records if numeric(record.get("max_concurrent_tool_count")) is not None),
            "maximum_concurrent_tool_count": max((record.get("max_concurrent_tool_count", 0) or 0 for record in records), default=0),
            "total_concurrent_tool_calls": sum(record.get("concurrent_tool_calls", 0) or 0 for record in records),
            "total_tool_calls_with_overlap": sum(record.get("tool_calls_with_overlap", 0) or 0 for record in records),
            "compound_bash_commands": sum(record.get("compound_bash_commands", 0) or 0 for record in records),
            "bash_commands": sum(record.get("bash_calls", 0) or 0 for record in records),
            "compound_bash_ratio": (sum(record.get("compound_bash_commands", 0) or 0 for record in records) / sum(record.get("bash_calls", 0) or 0 for record in records)) if sum(record.get("bash_calls", 0) or 0 for record in records) else None,
            "interpretation": "batching/fanout indicators are trace heuristics, not causal labels",
        },
        "tokens": {
            **token_totals,
            "total_tokens_per_task": total_tokens / len(records) if records else None,
            "total_tokens_per_resolved_issue": total_tokens / resolved if resolved else None,
            "input_token_definition": "provider_input_tokens = uncached + cache_read + cache_write; disjoint buckets",
        },
        "cost": {
            "estimated_cost_usd": cost_microdollars / 1_000_000 if cost_microdollars is not None else None,
            "estimated_cost_per_resolved_issue_usd": cost_per_resolved,
            "cost_microdollars": cost_microdollars,
            "availability": "native/telemetry cost fields only; Codex subscription spend may not be represented",
        },
        "memory": {
            "median_ygg_rss_kib": median(ygg_memory),
            "peak_ygg_rss_kib": max(ygg_memory) if ygg_memory else None,
            "median_process_tree_rss_kib": median(tree_memory),
            "peak_process_tree_rss_kib": max(tree_memory) if tree_memory else None,
            "median_container_memory_bytes": median(container_memory),
            "peak_container_memory_bytes": max(container_memory) if container_memory else None,
            "scope": "task container; remote model/provider server excluded",
        },
        "reliability": {
            "termination_reasons": dict(sorted(termination.items())),
            "evaluation_statuses": dict(sorted(result_status.items())),
            "timeout_rate": sum(record.get("termination_reason") == "benchmark_timeout" for record in records) / len(records) if records else None,
            "provider_error_rate": sum(record.get("termination_reason") == "provider_failure" for record in records) / len(records) if records else None,
            "empty_or_no_patch_rate": sum(not record.get("has_patch", False) for record in records) / len(records) if records else None,
        },
        "source": {
            "agent_manifest": manifest,
            "evaluation_results": str(result_path) if result_path else None,
            "per_instance_records": len(records),
        },
        "per_instance": records,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run-dir", type=Path, required=True)
    parser.add_argument("--evaluation-results", type=Path)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    run_dir = args.run_dir.resolve()
    manifest = read_json(run_dir / "manifest.json") or {}
    eval_results = task_result_map(args.evaluation_results.resolve()) if args.evaluation_results else {}
    records = []
    for metadata_path in sorted((run_dir / "instances").glob("*/metadata.json")):
        record = task_record(metadata_path.parent, eval_results)
        if record.get("instance_id"):
            records.append(record)
    output = aggregate(records, manifest, args.evaluation_results.resolve() if args.evaluation_results else None)
    output_path = (args.output or (run_dir / "aggregate-telemetry.json")).resolve()
    write_json(output_path, output)
    print(json.dumps({"tasks": len(records), "resolved": output["resolved_count"], "output": str(output_path)}, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
