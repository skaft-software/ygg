#!/usr/bin/env python3
"""Read-only Git helpers for Ygg's executable-extension protocol."""

import os
from pathlib import Path
import subprocess

from ygg_extension import Extension, RpcError


MAX_GIT_OUTPUT_BYTES = 256 * 1024
DEFAULT_MAX_ENTRIES = 80
MAX_ENTRIES = 200


ext = Extension()


def execution_workspace(context):
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


@ext.tool(
    name="git_status",
    description="Inspect the workspace Git status without acquiring optional locks",
    parameters={
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
)
def git_status(arguments, context):
    arguments = arguments or {}
    max_entries = bounded_integer(
        arguments.get("max_entries"), DEFAULT_MAX_ENTRIES, 1, MAX_ENTRIES
    )
    include_ignored = arguments.get("include_ignored", False)
    if not isinstance(include_ignored, bool):
        raise ValueError("include_ignored must be a boolean")
    try:
        status = run_git_status(
            execution_workspace(context),
            include_ignored=include_ignored,
            max_entries=max_entries,
        )
        return {"content": compact_status(status), "metadata": status}
    except (RuntimeError, ValueError) as error:
        return {"content": f"git_status failed: {error}", "is_error": True}


@ext.command(
    name="checkpoint",
    description="Preview a named, read-only workspace checkpoint",
    usage="/checkpoint [label]",
)
def checkpoint(arguments, context):
    label = " ".join(arguments).strip() or "working tree"
    try:
        status = run_git_status(execution_workspace(context))
        state = "clean" if status["clean"] else f"{status['total_entries']} changed paths"
        return {
            "text": f"Checkpoint preview · {label}\n{status['branch']} · {state}\n\n{compact_status(status)}",
            "notifications": [
                {
                    "level": "info",
                    "title": "Read-only checkpoint",
                    "message": "No commit or filesystem mutation was performed.",
                }
            ],
            "context": [],
        }
    except (RuntimeError, ValueError) as error:
        raise RpcError(-32001, f"checkpoint preview failed: {error}") from error


@ext.renderer("git_status")
def render_tool(params):
    output = params.get("output") or "git status pending"
    dirty = "state=dirty" in output or params.get("is_error", False)
    state_role = "extension.git_tools.error" if params.get("is_error", False) else (
        "extension.git_tools.dirty" if dirty else "extension.git_tools.clean"
    )
    headline = "git · attention" if dirty else "git · clean"
    return {
        "segments": [
            {"text": headline, "style_role": state_role},
            {"text": "\n", "style_role": None},
            {"text": output, "style_role": "extension.git_tools.detail"},
        ]
    }


if __name__ == "__main__":
    ext.run()
