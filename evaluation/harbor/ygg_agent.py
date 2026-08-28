"""A minimal Harbor ``BaseAgent`` adapter for the Ygg headless CLI."""

from __future__ import annotations

import asyncio
import hashlib
import json
import os
import shlex
import time
from math import ceil
from pathlib import Path, PurePosixPath
from typing import Any

from harbor.agents.base import BaseAgent
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

from .command import (
    FailureKind,
    YggCommand,
    build_ygg_command,
    classify_failure,
    parse_ygg_version,
    terminate_process_group_command,
    wrap_in_process_group,
    wrap_with_timeout,
)
from .config import (
    DEFAULT_BINARY_PATH,
    DEFAULT_BINARY_SOURCE,
    DEFAULT_MODEL,
    DEFAULT_PROVIDER_ENV,
    DEFAULT_REASONING,
    DEFAULT_SESSION_DIR,
    PINNED_YGG_VERSION,
)
from .redaction import redact_jsonl, redact_text
from .session import SessionConversion, convert_native_sessions

_TIMEOUT_GRACE_SECONDS = 15
_PROCESS_GROUP_TERM_GRACE_SECONDS = 5
_PROCESS_GROUP_CLEANUP_TIMEOUT_SECONDS = 10
_SESSION_CONTAINER_ROOT = PurePosixPath("/logs/agent")
_PROCESS_GROUP_FILE = str(_SESSION_CONTAINER_ROOT / "ygg-process-group.pid")


class YggSetupError(RuntimeError):
    """The pinned Ygg executable could not be installed or verified."""


class YggAgentError(RuntimeError):
    """Ygg exited before the Harbor verifier could determine task success."""


class YggProviderError(YggAgentError):
    """Ygg reported a provider, authentication, or model-service failure."""


class YggBenchmarkTimeoutError(YggAgentError):
    """The adapter's process timeout expired before Ygg returned."""


