#!/usr/bin/env python3
"""Run a small active-agent RSS shootout on one Terminal-Bench 2.1 task."""

from __future__ import annotations

import hashlib
import json
import os
import platform
import shutil
import signal
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

MODEL = "openrouter/z-ai/glm-5.3-flash"
MODEL_BARE = "z-ai/glm-5.3-flash"
YGG_MODEL = "custom/openrouter-chat/z-ai/glm-5.3-flash"
TASK = "terminal-bench/openssl-selfsigned-cert"
REPETITIONS = 3
TIMEOUT_SECONDS = 300
SAMPLE_INTERVAL_SECONDS = 0.1

TASK_INSTRUCTION = """Your company needs a self-signed TLS certificate for an internal development server. Complete this Terminal-Bench 2.1 task in the current workspace, which is mounted as `/app` for verification.

1. Create `ssl/` to store all certificate files.
2. Generate a 2048-bit RSA private key at `ssl/server.key` with permissions 600.
3. Create a self-signed certificate valid for exactly 365 days with Organization Name `DevOps Team` and Common Name `dev-internal.company.local`; save it as `ssl/server.crt`.
4. Create `ssl/server.pem` containing both the private key and certificate.
5. Create `ssl/verification.txt` containing the certificate subject, validity dates, and SHA-256 fingerprint.
6. Create `check_cert.py` using only the Python standard library and OpenSSL subprocesses. It must load and verify the certificate, print its Common Name and expiration date in YYYY-MM-DD format, and print `Certificate verification successful` when all checks pass. The verifier environment does not install third-party Python packages.

Use OpenSSL commands, ensure all files have the correct formats and permissions, and do not merely explain the solution. This is a speed-focused run: do not inspect the empty workspace or plan at length. Immediately create every required artifact, preferably in one shell invocation, run a concise verification, and finish.
"""


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def ps_rows() -> list[tuple[int, int, int]]:
    result = subprocess.run(
        ["ps", "-axo", "pid=,ppid=,rss="],
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        check=False,
    )
    rows: list[tuple[int, int, int]] = []
    for line in result.stdout.splitlines():
        parts = line.split()
        if len(parts) != 3:
            continue
        try:
            rows.append(tuple(map(int, parts)))
        except ValueError:
            continue
    return rows


def rss_for_tree(root_pid: int) -> tuple[int | None, int]:
    rows = ps_rows()
    by_pid = {pid: rss for pid, _ppid, rss in rows}
    children: dict[int, list[int]] = {}
    for pid, ppid, _rss in rows:
        children.setdefault(ppid, []).append(pid)
    tree = {root_pid}
    pending = [root_pid]
    while pending:
        parent = pending.pop()
        for child in children.get(parent, []):
            if child not in tree:
                tree.add(child)
                pending.append(child)
    return by_pid.get(root_pid), sum(by_pid.get(pid, 0) for pid in tree)


def command_for(harness: str, workspace: Path, run_root: Path) -> list[str]:
    if harness == "ygg":
        return [
            shutil.which("ygg") or "ygg",
            "--print",
            "--model",
            YGG_MODEL,
            "--offline",
            "--session-dir",
            str(run_root / "sessions"),
            "--no-context-files",
            "--tools=bash",
            TASK_INSTRUCTION,
        ]
    if harness == "codex":
        return [
            shutil.which("codex") or "codex",
            "exec",
            "--ignore-rules",
            "--ephemeral",
            "--approve-for-me",
            "--cd",
            str(workspace),
            "--profile",
            "ox",
            "--model",
            MODEL_BARE,
            "--config",
            'model_reasoning_effort="medium"',
            TASK_INSTRUCTION,
        ]
    if harness == "pi":
        return [
            shutil.which("pi") or "pi",
            "--print",
            "--provider",
            "openrouter",
            "--model",
            MODEL_BARE,
            "--thinking",
            "medium",
            "--no-session",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--no-context-files",
            "--tools",
            "bash",
            "--",
            TASK_INSTRUCTION,
        ]
    if harness == "opencode":
        return [
            shutil.which("opencode") or "opencode",
            "run",
            "--pure",
            "--auto",
            "--model",
            MODEL,
            "--variant",
            "medium",
            "--dir",
            str(workspace),
            "--format",
            "json",
            TASK_INSTRUCTION,
        ]
    raise ValueError(harness)


