"""Bounded MCP Streamable HTTP client.

The transport deliberately uses only the Python standard library.  Every request
uses the exact configured endpoint (no redirects, cookies, proxy discovery, or
URL credentials), while session and SSE resumption identifiers remain process
memory only.
"""

from __future__ import annotations

from dataclasses import dataclass, field
import http.client
import json
import socket
import ssl
import threading
import time
from typing import Any, Callable, Mapping, Optional, Protocol
from urllib.parse import urlsplit

from .config import Limits, ServerConfig
from .protocol import (
    BoundedLog,
    CLIENT_NAME,
    CLIENT_VERSION,
    MCP_PROTOCOL_VERSION,
    MAX_CURSOR_BYTES,
    McpCancelled,
    McpError,
    McpProtocolError,
    McpRemoteError,
    McpTimeout,
    McpTransportError,
    SUPPORTED_PROTOCOL_VERSIONS,
)


MAX_HTTP_SESSION_ID_BYTES = 512
MAX_HTTP_EVENT_ID_BYTES = 1024
MAX_HTTP_EVENTS = 256
MAX_CREDENTIAL_BYTES = 64 * 1024


class CredentialProvider(Protocol):
    """Resolve a configured non-secret credential reference at request time.

    Implementations must return an ephemeral bearer token or ``None``.  The
    bridge never stores the token, puts it in configuration, logs it, or exposes
    it through presentation/result metadata.  OAuth/browser flows are outside
    this adapter and are intentionally not implemented by this transport.
    """

    def bearer_token(self, credential: str, *, server_id: str) -> Optional[str]:
        """Return an ephemeral token for one configured logical reference."""


class UnavailableCredentialProvider:
    """The safe default when no credential broker was explicitly composed."""

    def bearer_token(self, credential: str, *, server_id: str) -> Optional[str]:
        del credential, server_id
        return None


class McpAuthenticationError(McpError):
    """Authentication failed without retaining server-controlled text."""


@dataclass(frozen=True, repr=False)
class _Endpoint:
    scheme: str
    host: str
    port: Optional[int]
    target: str


@dataclass(repr=False)
class _HttpRead:
    messages: list[dict[str, Any]] = field(default_factory=list)
    complete: bool = False
    is_sse: bool = False
    last_event_id: Optional[str] = None
    retry_ms: Optional[int] = None


class _HttpOperation:
    """A cancellable, bounded set of sockets owned by one caller operation."""

    def __init__(self) -> None:
        self.done = threading.Event()
        self.error: Optional[BaseException] = None
        self.result: Any = None
        self._aborted = threading.Event()
        self._connections: list[http.client.HTTPConnection] = []
        self._lock = threading.Lock()

    @property
    def aborted(self) -> bool:
        return self._aborted.is_set()

    def add_connection(self, connection: http.client.HTTPConnection) -> None:
        with self._lock:
            if self._aborted.is_set():
                try:
                    connection.close()
                except OSError:
                    pass
                raise McpTransportError("operation_cancelled", "MCP HTTP operation was cancelled")
            self._connections.append(connection)

    def remove_connection(self, connection: http.client.HTTPConnection) -> None:
        with self._lock:
            try:
                self._connections.remove(connection)
            except ValueError:
                return

    def abort(self) -> None:
        self._aborted.set()
        with self._lock:
            connections = tuple(self._connections)
        for connection in connections:
            try:
                connection.close()
            except OSError:
                pass


