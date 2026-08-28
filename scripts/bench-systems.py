#!/usr/bin/env python3
"""Reproducible, dependency-free systems measurements for local coding agents.

The default case measures cold process launch.  Additional commands can be
provided as ``NAME=ARGV`` values (parsed with :mod:`shlex`, never a shell), and
an arbitrary long-lived command can be measured for RSS/PSS and CPU with
``--idle-command``.  The same runner can therefore be used for another agent
without pretending that a Ygg-specific command is a competitor comparison.

Examples:

    python3 scripts/bench-systems.py \
      --binary ./target/release/ygg --repetitions 9 \
      --output /tmp/ygg-systems.json

    python3 scripts/bench-systems.py \
      --command sessions='./target/release/ygg --offline sessions list' \
      --idle-command idle='./target/release/ygg --plain --offline ...' \
      --concurrency 1,2,4 --telemetry /tmp/ygg-telemetry.jsonl
"""

from __future__ import annotations

import argparse
import glob
import json
import math
import os
import platform
import shlex
import signal
import statistics
import subprocess
import sys
import time
from pathlib import Path
from typing import Any

SCHEMA = "ygg.systems-benchmark.v1"
DEFAULT_REPETITIONS = 9
DEFAULT_TIMEOUT_SECONDS = 30.0
DEFAULT_IDLE_SECONDS = 1.0
DEFAULT_SETTLE_SECONDS = 0.25


def parse_named_command(raw: str) -> tuple[str, list[str]]:
    name, separator, command = raw.partition("=")
    if not separator or not name.strip():
        raise argparse.ArgumentTypeError("expected NAME=COMMAND")
    argv = shlex.split(command)
    if not argv:
        raise argparse.ArgumentTypeError(f"command for {name!r} is empty")
    return name.strip(), argv


def finite_number(value: float) -> float | None:
    return value if math.isfinite(value) else None


def quantile(values: list[float], fraction: float) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    if len(ordered) == 1:
        return ordered[0]
    position = fraction * (len(ordered) - 1)
    lower = math.floor(position)
    upper = math.ceil(position)
    if lower == upper:
        return ordered[lower]
    weight = position - lower
    return ordered[lower] * (1.0 - weight) + ordered[upper] * weight


def summarize(values: list[float]) -> dict[str, float | int | None]:
    return {
        "count": len(values),
        "min": min(values) if values else None,
        "median": statistics.median(values) if values else None,
        "p95": quantile(values, 0.95),
        "max": max(values) if values else None,
    }


def read_cpu_percent(pid: int) -> float | None:
    try:
        result = subprocess.run(
            ["ps", "-o", "pcpu=", "-p", str(pid)],
            check=True,
            capture_output=True,
            text=True,
            timeout=2,
        )
        return finite_number(float(result.stdout.strip()))
    except (FileNotFoundError, OSError, subprocess.SubprocessError, ValueError):
        return None


def read_memory(pid: int) -> dict[str, float | int | None]:
    """Return best-effort process memory and CPU data for one PID."""

    if sys.platform.startswith("linux"):
        rss: int | None = None
        try:
            for line in Path(f"/proc/{pid}/status").read_text().splitlines():
                if line.startswith("VmRSS:"):
                    rss = int(line.split()[1]) * 1024
                    break
        except (FileNotFoundError, OSError, ValueError):
            pass
        pss: int | None = None
        try:
            for line in Path(f"/proc/{pid}/smaps_rollup").read_text().splitlines():
                if line.startswith("Pss:"):
                    pss = int(line.split()[1]) * 1024
                    break
        except (FileNotFoundError, OSError, ValueError):
            pass
        cpu = read_cpu_percent(pid)
        return {"rss_bytes": rss, "pss_bytes": pss, "cpu_percent": cpu}

    # macOS and BSD expose RSS/CPU portably through ps. PSS is intentionally
    # reported as null rather than confused with RSS.
    try:
        result = subprocess.run(
            ["ps", "-o", "rss=,pcpu=", "-p", str(pid)],
            check=True,
            capture_output=True,
            text=True,
            timeout=2,
        )
        fields = result.stdout.split()
        if len(fields) >= 2:
            return {
                "rss_bytes": int(float(fields[0]) * 1024),
                "pss_bytes": None,
                "cpu_percent": read_cpu_percent(pid),
            }
    except (FileNotFoundError, OSError, subprocess.SubprocessError, ValueError):
        pass
    return {"rss_bytes": None, "pss_bytes": None, "cpu_percent": None}


