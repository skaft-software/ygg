"""Conservative conversion of native Ygg JSONL sessions to ATIF."""

from __future__ import annotations

import json
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


@dataclass
class SessionMetrics:
    """Aggregate usage recovered from Ygg ``usage`` records."""

    input_tokens: int = 0
    output_tokens: int = 0
    cache_tokens: int = 0
    cost_usd: float = 0.0
    turns: int = 0
    saw_usage: bool = False


@dataclass
class SessionConversion:
    """One converted session and the metrics used to populate Harbor context."""

    trajectory: dict[str, Any]
    metrics: SessionMetrics
    source: Path


def _validate_trajectory(trajectory: dict[str, Any]) -> None:
    """Validate ATIF with Harbor when the optional runtime is installed."""

    try:
        from harbor.models.trajectories import Trajectory
    except ImportError:
        return
    Trajectory.model_validate(trajectory)


def _variant(value: Any, *names: str) -> Any:
    if not isinstance(value, dict):
        return None

    def normalize(name: str) -> str:
        return name.casefold().replace("_", "").replace("-", "")

    wanted = {normalize(name) for name in names}
    for key, candidate in value.items():
        if isinstance(key, str) and normalize(key) in wanted:
            return candidate
    return None


def _entry_message(value: Any) -> tuple[str, Any] | None:
    if not isinstance(value, dict) or value.get("type", "").casefold() != "message":
        return None
    message = value.get("content")
    if message is None:
        message = value.get("message")
    if message is None:
        # This accommodates serde representations that use the tuple variant's
        # name as the field key.
        message = value.get("value")
    if message is None:
        message = {
            key: candidate
            for key, candidate in value.items()
            if key != "type"
        }
    if not isinstance(message, dict):
        return None
    user = _variant(message, "user")
    if user is not None:
        return "user", user
    assistant = _variant(message, "assistant")
    if assistant is not None:
        return "assistant", assistant
    return None


def _parts(message: Any) -> list[Any]:
    if not isinstance(message, dict):
        return []
    parts = message.get("content")
    return parts if isinstance(parts, list) else []


def _text_from_part(part: Any, *names: str) -> str | None:
    value = _variant(part, *names)
    if isinstance(value, str):
        return value
    if isinstance(value, dict):
        for key in ("text", "value", "content"):
            candidate = value.get(key)
            if isinstance(candidate, str):
                return candidate
    return None


def _tool_result(part: Any) -> dict[str, Any] | None:
    value = _variant(part, "tool_result")
    if not isinstance(value, dict):
        return None
    call_id = value.get("tool_call_id")
    if isinstance(call_id, dict):
        call_id = call_id.get("0") or call_id.get("id")
    if not isinstance(call_id, str):
        return None
    content_parts = value.get("content")
    text_parts: list[str] = []
    if isinstance(content_parts, list):
        for content in content_parts:
            text = _text_from_part(content, "text")
            if text is not None:
                text_parts.append(text)
            elif _variant(content, "media") is not None:
                text_parts.append("[media]")
    return {
        "source_call_id": call_id,
        "content": "\n".join(text_parts) if text_parts else None,
        "extra": {"is_error": bool(value.get("is_error", False))},
    }


def _tool_call(part: Any) -> dict[str, Any] | None:
    value = _variant(part, "tool_call")
    if not isinstance(value, dict):
        return None
    call_id = value.get("id")
    if isinstance(call_id, dict):
        call_id = call_id.get("0") or call_id.get("id")
    name = value.get("name")
    if not isinstance(call_id, str) or not isinstance(name, str):
        return None
    raw_arguments = value.get("arguments_json")
    if not isinstance(raw_arguments, str):
        raw_arguments = value.get("arguments")
    if isinstance(raw_arguments, dict):
        arguments = raw_arguments
    elif isinstance(raw_arguments, str):
        try:
            parsed = json.loads(raw_arguments)
        except json.JSONDecodeError:
            arguments = {"raw": raw_arguments}
        else:
            arguments = parsed if isinstance(parsed, dict) else {"raw": raw_arguments}
    else:
        arguments = {}
    return {
        "tool_call_id": call_id,
        "function_name": name,
        "arguments": arguments,
    }


def _timestamp(value: Any) -> str | None:
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        return datetime.fromtimestamp(value / 1000, tz=timezone.utc).isoformat()
    return value if isinstance(value, str) else None


