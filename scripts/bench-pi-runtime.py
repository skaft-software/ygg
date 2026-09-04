#!/usr/bin/env python3
"""Reproducible, credential-free Pi runtime evidence harness.

The checked-in fixture driver measures the pinned compatibility bridge without
starting a model provider or reading user homes. It is intentionally a release
*input*, not a release approval: wire an actual runtime-manager adapter before
using the resulting schema as candidate-release evidence.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import platform
import queue
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from typing import Any


SCHEMA = "ygg.pi.runtime.evidence.v1"
DRIVER_SCHEMA = "ygg.pi.runtime.benchmark-driver.v1"
PROFILES = ("no_extension", "legacy_eager", "lazy", "shared_workspace", "pi_aggregate")
MAX_REPETITIONS = 31
MAX_RESOURCE_SAMPLES = 256
MAX_STDERR_BYTES = 16 * 1024


class EvidenceError(RuntimeError):
    """An expected harness failure with a terse, non-secret diagnostic."""


def repository_root() -> Path:
    return Path(__file__).resolve().parents[1]


def load_identity_helpers(root: Path) -> Any:
    path = root / "extensions/ygg-pi-compat/tests/helpers.py"
    spec = importlib.util.spec_from_file_location("ygg_pi_bench_identity", path)
    if spec is None or spec.loader is None:
        raise EvidenceError("cannot load the hermetic Pi identity helper")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def command_output(command: list[str], *, timeout: float = 5.0) -> str | None:
    try:
        completed = subprocess.run(
            command,
            check=True,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
    except (OSError, subprocess.CalledProcessError, subprocess.TimeoutExpired):
        return None
    return completed.stdout.strip()


def scrubbed_environment(work: Path) -> dict[str, str]:
    """Do not pass provider credentials, user homes, npm config, or Ygg config."""
    home = work / "home"
    home.mkdir(parents=True, exist_ok=True)
    environment = {
        "HOME": str(home),
        "XDG_CONFIG_HOME": str(home / ".config"),
        "XDG_CACHE_HOME": str(home / ".cache"),
        "XDG_DATA_HOME": str(home / ".local/share"),
        "PATH": os.defpath,
        "LANG": "C",
        "LC_ALL": "C",
        "TZ": "UTC",
    }
    # Node occasionally needs this on Windows; it is harmless and does not
    # carry a credential. Do not inherit any other environment variable.
    if os.name == "nt" and "SYSTEMROOT" in os.environ:
        environment["SYSTEMROOT"] = os.environ["SYSTEMROOT"]
    return environment


def safe_float(value: float | int | None) -> float | None:
    return None if value is None else round(float(value), 3)


def percentile(values: list[float], point: float) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    index = max(0, math.ceil(point * len(ordered)) - 1)
    return safe_float(ordered[index])


def summary(values: list[float]) -> dict[str, float | int | None]:
    return {
        "count": len(values),
        "median": safe_float(statistics.median(values)) if values else None,
        "p95": percentile(values, 0.95),
        "min": safe_float(min(values)) if values else None,
        "max": safe_float(max(values)) if values else None,
    }


def linux_process_tree(root_pid: int) -> dict[str, int | float | None]:
    records: dict[int, tuple[int, int, int, int]] = {}
    proc = Path("/proc")
    try:
        entries = list(proc.iterdir())
    except OSError:
        return unavailable_resource_sample()
    for entry in entries:
        if not entry.name.isdigit():
            continue
        try:
            raw = (entry / "stat").read_text(encoding="utf-8")
            after = raw[raw.rfind(")") + 2 :].split()
            # Linux proc(5): state, ppid, ..., utime, stime, ..., num_threads.
            records[int(entry.name)] = (
                int(after[1]),
                int(after[11]),
                int(after[12]),
                int(after[17]),
            )
        except (OSError, ValueError, IndexError):
            continue
    descendants = {root_pid}
    changed = True
    while changed:
        changed = False
        for pid, (ppid, _utime, _stime, _threads) in records.items():
            if ppid in descendants and pid not in descendants:
                descendants.add(pid)
                changed = True
    rss_kib = 0
    pss_kib: int | None = 0
    fds = 0
    cpu_ticks = 0
    threads = 0
    page_kib = os.sysconf("SC_PAGE_SIZE") // 1024
    for pid in descendants:
        try:
            fields = (proc / str(pid) / "statm").read_text(encoding="utf-8").split()
            rss_kib += int(fields[1]) * page_kib
        except (OSError, ValueError, IndexError):
            pass
        try:
            for line in (proc / str(pid) / "smaps_rollup").read_text(encoding="utf-8").splitlines():
                if line.startswith("Pss:"):
                    pss_kib = (pss_kib or 0) + int(line.split()[1])
                    break
        except (OSError, ValueError, IndexError):
            pss_kib = None
        try:
            fds += len(list((proc / str(pid) / "fd").iterdir()))
        except OSError:
            pass
        if pid in records:
            _ppid, utime, stime, count = records[pid]
            cpu_ticks += utime + stime
            threads += count
    return {
        "rss_kib": rss_kib,
        "pss_kib": pss_kib,
        "cpu_ticks": cpu_ticks,
        "processes": len(descendants & records.keys()),
        "threads": threads,
        "fd_count": fds,
    }


def mac_process_tree(root_pid: int) -> dict[str, int | float | None]:
    output = command_output(["ps", "-axo", "pid=,ppid=,rss=,pcpu=,thcount="])
    if output is None:
        return unavailable_resource_sample()
    records: dict[int, tuple[int, int, float, int]] = {}
    for line in output.splitlines():
        try:
            pid, ppid, rss, cpu, threads = line.split()
            records[int(pid)] = (int(ppid), int(rss), float(cpu), int(threads))
        except ValueError:
            continue
    descendants = {root_pid}
    changed = True
    while changed:
        changed = False
        for pid, (ppid, _rss, _cpu, _threads) in records.items():
            if ppid in descendants and pid not in descendants:
                descendants.add(pid)
                changed = True
    selected = [records[pid] for pid in descendants if pid in records]
    return {
        "rss_kib": sum(record[1] for record in selected),
        "pss_kib": None,
        "cpu_ticks": None,
        "cpu_percent_snapshot": safe_float(sum(record[2] for record in selected)),
        "processes": len(selected),
        "threads": sum(record[3] for record in selected),
        "fd_count": None,
    }


def unavailable_resource_sample() -> dict[str, int | float | None]:
    return {
        "rss_kib": None,
        "pss_kib": None,
        "cpu_ticks": None,
        "processes": None,
        "threads": None,
        "fd_count": None,
    }


def process_tree_sample(pid: int) -> dict[str, int | float | None]:
    if sys.platform.startswith("linux"):
        return linux_process_tree(pid)
    if sys.platform == "darwin":
        return mac_process_tree(pid)
    return unavailable_resource_sample()


class ResourceSampler:
    def __init__(self, pid: int, interval_ms: int, maximum: int) -> None:
        self.pid = pid
        self.interval = interval_ms / 1000
        self.maximum = maximum
        self.samples: list[dict[str, Any]] = []
        self._samples_lock = threading.Lock()
        self._finish_lock = threading.Lock()
        self._finished = False
        self._origin_ns = time.monotonic_ns()
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._run, daemon=True)

    def start(self) -> None:
        self._capture()
        self._thread.start()

    def _capture(self) -> None:
        with self._samples_lock:
            if len(self.samples) >= self.maximum:
                return
            sample = process_tree_sample(self.pid)
            sample["t_ms"] = round((time.monotonic_ns() - self._origin_ns) / 1_000_000, 3)
            self.samples.append(sample)

    def _run(self) -> None:
        while not self._stop.wait(self.interval):
            self._capture()

    def finish(self) -> list[dict[str, Any]]:
        with self._finish_lock:
            if self._finished:
                return self.samples
            self._stop.set()
            self._thread.join(timeout=max(0.2, self.interval * 2))
            self._capture()
            self._finished = True
            return self.samples


def peak_resource(samples: list[dict[str, Any]]) -> dict[str, int | float | None]:
    result: dict[str, int | float | None] = {}
    for field in ("rss_kib", "pss_kib", "processes", "threads", "fd_count"):
        values = [sample[field] for sample in samples if isinstance(sample.get(field), (int, float))]
        result[f"peak_{field}"] = max(values) if values else None
    cpu_ticks = [sample["cpu_ticks"] for sample in samples if isinstance(sample.get("cpu_ticks"), int)]
    result["cpu_ticks_total"] = (max(cpu_ticks) - min(cpu_ticks)) if len(cpu_ticks) > 1 else 0
    if len(samples) > 1 and cpu_ticks and sys.platform.startswith("linux"):
        elapsed = (samples[-1]["t_ms"] - samples[0]["t_ms"]) / 1000
        ticks = os.sysconf(os.sysconf_names["SC_CLK_TCK"])
        result["cpu_percent_interval"] = safe_float((result["cpu_ticks_total"] / ticks) / elapsed * 100) if elapsed > 0 else 0
    else:
        result["cpu_percent_interval"] = None
    return result


class RpcPeer:
    """Minimal line-JSON RPC peer; retains bounded diagnostics only."""

    def __init__(self, command: list[str], environment: dict[str, str], work: Path) -> None:
        self.process = subprocess.Popen(
            command,
            cwd=work,
            env=environment,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            bufsize=1,
            start_new_session=True,
        )
        self._messages: queue.Queue[dict[str, Any]] = queue.Queue()
        self._pending: dict[int, dict[str, Any]] = {}
        self._next_id = 1
        self._write_lock = threading.Lock()
        self.stderr = ""
        self._stderr_lock = threading.Lock()
        self._stdout_thread = threading.Thread(target=self._read_stdout, daemon=True)
        self._stderr_thread = threading.Thread(target=self._read_stderr, daemon=True)
        self._stdout_thread.start()
        self._stderr_thread.start()

    def _read_stdout(self) -> None:
        assert self.process.stdout is not None
        for line in self.process.stdout:
            try:
                message = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(message, dict):
                self._messages.put(message)

    def _read_stderr(self) -> None:
        assert self.process.stderr is not None
        for line in self.process.stderr:
            with self._stderr_lock:
                if len(self.stderr.encode("utf-8")) < MAX_STDERR_BYTES:
                    self.stderr += line[:1024]

    def request(self, method: str, params: dict[str, Any], timeout: float = 10.0) -> dict[str, Any]:
        request_id = self._next_id
        self._next_id += 1
        assert self.process.stdin is not None
        payload = {"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}
        with self._write_lock:
            self.process.stdin.write(json.dumps(payload, separators=(",", ":")) + "\n")
            self.process.stdin.flush()
        deadline = time.monotonic() + timeout
        while True:
            cached = self._pending.pop(request_id, None)
            if cached is not None:
                return cached
            if self.process.poll() is not None:
                raise EvidenceError("fixture runtime exited before its protocol response")
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise EvidenceError("fixture runtime timed out waiting for its protocol response")
            try:
                message = self._messages.get(timeout=min(remaining, 0.1))
            except queue.Empty:
                continue
            if message.get("id") == request_id and "method" not in message:
                return message
            if isinstance(message.get("id"), int) and "method" not in message:
                self._pending[message["id"]] = message

    def close(self) -> None:
        if self.process.poll() is None:
            try:
                self.request("shutdown", {}, timeout=1.0)
            except EvidenceError:
                try:
                    os.killpg(self.process.pid, signal.SIGTERM)
                except (OSError, ProcessLookupError):
                    pass
        try:
            self.process.wait(timeout=2.0)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(self.process.pid, signal.SIGKILL)
            except (OSError, ProcessLookupError):
                pass
            self.process.wait(timeout=2.0)
        self._stdout_thread.join(timeout=0.2)
        self._stderr_thread.join(timeout=0.2)
        for stream in (self.process.stdin, self.process.stdout, self.process.stderr):
            if stream is not None:
                stream.close()


def bridge_spec(
    root: Path,
    identity: Any,
    work: Path,
    sources: list[Path],
    command_name: str,
) -> tuple[list[str], dict[str, Any], str]:
    node = shutil.which("node")
    if node is None:
        raise EvidenceError("node is required for the Pi runtime evidence harness")
    bridge = root / "extensions/ygg-pi-compat/bridge.mjs"
    fake_pi = root / "extensions/ygg-pi-compat/tests/fixtures/fake-pi"
    agent_dir = work / "agent"
    manifest = work / "manifest" / "extension.toml"
    source_hashes = [identity.compute_source_fingerprint(source) for source in sources]
    lock_hashes = [identity.source_lock_fingerprint(source) for source in sources]
    runtime_hash = identity.runtime_integrity(fake_pi)
    aggregate_digest = hashlib.sha256(
        ("fixture-aggregate\0" + command_name + "\0" + "\0".join(source_hashes)).encode("utf-8")
    ).hexdigest()
    link = identity.link_identity(
        extensions=sources,
        source_hashes=source_hashes,
        lock_hashes=lock_hashes,
        pi_package=fake_pi,
        pi_runtime_integrity=runtime_hash,
        aggregate_digest=aggregate_digest,
        manifest_path=manifest,
        command_name=command_name,
        ygg_version="0.6.7",
        agent_dir=agent_dir,
    )
    command = [node, str(bridge)]
    for source, source_hash, lock_hash in zip(sources, source_hashes, lock_hashes, strict=True):
        command.extend(
            [
                "--extension",
                str(source),
                "--source-fingerprint",
                source_hash,
                "--source-lock-fingerprint",
                lock_hash,
            ]
        )
    command.extend(
        [
            "--agent-dir",
            str(agent_dir),
            "--pi-package",
            str(fake_pi),
            "--pi-runtime-integrity",
            runtime_hash,
            "--aggregate-digest",
            aggregate_digest,
            "--link-manifest",
            str(manifest),
            "--link-identity",
            link,
            "--ygg-version",
            "0.6.7",
            "--command",
            command_name,
        ]
    )
    params = {
        "workspace": str(root),
        "host": {},
        "protocol": {"optional_features": ["lifecycle_events"]},
        "ygg_version": "0.6.7",
        "extension": {
            "name": command_name,
            "version": "fixture",
            "manifest_path": str(manifest),
            "source": "explicit",
        },
    }
    return command, params, "aggregate_state" if len(sources) > 1 else "fixture_echo"


def baseline_spec(root: Path) -> tuple[list[str], dict[str, Any], str]:
    node = shutil.which("node")
    if node is None:
        raise EvidenceError("node is required for the Pi runtime evidence harness")
    return [node, str(root / "extensions/ygg-pi-compat/tests/fixtures/runtime-idle.mjs")], {"workspace": str(root)}, "activate"


def start_and_initialize(
    command: list[str],
    params: dict[str, Any],
    environment: dict[str, str],
    work: Path,
    interval_ms: int,
    maximum_samples: int,
) -> tuple[RpcPeer, ResourceSampler, float]:
    started = time.perf_counter_ns()
    peer = RpcPeer(command, environment, work)
    sampler = ResourceSampler(peer.process.pid, interval_ms, maximum_samples)
    sampler.start()
    try:
        response = peer.request("initialize", params)
        if "error" in response:
            raise EvidenceError("fixture runtime rejected initialization")
    except BaseException:
        peer.close()
        sampler.finish()
        raise
    return peer, sampler, (time.perf_counter_ns() - started) / 1_000_000


def activate(peer: RpcPeer, tool: str) -> float:
    started = time.perf_counter_ns()
    if tool == "activate":
        response = peer.request("activate", {})
    else:
        response = peer.request("tool/call", {"name": tool, "arguments": {}, "catalog_revision": 0})
    if "error" in response:
        raise EvidenceError("fixture runtime rejected activation")
    return (time.perf_counter_ns() - started) / 1_000_000


def one_profile(
    profile: str,
    root: Path,
    identity: Any,
    environment: dict[str, str],
    interval_ms: int,
    maximum_samples: int,
) -> dict[str, Any]:
    fixtures = root / "extensions/ygg-pi-compat/tests/fixtures"
    extension = fixtures / "fixture-extension.mjs"
    aggregate = [fixtures / "aggregate/first.mjs", fixtures / "aggregate/second.mjs"]
    with tempfile.TemporaryDirectory(prefix="ygg-pi-evidence-") as directory:
        work = Path(directory)
        if profile == "no_extension":
            command, params, tool = baseline_spec(root)
            lazy = False
        elif profile == "pi_aggregate":
            command, params, tool = bridge_spec(root, identity, work, aggregate, "pi-aggregate")
            lazy = False
        elif profile == "legacy_eager":
            command, params, tool = bridge_spec(root, identity, work, [extension], "pi-legacy")
            lazy = False
        elif profile == "shared_workspace":
            command, params, tool = bridge_spec(root, identity, work, [extension], "pi-shared")
            lazy = False
        elif profile == "lazy":
            command, params, tool = baseline_spec(root)
            lazy = True
        else:
            raise EvidenceError(f"unknown benchmark profile {profile}")

        peer, sampler, ready_ms = start_and_initialize(
            command, params, environment, work, interval_ms, maximum_samples
        )
        lazy_peer: RpcPeer | None = None
        lazy_sampler: ResourceSampler | None = None
        try:
            if lazy:
                lazy_command, lazy_params, lazy_tool = bridge_spec(root, identity, work / "lazy", [extension], "pi-lazy")
                activation_start = time.perf_counter_ns()
                lazy_peer, lazy_sampler, lazy_ready_ms = start_and_initialize(
                    lazy_command, lazy_params, environment, work, interval_ms, maximum_samples
                )
                first_ms = lazy_ready_ms + activate(lazy_peer, lazy_tool)
                first_ms = max(first_ms, (time.perf_counter_ns() - activation_start) / 1_000_000)
                warm_ms = activate(lazy_peer, lazy_tool)
                active_peer, active_tool = lazy_peer, lazy_tool
            else:
                first_ms = activate(peer, tool)
                warm_ms = activate(peer, tool)
                active_peer, active_tool = peer, tool

            shared_reuse_ms: float | None = None
            if profile == "shared_workspace":
                active_peer.request("session/started", {})
                activate(active_peer, active_tool)
                active_peer.request("session/settled", {"outcome": "completed"})
                started = time.perf_counter_ns()
                active_peer.request("session/started", {})
                activate(active_peer, active_tool)
                active_peer.request("session/settled", {"outcome": "completed"})
                shared_reuse_ms = (time.perf_counter_ns() - started) / 1_000_000

            # Reload is deliberately a process replacement fixture. It is not
            # claimed to be #254's future manager-driven hot reload.
            active_peer.close()
            if active_peer is lazy_peer:
                assert lazy_sampler is not None
                active_samples = lazy_sampler.finish()
                lazy_peer = None
                lazy_sampler = None
            else:
                active_samples = sampler.finish()
                peer = None  # type: ignore[assignment]
            reload_command, reload_params, _reload_tool = (
                bridge_spec(root, identity, work / "reload", aggregate if profile == "pi_aggregate" else [extension], "pi-reload")
                if profile not in ("no_extension", "lazy")
                else (bridge_spec(root, identity, work / "reload", [extension], "pi-reload") if profile == "lazy" else baseline_spec(root))
            )
            reload_peer, reload_sampler, reload_ms = start_and_initialize(
                reload_command, reload_params, environment, work, interval_ms, maximum_samples
            )
            reload_peer.close()
            reload_samples = reload_sampler.finish()
            base_samples = [] if peer is None else sampler.finish()
            return {
                "profile": profile,
                "driver": "hermetic_fixture",
                "lifecycle_profile": "pi_aggregate" if profile == "pi_aggregate" else profile,
                "startup_readiness_ms": safe_float(ready_ms),
                "first_activation_ms": safe_float(first_ms),
                "warm_call_ms": safe_float(warm_ms),
                "process_restart_readiness_ms": safe_float(reload_ms),
                "shared_workspace_reuse_ms": safe_float(shared_reuse_ms),
                "agent": {
                    "initial_process": peak_resource(base_samples),
                    "active_extension_process": peak_resource(active_samples),
                    "reload_process": peak_resource(reload_samples),
                },
                "raw_resource_samples": {
                    "initial_process": base_samples,
                    "active_extension_process": active_samples,
                    "reload_process": reload_samples,
                },
                "inference": {
                    "included": False,
                    "reason": "hermetic runtime fixture does not launch or contact an inference server",
                },
            }
        finally:
            if lazy_peer is not None:
                lazy_peer.close()
            if lazy_sampler is not None:
                lazy_sampler.finish()
            if peer is not None:
                peer.close()
            sampler.finish()


def compact_raw(sample: dict[str, Any], max_samples: int) -> dict[str, Any]:
    """Defensively enforce the committed-artifact raw sample bound."""
    for group in sample.get("raw_resource_samples", {}).values():
        if isinstance(group, list) and len(group) > max_samples:
            del group[max_samples:]
    return sample


def aggregate_profile_runs(runs: list[dict[str, Any]]) -> dict[str, Any]:
    measurements = ("startup_readiness_ms", "first_activation_ms", "warm_call_ms", "process_restart_readiness_ms")
    result: dict[str, Any] = {"runs": runs, "summary": {}}
    for measurement in measurements:
        result["summary"][measurement] = summary(
            [float(run[measurement]) for run in runs if run.get(measurement) is not None]
        )
    values = [run.get("shared_workspace_reuse_ms") for run in runs]
    result["summary"]["shared_workspace_reuse_ms"] = summary([float(value) for value in values if value is not None])
    return result


def release_decision(
    profiles: dict[str, Any], startup_budget_ms: float | None, inference_included: bool
) -> dict[str, Any]:
    baseline = profiles["no_extension"]["summary"]["startup_readiness_ms"]["median"]
    aggregate = profiles["pi_aggregate"]["summary"]["startup_readiness_ms"]["median"]
    overhead = None if baseline is None or aggregate is None else safe_float(aggregate - baseline)
    reasons = [
        "HOLD: checked-in driver is a hermetic Pi bridge fixture, not an actual candidate runtime-manager adapter.",
        "HOLD: Linux and macOS candidate runs must both be reviewed before a release decision.",
        "HOLD: no inference server was launched; agent and inference resources remain intentionally separate."
        if not inference_included
        else "HOLD: external inference-process snapshots are separate from agent process-tree measurements.",
    ]
    if startup_budget_ms is not None and aggregate is not None and aggregate > startup_budget_ms:
        reasons.append(f"FAIL: aggregate median startup {aggregate} ms exceeds configured budget {startup_budget_ms} ms.")
    return {
        "status": "hold",
        "baseline_attribution": {
            "baseline_profile": "no_extension",
            "pi_aggregate_profile": "pi_aggregate",
            "startup_median_overhead_ms": overhead,
        },
        "reasons": reasons,
    }


def inference_evidence(pid: int | None) -> dict[str, Any]:
    if pid is None:
        return {
            "included": False,
            "reason": "no --inference-pid supplied; no inference process was launched or contacted",
            "gpu": {"available": False, "reason": "no inference process was sampled"},
        }
    sample = process_tree_sample(pid)
    if sample.get("processes") in (None, 0):
        return {
            "included": False,
            "reason": "the explicitly requested inference pid was unavailable",
            "gpu": {"available": False, "reason": "inference pid was unavailable"},
        }
    return {
        "included": True,
        "sampling": "explicit_pid_process_tree_snapshot",
        "resource": peak_resource([sample]),
        "gpu": {
            "available": False,
            "reason": "this portable harness records CPU/process metrics only; attach a platform GPU collector when relevant",
        },
    }


def sha256_file(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def fixture_inputs(root: Path, identity: Any) -> dict[str, Any]:
    fixtures = root / "extensions/ygg-pi-compat/tests/fixtures"
    runtime = fixtures / "fake-pi"
    try:
        package = json.loads((runtime / "package.json").read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise EvidenceError("cannot read the checked-in fake Pi package identity") from error
    source_paths = {
        "single_extension": fixtures / "fixture-extension.mjs",
        "aggregate_first": fixtures / "aggregate/first.mjs",
        "aggregate_second": fixtures / "aggregate/second.mjs",
        "no_extension_idle": fixtures / "runtime-idle.mjs",
    }
    return {
        "adapter": "hermetic_fixture",
        "benchmark_driver_sha256": sha256_file(Path(__file__).resolve()),
        "bridge": {"path": "extensions/ygg-pi-compat/bridge.mjs", "sha256": sha256_file(root / "extensions/ygg-pi-compat/bridge.mjs")},
        "pi_runtime": {
            "kind": "checked_in_fake_pi",
            "name": package.get("name"),
            "version": package.get("version"),
            "integrity_sha256": identity.runtime_integrity(runtime),
        },
        "sources": {
            name: {
                "path": str(path.relative_to(root)).replace(os.sep, "/"),
                "source_sha256": identity.compute_source_fingerprint(path),
                "dependency_lock_sha256": identity.source_lock_fingerprint(path),
            }
            for name, path in source_paths.items()
        },
    }


def system_metadata(candidate: str, node: str | None) -> dict[str, Any]:
    memory_kib = None
    if sys.platform.startswith("linux"):
        try:
            for line in Path("/proc/meminfo").read_text(encoding="utf-8").splitlines():
                if line.startswith("MemTotal:"):
                    memory_kib = int(line.split()[1])
                    break
        except OSError:
            pass
    cpu_model = None
    if sys.platform.startswith("linux"):
        try:
            cpu_model = next(
                (line.split(":", 1)[1].strip() for line in Path("/proc/cpuinfo").read_text().splitlines() if line.startswith("model name")),
                None,
            )
        except OSError:
            pass
    if sys.platform == "darwin":
        cpu_model = command_output(["sysctl", "-n", "machdep.cpu.brand_string"])
        memory_bytes = command_output(["sysctl", "-n", "hw.memsize"])
        if memory_bytes and memory_bytes.isdigit():
            memory_kib = int(memory_bytes) // 1024
    return {
        "candidate": candidate,
        "captured_at_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "platform": {"system": platform.system(), "release": platform.release(), "machine": platform.machine()},
        "hardware": {"logical_cpus": os.cpu_count(), "memory_kib": memory_kib, "cpu_model": cpu_model},
        "toolchain": {"python": sys.version.split()[0], "node": node, "extension_api_evidence_version": "0.3"},
        "safety": {
            "network_calls": False,
            "provider_calls": False,
            "credentials_inherited": False,
            "home_is_temporary": True,
        },
    }


def write_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    temporary.replace(path)


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--candidate", required=True, help="Exact candidate revision or immutable build identifier.")
    parser.add_argument("--output", type=Path, required=True, help="Directory for bounded JSON evidence.")
    parser.add_argument("--repetitions", type=int, default=5, help=f"Per-profile repetitions (1-{MAX_REPETITIONS}).")
    parser.add_argument("--sample-interval-ms", type=int, default=20)
    parser.add_argument("--max-resource-samples", type=int, default=64)
    parser.add_argument("--startup-budget-ms", type=float, default=None)
    parser.add_argument(
        "--inference-pid",
        type=int,
        default=None,
        help="Explicit external inference pid to snapshot separately; it is never contacted or controlled.",
    )
    return parser.parse_args()


def main() -> int:
    arguments = parse_arguments()
    if not 1 <= arguments.repetitions <= MAX_REPETITIONS:
        raise EvidenceError(f"--repetitions must be in 1..{MAX_REPETITIONS}")
    if not 1 <= arguments.max_resource_samples <= MAX_RESOURCE_SAMPLES:
        raise EvidenceError(f"--max-resource-samples must be in 1..{MAX_RESOURCE_SAMPLES}")
    if arguments.sample_interval_ms < 5:
        raise EvidenceError("--sample-interval-ms must be at least 5")
    if not arguments.candidate.strip():
        raise EvidenceError("--candidate must be a non-empty immutable identifier")
    if arguments.inference_pid is not None and arguments.inference_pid <= 0:
        raise EvidenceError("--inference-pid must be positive")
    root = repository_root()
    identity = load_identity_helpers(root)
    node = command_output([shutil.which("node") or "node", "--version"])
    if node is None:
        raise EvidenceError("node is required for the Pi runtime evidence harness")

    output = arguments.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    all_runs: dict[str, list[dict[str, Any]]] = {profile: [] for profile in PROFILES}
    with tempfile.TemporaryDirectory(prefix="ygg-pi-evidence-home-") as environment_directory:
        environment = scrubbed_environment(Path(environment_directory))
        for profile in PROFILES:
            for repetition in range(arguments.repetitions):
                run = one_profile(
                    profile,
                    root,
                    identity,
                    environment,
                    arguments.sample_interval_ms,
                    arguments.max_resource_samples,
                )
                run["repetition"] = repetition + 1
                all_runs[profile].append(compact_raw(run, arguments.max_resource_samples))

    profiles = {profile: aggregate_profile_runs(runs) for profile, runs in all_runs.items()}
    inference = inference_evidence(arguments.inference_pid)
    artifact = {
        "schema": SCHEMA,
        "schema_version": 1,
        "api": {"version": "0.3", "schema": "ygg.extension.api/0.3"},
        "driver": {"schema": DRIVER_SCHEMA, "name": "hermetic_fixture", "reload_semantics": "process_restart"},
        "inputs": fixture_inputs(root, identity),
        "metadata": system_metadata(arguments.candidate, node),
        "collection": {
            "profiles": list(PROFILES),
            "repetitions": arguments.repetitions,
            "resource_sample_interval_ms": arguments.sample_interval_ms,
            "max_resource_samples_per_process": arguments.max_resource_samples,
            "raw_samples_bounded": True,
        },
        "profiles": profiles,
        "inference_server": inference,
        "release_decision": release_decision(
            profiles,
            arguments.startup_budget_ms,
            bool(inference["included"]),
        ),
    }
    write_json(output / "results.json", artifact)
    digest = hashlib.sha256((output / "results.json").read_bytes()).hexdigest()
    (output / "SHA256SUMS").write_text(f"{digest}  results.json\n", encoding="utf-8")
    print(json.dumps({"output": str(output), "sha256": digest, "decision": artifact["release_decision"]["status"]}))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except EvidenceError as error:
        print(f"bench-pi-runtime: {error}", file=sys.stderr)
        raise SystemExit(2)
