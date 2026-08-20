#!/usr/bin/env python3
"""Deterministic fake OpenSSH CLI used only by ygg-ssh package tests."""

from __future__ import annotations

import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import time
from typing import Optional


def option_value(arguments: list[str], name: str) -> Optional[str]:
    for index, value in enumerate(arguments):
        if value == name and index + 1 < len(arguments):
            return arguments[index + 1]
        if name == "-o" and value.startswith("ControlPath="):
            return value.split("=", 1)[1]
    return None


def control_path(arguments: list[str]) -> Path:
    direct = option_value(arguments, "-S")
    if direct:
        return Path(direct)
    for index, value in enumerate(arguments):
        if value == "-o" and index + 1 < len(arguments):
            candidate = arguments[index + 1]
            if candidate.startswith("ControlPath="):
                return Path(candidate.split("=", 1)[1])
    raise SystemExit(2)


def destination(arguments: list[str]) -> tuple[str, Optional[str]]:
    try:
        marker = arguments.index("--")
    except ValueError:
        return "unknown", None
    alias = arguments[marker + 1] if marker + 1 < len(arguments) else "unknown"
    command = arguments[marker + 2] if marker + 2 < len(arguments) else None
    return alias, command


def state_path(path: Path) -> Path:
    return Path(str(path) + ".fake-master")


def log_event(kind: str, alias: str, arguments: list[str], **fields: object) -> None:
    log = os.environ.get("YGG_SSH_FAKE_LOG")
    if not log:
        return
    record = {
        "kind": kind,
        "alias": alias,
        "agent_available": bool(os.environ.get("SSH_AUTH_SOCK")),
        **fields,
    }
    with Path(log).open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(record, sort_keys=True) + "\n")


def pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except OSError:
        return False
    return True


def run_master(arguments: list[str], alias: str, path: Path) -> int:
    log_event("master", alias, arguments)
    banner = os.environ.get("YGG_SSH_FAKE_BANNER")
    if banner:
        print(banner, file=sys.stderr, flush=True)
    if os.environ.get("YGG_SSH_FAKE_CONNECT_FAIL") == "1":
        return 255
    state = state_path(path)
    state.parent.mkdir(parents=True, exist_ok=True)
    state.write_text(json.dumps({"pid": os.getpid(), "alias": alias}), encoding="utf-8")

    def stop(_signum: int, _frame: object) -> None:
        state.unlink(missing_ok=True)
        raise SystemExit(0)

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    try:
        while True:
            time.sleep(0.1)
    finally:
        state.unlink(missing_ok=True)


def run_control(arguments: list[str], alias: str, path: Path, operation: str) -> int:
    state = state_path(path)
    log_event("control", alias, arguments, operation=operation)
    try:
        value = json.loads(state.read_text(encoding="utf-8"))
        pid = int(value["pid"])
    except (OSError, ValueError, KeyError, json.JSONDecodeError):
        return 255
    if operation == "check":
        fail_flag = os.environ.get("YGG_SSH_FAKE_HEALTH_FAIL")
        return 255 if fail_flag == "1" or not pid_alive(pid) else 0
    if operation == "exit":
        try:
            os.kill(pid, signal.SIGTERM)
        except OSError:
            pass
        state.unlink(missing_ok=True)
        return 0
    return 2


def run_command(arguments: list[str], alias: str, path: Path, command: str) -> int:
    log_event("command", alias, arguments, remote_command_bytes=len(command.encode("utf-8")))
    state = state_path(path)
    if not state.exists():
        return 255
    if "fake-disconnect" in command:
        state.unlink(missing_ok=True)
        print("partial untrusted output", flush=True)
        return 255
    if "fake-descendant" in command:
        child = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(300)"],
            stdin=subprocess.DEVNULL,
        )
        pid_file = os.environ.get("YGG_SSH_FAKE_DESCENDANT_PID")
        if pid_file:
            Path(pid_file).write_text(str(child.pid), encoding="ascii")
        while True:
            time.sleep(1)
    match = re.search(r"fake-output:([0-9]+)", command)
    if match:
        sys.stdout.write("x" * int(match.group(1)))
        sys.stdout.flush()
        return 0
    # Execute the package-authored, shell-quoted remote command locally. Tests
    # point remoteCwd at a temporary fixture directory.
    os.execv("/bin/sh", ["sh", "-c", command])
    return 127


def main() -> int:
    arguments = sys.argv[1:]
    alias, command = destination(arguments)
    path = control_path(arguments)
    operation = option_value(arguments, "-O")
    if operation:
        return run_control(arguments, alias, path, operation)
    if "-M" in arguments:
        return run_master(arguments, alias, path)
    if command is None:
        return 2
    return run_command(arguments, alias, path, command)


if __name__ == "__main__":
    raise SystemExit(main())
