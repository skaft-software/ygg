"""API 0.2 executable-extension wiring for ygg-ssh."""

from __future__ import annotations

import argparse
from collections.abc import Mapping
import os
from pathlib import Path
import threading
from typing import Any, Optional, Union

from ygg_extension import Extension, text_content, tool_result

from .config import ConfigError, SshConfig, load_config
from .manager import AdapterError, SshManager
from .process import OpenSshBackend


SUPPORTED_FEATURES = (
    "request_cancellation",
    "content_parts",
    "request_progress",
    "lifecycle_events",
)
COMMON_OUTPUT_SCHEMA = {
    "type": "object",
    "properties": {
        "operation": {"type": "string", "maxLength": 32},
        "status": {"type": "string", "enum": ["ok", "error"]},
        "remote": {"type": "boolean"},
        "summary": {"type": "string", "maxLength": 4096},
        "untrusted": {"type": "boolean"},
        "code": {"type": "string", "maxLength": 128},
        "ambiguous": {"type": "boolean"},
    },
    "required": ["operation", "status", "remote", "summary", "untrusted"],
    "additionalProperties": True,
}


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
    ssh_binary: Optional[Union[Path, str]] = None,
    environment: Optional[Mapping[str, str]] = None,
    runtime_directory: Optional[Path] = None,
) -> tuple[ProtocolReadyExtension, SshManager]:
    workspace = os.environ.get("YGG_WORKSPACE")
    configuration_error = None
    try:
        config = load_config(config_path, workspace=workspace)
    except ConfigError:
        config = SshConfig.empty(Path(config_path) if config_path else None)
        configuration_error = "SSH configuration failed a bounded trust or schema check"

    extension = ProtocolReadyExtension(
        api_version="0.2",
        max_concurrent_requests=8,
        max_pending_requests=32,
        writer_queue_size=64,
        shutdown_timeout=2.0,
        cancellation_grace=0.25,
        supported_features=SUPPORTED_FEATURES,
    )
    scratch = runtime_directory or Path(
        os.environ.get("YGG_EXTENSION_SCRATCH", ".ygg-ssh-scratch")
    )
    backend = OpenSshBackend(
        config.limits,
        ssh_binary=ssh_binary,
        runtime_directory=scratch,
        environment=environment,
    )

    def confirm(prompt: str, detail: str, destructive: bool) -> bool:
        return extension.confirm(
            prompt,
            detail=detail,
            destructive=destructive,
            default=False,
        )

    def publish_presentation(
        snapshot: Mapping[str, Any], owner: Optional[Mapping[str, Any]]
    ) -> None:
        if owner is None:
            extension.publish_presentation(snapshot)
        else:
            extension.publish_presentation(snapshot, resource_owner=owner)

    manager = SshManager(
        config,
        backend,
        confirm=confirm,
        publisher=publish_presentation,
        logger=extension.log,
        configuration_error=configuration_error,
    )

    status_parameters = {"type": "object", "properties": {}, "additionalProperties": False}
    exec_parameters = {
        "type": "object",
        "properties": {
            "argv": {
                "type": "array",
                "minItems": 1,
                "maxItems": config.limits.max_command_args,
                "items": {"type": "string"},
            },
            "timeout_ms": {
                "type": "integer",
                "minimum": 100,
                "maximum": config.limits.operation_timeout_ms,
            },
        },
        "required": ["argv"],
        "additionalProperties": False,
    }
    read_parameters = {
        "type": "object",
        "properties": {
            "path": {"type": "string", "minLength": 1, "maxLength": 4096},
            "offset": {"type": "integer", "minimum": 0},
            "max_bytes": {
                "type": "integer",
                "minimum": 1,
                "maximum": config.limits.max_file_bytes,
            },
            "timeout_ms": {
                "type": "integer",
                "minimum": 100,
                "maximum": config.limits.operation_timeout_ms,
            },
        },
        "required": ["path"],
        "additionalProperties": False,
    }
    write_parameters = {
        "type": "object",
        "properties": {
            "path": {"type": "string", "minLength": 1, "maxLength": 4096},
            "data": {"type": "string", "maxLength": config.limits.max_file_bytes * 2},
            "encoding": {"type": "string", "enum": ["utf8", "base64"]},
            "overwrite": {"type": "boolean"},
            "timeout_ms": {
                "type": "integer",
                "minimum": 100,
                "maximum": config.limits.operation_timeout_ms,
            },
        },
        "required": ["path", "data"],
        "additionalProperties": False,
    }

    @extension.tool(
        name="ssh_status",
        description=(
            "Inspect the current owner-fenced authenticated OpenSSH session. This tool cannot "
            "select a host; use /ssh connect with an explicit configured target."
        ),
        parameters=status_parameters,
        output_schema=COMMON_OUTPUT_SCHEMA,
    )
    def ssh_status(arguments: Mapping[str, Any], context: Mapping[str, Any]) -> dict[str, Any]:
        del arguments
        try:
            data = manager.status(context, cancellation=extension.cancellation)
        except AdapterError as error:
            return _error_result("status", error)
        structured = {
            "operation": "status",
            "status": "ok",
            "remote": bool(data.get("connected")),
            "summary": _status_summary(data),
            "untrusted": False,
            **{key: value for key, value in data.items() if value is not None},
        }
        return tool_result(
            text_content(structured["summary"]),
            structured_content=structured,
        )

    @extension.tool(
        name="ssh_exec",
        description=(
            "Run a bounded argv command in the selected configured remote cwd. V1 treats every "
            "command as a mutation: it is disabled for read-only targets and requires a fresh "
            "interactive approval. Host aliases, users, ports, jumps, cwd, and credentials are "
            "not accepted as arguments."
        ),
        parameters=exec_parameters,
        output_schema=COMMON_OUTPUT_SCHEMA,
    )
    def ssh_exec(arguments: Mapping[str, Any], context: Mapping[str, Any]) -> dict[str, Any]:
        try:
            data = manager.execute(
                context,
                arguments.get("argv"),
                timeout_ms=arguments.get("timeout_ms"),
                cancellation=extension.cancellation,
            )
        except AdapterError as error:
            return _error_result("exec", error)
        structured = {
            "operation": "exec",
            "status": "ok" if data["ok"] else "error",
            "remote": True,
            "summary": (
                f"Remote mutation on configured alias {data['alias']} settled with exit "
                f"status {data['exit_status']}."
            ),
            "untrusted": True,
            **data,
        }
        return tool_result(
            text_content(_format_exec_result(data)),
            structured_content=structured,
            is_error=not data["ok"],
        )

    @extension.tool(
        name="ssh_read",
        description=(
            "Read bounded bytes from a normalized relative path below the selected configured "
            "remote cwd. The lexical path check is not a remote filesystem sandbox; symlinks "
            "remain controlled by the remote account."
        ),
        parameters=read_parameters,
        output_schema=COMMON_OUTPUT_SCHEMA,
    )
    def ssh_read(arguments: Mapping[str, Any], context: Mapping[str, Any]) -> dict[str, Any]:
        try:
            data = manager.read_file(
                context,
                arguments.get("path"),
                offset=arguments.get("offset", 0),
                max_bytes=arguments.get("max_bytes"),
                timeout_ms=arguments.get("timeout_ms"),
                cancellation=extension.cancellation,
            )
        except AdapterError as error:
            return _error_result("read", error)
        structured = {
            "operation": "read",
            "status": "ok",
            "remote": True,
            "summary": (
                f"Read {data['bytes']} bounded untrusted bytes from configured alias "
                f"{data['alias']}."
            ),
            "untrusted": True,
            **data,
        }
        marker = " (truncated)" if data["truncated"] else ""
        text = (
            f"REMOTE · {data['alias']} · read · generation {data['connection_generation']}\n"
            f"Bounded untrusted remote file data{marker}; encoding={data['encoding']}:\n"
            f"--- BEGIN UNTRUSTED REMOTE DATA ---\n{data['data']}\n"
            "--- END UNTRUSTED REMOTE DATA ---"
        )
        return tool_result(text_content(text), structured_content=structured)

    @extension.tool(
        name="ssh_write",
        description=(
            "Atomically write bounded bytes to a normalized relative path below the selected "
            "configured remote cwd. Requires a read-write target and fresh interactive approval."
        ),
        parameters=write_parameters,
        output_schema=COMMON_OUTPUT_SCHEMA,
    )
    def ssh_write(arguments: Mapping[str, Any], context: Mapping[str, Any]) -> dict[str, Any]:
        try:
            data = manager.write_file(
                context,
                arguments.get("path"),
                arguments.get("data"),
                encoding=arguments.get("encoding", "utf8"),
                overwrite=arguments.get("overwrite", False),
                timeout_ms=arguments.get("timeout_ms"),
                cancellation=extension.cancellation,
            )
        except AdapterError as error:
            return _error_result("write", error)
        structured = {
            "operation": "write",
            "status": "ok",
            "remote": True,
            "summary": (
                f"Remote mutation wrote {data['bytes_written']} bytes on configured alias "
                f"{data['alias']}."
            ),
            "untrusted": True,
            **data,
        }
        text = (
            f"REMOTE · {data['alias']} · mutation · generation "
            f"{data['connection_generation']}\n"
            f"Approved bounded atomic remote write completed ({data['bytes_written']} bytes)."
        )
        return tool_result(text_content(text), structured_content=structured)

    @extension.command(
        name="ssh",
        description="Inspect configured SSH targets or request safe connection lifecycle actions",
        usage=(
            "/ssh [status|list|snapshot|show <target>|connect <target>|retry <target>|"
            "disconnect <target>]"
        ),
    )
    def ssh_command(arguments: list[str], context: Mapping[str, Any]) -> dict[str, Any]:
        return manager.execute_command(arguments, context)

    @extension.status("status")
    def ssh_surface(params: Mapping[str, Any]) -> dict[str, Any]:
        contribution = manager.status_contribution()
        contribution["surface"] = params.get("surface", "status")
        return contribution

    @extension.on_lifecycle("session/settled")
    def session_settled(event: Mapping[str, Any]) -> None:
        manager.settle_session(event.get("session_id"))

    @extension.on_shutdown
    def shutdown(params: Mapping[str, Any]) -> None:
        del params
        manager.shutdown()

    return extension, manager


