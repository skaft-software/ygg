"""Bounded JSON-RPC stdio client for MCP servers.

This module deliberately implements the MCP client boundary inside the package;
the Ygg kernel never parses MCP frames or owns an MCP process.
"""

from __future__ import annotations

from collections import deque
from dataclasses import dataclass
import json
import os
import select
import signal
import subprocess
import threading
import time
from typing import Any, Callable, Mapping, Optional

if os.name == "nt":  # pragma: no cover - exercised by Windows release CI
    import ctypes
    from ctypes import wintypes


    class _JobBasicLimitInformation(ctypes.Structure):
        _fields_ = [
            ("PerProcessUserTimeLimit", ctypes.c_longlong),
            ("PerJobUserTimeLimit", ctypes.c_longlong),
            ("LimitFlags", wintypes.DWORD),
            ("MinimumWorkingSetSize", ctypes.c_size_t),
            ("MaximumWorkingSetSize", ctypes.c_size_t),
            ("ActiveProcessLimit", wintypes.DWORD),
            ("Affinity", ctypes.c_size_t),
            ("PriorityClass", wintypes.DWORD),
            ("SchedulingClass", wintypes.DWORD),
        ]


    class _IoCounters(ctypes.Structure):
        _fields_ = [(name, ctypes.c_ulonglong) for name in (
            "ReadOperationCount",
            "WriteOperationCount",
            "OtherOperationCount",
            "ReadTransferCount",
            "WriteTransferCount",
            "OtherTransferCount",
        )]


    class _JobExtendedLimitInformation(ctypes.Structure):
        _fields_ = [
            ("BasicLimitInformation", _JobBasicLimitInformation),
            ("IoInfo", _IoCounters),
            ("ProcessMemoryLimit", ctypes.c_size_t),
            ("JobMemoryLimit", ctypes.c_size_t),
            ("PeakProcessMemoryUsed", ctypes.c_size_t),
            ("PeakJobMemoryUsed", ctypes.c_size_t),
        ]


    class _WindowsJob:
        _KILL_ON_CLOSE = 0x00002000
        _EXTENDED_LIMIT_INFORMATION = 9

        def __init__(self, process: subprocess.Popen[bytes]) -> None:
            kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
            kernel32.CreateJobObjectW.argtypes = [ctypes.c_void_p, wintypes.LPCWSTR]
            kernel32.CreateJobObjectW.restype = wintypes.HANDLE
            kernel32.SetInformationJobObject.argtypes = [
                wintypes.HANDLE,
                ctypes.c_int,
                ctypes.c_void_p,
                wintypes.DWORD,
            ]
            kernel32.SetInformationJobObject.restype = wintypes.BOOL
            kernel32.AssignProcessToJobObject.argtypes = [wintypes.HANDLE, wintypes.HANDLE]
            kernel32.AssignProcessToJobObject.restype = wintypes.BOOL
            kernel32.TerminateJobObject.argtypes = [wintypes.HANDLE, wintypes.UINT]
            kernel32.TerminateJobObject.restype = wintypes.BOOL
            kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
            kernel32.CloseHandle.restype = wintypes.BOOL
            self._kernel32 = kernel32
            self._handle = kernel32.CreateJobObjectW(None, None)
            if not self._handle:
                raise OSError(ctypes.get_last_error(), "CreateJobObjectW failed")
            limits = _JobExtendedLimitInformation()
            limits.BasicLimitInformation.LimitFlags = self._KILL_ON_CLOSE
            if not kernel32.SetInformationJobObject(
                self._handle,
                self._EXTENDED_LIMIT_INFORMATION,
                ctypes.byref(limits),
                ctypes.sizeof(limits),
            ) or not kernel32.AssignProcessToJobObject(self._handle, int(process._handle)):
                error = ctypes.get_last_error()
                kernel32.CloseHandle(self._handle)
                self._handle = None
                raise OSError(error, "could not assign MCP server to a kill-on-close job")

        def terminate(self) -> None:
            if self._handle:
                self._kernel32.TerminateJobObject(self._handle, 1)

        def close(self) -> None:
            if self._handle:
                self._kernel32.CloseHandle(self._handle)
                self._handle = None


from .config import Limits, ServerConfig