class Ygg(BaseAgent):
    """Run one pinned Ygg binary in a Harbor task workspace.

    The task image is expected to contain the binary produced by the pinned
    build recipe. ``setup`` copies it to a task-local executable path so the
    run does not depend on a mutable host installation.
    """

    SUPPORTS_ATIF = True
    SUPPORTS_WINDOWS = False

    def __init__(
        self,
        logs_dir: Path,
        model_name: str | None = DEFAULT_MODEL,
        *,
        ygg_binary: str = DEFAULT_BINARY_SOURCE,
        ygg_binary_sha256: str | None = None,
        reasoning: str | None = DEFAULT_REASONING,
        session_dir: str = DEFAULT_SESSION_DIR,
        max_turns: int | None = None,
        bash_timeout_secs: int | None = None,
        workspace_trusted: bool = True,
        agent_timeout_sec: float | None = None,
        telemetry: bool = False,
        extra_env: dict[str, str] | None = None,
        **kwargs: Any,
    ) -> None:
        effective_model = model_name or DEFAULT_MODEL
        super().__init__(
            logs_dir=logs_dir,
            model_name=effective_model,
            extra_env=extra_env,
            **kwargs,
        )
        self._source_binary = ygg_binary
        self._binary_sha256 = ygg_binary_sha256
        self._reasoning = reasoning
        self._session_dir = session_dir
        self._session_relative = self._validate_session_dir(session_dir)
        self._max_turns = max_turns
        self._bash_timeout_secs = bash_timeout_secs
        self._workspace_trusted = workspace_trusted
        self._agent_timeout_sec = (
            max(1, ceil(agent_timeout_sec)) if agent_timeout_sec else None
        )
        self._telemetry = telemetry
        self._binary = DEFAULT_BINARY_PATH
        self._installed_version: str | None = None
        self._last_conversion: SessionConversion | None = None

    @staticmethod
    def _validate_session_dir(session_dir: str) -> Path:
        path = PurePosixPath(session_dir)
        try:
            relative = path.relative_to(_SESSION_CONTAINER_ROOT)
        except ValueError as error:
            raise ValueError(
                "session_dir must be under /logs/agent so Harbor retains native sessions"
            ) from error
        if not relative.parts or ".." in relative.parts:
            raise ValueError("session_dir must name a child of /logs/agent")
        return Path(*relative.parts)

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

    @property
    def _host_session_root(self) -> Path:
        return self.logs_dir / self._session_relative

    def _provider_env(self) -> dict[str, str]:
        return {
            name: value
            for name in DEFAULT_PROVIDER_ENV
            if (value := os.environ.get(name))
        }

    def _secrets(self) -> tuple[str, ...]:
        configured = tuple(self.extra_env.values())
        return configured + tuple(self._provider_env().values())

    def _run_env(self) -> dict[str, str]:
        """Forward only the pinned provider credentials plus explicit overrides."""

        return {**self._provider_env(), **self.extra_env}

    def _write_text(self, filename: str, content: str | None) -> None:
        self.logs_dir.mkdir(parents=True, exist_ok=True)
        (self.logs_dir / filename).write_text(
            redact_text(content, self._secrets()), encoding="utf-8"
        )

    def _write_json(self, filename: str, value: dict[str, Any]) -> None:
        self.logs_dir.mkdir(parents=True, exist_ok=True)
        (self.logs_dir / filename).write_text(
            json.dumps(value, indent=2, sort_keys=True, ensure_ascii=False) + "\n",
            encoding="utf-8",
        )

    def _expected_hash_check(self) -> str:
        if self._binary_sha256 is None:
            return ""
        digest = self._binary_sha256.strip().lower()
        if len(digest) != 64 or any(
            character not in "0123456789abcdef" for character in digest
        ):
            raise ValueError(
                "ygg_binary_sha256 must be a 64-character hexadecimal digest"
            )
        return (
            f"printf '%s  %s\\n' {shlex.quote(digest)} "
            f"{shlex.quote(self._source_binary)} | sha256sum -c -"
        )

    def _write_setup_result(
        self,
        *,
        stdout: str | None,
        stderr: str | None,
        return_code: int | None,
        error: BaseException | None = None,
    ) -> None:
        self._write_text("setup-stdout.txt", stdout)
        self._write_text("setup-stderr.txt", stderr)
        self._write_text(
            "setup-exit-status.txt",
            "unknown\n" if return_code is None else f"{return_code}\n",
        )
        if error is not None:
            self._write_text("setup-error.txt", f"{type(error).__name__}: {error}\n")

    async def setup(self, environment: BaseEnvironment) -> None:
        """Copy and verify the pinned binary in the task environment."""

        try:
            hash_check = self._expected_hash_check()
            checks = [
                "set -eu",
                "command -v bash >/dev/null",
                "command -v setsid >/dev/null",
                f"test -x {shlex.quote(self._source_binary)}",
                f"mkdir -p {shlex.quote(self._session_dir)}",
            ]
            if self._agent_timeout_sec:
                checks.append("command -v timeout >/dev/null")
            if hash_check:
                checks.append(hash_check)
            checks.extend(
                [
                    f"install -m 0755 {shlex.quote(self._source_binary)} {shlex.quote(self._binary)}",
                    f"{shlex.quote(self._binary)} --version",
                ]
            )
            result = await environment.exec(command=" && ".join(checks))
        except Exception as error:
            self._write_setup_result(
                stdout=None,
                stderr=None,
                return_code=None,
                error=error,
            )
            raise YggSetupError(
                "Ygg setup command failed; inspect setup-*.txt in the Harbor agent logs"
            ) from error

        self._write_setup_result(
            stdout=result.stdout,
            stderr=result.stderr,
            return_code=result.return_code,
        )
        if result.return_code != 0:
            raise YggSetupError(
                f"Ygg setup failed with exit {result.return_code}; "
                "inspect setup-*.txt in the Harbor agent logs"
            )

        version = parse_ygg_version(result.stdout or "")
        if version != PINNED_YGG_VERSION:
            raise YggSetupError(
                f"Ygg version mismatch: expected {PINNED_YGG_VERSION}, "
                f"got {version or 'unknown'}"
            )
        self._installed_version = version
        self._write_text("native-session-root.txt", f"{self._host_session_root}\n")

    def _write_invocation(self, command: YggCommand, instruction: str) -> None:
        """Record reproducibility metadata without persisting the prompt."""

        self._write_json(
            "invocation.json",
            {
                "argv_without_instruction": list(command.argv[:-1]),
                "instruction_bytes": len(instruction.encode("utf-8")),
                "instruction_sha256": hashlib.sha256(
                    instruction.encode("utf-8")
                ).hexdigest(),
                "session_dir": self._session_dir,
                "binary_source": self._source_binary,
                "binary_path": self._binary,
                "binary_sha256": self._binary_sha256,
                "verified_version": self.version(),
                "model": self.model_name,
                "reasoning": self._reasoning,
                "max_turns": self._max_turns,
                "bash_timeout_secs": self._bash_timeout_secs,
                "workspace_trusted": self._workspace_trusted,
                "telemetry": self._telemetry,
                "timeout_sec": self._agent_timeout_sec,
                "process_group_cleanup": True,
            },
        )

    def _write_run_result(
        self,
        *,
        command: YggCommand,
        instruction: str,
        kind: FailureKind,
        started: float,
        stdout: str | None,
        stderr: str | None,
        return_code: int | None,
        timed_out: bool,
        error: BaseException | None = None,
    ) -> None:
        self._write_text("stdout.txt", stdout)
        self._write_text("stderr.txt", stderr)
        self._write_text(
            "exit-status.txt",
            "unknown\n" if return_code is None else f"{return_code}\n",
        )
        self._write_text("failure-classification.txt", f"{kind.value}\n")
        if error is not None:
            self._write_text(
                "execution-error.txt", f"{type(error).__name__}: {error}\n"
            )
        self._write_json(
            "run-metadata.json",
            {
                "failure_classification": kind.value,
                "return_code": return_code,
                "timed_out": timed_out,
                "duration_seconds": round(time.monotonic() - started, 3),
                "instruction_sha256": hashlib.sha256(
                    instruction.encode("utf-8")
                ).hexdigest(),
                "binary_sha256": self._binary_sha256,
                "verified_version": self.version(),
                "command_argv_without_instruction": list(command.argv[:-1]),
            },
        )

    def _redact_native_sessions(self) -> bool:
        """Redact credentials in-place while preserving valid JSONL records."""

        root = self._host_session_root
        if not root.is_dir():
            return True
        complete = True
        for path in root.rglob("*.jsonl"):
            if not path.is_file():
                continue
            try:
                original = path.read_text(encoding="utf-8")
                redacted = redact_jsonl(original, self._secrets())
                if redacted != original:
                    path.write_text(redacted, encoding="utf-8")
            except (OSError, UnicodeError):
                complete = False
        return complete

    def _convert_native_session(self) -> None:
        conversion = convert_native_sessions(
            self._host_session_root,
            agent_name=self.name(),
            agent_version=self.version(),
            model_name=self.model_name,
            reasoning=self._reasoning,
        )
        self._last_conversion = conversion
        if conversion is not None:
            self._write_json("trajectory.json", conversion.trajectory)

    def _write_session_manifest(self) -> None:
        root = self._host_session_root
        files: list[dict[str, Any]] = []
        if root.is_dir():
            for path in sorted(root.rglob("*.jsonl")):
                if path.is_file():
                    digest = hashlib.sha256(path.read_bytes()).hexdigest()
                    files.append(
                        {
                            "path": str(path.relative_to(self.logs_dir)),
                            "bytes": path.stat().st_size,
                            "sha256": digest,
                        }
                    )
        telemetry = self.logs_dir / "ygg-telemetry.jsonl"
        if self._telemetry and telemetry.is_file():
            files.append(
                {
                    "path": str(telemetry.relative_to(self.logs_dir)),
                    "bytes": telemetry.stat().st_size,
                    "sha256": hashlib.sha256(telemetry.read_bytes()).hexdigest(),
                }
            )
        self._write_json(
            "native-session-manifest.json",
            {
                "container_root": self._session_dir,
                "host_root": str(root),
                "files": files,
            },
        )

    def _finalize_session_artifacts(self) -> None:
        """Retain, redact, summarize, and index native session artifacts."""

        redaction_complete = True
        try:
            redaction_complete = self._redact_native_sessions()
            if not redaction_complete:
                self._write_text(
                    "session-conversion-error.txt",
                    "credential redaction did not complete for every native session\n",
                )
        except (OSError, UnicodeError) as error:
            redaction_complete = False
            self._write_text(
                "session-redaction-error.txt",
                f"{type(error).__name__}: {error}\n",
            )
        if redaction_complete:
            try:
                self._convert_native_session()
            except (
                OSError,
                TypeError,
                ValueError,
                KeyError,
                AttributeError,
            ) as error:
                self._write_text(
                    "session-conversion-error.txt",
                    f"{type(error).__name__}: {error}\n",
                )
        try:
            self._write_session_manifest()
        except (OSError, ValueError) as error:
            self._write_text(
                "session-manifest-error.txt",
                f"{type(error).__name__}: {error}\n",
            )

    def populate_context_post_run(self, context: AgentContext) -> None:
        """Populate Harbor metrics from durable Ygg usage records."""

        if self._last_conversion is None:
            self._finalize_session_artifacts()
        conversion = self._last_conversion
        if conversion is None or not conversion.metrics.saw_usage:
            return

        metrics = conversion.metrics
        context.n_input_tokens = metrics.input_tokens
        context.n_cache_tokens = metrics.cache_tokens
        context.n_output_tokens = metrics.output_tokens
        context.cost_usd = metrics.cost_usd
        context.metadata = {
            **(context.metadata or {}),
            "native_session": str(conversion.source.relative_to(self.logs_dir)),
            "native_session_turns": metrics.turns,
        }

    @staticmethod
    def _is_timeout_error(error: BaseException) -> bool:
        if isinstance(error, (TimeoutError, asyncio.TimeoutError)):
            return True
        text = str(error).casefold()
        return "timed out" in text or "timeout" in text

    async def _terminate_run_process_group(self, environment: BaseEnvironment) -> None:
        """Stop and reap the exact process group before exposing final artifacts."""

        cleanup = terminate_process_group_command(
            _PROCESS_GROUP_FILE,
            term_grace_sec=_PROCESS_GROUP_TERM_GRACE_SECONDS,
        )
        result = await asyncio.shield(
            environment.exec(
                command=cleanup.shell,
                timeout_sec=_PROCESS_GROUP_CLEANUP_TIMEOUT_SECONDS,
            )
        )
        if result.return_code != 0:
            detail = redact_text(
                result.stderr or result.stdout or "no cleanup output",
                self._secrets(),
            ).strip()
            raise YggAgentError(
                "Ygg process-group cleanup failed "
                f"with exit {result.return_code}: {detail}"
            )

    def _write_termination_error(self, error: BaseException) -> None:
        self._write_text("termination-error.txt", f"{type(error).__name__}: {error}\n")

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
            bash_timeout_secs=self._bash_timeout_secs,
            workspace_trusted=self._workspace_trusted,
            telemetry_path="/logs/agent/ygg-telemetry.jsonl"
            if self._telemetry
            else None,
        )
        self._write_invocation(command, instruction)
        self._last_conversion = None
        started = time.monotonic()
        deadline_command = (
            wrap_with_timeout(command, self._agent_timeout_sec)
            if self._agent_timeout_sec
            else command
        )
        executed_command = wrap_in_process_group(deadline_command, _PROCESS_GROUP_FILE)
        environment_timeout = (
            self._agent_timeout_sec + _TIMEOUT_GRACE_SECONDS
            if self._agent_timeout_sec
            else None
        )

        try:
            result = await environment.exec(
                command=executed_command.shell,
                env=self._run_env() or None,
                timeout_sec=environment_timeout,
            )
        except asyncio.CancelledError as error:
            try:
                await self._terminate_run_process_group(environment)
            except Exception as termination_error:
                self._write_termination_error(termination_error)
                self._write_run_result(
                    command=command,
                    instruction=instruction,
                    kind=FailureKind.TIMEOUT,
                    started=started,
                    stdout=None,
                    stderr=None,
                    return_code=None,
                    timed_out=True,
                    error=termination_error,
                )
                # Artifacts are not immutable while process death is unproven.
                raise error
            self._write_run_result(
                command=command,
                instruction=instruction,
                kind=FailureKind.TIMEOUT,
                started=started,
                stdout=None,
                stderr=None,
                return_code=None,
                timed_out=True,
                error=error,
            )
            self._finalize_session_artifacts()
            raise
        except Exception as error:
            try:
                await self._terminate_run_process_group(environment)
            except Exception as termination_error:
                self._write_termination_error(termination_error)
                self._write_run_result(
                    command=command,
                    instruction=instruction,
                    kind=FailureKind.AGENT,
                    started=started,
                    stdout=None,
                    stderr=None,
                    return_code=None,
                    timed_out=False,
                    error=termination_error,
                )
                raise YggAgentError(
                    "Ygg process death was not confirmed; artifacts were not finalized"
                ) from termination_error
            timed_out = self._is_timeout_error(error)
            kind = FailureKind.TIMEOUT if timed_out else FailureKind.AGENT
            self._write_run_result(
                command=command,
                instruction=instruction,
                kind=kind,
                started=started,
                stdout=None,
                stderr=None,
                return_code=None,
                timed_out=timed_out,
                error=error,
            )
            self._finalize_session_artifacts()
            if timed_out:
                raise YggBenchmarkTimeoutError(
                    "Ygg benchmark timeout; inspect stdout.txt, stderr.txt, "
                    "and the native session manifest"
                ) from error
            raise YggAgentError(
                "Ygg execution infrastructure failed; inspect execution-error.txt"
            ) from error

        try:
            await self._terminate_run_process_group(environment)
        except Exception as termination_error:
            self._write_termination_error(termination_error)
            self._write_run_result(
                command=command,
                instruction=instruction,
                kind=FailureKind.AGENT,
                started=started,
                stdout=result.stdout,
                stderr=result.stderr,
                return_code=result.return_code,
                timed_out=False,
                error=termination_error,
            )
            raise YggAgentError(
                "Ygg process death was not confirmed; artifacts were not finalized"
            ) from termination_error

        timed_out = bool(self._agent_timeout_sec and result.return_code in (124, 137))
        kind = classify_failure(
            result.return_code,
            result.stdout,
            result.stderr,
            timed_out=timed_out,
        )
        self._write_run_result(
            command=command,
            instruction=instruction,
            kind=kind,
            started=started,
            stdout=result.stdout,
            stderr=result.stderr,
            return_code=result.return_code,
            timed_out=timed_out,
        )
        self._finalize_session_artifacts()

        if kind is FailureKind.SUCCESS:
            return
        if kind is FailureKind.TIMEOUT:
            raise YggBenchmarkTimeoutError(
                "Ygg benchmark timeout; inspect stdout.txt, stderr.txt, "
                "and the native session manifest"
            )
        if kind is FailureKind.PROVIDER:
            raise YggProviderError(
                "Ygg provider failure; inspect stdout.txt and stderr.txt"
            )
        raise YggAgentError("Ygg agent failure; inspect stdout.txt and stderr.txt")


__all__ = [
    "Ygg",
    "YggAgentError",
    "YggBenchmarkTimeoutError",
    "YggProviderError",
    "YggSetupError",
]