class McpStreamableHttpClient:
    """One reusable MCP session over the pinned Streamable HTTP conventions.

    Requests are POSTed as JSON-RPC.  Responses may be ``application/json`` or
    ``text/event-stream``.  A prematurely closed POST SSE response is resumed
    only with its server-issued event ID through a bounded GET; the original
    JSON-RPC request is never replayed.
    """

    def __init__(
        self,
        config: ServerConfig,
        limits: Limits,
        *,
        credential_provider: Optional[CredentialProvider] = None,
        on_failure: Optional[Callable[["McpStreamableHttpClient", McpError], None]] = None,
        on_tools_changed: Optional[Callable[["McpStreamableHttpClient"], None]] = None,
    ) -> None:
        if config.transport != "streamable-http" or config.url is None:
            raise ValueError("Streamable HTTP client requires a streamable-http server configuration")
        self.config = config
        self.limits = limits
        self.on_failure = on_failure
        self.on_tools_changed = on_tools_changed
        self._credential_provider = credential_provider or UnavailableCredentialProvider()
        self._endpoint = _endpoint(config.url)
        self.logs = BoundedLog(limits.max_log_entries, limits.max_log_line_bytes)
        self._pending_slots = threading.BoundedSemaphore(limits.max_pending_requests_per_server)
        self._lock = threading.RLock()
        self._operations: set[_HttpOperation] = set()
        self._next_id = 1
        self._started = False
        self._closing = False
        self._fatal: Optional[McpError] = None
        # These server-issued values are intentionally memory-only and never
        # enter logging, presentation, configuration, or result metadata.
        self._session_id: Optional[str] = None
        self._last_event_id: Optional[str] = None
        self.server_info: dict[str, Any] = {}
        self.server_capabilities: dict[str, Any] = {}
        self.protocol_version: Optional[str] = None

    @property
    def alive(self) -> bool:
        with self._lock:
            return self._started and not self._closing and self._fatal is None

    @property
    def fatal_error(self) -> Optional[McpError]:
        with self._lock:
            return self._fatal

    def start(self) -> None:
        """Initialize one explicit remote endpoint inside the startup deadline."""

        with self._lock:
            if self._started:
                raise RuntimeError("MCP client is already started")
            self._started = True
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
            with self._lock:
                self.protocol_version = str(protocol_version)
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
        """Send one JSON-RPC request without replaying it after uncertainty."""

        if not isinstance(method, str) or not method:
            raise ValueError("MCP request method must be non-empty")
        deadline = time.monotonic() + timeout_ms / 1000
        self._acquire_slot(deadline, cancellation)
        launched = False
        try:
            with self._lock:
                if self._closing:
                    raise McpTransportError("server_stopped", "MCP server is stopped")
                if self._fatal is not None:
                    raise self._fatal
                request_id = self._next_id
                self._next_id += 1
            progress_token = f"ygg-mcp:{request_id}"
            request_params = dict(params)
            if include_progress_token:
                metadata = request_params.get("_meta", {})
                if not isinstance(metadata, Mapping):
                    metadata = {}
                request_params["_meta"] = {**dict(metadata), "progressToken": progress_token}
            message = {
                "jsonrpc": "2.0",
                "id": request_id,
                "method": method,
                "params": request_params,
            }
            payload = _encode_message(message, self.limits.max_frame_bytes)
            operation = self._launch(
                lambda active: self._post_request(
                    active,
                    payload,
                    request_id,
                    method,
                    deadline,
                ),
                release_slot=True,
            )
            launched = True
            messages = self._await(
                operation,
                deadline,
                cancellation=cancellation,
                cancellation_request=(request_id, method),
            )
            return self._route_request_messages(
                messages,
                request_id=request_id,
                progress_token=progress_token,
                progress=progress,
            )
        except (McpProtocolError, McpTransportError, McpAuthenticationError) as error:
            self._fail(error)
            raise
        finally:
            if not launched:
                self._pending_slots.release()

    def notify(self, method: str, params: Mapping[str, Any]) -> None:
        """Send one bounded notification and accept the protocol's 202 response."""

        if not isinstance(method, str) or not method:
            raise ValueError("MCP notification method must be non-empty")
        deadline = time.monotonic() + self.config.request_timeout_ms / 1000
        self._acquire_slot(deadline, None)
        launched = False
        try:
            payload = _encode_message(
                {"jsonrpc": "2.0", "method": method, "params": dict(params)},
                self.limits.max_frame_bytes,
            )
            operation = self._launch(
                lambda active: self._exchange(
                    active,
                    verb="POST",
                    payload=payload,
                    expected_id=None,
                    deadline=deadline,
                    accept_session=False,
                    response_required=False,
                    phase="notification",
                ),
                release_slot=True,
            )
            launched = True
            self._await(operation, deadline)
        except (McpProtocolError, McpTransportError, McpAuthenticationError) as error:
            self._fail(error)
            raise
        finally:
            if not launched:
                self._pending_slots.release()

    def close(self) -> None:
        """Abort active sockets and best-effort DELETE the negotiated session."""

        with self._lock:
            if self._closing:
                return
            self._closing = True
            operations = tuple(self._operations)
            session_id = self._session_id
        for operation in operations:
            operation.abort()

        # Streamable HTTP permits (but does not require) session termination.
        # It is deliberately bounded and ignores unavailable/revoked credentials.
        if session_id is not None:
            deadline = time.monotonic() + self.limits.shutdown_timeout_ms / 1000
            try:
                operation = self._launch(
                    lambda active: self._exchange(
                        active,
                        verb="DELETE",
                        payload=None,
                        expected_id=None,
                        deadline=deadline,
                        accept_session=False,
                        response_required=False,
                        phase="delete",
                        allow_closing=True,
                    ),
                    release_slot=False,
                    allow_closing=True,
                )
                self._await(operation, deadline)
            except McpError:
                pass
        with self._lock:
            self._session_id = None
            self._last_event_id = None

    def _post_request(
        self,
        operation: _HttpOperation,
        payload: bytes,
        request_id: int,
        method: str,
        deadline: float,
    ) -> list[dict[str, Any]]:
        first = self._exchange(
            operation,
            verb="POST",
            payload=payload,
            expected_id=request_id,
            deadline=deadline,
            accept_session=method == "initialize",
            response_required=True,
            phase="request",
        )
        if first.complete:
            return first.messages
        # A direct JSON response without the matching ID is malformed. A closed
        # SSE response can be resumed only with its server-issued event ID; when
        # there is no ID, surface a transport loss so the lifecycle can build a
        # fresh session without replaying the uncertain request.
        if not first.is_sse:
            raise McpProtocolError(
                "missing_response", "MCP HTTP response did not contain the request result", permanent=True
            )
        if first.last_event_id is None:
            raise McpTransportError(
                "sse_response_interrupted",
                "MCP response stream closed before its result and cannot be resumed safely",
                ambiguous=method == "tools/call",
            )
        messages = list(first.messages)
        last_event_id = first.last_event_id
        retry_ms = first.retry_ms
        for _attempt in range(self.config.max_restarts):
            self._wait_for_resumption(operation, deadline, retry_ms)
            resumed = self._exchange(
                operation,
                verb="GET",
                payload=None,
                expected_id=request_id,
                deadline=deadline,
                accept_session=False,
                response_required=True,
                phase="resume",
                last_event_id=last_event_id,
            )
            messages.extend(resumed.messages)
            if resumed.complete:
                return messages
            if resumed.last_event_id is not None:
                last_event_id = resumed.last_event_id
            retry_ms = resumed.retry_ms if resumed.retry_ms is not None else retry_ms
        raise McpTransportError(
            "sse_resumption_exhausted",
            "MCP response stream ended before a result and bounded resumption was exhausted",
            ambiguous=method == "tools/call",
        )

    def _exchange(
        self,
        operation: _HttpOperation,
        *,
        verb: str,
        payload: Optional[bytes],
        expected_id: Optional[int],
        deadline: float,
        accept_session: bool,
        response_required: bool,
        phase: str,
        last_event_id: Optional[str] = None,
        allow_closing: bool = False,
    ) -> _HttpRead:
        headers, redactions = self._request_headers(
            verb=verb,
            has_payload=payload is not None,
            last_event_id=last_event_id,
            allow_closing=allow_closing,
        )
        connection: Optional[http.client.HTTPConnection] = None
        response: Optional[http.client.HTTPResponse] = None
        try:
            timeout = _remaining_timeout(deadline)
            connection = self._connection(timeout)
            operation.add_connection(connection)
            connection.request(verb, self._endpoint.target, body=payload, headers=headers)
            response = connection.getresponse()
            session_id = None
            if 200 <= response.status < 300:
                session_id = self._consume_session_header(response, accept_session=accept_session)
            response_redactions = self._redactions((*redactions, session_id))
            return self._read_response(
                response,
                operation=operation,
                expected_id=expected_id,
                deadline=deadline,
                response_required=response_required,
                phase=phase,
                redactions=response_redactions,
            )
        except McpError:
            raise
        except ssl.SSLError as error:
            raise McpProtocolError(
                "tls_failed", "MCP HTTPS connection failed certificate or TLS validation", permanent=True
            ) from error
        except (socket.timeout, TimeoutError) as error:
            raise McpTimeout(
                "request_timeout", "MCP HTTP request timed out; its external outcome was not retried",
                ambiguous=expected_id is not None,
            ) from error
        except (http.client.HTTPException, OSError, ValueError) as error:
            if operation.aborted:
                raise McpTransportError(
                    "operation_interrupted", "MCP HTTP operation was interrupted", ambiguous=expected_id is not None
                ) from error
            raise McpTransportError(
                "http_transport_lost",
                "MCP HTTP transport was lost",
                ambiguous=expected_id is not None,
            ) from error
        finally:
            if response is not None:
                _close_response(response)
            if connection is not None:
                operation.remove_connection(connection)
                try:
                    connection.close()
                except OSError:
                    pass
            # Header strings can contain an adapter-returned bearer token; keep
            # them local to this exchange and discard them promptly.
            headers.clear()

    def _read_response(
        self,
        response: http.client.HTTPResponse,
        *,
        operation: _HttpOperation,
        expected_id: Optional[int],
        deadline: float,
        response_required: bool,
        phase: str,
        redactions: tuple[str, ...],
    ) -> _HttpRead:
        status = response.status
        if status == 202:
            if response_required:
                _close_response(response)
                raise McpTransportError(
                    "http_accepted_without_response",
                    "MCP HTTP endpoint accepted a request without returning its result",
                    ambiguous=expected_id is not None,
                )
            _discard_bounded_body(response, operation, deadline, self.limits.max_frame_bytes)
            return _HttpRead(complete=True)
        if not 200 <= status < 300:
            error = self._status_error(status, response, phase=phase, expected_id=expected_id)
            _close_response(response)
            raise error
        if not response_required:
            _discard_bounded_body(response, operation, deadline, self.limits.max_frame_bytes)
            return _HttpRead(complete=True)

        content_type = _content_type(response)
        if content_type == "application/json":
            if phase == "resume":
                _close_response(response)
                raise McpProtocolError(
                    "invalid_content_type",
                    "MCP SSE resumption did not return an event stream",
                    permanent=True,
                )
            raw = _read_bounded_body(response, operation, deadline, self.limits.max_frame_bytes)
            message = _decode_json_message(raw, redactions)
            if expected_id is None:
                raise McpProtocolError(
                    "unexpected_response", "MCP notification received an unexpected JSON response"
                )
            return _HttpRead(
                messages=[message], complete=_matching_response_id(message, expected_id)
            )
        if content_type == "text/event-stream":
            return self._read_sse(
                response,
                operation=operation,
                expected_id=expected_id,
                deadline=deadline,
                redactions=redactions,
            )
        _close_response(response)
        raise McpProtocolError(
            "invalid_content_type",
            "MCP HTTP response used an unsupported content type",
            permanent=True,
        )

    def _read_sse(
        self,
        response: http.client.HTTPResponse,
        *,
        operation: _HttpOperation,
        expected_id: Optional[int],
        deadline: float,
        redactions: tuple[str, ...],
    ) -> _HttpRead:
        _validate_content_length(response, self.limits.max_frame_bytes)
        result = _HttpRead(is_sse=True)
        data_lines: list[str] = []
        event_type = "message"
        event_bytes = 0
        event_count = 0

        def dispatch() -> bool:
            nonlocal data_lines, event_type, event_bytes, event_count
            if not data_lines:
                event_type = "message"
                event_bytes = 0
                return False
            event_count += 1
            if event_count > MAX_HTTP_EVENTS:
                raise McpProtocolError(
                    "sse_event_limit", "MCP SSE response exceeded the event limit", permanent=True
                )
            event_redactions = self._redactions((*redactions, result.last_event_id))
            data = _redact_text("\n".join(data_lines), event_redactions)
            data_lines = []
            current_event_type = event_type
            event_type = "message"
            event_bytes = 0
            if current_event_type not in {"", "message"}:
                return False
            try:
                message = json.loads(data)
            except (json.JSONDecodeError, RecursionError) as error:
                raise McpProtocolError(
                    "malformed_sse_event", "MCP SSE event contained malformed JSON", permanent=True
                ) from error
            try:
                message = _redact_jsonrpc_message(message, event_redactions)
            except RecursionError as error:
                raise McpProtocolError(
                    "invalid_sse_event", "MCP SSE event exceeded the nesting limit", permanent=True
                ) from error
            if not isinstance(message, dict) or message.get("jsonrpc") != "2.0":
                raise McpProtocolError(
                    "invalid_sse_event", "MCP SSE event was not a JSON-RPC message", permanent=True
                )
            result.messages.append(message)
            return expected_id is not None and _matching_response_id(message, expected_id)

        total_bytes = 0
        while True:
            _check_operation_deadline(operation, deadline)
            try:
                line = response.readline(self.limits.max_frame_bytes + 1)
            except (socket.timeout, TimeoutError) as error:
                raise McpTimeout(
                    "request_timeout",
                    "MCP SSE response timed out; its external outcome was not retried",
                    ambiguous=expected_id is not None,
                ) from error
            if not line:
                if dispatch():
                    result.complete = True
                break
            total_bytes += len(line)
            event_bytes += len(line)
            if total_bytes > self.limits.max_frame_bytes:
                raise McpProtocolError(
                    "http_body_too_large", "MCP HTTP response exceeded the frame limit", permanent=True
                )
            if len(line) > self.limits.max_frame_bytes or event_bytes > self.limits.max_frame_bytes:
                raise McpProtocolError(
                    "sse_event_too_large", "MCP SSE event exceeded the frame limit", permanent=True
                )
            try:
                text = line.decode("utf-8")
            except UnicodeDecodeError as error:
                raise McpProtocolError(
                    "malformed_sse_event", "MCP SSE event was not UTF-8", permanent=True
                ) from error
            if text.endswith("\n"):
                text = text[:-1]
            if text.endswith("\r"):
                text = text[:-1]
            if not text:
                if dispatch():
                    result.complete = True
                    break
                continue
            if text.startswith(":"):
                continue
            field_name, separator, field_value = text.partition(":")
            if separator and field_value.startswith(" "):
                field_value = field_value[1:]
            if field_name == "data":
                data_lines.append(field_value)
            elif field_name == "event":
                event_type = field_value
            elif field_name == "id":
                result.last_event_id = _validate_event_id(field_value)
                self._remember_event_id(result.last_event_id)
            elif field_name == "retry" and field_value.isascii() and field_value.isdecimal():
                result.retry_ms = min(int(field_value), self.limits.backoff_max_ms)
        return result

    def _route_request_messages(
        self,
        messages: list[dict[str, Any]],
        *,
        request_id: int,
        progress_token: str,
        progress: Optional[Callable[[Mapping[str, Any]], None]],
    ) -> Any:
        terminal: Any = _MISSING
        for message in messages:
            if message.get("jsonrpc") != "2.0":
                raise McpProtocolError(
                    "invalid_response", "MCP HTTP response was not JSON-RPC", permanent=True
                )
            if "method" in message:
                self._route_server_message(message, progress_token=progress_token, progress=progress)
                continue
            if not _matching_response_id(message, request_id):
                self.logs.append(b"Unmatched MCP HTTP response ignored")
                continue
            has_result = "result" in message
            has_error = "error" in message
            if has_result == has_error:
                raise McpProtocolError(
                    "invalid_response", "MCP response did not have one terminal value", permanent=True
                )
            if terminal is not _MISSING:
                raise McpProtocolError(
                    "duplicate_response", "MCP HTTP response repeated a terminal result", permanent=True
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
                raise McpRemoteError(code)
            result = message["result"]
            _validate_result_size(result, self.limits.max_result_bytes)
            terminal = result
        if terminal is _MISSING:
            raise McpProtocolError(
                "missing_response", "MCP HTTP response did not contain the request result", permanent=True
            )
        return terminal

    def _route_server_message(
        self,
        message: Mapping[str, Any],
        *,
        progress_token: str,
        progress: Optional[Callable[[Mapping[str, Any]], None]],
    ) -> None:
        method = message.get("method")
        if not isinstance(method, str) or not method:
            raise McpProtocolError(
                "invalid_sse_event", "MCP server emitted an invalid method", permanent=True
            )
        if "id" in message:
            request_id = message.get("id")
            if isinstance(request_id, bool) or not isinstance(request_id, (int, str)):
                raise McpProtocolError(
                    "invalid_request", "MCP server request id was invalid", permanent=True
                )
            self._reply_method_not_found(request_id)
            return
        params = message.get("params", {})
        if method == "notifications/tools/list_changed":
            callback = self.on_tools_changed
            if callback is not None:
                callback(self)
        elif method == "notifications/progress" and isinstance(params, Mapping):
            token = params.get("progressToken")
            if (
                progress is not None
                and not isinstance(token, bool)
                and isinstance(token, (str, int))
                and str(token) == progress_token
            ):
                try:
                    progress(dict(params))
                except Exception:
                    return
        elif method == "notifications/message":
            # Never retain untrusted remote log text (which may contain a token).
            self.logs.append(b"MCP HTTP log notification received")

    def _reply_method_not_found(self, request_id: Any) -> None:
        payload = _encode_message(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {"code": -32601, "message": "Method not found"},
            },
            self.limits.max_frame_bytes,
        )
        deadline = time.monotonic() + min(0.5, self.limits.shutdown_timeout_ms / 1000)
        try:
            self._launch(
                lambda active: self._exchange(
                    active,
                    verb="POST",
                    payload=payload,
                    expected_id=None,
                    deadline=deadline,
                    accept_session=False,
                    response_required=False,
                    phase="server_request_reply",
                ),
                release_slot=False,
            )
        except McpError:
            return

    def _send_cancellation(self, request_id: int, reason: str) -> None:
        payload = _encode_message(
            {
                "jsonrpc": "2.0",
                "method": "notifications/cancelled",
                "params": {"requestId": request_id, "reason": reason[:256]},
            },
            self.limits.max_frame_bytes,
        )
        deadline = time.monotonic() + min(0.5, self.limits.shutdown_timeout_ms / 1000)

        def send() -> None:
            try:
                self._exchange(
                    _HttpOperation(),
                    verb="POST",
                    payload=payload,
                    expected_id=None,
                    deadline=deadline,
                    accept_session=False,
                    response_required=False,
                    phase="cancellation",
                    allow_closing=True,
                )
            except McpError:
                return

        threading.Thread(
            target=send,
            name=f"mcp-{self.config.id}-http-cancel",
            daemon=True,
        ).start()

    def _await(
        self,
        operation: _HttpOperation,
        deadline: float,
        *,
        cancellation: Any = None,
        cancellation_request: Optional[tuple[int, str]] = None,
    ) -> Any:
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                operation.abort()
                if cancellation_request is not None:
                    self._send_cancellation(cancellation_request[0], "timeout")
                raise McpTimeout(
                    "request_timeout",
                    "MCP HTTP request timed out; its external outcome was not retried",
                    ambiguous=cancellation_request is not None and cancellation_request[1] == "tools/call",
                )
            if cancellation is not None and bool(getattr(cancellation, "cancelled", False)):
                operation.abort()
                if cancellation_request is not None:
                    reason = getattr(cancellation, "reason", None) or "cancelled"
                    self._send_cancellation(cancellation_request[0], str(reason))
                raise McpCancelled(
                    "request_cancelled",
                    "MCP request cancellation was forwarded; rollback is not claimed",
                    ambiguous=cancellation_request is not None and cancellation_request[1] == "tools/call",
                )
            if operation.done.wait(min(0.05, remaining)):
                if operation.error is not None:
                    if isinstance(operation.error, McpError):
                        raise operation.error
                    raise McpTransportError(
                        "http_transport_failed", "MCP HTTP transport failed", ambiguous=cancellation_request is not None
                    )
                return operation.result

    def _launch(
        self,
        task: Callable[[_HttpOperation], Any],
        *,
        release_slot: bool,
        allow_closing: bool = False,
    ) -> _HttpOperation:
        operation = _HttpOperation()
        with self._lock:
            if self._closing and not allow_closing:
                raise McpTransportError("server_stopped", "MCP server is stopped")
            self._operations.add(operation)

        def run() -> None:
            try:
                operation.result = task(operation)
            except BaseException as error:
                operation.error = error
            finally:
                with self._lock:
                    self._operations.discard(operation)
                operation.done.set()
                if release_slot:
                    self._pending_slots.release()

        threading.Thread(
            target=run,
            name=f"mcp-{self.config.id}-http",
            daemon=True,
        ).start()
        return operation

    def _acquire_slot(self, deadline: float, cancellation: Any) -> None:
        while True:
            with self._lock:
                if self._closing:
                    raise McpTransportError("server_stopped", "MCP server is stopped")
                if self._fatal is not None:
                    raise self._fatal
            if cancellation is not None and bool(getattr(cancellation, "cancelled", False)):
                raise McpCancelled(
                    "request_cancelled", "MCP request was cancelled before admission"
                )
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise McpTimeout("request_queue_timeout", "MCP request queue wait timed out")
            if self._pending_slots.acquire(timeout=min(0.05, remaining)):
                return

    def _request_headers(
        self,
        *,
        verb: str,
        has_payload: bool,
        last_event_id: Optional[str],
        allow_closing: bool,
    ) -> tuple[dict[str, str], tuple[str, ...]]:
        with self._lock:
            if self._closing and not allow_closing:
                raise McpTransportError("server_stopped", "MCP server is stopped")
            if self._fatal is not None and not allow_closing:
                raise self._fatal
            session_id = self._session_id
            protocol_version = self.protocol_version
            remembered_event_id = self._last_event_id
        headers = {
            "Accept": "text/event-stream" if verb == "GET" else "application/json, text/event-stream",
            "User-Agent": f"{CLIENT_NAME}/{CLIENT_VERSION}",
        }
        if has_payload:
            headers["Content-Type"] = "application/json"
        if session_id is not None:
            headers["Mcp-Session-Id"] = session_id
        if protocol_version is not None:
            headers["MCP-Protocol-Version"] = protocol_version
        if last_event_id is not None:
            headers["Last-Event-ID"] = last_event_id
        token: Optional[str] = None
        if self.config.auth is not None:
            try:
                token = self._credential_provider.bearer_token(
                    self.config.auth.credential, server_id=self.config.id
                )
            except Exception:
                raise McpAuthenticationError(
                    "authentication_unavailable",
                    "MCP authentication is unavailable from the configured credential adapter",
                    permanent=True,
                ) from None
            if not _valid_bearer_token(token):
                raise McpAuthenticationError(
                    "authentication_unavailable",
                    "MCP authentication is unavailable from the configured credential adapter",
                    permanent=True,
                )
            headers["Authorization"] = f"Bearer {token}"
        return headers, self._redactions((token, session_id, remembered_event_id, last_event_id))

    def _consume_session_header(
        self, response: http.client.HTTPResponse, *, accept_session: bool
    ) -> Optional[str]:
        values = response.headers.get_all("Mcp-Session-Id", [])
        if not values:
            return None
        if len(values) != 1:
            raise McpProtocolError(
                "invalid_session_identity",
                "MCP server returned multiple session identities",
                permanent=True,
            )
        value = _validate_session_id(values[0])
        with self._lock:
            existing = self._session_id
            if accept_session:
                if existing is not None and existing != value:
                    raise McpProtocolError(
                        "session_identity_changed",
                        "MCP server changed the negotiated session identity",
                        permanent=True,
                    )
                self._session_id = value
            elif existing is None or existing != value:
                raise McpProtocolError(
                    "session_identity_changed",
                    "MCP server changed the negotiated session identity",
                    permanent=True,
                )
        return value

    def _remember_event_id(self, event_id: Optional[str]) -> None:
        if event_id is None:
            return
        with self._lock:
            self._last_event_id = event_id

    def _redactions(self, values: tuple[Optional[str], ...]) -> tuple[str, ...]:
        return tuple(value for value in values if isinstance(value, str) and value)

    def _connection(self, timeout: float) -> http.client.HTTPConnection:
        if self._endpoint.scheme == "https":
            return http.client.HTTPSConnection(
                self._endpoint.host,
                self._endpoint.port,
                timeout=timeout,
                context=ssl.create_default_context(),
            )
        return http.client.HTTPConnection(self._endpoint.host, self._endpoint.port, timeout=timeout)

    def _wait_for_resumption(
        self, operation: _HttpOperation, deadline: float, retry_ms: Optional[int]
    ) -> None:
        delay = (retry_ms or 0) / 1000
        while delay > 0:
            _check_operation_deadline(operation, deadline)
            step = min(0.05, delay, max(0.0, deadline - time.monotonic()))
            if step <= 0:
                _check_operation_deadline(operation, deadline)
            time.sleep(step)
            delay -= step

    def _status_error(
        self,
        status: int,
        response: http.client.HTTPResponse,
        *,
        phase: str,
        expected_id: Optional[int],
    ) -> McpError:
        ambiguous = expected_id is not None
        if 300 <= status < 400:
            return McpProtocolError(
                "redirect_rejected",
                "MCP HTTP endpoint returned a redirect, which the configured origin policy rejects",
                permanent=True,
            )
        if status in {401, 403}:
            return McpAuthenticationError(
                "authentication_required" if status == 401 else "authentication_denied",
                "MCP HTTP authentication was required or denied",
                permanent=True,
            )
        if phase == "resume" and status == 405:
            return McpTransportError(
                "sse_resumption_unavailable",
                "MCP server did not allow SSE response resumption",
                ambiguous=ambiguous,
            )
        if phase == "delete" and status == 405:
            return McpTransportError("session_delete_unsupported", "MCP session deletion is unsupported")
        if status == 404:
            with self._lock:
                had_session = self._session_id is not None
                self._session_id = None
            if had_session:
                return McpTransportError(
                    "session_expired",
                    "MCP HTTP session expired and requires a fresh connection",
                    ambiguous=ambiguous,
                )
            return McpProtocolError(
                "endpoint_not_found", "configured MCP HTTP endpoint was not found", permanent=True
            )
        if status == 429:
            return McpTransportError(
                "http_rate_limited",
                "MCP HTTP endpoint rate limited the connection",
                ambiguous=ambiguous,
                retry_after_ms=_retry_after_ms(response, self.limits.backoff_max_ms),
            )
        if 500 <= status < 600:
            return McpTransportError(
                "http_server_error", "MCP HTTP endpoint returned a transient server error", ambiguous=ambiguous
            )
        if status in {400, 405, 406, 415} or 400 <= status < 500:
            return McpProtocolError(
                "http_request_rejected", "MCP HTTP endpoint rejected the protocol request", permanent=True
            )
        return McpTransportError(
            "http_status_invalid", "MCP HTTP endpoint returned an invalid status", ambiguous=ambiguous
        )

    def _fail(self, error: McpError) -> None:
        callback: Optional[Callable[["McpStreamableHttpClient", McpError], None]]
        with self._lock:
            if self._closing or self._fatal is not None:
                return
            self._fatal = error
            operations = tuple(self._operations)
            callback = self.on_failure
        for operation in operations:
            operation.abort()
        if callback is not None:
            try:
                callback(self, error)
            except Exception:
                pass


