"""Executable Ygg extension wiring for the resident MCP bridge."""

from __future__ import annotations

import argparse
from collections.abc import Mapping
import os
from pathlib import Path
import threading
from typing import Any, Optional

from ygg_extension import Extension

from .config import BridgeConfig, ConfigError, load_config
from .manager import BridgeManager


SUPPORTED_FEATURES = (
    "request_cancellation",
    "content_parts",
    "request_progress",
    "artifacts",
    "policy_intents",
    "dynamic_tools",
    "approvals",
)


class ProtocolReadyExtension(Extension):
    """Expose a fence after the initialize response has actually been flushed."""

    def __init__(self, **kwargs: Any) -> None:
        super().__init__(**kwargs)
        self.protocol_ready = threading.Event()

    def _send_result(self, request_id: Any, result: Any) -> None:
        super()._send_result(request_id, result)
        if (
            isinstance(result, Mapping)
            and result.get("api_version") == "0.2"
            and isinstance(result.get("protocol"), Mapping)
        ):
            self.protocol_ready.set()


def build_runtime(
    *,
    config_path: Optional[Path] = None,
) -> tuple[ProtocolReadyExtension, BridgeManager]:
    workspace = os.environ.get("YGG_WORKSPACE")
    config_error = None
    try:
        config = load_config(config_path, workspace=workspace)
    except ConfigError:
        # Stay inspectable instead of crashing the extension handshake. The
        # bounded public error has no path, command, argument, environment, or
        # parser-controlled text.
        config = BridgeConfig.empty(Path(config_path) if config_path else None)
        config_error = {
            "code": "invalid_config",
            "summary": "MCP configuration failed a bounded trust or schema check",
        }

    extension = ProtocolReadyExtension(
        api_version="0.2",
        max_concurrent_requests=8,
        max_pending_requests=64,
        writer_queue_size=64,
        shutdown_timeout=2.0,
        cancellation_grace=0.25,
        supported_features=SUPPORTED_FEATURES,
    )
    manager = BridgeManager(
        extension,
        config,
        config_error=config_error,
        scratch_directory=Path(
            os.environ.get("YGG_EXTENSION_SCRATCH", ".ygg-mcp-scratch")
        ),
    )

    @extension.command(
        name="mcp",
        description="Inspect MCP server state or request a safe lifecycle action",
        usage=(
            "/mcp [status|list|snapshot|show <server>|refresh [server]|"
            "restart <server>|stop <server>]"
        ),
    )
    def mcp_command(arguments: list[str], context: Mapping[str, Any]) -> dict[str, Any]:
        del context
        return manager.execute_command(arguments)

    @extension.status("status")
    def mcp_status(params: Mapping[str, Any]) -> dict[str, Any]:
        contribution = manager.status_contribution()
        contribution["surface"] = params.get("surface", "status")
        return contribution

    @extension.on_shutdown
    def shutdown(params: Mapping[str, Any]) -> None:
        del params
        manager.shutdown()

    return extension, manager


def run(config_path: Optional[Path] = None) -> None:
    extension, manager = build_runtime(config_path=config_path)

    def start_after_handshake() -> None:
        if extension.protocol_ready.wait():
            manager.start()

    threading.Thread(
        target=start_after_handshake,
        name="ygg-mcp-bootstrap",
        daemon=True,
    ).start()
    try:
        extension.run()
    finally:
        manager.shutdown()


def main(argv: Optional[list[str]] = None) -> int:
    parser = argparse.ArgumentParser(
        prog="ygg-mcp",
        description="Ygg API 0.2 bridge for explicitly configured local stdio MCP servers",
    )
    parser.add_argument(
        "--config",
        type=Path,
        help="explicit user configuration path (normal Ygg launches use ~/.ygg/mcp.json)",
    )
    parser.add_argument(
        "--check-config",
        action="store_true",
        help="validate configuration without launching a server or starting the Ygg protocol",
    )
    arguments = parser.parse_args(argv)
    if arguments.check_config:
        try:
            config = load_config(
                arguments.config,
                workspace=os.environ.get("YGG_WORKSPACE"),
            )
        except ConfigError as error:
            parser.error(str(error))
        print(f"valid MCP configuration: {len(config.servers)} configured servers")
        return 0
    run(arguments.config)
    return 0
