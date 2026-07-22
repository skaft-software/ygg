#!/usr/bin/env python3
"""Read-only Git helpers for Ygg's executable-extension protocol."""

import json
import os
from pathlib import Path
import subprocess
import sys


API_VERSION = "0.1"
MAX_GIT_OUTPUT_BYTES = 256 * 1024
DEFAULT_MAX_ENTRIES = 80
MAX_ENTRIES = 200


def send(message):
    print(json.dumps(message, separators=(",", ":")), flush=True)


def result(request_id, value):
    send({"jsonrpc": "2.0", "id": request_id, "result": value})


def rpc_error(request_id, code, message):
    send(
        {
            "jsonrpc": "2.0",
            "id": request_id,
            "error": {"code": code, "message": message},
        }
    )


def execution_workspace(params):
    context = params.get("context") or {}
    value = context.get("workspace") or os.environ.get("YGG_WORKSPACE")
    if not value:
        raise ValueError("Ygg did not provide an active workspace")
    workspace = Path(value).resolve()
    if not workspace.is_dir():
        raise ValueError(f"workspace is not a directory: {workspace}")
    return workspace


def bounded_integer(value, default, minimum, maximum):
    if value is None:
        return default
    if isinstance(value, bool) or not isinstance(value, int):
        raise ValueError("max_entries must be an integer")
    if value < minimum or value > maximum:
        raise ValueError(f"max_entries must be between {minimum} and {maximum}")
    return value


def run_git_status(workspace, include_ignored=False, max_entries=DEFAULT_MAX_ENTRIES):
    command = [
        "git",
        "status",
        "--porcelain=v1",
        "--branch",
        "--untracked-files=all",
    ]
    if include_ignored:
        command.append("--ignored=matching")
    environment = os.environ.copy()
    environment.update(
        {
            "GIT_OPTIONAL_LOCKS": "0",
            "GIT_TERMINAL_PROMPT": "0",
            "LC_ALL": "C",
        }
    )
    try:
        completed = subprocess.run(
            command,
            cwd=workspace,
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=5,
            check=False,
        )
    except FileNotFoundError as error:
        raise RuntimeError("git executable was not found") from error
    except subprocess.TimeoutExpired as error:
        raise RuntimeError("git status exceeded the 5 second limit") from error

    if len(completed.stdout) > MAX_GIT_OUTPUT_BYTES:
        raise RuntimeError("git status exceeded the 256 KiB output limit")
    if completed.returncode != 0:
        detail = completed.stderr[:4096].decode("utf-8", errors="replace").strip()
        raise RuntimeError(detail or f"git status exited with {completed.returncode}")

    lines = completed.stdout.decode("utf-8", errors="replace").splitlines()
    branch = lines[0][3:] if lines and lines[0].startswith("## ") else "unknown"
    entries = lines[1:] if lines and lines[0].startswith("## ") else lines
    counts = {
        "staged": 0,
        "modified": 0,
        "untracked": 0,
        "ignored": 0,
        "conflicted": 0,
    }
    conflict_codes = {"DD", "AU", "UD", "UA", "DU", "AA", "UU"}
    for entry in entries:
        code = entry[:2]
        if code in conflict_codes:
            counts["conflicted"] += 1
        elif code == "??":
            counts["untracked"] += 1
        elif code == "!!":
            counts["ignored"] += 1
        else:
            if code[:1] not in {" ", "?", "!"}:
                counts["staged"] += 1
            if code[1:2] not in {" ", "?", "!"}:
                counts["modified"] += 1

    visible_entries = entries[:max_entries]
    return {
        "branch": branch,
        # Ignored paths are informative when explicitly requested, but do not
        # make an otherwise clean working tree dirty.
        "clean": not any(entry[:2] != "!!" for entry in entries),
        "counts": counts,
        "entries": visible_entries,
        "total_entries": len(entries),
        "truncated": len(entries) > len(visible_entries),
    }


def compact_status(status):
    lines = [
        f"branch={status['branch']}",
        f"state={'clean' if status['clean'] else 'dirty'}",
        "counts=" + ",".join(f"{key}:{value}" for key, value in status["counts"].items()),
    ]
    lines.extend(status["entries"])
    if status["truncated"]:
        omitted = status["total_entries"] - len(status["entries"])
        lines.append(f"... {omitted} additional entries omitted")
    return "\n".join(lines)