_MISSING = object()
_JSONRPC_ENVELOPE_VALUES = frozenset({"jsonrpc", "id", "method"})


def _endpoint(url: str) -> _Endpoint:
    parts = urlsplit(url)
    host = parts.hostname
    if host is None:  # Config validation makes this unreachable for normal callers.
        raise ValueError("Streamable HTTP endpoint has no host")
    return _Endpoint(
        scheme=parts.scheme,
        host=host,
        port=parts.port,
        target=parts.path or "/",
    )


def _encode_message(message: Mapping[str, Any], maximum: int) -> bytes:
    try:
        payload = json.dumps(
            message, ensure_ascii=False, separators=(",", ":"), allow_nan=False
        ).encode("utf-8")
    except (TypeError, ValueError) as error:
        raise McpProtocolError(
            "invalid_outbound", "bridge could not encode an MCP request"
        ) from error
    if len(payload) > maximum:
        raise McpProtocolError("outbound_too_large", "MCP request exceeded the frame limit")
    return payload


def _remaining_timeout(deadline: float) -> float:
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise McpTimeout("request_timeout", "MCP HTTP request timed out")
    return max(0.001, remaining)


def _check_operation_deadline(operation: _HttpOperation, deadline: float) -> None:
    if operation.aborted:
        raise McpTransportError("operation_interrupted", "MCP HTTP operation was interrupted")
    if time.monotonic() >= deadline:
        raise McpTimeout("request_timeout", "MCP HTTP request timed out")


