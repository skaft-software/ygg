"""A minimal Harbor ``BaseAgent`` adapter for the Ygg headless CLI."""

from __future__ import annotations

from math import ceil
from pathlib import Path
import shlex
from typing import Any

from harbor.agents.base import BaseAgent
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

from .command import FailureKind, build_ygg_command, classify_failure, parse_ygg_version
from .config import (
    DEFAULT_BINARY_PATH,
    DEFAULT_BINARY_SOURCE,
    DEFAULT_REASONING,
    DEFAULT_SESSION_DIR,
    PINNED_YGG_VERSION,
)


class YggSetupError(RuntimeError):
    """The pinned Ygg executable could not be installed or verified."""


class YggAgentError(RuntimeError):
    """Ygg exited before the Harbor verifier could determine task success."""


class Ygg(BaseAgent):
    """Run one pinned Ygg binary in a Harbor task workspace.

    The task image is expected to contain the binary produced by the pinned
    build recipe. ``setup`` copies it to a task-local executable path so the
    run does not depend on a mutable host installation.
    """

    SUPPORTS_ATIF = False
    SUPPORTS_WINDOWS = False

    def __init__(
        self,
        logs_dir: Path,
        model_name: str | None = None,
        *,
        ygg_binary: str = DEFAULT_BINARY_SOURCE,
        ygg_binary_sha256: str | None = None,
        reasoning: str | None = DEFAULT_REASONING,
        session_dir: str = DEFAULT_SESSION_DIR,
        max_turns: int | None = None,
        workspace_trusted: bool = True,
        agent_timeout_sec: float | None = None,
        extra_env: dict[str, str] | None = None,
        **kwargs: Any,
    ) -> None:
        super().__init__(
            logs_dir=logs_dir,
            model_name=model_name,
            extra_env=extra_env,
            **kwargs,
        )
        self._source_binary = ygg_binary
        self._binary_sha256 = ygg_binary_sha256
        self._reasoning = reasoning
        self._session_dir = session_dir
        self._max_turns = max_turns
        self._workspace_trusted = workspace_trusted
        self._agent_timeout_sec = (
            max(1, ceil(agent_timeout_sec)) if agent_timeout_sec else None
        )
        self._binary = DEFAULT_BINARY_PATH
        self._installed_version: str | None = None

    @staticmethod
    def name() -> str:
        """Return Harbor's stable adapter name."""

        return "ygg"

    def version(self) -> str:
        """Return the pinned version before setup or the verified version after it."""

        return self._installed_version or PINNED_YGG_VERSION

    @property
    def binary(self) -> str:
        """Return the task-local binary path used by ``run``."""

        return self._binary

    def _expected_hash_check(self) -> str:
        if self._binary_sha256 is None:
            return ""
        digest = self._binary_sha256.strip().lower()
        if len(digest) != 64 or any(character not in "0123456789abcdef" for character in digest):
            raise ValueError("ygg_binary_sha256 must be a 64-character hexadecimal digest")
        return f"printf '%s  %s\\n' {shlex.quote(digest)} {shlex.quote(self._source_binary)} | sha256sum -c -"

    async def setup(self, environment: BaseEnvironment) -> None:
        """Copy and verify the pinned binary in the task environment."""

        hash_check = self._expected_hash_check()
        checks = [
            "set -eu",
            f"test -x {shlex.quote(self._source_binary)}",
        ]
        if hash_check:
            checks.append(hash_check)
        checks.extend(
            [
                f"install -m 0755 {shlex.quote(self._source_binary)} {shlex.quote(self._binary)}",
                f"{shlex.quote(self._binary)} --version",
            ]
        )
        result = await environment.exec(command=" && ".join(checks))
        if result.return_code != 0:
            raise YggSetupError(
                f"Ygg setup failed with exit {result.return_code}; "
                "inspect the Harbor agent logs for setup output"
            )

        version = parse_ygg_version(result.stdout or "")
        if version != PINNED_YGG_VERSION:
            raise YggSetupError(
                f"Ygg version mismatch: expected {PINNED_YGG_VERSION}, "
                f"got {version or 'unknown'}"
            )
        self._installed_version = version

    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        """Run Ygg in print mode; Harbor's verifier remains authoritative."""

        command = build_ygg_command(
            self._binary,
            instruction,
            model=self.model_name,
            reasoning=self._reasoning,
            session_dir=self._session_dir,
            max_turns=self._max_turns,
            workspace_trusted=self._workspace_trusted,
        )
        result = await environment.exec(
            command=command.shell,
            env=self.extra_env or None,
            timeout_sec=self._agent_timeout_sec,
        )
        failure = classify_failure(result.return_code, result.stdout, result.stderr)
        if failure is not FailureKind.SUCCESS:
            raise YggAgentError(
                f"Ygg {failure.value} with exit {result.return_code}; "
                "inspect the Harbor agent logs for complete output"
            )


__all__ = ["Ygg", "YggAgentError", "YggSetupError"]
