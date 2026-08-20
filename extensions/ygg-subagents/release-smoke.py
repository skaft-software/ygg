#!/usr/bin/env python3
"""Compare one direct run with one bounded ygg-subagents run.

The recipe deliberately consumes measurements captured by the caller instead of
starting Ygg or a provider itself. That keeps release smoke deterministic and
prevents an install/package check from making network calls or running code at
installation time.
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys
from typing import Any, Dict, Mapping


MAX_FINDINGS = 512
MAX_FAILURES = 64


def load_run(path: Path) -> Dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        raise ValueError("cannot read smoke input %s: %s" % (path, error)) from error
    if not isinstance(value, dict):
        raise ValueError("smoke input must be an object")
    allowed = {
        "schema",
        "run_label",
        "findings",
        "input_tokens",
        "output_tokens",
        "wall_time_ms",
        "cpu_time_ms",
        "peak_rss_bytes",
        "failure_classes",
    }
    unknown = set(value) - allowed
    if unknown:
        raise ValueError("unknown smoke fields: %s" % ", ".join(sorted(unknown)))
    findings = value.get("findings")
    failures = value.get("failure_classes", [])
    if not isinstance(findings, list) or len(findings) > MAX_FINDINGS:
        raise ValueError("findings must be an array of at most 512 items")
    if not isinstance(failures, list) or len(failures) > MAX_FAILURES:
        raise ValueError("failure_classes must be an array of at most 64 items")
    normalized = []
    for item in findings:
        if (
            not isinstance(item, dict)
            or set(item) != {"id", "accepted"}
            or not isinstance(item.get("id"), str)
            or not item["id"]
            or len(item["id"].encode("utf-8")) > 256
            or not isinstance(item.get("accepted"), bool)
        ):
            raise ValueError("each finding requires bounded id and boolean accepted")
        normalized.append(dict(item))
    for field in (
        "input_tokens",
        "output_tokens",
        "wall_time_ms",
        "cpu_time_ms",
        "peak_rss_bytes",
    ):
        number = value.get(field)
        if not isinstance(number, int) or isinstance(number, bool) or number < 0:
            raise ValueError("%s must be a non-negative integer" % field)
    if any(
        not isinstance(item, str) or not item or len(item.encode("utf-8")) > 128
        for item in failures
    ):
        raise ValueError("failure classes must be bounded non-empty strings")
    value["findings"] = normalized
    value["failure_classes"] = sorted(set(failures))
    return value


def metrics(value: Mapping[str, Any]) -> Dict[str, Any]:
    identifiers = [item["id"] for item in value["findings"]]
    accepted = {item["id"] for item in value["findings"] if item["accepted"]}
    return {
        "quality_accepted_unique_findings": len(accepted),
        "reported_findings": len(identifiers),
        "duplicate_findings": len(identifiers) - len(set(identifiers)),
        "tokens": {
            "input": value["input_tokens"],
            "output": value["output_tokens"],
            "total": value["input_tokens"] + value["output_tokens"],
        },
        "wall_time_ms": value["wall_time_ms"],
        "cpu_time_ms": value["cpu_time_ms"],
        "peak_rss_bytes": value["peak_rss_bytes"],
        "failure_classes": value["failure_classes"],
    }


def compare(direct: Mapping[str, Any], delegated: Mapping[str, Any]) -> Dict[str, Any]:
    direct_metrics = metrics(direct)
    delegated_metrics = metrics(delegated)
    return {
        "schema": "ygg.subagents.release-smoke.v1",
        "direct": direct_metrics,
        "subagents": delegated_metrics,
        "quality_gain": (
            delegated_metrics["quality_accepted_unique_findings"]
            - direct_metrics["quality_accepted_unique_findings"]
        ),
        "token_delta": delegated_metrics["tokens"]["total"] - direct_metrics["tokens"]["total"],
        "wall_time_delta_ms": delegated_metrics["wall_time_ms"] - direct_metrics["wall_time_ms"],
        "cpu_time_delta_ms": delegated_metrics["cpu_time_ms"] - direct_metrics["cpu_time_ms"],
        "peak_rss_delta_bytes": delegated_metrics["peak_rss_bytes"] - direct_metrics["peak_rss_bytes"],
        "duplicate_finding_delta": delegated_metrics["duplicate_findings"] - direct_metrics["duplicate_findings"],
        "failure_classes": sorted(
            set(direct_metrics["failure_classes"] + delegated_metrics["failure_classes"])
        ),
    }


def main(argv: Any = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--direct", type=Path, required=True)
    parser.add_argument("--subagents", type=Path, required=True)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--require-gain", action="store_true")
    args = parser.parse_args(argv)
    try:
        report = compare(load_run(args.direct), load_run(args.subagents))
    except ValueError as error:
        print("release smoke input error: %s" % error, file=sys.stderr)
        return 2
    encoded = json.dumps(report, ensure_ascii=False, indent=2, sort_keys=True) + "\n"
    if args.output:
        args.output.write_text(encoded, encoding="utf-8")
    else:
        sys.stdout.write(encoded)
    if args.require_gain and report["quality_gain"] <= 0:
        print("release smoke did not show a positive accepted-finding gain", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