def _content_type(response: http.client.HTTPResponse) -> str:
    value = response.getheader("Content-Type")
    if value is None:
        return ""
    return value.split(";", 1)[0].strip().lower()


def _validate_content_length(response: http.client.HTTPResponse, maximum: int) -> None:
    values = response.headers.get_all("Content-Length", [])
    if not values:
        return
    if len(values) != 1 or not values[0].isdigit():
        raise McpProtocolError(
            "invalid_content_length", "MCP HTTP response had an invalid content length", permanent=True
        )
    if len(values[0]) > 10 or int(values[0]) > maximum:
        raise McpProtocolError(
            "http_body_too_large", "MCP HTTP response exceeded the frame limit", permanent=True
        )


def _read_bounded_body(
    response: http.client.HTTPResponse,
    operation: _HttpOperation,
    deadline: float,
    maximum: int,
) -> bytes:
    _validate_content_length(response, maximum)
    chunks: list[bytes] = []
    total = 0
    while True:
        _check_operation_deadline(operation, deadline)
        try:
            chunk = response.read(min(64 * 1024, maximum + 1 - total))
        except (socket.timeout, TimeoutError) as error:
            raise McpTimeout("request_timeout", "MCP HTTP request timed out") from error
        if not chunk:
            break
        total += len(chunk)
        if total > maximum:
            raise McpProtocolError(
                "http_body_too_large", "MCP HTTP response exceeded the frame limit", permanent=True
            )
        chunks.append(chunk)
    return b"".join(chunks)


