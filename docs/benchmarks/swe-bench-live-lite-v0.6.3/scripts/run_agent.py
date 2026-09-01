#!/usr/bin/env python3
"""Run Ygg exactly once per selected SWE-bench-Live Lite instance.

The runner intentionally keeps dataset/evaluator data on the host.  Only the
problem statement is placed in the Ygg argv; gold fields never enter the task
container.  The task image is disposable and the only writable host mount is
that instance's evidence directory.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import io
import json
import os
import re
import shlex
import stat
import subprocess
import tarfile
import threading
import time
import traceback
import uuid
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable

try:
    import docker
    from docker.errors import APIError, ImageNotFound
except ImportError as error:  # pragma: no cover - setup failure
    raise SystemExit("docker is required; install scripts/requirements.txt") from error

SCRIPT_DIR = Path(__file__).resolve().parent
BENCHMARK_ROOT = SCRIPT_DIR.parent
REPO_ROOT = BENCHMARK_ROOT.parents[2]
import sys

sys.path.insert(0, str(SCRIPT_DIR))
sys.path.insert(0, str(REPO_ROOT))
from common import (  # noqa: E402
    DOCKER_PLATFORM,
    IMAGE_ARCH,
    IMAGE_NAMESPACE,
    IMAGE_TAG,
    ROOT,
    TASK_LOG_ROOT,
    TASK_WORKSPACE,
    YGG_BINARY_TARGET,
    YGG_COMMIT,
    YGG_RELEASE_REF,
    YGG_VERSION,
    canonical_json,
    ensure_dataset,
    image_reference,
    load_rows,
    load_selection,
    rows_by_id,
    sha256_bytes,
    sha256_file,
    system_prompt_identity,
    tool_schema_identity,
    write_json,
    write_jsonl,
)

TIMEOUT_GRACE_SECONDS = 15
EXEC_GRACE_SECONDS = 10
SETUP_TIMEOUT_SECONDS = 180
CAPTURE_TIMEOUT_SECONDS = 180


@dataclass
class ExecResult:
    return_code: int | None
    stdout: str
    stderr: str
    duration_seconds: float
    timed_out: bool = False
    error: str | None = None


@dataclass
class ResourceMonitor:
    """Best-effort task-container process and cgroup memory sampler."""

    container: Any
    interval_seconds: float = 0.5
    stop_event: threading.Event = field(default_factory=threading.Event)
    thread: threading.Thread | None = None
    peak_ygg_rss_kib: int | None = None
    peak_process_tree_rss_kib: int | None = None
    peak_container_memory_bytes: int | None = None
    samples: int = 0
    errors: int = 0

    def start(self) -> None:
        self.thread = threading.Thread(target=self._run, name="swebench-resource-monitor", daemon=True)
        self.thread.start()

    def stop(self) -> None:
        self.stop_event.set()
        if self.thread is not None:
            self.thread.join(timeout=EXEC_GRACE_SECONDS)

    @staticmethod
    def _number(value: Any) -> int | None:
        try:
            return int(value)
        except (TypeError, ValueError):
            return None

    def _sample_top(self) -> None:
        try:
            top = self.container.top(ps_args="-eo pid,ppid,rss,args")
            titles = [str(value).casefold() for value in top.get("Titles", [])]
            processes = top.get("Processes", [])
            indices = {name: index for index, name in enumerate(titles)}
            pid_index = indices.get("pid", 0)
            ppid_index = indices.get("ppid", 1)
            rss_index = indices.get("rss", 2)
            args_index = indices.get("args", len(titles) - 1)
            rows: list[tuple[int, int, int, str]] = []
            for process in processes:
                if not isinstance(process, (list, tuple)):
                    continue
                try:
                    pid = int(process[pid_index])
                    ppid = int(process[ppid_index])
                    rss = int(process[rss_index])
                    args = " ".join(str(value) for value in process[args_index:])
                except (IndexError, TypeError, ValueError):
                    continue
                rows.append((pid, ppid, max(0, rss), args))
            if not rows:
                return
            ygg_roots = {
                pid
                for pid, _ppid, _rss, args in rows
                if "/usr/local/bin/ygg" in args
            }
            if not ygg_roots:
                return
            children: dict[int, list[int]] = {}
            for pid, ppid, _rss, _args in rows:
                children.setdefault(ppid, []).append(pid)
            tree = set(ygg_roots)
            pending = list(ygg_roots)
            while pending:
                parent = pending.pop()
                for child in children.get(parent, []):
                    if child not in tree:
                        tree.add(child)
                        pending.append(child)
            by_pid = {pid: (rss, args) for pid, _ppid, rss, args in rows}
            ygg_rss = sum(by_pid[pid][0] for pid in ygg_roots if pid in by_pid)
            tree_rss = sum(by_pid[pid][0] for pid in tree if pid in by_pid)
            self.peak_ygg_rss_kib = max(self.peak_ygg_rss_kib or 0, ygg_rss)
            self.peak_process_tree_rss_kib = max(
                self.peak_process_tree_rss_kib or 0, tree_rss
            )
        except Exception:
            self.errors += 1

    def _sample_stats(self) -> None:
        try:
            stats = self.container.stats(stream=False)
            if isinstance(stats, bytes):
                stats = json.loads(stats)
            memory = stats.get("memory_stats", {}) if isinstance(stats, dict) else {}
            current = self._number(memory.get("usage"))
            if current is not None:
                self.peak_container_memory_bytes = max(
                    self.peak_container_memory_bytes or 0, current
                )
        except Exception:
            self.errors += 1

    def _run(self) -> None:
        while not self.stop_event.is_set():
            self._sample_top()
            self._sample_stats()
            self.samples += 1
            self.stop_event.wait(self.interval_seconds)


class ContainerStopped(RuntimeError):
    pass


def now_iso() -> str:
    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")


def safe_name(value: str) -> str:
    return re.sub(r"[^a-zA-Z0-9_.-]+", "-", value).strip("-")[:80] or "task"


def decode(value: bytes | str | None) -> str:
    if value is None:
        return ""
    if isinstance(value, bytes):
        return value.decode("utf-8", errors="replace")
    return value


def tar_bytes(files: dict[str, bytes], *, mode: int = 0o600) -> bytes:
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w") as archive:
        for name, content in files.items():
            info = tarfile.TarInfo(name=name)
            info.size = len(content)
            info.mode = mode
            info.uid = 0
            info.gid = 0
            info.mtime = 0
            archive.addfile(info, io.BytesIO(content))
    return output.getvalue()


def stop_container(container: Any) -> None:
    try:
        container.stop(timeout=5)
    except Exception:
        try:
            container.kill()
        except Exception:
            pass


def exec_stream(
    container: Any,
    argv: list[str],
    *,
    workdir: str | None = None,
    environment: dict[str, str] | None = None,
    timeout_seconds: float | None = None,
    on_timeout: Callable[[], None] | None = None,
) -> ExecResult:
    """Run one direct Docker exec and retain separate stdout/stderr streams."""

    started = time.monotonic()
    stdout = bytearray()
    stderr = bytearray()
    error_holder: list[str] = []
    api = container.client.api
    try:
        created = api.exec_create(
            container.id,
            cmd=argv,
            user="0",
            workdir=workdir,
            environment=environment,
        )
        exec_id = created["Id"]
    except Exception as error:
        return ExecResult(
            None,
            "",
            "",
            time.monotonic() - started,
            error=f"exec_create: {type(error).__name__}: {error}",
        )

    def consume() -> None:
        try:
            stream = api.exec_start(exec_id, stream=True, demux=True)
            for chunk in stream:
                if isinstance(chunk, tuple):
                    out, err = chunk
                    if out:
                        stdout.extend(out)
                    if err:
                        stderr.extend(err)
                elif chunk:
                    stdout.extend(chunk)
        except Exception as error:  # the inspect result remains useful
            error_holder.append(f"exec_start: {type(error).__name__}: {error}")

    thread = threading.Thread(target=consume, name="docker-exec-reader", daemon=True)
    thread.start()
    timed_out = False
    if timeout_seconds is None:
        thread.join()
    else:
        thread.join(timeout_seconds)
        if thread.is_alive():
            timed_out = True
            if on_timeout is not None:
                try:
                    on_timeout()
                except Exception as error:
                    error_holder.append(f"timeout cleanup: {type(error).__name__}: {error}")
            thread.join(TIMEOUT_GRACE_SECONDS)
            if thread.is_alive():
                stop_container(container)
                thread.join(EXEC_GRACE_SECONDS)

    try:
        inspected = api.exec_inspect(exec_id)
        return_code = inspected.get("ExitCode")
    except Exception as error:
        return_code = 124 if timed_out else None
        error_holder.append(f"exec_inspect: {type(error).__name__}: {error}")
    return ExecResult(
        return_code,
        decode(bytes(stdout)),
        decode(bytes(stderr)),
        time.monotonic() - started,
        timed_out=timed_out,
        error="; ".join(error_holder) if error_holder else None,
    )


def command_result_ok(result: ExecResult) -> bool:
    return result.return_code == 0 and not result.error and not result.timed_out


def compact_result(result: ExecResult) -> dict[str, Any]:
    return {
        "return_code": result.return_code,
        "duration_seconds": round(result.duration_seconds, 3),
        "timed_out": result.timed_out,
        "error": result.error,
        "stdout_bytes": len(result.stdout.encode("utf-8")),
        "stderr_bytes": len(result.stderr.encode("utf-8")),
    }


def process_failure_kind(result: ExecResult) -> str:
    if result.timed_out:
        return "benchmark_timeout"
    if result.return_code == 0:
        return "completed_process"
    text = f"{result.stdout}\n{result.stderr}".casefold()
    provider_markers = (
        "api key",
        "authentication",
        "unauthorized",
        "forbidden",
        "rate limit",
        "quota exceeded",
        "provider error",
        "model not found",
        "connection refused",
        "connection reset",
        "network error",
        "http 401",
        "http 403",
        "http 429",
        "http 500",
        "http 502",
        "http 503",
    )
    if any(marker in text for marker in provider_markers):
        return "provider_failure"
    return "agent_process_failure"


def write_text(path: Path, value: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(value, encoding="utf-8")


def image_identity(client: Any, reference: str, *, pull_timeout_seconds: int = 300) -> dict[str, Any]:
    del client  # image acquisition is deliberately bounded through the CLI

    def inspect() -> dict[str, Any] | None:
        try:
            inspected = subprocess.run(
                ["docker", "image", "inspect", reference],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                timeout=30,
                check=False,
            )
        except subprocess.TimeoutExpired as error:
            raise TimeoutError(f"timed out inspecting task image: {reference}") from error
        if inspected.returncode != 0:
            return None
        try:
            values = json.loads(inspected.stdout)
        except json.JSONDecodeError as error:
            raise RuntimeError(f"docker image inspect returned invalid JSON: {reference}") from error
        if not isinstance(values, list) or not values or not isinstance(values[0], dict):
            raise RuntimeError(f"docker image inspect returned no image: {reference}")
        return values[0]

    attrs = inspect()
    source = "docker.io"
    acquisition_error = None
    if attrs is None:
        try:
            pulled = subprocess.run(
                ["docker", "pull", "--platform", DOCKER_PLATFORM, reference],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                timeout=pull_timeout_seconds,
                check=False,
            )
        except subprocess.TimeoutExpired as error:
            pulled = None
            acquisition_error = f"docker pull timed out after {pull_timeout_seconds}s: {error}"
        if pulled is not None and pulled.returncode == 0:
            attrs = inspect()
        elif pulled is not None:
            acquisition_error = (pulled.stderr or pulled.stdout or "no docker pull output").strip()[-1000:]

        if attrs is None:
            mirror = f"mirror.gcr.io/{reference}"
            try:
                mirror_pull = subprocess.run(
                    ["docker", "pull", "--platform", DOCKER_PLATFORM, mirror],
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                    timeout=pull_timeout_seconds,
                    check=False,
                )
            except subprocess.TimeoutExpired as error:
                raise TimeoutError(
                    f"timed out pulling task image from Docker Hub and mirror: {reference}"
                ) from error
            if mirror_pull.returncode != 0:
                detail = (mirror_pull.stderr or mirror_pull.stdout or "no mirror pull output").strip()
                raise RuntimeError(
                    f"docker pull failed for {reference}; mirror fallback also failed: {detail[-1000:]}"
                )
            tag = subprocess.run(
                ["docker", "tag", mirror, reference],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                timeout=30,
                check=False,
            )
            if tag.returncode != 0:
                raise RuntimeError(f"failed to retag mirror image for {reference}: {tag.stderr[-1000:]}")
            subprocess.run(
                ["docker", "image", "rm", mirror],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                timeout=30,
                check=False,
            )
            attrs = inspect()
            if attrs is None:
                raise RuntimeError(f"mirror pull completed but image is not inspectable: {reference}")
            source = "mirror.gcr.io"
    repo_digests = attrs.get("RepoDigests", []) if isinstance(attrs, dict) else []
    digest = None
    prefix = reference.split(":", 1)[0] + "@"
    for candidate in repo_digests:
        if isinstance(candidate, str) and candidate.startswith(prefix):
            digest = candidate.split("@", 1)[1]
            break
    return {
        "reference": reference,
        "image_id": attrs.get("Id") if isinstance(attrs, dict) else None,
        "architecture": attrs.get("Architecture") if isinstance(attrs, dict) else None,
        "os": attrs.get("Os") if isinstance(attrs, dict) else None,
        "repo_digests": repo_digests,
        "resolved_digest": digest,
        "acquisition_source": source,
        "direct_acquisition_error": acquisition_error,
    }


def copy_credentials(container: Any, credential_dir: Path) -> list[str]:
    files: dict[str, bytes] = {}
    for filename in ("codex.json", "codex-models.json"):
        source = credential_dir / filename
        if source.is_file():
            files[f"credentials/{filename}"] = source.read_bytes()
    if "credentials/codex.json" not in files:
        raise FileNotFoundError(f"missing {credential_dir / 'codex.json'}")
    # The archive contains only disposable copies.  It is never written to an
    # evidence log and is not mounted back to the host after the task.
    result = exec_stream(
        container,
        ["mkdir", "-p", "/root/.ygg/credentials"],
        timeout_seconds=30,
    )
    if not command_result_ok(result):
        raise RuntimeError(f"creating credential directory failed: {compact_result(result)}")
    if not container.put_archive("/root/.ygg", tar_bytes(files)):
        raise RuntimeError("Docker rejected the disposable credential archive")
    chmod = exec_stream(
        container,
        ["chmod", "700", "/root/.ygg", "/root/.ygg/credentials"],
        timeout_seconds=30,
    )
    if not command_result_ok(chmod):
        raise RuntimeError(f"credential directory permission setup failed: {compact_result(chmod)}")
    for filename in ("codex.json", "codex-models.json"):
        if filename in {Path(name).name for name in files}:
            chmod_file = exec_stream(
                container,
                ["chmod", "600", f"/root/.ygg/credentials/{filename}"],
                timeout_seconds=30,
            )
            if not command_result_ok(chmod_file):
                raise RuntimeError(f"credential file permission setup failed: {filename}")
    return sorted(Path(name).name for name in files)


def copy_binary(container: Any, binary: Path, expected_sha256: str) -> dict[str, Any]:
    content = binary.read_bytes()
    if sha256_bytes(content) != expected_sha256:
        raise ValueError("binary changed after its host hash was recorded")
    if not container.put_archive("/usr/local/bin", tar_bytes({"ygg": content}, mode=0o555)):
        raise RuntimeError("Docker rejected the Ygg binary archive")
    chmod = exec_stream(container, ["chmod", "0555", "/usr/local/bin/ygg"], timeout_seconds=30)
    if not command_result_ok(chmod):
        raise RuntimeError(f"binary chmod failed: {compact_result(chmod)}")
    digest_result = exec_stream(container, ["sha256sum", "/usr/local/bin/ygg"], timeout_seconds=30)
    version_result = exec_stream(container, ["/usr/local/bin/ygg", "--version"], timeout_seconds=30)
    observed = digest_result.stdout.split()[0] if digest_result.stdout.split() else None
    version = version_result.stdout.strip()
    if observed != expected_sha256:
        raise RuntimeError(f"container binary hash mismatch: {observed}")
    if version not in {f"ygg {YGG_VERSION}", YGG_VERSION}:
        raise RuntimeError(f"container Ygg version mismatch: {version!r}")
    return {
        "sha256": observed,
        "version_output": version,
        "digest_check": compact_result(digest_result),
        "version_check": compact_result(version_result),
    }


def prepare_repository(container: Any, base_commit: str) -> tuple[bool, dict[str, Any]]:
    commands = [
        ["git", "config", "--global", "--add", "safe.directory", TASK_WORKSPACE],
        ["git", "checkout", "--detach", base_commit],
        ["git", "reset", "--hard", base_commit],
        ["git", "clean", "-fdx"],
        ["git", "rev-parse", "HEAD"],
        ["git", "status", "--porcelain=v1"],
    ]
    records: list[dict[str, Any]] = []
    for command in commands:
        result = exec_stream(
            container,
            command,
            workdir=TASK_WORKSPACE,
            timeout_seconds=SETUP_TIMEOUT_SECONDS,
        )
        records.append({"argv": command, **compact_result(result)})
        if not command_result_ok(result):
            return False, {"commands": records, "error": "repository setup command failed"}
        if command[-1] == "HEAD" and result.stdout.strip() != base_commit:
            return False, {"commands": records, "error": "base commit mismatch"}
        if command[1:3] == ["status", "--porcelain=v1"] and result.stdout.strip():
            return False, {"commands": records, "error": "working tree is not clean"}
    return True, {"commands": records, "base_commit": base_commit, "working_tree_clean": True}


def capture_patch(container: Any, instance_dir: Path) -> dict[str, Any]:
    records: list[dict[str, Any]] = []
    for command in [
        ["git", "add", "-N", "--all"],
        ["git", "diff", "--binary", "--no-ext-diff", "HEAD"],
        ["git", "status", "--porcelain=v1"],
        ["git", "diff", "--name-only", "HEAD"],
        ["git", "diff", "--numstat", "HEAD"],
    ]:
        result = exec_stream(
            container,
            command,
            workdir=TASK_WORKSPACE,
            timeout_seconds=CAPTURE_TIMEOUT_SECONDS,
        )
        records.append({"argv": command, **compact_result(result)})
        if not command_result_ok(result):
            return {"ok": False, "commands": records, "error": "diff capture command failed"}
        if command[1:3] == ["diff", "--binary"]:
            write_text(instance_dir / "final_patch.diff", result.stdout)
        elif command[1:3] == ["status", "--porcelain=v1"]:
            write_text(instance_dir / "git-status.txt", result.stdout)
        elif command[1:3] == ["diff", "--name-only"]:
            write_text(instance_dir / "changed-files.txt", result.stdout)
        elif command[1:3] == ["diff", "--numstat"]:
            write_text(instance_dir / "diff-numstat.txt", result.stdout)
    patch = (instance_dir / "final_patch.diff").read_text(encoding="utf-8")
    status = (instance_dir / "git-status.txt").read_text(encoding="utf-8")
    return {
        "ok": True,
        "commands": records,
        "patch_bytes": len(patch.encode("utf-8")),
        "patch_lines": len(patch.splitlines()),
        "status_bytes": len(status.encode("utf-8")),
        "has_patch": bool(patch.strip()),
    }


def native_session_manifest(instance_dir: Path) -> dict[str, Any]:
    root = instance_dir / "trajectory"
    files: list[dict[str, Any]] = []
    if root.is_dir():
        for path in sorted(root.rglob("*.jsonl")):
            if path.is_file():
                files.append(
                    {
                        "path": str(path.relative_to(instance_dir)),
                        "bytes": path.stat().st_size,
                        "sha256": sha256_file(path),
                    }
                )
    value = {
        "container_root": TASK_LOG_ROOT,
        "host_root": str(root),
        "files": files,
        "telemetry_path": str(instance_dir / "trajectory/ygg-telemetry.jsonl"),
    }
    write_json(instance_dir / "native-session-manifest.json", value)
    return value


def convert_trajectory(
    instance_dir: Path,
    *,
    model: str,
    reasoning: str,
    ygg_source: Path | None,
) -> dict[str, Any]:
    """Use the pinned checkout's conservative Ygg→ATIF converter."""

    try:
        converter_path = (
            (ygg_source / "evaluation/harbor/session.py")
            if ygg_source is not None
            else (REPO_ROOT / "evaluation/harbor/session.py")
        )
        if converter_path.is_file():
            spec = importlib.util.spec_from_file_location(
                "ygg_benchmark_session_converter", converter_path
            )
            if spec is None or spec.loader is None:
                raise ImportError(f"cannot load converter: {converter_path}")
            module = importlib.util.module_from_spec(spec)
            sys.modules[spec.name] = module
            spec.loader.exec_module(module)
            convert_native_sessions = module.convert_native_sessions
        else:
            from evaluation.harbor.session import convert_native_sessions

        conversion = convert_native_sessions(
            instance_dir / "trajectory/sessions",
            agent_name="ygg",
            agent_version=YGG_VERSION,
            model_name=model,
            reasoning=reasoning,
        )
        if conversion is None:
            raise ValueError("no convertible native session")
        write_json(instance_dir / "trajectory.json", conversion.trajectory)
        metrics = conversion.metrics
        return {
            "ok": True,
            "source": str(conversion.source.relative_to(instance_dir)),
            "input_tokens": metrics.input_tokens,
            "cache_tokens": metrics.cache_tokens,
            "output_tokens": metrics.output_tokens,
            "cost_usd": metrics.cost_usd,
            "turns": metrics.turns,
        }
    except Exception as error:
        write_text(
            instance_dir / "trajectory-conversion-error.txt",
            f"{type(error).__name__}: {error}\n{traceback.format_exc()}",
        )
        return {"ok": False, "error": f"{type(error).__name__}: {error}"}