def _usage_for_record(record: dict[str, Any]) -> tuple[str | None, dict[str, Any]] | None:
    kind = record.get("kind")
    if isinstance(kind, dict):
        assistant = kind.get("assistant")
        kind_name = kind.get("kind")
        if isinstance(assistant, dict):
            assistant = assistant.get("0") or assistant.get("id")
        if kind_name == "assistant_turn" and isinstance(assistant, str):
            usage = record.get("usage")
            return assistant, usage if isinstance(usage, dict) else {}
    return None


def _microdollars_to_usd(cost: Any) -> float:
    if isinstance(cost, dict):
        cost = cost.get("total")
    if isinstance(cost, (int, float)) and not isinstance(cost, bool):
        return float(cost) / 1_000_000
    return 0.0


def _metrics_for_usage(record: dict[str, Any], metrics: SessionMetrics) -> dict[str, Any] | None:
    usage = record.get("usage")
    if not isinstance(usage, dict):
        return None
    numeric = {
        key: int(value)
        for key, value in usage.items()
        if key in {
            "input_tokens",
            "cache_read_tokens",
            "cache_write_tokens",
            "cache_write_1h_tokens",
            "output_tokens",
            "reasoning_tokens",
            "total_tokens",
        }
        and isinstance(value, int)
        and not isinstance(value, bool)
    }
    cost_usd = _microdollars_to_usd(
        record.get("cost", record.get("cost_microdollars"))
    )
    if not numeric and cost_usd == 0:
        return None
    input_tokens = numeric.get("input_tokens", 0)
    cache_tokens = numeric.get("cache_read_tokens", 0)
    output_tokens = numeric.get("output_tokens", 0)
    metrics.input_tokens += input_tokens + cache_tokens
    metrics.cache_tokens += cache_tokens
    metrics.output_tokens += output_tokens
    metrics.cost_usd += cost_usd
    metrics.turns += 1
    metrics.saw_usage = True
    extra = {
        key: value
        for key, value in numeric.items()
        if key not in {"input_tokens", "output_tokens", "cache_read_tokens"}
    }
    if record.get("endpoint") is not None:
        extra["endpoint"] = record["endpoint"]
    return {
        "prompt_tokens": input_tokens + cache_tokens,
        "completion_tokens": output_tokens,
        "cached_tokens": cache_tokens,
        "cost_usd": cost_usd,
        "extra": extra or None,
    }


def _active_entries(records: list[dict[str, Any]]) -> list[dict[str, Any]]:
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

    active_ids: set[str] = set()
    current = heads[-1]
    while isinstance(current, str) and current not in active_ids:
        active_ids.add(current)
        entry = entries.get(current)
        current = entry.get("parent") if isinstance(entry, dict) else None
    return [
        record
        for record in records
        if record.get("type") == "entry" and record.get("id") in active_ids
    ]


def _read_records(path: Path) -> list[dict[str, Any]]:
    records: list[dict[str, Any]] = []
    for line in path.read_text(encoding="utf-8").splitlines():
        try:
            record = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(record, dict):
            records.append(record)
    return records


