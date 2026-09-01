"""Public Python API for Ygg executable extensions."""

from __future__ import annotations

import base64
import contextvars
import hashlib
import inspect
import json
import os
import queue
import threading
import time
from collections import OrderedDict
from collections.abc import Mapping, Sequence
from concurrent.futures import Future, ThreadPoolExecutor
from dataclasses import dataclass, field
from typing import Any, Callable, Optional

from .protocol import (
    DEFAULT_API_VERSION,
    DEFAULT_MAX_MESSAGE_BYTES,
    JsonRpcTransport,
    Logger,
    ProtocolError,
    RpcError,
)


Handler = Callable[..., Any]
_MISSING = object()
REQUEST_CANCELLED = -32800
MAX_INPUT_PROMPT_BYTES = 16 * 1024
MAX_INPUT_VALUE_BYTES = 256 * 1024
MAX_SECRET_VALUE_BYTES = 64 * 1024
MAX_TOOL_CONTENT_PARTS = 256
MAX_DYNAMIC_TOOLS = 256
MAX_TOOL_CATALOG_REVISIONS = 8
MAX_PRESENTATION_BYTES = 256 * 1024
MAX_PRESENTATION_ACTIVITIES = 128
MAX_PRESENTATION_NODES = 256
MAX_PRESENTATION_ACTIONS = 64
MAX_PRESENTATION_REVISION = (2**53) - 1
API_V02_FEATURES = (
    "request_cancellation",
    "request_progress",
    "content_parts",
    "artifacts",
    "lifecycle_events",
    "policy_intents",
    "dynamic_tools",
    "agent_sessions",
    "delegation_telemetry_v1",
    "approvals",
    "secrets",
)
LIFECYCLE_METHODS = (
    "session/started",
    "session/settled",
    "turn/started",
    "turn/settled",
    "tool/started",
    "tool/settled",
)


class CancelledError(RpcError):
    """The host cancelled an in-flight extension request."""

    def __init__(self, reason: str = "cancelled") -> None:
        super().__init__(REQUEST_CANCELLED, "request cancelled", {"reason": str(reason)})


class CancellationToken:
    """Thread-safe cooperative cancellation token for the current request."""

    def __init__(self, request_id: Any) -> None:
        self.request_id = request_id
        self._event = threading.Event()
        self._lock = threading.Lock()
        self._reason = "cancelled"
        self._terminal = False
        self._sequence = 0

    @property
    def cancelled(self) -> bool:
        return self._event.is_set()

    @property
    def reason(self) -> Optional[str]:
        return self._reason if self.cancelled else None

    def wait(self, timeout: Optional[float] = None) -> bool:
        """Wait until cancellation and return whether it was observed."""

        return self._event.wait(timeout)

    def raise_if_cancelled(self) -> None:
        """Raise :class:`CancelledError` when cancellation was requested."""

        if self.cancelled:
            raise CancelledError(self._reason)

    def _cancel(self, reason: Any) -> bool:
        with self._lock:
            if self._terminal or self._event.is_set():
                return False
            self._reason = str(reason) if reason is not None else "cancelled"
            self._event.set()
            return True

    def _complete(self) -> bool:
        with self._lock:
            if self._terminal or self._event.is_set():
                return False
            self._terminal = True
            return True

    def _finish_cancelled(self) -> None:
        with self._lock:
            self._terminal = True

    def _next_sequence(self) -> int:
        with self._lock:
            if self._terminal:
                raise RpcError(-32000, "request has already settled")
            if self._event.is_set():
                raise CancelledError(self._reason)
            self._sequence += 1
            return self._sequence


@dataclass(frozen=True)
class _RequestScope:
    extension: "Extension"
    request_id: Any
    cancellation: CancellationToken


_CURRENT_REQUEST: contextvars.ContextVar[Optional[_RequestScope]] = contextvars.ContextVar(
    "ygg_extension_current_request", default=None
)


def current_cancellation() -> Optional[CancellationToken]:
    """Return the ambient handler cancellation token, if inside a request."""

    scope = _CURRENT_REQUEST.get()
    return scope.cancellation if scope is not None else None


def current_request_id() -> Any:
    """Return the ambient host request ID, or ``None`` outside a handler."""

    scope = _CURRENT_REQUEST.get()
    return scope.request_id if scope is not None else None


def _valid_approval_token(value: str) -> bool:
    return len(value) == 64 and all(character in "0123456789abcdef" for character in value)


def _valid_extension_identifier(value: Any) -> bool:
    if not isinstance(value, str) or not value or len(value.encode("utf-8")) > 64:
        return False
    first, rest = value[0], value[1:]
    return (first.isascii() and (first.isalpha() or first == "_")) and all(
        character.isascii()
        and (character.isalnum() or character in {"_", "-", "."})
        for character in rest
    )


def text_content(text: Any) -> dict[str, Any]:
    """Build a text content part for an API 0.2 tool result."""

    return {"type": "text", "text": str(text)}


def image_content(
    artifact_id: str,
    mime_type: str,
    *,
    alt: Optional[str] = None,
) -> dict[str, Any]:
    """Build an image content part backed by a host artifact."""

    part = _media_content("image", artifact_id, mime_type)
    if alt is not None:
        part["alt"] = str(alt)
    return part


def audio_content(
    artifact_id: str,
    mime_type: str,
    *,
    transcript: Optional[str] = None,
) -> dict[str, Any]:
    """Build an audio content part backed by a host artifact."""

    part = _media_content("audio", artifact_id, mime_type)
    if transcript is not None:
        part["transcript"] = str(transcript)
    return part


def tool_result(
    *parts: Mapping[str, Any],
    structured_content: Any = _MISSING,
    is_error: bool = False,
    metadata: Optional[Mapping[str, Any]] = None,
) -> dict[str, Any]:
    """Build an API 0.2 structured tool-result envelope."""

    result: dict[str, Any] = {
        "content": [dict(part) for part in parts],
        "is_error": bool(is_error),
        "metadata": dict(metadata) if metadata is not None else {},
    }
    if structured_content is not _MISSING:
        result["structured_content"] = structured_content
    return result


def _media_content(kind: str, artifact_id: str, mime_type: str) -> dict[str, Any]:
    if not isinstance(artifact_id, str) or not artifact_id:
        raise ValueError("artifact_id must be non-empty")
    if not isinstance(mime_type, str) or "/" not in mime_type:
        raise ValueError("mime_type must be a media type")
    return {"type": kind, "artifact_id": artifact_id, "mime_type": mime_type}


@dataclass
class _Tool:
    name: str
    description: str
    parameters: dict[str, Any]
    output_schema: Optional[dict[str, Any]]
    handler: Handler


@dataclass
class _Command:
    name: str
    description: str
    usage: Optional[str]
    handler: Handler


@dataclass
class _PendingResponse:
    event: threading.Event = field(default_factory=threading.Event)
    response: Optional[dict[str, Any]] = None
    error: Optional[RpcError] = None

    def resolve(self, response: dict[str, Any]) -> None:
        self.response = response
        self.event.set()

    def fail(self, error: RpcError) -> None:
        self.error = error
        self.event.set()


@dataclass
class _WriteItem:
    message: Mapping[str, Any]
    done: threading.Event = field(default_factory=threading.Event)
    error: Optional[BaseException] = None


@dataclass(frozen=True)
class _InboundError:
    error: RpcError


class _SerializedWriter:
    """One bounded queue and one stdout owner for complete JSON-RPC frames."""

    _STOP = object()

    def __init__(self, transport: JsonRpcTransport, capacity: int) -> None:
        if capacity <= 0:
            raise ValueError("writer_queue_size must be greater than zero")
        self._transport = transport
        self._queue: queue.Queue[Any] = queue.Queue(maxsize=capacity)
        self._closed = threading.Event()
        self._failure: Optional[BaseException] = None
        self._thread = threading.Thread(
            target=self._run,
            name="ygg-extension-writer",
            daemon=True,
        )

    def start(self) -> None:
        self._thread.start()

    def send(self, message: Mapping[str, Any]) -> None:
        item = _WriteItem(message)
        while True:
            if self._closed.is_set():
                raise self._closed_error()
            try:
                self._queue.put(item, timeout=0.05)
                break
            except queue.Full:
                continue
        while not item.done.wait(0.05):
            if self._closed.is_set() and self._failure is not None:
                raise self._closed_error()
        if item.error is not None:
            raise item.error

    def close(self) -> None:
        if self._closed.is_set():
            return
        while True:
            try:
                self._queue.put(self._STOP, timeout=0.05)
                break
            except queue.Full:
                if self._closed.is_set():
                    return
        self._thread.join(timeout=2.0)
        self._closed.set()

    def _run(self) -> None:
        try:
            while True:
                item = self._queue.get()
                if item is self._STOP:
                    return
                assert isinstance(item, _WriteItem)
                try:
                    self._transport.send(item.message)
                except ProtocolError as error:
                    # A locally invalid/oversized frame is rejected without
                    # sacrificing an otherwise healthy persistent stream.
                    item.error = error
                    item.done.set()
                    continue
                except BaseException as error:
                    item.error = error
                    self._failure = error
                    item.done.set()
                    return
                item.done.set()
        finally:
            self._closed.set()
            while True:
                try:
                    pending = self._queue.get_nowait()
                except queue.Empty:
                    break
                if isinstance(pending, _WriteItem):
                    pending.error = self._closed_error()
                    pending.done.set()

    def _closed_error(self) -> BaseException:
        if self._failure is not None:
            return self._failure
        return BrokenPipeError("extension protocol writer is closed")


