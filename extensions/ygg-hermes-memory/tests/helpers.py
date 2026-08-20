"""Shared deterministic fixtures for ygg-hermes-memory tests."""

from __future__ import annotations

import json
import os
from pathlib import Path
import sys
import tempfile
import time
from typing import Any, Callable, Mapping, Optional

ROOT = Path(__file__).resolve().parents[1]
VENDOR = ROOT / "vendor"
HERMES_ENV = ROOT / "fixtures" / "hermes_environment"
for path in (VENDOR, ROOT, HERMES_ENV):
    if str(path) not in sys.path:
        sys.path.insert(0, str(path))

from ygg_hermes_memory.config import load_config


class FakeExtension:
    def __init__(self) -> None:
        self.tools = {}
        self.catalog_revision = 0
        self.presentations = []
        self.mutations = []

    def register_tools(self, definitions):
        self.catalog_revision += 1
        for definition in definitions:
            self.tools[definition["name"]] = dict(definition)
        self.mutations.append(("register", tuple(item["name"] for item in definitions)))
        return {"revision": self.catalog_revision, "tools": sorted(self.tools)}

    def unregister_tools(self, *names):
        self.catalog_revision += 1
        for name in names:
            self.tools.pop(name, None)
        self.mutations.append(("unregister", tuple(names)))
        return {"revision": self.catalog_revision, "tools": sorted(self.tools)}

    def publish_presentation(self, snapshot, *, resource_owner=None):
        if self.presentations:
            assert snapshot["revision"] > self.presentations[-1]["snapshot"]["revision"]
        self.presentations.append(
            {
                "snapshot": json.loads(json.dumps(snapshot)),
                "resource_owner": (
                    json.loads(json.dumps(resource_owner)) if resource_owner is not None else None
                ),
            }
        )


def owner_context(session: str = "session-a", generation: int = 1) -> Mapping[str, Any]:
    return {
        "workspace": str(ROOT),
        "resource_owner": {
            "session_id": session,
            "extension_instance_id": "instance-test",
            "process_generation": generation,
        },
        "host": {"session_id": session, "model": "test-model", "active_skills": []},
    }


def write_config(
    directory: Path,
    *,
    providers: Optional[list[Mapping[str, Any]]] = None,
    include_entry_points: bool = True,
    trusted: Optional[Mapping[str, str]] = None,
    default_provider: Optional[str] = None,
    limits: Optional[Mapping[str, int]] = None,
    environment_id: str = "fixture-env",
) -> Path:
    home = directory / "hermes-home"
    home.mkdir(parents=True, exist_ok=True)
    value = {
        "version": 1,
        "contract": {
            "hermesVersion": "0.20.1",
            "commit": "7095e23eb2066fe9a2f93b99cdbfe0e2b5ece397",
        },
        "environment": {
            "id": environment_id,
            "python": sys.executable,
            "hermesHome": str(home),
            "includeEntryPoints": include_entry_points,
        },
        "directories": list(providers or []),
        "providerMetadata": {
            "entrypoint:entrypoint-memory": {
                "label": "Offline entry point",
                "network": "none",
                "storage": "none",
                "setup": "configured",
                "readTools": ["entrypoint_recall"],
                "writeTools": [],
            }
        },
        "trustedProviders": dict(trusted or {}),
        "defaultProvider": default_provider,
        "limits": dict(limits or {}),
    }
    path = directory / "config.json"
    path.write_text(json.dumps(value, indent=2), encoding="utf-8")
    os.chmod(path, 0o600)
    return path


def mock_descriptor() -> Mapping[str, Any]:
    return {
        "id": "mock",
        "path": str(ROOT / "fixtures" / "providers" / "mock_provider"),
        "label": "Mock memory",
        "network": "none",
        "storage": "none",
        "setup": "configured",
        "readTools": ["recall_mock"],
        "writeTools": ["remember_mock"],
    }


def offline_descriptor() -> Mapping[str, Any]:
    return {
        "id": "offline",
        "path": str(ROOT / "fixtures" / "providers" / "offline_provider"),
        "label": "Offline recall",
        "network": "none",
        "storage": "local",
        "setup": "configured",
        "readTools": ["recall_offline"],
        "writeTools": ["remember_offline"],
    }


def load_fixture_config(directory: Path, **kwargs):
    return load_config(write_config(directory, **kwargs))


def wait_until(predicate: Callable[[], bool], timeout: float = 2.0) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(0.01)
    return bool(predicate())


class temporary_directory:
    def __enter__(self) -> Path:
        self._value = tempfile.TemporaryDirectory()
        return Path(self._value.name)

    def __exit__(self, *args: Any) -> None:
        self._value.cleanup()