def convert_session_file(
    path: Path,
    *,
    agent_name: str,
    agent_version: str,
    model_name: str | None,
    reasoning: str | None,
) -> SessionConversion:
    """Convert the active branch of one native Ygg session."""

    records = _read_records(path)
    if not records:
        raise ValueError("session contains no valid JSONL records")

    usage_by_assistant: dict[str, dict[str, Any]] = {}
    for record in records:
        if record.get("type") != "usage":
            continue
        usage_record = record.get("record")
        if isinstance(usage_record, dict):
            result = _usage_for_record(usage_record)
            if result is not None:
                usage_by_assistant[result[0] or ""] = usage_record

    metrics = SessionMetrics()
    steps: list[dict[str, Any]] = []
    pending_results: list[dict[str, Any]] = []
    tool_call_steps: dict[str, int] = {}
    default_model = model_name

    for record in _active_entries(records):
        parsed = _entry_message(record.get("value"))
        if parsed is None:
            continue
        role, message = parsed
        if role == "user":
            text_parts: list[str] = []
            results: list[dict[str, Any]] = []
            for part in _parts(message):
                text = _text_from_part(part, "text")
                if text:
                    text_parts.append(text)
                result = _tool_result(part)
                if result is not None:
                    results.append(result)
            if results:
                pending_results.extend(results)
                if not text_parts:
                    continue
            step: dict[str, Any] = {
                "step_id": len(steps) + 1,
                "source": "user",
                "message": "\n".join(text_parts),
            }
            if record.get("timestamp") is not None:
                step["timestamp"] = _timestamp(record["timestamp"])
            steps.append(step)
            continue

        text_parts = []
        reasoning_parts: list[str] = []
        tool_calls: list[dict[str, Any]] = []
        for part in _parts(message):
            text = _text_from_part(part, "text")
            if text:
                text_parts.append(text)
            reasoning_text = _text_from_part(part, "reasoning")
            if reasoning_text:
                reasoning_parts.append(reasoning_text)
            call = _tool_call(part)
            if call is not None:
                tool_calls.append(call)
            elif _variant(part, "media") is not None:
                text_parts.append("[media]")
        assistant_model = message.get("model") if isinstance(message, dict) else None
        if isinstance(assistant_model, str):
            default_model = default_model or assistant_model
        step = {
            "step_id": len(steps) + 1,
            "source": "agent",
            "message": "\n".join(text_parts),
        }
        if record.get("timestamp") is not None:
            step["timestamp"] = _timestamp(record["timestamp"])
        if assistant_model or default_model:
            step["model_name"] = assistant_model or default_model
        if reasoning:
            step["reasoning_effort"] = reasoning
        if reasoning_parts:
            step["reasoning_content"] = "\n".join(reasoning_parts)
        if tool_calls:
            step["tool_calls"] = tool_calls
            step_index = len(steps)
            tool_call_steps.update(
                {call["tool_call_id"]: step_index for call in tool_calls}
            )
        usage_record = usage_by_assistant.get(str(record.get("id")))
        if usage_record is not None:
            atif_metrics = _metrics_for_usage(usage_record, metrics)
            if atif_metrics is not None:
                step["metrics"] = atif_metrics
        steps.append(step)

    for result in pending_results:
        step_index = tool_call_steps.get(result.get("source_call_id"))
        if step_index is not None:
            steps[step_index].setdefault("observation", {"results": []})[
                "results"
            ].append(result)
            continue
        if steps:
            extra = {"orphaned": True}
            source_call_id = result.get("source_call_id")
            if source_call_id is not None:
                extra["unmatched_source_call_id"] = source_call_id
            steps[-1].setdefault("observation", {"results": []})["results"].append(
                {"content": result.get("content"), "extra": extra}
            )
    if not steps:
        raise ValueError("session contains no convertible messages")

    trajectory: dict[str, Any] = {
        "schema_version": "ATIF-v1.6",
        "session_id": path.stem,
        "agent": {
            "name": agent_name,
            "version": agent_version,
            "model_name": default_model,
        },
        "steps": steps,
        "final_metrics": {
            "total_prompt_tokens": metrics.input_tokens if metrics.saw_usage else None,
            "total_completion_tokens": metrics.output_tokens if metrics.saw_usage else None,
            "total_cached_tokens": metrics.cache_tokens if metrics.saw_usage else None,
            "total_cost_usd": metrics.cost_usd if metrics.saw_usage else None,
            "total_steps": len(steps),
        },
        "notes": (
            "Conservative conversion of durable message, tool-call, tool-result, "
            "and usage records from the native Ygg JSONL session. Native timestamps, "
            "system/config/compaction records, opaque provider sidecars, media bytes, "
            "and unrecognized fields remain available only in the native artifact."
        ),
        "extra": {"source": str(path), "conversion": "ygg-jsonl-active-branch"},
    }
    _validate_trajectory(trajectory)
    return SessionConversion(trajectory=trajectory, metrics=metrics, source=path)


def convert_native_sessions(
    root: Path,
    *,
    agent_name: str,
    agent_version: str,
    model_name: str | None,
    reasoning: str | None,
) -> SessionConversion | None:
    """Choose the newest convertible native session below ``root``."""

    candidates = sorted(
        (path for path in root.rglob("*.jsonl") if path.is_file()),
        key=lambda path: path.stat().st_mtime_ns,
        reverse=True,
    ) if root.is_dir() else []
    for path in candidates:
        try:
            return convert_session_file(
                path,
                agent_name=agent_name,
                agent_version=agent_version,
                model_name=model_name,
                reasoning=reasoning,
            )
        except (OSError, ValueError):
            continue
    return None