def run_one(
    *,
    client: Any,
    row: dict[str, Any],
    instance_dir: Path,
    binary: Path,
    binary_sha256: str,
    credential_dir: Path,
    model: str,
    reasoning: str,
    timeout_seconds: int,
    image_arch: str,
    image_tag: str,
    run_id: str,
    remove_image_after: bool,
    ygg_source: Path | None,
) -> dict[str, Any]:
    instance_id = row["instance_id"]
    prompt = row["problem_statement"]
    prompt_bytes = prompt.encode("utf-8")
    instance_dir.mkdir(parents=True, exist_ok=True)
    (instance_dir / "trajectory").mkdir(parents=True, exist_ok=True)
    (instance_dir / "trajectory").chmod(0o700)
    started_at = now_iso()
    started_monotonic = time.monotonic()
    reference = image_reference(instance_id, image_arch, image_tag)
    metadata: dict[str, Any] = {
        "schema_version": "swebench-live-instance-v1",
        "instance_id": instance_id,
        "repo": row["repo"],
        "base_commit": row["base_commit"],
        "start_timestamp": started_at,
        "prompt_sha256": sha256_bytes(prompt_bytes),
        "prompt_bytes": len(prompt_bytes),
        "image_reference": reference,
        "docker_platform": DOCKER_PLATFORM,
        "image_arch": image_arch,
        "termination_reason": None,
        "resolved": None,
        "trajectory_path": str((instance_dir / "trajectory").relative_to(BENCHMARK_ROOT)),
    }
    write_json(instance_dir / "metadata.json", metadata)
    container = None
    monitor: ResourceMonitor | None = None
    agent_result: ExecResult | None = None
    setup_ok = False
    process_kind = "environment_error"
    try:
        image_started = time.monotonic()
        identity = image_identity(client, reference)
        metadata["image"] = identity
        metadata["image_pull_seconds"] = round(time.monotonic() - image_started, 3)
        container_name = f"ygg-swebench-{safe_name(instance_id)}-{safe_name(run_id)}-{uuid.uuid4().hex[:8]}"
        container = client.containers.create(
            image=reference,
            name=container_name,
            user="0",
            detach=True,
            command=["tail", "-f", "/dev/null"],
            platform=DOCKER_PLATFORM,
            volumes={
                str((instance_dir / "trajectory").resolve()): {
                    "bind": TASK_LOG_ROOT,
                    "mode": "rw",
                }
            },
        )
        container.start()
        metadata["container_id"] = container.id
        write_json(instance_dir / "metadata.json", metadata)
        monitor = ResourceMonitor(container)
        monitor.start()

        credential_names = copy_credentials(container, credential_dir)
        metadata["credential_files_copied"] = credential_names
        binary_identity = copy_binary(container, binary, binary_sha256)
        metadata["container_binary"] = binary_identity
        setup_started = time.monotonic()
        setup_ok, setup = prepare_repository(container, row["base_commit"])
        metadata["repository_setup"] = setup
        metadata["setup_seconds"] = round(time.monotonic() - setup_started, 3)
        if not setup_ok:
            process_kind = "environment_error"
        else:
            ygg_argv = [
                "/usr/local/bin/ygg",
                "--print",
                "--model",
                model,
                "--reasoning",
                reasoning,
                "--session-dir",
                f"{TASK_LOG_ROOT}/sessions",
                "--workspace-trusted",
                "--telemetry",
                f"{TASK_LOG_ROOT}/ygg-telemetry.jsonl",
                "--",
                prompt,
            ]
            write_json(
                instance_dir / "agent-command.json",
                {
                    "argv_without_prompt": ygg_argv[:-1],
                    "prompt_sha256": metadata["prompt_sha256"],
                    "prompt_bytes": metadata["prompt_bytes"],
                    "model": model,
                    "reasoning": reasoning,
                    "session_dir": f"{TASK_LOG_ROOT}/sessions",
                    "telemetry_path": f"{TASK_LOG_ROOT}/ygg-telemetry.jsonl",
                    "workspace": TASK_WORKSPACE,
                    "workspace_trusted": True,
                    "tools": "default built-in surface",
                },
            )
            agent_started = time.monotonic()

            def kill_ygg() -> None:
                # Keep the container alive long enough to capture the final diff.
                # Ygg's own cancellation handles registered descendants; this is
                # only the outer deadline fallback.
                for signal in ("-TERM", "-KILL"):
                    result = exec_stream(
                        container,
                        ["pkill", signal, "-x", "ygg"],
                        timeout_seconds=5,
                    )
                    if result.return_code == 0:
                        if signal == "-TERM":
                            time.sleep(1)
                        else:
                            break

            agent_result = exec_stream(
                container,
                ygg_argv,
                workdir=TASK_WORKSPACE,
                environment={"HOME": "/root", "PATH": "/usr/local/bin:/usr/bin:/bin"},
                timeout_seconds=timeout_seconds,
                on_timeout=kill_ygg,
            )
            metadata["agent_seconds"] = round(time.monotonic() - agent_started, 3)
            process_kind = process_failure_kind(agent_result)
            write_text(instance_dir / "stdout.txt", agent_result.stdout)
            write_text(instance_dir / "stderr.txt", agent_result.stderr)
            write_json(instance_dir / "agent-exec.json", compact_result(agent_result))
            capture = capture_patch(container, instance_dir)
            metadata["patch_capture"] = capture
            if agent_result.timed_out:
                metadata["termination_reason"] = "benchmark_timeout"
            elif setup_ok:
                metadata["termination_reason"] = process_kind
    except ImageNotFound as error:
        metadata["termination_reason"] = "environment_error"
        metadata["error"] = f"image_not_found: {error}"
        write_text(instance_dir / "runner-error.txt", metadata["error"] + "\n")
    except Exception as error:
        metadata["termination_reason"] = "environment_error"
        metadata["error"] = f"{type(error).__name__}: {error}"
        write_text(instance_dir / "runner-error.txt", metadata["error"] + "\n" + traceback.format_exc())
    finally:
        if monitor is not None:
            monitor.stop()
            metadata["memory"] = {
                "peak_ygg_rss_kib": monitor.peak_ygg_rss_kib,
                "peak_process_tree_rss_kib": monitor.peak_process_tree_rss_kib,
                "peak_container_memory_bytes": monitor.peak_container_memory_bytes,
                "samples": monitor.samples,
                "monitor_errors": monitor.errors,
                "scope": "task container; remote model/provider server excluded",
            }
        if container is not None:
            try:
                container.reload()
                metadata["container_exit_status"] = container.attrs.get("State", {}).get("ExitCode")
            except Exception:
                pass
            stop_container(container)
            try:
                container.remove(force=True)
                metadata["container_removed"] = True
            except Exception as error:
                metadata["container_removed"] = False
                metadata["container_remove_error"] = f"{type(error).__name__}: {error}"
        if remove_image_after and metadata.get("image"):
            try:
                client.images.remove(reference, force=True)
                metadata["image_removed"] = True
            except Exception as error:
                metadata["image_removed"] = False
                metadata["image_remove_error"] = f"{type(error).__name__}: {error}"
        metadata["native_session_manifest"] = native_session_manifest(instance_dir)
        metadata["trajectory_conversion"] = convert_trajectory(
            instance_dir, model=model, reasoning=reasoning, ygg_source=ygg_source
        )
        metadata["finish_timestamp"] = now_iso()
        metadata["wall_seconds"] = round(time.monotonic() - started_monotonic, 3)
        metadata["process_kind"] = process_kind
        if metadata.get("termination_reason") is None:
            metadata["termination_reason"] = process_kind
        write_json(instance_dir / "metadata.json", metadata)
        write_json(instance_dir / "telemetry.json", metadata)
    patch_path = instance_dir / "final_patch.diff"
    patch = patch_path.read_text(encoding="utf-8") if patch_path.is_file() else ""
    return {
        "instance_id": instance_id,
        "repo": row["repo"],
        "base_commit": row["base_commit"],
        "termination_reason": metadata.get("termination_reason"),
        "process_kind": metadata.get("process_kind"),
        "patch_bytes": len(patch.encode("utf-8")),
        "patch_lines": len(patch.splitlines()),
        "has_patch": bool(patch.strip()),
        "wall_seconds": metadata.get("wall_seconds"),
        "instance_dir": str(instance_dir),
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
    parser.add_argument("--ygg-source", type=Path, help="source checkout used for prompt provenance")
    parser.add_argument("--workers", type=int, default=1)
    parser.add_argument(
        "--keep-images",
        action="store_true",
        help="retain task images; the baseline leaves this off to bound Docker disk usage",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.workers != 1:
        raise SystemExit("the frozen baseline requires --workers 1; use separate runs for concurrency studies")
    if args.timeout_seconds < 1:
        raise SystemExit("--timeout-seconds must be positive")
    parquet = ensure_dataset(args.parquet.resolve())
    rows = load_rows(parquet)
    by_id = rows_by_id(rows)
    selected_ids = load_selection(args.selection.resolve())
    if any(instance_id not in by_id for instance_id in selected_ids):
        missing = sorted(set(selected_ids) - set(by_id))
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
    output_dir.mkdir(parents=True, exist_ok=True)
    binary_sha256 = sha256_file(binary)
    start = now_iso()
    config = {
        "schema_version": "swebench-live-ygg-run-v1",
        "phase": args.run_id,
        "run_id": args.run_id,
        "start_timestamp": start,
        "dataset": {
            "repository": "SWE-bench-Live/SWE-bench-Live",
            "revision": "a637bd46829f3132e12938c8a0ca93173a977b8e",
            "parquet_sha256": sha256_file(parquet),
            "split": "lite",
            "nominal_count": 300,
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
            "binary_sha256": binary_sha256,
            "target": YGG_BINARY_TARGET,
            "source_checkout": str(args.ygg_source.resolve()) if args.ygg_source else None,
        },
        "model": args.model,
        "provider": "Codex OAuth credential copied into disposable task container",
        "reasoning": args.reasoning,
        "k": 1,
        "workers": args.workers,
        "timeout_seconds": args.timeout_seconds,
        "timeout_grace_seconds": TIMEOUT_GRACE_SECONDS,
        "remove_images_after_task": not args.keep_images,
        "image_namespace": IMAGE_NAMESPACE,
        "image_arch": args.image_arch,
        "image_tag": args.image_tag,
        "docker_platform": DOCKER_PLATFORM,
        "task_workspace": TASK_WORKSPACE,
        "agent_prompt": "exact pinned problem_statement field; no hints/gold/evaluator fields; raw prompt retained only in native trajectory",
        "system_prompt": system_prompt_identity(args.ygg_source.resolve() if args.ygg_source else None),
        "tool_schema": tool_schema_identity(),
        "credentials": {
            "source": str(credential_dir),
            "files": ["codex.json", "codex-models.json"],
            "values": "redacted/not recorded",
            "copied_per_task": True,
            "returned_to_host": False,
        },
        "extensions": "none mounted or enabled",
        "status": "running",
    }
    write_json(output_dir / "manifest.json", config)
    client = docker.from_env(timeout=60)
    summaries: list[dict[str, Any]] = []
    prediction_path = output_dir / "predictions.jsonl"
    for index, instance_id in enumerate(selected_ids, start=1):
        row = by_id[instance_id]
        print(f"[{index}/{len(selected_ids)}] {instance_id}", flush=True)
        try:
            summary = run_one(
                client=client,
                row=row,
                instance_dir=output_dir / "instances" / safe_name(instance_id),
                binary=binary,
                binary_sha256=binary_sha256,
                credential_dir=credential_dir,
                model=args.model,
                reasoning=args.reasoning,
                timeout_seconds=args.timeout_seconds,
                image_arch=args.image_arch,
                image_tag=args.image_tag,
                run_id=args.run_id,
                remove_image_after=not args.keep_images,
                ygg_source=args.ygg_source.resolve() if args.ygg_source else None,
            )
        except Exception as error:  # keep a single task failure from hiding the campaign record
            task_dir = output_dir / "instances" / safe_name(instance_id)
            task_dir.mkdir(parents=True, exist_ok=True)
            write_text(task_dir / "runner-error.txt", f"{type(error).__name__}: {error}\n{traceback.format_exc()}")
            summary = {
                "instance_id": instance_id,
                "repo": row["repo"],
                "base_commit": row["base_commit"],
                "termination_reason": "harness_error",
                "process_kind": "harness_error",
                "patch_bytes": 0,
                "patch_lines": 0,
                "has_patch": False,
                "wall_seconds": None,
                "instance_dir": str(task_dir),
            }
        summaries.append(summary)
        patch_path = Path(summary["instance_dir"]) / "final_patch.diff"
        patch = patch_path.read_text(encoding="utf-8") if patch_path.is_file() else ""
        prediction = {
            "instance_id": instance_id,
            "model_name_or_path": f"ygg-{args.model}-{args.reasoning}",
            "model_patch": patch,
        }
        with prediction_path.open("a", encoding="utf-8") as output:
            output.write(json.dumps(prediction, ensure_ascii=False) + "\n")
        write_json(output_dir / "progress.json", {"completed": index, "total": len(selected_ids), "last": summary})
    aggregate = {
        "schema_version": "swebench-live-run-summary-v1",
        "run_id": args.run_id,
        "start_timestamp": start,
        "finish_timestamp": now_iso(),
        "nominal_selection_count": len(selected_ids),
        "prediction_count": len(summaries),
        "summaries": summaries,
    }
    write_json(output_dir / "run-summary.json", aggregate)
    config["status"] = "complete"
    config["finish_timestamp"] = aggregate["finish_timestamp"]
    config["prediction_path"] = str(prediction_path)
    write_json(output_dir / "manifest.json", config)
    print(f"completed {len(summaries)} tasks; predictions: {prediction_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