def terminate_process(process: subprocess.Popen[bytes]) -> None:
    if process.poll() is not None:
        return
    try:
        if os.name == "posix":
            os.killpg(process.pid, signal.SIGTERM)
        else:
            process.terminate()
        process.wait(timeout=5)
    except (ProcessLookupError, subprocess.TimeoutExpired):
        try:
            if os.name == "posix":
                os.killpg(process.pid, signal.SIGKILL)
            else:
                process.kill()
        except ProcessLookupError:
            pass
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            pass


def launch_kwargs() -> dict[str, Any]:
    kwargs: dict[str, Any] = {
        "stdin": subprocess.DEVNULL,
        "stdout": subprocess.DEVNULL,
        "stderr": subprocess.DEVNULL,
    }
    if os.name == "posix":
        kwargs["start_new_session"] = True
    return kwargs


def idle_launch_kwargs() -> dict[str, Any]:
    kwargs = launch_kwargs()
    kwargs["stdin"] = subprocess.PIPE
    return kwargs


def benchmark_startup(
    name: str,
    argv: list[str],
    repetitions: int,
    timeout_seconds: float,
) -> dict[str, Any]:
    durations: list[float] = []
    return_codes: list[int | None] = []
    errors: list[str] = []
    for _ in range(repetitions):
        started = time.perf_counter_ns()
        try:
            result = subprocess.run(
                argv,
                **launch_kwargs(),
                timeout=timeout_seconds,
                check=False,
            )
            return_codes.append(result.returncode)
            if result.returncode == 0:
                durations.append((time.perf_counter_ns() - started) / 1_000_000)
        except (OSError, subprocess.TimeoutExpired) as error:
            return_codes.append(None)
            errors.append(type(error).__name__)
    return {
        "kind": "startup",
        "name": name,
        "argv": argv,
        "repetitions": repetitions,
        "successful_runs": len(durations),
        "duration_ms": summarize(durations),
        "return_codes": return_codes,
        "errors": errors,
    }


def benchmark_idle(
    name: str,
    argv: list[str],
    repetitions: int,
    idle_seconds: float,
    settle_seconds: float,
    timeout_seconds: float,
) -> dict[str, Any]:
    samples: list[dict[str, float | int | None]] = []
    startup_ms: list[float] = []
    exit_codes: list[int | None] = []
    errors: list[str] = []
    for _ in range(repetitions):
        started = time.perf_counter_ns()
        try:
            process = subprocess.Popen(argv, **idle_launch_kwargs())
        except OSError as error:
            errors.append(type(error).__name__)
            continue
        startup_ms.append((time.perf_counter_ns() - started) / 1_000_000)
        time.sleep(max(0.0, settle_seconds))
        deadline = time.monotonic() + max(0.0, idle_seconds)
        while time.monotonic() < deadline and process.poll() is None:
            sample = read_memory(process.pid)
            if sample["rss_bytes"] is not None:
                samples.append(sample)
            time.sleep(0.05)
        exit_codes.append(process.poll())
        if process.poll() is None:
            terminate_process(process)
        elif process.returncode != 0:
            errors.append(f"exit:{process.returncode}")
        if time.monotonic() - started > timeout_seconds:
            errors.append("measurement_timeout")
            terminate_process(process)

    rss = [float(sample["rss_bytes"]) / 1024 for sample in samples if sample["rss_bytes"] is not None]
    pss = [float(sample["pss_bytes"]) / 1024 for sample in samples if sample["pss_bytes"] is not None]
    cpu = [float(sample["cpu_percent"]) for sample in samples if sample["cpu_percent"] is not None]
    return {
        "kind": "idle_memory",
        "name": name,
        "argv": argv,
        "repetitions": repetitions,
        "startup_ms": summarize(startup_ms),
        "sample_count": len(samples),
        "rss_kib": summarize(rss),
        "pss_kib": summarize(pss),
        "cpu_percent": summarize(cpu),
        "exit_codes": exit_codes,
        "errors": errors,
        "memory_metric_notes": "RSS is resident bytes; PSS is Linux smaps_rollup when available; direct child only.",
    }