MCP_PROTOCOL_VERSION = "2025-06-18"
SUPPORTED_PROTOCOL_VERSIONS = frozenset(
    {MCP_PROTOCOL_VERSION, "2025-03-26", "2024-11-05"}
)
CLIENT_NAME = "ygg-mcp"
CLIENT_VERSION = "0.1.0"
MAX_CURSOR_BYTES = 1024
MAX_TOMBSTONES = 256


class McpError(RuntimeError):
    """A safe, classified bridge error.

    ``safe_summary`` is bridge-authored and may be used in health state. Raw
    server text is intentionally not carried by this type.
    """

    def __init__(
        self,
        code: str,
        safe_summary: str,
        *,
        permanent: bool = False,
        ambiguous: bool = False,
        retry_after_ms: Optional[int] = None,
    ) -> None:
        super().__init__(safe_summary)
        self.code = code
        self.safe_summary = safe_summary
        self.permanent = permanent
        self.ambiguous = ambiguous
        self.retry_after_ms = retry_after_ms


class McpLaunchError(McpError):
    pass


class McpProtocolError(McpError):
    pass


class McpTransportError(McpError):
    pass


class McpTimeout(McpError):
    pass


class McpCancelled(McpError):
    pass


class McpRemoteError(McpError):
    """A JSON-RPC error reported by the server, without its untrusted message."""

    def __init__(self, rpc_code: int) -> None:
        super().__init__("remote_error", f"MCP server returned JSON-RPC error {rpc_code}")
        self.rpc_code = rpc_code


@dataclass(frozen=True)
class LogEntry:
    timestamp_ms: int
    text: str
    truncated: bool


class BoundedLog:
    """A redacted in-memory ring; it is never emitted into compact status."""

    def __init__(self, capacity: int, line_bytes: int, redactions: tuple[str, ...] = ()) -> None:
        self._entries: deque[LogEntry] = deque(maxlen=capacity)
        self._line_bytes = line_bytes
        self._redactions = tuple(value for value in redactions if value)
        self._dropped = 0
        self._lock = threading.Lock()

    def append(self, raw: bytes) -> None:
        decoded = raw.decode("utf-8", errors="replace")
        decoded = "".join(
            character if character in "\t" or ord(character) >= 32 else "�"
            for character in decoded.rstrip("\r\n")
        )
        for value in self._redactions:
            decoded = decoded.replace(value, "[redacted]")
        encoded = decoded.encode("utf-8")
        truncated = len(encoded) > self._line_bytes
        if truncated:
            encoded = encoded[: self._line_bytes]
            while encoded:
                try:
                    decoded = encoded.decode("utf-8")
                    break
                except UnicodeDecodeError:
                    encoded = encoded[:-1]
            else:
                decoded = ""
        with self._lock:
            if len(self._entries) == self._entries.maxlen:
                self._dropped += 1
            self._entries.append(
                LogEntry(timestamp_ms=int(time.time() * 1000), text=decoded, truncated=truncated)
            )

    def snapshot(self) -> tuple[LogEntry, ...]:
        with self._lock:
            return tuple(self._entries)

    @property
    def dropped(self) -> int:
        with self._lock:
            return self._dropped


@dataclass
class _Pending:
    request_id: int
    method: str
    progress_token: str
    event: threading.Event
    result: Any = None
    error: Optional[McpError] = None
    progress: Optional[Callable[[Mapping[str, Any]], None]] = None


