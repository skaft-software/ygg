#!/usr/bin/env python3
"""Minimal dependency-free Ygg executable extension."""

import json
import sys


def send(message):
    print(json.dumps(message, separators=(",", ":")), flush=True)


def result(request_id, value):
    send({"jsonrpc": "2.0", "id": request_id, "result": value})


def notify(message):
    send(
        {
            "jsonrpc": "2.0",
            "method": "notification",
            "params": {"level": "success", "message": message},
        }
    )


def handle(request):
    request_id = request.get("id")
    method = request.get("method")
    params = request.get("params", {})

    if method == "initialize":
        result(
            request_id,
            {
                "api_version": "0.1",
                "tools": [
                    {
                        "name": "hello_world",
                        "description": "Greet someone from an executable extension",
                        "parameters": {
                            "type": "object",
                            "properties": {"name": {"type": "string"}},
                            "required": ["name"],
                            "additionalProperties": False,
                        },
                    }
                ],
                "commands": [
                    {
                        "name": "hello",
                        "description": "Show a greeting notification",
                        "usage": "/hello [name]",
                    }
                ],
            },
        )
    elif method == "tool/call":
        name = params.get("arguments", {}).get("name", "tinkerer")
        notify(f"hello_world greeted {name}")
        result(
            request_id,
            {"content": f"Hello, {name}!", "is_error": False, "metadata": {}},
        )
    elif method == "command/execute":
        arguments = params.get("arguments", [])
        name = arguments[0] if arguments else "tinkerer"
        result(
            request_id,
            {
                "text": f"Hello, {name}!",
                "notifications": [],
                "context": [],
            },
        )
    elif method == "hook/run":
        result(
            request_id,
            {"disposition": {"action": "continue"}, "context": [], "notifications": []},
        )
    elif method == "context/collect":
        result(
            request_id,
            [
                {
                    "label": "hello-world",
                    "content": "The hello-world extension is active.",
                    "placement": "system_suffix",
                }
            ],
        )
    elif method == "status/collect":
        result(
            request_id,
            {
                "surface": "status",
                "text": "hello",
                "style_role": "extension.hello_world.status",
                "priority": 0,
            },
        )
    elif method == "tool/render":
        output = params.get("output") or "waiting"
        result(
            request_id,
            {
                "segments": [
                    {
                        "text": f"hello_world · {output}",
                        "style_role": "extension.hello_world.tool",
                    }
                ]
            },
        )
    elif method == "shutdown":
        result(request_id, {})
        return False
    else:
        send(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {"code": -32601, "message": f"unknown method: {method}"},
            }
        )
    return True


for line in sys.stdin:
    try:
        request = json.loads(line)
        if not handle(request):
            break
    except Exception as error:  # Protocol diagnostics belong on stderr.
        print(f"hello-world extension error: {error}", file=sys.stderr, flush=True)