def clean_env(run_root: Path, api_key: str) -> dict[str, str]:
    home = run_root / "home"
    home.mkdir(parents=True, exist_ok=True)
    (home / ".codex").mkdir(parents=True, exist_ok=True)
    (home / ".pi/agent").mkdir(parents=True, exist_ok=True)
    ygg_credentials = home / ".ygg/credentials"
    ygg_credentials.mkdir(parents=True, exist_ok=True)
    custom_provider = ygg_credentials / "custom.json"
    custom_provider.write_text(
        json.dumps(
            {
                "version": 1,
                "providers": {
                    "openrouter-chat": {
                        "label": "OpenRouter Chat",
                        "base_url": "https://openrouter.ai/api/v1/",
                        "auth": {"kind": "bearer_env", "var": "OPENROUTER_API_KEY"},
                        "auto_discover": False,
                        "models": [
                            {
                                "api_name": MODEL_BARE,
                                "display_name": "GLM-5.3 Flash",
                                "context_window": 1_310_720,
                                "max_output_tokens": 16_384,
                                "tools": True,
                                "parallel_tool_calls": False,
                                "vision": False,
                                "structured_output": False,
                                "reasoning": True,
                                "reasoning_configurable": False,
                            }
                        ],
                    }
                },
            },
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )
    os.chmod(custom_provider, 0o600)
    env = {
        "HOME": str(home),
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "LANG": os.environ.get("LANG", "en_US.UTF-8"),
        "LC_ALL": os.environ.get("LC_ALL", "en_US.UTF-8"),
        "TERM": "dumb",
        "NO_COLOR": "1",
        "OPENROUTER_API_KEY": api_key,
        "XDG_CONFIG_HOME": str(home / ".config"),
        "XDG_CACHE_HOME": str(home / ".cache"),
        "XDG_DATA_HOME": str(home / ".local/share"),
        "CODEX_HOME": str(home / ".codex"),
        "PI_CODING_AGENT_DIR": str(home / ".pi/agent"),
        "PI_TELEMETRY": "0",
    }
    for key in ("SSL_CERT_FILE", "SSL_CERT_DIR", "HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY", "NO_PROXY"):
        if value := os.environ.get(key):
            env[key] = value
    return env


def redact_file(path: Path, secret: str) -> None:
    if not path.is_file():
        return
    data = path.read_bytes()
    encoded = secret.encode()
    if encoded in data:
        path.write_bytes(data.replace(encoded, b"<REDACTED_OPENROUTER_KEY>"))
    os.chmod(path, 0o600)


def verify(workspace: Path, verifier_image: str, output: Path) -> bool:
    if not (workspace / "ssl/server.crt").is_file():
        output.write_text("ssl/server.crt missing\n", encoding="utf-8")
        return False
    result = subprocess.run(
        [
            "docker",
            "run",
            "--rm",
            "--platform",
            "linux/amd64",
            "--mount",
            f"type=bind,source={workspace.resolve()},target=/app,readonly",
            verifier_image,
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        timeout=120,
        check=False,
    )
    output.write_text(result.stdout, encoding="utf-8")
    return result.returncode == 0


def run_one(
    harness: str,
    repetition: int,
    output_root: Path,
    api_key: str,
    verifier_image: str,
) -> dict[str, Any]:
    run_root = output_root / "runs" / harness / f"trial-{repetition}"
    workspace = run_root / "workspace"
    workspace.mkdir(parents=True)
    os.chmod(run_root, 0o700)
    (workspace / "README.md").write_text(
        "# Terminal-Bench 2.1 openssl-selfsigned-cert task\n\n" + TASK_INSTRUCTION,
        encoding="utf-8",
    )
    subprocess.run(["git", "init", "-q"], cwd=workspace, check=True)
    subprocess.run(["git", "add", "README.md"], cwd=workspace, check=True)
    subprocess.run(
        [
            "git",
            "-c",
            "user.name=Benchmark",
            "-c",
            "user.email=benchmark@localhost",
            "commit",
            "-qm",
            "initial task",
        ],
        cwd=workspace,
        check=True,
    )

    stdout_path = run_root / "stdout.txt"
    stderr_path = run_root / "stderr.txt"
    verifier_path = run_root / "verifier.txt"
    argv = command_for(harness, workspace, run_root)
    env = clean_env(run_root, api_key)
    if harness == "codex":
        env["CODEX_HOME"] = str(Path.home() / ".codex")
    started = datetime.now(timezone.utc)
    begin = time.monotonic()
    timed_out = False
    peak_root = 0
    peak_tree = 0
    samples = 0
    with stdout_path.open("wb") as stdout, stderr_path.open("wb") as stderr:
        process = subprocess.Popen(
            argv,
            cwd=workspace,
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=stdout,
            stderr=stderr,
            start_new_session=True,
        )
        deadline = begin + TIMEOUT_SECONDS
        while process.poll() is None:
            root_rss, tree_rss = rss_for_tree(process.pid)
            if root_rss is not None:
                peak_root = max(peak_root, root_rss)
            peak_tree = max(peak_tree, tree_rss)
            samples += 1
            if time.monotonic() >= deadline:
                timed_out = True
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                break
            time.sleep(SAMPLE_INTERVAL_SECONDS)
        return_code = process.wait()
    wall_seconds = time.monotonic() - begin
    quality_pass = verify(workspace, verifier_image, verifier_path)
    for path in (stdout_path, stderr_path, verifier_path):
        redact_file(path, api_key)
    certificate = workspace / "ssl/server.crt"
    return {
        "harness": harness,
        "trial": repetition,
        "started_at": started.isoformat(),
        "wall_seconds": round(wall_seconds, 3),
        "return_code": return_code,
        "timed_out": timed_out,
        "samples": samples,
        "sample_interval_seconds": SAMPLE_INTERVAL_SECONDS,
        "peak_root_rss_kib": peak_root or None,
        "peak_process_tree_rss_kib": peak_tree or None,
        "quality_pass": quality_pass,
        "certificate_bytes": certificate.stat().st_size if certificate.is_file() else None,
        "certificate_sha256": sha256(certificate) if certificate.is_file() else None,
        "argv": argv[:-1] + ["<TASK_INSTRUCTION>"],
        "artifact_dir": str(run_root.relative_to(output_root)),
    }


def summarize(records: list[dict[str, Any]]) -> dict[str, Any]:
    import statistics

    summary: dict[str, Any] = {}
    for harness in ("ygg", "codex", "pi", "opencode"):
        rows = [row for row in records if row["harness"] == harness]
        walls = [row["wall_seconds"] for row in rows]
        roots = [row["peak_root_rss_kib"] for row in rows if row["peak_root_rss_kib"] is not None]
        trees = [row["peak_process_tree_rss_kib"] for row in rows if row["peak_process_tree_rss_kib"] is not None]
        summary[harness] = {
            "trials": len(rows),
            "quality_passes": sum(bool(row["quality_pass"]) for row in rows),
            "median_wall_seconds": statistics.median(walls) if walls else None,
            "median_peak_root_rss_mib": statistics.median(roots) / 1024 if roots else None,
            "max_peak_root_rss_mib": max(roots) / 1024 if roots else None,
            "median_peak_process_tree_rss_mib": statistics.median(trees) / 1024 if trees else None,
            "max_peak_process_tree_rss_mib": max(trees) / 1024 if trees else None,
        }
    return summary


def main() -> int:
    api_key = os.environ.get("OPENROUTER_API_KEY")
    if not api_key:
        raise SystemExit("OPENROUTER_API_KEY is required")
    output_root = Path(__file__).resolve().parent
    os.chmod(output_root, 0o700)
    verifier_image = "ygg-glm-shootout-openssl-verifier:2026-08-30"
    versions = {}
    for harness in ("ygg", "codex", "pi", "opencode"):
        exe = shutil.which(harness)
        if not exe:
            raise SystemExit(f"missing executable: {harness}")
        result = subprocess.run([exe, "--version"], capture_output=True, text=True, check=False)
        versions[harness] = {
            "path": exe,
            "version": (result.stdout or result.stderr).strip(),
            "sha256": sha256(Path(exe).resolve()),
        }

    records: list[dict[str, Any]] = []
    # Rotate the order across repetitions rather than giving one harness every warm/cold slot.
    orders = [
        ["ygg", "codex", "pi", "opencode"],
        ["codex", "pi", "opencode", "ygg"],
        ["pi", "opencode", "ygg", "codex"],
    ]
    result_path = output_root / "results.json"
    for repetition, order in enumerate(orders, 1):
        for harness in order:
            print(f"running {harness} trial {repetition}", flush=True)
            record = run_one(harness, repetition, output_root, api_key, verifier_image)
            records.append(record)
            payload = {
                "schema": "ygg.direct-shootout.v1",
                "status": "running",
                "generated_at": datetime.now(timezone.utc).isoformat(),
                "model": MODEL,
                "task": TASK,
                "repetitions": REPETITIONS,
                "host": {
                    "platform": platform.platform(),
                    "machine": platform.machine(),
                    "python": platform.python_version(),
                },
                "versions": versions,
                "measurement": {
                    "rss_source": "macOS ps rss, KiB",
                    "root": "directly launched CLI process",
                    "process_tree": "direct process plus all descendants observed in the same sample",
                    "sample_interval_seconds": SAMPLE_INTERVAL_SECONDS,
                    "remote_inference_server_excluded": True,
                    "codex_route": "existing ox profile with its model overridden to z-ai/glm-5.3-flash; direct OpenRouter Responses API",
                },
                "records": records,
                "summary": summarize(records),
            }
            result_path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    payload["status"] = "complete"
    payload["generated_at"] = datetime.now(timezone.utc).isoformat()
    result_path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    os.chmod(result_path, 0o600)
    print(json.dumps(payload["summary"], indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