class McpStdioClient:
    """One reusable, bounded MCP session over a local subprocess's stdio."""

    def __init__(
        self,
        config: ServerConfig,
        limits: Limits,
        *,
        on_failure: Optional[Callable[["McpStdioClient", McpError], None]] = None,
        on_tools_changed: Optional[Callable[["McpStdioClient"], None]] = None,
    ) -> None:
        self.config = config
        self.limits = limits
        self.on_failure = on_failure
        self.on_tools_changed = on_tools_changed
        self.logs = BoundedLog(
            limits.max_log_entries,
            limits.max_log_line_bytes,
            tuple(config.environment.values()),
        )
        self._process: Optional[subprocess.Popen[bytes]] = None
        self._windows_job: Any = None
        self._pending: dict[int, _Pending] = {}
        self._progress: dict[str, _Pending] = {}
        self._tombstones: deque[int] = deque(maxlen=MAX_TOMBSTONES)
        self._next_id = 1
        self._fatal: Optional[McpError] = None
        self._closing = False
        self._started = False
        self._write_lock = threading.Lock()
        self._lock = threading.RLock()
        self._request_slots = threading.BoundedSemaphore(
            limits.max_pending_requests_per_server
        )
        self.server_info: dict[str, Any] = {}
        self.server_capabilities: dict[str, Any] = {}
        self.protocol_version: Optional[str] = None

    @property
    def alive(self) -> bool:
        with self._lock:
            process = self._process
            return (
                self._started
                and not self._closing
                and self._fatal is None
                and process is not None
                and process.poll() is None
            )

    @property
    def fatal_error(self) -> Optional[McpError]:
        with self._lock:
            return self._fatal

    def start(self) -> None:
        """Spawn, initialize, and notify one MCP server within the startup bound."""

        with self._lock:
            if self._started:
                raise RuntimeError("MCP client is already started")
            self._started = True
        environment = self._server_environment()
        process_group: dict[str, Any] = {}
        if os.name == "posix":
            process_group["start_new_session"] = True
        elif os.name == "nt":  # pragma: no cover - Windows release host
            process_group["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
        try:
            process = subprocess.Popen(
                [self.config.command, *self.config.args],
                cwd=str(self.config.cwd),
                env=environment,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                bufsize=0,
                shell=False,
                **process_group,
            )
        except FileNotFoundError as error:
            raise McpLaunchError(
                "executable_not_found",
                "configured MCP executable was not found",
                permanent=True,
            ) from error
        except (OSError, ValueError) as error:
            raise McpLaunchError(
                "launch_failed", "configured MCP server could not be launched", permanent=True
            ) from error
        if os.name == "nt":  # pragma: no cover - Windows release host
            try:
                self._windows_job = _WindowsJob(process)
            except OSError as error:
                process.kill()
                process.wait(timeout=1)
                raise McpLaunchError(
                    "process_tree_setup_failed",
                    "configured MCP server could not be assigned to a kill-on-close job",
                    permanent=True,
                ) from error
        with self._lock:
            self._process = process
        if os.name == "posix" and process.stdin is not None:
            try:
                os.set_blocking(process.stdin.fileno(), False)
            except OSError:
                self._terminate_process_tree(process, force=True)
                raise McpLaunchError(
                    "stdio_setup_failed",
                    "configured MCP stdin could not be made nonblocking",
                    permanent=True,
                )
        threading.Thread(target=self._stdout_loop, name=f"mcp-{self.config.id}-stdout", daemon=True).start()
        threading.Thread(target=self._stderr_loop, name=f"mcp-{self.config.id}-stderr", daemon=True).start()
        threading.Thread(target=self._wait_loop, name=f"mcp-{self.config.id}-wait", daemon=True).start()

        try:
            result = self.request(
                "initialize",
                {
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": CLIENT_NAME, "version": CLIENT_VERSION},
                },
                timeout_ms=self.config.startup_timeout_ms,
            )
            if not isinstance(result, Mapping):
                raise McpProtocolError(
                    "invalid_initialize", "MCP initialize result was not an object", permanent=True
                )
            protocol_version = result.get("protocolVersion")
            if protocol_version not in SUPPORTED_PROTOCOL_VERSIONS:
                raise McpProtocolError(
                    "unsupported_protocol",
                    "MCP server selected an unsupported protocol version",
                    permanent=True,
                )
            server_info = result.get("serverInfo", {})
            capabilities = result.get("capabilities", {})
            if not isinstance(server_info, Mapping) or not isinstance(capabilities, Mapping):
                raise McpProtocolError(
                    "invalid_initialize", "MCP initialize metadata was malformed", permanent=True
                )
            self.protocol_version = str(protocol_version)
            # These values remain internal and are never trusted as presentation labels.
            self.server_info = dict(server_info)
            self.server_capabilities = dict(capabilities)
            self.notify("notifications/initialized", {})
        except BaseException:
            self.close()
            raise

    def list_tools(self) -> list[dict[str, Any]]:
        """Read a bounded, cycle-checked MCP tool catalog."""

        cursor: Optional[str] = None
        seen_cursors: set[str] = set()
        tools: list[dict[str, Any]] = []
        names: set[str] = set()
        for _page in range(self.limits.max_catalog_pages):
            params: dict[str, Any] = {}
            if cursor is not None:
                params["cursor"] = cursor
            result = self.request(
                "tools/list", params, timeout_ms=self.config.startup_timeout_ms
            )
            if not isinstance(result, Mapping) or not isinstance(result.get("tools"), list):
                raise McpProtocolError(
                    "invalid_catalog", "MCP tools/list result was malformed", permanent=True
                )
            for item in result["tools"]:
                if not isinstance(item, Mapping):
                    raise McpProtocolError(
                        "invalid_catalog", "MCP tool definition was malformed", permanent=True
                    )
                name = item.get("name")
                if not isinstance(name, str) or not name:
                    raise McpProtocolError(
                        "invalid_catalog", "MCP tool name was malformed", permanent=True
                    )
                if name in names:
                    raise McpProtocolError(
                        "duplicate_tool", "MCP catalog contained a duplicate tool", permanent=True
                    )
                names.add(name)
                tools.append(dict(item))
                if len(tools) > self.limits.max_tools_per_server:
                    raise McpProtocolError(
                        "catalog_too_large",
                        "MCP catalog exceeded the configured tool limit",
                        permanent=True,
                    )
            next_cursor = result.get("nextCursor")
            if next_cursor is None:
                return tools
            if (
                not isinstance(next_cursor, str)
                or not next_cursor
                or len(next_cursor.encode("utf-8")) > MAX_CURSOR_BYTES
                or next_cursor in seen_cursors
            ):
                raise McpProtocolError(
                    "invalid_cursor", "MCP catalog cursor was invalid", permanent=True
                )
            seen_cursors.add(next_cursor)
            cursor = next_cursor
        raise McpProtocolError(
            "catalog_page_limit", "MCP catalog exceeded the pagination limit", permanent=True
        )

    def call_tool(
        self,
        name: str,
        arguments: Mapping[str, Any],
        *,
        cancellation: Any = None,
        progress: Optional[Callable[[Mapping[str, Any]], None]] = None,
    ) -> Mapping[str, Any]:
        result = self.request(
            "tools/call",
            {"name": name, "arguments": dict(arguments)},
            timeout_ms=self.config.request_timeout_ms,
            cancellation=cancellation,
            progress=progress,
            include_progress_token=True,
        )
        if not isinstance(result, Mapping):
            raise McpProtocolError("invalid_result", "MCP tool result was malformed")
        return result

    def request(
        self,
        method: str,
        params: Mapping[str, Any],
        *,
        timeout_ms: int,
        cancellation: Any = None,
        progress: Optional[Callable[[Mapping[str, Any]], None]] = None,
        include_progress_token: bool = False,
    ) -> Any:
        if not isinstance(method, str) or not method:
            raise ValueError("MCP request method must be non-empty")
        deadline = time.monotonic() + timeout_ms / 1000
        if not self._acquire_slot(deadline, cancellation):
            raise McpTimeout("request_queue_timeout", "MCP request queue wait timed out")
        pending: Optional[_Pending] = None
        try:
            with self._lock:
                if self._closing:
                    raise McpTransportError("server_stopped", "MCP server is stopped")
                if self._fatal is not None:
                    raise self._fatal
                request_id = self._next_id
                self._next_id += 1
                progress_token = f"ygg-mcp:{request_id}"
                pending = _Pending(
                    request_id=request_id,
                    method=method,
                    progress_token=progress_token,
                    event=threading.Event(),
                    progress=progress,
                )
                self._pending[request_id] = pending
                if progress is not None:
                    self._progress[progress_token] = pending
            request_params = dict(params)
            if include_progress_token:
                metadata = request_params.get("_meta", {})
                if not isinstance(metadata, Mapping):
                    metadata = {}
                request_params["_meta"] = {**dict(metadata), "progressToken": progress_token}
            self._send_message(
                {
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "method": method,
                    "params": request_params,
                },
                deadline=deadline,
                cancellation=cancellation,
            )
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    self._cancel_pending(pending, "timeout")
                    raise McpTimeout(
                        "request_timeout",
                        "MCP request timed out; its external outcome was not retried",
                        ambiguous=method == "tools/call",
                    )
                if pending.event.wait(min(0.05, remaining)):
                    if pending.error is not None:
                        raise pending.error
                    return pending.result
                if cancellation is not None and bool(getattr(cancellation, "cancelled", False)):
                    reason = getattr(cancellation, "reason", None) or "cancelled"
                    self._cancel_pending(pending, str(reason))
                    raise McpCancelled(
                        "request_cancelled",
                        "MCP request cancellation was forwarded; rollback is not claimed",
                        ambiguous=method == "tools/call",
                    )
        finally:
            if pending is not None:
                with self._lock:
                    self._pending.pop(pending.request_id, None)
                    self._progress.pop(pending.progress_token, None)
            self._request_slots.release()

    def notify(self, method: str, params: Mapping[str, Any]) -> None:
        self._send_message(
            {"jsonrpc": "2.0", "method": method, "params": dict(params)}
        )

    def close(self) -> None:
        """Boundedly close the server-owned process group and all descendants."""

        with self._lock:
            if self._closing:
                return
            self._closing = True
            process = self._process
            pending = list(self._pending.values())
            for item in pending:
                item.error = McpTransportError("server_stopped", "MCP server was stopped")
                item.event.set()
        if process is None:
            return
        stdin = process.stdin
        if stdin is not None:
            try:
                stdin.close()
            except OSError:
                pass
        timeout = self.limits.shutdown_timeout_ms / 1000
        try:
            process.wait(timeout=timeout / 2)
            # The group leader may exit while descendants retain stdio.
            self._terminate_process_tree(process, force=True)
        except subprocess.TimeoutExpired:
            self._terminate_process_tree(process, force=False)
            try:
                process.wait(timeout=timeout / 2)
            except subprocess.TimeoutExpired:
                self._terminate_process_tree(process, force=True)
                try:
                    process.wait(timeout=max(0.05, timeout / 2))
                except subprocess.TimeoutExpired:
                    pass
        finally:
            for stream in (process.stdin, process.stdout, process.stderr):
                if stream is not None:
                    try:
                        stream.close()
                    except OSError:
                        pass
            if self._windows_job is not None:  # pragma: no cover - Windows release host
                self._windows_job.close()
                self._windows_job = None

    def _acquire_slot(self, deadline: float, cancellation: Any) -> bool:
        while True:
            if cancellation is not None and bool(getattr(cancellation, "cancelled", False)):
                raise McpCancelled(
                    "request_cancelled", "MCP request was cancelled before admission"
                )
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return False
            if self._request_slots.acquire(timeout=min(0.05, remaining)):
                return True

    def _cancel_pending(self, pending: _Pending, reason: str) -> None:
        with self._lock:
            if pending.event.is_set():
                return
            self._remember_tombstone(pending.request_id)
            self._pending.pop(pending.request_id, None)
            self._progress.pop(pending.progress_token, None)
        try:
            self._send_message(
                {
                    "jsonrpc": "2.0",
                    "method": "notifications/cancelled",
                    "params": {"requestId": pending.request_id, "reason": reason[:256]},
                },
                deadline=time.monotonic() + min(0.5, self.limits.shutdown_timeout_ms / 1000),
            )
        except McpError:
            pass

    def _send_message(
        self,
        message: Mapping[str, Any],
        *,
        deadline: Optional[float] = None,
        cancellation: Any = None,
    ) -> None:
        try:
            payload = json.dumps(
                message,
                ensure_ascii=False,
                separators=(",", ":"),
                allow_nan=False,
            ).encode("utf-8") + b"\n"
        except (TypeError, ValueError) as error:
            raise McpProtocolError(
                "invalid_outbound", "bridge could not encode an MCP request"
            ) from error
        if len(payload) > self.limits.max_frame_bytes:
            raise McpProtocolError(
                "outbound_too_large", "MCP request exceeded the frame limit"
            )
        if deadline is None:
            deadline = time.monotonic() + self.config.request_timeout_ms / 1000
        with self._write_lock:
            with self._lock:
                process = self._process
                if self._closing or process is None or process.stdin is None:
                    raise McpTransportError("server_stopped", "MCP server is stopped")
                if self._fatal is not None:
                    raise self._fatal
                stdin = process.stdin
            try:
                if os.name == "posix":
                    self._write_nonblocking(stdin.fileno(), payload, deadline, cancellation)
                else:  # pragma: no cover - Windows release host
                    self._write_in_thread(stdin, payload, deadline, cancellation)
            except (McpTimeout, McpCancelled):
                raise
            except (BrokenPipeError, OSError, ValueError) as error:
                transport_error = McpTransportError(
                    "write_failed", "MCP transport closed while writing", ambiguous=True
                )
                self._fail(transport_error)
                raise transport_error from error

    def _write_nonblocking(
        self, fd: int, payload: bytes, deadline: float, cancellation: Any
    ) -> None:
        view = memoryview(payload)
        written = 0
        while written < len(view):
            self._check_write_boundary(deadline, cancellation)
            try:
                count = os.write(fd, view[written:])
            except BlockingIOError:
                count = 0
            if count > 0:
                written += count
                continue
            remaining = deadline - time.monotonic()
            select.select([], [fd], [], min(0.05, max(0.0, remaining)))

    def _write_in_thread(
        self, stdin: Any, payload: bytes, deadline: float, cancellation: Any
    ) -> None:
        complete = threading.Event()
        errors: list[BaseException] = []

        def write() -> None:
            try:
                stdin.write(payload)
                stdin.flush()
            except BaseException as error:
                errors.append(error)
            finally:
                complete.set()

        threading.Thread(
            target=write,
            name=f"mcp-{self.config.id}-stdin",
            daemon=True,
        ).start()
        while not complete.wait(0.05):
            self._check_write_boundary(deadline, cancellation)
        if errors:
            raise errors[0]

    def _check_write_boundary(self, deadline: float, cancellation: Any) -> None:
        if cancellation is not None and bool(getattr(cancellation, "cancelled", False)):
            error = McpCancelled(
                "request_cancelled",
                "MCP request was cancelled while writing; external outcome is ambiguous",
                ambiguous=True,
            )
            self._fail(error)
            raise error
        if time.monotonic() >= deadline:
            error = McpTimeout(
                "write_timeout",
                "MCP request timed out while writing; external outcome was not retried",
                ambiguous=True,
            )
            self._fail(error)
            raise error

    def _stdout_loop(self) -> None:
        with self._lock:
            process = self._process
        if process is None or process.stdout is None:
            return
        stream = process.stdout
        while True:
            try:
                line = stream.readline(self.limits.max_frame_bytes + 1)
            except OSError:
                self._fail(McpTransportError("read_failed", "MCP transport read failed"))
                return
            if not line:
                return
            if len(line) > self.limits.max_frame_bytes:
                self._fail(
                    McpProtocolError(
                        "oversized_frame",
                        "MCP server emitted an oversized frame",
                        permanent=True,
                    )
                )
                return
            if not line.endswith(b"\n"):
                self._fail(
                    McpProtocolError(
                        "unterminated_frame",
                        "MCP server emitted an unterminated frame",
                        permanent=True,
                    )
                )
                return
            try:
                decoded = line[:-1].decode("utf-8")
                message = json.loads(decoded)
            except (UnicodeDecodeError, json.JSONDecodeError):
                self._fail(
                    McpProtocolError(
                        "malformed_frame",
                        "MCP server emitted malformed JSON",
                        permanent=True,
                    )
                )
                return
            if not isinstance(message, dict) or message.get("jsonrpc") != "2.0":
                self._fail(
                    McpProtocolError(
                        "invalid_frame",
                        "MCP server emitted an invalid JSON-RPC frame",
                        permanent=True,
                    )
                )
                return
            try:
                self._route_message(message)
            except McpProtocolError as error:
                self._fail(error)
                return

    def _route_message(self, message: dict[str, Any]) -> None:
        if "method" in message:
            method = message.get("method")
            if not isinstance(method, str) or not method:
                raise McpProtocolError(
                    "invalid_frame", "MCP server emitted an invalid method", permanent=True
                )
            if "id" in message:
                self._reply_method_not_found(message["id"])
                return
            params = message.get("params", {})
            if method == "notifications/tools/list_changed":
                callback = self.on_tools_changed
                if callback is not None:
                    callback(self)
            elif method == "notifications/progress" and isinstance(params, Mapping):
                self._route_progress(params)
            elif method == "notifications/message":
                # Log content is untrusted. Retain only a generic bounded marker;
                # server stderr already provides the opt-in diagnostic ring.
                self.logs.append(b"MCP log notification received")
            return

        request_id = message.get("id")
        if isinstance(request_id, bool) or not isinstance(request_id, int):
            raise McpProtocolError(
                "invalid_response", "MCP response id was invalid", permanent=True
            )
        with self._lock:
            pending = self._pending.get(request_id)
            tombstoned = request_id in self._tombstones
        if pending is None:
            if not tombstoned:
                self.logs.append(b"Unmatched MCP response ignored")
            return
        has_result = "result" in message
        has_error = "error" in message
        if has_result == has_error:
            raise McpProtocolError(
                "invalid_response", "MCP response did not have one terminal value", permanent=True
            )
        if has_error:
            error = message["error"]
            if not isinstance(error, Mapping):
                raise McpProtocolError(
                    "invalid_response", "MCP JSON-RPC error was malformed", permanent=True
                )
            code = error.get("code")
            if isinstance(code, bool) or not isinstance(code, int):
                raise McpProtocolError(
                    "invalid_response", "MCP JSON-RPC error code was malformed", permanent=True
                )
            pending.error = McpRemoteError(code)
        else:
            result = message["result"]
            try:
                encoded = json.dumps(result, separators=(",", ":"), allow_nan=False).encode(
                    "utf-8"
                )
            except (TypeError, ValueError) as error:
                raise McpProtocolError(
                    "invalid_response", "MCP result was not valid JSON", permanent=True
                ) from error
            if len(encoded) > self.limits.max_result_bytes:
                pending.error = McpProtocolError(
                    "result_too_large", "MCP result exceeded the configured result limit"
                )
            else:
                pending.result = result
        pending.event.set()

    def _route_progress(self, params: Mapping[str, Any]) -> None:
        token = params.get("progressToken")
        if not isinstance(token, (str, int)) or isinstance(token, bool):
            return
        with self._lock:
            pending = self._progress.get(str(token))
            if pending is None and isinstance(token, str):
                pending = self._progress.get(token)
        if pending is None or pending.progress is None or pending.event.is_set():
            return
        try:
            pending.progress(dict(params))
        except Exception:
            # Frontend progress is best-effort and can never break the MCP read loop.
            return

    def _reply_method_not_found(self, request_id: Any) -> None:
        if isinstance(request_id, bool) or not isinstance(request_id, (int, str)):
            raise McpProtocolError(
                "invalid_request", "MCP server request id was invalid", permanent=True
            )
        self._send_message(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {"code": -32601, "message": "Method not found"},
            }
        )

    def _stderr_loop(self) -> None:
        with self._lock:
            process = self._process
        if process is None or process.stderr is None:
            return
        stream = process.stderr
        limit = self.limits.max_log_line_bytes
        while True:
            try:
                line = stream.readline(limit + 1)
            except OSError:
                return
            if not line:
                return
            oversized = len(line) > limit and not line.endswith(b"\n")
            self.logs.append(line[:limit])
            if oversized:
                while line and not line.endswith(b"\n"):
                    try:
                        line = stream.readline(limit + 1)
                    except OSError:
                        return

    def _wait_loop(self) -> None:
        with self._lock:
            process = self._process
        if process is None:
            return
        try:
            return_code = process.wait()
        except OSError:
            return
        with self._lock:
            closing = self._closing
        if not closing:
            self._fail(
                McpTransportError(
                    "server_exited",
                    "MCP server exited unexpectedly",
                    ambiguous=True,
                )
            )
        del return_code

    def _terminate_process_tree(self, process: subprocess.Popen[bytes], *, force: bool) -> None:
        try:
            if os.name == "posix":
                os.killpg(process.pid, signal.SIGKILL if force else signal.SIGTERM)
            elif self._windows_job is not None:  # pragma: no cover - Windows release host
                self._windows_job.terminate()
            elif process.poll() is None:  # pragma: no cover - defensive Windows fallback
                if force:
                    process.kill()
                else:
                    process.terminate()
        except (OSError, ProcessLookupError):
            pass

    def _fail(self, error: McpError) -> None:
        callback: Optional[Callable[[McpStdioClient, McpError], None]]
        process: Optional[subprocess.Popen[bytes]]
        with self._lock:
            if self._closing or self._fatal is not None:
                return
            self._fatal = error
            for pending in self._pending.values():
                if not pending.event.is_set():
                    pending.error = error
                    pending.event.set()
            callback = self.on_failure
            process = self._process
        if process is not None:
            self._terminate_process_tree(process, force=False)
        if callback is not None:
            try:
                callback(self, error)
            except Exception:
                pass

    def _remember_tombstone(self, request_id: int) -> None:
        self._tombstones.append(request_id)

    def _server_environment(self) -> dict[str, str]:
        allowed = ("PATH", "LANG", "LC_ALL", "LC_CTYPE", "TMPDIR", "TMP", "TEMP")
        environment = {
            name: value for name in allowed if (value := os.environ.get(name)) is not None
        }
        environment.update(self.config.environment)
        return environment
