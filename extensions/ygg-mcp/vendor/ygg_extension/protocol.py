"""Small standard-library JSON-RPC transport used by :mod:`ygg_extension`."""

from __future__ import annotations

import json
import sys
import threading
from typing import Any, IO, Mapping, Optional


DEFAULT_API_VERSION = "0.1"
DEFAULT_MAX_MESSAGE_BYTES = 1024 * 1024


class RpcError(Exception):
    """A JSON-RPC error that can be returned to the Ygg host."""

    def __init__(self, code: int, message: str, data: Any = None) -> None:
        super().__init__(message)
        self.code = code
        self.message = message
        self.data = data

    def error_object(self) -> dict[str, Any]:
        error: dict[str, Any] = {"code": self.code, "message": self.message}
        if self.data is not None:
            error["data"] = self.data
        return error

    @classmethod
    def from_response(cls, response: Mapping[str, Any]) -> "RpcError":
        error = response.get("error")
        if not isinstance(error, Mapping):
            return cls(-32603, "invalid JSON-RPC error response")
        code = error.get("code", -32603)
        message = error.get("message", "remote JSON-RPC error")
        if not isinstance(code, int) or isinstance(code, bool):
            code = -32603
        if not isinstance(message, str):
            message = str(message)
        return cls(code, message, error.get("data"))


class ProtocolError(RpcError):
    """A malformed or otherwise invalid JSON-RPC message."""


class Logger:
    """Emit compact structured JSON diagnostics to stderr.

    The SDK never writes diagnostics to the protocol stream.  ``fields`` must
    be JSON-compatible; unknown values are represented with ``str`` so logging
    an exception cannot itself break an extension.
    """

    _LEVELS = {"debug": 10, "info": 20, "warning": 30, "error": 40}

    def __init__(
        self,
        stream: Optional[IO[Any]] = None,
        *,
        level: str = "info",
    ) -> None:
        level = level.lower()
        if level not in self._LEVELS:
            raise ValueError(f"unknown log level: {level}")
        self.stream = stream if stream is not None else sys.stderr
        self.level = level
        self._lock = threading.Lock()

    def log(self, level: str, message: str, **fields: Any) -> None:
        level = level.lower()
        if level not in self._LEVELS:
            raise ValueError(f"unknown log level: {level}")
        if self._LEVELS[level] < self._LEVELS[self.level]:
            return
        entry: dict[str, Any] = {"level": level, "message": str(message)}
        entry.update(fields)
        encoded = json.dumps(
            entry,
            separators=(",", ":"),
            ensure_ascii=False,
            default=str,
        ) + "\n"
        with self._lock:
            try:
                self.stream.write(encoded)
            except TypeError:
                # BytesIO and binary stderr adapters are useful in tests and
                # are cheap to support without affecting normal text streams.
                self.stream.write(encoded.encode("utf-8"))
            self.stream.flush()

    def debug(self, message: str, **fields: Any) -> None:
        self.log("debug", message, **fields)

    def info(self, message: str, **fields: Any) -> None:
        self.log("info", message, **fields)

    def warning(self, message: str, **fields: Any) -> None:
        self.log("warning", message, **fields)

    def error(self, message: str, **fields: Any) -> None:
        self.log("error", message, **fields)


class JsonRpcTransport:
    """Read and write one compact JSON object per line."""

    def __init__(
        self,
        reader: IO[Any],
        writer: IO[Any],
        *,
        max_message_bytes: int = DEFAULT_MAX_MESSAGE_BYTES,
    ) -> None:
        if max_message_bytes <= 0:
            raise ValueError("max_message_bytes must be greater than zero")
        self.reader = reader
        self.writer = writer
        self.max_message_bytes = max_message_bytes
        self._write_lock = threading.Lock()

    def send(self, message: Mapping[str, Any]) -> None:
        if not isinstance(message, Mapping):
            raise TypeError("JSON-RPC messages must be objects")
        try:
            encoded = json.dumps(
                dict(message),
                separators=(",", ":"),
                ensure_ascii=False,
                allow_nan=False,
            ) + "\n"
        except (TypeError, ValueError) as error:
            raise ProtocolError(-32603, f"cannot serialize JSON-RPC message: {error}") from error
        if len(encoded.encode("utf-8")) - 1 > self.max_message_bytes:
            raise ProtocolError(
                -32603,
                f"JSON-RPC message exceeds {self.max_message_bytes} bytes",
            )
        with self._write_lock:
            try:
                self.writer.write(encoded)
            except TypeError:
                self.writer.write(encoded.encode("utf-8"))
            self.writer.flush()

    def read(self) -> Optional[dict[str, Any]]:
        """Read the next message, returning ``None`` on clean EOF."""

        while True:
            line = self.reader.readline()
            if line is None or line == "" or line == b"":
                return None
            if isinstance(line, bytes):
                raw = line.rstrip(b"\r\n")
                try:
                    text = raw.decode("utf-8")
                except UnicodeDecodeError as error:
                    raise ProtocolError(-32700, f"invalid UTF-8: {error}") from error
            else:
                text = str(line).rstrip("\r\n")
                raw = text.encode("utf-8")
            if len(raw) > self.max_message_bytes:
                raise ProtocolError(
                    -32700,
                    f"JSON-RPC message exceeds {self.max_message_bytes} bytes",
                )
            if not text.strip():
                continue
            try:
                value = json.loads(text)
            except json.JSONDecodeError as error:
                raise ProtocolError(-32700, f"invalid JSON: {error.msg}") from error
            if not isinstance(value, dict):
                raise ProtocolError(-32600, "JSON-RPC message must be an object")
            if value.get("jsonrpc") != "2.0":
                raise ProtocolError(-32600, "JSON-RPC message must set jsonrpc to 2.0")
            return value