def benchmark_concurrency(
    name: str,
    argv: list[str],
    levels: list[int],
    repetitions: int,
    idle_seconds: float,
    settle_seconds: float,
) -> dict[str, Any]:
    measurements: list[dict[str, Any]] = []
    for level in levels:
        runs: list[dict[str, Any]] = []
        for _ in range(repetitions):
            processes: list[subprocess.Popen[bytes]] = []
            started = time.perf_counter_ns()
            errors: list[str] = []
            try:
                for _ in range(level):
                    processes.append(subprocess.Popen(argv, **idle_launch_kwargs()))
                launch_ms = (time.perf_counter_ns() - started) / 1_000_000
                time.sleep(max(0.0, settle_seconds))
                deadline = time.monotonic() + max(0.0, idle_seconds)
                rss_samples: list[float] = []
                pss_samples: list[float] = []
                while time.monotonic() < deadline:
                    rss_total = 0.0
                    pss_total = 0.0
                    pss_complete = True
                    for process in processes:
                        sample = read_memory(process.pid)
                        if sample["rss_bytes"] is not None:
                            rss_total += float(sample["rss_bytes"]) / 1024
                        if sample["pss_bytes"] is None:
                            pss_complete = False
                        else:
                            pss_total += float(sample["pss_bytes"]) / 1024
                    if rss_total:
                        rss_samples.append(rss_total)
                    if pss_complete and pss_total:
                        pss_samples.append(pss_total)
                    time.sleep(0.05)
                runs.append(
                    {
                        "launch_ms": launch_ms,
                        "rss_peak_kib": max(rss_samples) if rss_samples else None,
                        "pss_peak_kib": max(pss_samples) if pss_samples else None,
                    }
                )
            except OSError as error:
                errors.append(type(error).__name__)
            finally:
                for process in processes:
                    terminate_process(process)
            if errors:
                runs.append({"errors": errors})
        measurements.append(
            {
                "sessions": level,
                "repetitions": repetitions,
                "launch_ms": summarize([float(run["launch_ms"]) for run in runs if "launch_ms" in run]),
                "rss_peak_kib": summarize([float(run["rss_peak_kib"]) for run in runs if run.get("rss_peak_kib") is not None]),
                "pss_peak_kib": summarize([float(run["pss_peak_kib"]) for run in runs if run.get("pss_peak_kib") is not None]),
                "runs": runs,
            }
        )
    return {
        "kind": "concurrency_memory",
        "name": name,
        "argv": argv,
        "levels": measurements,
        "memory_metric_notes": "Totals cover the directly launched processes, not descendants; PSS is best effort.",
    }


def telemetry_summary(paths: list[str]) -> dict[str, Any]:
    files: list[str] = []
    records: list[dict[str, Any]] = []
    for pattern in paths:
        matches = sorted(glob.glob(pattern)) or [pattern]
        for match in matches:
            path = Path(match)
            if not path.is_file():
                continue
            files.append(str(path))
            try:
                lines = path.read_text(encoding="utf-8").splitlines()
            except (OSError, UnicodeError):
                continue
            for line in lines:
                try:
                    value = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if isinstance(value, dict) and value.get("schema") == "ygg.telemetry.v1":
                    records.append(value)
    request_latencies = [float(record["elapsed_ms"]) for record in records if record.get("record") == "model_request_finished" and isinstance(record.get("elapsed_ms"), (int, float))]
    ttft = [float(record["ttft_ms"]) for record in records if record.get("record") == "model_request_finished" and isinstance(record.get("ttft_ms"), (int, float))]
    tool_latencies = [float(record["elapsed_ms"]) for record in records if record.get("record") == "tool_finished" and isinstance(record.get("elapsed_ms"), (int, float))]
    run_records = [record for record in records if record.get("record") == "run_finished"]
    return {
        "kind": "agent_telemetry",
        "files": files,
        "records": len(records),
        "model_requests": len([record for record in records if record.get("record") == "model_request_finished"]),
        "tool_calls": len([record for record in records if record.get("record") == "tool_started"]),
        "repeated_tool_calls": sum(int(record.get("repeated_recently", 0)) > 0 for record in records if record.get("record") == "tool_started"),
        "runs": len(run_records),
        "completed_runs": sum(record.get("status") == "completed" for record in run_records),
        "request_elapsed_ms": summarize(request_latencies),
        "ttft_ms": summarize(ttft),
        "tool_elapsed_ms": summarize(tool_latencies),
        "usage_semantics": "uncached_input_tokens + cache_read_tokens + cache_write_tokens = provider_input_tokens; total_tokens is Ygg's normalized canonical total.",
    }


