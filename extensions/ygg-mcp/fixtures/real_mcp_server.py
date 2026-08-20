#!/usr/bin/env python3
"""Small conforming local MCP fixture with no external services or packages."""

from __future__ import annotations

import base64
import json
import sys
from typing import Any


PNG = b"\x89PNG\r\n\x1a\nfixture-payload"
WAV = b"RIFF\x04\x00\x00\x00WAVE"


def send(message: dict[str, Any]) -> None:
    sys.stdout.write(json.dumps(message, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def result(request_id: Any, value: Any) -> None:
    send({"jsonrpc": "2.0", "id": request_id, "result": value})


def tools() -> list[dict[str, Any]]:
    return [
        {
            "name": "fixture_echo",
            "description": "Echo one bounded value from the local fixture",
            "inputSchema": {
                "type": "object",
                "properties": {"value": {"type": "string"}},
                "required": ["value"],
                "additionalProperties": False,
            },
            "outputSchema": {
                "type": "object",
                "properties": {"echo": {"type": "string"}},
                "required": ["echo"],
                "additionalProperties": False,
            },
            "annotations": {"readOnlyHint": True},
        },
        {
            "name": "fixture_media",
            "description": "Return local image and audio fixture bytes",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": False},
            "annotations": {"readOnlyHint": True},
        },
        {
            "name": "fixture_unknown_effect",
            "description": "Exercise fail-closed approval when annotations are absent",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": False},
        },
    ]


for line in sys.stdin:
    try:
        message = json.loads(line)
    except json.JSONDecodeError:
        continue
    method = message.get("method")
    request_id = message.get("id")
    params = message.get("params", {})
    if method == "initialize" and request_id is not None:
        result(
            request_id,
            {
                "protocolVersion": "2025-06-18",
                "capabilities": {"tools": {"listChanged": False}},
                "serverInfo": {"name": "ygg-mcp-real-fixture", "version": "1.0.0"},
            },
        )
    elif method == "notifications/initialized":
        continue
    elif method == "tools/list" and request_id is not None:
        result(request_id, {"tools": tools()})
    elif method == "tools/call" and request_id is not None:
        name = params.get("name") if isinstance(params, dict) else None
        arguments = params.get("arguments", {}) if isinstance(params, dict) else {}
        if name == "fixture_echo":
            value = arguments.get("value", "") if isinstance(arguments, dict) else ""
            result(
                request_id,
                {
                    "content": [{"type": "text", "text": f"fixture echo: {value}"}],
                    "structuredContent": {"echo": value},
                    "isError": False,
                },
            )
        elif name == "fixture_media":
            result(
                request_id,
                {
                    "content": [
                        {"type": "text", "text": "fixture media"},
                        {
                            "type": "image",
                            "mimeType": "image/png",
                            "data": base64.b64encode(PNG).decode("ascii"),
                        },
                        {
                            "type": "audio",
                            "mimeType": "audio/wav",
                            "data": base64.b64encode(WAV).decode("ascii"),
                        },
                    ],
                    "isError": False,
                },
            )
        elif name == "fixture_unknown_effect":
            result(
                request_id,
                {"content": [{"type": "text", "text": "should be policy gated"}], "isError": False},
            )
        else:
            send(
                {
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "error": {"code": -32601, "message": "unknown fixture tool"},
                }
            )
    elif request_id is not None:
        send(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {"code": -32601, "message": "method not found"},
            }
        )
