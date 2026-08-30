"""Hermetic subprocess harness for the ygg-pi-compat bridge."""

from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import subprocess
import threading
import time
from typing import Any, Callable


ROOT = Path(__file__).resolve().parents[1]
FIXTURES = Path(__file__).resolve().parent / "fixtures"
BRIDGE = ROOT / "bridge.mjs"
FAKE_PI = FIXTURES / "fake-pi"
FIXTURE_EXTENSION = FIXTURES / "fixture-extension.mjs"
NODE = shutil.which("node")


class BridgeProcess:
    """Line-oriented JSON-RPC peer which drains both subprocess pipes."""

    def __init__(
        self,
        *,
        pi_package: Path = FAKE_PI,
        extension: Path = FIXTURE_EXTENSION,
        source_fingerprint: str | None = None,
    ) -> None:
        if NODE is None:
            raise RuntimeError("node is unavailable")
        environment = os.environ.copy()
        command = [
            NODE,
            str(BRIDGE),
            "--extension",
            str(extension),
        ]
        if source_fingerprint is not None:
            command.extend(["--source-fingerprint", source_fingerprint])
        command.extend(["--pi-package", str(pi_package)])
        self.process = subprocess.Popen(
            command,
            cwd=ROOT,
            env=environment,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            bufsize=1,
        )
        self.messages: list[dict[str, Any]] = []
        self.stderr: list[str] = []
        self.protocol_errors: list[str] = []
        self.handlers: dict[str, Callable[[dict[str, Any]], Any]] = {}
        self.initialized = False
        self._condition = threading.Condition()
        self._write_lock = threading.Lock()
        self._next_id = 1
        self._stdout_thread = threading.Thread(target=self._read_stdout, daemon=True)
        self._stderr_thread = threading.Thread(target=self._read_stderr, daemon=True)
        self._stdout_thread.start()
        self._stderr_thread.start()

    def _read_stdout(self) -> None:
        assert self.process.stdout is not None
        for line in self.process.stdout:
            try:
                message = json.loads(line)
                if not isinstance(message, dict):
                    raise ValueError("JSON-RPC line was not an object")
            except (json.JSONDecodeError, ValueError) as error:
                with self._condition:
                    self.protocol_errors.append(f"{error}: {line.rstrip()}")
                    self._condition.notify_all()
                continue
            with self._condition:
                self.messages.append(message)
                self._condition.notify_all()
            method = message.get("method")
            if method in self.handlers and "id" in message:
                try:
                    result = self.handlers[method](message)
                    self.send({"jsonrpc": "2.0", "id": message["id"], "result": result})
                except Exception as error:  # pragma: no cover - fixture diagnosis
                    self.send(
                        {
                            "jsonrpc": "2.0",
                            "id": message["id"],
                            "error": {"code": -32000, "message": str(error)},
                        }
                    )

    def _read_stderr(self) -> None:
        assert self.process.stderr is not None
        for line in self.process.stderr:
            with self._condition:
                self.stderr.append(line.rstrip("\n"))
                self._condition.notify_all()

    def send(self, message: dict[str, Any]) -> None:
        assert self.process.stdin is not None
        line = json.dumps(message, separators=(",", ":")) + "\n"
        with self._write_lock:
            self.process.stdin.write(line)
            self.process.stdin.flush()

    def send_request(self, method: str, params: dict[str, Any] | None = None) -> int:
        request_id = self._next_id
        self._next_id += 1
        self.send(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "method": method,
                "params": params or {},
            }
        )
        return request_id

    def request(
        self, method: str, params: dict[str, Any] | None = None, timeout: float = 3.0
    ) -> dict[str, Any]:
        return self.wait_response(self.send_request(method, params), timeout=timeout)

    def wait_response(self, request_id: int, timeout: float = 3.0) -> dict[str, Any]:
        return self.wait_for(
            lambda messages: next(
                (message for message in messages if message.get("id") == request_id and "method" not in message),
                None,
            ),
            timeout=timeout,
            description=f"response {request_id}",
        )

    def wait_for(
        self,
        predicate: Callable[[list[dict[str, Any]]], Any],
        *,
        timeout: float = 3.0,
        description: str = "protocol message",
    ) -> Any:
        deadline = time.monotonic() + timeout
        with self._condition:
            while True:
                result = predicate(list(self.messages))
                if result:
                    return result
                if self.process.poll() is not None:
                    raise AssertionError(
                        f"bridge exited {self.process.returncode} waiting for {description}; "
                        f"stderr={self.stderr!r}; protocol_errors={self.protocol_errors!r}"
                    )
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise AssertionError(
                        f"timed out waiting for {description}; messages={self.messages!r}; "
                        f"stderr={self.stderr!r}; protocol_errors={self.protocol_errors!r}"
                    )
                self._condition.wait(min(remaining, 0.05))

    def initialize(
        self,
        *optional_features: str,
        host: dict[str, Any] | None = None,
    ) -> dict[str, Any]:
        response = self.request(
            "initialize",
            {
                "workspace": str(ROOT),
                "host": host or {},
                "protocol": {"optional_features": list(optional_features)},
            },
        )
        if "error" in response:
            raise AssertionError(f"initialize failed: {response}")
        self.initialized = True
        return response["result"]

    def notifications(self) -> list[str]:
        return [
            str(message.get("params", {}).get("message", ""))
            for message in self.messages
            if message.get("method") == "notification"
        ]

    def close(self) -> None:
        if self.process.poll() is None and self.initialized:
            try:
                self.request("shutdown", timeout=1.0)
            except (AssertionError, BrokenPipeError):
                self.process.terminate()
        elif self.process.poll() is None:
            self.process.terminate()
        try:
            self.process.wait(timeout=2.0)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=2.0)
        self._stdout_thread.join(timeout=1.0)
        self._stderr_thread.join(timeout=1.0)
        for stream in (self.process.stdin, self.process.stdout, self.process.stderr):
            if stream is not None:
                stream.close()

    def __enter__(self) -> "BridgeProcess":
        return self

    def __exit__(self, *_exc: object) -> None:
        self.close()
