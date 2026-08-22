"""API 0.2 executable-extension wiring for the ygg-ssh portal."""

from __future__ import annotations

import argparse
from collections.abc import Mapping
from pathlib import Path
from typing import Any, Optional, Union

from ygg_extension import Extension

from .config import ConfigError, SshConfig, load_config
from .session import DEFAULT_CONNECT_TIMEOUT_MS, SshSessions


SUPPORTED_FEATURES = (
    "request_cancellation",
    "content_parts",
    "request_progress",
    "lifecycle_events",
)


def build_runtime(
    *,
    config_path: Optional[Path] = None,
    ssh_binary: Optional[Union[Path, str]] = None,
    connect_timeout_ms: int = DEFAULT_CONNECT_TIMEOUT_MS,
) -> tuple[Extension, SshSessions]:
    try:
        config = load_config(config_path)
    except ConfigError:
        config = SshConfig.empty(Path(config_path) if config_path else None)

    extension = Extension(
        api_version="0.2",
        max_concurrent_requests=8,
        max_pending_requests=32,
        writer_queue_size=64,
        shutdown_timeout=2.0,
        supported_features=SUPPORTED_FEATURES,
    )

    sessions = SshSessions(config, connect_timeout_ms=connect_timeout_ms)
    if ssh_binary is not None:
        sessions._prober = lambda target: _probe_with_binary(
            ssh_binary, target, connect_timeout_ms
        )

    @extension.command(
        name="ssh",
        description="Inspect configured SSH targets or request safe connection lifecycle actions",
        usage=(
            "/ssh [status|list|show <target>|connect <target>|disconnect <target>]"
        ),
    )
    def ssh_command(arguments: list[str], context: Mapping[str, Any]) -> dict[str, Any]:
        return sessions.execute_command(arguments, context)

    @extension.status("status")
    def ssh_surface(params: Mapping[str, Any]) -> dict[str, Any]:
        contribution = sessions.status_contribution()
        contribution["surface"] = params.get("surface", "status")
        return contribution

    @extension.context()
    def collect_context(request: Mapping[str, Any], context: Mapping[str, Any]) -> list[dict[str, Any]]:
        del context
        contribution = sessions.context_contribution()
        if contribution is None:
            return []
        return [contribution]

    @extension.on_lifecycle("session/settled")
    def session_settled(event: Mapping[str, Any]) -> None:
        sessions.settle_session(event.get("session_id"))

    return extension, sessions


def _probe_with_binary(
    ssh_binary: Union[Path, str],
    target: Any,
    connect_timeout_ms: int,
) -> Any:
    """Probe using an explicit ssh binary (used by tests and smoke setups)."""

    import subprocess
    import time

    argv = [
        str(ssh_binary),
        "-o",
        "BatchMode=yes",
        "-o",
        "NumberOfPasswordPrompts=0",
        "-o",
        f"ConnectTimeout={max(1, round(connect_timeout_ms / 1000))}",
        "--",
        target.alias,
        "true",
    ]
    started = time.monotonic()
    try:
        completed = subprocess.run(
            argv,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=connect_timeout_ms / 1000 + 1.0,
            check=False,
        )
    except (subprocess.TimeoutExpired, OSError):
        from .session import ProbeResult

        return ProbeResult(False, None, int((time.monotonic() - started) * 1000))
    from .session import ProbeResult

    return ProbeResult(
        completed.returncode == 0,
        completed.returncode,
        int((time.monotonic() - started) * 1000),
    )


def run(
    *,
    config_path: Optional[Path] = None,
    ssh_binary: Optional[Union[Path, str]] = None,
) -> None:
    extension, _sessions = build_runtime(config_path=config_path, ssh_binary=ssh_binary)
    extension.run()


def main(argv: Optional[list[str]] = None) -> int:
    parser = argparse.ArgumentParser(
        prog="ygg-ssh",
        description="Ygg SSH portal for explicit authenticated OpenSSH aliases",
    )
    parser.add_argument(
        "--config",
        type=Path,
        help="explicit user configuration path (normal Ygg launches use ~/.ygg/ssh.json)",
    )
    parser.add_argument(
        "--ssh-binary",
        type=Path,
        help=argparse.SUPPRESS,
    )
    parser.add_argument(
        "--check-config",
        action="store_true",
        help="validate configuration without connecting, authenticating, or starting the protocol",
    )
    arguments = parser.parse_args(argv)
    if arguments.check_config:
        try:
            config = load_config(arguments.config)
        except ConfigError as error:
            parser.error(str(error))
        print(f"valid SSH configuration: {len(config.targets)} explicit targets")
        return 0
    run(config_path=arguments.config, ssh_binary=arguments.ssh_binary)
    return 0
