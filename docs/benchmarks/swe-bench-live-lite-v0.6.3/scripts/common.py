"""Shared, deliberately boring helpers for the SWE-bench-Live Lite run.

This module keeps the privileged dataset fields on the host side.  The agent
runner imports rows only to select the issue text; it never mounts this module,
the parquet file, or the generated full evaluator dataset into a task
container.
"""

from __future__ import annotations

import hashlib
import json
import os
import stat
import urllib.request
from datetime import date, datetime
from pathlib import Path
from typing import Any, Iterable

ROOT = Path(__file__).resolve().parents[1]
DATASET_REPOSITORY = "SWE-bench-Live/SWE-bench-Live"
DATASET_REVISION = "a637bd46829f3132e12938c8a0ca93173a977b8e"
DATASET_PARQUET = "data/lite-00000-of-00001.parquet"
DATASET_URL = (
    "https://huggingface.co/datasets/"
    f"{DATASET_REPOSITORY}/resolve/{DATASET_REVISION}/{DATASET_PARQUET}"
)
DATASET_SHA256 = "7ee0a75c41bfc954fd441b67ce738fc5c1cbae00721c4e30e7db4d893057c9ab"
DATASET_ROWS = 300
DATASET_COLUMNS = (
    "repo",
    "pull_number",
    "instance_id",
    "issue_numbers",
    "base_commit",
    "patch",
    "test_patch",
    "problem_statement",
    "hints_text",
    "all_hints_text",
    "commit_urls",
    "created_at",
    "commit_url",
    "test_cmds",
    "log_parser",
    "difficulty",
    "FAIL_TO_PASS",
    "PASS_TO_PASS",
)

YGG_REPOSITORY = "https://github.com/skaft-software/ygg.git"
YGG_COMMIT = "cb6be3686181de743905b115442bf090afb822e6"
YGG_VERSION = "0.6.3"
YGG_RELEASE_REF = "mission/v0.6.3-next"
YGG_BINARY_TARGET = "x86_64-unknown-linux-musl"

EVALUATOR_REPOSITORY = "https://github.com/microsoft/SWE-bench-Live.git"
EVALUATOR_REF = "python-only"
EVALUATOR_COMMIT = "ad79b850f15e33992e96f03f6e97f05ddf9aa0be"
EVALUATOR_VERSION = "4.0.3"

IMAGE_NAMESPACE = "starryzhang"
IMAGE_ARCH = "x86_64"
IMAGE_TAG = "latest"
DOCKER_PLATFORM = "linux/amd64"
TASK_WORKSPACE = "/testbed"
TASK_LOG_ROOT = "/logs/agent"

AGENT_VISIBLE_FIELDS = (
    "repo",
    "instance_id",
    "base_commit",
    "problem_statement",
)
PRIVILEGED_FIELDS = (
    "patch",
    "test_patch",
    "FAIL_TO_PASS",
    "PASS_TO_PASS",
    "test_cmds",
    "log_parser",
    "hints_text",
    "all_hints_text",
    "commit_urls",
    "commit_url",
)


def canonical_json(value: Any) -> bytes:
    return (json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False) + "\n").encode("utf-8")


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def json_safe(value: Any) -> Any:
    """Convert Arrow/Python values to deterministic JSON values."""

    if isinstance(value, (datetime, date)):
        return value.isoformat()
    if isinstance(value, dict):
        return {str(key): json_safe(candidate) for key, candidate in value.items()}
    if isinstance(value, (list, tuple)):
        return [json_safe(candidate) for candidate in value]
    if hasattr(value, "as_py"):
        return json_safe(value.as_py())
    return value


def load_rows(parquet_path: Path) -> list[dict[str, Any]]:
    try:
        import pyarrow.parquet as parquet
    except ImportError as error:  # pragma: no cover - exercised by setup failures
        raise SystemExit("pyarrow is required; install scripts/requirements.txt") from error
    table = parquet.read_table(parquet_path)
    rows = [json_safe(row) for row in table.to_pylist()]
    if len(rows) != DATASET_ROWS:
        raise ValueError(f"expected {DATASET_ROWS} Lite rows, found {len(rows)}")
    if tuple(table.column_names) != DATASET_COLUMNS:
        raise ValueError("Lite schema changed; inspect and freeze a new benchmark revision")
    ids = [row.get("instance_id") for row in rows]
    if any(not isinstance(instance_id, str) or not instance_id for instance_id in ids):
        raise ValueError("dataset contains an invalid instance_id")
    if len(set(ids)) != len(ids):
        raise ValueError("dataset contains duplicate instance IDs")
    return rows