def initialize(request_id):
    result(
        request_id,
        {
            "api_version": API_VERSION,
            "tools": [
                {
                    "name": "git_status",
                    "description": "Inspect the workspace Git status without acquiring optional locks",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "include_ignored": {
                                "type": "boolean",
                                "description": "Include ignored paths in the bounded result",
                                "default": False,
                            },
                            "max_entries": {
                                "type": "integer",
                                "minimum": 1,
                                "maximum": MAX_ENTRIES,
                                "default": DEFAULT_MAX_ENTRIES,
                            },
                        },
                        "additionalProperties": False,
                    },
                }
            ],
            "commands": [
                {
                    "name": "checkpoint",
                    "description": "Preview a named, read-only workspace checkpoint",
                    "usage": "/checkpoint [label]",
                }
            ],
        },
    )


def call_tool(request_id, params):
    if params.get("name") != "git_status":
        rpc_error(request_id, -32601, "unknown tool")
        return
    arguments = params.get("arguments") or {}
    try:
        max_entries = bounded_integer(
            arguments.get("max_entries"), DEFAULT_MAX_ENTRIES, 1, MAX_ENTRIES
        )
        include_ignored = arguments.get("include_ignored", False)
        if not isinstance(include_ignored, bool):
            raise ValueError("include_ignored must be a boolean")
        status = run_git_status(
            execution_workspace(params),
            include_ignored=include_ignored,
            max_entries=max_entries,
        )
        result(
            request_id,
            {
                "content": compact_status(status),
                "is_error": False,
                "metadata": status,
            },
        )
    except (RuntimeError, ValueError) as error:
        result(
            request_id,
            {"content": f"git_status failed: {error}", "is_error": True, "metadata": {}},
        )


def execute_checkpoint(request_id, params):
    if params.get("name") != "checkpoint":
        rpc_error(request_id, -32601, "unknown command")
        return
    arguments = params.get("arguments") or []
    label = " ".join(arguments).strip() or "working tree"
    try:
        status = run_git_status(execution_workspace(params))
        state = "clean" if status["clean"] else f"{status['total_entries']} changed paths"
        result(
            request_id,
            {
                "text": f"Checkpoint preview · {label}\n{status['branch']} · {state}\n\n{compact_status(status)}",
                "notifications": [
                    {
                        "level": "info",
                        "title": "Read-only checkpoint",
                        "message": "No commit or filesystem mutation was performed.",
                    }
                ],
                "context": [],
            },
        )
    except (RuntimeError, ValueError) as error:
        rpc_error(request_id, -32001, f"checkpoint preview failed: {error}")


def render_tool(request_id, params):
    if params.get("name") != "git_status":
        rpc_error(request_id, -32601, "unknown tool renderer")
        return
    output = params.get("output") or "git status pending"
    dirty = "state=dirty" in output or params.get("is_error", False)
    state_role = "extension.git_tools.error" if params.get("is_error", False) else (
        "extension.git_tools.dirty" if dirty else "extension.git_tools.clean"
    )
    headline = "git · attention" if dirty else "git · clean"
    result(
        request_id,
        {
            "segments": [
                {"text": headline, "style_role": state_role},
                {"text": "\n", "style_role": None},
                {"text": output, "style_role": "extension.git_tools.detail"},
            ]
        },
    )


def handle(request):
    request_id = request.get("id")
    method = request.get("method")
    params = request.get("params") or {}
    if method == "initialize":
        initialize(request_id)
    elif method == "tool/call":
        call_tool(request_id, params)
    elif method == "command/execute":
        execute_checkpoint(request_id, params)
    elif method == "tool/render":
        render_tool(request_id, params)
    elif method == "shutdown":
        result(request_id, {})
        return False
    else:
        rpc_error(request_id, -32601, f"unknown method: {method}")
    return True


for line in sys.stdin:
    try:
        request = json.loads(line)
        if not handle(request):
            break
    except Exception as error:  # Protocol diagnostics must never use stdout.
        print(f"git-tools extension error: {error}", file=sys.stderr, flush=True)