class Extension:
    """Define and run a dependency-free Ygg executable extension.

    API 0.1 remains sequential and wire-compatible. API 0.2 negotiates a
    bounded worker pool while a dedicated reader continues to consume control
    frames and a single writer serializes complete stdout frames.
    """

    def __init__(
        self,
        *,
        api_version: Optional[str] = None,
        stdin: Any = None,
        stdout: Any = None,
        stderr: Any = None,
        logger: Optional[Logger] = None,
        max_message_bytes: int = DEFAULT_MAX_MESSAGE_BYTES,
        max_concurrent_requests: int = 8,
        max_pending_requests: int = 64,
        writer_queue_size: int = 64,
        shutdown_timeout: float = 2.0,
        cancellation_grace: float = 0.25,
        supported_features: Optional[Sequence[str]] = None,
    ) -> None:
        self.api_version = api_version or os.environ.get(
            "YGG_EXTENSION_API_VERSION", DEFAULT_API_VERSION
        )
        if isinstance(max_concurrent_requests, bool) or max_concurrent_requests <= 0:
            raise ValueError("max_concurrent_requests must be greater than zero")
        if isinstance(max_pending_requests, bool) or max_pending_requests <= 0:
            raise ValueError("max_pending_requests must be greater than zero")
        if shutdown_timeout < 0 or cancellation_grace < 0:
            raise ValueError("shutdown timeouts cannot be negative")
        self.stdin = stdin
        self.stdout = stdout
        self.logger = logger or Logger(stderr)
        self.log = self.logger
        self.max_message_bytes = max_message_bytes
        self.max_concurrent_requests = max_concurrent_requests
        self.max_pending_requests = max_pending_requests
        self.writer_queue_size = writer_queue_size
        self.shutdown_timeout = shutdown_timeout
        self.cancellation_grace = cancellation_grace
        selected_features = API_V02_FEATURES if supported_features is None else supported_features
        if not all(isinstance(feature, str) and feature for feature in selected_features):
            raise ValueError("supported_features must contain non-empty strings")
        unknown_features = set(selected_features) - set(API_V02_FEATURES)
        if unknown_features:
            raise ValueError(f"unknown supported_features: {sorted(unknown_features)}")
        self._supported_features = tuple(dict.fromkeys(selected_features))

        self._tools: dict[str, _Tool] = {}
        self._commands: dict[str, _Command] = {}
        self._hooks: dict[str, Handler] = {}
        self._context_handler: Optional[Handler] = None
        self._status_handlers: dict[str, Handler] = {}
        self._renderer_handlers: dict[str, Handler] = {}
        self._lifecycle_handlers: dict[str, Handler] = {}
        self._shutdown_handler: Optional[Handler] = None

        self._transport: Optional[JsonRpcTransport] = None
        self._writer: Optional[_SerializedWriter] = None
        self._executor: Optional[ThreadPoolExecutor] = None
        self._initialized = False
        self._running = False
        self._closed = False
        self._draining = False
        self._next_request_id = 1
        self._initialization: Optional[dict[str, Any]] = None
        self._declared: dict[str, Any] = {}
        self._features: frozenset[str] = frozenset()
        self._negotiated_concurrency = 1
        self._active: dict[tuple[type, Any], CancellationToken] = {}
        self._pending: dict[tuple[type, Any], _PendingResponse] = {}
        self._futures: set[Future[Any]] = set()
        self._tool_catalog_revision = 0
        self._tool_catalogs: OrderedDict[int, dict[str, _Tool]] = OrderedDict()
        self._staged_tool_catalog: Optional[dict[str, _Tool]] = None
        self._staged_tool_catalog_revision: Optional[int] = None
        self._tool_catalog_lock = threading.RLock()
        self._tool_catalog_update_lock = threading.Lock()
        self._presentation_revision: Optional[int] = None
        self._presentation_lock = threading.Lock()
        self._admission = threading.BoundedSemaphore(max_pending_requests)
        self._lock = threading.RLock()
        self._future_condition = threading.Condition(self._lock)
        self._shutdown_done = threading.Event()
        self._eof_done = threading.Event()

    @property
    def initialized(self) -> bool:
        return self._initialized

    @property
    def running(self) -> bool:
        return self._running

    @property
    def initialization(self) -> Optional[dict[str, Any]]:
        return self._initialization

    @property
    def negotiated_features(self) -> frozenset[str]:
        """The API 0.2 feature subset accepted during initialization."""

        return self._features

    @property
    def negotiated_concurrency(self) -> int:
        return self._negotiated_concurrency

    @property
    def tool_catalog_revision(self) -> int:
        """The last host revision returned by a dynamic tool mutation."""

        with self._tool_catalog_lock:
            return self._tool_catalog_revision

    @property
    def cancellation(self) -> Optional[CancellationToken]:
        """The ambient cancellation token for the calling handler thread."""

        scope = _CURRENT_REQUEST.get()
        if scope is None or scope.extension is not self:
            return None
        return scope.cancellation

    @property
    def request_id(self) -> Any:
        """The ambient host request ID for the calling handler thread."""

        scope = _CURRENT_REQUEST.get()
        return scope.request_id if scope is not None and scope.extension is self else None

    @property
    def host(self) -> dict[str, Any]:
        if not self._initialization:
            return {}
        value = self._initialization.get("host")
        return dict(value) if isinstance(value, Mapping) else {}

    @property
    def workspace(self) -> Optional[str]:
        if not self._initialization:
            return None
        value = self._initialization.get("workspace")
        return value if isinstance(value, str) else None

    def tool(
        self,
        *,
        name: str,
        description: str,
        parameters: Optional[Mapping[str, Any]] = None,
        output_schema: Optional[Mapping[str, Any]] = None,
    ) -> Callable[[Handler], Handler]:
        self._validate_name("tool", name)
        if not isinstance(description, str) or not description.strip():
            raise ValueError("tool description must be non-empty")
        schema = dict(parameters) if parameters is not None else {"type": "object"}
        result_schema = dict(output_schema) if output_schema is not None else None
        if result_schema is not None and self.api_version != "0.2":
            raise ValueError("output_schema requires extension API 0.2")

        def decorate(handler: Handler) -> Handler:
            if name in self._tools:
                raise ValueError(f"duplicate tool: {name}")
            self._tools[name] = _Tool(name, description, schema, result_schema, handler)
            return handler

        return decorate

    def command(
        self,
        *,
        name: str,
        description: str,
        usage: Optional[str] = None,
    ) -> Callable[[Handler], Handler]:
        self._validate_name("command", name)
        if not isinstance(description, str) or not description.strip():
            raise ValueError("command description must be non-empty")
        if usage is not None and not isinstance(usage, str):
            raise TypeError("command usage must be a string or None")

        def decorate(handler: Handler) -> Handler:
            if name in self._commands:
                raise ValueError(f"duplicate command: {name}")
            self._commands[name] = _Command(name, description, usage, handler)
            return handler

        return decorate

    def hook(self, name: str) -> Callable[[Handler], Handler]:
        self._validate_name("hook", name)

        def decorate(handler: Handler) -> Handler:
            if name in self._hooks:
                raise ValueError(f"duplicate hook: {name}")
            self._hooks[name] = handler
            return handler

        return decorate

    def context(self, handler: Optional[Handler] = None) -> Any:
        def decorate(callback: Handler) -> Handler:
            if self._context_handler is not None:
                raise ValueError("duplicate context handler")
            self._context_handler = callback
            return callback

        return decorate(handler) if handler is not None else decorate

    def status(self, surface: Any = "status") -> Any:
        if callable(surface):
            return self._register_status("status", surface)
        self._validate_name("UI surface", surface)

        def decorate(handler: Handler) -> Handler:
            return self._register_status(surface, handler)

        return decorate

    def renderer(self, name: str) -> Callable[[Handler], Handler]:
        self._validate_name("renderer", name)

        def decorate(handler: Handler) -> Handler:
            if name in self._renderer_handlers:
                raise ValueError(f"duplicate renderer: {name}")
            self._renderer_handlers[name] = handler
            return handler

        return decorate

    tool_renderer = renderer

    def on_lifecycle(self, event: str) -> Callable[[Handler], Handler]:
        """Subscribe to an observational API 0.2 lifecycle notification."""

        method = self._lifecycle_method(event)

        def decorate(handler: Handler) -> Handler:
            if method in self._lifecycle_handlers:
                raise ValueError(f"duplicate lifecycle handler: {method}")
            self._lifecycle_handlers[method] = handler
            return handler

        return decorate

    lifecycle = on_lifecycle

    def on_shutdown(self, handler: Handler) -> Handler:
        if self._shutdown_handler is not None:
            raise ValueError("duplicate shutdown handler")
        self._shutdown_handler = handler
        return handler

    def notify(
        self,
        message: str,
        *,
        level: str = "info",
        title: Optional[str] = None,
    ) -> None:
        if level not in {"info", "success", "warning", "error"}:
            raise ValueError(f"unknown notification level: {level}")
        self._require_capability("notifications")
        params: dict[str, Any] = {"level": level, "message": str(message)}
        if title is not None:
            params["title"] = str(title)
        self._send({"jsonrpc": "2.0", "method": "notification", "params": params})

    send_notification = notify

    def publish_presentation(
        self,
        snapshot: Mapping[str, Any],
        *,
        resource_owner: Optional[Mapping[str, Any]] = None,
    ) -> None:
        """Publish one complete, monotonic API 0.2 semantic UI snapshot.

        Calls made inside a host request are owner-correlated automatically.
        Background publishers may pass the complete host-derived
        ``context["resource_owner"]`` triple; omitting both produces
        process-scoped state.
        """

        if self.api_version != "0.2":
            raise RpcError(-32601, "semantic presentation requires extension API 0.2")
        self._require_capability("presentation")
        if not isinstance(snapshot, Mapping):
            raise TypeError("presentation snapshot must be an object")
        payload = dict(snapshot)
        allowed = {"revision", "status", "activities", "collection", "actions"}
        unknown = set(payload) - allowed
        if unknown:
            raise ValueError(f"unknown presentation snapshot fields: {sorted(unknown)}")
        revision = payload.get("revision")
        if (
            not isinstance(revision, int)
            or isinstance(revision, bool)
            or revision < 0
            or revision > MAX_PRESENTATION_REVISION
        ):
            raise ValueError("presentation revision must be a portable non-negative integer")
        activities = payload.get("activities", [])
        actions = payload.get("actions", [])
        collection = payload.get("collection")
        if not isinstance(activities, list) or len(activities) > MAX_PRESENTATION_ACTIVITIES:
            raise ValueError(
                f"presentation activities must be an array of at most {MAX_PRESENTATION_ACTIVITIES} items"
            )
        if not isinstance(actions, list) or len(actions) > MAX_PRESENTATION_ACTIONS:
            raise ValueError(
                f"presentation actions must be an array of at most {MAX_PRESENTATION_ACTIONS} items"
            )
        if collection is not None:
            if not isinstance(collection, Mapping):
                raise ValueError("presentation collection must be an object or null")
            nodes = collection.get("nodes", [])
            if not isinstance(nodes, list) or len(nodes) > MAX_PRESENTATION_NODES:
                raise ValueError(
                    f"presentation collection nodes must contain at most {MAX_PRESENTATION_NODES} items"
                )
        try:
            encoded = json.dumps(payload, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
        except (TypeError, ValueError) as error:
            raise ValueError("presentation snapshot must be JSON-serializable") from error
        if len(encoded) > MAX_PRESENTATION_BYTES:
            raise ValueError(
                f"presentation snapshot is {len(encoded)} bytes; limit is {MAX_PRESENTATION_BYTES}"
            )
        scope = _CURRENT_REQUEST.get()
        parent_request_id = scope.request_id if scope is not None else None
        owner_payload: Optional[dict[str, Any]] = None
        if resource_owner is not None:
            if not isinstance(resource_owner, Mapping):
                raise TypeError("presentation resource_owner must be an object")
            owner_payload = dict(resource_owner)
            if set(owner_payload) != {
                "session_id",
                "extension_instance_id",
                "process_generation",
            }:
                raise ValueError("presentation resource_owner must contain the exact owner triple")
            if any(
                not isinstance(owner_payload[field], str) or not owner_payload[field].strip()
                for field in ("session_id", "extension_instance_id")
            ) or (
                not isinstance(owner_payload["process_generation"], int)
                or isinstance(owner_payload["process_generation"], bool)
                or owner_payload["process_generation"] < 1
            ):
                raise ValueError("presentation resource_owner is invalid")
        params: dict[str, Any] = {"snapshot": payload}
        if owner_payload is not None:
            params["resource_owner"] = owner_payload
        elif parent_request_id is not None:
            params["parent_request_id"] = parent_request_id
        with self._presentation_lock:
            if self._presentation_revision is not None and revision <= self._presentation_revision:
                raise ValueError("presentation revision must increase monotonically")
            self._send(
                {
                    "jsonrpc": "2.0",
                    "method": "presentation/update",
                    "params": params,
                }
            )
            self._presentation_revision = revision

    presentation = publish_presentation

    def request(
        self,
        method: str,
        params: Optional[Mapping[str, Any]] = None,
        *,
        parent_request_id: Any = _MISSING,
        operation_scoped: bool = False,
    ) -> Any:
        """Send a request to the host while the reader keeps dispatching frames."""

        return self._request(
            method,
            params,
            parent_request_id=parent_request_id,
            operation_scoped=operation_scoped,
            correlate_parent=True,
            cancel_with_parent=True,
        )

    def _request(
        self,
        method: str,
        params: Optional[Mapping[str, Any]],
        *,
        parent_request_id: Any,
        operation_scoped: bool,
        correlate_parent: bool,
        cancel_with_parent: bool,
    ) -> Any:
        """Send one host request with explicit ownership semantics."""

        if not isinstance(method, str) or not method:
            raise ValueError("request method must be non-empty")
        self._require_initialized()
        payload = dict(params) if params is not None else {}
        if self.api_version == "0.2" and correlate_parent:
            parent = self._resolve_parent(parent_request_id, required=operation_scoped)
            if parent is not _MISSING:
                payload["parent_request_id"] = parent
        with self._lock:
            sequence = self._next_request_id
            self._next_request_id += 1
            # Each JSON-RPC direction owns an ID namespace, but a distinct
            # prefix also removes any ambiguity in bidirectional cancellation.
            request_id: Any = sequence if self.api_version == "0.1" else f"py:{sequence}"
            key = self._id_key(request_id)
            pending = _PendingResponse()
            self._pending[key] = pending
        try:
            self._send(
                {
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "method": method,
                    "params": payload,
                }
            )
            scope = _CURRENT_REQUEST.get() if cancel_with_parent else None
            cancellation_sent = False
            while not pending.event.wait(0.05):
                if self._closed:
                    raise RpcError(-32000, "protocol transport is closed")
                if scope is not None and scope.cancellation.cancelled and not cancellation_sent:
                    cancellation_sent = True
                    self._send(
                        {
                            "jsonrpc": "2.0",
                            "method": "$/cancelRequest",
                            "params": {
                                "id": request_id,
                                "reason": scope.cancellation.reason or "parent_cancelled",
                            },
                        }
                    )
                    raise CancelledError(scope.cancellation.reason or "parent_cancelled")
            if pending.error is not None:
                raise pending.error
            response = pending.response
            if response is None:
                raise RpcError(-32603, "missing JSON-RPC response")
            if "error" in response:
                raise RpcError.from_response(response)
            return response.get("result")
        finally:
            with self._lock:
                self._pending.pop(key, None)

    def register_tool(
        self,
        *,
        name: str,
        description: str,
        handler: Handler,
        parameters: Optional[Mapping[str, Any]] = None,
        output_schema: Optional[Mapping[str, Any]] = None,
    ) -> dict[str, Any]:
        """Add or replace one live API 0.2 tool and its local handler."""

        return self.register_tools(
            [
                {
                    "name": name,
                    "description": description,
                    "parameters": parameters,
                    "output_schema": output_schema,
                    "handler": handler,
                }
            ]
        )

    def register_tools(self, tools: Sequence[Mapping[str, Any]]) -> dict[str, Any]:
        """Transactionally add or replace live tools through ``tools/register``."""

        self._require_feature("dynamic_tools")
        if isinstance(tools, (str, bytes)) or not isinstance(tools, Sequence):
            raise TypeError("tools must be a sequence of tool definitions")
        if not tools:
            raise ValueError("tools/register requires at least one tool")
        if len(tools) > MAX_DYNAMIC_TOOLS:
            raise ValueError(f"tools/register accepts at most {MAX_DYNAMIC_TOOLS} tools")

        registrations: list[_Tool] = []
        names: set[str] = set()
        allowed = {"name", "description", "parameters", "output_schema", "handler"}
        for registration in tools:
            if not isinstance(registration, Mapping):
                raise TypeError("dynamic tool definitions must be objects")
            unknown = set(registration) - allowed
            if unknown:
                raise ValueError(
                    f"unknown dynamic tool fields: {sorted(map(str, unknown))}"
                )
            if "name" not in registration or "description" not in registration:
                raise ValueError("dynamic tools require name and description")
            if "handler" not in registration or not callable(registration["handler"]):
                raise TypeError("dynamic tools require a callable handler")
            name = registration["name"]
            description = registration["description"]
            self._validate_name("tool", name)
            if name in names:
                raise ValueError(f"duplicate dynamic tool: {name}")
            names.add(name)
            if not isinstance(description, str) or not description.strip():
                raise ValueError("tool description must be non-empty")
            parameters = registration.get("parameters")
            if parameters is None:
                parameter_schema = {"type": "object"}
            elif isinstance(parameters, Mapping):
                parameter_schema = dict(parameters)
            else:
                raise TypeError("tool parameters must be an object or None")
            output_schema = registration.get("output_schema")
            if output_schema is not None and not isinstance(output_schema, Mapping):
                raise TypeError("tool output_schema must be an object or None")
            registrations.append(
                _Tool(
                    name=name,
                    description=description,
                    parameters=parameter_schema,
                    output_schema=(dict(output_schema) if output_schema is not None else None),
                    handler=registration["handler"],
                )
            )

        definitions = [self._tool_definition(tool) for tool in registrations]
        with self._tool_catalog_update_lock:
            with self._tool_catalog_lock:
                staged = dict(self._tools)
                for tool in registrations:
                    staged[tool.name] = tool
                if len(staged) > MAX_DYNAMIC_TOOLS:
                    raise ValueError(f"live tool catalog exceeds {MAX_DYNAMIC_TOOLS} tools")
                staged_revision = self._tool_catalog_revision + 1
                self._staged_tool_catalog = staged
                self._staged_tool_catalog_revision = staged_revision
            try:
                result = self._request(
                    "tools/register",
                    {"tools": definitions},
                    parent_request_id=_MISSING,
                    operation_scoped=False,
                    correlate_parent=False,
                    cancel_with_parent=False,
                )
                response = self._validate_catalog_update(result, staged)
            except BaseException:
                with self._tool_catalog_lock:
                    self._discard_staged_tool_catalog()
                raise
            with self._tool_catalog_lock:
                self._commit_staged_tool_catalog(response)
            return response

    def unregister_tool(self, name: str) -> dict[str, Any]:
        """Remove one live API 0.2 tool."""

        return self.unregister_tools(name)

    def unregister_tools(self, *names: str) -> dict[str, Any]:
        """Transactionally remove live tools through ``tools/unregister``."""

        self._require_feature("dynamic_tools")
        if not names:
            raise ValueError("tools/unregister requires at least one tool name")
        if len(names) > MAX_DYNAMIC_TOOLS:
            raise ValueError(f"tools/unregister accepts at most {MAX_DYNAMIC_TOOLS} names")
        if len(set(names)) != len(names):
            raise ValueError("tools/unregister contains duplicate names")
        for name in names:
            self._validate_name("tool", name)

        with self._tool_catalog_update_lock:
            with self._tool_catalog_lock:
                staged = {
                    name: tool for name, tool in self._tools.items() if name not in names
                }
                staged_revision = self._tool_catalog_revision + 1
                self._staged_tool_catalog = staged
                self._staged_tool_catalog_revision = staged_revision
            try:
                result = self._request(
                    "tools/unregister",
                    {"names": list(names)},
                    parent_request_id=_MISSING,
                    operation_scoped=False,
                    correlate_parent=False,
                    cancel_with_parent=False,
                )
                response = self._validate_catalog_update(result, staged)
            except BaseException:
                with self._tool_catalog_lock:
                    self._discard_staged_tool_catalog()
                raise
            with self._tool_catalog_lock:
                self._commit_staged_tool_catalog(response)
            return response

    def confirm(
        self,
        prompt: str,
        *,
        detail: Optional[str] = None,
        destructive: bool = False,
        default: bool = False,
        parent_request_id: Any = _MISSING,
    ) -> bool:
        self._require_capability("confirmations")
        params: dict[str, Any] = {
            "prompt": str(prompt),
            "destructive": bool(destructive),
            "default": bool(default),
        }
        if detail is not None:
            params["detail"] = str(detail)
        result = self.request(
            "confirmation/request",
            params,
            parent_request_id=parent_request_id,
            operation_scoped=self.api_version == "0.2",
        )
        if not isinstance(result, Mapping) or not isinstance(result.get("confirmed"), bool):
            raise RpcError(-32603, "invalid confirmation response")
        return bool(result["confirmed"])

    def request_input(
        self,
        prompt: str,
        *,
        secret: bool = False,
        parent_request_id: Any = _MISSING,
    ) -> Optional[str]:
        """Request ephemeral text input owned by the active API 0.2 request.

        ``None`` means the frontend cancelled or could not answer. Secret
        values use the same private response path and are never logged by the
        SDK; callers should still keep the returned Python string short-lived.
        """

        if not isinstance(prompt, str):
            raise TypeError("input prompt must be a string")
        if not prompt.strip():
            raise ValueError("input prompt must be non-empty")
        if len(prompt.encode("utf-8")) > min(
            self.max_message_bytes, MAX_INPUT_PROMPT_BYTES
        ):
            raise ValueError("input prompt exceeds the 16 KiB bound")
        if not isinstance(secret, bool):
            raise TypeError("input secret must be a boolean")
        self._require_initialized()
        if self.api_version != "0.2":
            raise RpcError(-32601, "input/request requires extension API 0.2")
        result = self.request(
            "input/request",
            {"prompt": prompt, "secret": secret},
            parent_request_id=parent_request_id,
            operation_scoped=True,
        )
        if not isinstance(result, Mapping) or set(result) != {"value"}:
            raise RpcError(-32603, "invalid input response")
        value = result["value"]
        if value is not None and not isinstance(value, str):
            raise RpcError(-32603, "invalid input response")
        if value is not None and len(value.encode("utf-8")) > min(
            self.max_message_bytes, MAX_INPUT_VALUE_BYTES
        ):
            raise RpcError(-32603, "input response exceeds the 256 KiB bound")
        return value

    def progress(
        self,
        event: Any = None,
        *,
        message: Optional[str] = None,
        current: Any = _MISSING,
        total: Any = _MISSING,
        unit: Optional[str] = None,
        request_id: Any = _MISSING,
    ) -> int:
        """Send monotonic request-scoped API 0.2 progress and return its sequence."""

        self._require_feature("request_progress")
        scope = _CURRENT_REQUEST.get()
        if request_id is _MISSING:
            if scope is None or scope.extension is not self:
                raise RpcError(-32602, "progress requires an active parent request")
            token = scope.cancellation
            request_id = scope.request_id
        else:
            with self._lock:
                token = self._active.get(self._id_key(request_id))
            if token is None:
                raise RpcError(-32602, "unknown progress request_id")
        if event is None:
            if message is None:
                raise ValueError("progress requires an event or status message")
            value: dict[str, Any] = {"type": "status", "message": str(message)}
            if current is not _MISSING:
                value["current"] = current
            if total is not _MISSING:
                value["total"] = total
            if unit is not None:
                value["unit"] = str(unit)
            event = value
        if not isinstance(event, Mapping):
            raise TypeError("progress event must be an object")
        normalized = self._validate_progress_event(dict(event))
        sequence = token._next_sequence()
        self._send(
            {
                "jsonrpc": "2.0",
                "method": "$/progress",
                "params": {
                    "request_id": request_id,
                    "sequence": sequence,
                    "event": normalized,
                },
            }
        )
        return sequence

    report_progress = progress

    def publish_artifact(
        self,
        *,
        mime_type: str,
        data: Any = _MISSING,
        path: Any = _MISSING,
        size: Optional[int] = None,
        sha256: Optional[str] = None,
        parent_request_id: Any = _MISSING,
    ) -> str:
        """Publish bounded inline bytes or a relative scratch artifact to Ygg."""

        if not isinstance(mime_type, str) or "/" not in mime_type:
            raise ValueError("mime_type must be a media type")
        if (data is _MISSING) == (path is _MISSING):
            raise ValueError("provide exactly one of data or path")
        params: dict[str, Any] = {"mime_type": mime_type}
        if data is not _MISSING:
            if isinstance(data, str):
                raw = data.encode("utf-8")
            elif isinstance(data, (bytes, bytearray, memoryview)):
                raw = bytes(data)
            else:
                raise TypeError("artifact data must be bytes or a string")
            actual_digest = hashlib.sha256(raw).hexdigest()
            if size is not None and size != len(raw):
                raise ValueError("artifact size does not match inline data")
            if sha256 is not None and sha256.lower() != actual_digest:
                raise ValueError("artifact sha256 does not match inline data")
            params.update(
                {
                    "size": len(raw),
                    "sha256": actual_digest,
                    "data": {
                        "encoding": "base64",
                        "data": base64.b64encode(raw).decode("ascii"),
                    },
                }
            )
        else:
            if not isinstance(path, str) or not path:
                raise ValueError("artifact path must be non-empty")
            if os.path.isabs(path) or ".." in path.replace("\\", "/").split("/"):
                raise ValueError("artifact path must be relative and cannot traverse parents")
            if not isinstance(size, int) or isinstance(size, bool) or size < 0:
                raise ValueError("scratch artifact size must be a non-negative integer")
            if not isinstance(sha256, str) or len(sha256) != 64:
                raise ValueError("scratch artifact sha256 must be a 64-character digest")
            try:
                int(sha256, 16)
            except ValueError as error:
                raise ValueError("scratch artifact sha256 must be hexadecimal") from error
            params.update({"path": path, "size": size, "sha256": sha256.lower()})
        self._require_feature("artifacts")
        result = self.request(
            "artifact/publish",
            params,
            parent_request_id=parent_request_id,
            operation_scoped=True,
        )
        if not isinstance(result, Mapping) or not isinstance(result.get("artifact_id"), str):
            raise RpcError(-32603, "invalid artifact publication response")
        return result["artifact_id"]

    def evaluate_policy(
        self,
        intent: Mapping[str, Any],
        *,
        approval_token: Optional[str] = None,
        parent_request_id: Any = _MISSING,
    ) -> dict[str, Any]:
        """Ask the host to classify a structured, non-authoritative action intent."""

        if not isinstance(intent, Mapping):
            raise TypeError("policy intent must be an object")
        value = dict(intent)
        if not isinstance(value.get("kind"), str) or not value["kind"].strip():
            raise ValueError("policy intent kind must be non-empty")
        if not isinstance(value.get("operation"), str) or not value["operation"].strip():
            raise ValueError("policy intent operation must be non-empty")
        if not isinstance(value.get("target"), Mapping):
            raise ValueError("policy intent target must be an object")
        if "data_classes" in value and (
            not isinstance(value["data_classes"], list)
            or not all(isinstance(item, str) for item in value["data_classes"])
        ):
            raise ValueError("policy intent data_classes must be an array of strings")
        if "adapter_hints" in value and not isinstance(value["adapter_hints"], Mapping):
            raise ValueError("policy intent adapter_hints must be an object")
        unknown = set(value) - {
            "kind",
            "operation",
            "target",
            "data_classes",
            "adapter_hints",
        }
        if unknown:
            raise ValueError(f"unknown policy intent fields: {sorted(unknown)}")
        hints = value.get("adapter_hints", {})
        if isinstance(hints, Mapping):
            unknown_hints = set(hints) - {"read_only", "destructive"}
            if unknown_hints:
                raise ValueError(f"unknown adapter_hints fields: {sorted(unknown_hints)}")
            if any(
                hint is not None and not isinstance(hint, bool)
                for hint in hints.values()
            ):
                raise ValueError("policy adapter hints must be booleans or null")
        self._require_feature("policy_intents")
        params: dict[str, Any] = {"intent": value}
        if approval_token is not None:
            if not isinstance(approval_token, str) or not _valid_approval_token(
                approval_token
            ):
                raise ValueError(
                    "approval_token must be 64 lowercase hexadecimal characters"
                )
            self._require_feature("approvals")
            params["approval_token"] = approval_token
        result = self.request(
            "policy/evaluate",
            params,
            parent_request_id=parent_request_id,
            operation_scoped=True,
        )
        if not isinstance(result, Mapping) or result.get("decision") not in {
            "allow",
            "ask",
            "deny",
        }:
            raise RpcError(-32603, "invalid policy evaluation response")
        response = dict(result)
        if (
            "approval_token" in response
            and response["approval_token"] is not None
            and (
                not isinstance(response["approval_token"], str)
                or not _valid_approval_token(response["approval_token"])
            )
        ):
            raise RpcError(-32603, "invalid policy approval token")
        if response.get("approval_token") is not None:
            self._require_feature("approvals")
            if response["decision"] != "ask":
                raise RpcError(-32603, "policy approval token requires an ask decision")
        return response

    def get_secret(
        self,
        name: str,
        *,
        parent_request_id: Any = _MISSING,
    ) -> str:
        """Resolve one manifest-allowlisted, owner-scoped host secret."""

        if not _valid_extension_identifier(name):
            raise ValueError("secret name must be a valid extension identifier")
        self._require_feature("secrets")
        result = self.request(
            "secret/get",
            {"name": name},
            parent_request_id=parent_request_id,
            operation_scoped=True,
        )
        if (
            not isinstance(result, Mapping)
            or set(result) != {"value"}
            or not isinstance(result.get("value"), str)
            or len(result["value"].encode("utf-8")) > MAX_SECRET_VALUE_BYTES
        ):
            raise RpcError(-32603, "invalid secret lookup response")
        return result["value"]

    def spawn_agent(
        self,
        *,
        task_name: str,
        message: str,
        idempotency_key: str,
        tools: Sequence[str],
        max_depth: int,
        max_concurrent_children: int,
        max_turns: Optional[int] = None,
        max_tokens: Optional[int] = None,
        max_cost_microdollars: Optional[int] = None,
        max_output_bytes: int,
        timeout_ms: Optional[int] = None,
        profile: Optional[str] = None,
        fingerprint: Optional[str] = None,
        parent_request_id: Any = _MISSING,
    ) -> dict[str, Any]:
        """Create a host-bounded child owned by the active request; omitted ceilings inherit the parent session's limits."""

        if not isinstance(task_name, str) or not task_name.strip():
            raise ValueError("agent task_name must be non-empty")
        if profile is not None and (
            not isinstance(profile, str)
            or not profile
            or len(profile) > 48
            or any(
                not (character.isascii() and (character.islower() or character.isdigit()))
                and character not in {"_", "-"}
                for character in profile
            )
        ):
            raise ValueError("agent profile must be a bounded lowercase stable identifier")
        if fingerprint is not None and (
            not isinstance(fingerprint, str)
            or len(fingerprint) != 64
            or any(character not in "0123456789abcdef" for character in fingerprint)
        ):
            raise ValueError("agent fingerprint must be a lowercase SHA-256 digest")
        if not isinstance(message, str) or not message:
            raise ValueError("agent message must be non-empty")
        if not isinstance(idempotency_key, str) or not idempotency_key.strip():
            raise ValueError("agent idempotency_key must be non-empty")
        if (
            isinstance(tools, (str, bytes, bytearray))
            or not isinstance(tools, Sequence)
            or not tools
            or len(tools) > 5
            or any(not isinstance(tool, str) for tool in tools)
            or len(set(tools)) != len(tools)
            or any(
                tool not in {"read", "search", "edit", "write", "bash"} for tool in tools
            )
        ):
            raise ValueError(
                "agent tools must be a duplicate-free subset of read, search, edit, write, and bash"
            )
        integer_limits = {
            "max_depth": (max_depth, 1, 1),
            "max_concurrent_children": (max_concurrent_children, 1, 8),
            "max_output_bytes": (max_output_bytes, 512, 16 * 1024),
        }
        for name, (value, minimum, maximum) in integer_limits.items():
            if (
                not isinstance(value, int)
                or isinstance(value, bool)
                or not minimum <= value <= maximum
            ):
                raise ValueError(
                    f"agent {name} must be an integer between {minimum} and {maximum}"
                )
        optional_limits = {
            "max_turns": (max_turns, 1, 256),
            "max_cost_microdollars": (max_cost_microdollars, 1, 50_000_000),
            "timeout_ms": (timeout_ms, 5_000, 24 * 60 * 60 * 1_000),
        }
        for name, (value, minimum, maximum) in optional_limits.items():
            if value is None:
                continue
            if (
                not isinstance(value, int)
                or isinstance(value, bool)
                or not minimum <= value <= maximum
            ):
                raise ValueError(
                    f"agent {name} must be null or an integer between {minimum} and {maximum}"
                )
        if max_tokens is not None and (
            not isinstance(max_tokens, int)
            or isinstance(max_tokens, bool)
            or not 1_000 <= max_tokens <= 64_000
        ):
            raise ValueError(
                "agent max_tokens must be null or an integer between 1000 and 64000"
            )
        self._require_feature("agent_sessions")
        params: dict[str, Any] = {
            "task_name": task_name,
            "message": message,
            "idempotency_key": idempotency_key,
            "policy": {
                "tools": list(tools),
                "max_depth": max_depth,
                "max_concurrent_children": max_concurrent_children,
                "max_turns": max_turns,
                "max_tokens": max_tokens,
                "max_cost_microdollars": max_cost_microdollars,
                "max_output_bytes": max_output_bytes,
                "timeout_ms": timeout_ms,
            },
        }
        if profile is not None:
            params["profile"] = profile
        if fingerprint is not None:
            params["fingerprint"] = fingerprint
        result = self.request(
            "agent/spawn",
            params,
            parent_request_id=parent_request_id,
            operation_scoped=True,
        )
        if not isinstance(result, Mapping) or not isinstance(result.get("agent_id"), str):
            raise RpcError(-32603, "invalid agent spawn response")
        return dict(result)

    def send_agent_message(
        self,
        target: str,
        message: str,
        *,
        parent_request_id: Any = _MISSING,
    ) -> dict[str, Any]:
        """Steer one child session owned by this extension resource owner."""

        return self._agent_message_request(
            "agent/message",
            target,
            message,
            parent_request_id=parent_request_id,
        )

    def follow_up_agent(
        self,
        target: str,
        message: str,
        *,
        parent_request_id: Any = _MISSING,
    ) -> dict[str, Any]:
        """Queue a follow-up task on one owned child session."""

        return self._agent_message_request(
            "agent/follow_up",
            target,
            message,
            parent_request_id=parent_request_id,
        )

    def _agent_message_request(
        self,
        method: str,
        target: str,
        message: str,
        *,
        parent_request_id: Any,
    ) -> dict[str, Any]:
        if not isinstance(target, str) or not target.strip():
            raise ValueError("agent target must be non-empty")
        if not isinstance(message, str) or not message:
            raise ValueError("agent message must be non-empty")
        self._require_feature("agent_sessions")
        result = self.request(
            method,
            {"target": target, "message": message},
            parent_request_id=parent_request_id,
            operation_scoped=True,
        )
        if not isinstance(result, Mapping):
            raise RpcError(-32603, f"invalid {method} response")
        return dict(result)

    def list_agents(
        self, *, parent_request_id: Any = _MISSING
    ) -> dict[str, Any]:
        """List only child-session trees owned by the active resource owner."""

        self._require_feature("agent_sessions")
        result = self.request(
            "agent/list",
            {},
            parent_request_id=parent_request_id,
            operation_scoped=True,
        )
        if not isinstance(result, Mapping) or not isinstance(result.get("agents"), list):
            raise RpcError(-32603, "invalid agent list response")
        return dict(result)

    def wait_agents(
        self,
        *,
        timeout_ms: int = 30_000,
        parent_request_id: Any = _MISSING,
    ) -> dict[str, Any]:
        """Wait up to 60 seconds for owned child-session state to settle."""

        if (
            not isinstance(timeout_ms, int)
            or isinstance(timeout_ms, bool)
            or not 1 <= timeout_ms <= 60_000
        ):
            raise ValueError("agent timeout_ms must be between 1 and 60000")
        self._require_feature("agent_sessions")
        result = self.request(
            "agent/wait",
            {"timeout_ms": timeout_ms},
            parent_request_id=parent_request_id,
            operation_scoped=True,
        )
        if not isinstance(result, Mapping) or not isinstance(result.get("timed_out"), bool):
            raise RpcError(-32603, "invalid agent wait response")
        return dict(result)

    def interrupt_agent(
        self,
        target: str,
        *,
        parent_request_id: Any = _MISSING,
    ) -> dict[str, Any]:
        """Interrupt an owned child-session tree."""

        if not isinstance(target, str) or not target.strip():
            raise ValueError("agent target must be non-empty")
        self._require_feature("agent_sessions")
        result = self.request(
            "agent/interrupt",
            {"target": target},
            parent_request_id=parent_request_id,
            operation_scoped=True,
        )
        if not isinstance(result, Mapping):
            raise RpcError(-32603, "invalid agent interrupt response")
        return dict(result)

    def run(self, *, stdin: Any = None, stdout: Any = None) -> None:
        """Run until graceful shutdown or stdin EOF."""

        if self._running:
            raise RuntimeError("extension is already running")
        reader = stdin if stdin is not None else self.stdin
        writer = stdout if stdout is not None else self.stdout
        if reader is None:
            import sys

            reader = sys.stdin
        if writer is None:
            import sys

            writer = sys.stdout
        self._reset_runtime_state()
        self._transport = JsonRpcTransport(
            reader,
            writer,
            max_message_bytes=self.max_message_bytes,
        )
        self._writer = _SerializedWriter(self._transport, self.writer_queue_size)
        self._writer.start()
        inbound: queue.Queue[Any] = queue.Queue()
        reader_thread = threading.Thread(
            target=self._read_loop,
            args=(inbound,),
            name="ygg-extension-reader",
            daemon=True,
        )
        self._running = True
        reader_thread.start()
        eof_seen = False
        try:
            while True:
                if self._shutdown_done.is_set() or self._eof_done.is_set():
                    break
                try:
                    item = inbound.get(timeout=0.05)
                except queue.Empty:
                    continue
                if item is _MISSING:
                    if not eof_seen:
                        eof_seen = True
                        self.logger.info("extension stdin closed")
                        self._start_eof_drain()
                    continue
                if isinstance(item, _InboundError):
                    self.logger.error(
                        "invalid protocol input",
                        code=item.error.code,
                        error=item.error.message,
                    )
                    self._send_error(None, item.error)
                    continue
                self._route_message(item)
        except (BrokenPipeError, EOFError, OSError):
            self.logger.info("extension protocol stream closed")
            self._cancel_all("transport_lost")
            self._fail_pending(RpcError(-32000, "protocol stream is closed"))
        finally:
            self._draining = True
            self._cancel_all("shutdown")
            self._fail_pending(RpcError(-32000, "extension stopped"))
            if self._executor is not None:
                self._executor.shutdown(wait=False, cancel_futures=True)
            if self._writer is not None:
                self._writer.close()
            self._running = False
            self._closed = True
            self._transport = None
            self._writer = None

    def handle_message(self, message: Mapping[str, Any]) -> bool:
        """Queueing is required for 0.2; use :meth:`run` for embedded tests."""

        if self._transport is None:
            raise RuntimeError("run must establish a transport before handling messages")
        self._route_message(dict(message))
        return not self._draining

    def _read_loop(self, inbound: queue.Queue[Any]) -> None:
        while True:
            transport = self._transport
            if transport is None:
                return
            try:
                message = transport.read()
            except RpcError as error:
                inbound.put(_InboundError(error))
                continue
            except (BrokenPipeError, EOFError, OSError):
                inbound.put(_MISSING)
                return
            if message is None:
                inbound.put(_MISSING)
                return
            inbound.put(message)

    def _route_message(self, message: dict[str, Any]) -> None:
        if "method" not in message:
            if "id" in message and ("result" in message or "error" in message):
                self._resolve_response(message)
            else:
                self._send_error(message.get("id"), ProtocolError(-32600, "invalid JSON-RPC request"))
            return
        method = message.get("method")
        if not isinstance(method, str) or not method:
            self._send_error(message.get("id"), ProtocolError(-32600, "method must be a string"))
            return
        request_id = message.get("id", _MISSING)
        if method == "initialize":
            if request_id is _MISSING:
                self.logger.warning("ignored initialize notification")
                return
            try:
                result = self._initialize(message.get("params", {}))
            except RpcError as error:
                self._send_error(request_id, error)
                return
            self._send_result(request_id, result)
            return
        if method == "$/cancelRequest":
            self._handle_cancellation(message.get("params", {}))
            if request_id is not _MISSING:
                self._send_result(request_id, {})
            return
        if method == "shutdown":
            if request_id is _MISSING:
                self.logger.warning("ignored shutdown notification")
                return
            if not self._initialized:
                self._send_error(request_id, RpcError(-32600, "initialize must be the first request"))
                return
            if self._draining:
                self._send_error(request_id, RpcError(-32000, "extension is already draining"))
                return
            self._draining = True
            self._start_shutdown(request_id, message.get("params", {}))
            return
        if not self._initialized:
            if request_id is not _MISSING:
                self._send_error(request_id, RpcError(-32600, "initialize must be the first request"))
            return
        if self._draining:
            if request_id is not _MISSING:
                self._send_error(request_id, RpcError(-32000, "extension is draining"))
            return
        if request_id is _MISSING:
            self._submit_notification(method, message.get("params", {}))
        else:
            try:
                self._id_key(request_id)
            except RpcError as error:
                self._send_error(request_id, error)
                return
            self._submit_request(request_id, method, message.get("params", {}))

    def _submit_request(self, request_id: Any, method: str, params: Any) -> None:
        executor = self._executor
        if executor is None:
            self._send_error(request_id, RpcError(-32600, "initialize must be the first request"))
            return
        if not self._admission.acquire(blocking=False):
            self._send_error(request_id, RpcError(-32000, "extension request queue is full"))
            return
        token = CancellationToken(request_id)
        key = self._id_key(request_id)
        with self._lock:
            if key in self._active:
                self._admission.release()
                self._send_error(request_id, RpcError(-32600, "duplicate active request id"))
                return
            self._active[key] = token
        try:
            future = executor.submit(self._handle_request, request_id, method, params, token)
        except Exception:
            with self._lock:
                self._active.pop(key, None)
            self._admission.release()
            raise
        self._track_future(future, admitted=True)

    def _submit_notification(self, method: str, params: Any) -> None:
        executor = self._executor
        if executor is None:
            return
        if method not in self._lifecycle_handlers:
            self.logger.warning("ignored unknown notification", method=method)
            return
        if "lifecycle_events" not in self._features:
            self.logger.warning("ignored unnegotiated lifecycle event", method=method)
            return
        if not self._admission.acquire(blocking=False):
            self.logger.warning("dropped lifecycle event because request queue is full", method=method)
            return
        try:
            future = executor.submit(self._handle_lifecycle, method, params)
        except Exception:
            self._admission.release()
            raise
        self._track_future(future, admitted=True)

    def _handle_request(
        self,
        request_id: Any,
        method: str,
        params: Any,
        cancellation: CancellationToken,
    ) -> None:
        scope_token = _CURRENT_REQUEST.set(_RequestScope(self, request_id, cancellation))
        try:
            cancellation.raise_if_cancelled()
            result = self._dispatch(method, params)
            if cancellation._complete():
                self._send_result(request_id, result)
            else:
                cancellation._finish_cancelled()
                self._send_error(request_id, CancelledError(cancellation.reason or "cancelled"))
        except CancelledError as error:
            cancellation._cancel(error.data.get("reason") if isinstance(error.data, Mapping) else "cancelled")
            cancellation._finish_cancelled()
            self._send_error(request_id, error)
        except RpcError as error:
            if cancellation._complete():
                self._send_error(request_id, error)
            elif cancellation.cancelled:
                cancellation._finish_cancelled()
                self._send_error(request_id, CancelledError(cancellation.reason or "cancelled"))
            else:
                # A result frame can fail local serialization or the configured
                # size bound after the terminal race was already claimed. The
                # request still failed; it was not cancelled.
                self._send_error(request_id, error)
            self.logger.error("request failed", method=method, code=error.code, error=error.message)
        except Exception as error:  # Extension code must never corrupt stdout.
            self.logger.error("request handler failed", method=method, error=str(error))
            if cancellation._complete():
                self._send_error(request_id, RpcError(-32603, "internal error"))
            elif cancellation.cancelled:
                cancellation._finish_cancelled()
                self._send_error(request_id, CancelledError(cancellation.reason or "cancelled"))
            else:
                self._send_error(request_id, RpcError(-32603, "internal error"))
        finally:
            _CURRENT_REQUEST.reset(scope_token)
            with self._lock:
                self._active.pop(self._id_key(request_id), None)

    def _handle_lifecycle(self, method: str, params: Any) -> None:
        handler = self._lifecycle_handlers[method]
        try:
            self._invoke(handler, self._object_params(params, method))
        except Exception as error:
            self.logger.error("lifecycle handler failed", event=method, error=str(error))

    def _dispatch(self, method: str, params: Any) -> Any:
        if method == "tool/call":
            return self._call_tool(params)
        if method == "command/execute":
            return self._execute_command(params)
        if method == "hook/run":
            return self._run_hook(params)
        if method == "context/collect":
            return self._collect_context(params)
        if method == "status/collect":
            return self._collect_status(params)
        if method == "tool/render":
            return self._render_tool(params)
        raise RpcError(-32601, f"unknown method: {method}")

    def _initialize(self, params: Any) -> dict[str, Any]:
        if self._initialized:
            raise RpcError(-32600, "initialize must be the first request")
        if not isinstance(params, Mapping):
            raise RpcError(-32602, "initialize params must be an object")
        if self.api_version == "0.3":
            raise RpcError(
                -32000,
                "extension API 0.3 is defined but not runtime-ready in this SDK build",
            )
        if self.api_version not in {"0.1", "0.2"}:
            raise RpcError(-32000, f"unsupported extension API version: {self.api_version!r}")
        host_version = params.get("api_version")
        if host_version != self.api_version:
            raise RpcError(
                -32000,
                f"unsupported API version: host requested {host_version!r}, SDK implements {self.api_version!r}",
            )
        contributes = params.get("contributes", {})
        if not isinstance(contributes, Mapping):
            raise RpcError(-32602, "initialize contributes must be an object")
        self._declared = dict(contributes)
        self._validate_declarations()

        protocol_response: Optional[dict[str, Any]] = None
        if self.api_version == "0.2":
            protocol = params.get("protocol")
            if not isinstance(protocol, Mapping):
                raise RpcError(-32602, "API 0.2 initialize requires a protocol object")
            if protocol.get("version") != "0.2":
                raise RpcError(-32000, "unsupported executable-extension protocol version")
            required = self._feature_list(protocol.get("required_features", []), "required_features")
            optional = self._feature_list(protocol.get("optional_features", []), "optional_features")
            unsupported = [feature for feature in required if feature not in self._supported_features]
            if unsupported:
                raise RpcError(
                    -32000,
                    "required API 0.2 features are unsupported",
                    {"unsupported_features": unsupported},
                )
            features = list(dict.fromkeys(required + optional))
            features = [feature for feature in features if feature in self._supported_features]
            if not self._lifecycle_handlers:
                features = [feature for feature in features if feature != "lifecycle_events"]
            limits = protocol.get("limits", {})
            if not isinstance(limits, Mapping):
                raise RpcError(-32602, "protocol.limits must be an object")
            requested = limits.get("max_concurrent_requests", 1)
            if not isinstance(requested, int) or isinstance(requested, bool) or requested <= 0:
                raise RpcError(
                    -32602,
                    "protocol.limits.max_concurrent_requests must be a positive integer",
                )
            self._features = frozenset(features)
            self._negotiated_concurrency = min(requested, self.max_concurrent_requests)
            if "dynamic_tools" in self._features:
                with self._tool_catalog_lock:
                    self._tool_catalogs[0] = dict(self._tools)
            protocol_response = {
                "version": "0.2",
                "features": features,
                "limits": {"max_concurrent_requests": self._negotiated_concurrency},
            }
            if "lifecycle_events" in self._features:
                protocol_response["lifecycle_events"] = sorted(self._lifecycle_handlers)
        else:
            self._features = frozenset()
            self._negotiated_concurrency = 1

        self._initialization = dict(params)
        self._initialized = True
        self._executor = ThreadPoolExecutor(
            max_workers=self._negotiated_concurrency,
            thread_name_prefix="ygg-extension-handler",
        )
        result: dict[str, Any] = {
            "api_version": self.api_version,
            "tools": [self._tool_definition(tool) for tool in self._tools.values()],
            "commands": [
                {
                    "name": command.name,
                    "description": command.description,
                    **({"usage": command.usage} if command.usage is not None else {}),
                }
                for command in self._commands.values()
            ],
        }
        if protocol_response is not None:
            result["protocol"] = protocol_response
        return result

    def _validate_declarations(self) -> None:
        self._require_exact_names("tools", self._declared_names("tools"), self._tools)
        self._require_exact_names("commands", self._declared_names("commands"), self._commands)

    def _declared_names(self, key: str) -> list[str]:
        value = self._declared.get(key, _MISSING)
        if value is _MISSING:
            return list(self._tools if key == "tools" else self._commands)
        if not isinstance(value, list) or not all(isinstance(name, str) for name in value):
            raise RpcError(-32602, f"initialize contributes.{key} must be an array of strings")
        if len(set(value)) != len(value):
            raise RpcError(-32602, f"initialize contributes.{key} contains duplicate names")
        return list(value)

    @staticmethod
    def _require_exact_names(kind: str, declared: list[str], registered: Mapping[str, Any]) -> None:
        if set(declared) != set(registered) or len(declared) != len(registered):
            raise RpcError(
                -32602,
                f"registered {kind} do not match manifest declarations",
                {"declared": declared, "registered": list(registered)},
            )

    def _call_tool(self, params: Any) -> dict[str, Any]:
        request = self._object_params(params, "tool/call")
        name = request.get("name")
        with self._tool_catalog_lock:
            catalog_revision = request.get("catalog_revision", _MISSING)
            if catalog_revision is _MISSING:
                catalog = self._tools
            else:
                if self.api_version != "0.2" or "dynamic_tools" not in self._features:
                    raise RpcError(
                        -32602,
                        "tool/call catalog_revision requires negotiated dynamic_tools",
                    )
                if (
                    not isinstance(catalog_revision, int)
                    or isinstance(catalog_revision, bool)
                    or catalog_revision < 0
                    or catalog_revision > (2**64 - 1)
                ):
                    raise RpcError(
                        -32602,
                        "tool/call catalog_revision must be an unsigned 64-bit integer",
                    )
                catalog = self._tool_catalogs.get(catalog_revision)
                if (
                    catalog is None
                    and catalog_revision == self._staged_tool_catalog_revision
                ):
                    catalog = self._staged_tool_catalog
                if catalog is None:
                    raise RpcError(
                        -32602,
                        f"unknown or retired tool catalog revision: {catalog_revision}",
                    )
            tool = catalog.get(name) if isinstance(name, str) else None
        if tool is None:
            raise RpcError(-32601, f"unknown tool: {name}")
        try:
            value = self._invoke(
                tool.handler,
                request.get("arguments", {}),
                self._context_from(request),
            )
        except (CancelledError, RpcError):
            raise
        except Exception as error:
            self.logger.error("tool handler failed", tool=name, error=str(error))
            if self.api_version == "0.2":
                return self._tool_result(
                    tool_result(text_content(str(error)), is_error=True),
                    tool,
                )
            return {"content": str(error), "is_error": True, "metadata": {}}
        return self._tool_result(value, tool)

    def _execute_command(self, params: Any) -> dict[str, Any]:
        request = self._object_params(params, "command/execute")
        name = request.get("name")
        command = self._commands.get(name) if isinstance(name, str) else None
        if command is None:
            raise RpcError(-32601, f"unknown command: {name}")
        arguments = request.get("arguments", [])
        if not isinstance(arguments, list):
            raise RpcError(-32602, "command arguments must be an array")
        try:
            value = self._invoke(command.handler, arguments, self._context_from(request))
        except (CancelledError, RpcError):
            raise
        except Exception as error:
            self.logger.error("command handler failed", command=name, error=str(error))
            raise RpcError(-32603, "internal error") from error
        return self._command_result(value)

    def _run_hook(self, params: Any) -> dict[str, Any]:
        request = self._object_params(params, "hook/run")
        name = request.get("hook")
        if not isinstance(name, str) or not name:
            raise RpcError(-32602, "hook must be a string")
        self._require_declared_name("hooks", name)
        handler = self._hooks.get(name)
        if handler is None:
            return {"disposition": {"action": "continue"}, "context": [], "notifications": []}
        try:
            value = self._invoke(handler, request.get("payload", {}), self._context_from(request))
        except (CancelledError, RpcError):
            raise
        except Exception as error:
            self.logger.error("hook handler failed", hook=name, error=str(error))
            raise RpcError(-32603, "internal error") from error
        return self._hook_result(value)

    def _collect_context(self, params: Any) -> list[Any]:
        request = self._object_params(params, "context/collect")
        if self._declared.get("context", False) is not True:
            raise RpcError(-32601, "context contributions are not declared")
        if self._context_handler is None:
            return []
        value = self._invoke(self._context_handler, request, self._context_from(request))
        if value is None:
            return []
        if not isinstance(value, list):
            raise RpcError(-32603, "context handler must return an array")
        return value

    def _collect_status(self, params: Any) -> Optional[dict[str, Any]]:
        request = self._object_params(params, "status/collect")
        surface = request.get("surface")
        if not isinstance(surface, str) or not surface:
            raise RpcError(-32602, "status surface must be a string")
        self._require_declared_name("ui", surface)
        handler = self._status_handlers.get(surface)
        if handler is None:
            return None
        return self._status_result(self._invoke(handler, request, self._context_from(request)))

    def _render_tool(self, params: Any) -> dict[str, Any]:
        request = self._object_params(params, "tool/render")
        name = request.get("name")
        if not isinstance(name, str) or not name:
            raise RpcError(-32602, "renderer name must be a string")
        self._require_declared_name("tool_renderers", name)
        handler = self._renderer_handlers.get(name)
        if handler is None:
            return {"segments": []}
        return self._render_result(self._invoke(handler, request, self._context_from(request)))

    def _tool_result(self, value: Any, tool: _Tool) -> dict[str, Any]:
        if self.api_version == "0.1":
            if isinstance(value, Mapping):
                content = value.get("content", "")
                return {
                    "content": content if isinstance(content, str) else str(content),
                    "is_error": bool(value.get("is_error", False)),
                    "metadata": value.get("metadata", {}),
                }
            return {"content": "" if value is None else str(value), "is_error": False, "metadata": {}}

        if not isinstance(value, Mapping):
            value = {"content": [] if value is None else [text_content(value)]}
        unknown = set(value) - {"content", "is_error", "metadata", "structured_content"}
        if unknown:
            raise RpcError(
                -32603,
                f"unknown API 0.2 tool result fields: {sorted(map(str, unknown))}",
            )
        content = value.get("content", [])
        if isinstance(content, str):
            content = [text_content(content)]
        if not isinstance(content, list):
            raise RpcError(-32603, "API 0.2 tool content must be an array")
        if not content:
            raise RpcError(-32603, "API 0.2 tool content must not be empty")
        if len(content) > MAX_TOOL_CONTENT_PARTS:
            raise RpcError(
                -32603,
                f"API 0.2 tool content exceeds {MAX_TOOL_CONTENT_PARTS} parts",
            )
        parts = [self._validate_content_part(part) for part in content]
        if not any(part["type"] == "text" for part in parts):
            raise RpcError(-32603, "API 0.2 tool content requires an explicit text part")
        if any(part["type"] in {"image", "audio"} for part in parts):
            if "artifacts" not in self._features:
                raise RpcError(-32603, "media tool content requires artifacts negotiation")
        is_error = value.get("is_error", False)
        if not isinstance(is_error, bool):
            raise RpcError(-32603, "tool is_error must be a boolean")
        metadata = value.get("metadata", {})
        if metadata is not None and not isinstance(metadata, Mapping):
            raise RpcError(-32603, "tool metadata must be an object or null")
        has_structured_content = "structured_content" in value
        if tool.output_schema is None and has_structured_content:
            raise RpcError(-32603, "structured_content requires a declared output_schema")
        if tool.output_schema is not None and not has_structured_content and not is_error:
            raise RpcError(-32603, "tool declared output_schema but omitted structured_content")
        result: dict[str, Any] = {
            "content": parts,
            "is_error": is_error,
            "metadata": None if metadata is None else dict(metadata),
        }
        if has_structured_content:
            result["structured_content"] = value["structured_content"]
        return result

    @staticmethod
    def _validate_content_part(part: Any) -> dict[str, Any]:
        if not isinstance(part, Mapping):
            raise RpcError(-32603, "tool content parts must be objects")
        result = dict(part)
        kind = result.get("type")
        if kind == "text":
            allowed = {"type", "text"}
            if not isinstance(result.get("text"), str):
                raise RpcError(-32603, "text content requires a string text field")
        elif kind in {"image", "audio"}:
            allowed = (
                {"type", "artifact_id", "mime_type", "alt"}
                if kind == "image"
                else {"type", "artifact_id", "mime_type", "transcript"}
            )
            if not isinstance(result.get("artifact_id"), str) or not result["artifact_id"]:
                raise RpcError(-32603, f"{kind} content requires artifact_id")
            if not isinstance(result.get("mime_type"), str) or "/" not in result["mime_type"]:
                raise RpcError(-32603, f"{kind} content requires mime_type")
            annotation = "alt" if kind == "image" else "transcript"
            if annotation in result and not isinstance(result[annotation], str):
                raise RpcError(-32603, f"{kind} content {annotation} must be a string")
        else:
            raise RpcError(-32603, f"unknown tool content type: {kind}")
        unknown = set(result) - allowed
        if unknown:
            raise RpcError(
                -32603,
                f"unknown {kind} content fields: {sorted(map(str, unknown))}",
            )
        return result

    @staticmethod
    def _tool_definition(tool: _Tool) -> dict[str, Any]:
        definition: dict[str, Any] = {
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.parameters,
        }
        if tool.output_schema is not None:
            definition["output_schema"] = tool.output_schema
        return definition

    def _validate_catalog_update(
        self,
        value: Any,
        local_tools: Mapping[str, _Tool],
    ) -> dict[str, Any]:
        if not isinstance(value, Mapping) or set(value) != {"revision", "tools"}:
            raise RpcError(-32603, "invalid dynamic tool catalog response")
        revision = value["revision"]
        if (
            not isinstance(revision, int)
            or isinstance(revision, bool)
            or revision < 0
            or revision > (2**64 - 1)
        ):
            raise RpcError(-32603, "invalid dynamic tool catalog revision")
        names = value["tools"]
        if (
            not isinstance(names, list)
            or len(names) > MAX_DYNAMIC_TOOLS
            or not all(isinstance(name, str) and name for name in names)
            or len(set(names)) != len(names)
            or any(name not in local_tools for name in names)
        ):
            raise RpcError(-32603, "invalid dynamic tool catalog names")
        return {"revision": revision, "tools": list(names)}

    def _commit_staged_tool_catalog(self, response: Mapping[str, Any]) -> None:
        staged = self._staged_tool_catalog
        staged_revision = self._staged_tool_catalog_revision
        if staged is None or staged_revision is None:
            raise RpcError(-32603, "dynamic tool catalog mutation was not staged")
        if response["revision"] != staged_revision:
            self._discard_staged_tool_catalog()
            raise RpcError(
                -32603,
                "invalid dynamic tool catalog revision: "
                f"expected {staged_revision}, got {response['revision']}",
            )
        active = {name: staged[name] for name in response["tools"]}
        self._tools = active
        self._tool_catalog_revision = staged_revision
        self._tool_catalogs[staged_revision] = dict(active)
        while len(self._tool_catalogs) > MAX_TOOL_CATALOG_REVISIONS:
            self._tool_catalogs.popitem(last=False)
        self._discard_staged_tool_catalog()

    def _discard_staged_tool_catalog(self) -> None:
        self._staged_tool_catalog = None
        self._staged_tool_catalog_revision = None

    @staticmethod
    def _command_result(value: Any) -> dict[str, Any]:
        if value is None:
            return {"text": "", "notifications": [], "context": []}
        if isinstance(value, Mapping):
            text = value.get("text", "")
            return {
                "text": text if isinstance(text, str) else str(text),
                "notifications": value.get("notifications", []),
                "context": value.get("context", []),
            }
        return {"text": str(value), "notifications": [], "context": []}

    @staticmethod
    def _hook_result(value: Any) -> dict[str, Any]:
        if value is None:
            return {"disposition": {"action": "continue"}, "context": [], "notifications": []}
        if isinstance(value, Mapping):
            return {
                "disposition": value.get("disposition", {"action": "continue"}),
                "context": value.get("context", []),
                "notifications": value.get("notifications", []),
            }
        raise RpcError(-32603, "hook handler must return an object")

    @staticmethod
    def _status_result(value: Any) -> Optional[dict[str, Any]]:
        if value is None:
            return None
        if not isinstance(value, Mapping):
            raise RpcError(-32603, "status handler must return an object or null")
        result: dict[str, Any] = {
            "surface": value.get("surface", "status"),
            "text": value.get("text", ""),
            "priority": value.get("priority", 0),
        }
        if "style_role" in value:
            result["style_role"] = value["style_role"]
        return result

    @staticmethod
    def _render_result(value: Any) -> dict[str, Any]:
        if value is None:
            return {"segments": []}
        segments = value.get("segments", []) if isinstance(value, Mapping) else [
            {"text": str(value), "style_role": None}
        ]
        if not isinstance(segments, list):
            raise RpcError(-32603, "renderer must return a segments array")
        return {"segments": segments}

    def _handle_cancellation(self, params: Any) -> None:
        if not isinstance(params, Mapping) or "id" not in params:
            self.logger.warning("ignored malformed cancellation")
            return
        try:
            key = self._id_key(params["id"])
        except RpcError:
            self.logger.warning("ignored cancellation with invalid id")
            return
        reason = params.get("reason", "cancelled")
        with self._lock:
            token = self._active.get(key)
            pending = self._pending.get(key)
        if token is not None:
            token._cancel(reason)
        elif pending is not None:
            pending.fail(CancelledError(str(reason)))

    def _resolve_response(self, message: dict[str, Any]) -> None:
        try:
            key = self._id_key(message.get("id"))
        except RpcError:
            self.logger.warning("ignored response with invalid id")
            return
        with self._lock:
            pending = self._pending.get(key)
        if pending is None:
            self.logger.warning("ignored late or unknown response", response_id=message.get("id"))
            return
        pending.resolve(message)

    def _start_shutdown(self, request_id: Any, params: Any) -> None:
        values = dict(params) if isinstance(params, Mapping) else {}
        requested_ms = values.get("drain_timeout_ms")
        timeout = self.shutdown_timeout
        if isinstance(requested_ms, int) and not isinstance(requested_ms, bool) and requested_ms >= 0:
            timeout = min(timeout, requested_ms / 1000.0)

        def shutdown_flow() -> None:
            try:
                if not self._wait_for_futures(timeout):
                    self._cancel_all("shutdown")
                    self._wait_for_futures(self.cancellation_grace)
                if self._shutdown_handler is not None:
                    self._invoke(self._shutdown_handler, values, self._context_from(values))
                self._send_result(request_id, {})
            except Exception as error:
                self.logger.error("shutdown handler failed", error=str(error))
                self._send_error(request_id, RpcError(-32603, "shutdown failed"))
            finally:
                self._shutdown_done.set()

        threading.Thread(
            target=shutdown_flow,
            name="ygg-extension-shutdown",
            daemon=True,
        ).start()

    def _start_eof_drain(self) -> None:
        self._draining = True

        def eof_flow() -> None:
            if not self._wait_for_futures(self.shutdown_timeout):
                self._cancel_all("transport_lost")
                self._wait_for_futures(self.cancellation_grace)
            self._fail_pending(RpcError(-32000, "stdin closed while waiting for host response"))
            self._eof_done.set()

        threading.Thread(target=eof_flow, name="ygg-extension-eof", daemon=True).start()

    def _track_future(self, future: Future[Any], *, admitted: bool = False) -> None:
        admission = self._admission
        with self._future_condition:
            self._futures.add(future)

        def completed(done: Future[Any]) -> None:
            with self._future_condition:
                self._futures.discard(done)
                self._future_condition.notify_all()
            if admitted:
                admission.release()

        future.add_done_callback(completed)

    def _wait_for_futures(self, timeout: float) -> bool:
        deadline = time.monotonic() + timeout
        with self._future_condition:
            while self._futures:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    return False
                self._future_condition.wait(remaining)
            return True

    def _cancel_all(self, reason: str) -> None:
        with self._lock:
            tokens = list(self._active.values())
        for token in tokens:
            token._cancel(reason)

    def _fail_pending(self, error: RpcError) -> None:
        with self._lock:
            pending = list(self._pending.values())
        for waiter in pending:
            waiter.fail(error)

    def _send(self, message: Mapping[str, Any]) -> None:
        writer = self._writer
        if writer is None or self._closed:
            raise RpcError(-32000, "protocol transport is closed")
        try:
            writer.send(message)
        except (BrokenPipeError, OSError) as error:
            self._closed = True
            raise RpcError(-32000, "protocol stream is closed") from error

    def _send_result(self, request_id: Any, result: Any) -> None:
        self._send({"jsonrpc": "2.0", "id": request_id, "result": result})

    def _send_error(self, request_id: Any, error: RpcError) -> None:
        try:
            self._send({"jsonrpc": "2.0", "id": request_id, "error": error.error_object()})
        except RpcError:
            self._closed = True

    def _require_initialized(self) -> None:
        if not self._initialized:
            raise RpcError(-32600, "initialize must be the first request")

    def _require_capability(self, capability: str) -> None:
        self._require_initialized()
        if self._declared.get(capability) is not True:
            raise RpcError(-32601, f"{capability} are not declared")

    def _require_feature(self, feature: str) -> None:
        self._require_initialized()
        if self.api_version != "0.2" or feature not in self._features:
            raise RpcError(-32601, f"API 0.2 feature is not negotiated: {feature}")

    def _require_declared_name(self, key: str, name: str) -> None:
        value = self._declared.get(key, _MISSING)
        if value is not _MISSING and (not isinstance(value, list) or name not in value):
            raise RpcError(-32601, f"{name} is not a declared {key.rstrip('s')}")

    def _resolve_parent(self, explicit: Any, *, required: bool) -> Any:
        if explicit is not _MISSING:
            if (
                not isinstance(explicit, int)
                or isinstance(explicit, bool)
                or explicit < 0
                or explicit > (2**64 - 1)
            ):
                raise RpcError(-32602, "parent_request_id must be an unsigned 64-bit integer")
            self._id_key(explicit)
            return explicit
        scope = _CURRENT_REQUEST.get()
        if scope is not None and scope.extension is self:
            parent = scope.request_id
            if (
                not isinstance(parent, int)
                or isinstance(parent, bool)
                or parent < 0
                or parent > (2**64 - 1)
            ):
                raise RpcError(-32602, "parent_request_id must be an unsigned 64-bit integer")
            return parent
        if required:
            raise RpcError(-32602, "operation-scoped host request requires parent_request_id")
        return _MISSING

    @staticmethod
    def _feature_list(value: Any, field_name: str) -> list[str]:
        if not isinstance(value, list) or not all(isinstance(item, str) and item for item in value):
            raise RpcError(-32602, f"protocol.{field_name} must be an array of strings")
        if len(set(value)) != len(value):
            raise RpcError(-32602, f"protocol.{field_name} contains duplicate features")
        return list(value)

    @staticmethod
    def _validate_progress_event(event: dict[str, Any]) -> dict[str, Any]:
        kind = event.get("type")
        if kind == "status":
            if not isinstance(event.get("message"), str):
                raise ValueError("status progress requires a string message")
            for field_name in ("current", "total"):
                value = event.get(field_name)
                if value is not None and (
                    not isinstance(value, int) or isinstance(value, bool) or value < 0
                ):
                    raise ValueError(f"status progress {field_name} must be a non-negative integer")
            if "unit" in event and event["unit"] is not None and not isinstance(event["unit"], str):
                raise ValueError("status progress unit must be a string")
            allowed = {"type", "message", "current", "total", "unit"}
        elif kind == "output":
            if event.get("stream") not in {"stdout", "stderr"}:
                raise ValueError("output progress stream must be stdout or stderr")
            if event.get("encoding") not in {"utf8", "base64"}:
                raise ValueError("output progress encoding must be utf8 or base64")
            if not isinstance(event.get("data"), str):
                raise ValueError("output progress data must be a string")
            if event["encoding"] == "base64":
                try:
                    base64.b64decode(event["data"], validate=True)
                except ValueError as error:
                    raise ValueError("output progress data is not valid base64") from error
            allowed = {"type", "stream", "encoding", "data"}
        else:
            raise ValueError("progress event type must be status or output")
        unknown = set(event) - allowed
        if unknown:
            raise ValueError(f"unknown progress event fields: {sorted(unknown)}")
        return event

    @staticmethod
    def _object_params(params: Any, method: str) -> dict[str, Any]:
        if not isinstance(params, Mapping):
            raise RpcError(-32602, f"{method} params must be an object")
        return dict(params)

    @staticmethod
    def _context_from(params: Any) -> dict[str, Any]:
        if not isinstance(params, Mapping):
            return {}
        context = params.get("context")
        return dict(context) if isinstance(context, Mapping) else {}

    @staticmethod
    def _id_key(request_id: Any) -> tuple[type, Any]:
        if isinstance(request_id, bool) or not isinstance(request_id, (int, str)):
            raise RpcError(-32600, "JSON-RPC id must be an integer or string")
        return (type(request_id), request_id)

    @staticmethod
    def _lifecycle_method(event: str) -> str:
        if not isinstance(event, str):
            raise TypeError("lifecycle event must be a string")
        method = event.replace("_", "/")
        if method not in LIFECYCLE_METHODS:
            raise ValueError(f"unknown lifecycle event: {event}")
        return method

    def _register_status(self, surface: str, handler: Handler) -> Handler:
        if surface in self._status_handlers:
            raise ValueError(f"duplicate status handler: {surface}")
        self._status_handlers[surface] = handler
        return handler

    def _reset_runtime_state(self) -> None:
        self._initialized = False
        self._running = False
        self._closed = False
        self._draining = False
        self._initialization = None
        self._declared = {}
        self._features = frozenset()
        self._negotiated_concurrency = 1
        with self._tool_catalog_lock:
            self._tool_catalog_revision = 0
            self._tool_catalogs.clear()
            self._discard_staged_tool_catalog()
        with self._presentation_lock:
            self._presentation_revision = None
        self._next_request_id = 1
        self._active.clear()
        self._pending.clear()
        self._futures.clear()
        self._admission = threading.BoundedSemaphore(self.max_pending_requests)
        self._shutdown_done.clear()
        self._eof_done.clear()

    @staticmethod
    def _validate_name(kind: str, name: Any) -> None:
        if not isinstance(name, str) or not name.strip():
            raise ValueError(f"{kind} name must be non-empty")

    @staticmethod
    def _invoke(handler: Handler, *args: Any) -> Any:
        try:
            signature = inspect.signature(handler)
        except (TypeError, ValueError):
            return handler(*args)
        positional = [
            parameter
            for parameter in signature.parameters.values()
            if parameter.kind
            in (inspect.Parameter.POSITIONAL_ONLY, inspect.Parameter.POSITIONAL_OR_KEYWORD)
        ]
        if any(
            parameter.kind == inspect.Parameter.VAR_POSITIONAL
            for parameter in signature.parameters.values()
        ):
            return handler(*args)
        if len(positional) >= len(args):
            return handler(*args)
        if len(positional) == 1:
            return handler(args[0])
        return handler()