def run(
    *,
    config_path: Optional[Path] = None,
    ssh_binary: Optional[Union[Path, str]] = None,
) -> None:
    extension, manager = build_runtime(config_path=config_path, ssh_binary=ssh_binary)

    def activate_after_handshake() -> None:
        if extension.protocol_ready.wait():
            manager.activate_presentation()

    threading.Thread(
        target=activate_after_handshake,
        name="ygg-ssh-presentation-bootstrap",
        daemon=True,
    ).start()
    try:
        extension.run()
    finally:
        manager.shutdown()


def main(argv: Optional[list[str]] = None) -> int:
    parser = argparse.ArgumentParser(
        prog="ygg-ssh",
        description="Ygg adapter for explicit authenticated OpenSSH aliases",
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
            config = load_config(
                arguments.config,
                workspace=os.environ.get("YGG_WORKSPACE"),
            )
        except ConfigError as error:
            parser.error(str(error))
        print(f"valid SSH configuration: {len(config.targets)} explicit targets")
        return 0
    run(config_path=arguments.config, ssh_binary=arguments.ssh_binary)
    return 0


def _error_result(operation: str, error: AdapterError) -> dict[str, Any]:
    structured = {
        "operation": operation,
        "status": "error",
        "remote": False,
        "summary": error.safe_summary[:4096],
        "untrusted": False,
        "code": error.code[:128],
        "ambiguous": error.ambiguous,
    }
    return tool_result(
        text_content(f"SSH {operation} failed: {structured['summary']}"),
        structured_content=structured,
        is_error=True,
    )


def _status_summary(data: Mapping[str, Any]) -> str:
    if not data.get("connected"):
        selected = data.get("target_id")
        suffix = f"; selected target {selected}" if selected else ""
        return _bounded_utf8(
            f"SSH is disconnected{suffix}. Use /ssh to inspect explicit configured aliases.",
            4096,
        )
    return _bounded_utf8(
        f"REMOTE · {data['alias']} · {data['authority']} · {data['remote_cwd']} · "
        f"generation {data['connection_generation']} · {data['health']}",
        4096,
    )


def _bounded_utf8(value: str, maximum: int) -> str:
    encoded = value.encode("utf-8")
    if len(encoded) <= maximum:
        return value
    encoded = encoded[:maximum]
    while encoded:
        try:
            return encoded.decode("utf-8")
        except UnicodeDecodeError:
            encoded = encoded[:-1]
    return "SSH status"



def _format_exec_result(data: Mapping[str, Any]) -> str:
    lines = [
        f"REMOTE · {data['alias']} · mutation · generation {data['connection_generation']}",
        f"Exit status: {data['exit_status']} · duration: {data['duration_ms']} ms",
    ]
    for stream_name in ("stdout", "stderr"):
        stream = data[stream_name]
        if not stream["data"] and not stream["truncated"]:
            continue
        marker = " · truncated" if stream["truncated"] else ""
        lines.extend(
            [
                f"--- BEGIN UNTRUSTED REMOTE {stream_name.upper()} ({stream['encoding']}{marker}) ---",
                stream["data"],
                f"--- END UNTRUSTED REMOTE {stream_name.upper()} ---",
            ]
        )
    if len(lines) == 2:
        lines.append("No remote output was returned.")
    return "\n".join(lines)