def print_summary(report: dict[str, Any]) -> None:
    print(f"systems benchmark {report['schema']} on {report['environment']['platform']}")
    for measurement in report["measurements"]:
        if measurement["kind"] == "startup":
            print(f"  {measurement['name']}: median {measurement['duration_ms']['median']} ms, p95 {measurement['duration_ms']['p95']} ms")
        elif measurement["kind"] == "idle_memory":
            print(f"  {measurement['name']}: RSS median {measurement['rss_kib']['median']} KiB, peak {measurement['rss_kib']['max']} KiB")
        elif measurement["kind"] == "concurrency_memory":
            for level in measurement["levels"]:
                print(f"  {measurement['name']} x{level['sessions']}: RSS peak median {level['rss_peak_kib']['median']} KiB")
        elif measurement["kind"] == "agent_telemetry":
            print(f"  telemetry: {measurement['runs']} runs, {measurement['model_requests']} model requests, {measurement['tool_calls']} tool calls")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="ygg", help="default executable for the cold-launch case")
    parser.add_argument("--repetitions", type=int, default=DEFAULT_REPETITIONS)
    parser.add_argument("--timeout-seconds", type=float, default=DEFAULT_TIMEOUT_SECONDS)
    parser.add_argument("--idle-seconds", type=float, default=DEFAULT_IDLE_SECONDS)
    parser.add_argument("--settle-seconds", type=float, default=DEFAULT_SETTLE_SECONDS)
    parser.add_argument("--command", action="append", type=parse_named_command, default=[], metavar="NAME=ARGV")
    parser.add_argument("--idle-command", type=parse_named_command, metavar="NAME=ARGV")
    parser.add_argument("--concurrency", default="1,2,4", help="comma-separated process counts for --idle-command")
    parser.add_argument("--telemetry", action="append", default=[], metavar="PATH_OR_GLOB")
    parser.add_argument("--output", type=Path, help="write the complete JSON report to this path")
    args = parser.parse_args()
    if args.repetitions < 1:
        parser.error("--repetitions must be positive")
    if args.timeout_seconds <= 0 or args.idle_seconds < 0 or args.settle_seconds < 0:
        parser.error("timeouts and durations must be non-negative; command timeout must be positive")

    measurements: list[dict[str, Any]] = []
    command_cases = [("cold_launch", [args.binary, "--version"]), *args.command]
    for name, argv in command_cases:
        measurements.append(benchmark_startup(name, argv, args.repetitions, args.timeout_seconds))

    if args.idle_command:
        name, argv = args.idle_command
        measurements.append(
            benchmark_idle(
                name,
                argv,
                args.repetitions,
                args.idle_seconds,
                args.settle_seconds,
                args.timeout_seconds,
            )
        )
        try:
            levels = [int(value) for value in args.concurrency.split(",") if value.strip()]
        except ValueError as error:
            parser.error(f"invalid --concurrency: {error}")
        if any(level < 1 for level in levels):
            parser.error("--concurrency values must be positive")
        measurements.append(
            benchmark_concurrency(
                name,
                argv,
                levels,
                args.repetitions,
                args.idle_seconds,
                args.settle_seconds,
            )
        )

    if args.telemetry:
        measurements.append(telemetry_summary(args.telemetry))

    report = {
        "schema": SCHEMA,
        "created_unix_ms": int(time.time() * 1000),
        "environment": {
            "platform": platform.platform(),
            "machine": platform.machine(),
            "python": platform.python_version(),
            "cpu_count": os.cpu_count(),
            "cwd": str(Path.cwd()),
        },
        "measurements": measurements,
        "methodology": {
            "startup": "wall time around subprocess creation and exit; child stdout/stderr discarded",
            "memory": "sampled after a settle interval; RSS and best-effort Linux PSS; no inference server included",
            "concurrency": "sum of directly launched agent processes at each level",
            "telemetry": "reads ygg.telemetry.v1 without raw prompts, arguments, or provider payloads",
        },
    }
    encoded = json.dumps(report, indent=2, sort_keys=True) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(encoded, encoding="utf-8")
    print_summary(report)
    if args.output:
        print(f"  report: {args.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
