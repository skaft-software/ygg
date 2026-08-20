from __future__ import annotations

import json
import os
from pathlib import Path
import tempfile
from typing import Any, Optional

from ygg_ssh.config import Limits, SshConfig, Target
from ygg_ssh.manager import OwnerFence, SshManager
from ygg_ssh.process import OpenSshBackend


ROOT = Path(__file__).resolve().parents[1]
FAKE_SSH = ROOT / "fixtures" / "fake_ssh.py"
OWNER = {
    "session_id": "fixture-session",
    "extension_instance_id": "fixture-instance",
    "process_generation": 7,
}
CONTEXT = {"resource_owner": OWNER, "host": {"session_id": "fixture-session"}}
OWNER_FENCE = OwnerFence.from_context(CONTEXT)


def write_json(path: Path, value: Any) -> Path:
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")
    path.chmod(0o600)
    return path


def config_document(remote: Path, *, authority: str = "read-only", enabled: bool = True) -> dict[str, Any]:
    return {
        "version": 1,
        "limits": {
            "connectTimeoutMs": 2000,
            "operationTimeoutMs": 2000,
            "healthIntervalMs": 60000,
            "terminationGraceMs": 100,
            "maxOutputBytes": 4096,
            "maxFileBytes": 4096,
        },
        "targets": {
            "fixture": {
                "alias": "fixture-alias",
                "label": "Fixture remote",
                "remoteCwd": str(remote),
                "authority": authority,
                "enabled": enabled,
            }
        },
        "trustedProjects": [],
    }


class ManagerHarness:
    def __init__(
        self,
        *,
        authority: str = "read-write",
        confirm: bool = True,
        agent: bool = True,
        health_interval_ms: int = 60000,
        environment: Optional[dict[str, str]] = None,
    ) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.remote = self.root / "remote"
        self.remote.mkdir()
        self.log = self.root / "fake.log"
        env = dict(os.environ) if environment is None else dict(environment)
        env["YGG_SSH_FAKE_LOG"] = str(self.log)
        if agent:
            env["SSH_AUTH_SOCK"] = str(self.root / "agent.sock")
        else:
            env.pop("SSH_AUTH_SOCK", None)
        self.environment = env
        self.limits = Limits(
            connect_timeout_ms=2000,
            operation_timeout_ms=2000,
            max_output_bytes=4096,
            max_file_bytes=4096,
            health_interval_ms=health_interval_ms,
            shutdown_timeout_ms=500,
            termination_grace_ms=100,
        )
        self.config = SshConfig(
            targets=(
                Target(
                    id="fixture",
                    alias="fixture-alias",
                    label="Fixture remote",
                    remote_cwd=str(self.remote),
                    authority=authority,
                ),
            ),
            limits=self.limits,
        )
        self.backend = OpenSshBackend(
            self.limits,
            ssh_binary=FAKE_SSH,
            runtime_directory=self.root / "control",
            environment=env,
        )
        self.approvals: list[tuple[str, str, bool]] = []

        def approval(prompt: str, detail: str, destructive: bool) -> bool:
            self.approvals.append((prompt, detail, destructive))
            return confirm

        self.snapshots: list[dict[str, Any]] = []
        self.manager = SshManager(
            self.config,
            self.backend,
            confirm=approval,
            publisher=lambda value, _owner: self.snapshots.append(dict(value)),
        )

    def connect(self, context: dict[str, Any] = CONTEXT) -> None:
        self.manager.request_action("connect", "fixture", context)

    def events(self) -> list[dict[str, Any]]:
        if not self.log.exists():
            return []
        return [json.loads(line) for line in self.log.read_text(encoding="utf-8").splitlines()]

    def close(self) -> None:
        self.manager.shutdown()
        self.temp.cleanup()

    def __enter__(self) -> "ManagerHarness":
        return self

    def __exit__(self, *args: object) -> None:
        self.close()
