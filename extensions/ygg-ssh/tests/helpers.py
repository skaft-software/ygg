from __future__ import annotations

import json
from pathlib import Path
from typing import Any, Optional

from ygg_ssh.config import SshConfig, Target
from ygg_ssh.session import ProbeResult, SshSessions


ROOT = Path(__file__).resolve().parents[1]
FAKE_SSH = ROOT / "fixtures" / "fake_ssh"

CONTEXT = {
    "workspace": "/fixture/workspace",
    "resource_owner": {
        "session_id": "fixture-session",
        "extension_instance_id": "fixture-instance",
        "process_generation": 7,
    },
    "host": {"session_id": "fixture-session"},
}


def write_json(path: Path, value: Any) -> Path:
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")
    path.chmod(0o600)
    return path


def config_document(*, enabled: bool = True) -> dict[str, Any]:
    return {
        "version": 1,
        "targets": {
            "fixture": {
                "alias": "fixture-alias",
                "label": "Fixture remote",
                "cwd": "/srv/fixture",
                "enabled": enabled,
            }
        },
    }


def fixture_config(*, enabled: bool = True) -> SshConfig:
    return SshConfig(
        targets=(
            Target(
                id="fixture",
                alias="fixture-alias",
                label="Fixture remote",
                cwd="/srv/fixture",
                enabled=enabled,
            ),
        )
    )


class SessionsHarness:
    """Builds :class:`SshSessions` with a scripted probe and captured calls."""

    def __init__(self, *, probe_exit: int = 0, enabled: bool = True) -> None:
        self.config = fixture_config(enabled=enabled)
        self.probe_calls: list[str] = []
        self._probe_exit = probe_exit

        def prober(target: Target) -> ProbeResult:
            self.probe_calls.append(target.alias)
            return ProbeResult(ok=self._probe_exit == 0, exit_status=self._probe_exit, duration_ms=1)

        self.sessions = SshSessions(self.config, prober=prober)

    def connect(self, target_id: str = "fixture", context: Optional[dict] = CONTEXT) -> str:
        assert context is not None
        result = self.sessions.execute_command(["connect", target_id], dict(context))
        return str(result["text"])
