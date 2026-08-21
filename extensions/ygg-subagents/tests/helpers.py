"""Shared package test helpers."""

from __future__ import annotations

import json
from pathlib import Path
import queue
import sys
import threading
import time
from typing import Any, Callable, Mapping, Optional


ROOT = Path(__file__).resolve().parents[1]
REPOSITORY = ROOT.parents[1]
VENDOR = ROOT / "vendor"
FIXTURES = ROOT / "fixtures"
for path in (str(VENDOR), str(ROOT), str(FIXTURES)):
    if path not in sys.path:
        sys.path.insert(0, path)


class FakeCancellation:
    def __init__(self, *, cancel_after_checks: Optional[int] = None) -> None:
        self.cancel_after_checks = cancel_after_checks
        self.checks = 0
        self.cancelled = False

    def cancel(self) -> None:
        self.cancelled = True

    def raise_if_cancelled(self) -> None:
        self.checks += 1
        if self.cancel_after_checks is not None and self.checks > self.cancel_after_checks:
            self.cancelled = True
        if self.cancelled:
            from ygg_extension import CancelledError

            raise CancelledError("fixture")


def owner(
    *,
    session: str = "owner-a",
    instance: str = "instance-a",
    generation: int = 1,
    host_session: str = "parent-session",
):
    from ygg_subagents.model import Owner

    return Owner(
        session_id=session,
        extension_instance_id=instance,
        process_generation=generation,
        host_session_id=host_session,
        workspace="/workspace",
        inherited_model="claude-sonnet-test",
    )


def rpc_request(request_id: Any, method: str, params: Optional[Mapping[str, Any]] = None):
    return {
        "jsonrpc": "2.0",
        "id": request_id,
        "method": method,
        "params": {} if params is None else dict(params),
    }


def initialize_request(*, agent_sessions: bool = True):
    optional = ["lifecycle_events"]
    required = ["request_cancellation", "content_parts"]
    if agent_sessions:
        optional.append("agent_sessions")
        required.append("delegation_telemetry_v1")
    return rpc_request(
        1,
        "initialize",
        {
            "api_version": "0.2",
            "workspace": "/workspace",
            "contributes": {
                "tools": [
                    "subagent_spawn",
                    "subagent_status",
                    "subagent_wait",
                    "subagent_stop",
                    "subagent_continue",
                ],
                "commands": ["subagents"],
                "ui": ["status"],
                "presentation": True,
            },
            "host": {
                "session_id": "parent-session",
                "model": "claude-sonnet-test",
                "active_skills": [],
            },
            "protocol": {
                "version": "0.2",
                "required_features": required,
                "optional_features": optional,
                "limits": {"max_concurrent_requests": 8},
            },
        },
    )


def tool_context(*, generation: int = 1, session: str = "owner-a"):
    return {
        "workspace": "/workspace",
        "resource_owner": {
            "session_id": session,
            "extension_instance_id": "instance-a",
            "process_generation": generation,
        },
        "host": {
            "session_id": "parent-session",
            "model": "claude-sonnet-test",
            "active_skills": [],
        },
    }


class QueueReader:
    _EOF = object()

    def __init__(self) -> None:
        self._lines: "queue.Queue[Any]" = queue.Queue()

    def feed(self, message: Mapping[str, Any]) -> None:
        self._lines.put(json.dumps(message, separators=(",", ":")) + "\n")

    def close(self) -> None:
        self._lines.put(self._EOF)

    def readline(self) -> str:
        value = self._lines.get()
        return "" if value is self._EOF else value


class RecordingWriter:
    def __init__(
        self,
        reader: QueueReader,
        responder: Optional[Callable[[Mapping[str, Any]], Optional[Mapping[str, Any]]]] = None,
    ) -> None:
        self.reader = reader
        self.responder = responder
        self.messages: list[dict[str, Any]] = []
        self._condition = threading.Condition()
        self._writing = False
        self.concurrent_write = False

    def write(self, value: str) -> int:
        with self._condition:
            if self._writing:
                self.concurrent_write = True
            self._writing = True
        try:
            decoded = [json.loads(line) for line in value.splitlines() if line]
            with self._condition:
                self.messages.extend(decoded)
                self._condition.notify_all()
            for message in decoded:
                if self.responder is not None:
                    response = self.responder(message)
                    if response is not None:
                        self.reader.feed(response)
            return len(value)
        finally:
            with self._condition:
                self._writing = False

    def flush(self) -> None:
        return None

    def wait_for(self, predicate: Callable[[Mapping[str, Any]], bool], timeout: float = 3.0):
        deadline = time.monotonic() + timeout
        with self._condition:
            while True:
                for message in self.messages:
                    if predicate(message):
                        return message
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise AssertionError("timed out; protocol messages=%r" % self.messages)
                self._condition.wait(remaining)

    def matching(self, predicate: Callable[[Mapping[str, Any]], bool]):
        with self._condition:
            return [message for message in self.messages if predicate(message)]


class RunningExtension:
    def __init__(self, extension: Any, responder: Optional[Callable] = None) -> None:
        self.extension = extension
        self.reader = QueueReader()
        self.writer = RecordingWriter(self.reader, responder)
        self.thread = threading.Thread(
            target=extension.run,
            kwargs={"stdin": self.reader, "stdout": self.writer},
            daemon=True,
        )

    def start(self, initialize: Optional[Mapping[str, Any]] = None):
        self.thread.start()
        self.reader.feed(initialize or initialize_request())
        return self.writer.wait_for(lambda message: message.get("id") == 1)

    def shutdown(self, request_id: int = 900):
        self.reader.feed(rpc_request(request_id, "shutdown", {}))
        response = self.writer.wait_for(lambda message: message.get("id") == request_id)
        self.reader.close()
        self.thread.join(timeout=3.0)
        if self.thread.is_alive():
            raise AssertionError("extension did not stop")
        return response
