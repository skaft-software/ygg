"""Hermetic subprocess harness for the ygg-pi-compat bridge."""

from __future__ import annotations

import hashlib
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


LOCK_FILES = ("package-lock.json", "npm-shrinkwrap.json", "pnpm-lock.yaml", "yarn.lock", "bun.lockb")
SKIPPED_DIRECTORIES = {".git", ".pytest_cache", "__pycache__", "node_modules", "target"}


def _frame(hasher: "hashlib._Hash", value: bytes | str) -> None:
    data = value.encode() if isinstance(value, str) else value
    hasher.update(len(data).to_bytes(8, "big"))
    hasher.update(data)


def compute_source_fingerprint(path: Path) -> str:
    path = path.resolve()
    entries: list[tuple[str, str, Path | None]] = []
    if path.is_file():
        root_tag = "f"
        entries.append(("f", ".", path))
    else:
        root_tag = "d"
        for current, directories, files in os.walk(path):
            directories[:] = sorted(directory for directory in directories if directory not in SKIPPED_DIRECTORIES)
            for directory in directories:
                entry = Path(current) / directory
                entries.append(("d", entry.relative_to(path).as_posix(), None))
            for filename in sorted(files):
                entry = Path(current) / filename
                entries.append(("f", entry.relative_to(path).as_posix(), entry))
    entries.sort(key=lambda entry: (entry[1].encode(), entry[0].encode()))
    digest = hashlib.sha256()
    digest.update(b"ygg-pi-source-fingerprint\0")
    digest.update((1).to_bytes(4, "big"))
    digest.update(root_tag.encode())
    for tag, relative, file_path in entries:
        digest.update(tag.encode())
        _frame(digest, relative)
        if file_path is not None:
            data = file_path.read_bytes()
            digest.update(len(data).to_bytes(8, "big"))
            digest.update(data)
    return digest.hexdigest()


def source_lock_fingerprint(path: Path) -> str:
    path = path.resolve()
    root = path if path.is_dir() else path.parent
    entries = [(name, root / name) for name in LOCK_FILES if (root / name).is_file()]
    digest = hashlib.sha256()
    digest.update(b"ygg-pi-source-lock-fingerprint\0")
    digest.update((1).to_bytes(4, "big"))
    digest.update(len(entries).to_bytes(4, "big"))
    for name, lock_path in entries:
        _frame(digest, name)
        data = lock_path.read_bytes()
        digest.update(len(data).to_bytes(8, "big"))
        digest.update(data)
    return digest.hexdigest()


def runtime_integrity(pi_package: Path) -> str:
    root = pi_package.resolve()
    digest = hashlib.sha256()
    digest.update(b"ygg-pi-runtime-integrity\0")
    digest.update((1).to_bytes(4, "big"))
    _frame(digest, (root / "package.json").read_bytes())
    _frame(digest, compute_source_fingerprint(root / "dist"))
    return digest.hexdigest()


def link_identity(
    *,
    extensions: list[Path],
    source_hashes: list[str],
    lock_hashes: list[str],
    pi_package: Path,
    pi_runtime_integrity: str,
    aggregate_digest: str,
    manifest_path: Path,
    command_name: str,
    ygg_version: str,
    agent_dir: Path,
) -> str:
    digest = hashlib.sha256()
    digest.update(b"ygg-pi-aggregate-link-identity\0")
    digest.update((1).to_bytes(4, "big"))
    for value in (
        "0.3.0",
        "0.84.4",
        ygg_version,
        command_name,
        os.path.abspath(manifest_path),
        str(pi_package.resolve()),
        pi_runtime_integrity,
        aggregate_digest,
        "explicit_enable_and_trust_required",
        os.path.abspath(agent_dir),
    ):
        _frame(digest, value)
    digest.update(len(extensions).to_bytes(4, "big"))
    for extension, source_hash, lock_hash in zip(extensions, source_hashes, lock_hashes, strict=True):
        _frame(digest, os.path.abspath(extension))
        _frame(digest, source_hash)
        _frame(digest, lock_hash)
    return digest.hexdigest()


class BridgeProcess:
    """Line-oriented JSON-RPC peer which drains both subprocess pipes."""

    def __init__(
        self,
        *,
        pi_package: Path = FAKE_PI,
        extension: Path = FIXTURE_EXTENSION,
        extensions: list[Path] | None = None,
        source_fingerprint: str | None = None,
        fixture_mode: str | None = None,
        fixture_events: list[str] | None = None,
        strict_identity: bool = False,
        aggregate_digest: str = "a" * 64,
        command_name: str = "pi",
        agent_dir: Path | None = None,
        manifest_path: Path | None = None,
        ygg_version: str = "0.6.7",
    ) -> None:
        if NODE is None:
            raise RuntimeError("node is unavailable")
        environment = os.environ.copy()
        environment.pop("YGG_PI_FIXTURE_MODE", None)
        environment.pop("YGG_PI_FIXTURE_EVENTS", None)
        if fixture_mode is not None:
            environment["YGG_PI_FIXTURE_MODE"] = fixture_mode
        if fixture_events is not None:
            environment["YGG_PI_FIXTURE_EVENTS"] = ",".join(fixture_events)
        selected_extensions = [Path(item).resolve() for item in (extensions or [extension])]
        selected_agent_dir = Path(agent_dir or (ROOT / ".test-pi-agent")).absolute()
        selected_manifest = Path(manifest_path or (ROOT / ".test-link" / "extension.toml")).absolute()
        command = [NODE, str(BRIDGE)]
        for selected in selected_extensions:
            command.extend(["--extension", str(selected)])
        if strict_identity:
            source_hashes = [source_fingerprint or compute_source_fingerprint(selected) for selected in selected_extensions]
            lock_hashes = [source_lock_fingerprint(selected) for selected in selected_extensions]
            runtime_hash = runtime_integrity(pi_package)
            identity = link_identity(
                extensions=selected_extensions,
                source_hashes=source_hashes,
                lock_hashes=lock_hashes,
                pi_package=pi_package,
                pi_runtime_integrity=runtime_hash,
                aggregate_digest=aggregate_digest,
                manifest_path=selected_manifest,
                command_name=command_name,
                ygg_version=ygg_version,
                agent_dir=selected_agent_dir,
            )
            for source_hash in source_hashes:
                command.extend(["--source-fingerprint", source_hash])
            for lock_hash in lock_hashes:
                command.extend(["--source-lock-fingerprint", lock_hash])
            command.extend([
                "--agent-dir", str(selected_agent_dir),
                "--pi-package", str(pi_package),
                "--pi-runtime-integrity", runtime_hash,
                "--aggregate-digest", aggregate_digest,
                "--link-manifest", str(selected_manifest),
                "--link-identity", identity,
                "--ygg-version", ygg_version,
                "--command", command_name,
            ])
        else:
            if source_fingerprint is not None:
                command.extend(["--source-fingerprint", source_fingerprint])
            command.extend(["--pi-package", str(pi_package), "--command", command_name])
        self.strict_identity = strict_identity
        self.command_name = command_name
        self.manifest_path = selected_manifest
        self.ygg_version = ygg_version
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
                **(
                    {
                        "ygg_version": self.ygg_version,
                        "extension": {
                            "name": self.command_name,
                            "version": "fixture",
                            "manifest_path": str(self.manifest_path),
                            "source": "explicit",
                        },
                    }
                    if self.strict_identity
                    else {}
                ),
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