def _discard_bounded_body(
    response: http.client.HTTPResponse,
    operation: _HttpOperation,
    deadline: float,
    maximum: int,
) -> None:
    _read_bounded_body(response, operation, deadline, maximum)


def _close_response(response: http.client.HTTPResponse) -> None:
    try:
        response.close()
    except OSError:
        pass


def _decode_json_message(raw: bytes, redactions: tuple[str, ...]) -> dict[str, Any]:
    try:
        value = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError, RecursionError) as error:
        raise McpProtocolError(
            "malformed_http_body", "MCP HTTP response contained malformed JSON", permanent=True
        ) from error
    try:
        value = _redact_jsonrpc_message(value, redactions)
    except RecursionError as error:
        raise McpProtocolError(
            "invalid_http_body", "MCP HTTP response exceeded the nesting limit", permanent=True
        ) from error
    if not isinstance(value, dict) or value.get("jsonrpc") != "2.0":
        raise McpProtocolError(
            "invalid_http_body", "MCP HTTP response was not a JSON-RPC message", permanent=True
        )
    return value


def _matching_response_id(message: Mapping[str, Any], request_id: int) -> bool:
    value = message.get("id")
    return isinstance(value, int) and not isinstance(value, bool) and value == request_id


def _validate_result_size(result: Any, maximum: int) -> None:
    try:
        encoded = json.dumps(result, separators=(",", ":"), allow_nan=False).encode("utf-8")
    except (TypeError, ValueError, RecursionError) as error:
        raise McpProtocolError(
            "invalid_response", "MCP result was not valid JSON", permanent=True
        ) from error
    if len(encoded) > maximum:
        raise McpProtocolError("result_too_large", "MCP result exceeded the configured result limit")


