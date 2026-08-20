#!/usr/bin/env python3
"""Adversarial local MCP stdio fixture used only by ygg-mcp tests."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import threading
import time
from typing import Any


parser = argparse.ArgumentParser()
parser.add_argument(
    "--scenario",
    choices=("stable", "catalog", "malformed", "oversized", "crash", "timeout", "logs", "blocked"),
    default="stable",
)
parser.add_argument("--oversized-bytes", type=int, default=2 * 1024 * 1024)
parser.add_argument("--shutdown-marker", type=Path)
parser.add_argument("--descendant-pid", type=Path)
args = parser.parse_args()

if args.descendant_pid is not None:
    descendant = subprocess.Popen(
        [sys.executable, "-c", "import time; time.sleep(300)"],
        stdin=subprocess.DEVNULL,
    )
    args.descendant_pid.write_text(str(descendant.pid), encoding="ascii")

write_lock = threading.Lock()
cancelled: dict[int, threading.Event] = {}
list_count = 0


def send(message: dict[str, Any]) -> None:
    with write_lock:
        sys.stdout.write(json.dumps(message, separators=(",", ":")) + "\n")
        sys.stdout.flush()


def result(request_id: Any, value: Any) -> None:
    send({"jsonrpc": "2.0", "id": request_id, "result": value})


def error(request_id: Any, code: int, message: str) -> None:
    send(
        {
            "jsonrpc": "2.0",
            "id": request_id,
            "error": {"code": code, "message": message},
        }
    )


def tool(name: str, marker: str = "v1") -> dict[str, Any]:
    return {
        "name": name,
        "description": f"fixture {name} {marker}",
        "inputSchema": {
            "type": "object",
            "properties": {"value": {"type": "string"}},
            "required": ["value"],
            "additionalProperties": False,
        },
        "outputSchema": {
            "type": "object",
            "properties": {"value": {"type": "string"}, "version": {"type": "string"}},
            "required": ["value", "version"],
            "additionalProperties": False,
        },
        "annotations": {"readOnlyHint": True},
    }


def catalog() -> list[dict[str, Any]]:
    if args.scenario != "catalog":
        return [tool("echo")]
    if list_count <= 1:
        return [tool("versioned", "v1"), tool("removed", "v1")]
    if list_count == 2:
        changed = tool("versioned", "v2")
        changed["inputSchema"]["properties"]["extra"] = {"type": "integer"}
        return [changed, tool("added", "v1")]
    return [tool("versioned", "v2")]


def call_worker(request_id: int, params: dict[str, Any]) -> None:
    name = params.get("name")
    arguments = params.get("arguments", {})
    event = cancelled.setdefault(request_id, threading.Event())
    if args.scenario == "crash":
        os._exit(23)
    if args.scenario in {"timeout", "stable"} and name == "sleep":
        for _ in range(200):
            if event.wait(0.01):
                error(request_id, -32800, "cancelled")
                return
        error(request_id, -32001, "sleep finished unexpectedly")
        return
    if name == "remote_error":
        error(request_id, -32042, "untrusted server error text")
        return
    value = arguments.get("value", "") if isinstance(arguments, dict) else ""
    version = "v2" if list_count >= 2 and name == "versioned" else "v1"
    result(
        request_id,
        {
            "content": [{"type": "text", "text": f"{name}:{value}:{version}"}],
            "structuredContent": {"value": value, "version": version},
            "isError": False,
        },
    )


try:
    for line in sys.stdin:
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        method = message.get("method")
        request_id = message.get("id")
        params = message.get("params", {})
        if method == "initialize" and request_id is not None:
            if args.scenario == "logs":
                for index in range(200):
                    print(f"fixture log {index} SECRET_FIXTURE_VALUE", file=sys.stderr, flush=True)
            result(
                request_id,
                {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {"tools": {"listChanged": True}},
                    "serverInfo": {"name": "untrusted fixture", "version": "1"},
                },
            )
        elif method == "notifications/initialized":
            if args.scenario == "blocked":
                while True:
                    time.sleep(1)
            continue
        elif method == "tools/list" and request_id is not None:
            list_count += 1
            if args.scenario == "malformed":
                with write_lock:
                    sys.stdout.write("{not json}\n")
                    sys.stdout.flush()
                continue
            if args.scenario == "oversized":
                with write_lock:
                    sys.stdout.write("{\"padding\":\"")
                    sys.stdout.write("x" * args.oversized_bytes)
                    sys.stdout.write("\"}\n")
                    sys.stdout.flush()
                continue
            result(request_id, {"tools": catalog()})
        elif method == "tools/call" and request_id is not None:
            thread = threading.Thread(
                target=call_worker,
                args=(request_id, params if isinstance(params, dict) else {}),
                daemon=True,
            )
            thread.start()
        elif method == "notifications/cancelled":
            cancelled_id = params.get("requestId") if isinstance(params, dict) else None
            if isinstance(cancelled_id, int):
                cancelled.setdefault(cancelled_id, threading.Event()).set()
        elif request_id is not None:
            error(request_id, -32601, "method not found")
finally:
    if args.shutdown_marker is not None:
        args.shutdown_marker.write_text("closed\n", encoding="utf-8")