def fetch_dataset(path: Path, *, url: str = DATASET_URL) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".partial")
    request = urllib.request.Request(url, headers={"User-Agent": "ygg-swebench-lite-runner/1"})
    with urllib.request.urlopen(request, timeout=60) as response, temporary.open("wb") as output:
        while True:
            chunk = response.read(1024 * 1024)
            if not chunk:
                break
            output.write(chunk)
    digest = sha256_file(temporary)
    if digest != DATASET_SHA256:
        temporary.unlink(missing_ok=True)
        raise ValueError(f"dataset SHA-256 mismatch: expected {DATASET_SHA256}, got {digest}")
    os.replace(temporary, path)


def ensure_dataset(path: Path) -> Path:
    if not path.is_file():
        fetch_dataset(path)
    digest = sha256_file(path)
    if digest != DATASET_SHA256:
        raise ValueError(f"dataset SHA-256 mismatch: expected {DATASET_SHA256}, got {digest}")
    return path


def write_json(path: Path, value: Any, *, private: bool = False) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    encoded = json.dumps(value, indent=2, sort_keys=True, ensure_ascii=False) + "\n"
    temporary = path.with_suffix(path.suffix + ".partial")
    temporary.write_text(encoded, encoding="utf-8")
    if private:
        temporary.chmod(stat.S_IRUSR | stat.S_IWUSR)
    os.replace(temporary, path)
    if private:
        path.chmod(stat.S_IRUSR | stat.S_IWUSR)


def write_jsonl(path: Path, values: Iterable[Any], *, private: bool = False) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".partial")
    with temporary.open("w", encoding="utf-8") as output:
        for value in values:
            output.write(json.dumps(value, sort_keys=True, ensure_ascii=False) + "\n")
    if private:
        temporary.chmod(stat.S_IRUSR | stat.S_IWUSR)
    os.replace(temporary, path)
    if private:
        path.chmod(stat.S_IRUSR | stat.S_IWUSR)


def public_task(row: dict[str, Any]) -> dict[str, Any]:
    return {key: json_safe(row[key]) for key in AGENT_VISIBLE_FIELDS}


def image_repository(instance_id: str, arch: str = IMAGE_ARCH) -> str:
    suffix = instance_id.lower().replace("__", "_1776_")
    return f"{IMAGE_NAMESPACE}/sweb.eval.{arch}.{suffix}"


def image_reference(instance_id: str, arch: str = IMAGE_ARCH, tag: str = IMAGE_TAG) -> str:
    return f"{image_repository(instance_id, arch)}:{tag}"


def load_selection(path: Path) -> list[str]:
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict) or not isinstance(value.get("instances"), list):
        raise ValueError(f"invalid selection manifest: {path}")
    ids: list[str] = []
    for item in value["instances"]:
        if isinstance(item, str):
            instance_id = item
        elif isinstance(item, dict) and isinstance(item.get("instance_id"), str):
            instance_id = item["instance_id"]
        else:
            raise ValueError(f"invalid selection entry in {path}")
        ids.append(instance_id)
    if len(ids) != len(set(ids)):
        raise ValueError(f"duplicate selection entry in {path}")
    return ids


def rows_by_id(rows: list[dict[str, Any]]) -> dict[str, dict[str, Any]]:
    return {row["instance_id"]: row for row in rows}


def system_prompt_identity(ygg_source: Path | None = None) -> dict[str, Any]:
    """Describe the default prompt without pretending Ygg exposes its text.

    The v0.6.3 telemetry contract exposes the user-input identity but not the
    fully composed system prompt.  A null hash is more honest than hashing a
    guessed reconstruction; the source/config identity remains recorded.
    """

    source_hash = None
    if ygg_source is not None:
        resources = ygg_source / "crates/ygg-coding-agent/src/resources.rs"
        if resources.is_file():
            source_hash = sha256_file(resources)
    return {
        "sha256": None,
        "available": False,
        "unavailable_reason": "Ygg v0.6.3 telemetry does not expose the composed system prompt",
        "composition": "built-in default; no --system-prompt override; workspace-trusted context files enabled",
        "resources_rs_sha256": source_hash,
    }


def tool_schema_identity() -> dict[str, Any]:
    descriptor = {
        "surface": "Ygg built-in tools",
        "tools": ["read", "edit", "write", "bash", "search"],
        "policy": "default full-access isolated task container",
        "source_commit": YGG_COMMIT,
    }
    return {
        "sha256": sha256_bytes(canonical_json(descriptor)),
        "version": f"builtin-tools@{YGG_COMMIT}",
        "basis": "canonical enabled-name/policy descriptor; Ygg does not export full JSON schemas",
        "descriptor": descriptor,
    }