def _validate_session_id(value: str) -> str:
    if (
        not value
        or len(value.encode("utf-8")) > MAX_HTTP_SESSION_ID_BYTES
        or any(ord(character) < 33 or ord(character) > 126 for character in value)
    ):
        raise McpProtocolError(
            "invalid_session_identity", "MCP server returned an invalid session identity", permanent=True
        )
    return value


def _validate_event_id(value: str) -> Optional[str]:
    if not value:
        return None
    if (
        len(value.encode("utf-8")) > MAX_HTTP_EVENT_ID_BYTES
        or any(ord(character) < 33 or ord(character) > 126 for character in value)
    ):
        raise McpProtocolError(
            "invalid_event_identity", "MCP server returned an invalid SSE event identity", permanent=True
        )
    return value


def _valid_bearer_token(value: Any) -> bool:
    return (
        isinstance(value, str)
        and bool(value)
        and len(value.encode("utf-8")) <= MAX_CREDENTIAL_BYTES
        and all(33 <= ord(character) <= 126 for character in value)
    )


def _redact_text(value: str, redactions: tuple[str, ...]) -> str:
    for secret in redactions:
        value = value.replace(secret, "[redacted]")
    return value


def _redact_jsonrpc_message(value: Any, redactions: tuple[str, ...]) -> Any:
    """Redact payload values without corrupting the JSON-RPC envelope keys."""

    if not isinstance(value, dict):
        return _redact_value(value, redactions)
    return {
        key: item if key in _JSONRPC_ENVELOPE_VALUES else _redact_value(item, redactions)
        for key, item in value.items()
    }


def _redact_value(value: Any, redactions: tuple[str, ...]) -> Any:
    if isinstance(value, str):
        return _redact_text(value, redactions)
    if isinstance(value, list):
        return [_redact_value(item, redactions) for item in value]
    if isinstance(value, dict):
        return {
            _redact_text(key, redactions) if isinstance(key, str) else key: _redact_value(item, redactions)
            for key, item in value.items()
        }
    return value


def _retry_after_ms(response: http.client.HTTPResponse, maximum: int) -> Optional[int]:
    value = response.getheader("Retry-After")
    if value is None or not value.isascii() or not value.isdecimal():
        return None
    if len(value) > 10:
        return maximum
    return min(int(value) * 1000, maximum)
