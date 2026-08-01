"""Pure command and outcome helpers for the Harbor adapter."""

from __future__ import annotations

import re
import shlex
from collections.abc import Sequence
from dataclasses import dataclass
from enum import Enum


class FailureKind(str, Enum):
    """Failure categories retained in Harbor trial errors and logs."""

    SUCCESS = "success"
    PROVIDER = "provider_failure"
    AGENT = "agent_failure"
    TIMEOUT = "benchmark_timeout"


@dataclass(frozen=True)
class YggCommand:
    """A shell-safe Ygg invocation and its inspectable argument vector."""

    argv: tuple[str, ...]

    @property
    def shell(self) -> str:
        """Return a POSIX shell representation without interpolating input."""

        return shlex.join(self.argv)


_MAX_BASH_TIMEOUT_SECS = 3_600


def build_ygg_argv(
    binary: str,
    instruction: str,
    *,
    model: str | None,
    reasoning: str | None,
    session_dir: str,
    max_turns: int | None = None,
    bash_timeout_secs: int | None = None,
    workspace_trusted: bool = True,
) -> tuple[str, ...]:
    """Build the explicit headless Ygg argument vector.

    The ``--`` separator keeps a leading dash in the instruction from being
    interpreted as another option. Callers should pass the resulting vector
    through :func:`shlex.join` (or an equivalent shell quoting function) rather
    than concatenating the instruction into a command.
    """

    if not instruction.strip():
        raise ValueError("instruction must not be empty")
    if not binary:
        raise ValueError("binary must not be empty")
    if not session_dir:
        raise ValueError("session_dir must not be empty")
    if max_turns is not None and max_turns < 1:
        raise ValueError("max_turns must be greater than zero")
    if (
        bash_timeout_secs is not None
        and not 1 <= bash_timeout_secs <= _MAX_BASH_TIMEOUT_SECS
    ):
        raise ValueError("bash_timeout_secs must be between 1 and 3,600 seconds")

    argv: list[str] = [binary, "--print"]
    if model:
        argv.extend(("--model", model))
    if reasoning:
        argv.extend(("--reasoning", reasoning))
    argv.extend(("--session-dir", session_dir))
    if workspace_trusted:
        argv.append("--workspace-trusted")
    if max_turns is not None:
        argv.extend(("--max-turns", str(max_turns)))
    if bash_timeout_secs is not None:
        argv.extend(("--bash-timeout-secs", str(bash_timeout_secs)))
    argv.extend(("--", instruction))
    return tuple(argv)


def build_ygg_command(
    binary: str,
    instruction: str,
    *,
    model: str | None,
    reasoning: str | None,
    session_dir: str,
    max_turns: int | None = None,
    bash_timeout_secs: int | None = None,
    workspace_trusted: bool = True,
) -> YggCommand:
    """Build a shell-safe Ygg command."""

    return YggCommand(
        build_ygg_argv(
            binary,
            instruction,
            model=model,
            reasoning=reasoning,
            session_dir=session_dir,
            max_turns=max_turns,
            bash_timeout_secs=bash_timeout_secs,
            workspace_trusted=workspace_trusted,
        )
    )


def wrap_with_timeout(command: YggCommand, timeout_sec: int) -> YggCommand:
    """Run a command through coreutils timeout while retaining its output."""

    if timeout_sec < 1:
        raise ValueError("timeout_sec must be greater than zero")
    return YggCommand(
        (
            "timeout",
            "--signal=TERM",
            "--kill-after=5s",
            f"{timeout_sec}s",
            *command.argv,
        )
    )


_VERSION_RE = re.compile(r"\bygg(?:\s+version)?\s+v?(\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?)\b")
_FALLBACK_VERSION_RE = re.compile(r"\bv?(\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?)\b")


def parse_ygg_version(output: str) -> str | None:
    """Extract a semantic Ygg version from ``ygg --version`` output."""

    match = _VERSION_RE.search(output)
    if match:
        return match.group(1)
    match = _FALLBACK_VERSION_RE.search(output)
    return match.group(1) if match else None


_PROVIDER_MARKERS: tuple[str, ...] = (
    "api key",
    "api_key",
    "authentication",
    "unauthorized",
    "forbidden",
    "rate limit",
    "rate-limit",
    "quota exceeded",
    "usage limit",
    "provider error",
    "provider failure",
    "model not found",
    "cannot use this model",
    "connection refused",
    "connection reset",
    "network error",
    "tls handshake",
    "http 401",
    "http 403",
    "http 429",
    "http 500",
    "http 502",
    "http 503",
)


def classify_failure(
    return_code: int,
    stdout: str | None = None,
    stderr: str | None = None,
    *,
    timed_out: bool = False,
) -> FailureKind:
    """Classify a completed Ygg process without treating exit as task success."""

    if timed_out:
        return FailureKind.TIMEOUT
    if return_code == 0:
        return FailureKind.SUCCESS

    output = f"{stdout or ''}\n{stderr or ''}".casefold()
    if any(marker in output for marker in _PROVIDER_MARKERS):
        return FailureKind.PROVIDER
    return FailureKind.AGENT


def classify_setup(return_code: int) -> FailureKind:
    """Return the stable setup category for a failed setup command."""

    return FailureKind.AGENT if return_code != 0 else FailureKind.SUCCESS


def ensure_sequence(values: Sequence[str]) -> tuple[str, ...]:
    """Normalize a sequence for immutable adapter configuration."""

    return tuple(value for value in values if value)
