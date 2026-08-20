"""API 0.2 executable-extension wiring for ygg-hermes-memory."""

from __future__ import annotations

import argparse
from collections.abc import Mapping
from pathlib import Path
import threading
from typing import Any, Optional

from ygg_extension import Extension

from .config import BridgeConfig, ConfigError, load_config
from .constants import HERMES_CONTRACT_ID
from .discovery import discover_providers
from .manager import MemoryBridge


SUPPORTED_FEATURES = (
    "request_cancellation",
    "content_parts",
    "request_progress",
    "lifecycle_events",
    "dynamic_tools",
)


class ProtocolReadyExtension(Extension):
    """Expose a fence after the initialize response is actually flushed."""

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
) -> tuple[ProtocolReadyExtension, MemoryBridge]:
    config_error = None
    try:
        config = load_config(config_path)
    except ConfigError:
        config = BridgeConfig.empty(Path(config_path) if config_path else None)
        config_error = "invalid_config"

    extension = ProtocolReadyExtension(
        api_version="0.2",
        max_concurrent_requests=8,
        max_pending_requests=64,
        writer_queue_size=64,
        shutdown_timeout=2.0,
        cancellation_grace=0.25,
        supported_features=SUPPORTED_FEATURES,
    )
    bridge = MemoryBridge(extension, config, config_error_code=config_error)

    @extension.command(
        name="memory",
        description="Inspect, trust, select, disable, retry, or reload a Hermes memory provider",
        usage=(
            "/memory [status|list|snapshot|show ID|trust ID FINGERPRINT|"
            "select ID|off|retry|reload|discover|lifecycle]"
        ),
    )
    def memory_command(arguments: list[str], context: Mapping[str, Any]) -> Mapping[str, Any]:
        return bridge.execute_command(arguments, context)

    @extension.context
    def memory_context(params: Mapping[str, Any], context: Mapping[str, Any]) -> list[Mapping[str, Any]]:
        return bridge.collect_context(params, context)

    @extension.status("status")
    def memory_status(params: Mapping[str, Any], context: Mapping[str, Any]) -> Mapping[str, Any]:
        surface = params.get("surface", "status")
        return bridge.status_contribution(context, str(surface))

    @extension.hook("before_prompt")
    def before_prompt(payload: Mapping[str, Any], context: Mapping[str, Any]) -> Mapping[str, Any]:
        return bridge.before_prompt(payload, context)

    @extension.hook("after_response")
    def after_response(payload: Mapping[str, Any], context: Mapping[str, Any]) -> Mapping[str, Any]:
        return bridge.after_response(payload, context)

    @extension.hook("after_tool_call")
    def after_tool_call(payload: Mapping[str, Any], context: Mapping[str, Any]) -> Mapping[str, Any]:
        return bridge.after_tool_call(payload, context)

    for lifecycle_method in (
        "session/started",
        "session/settled",
        "turn/started",
        "turn/settled",
        "tool/started",
        "tool/settled",
    ):
        def register(method: str) -> None:
            @extension.on_lifecycle(method)
            def observe(event: Mapping[str, Any]) -> None:
                bridge.lifecycle(method, event)

        register(lifecycle_method)

    @extension.on_shutdown
    def shutdown(params: Mapping[str, Any]) -> None:
        del params
        bridge.shutdown()

    return extension, bridge


def run(config_path: Optional[Path] = None) -> None:
    extension, bridge = build_runtime(config_path=config_path)

    def start_after_handshake() -> None:
        if extension.protocol_ready.wait():
            bridge.start(extension.initialization)

    threading.Thread(
        target=start_after_handshake,
        name="ygg-hermes-memory-bootstrap",
        daemon=True,
    ).start()
    try:
        extension.run()
    finally:
        bridge.shutdown()


def main(argv: Optional[list[str]] = None) -> int:
    parser = argparse.ArgumentParser(
        prog="ygg-hermes-memory",
        description="Ygg bridge for the pinned Hermes Agent MemoryProvider contract",
    )
    parser.add_argument(
        "--config",
        type=Path,
        help="explicit configuration (normal launches use ~/.ygg/hermes-memory.json)",
    )
    parser.add_argument(
        "--check-config",
        action="store_true",
        help="validate configuration without importing or initializing a provider",
    )
    parser.add_argument(
        "--discover",
        action="store_true",
        help="print metadata-only provider ids/fingerprints without importing providers",
    )
    parser.add_argument(
        "--contract",
        action="store_true",
        help="print the exact targeted Hermes contract id",
    )
    arguments = parser.parse_args(argv)
    if arguments.contract:
        print(HERMES_CONTRACT_ID)
        return 0
    if arguments.check_config or arguments.discover:
        try:
            config = load_config(arguments.config)
        except ConfigError as error:
            parser.error(str(error))
        if arguments.discover:
            snapshot = discover_providers(config)
            print(
                f"environment={snapshot.environment_id} "
                f"version={snapshot.environment_version or 'unavailable'} "
                f"state={snapshot.environment_state}"
            )
            for candidate in snapshot.candidates:
                print(
                    f"{candidate.id}\t{candidate.label}\t{candidate.version}\t"
                    f"{candidate.fingerprint or 'unavailable'}\t{candidate.availability}"
                )
        else:
            print(
                f"valid Hermes memory configuration: {len(config.directories)} explicit "
                f"directory provider(s); contract {config.contract_id}"
            )
        return 0
    run(arguments.config)
    return 0
