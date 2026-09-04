from __future__ import annotations

from dataclasses import dataclass, field
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import tempfile
import threading
from typing import Any, Callable, Optional
import unittest

from ygg_mcp.config import BridgeConfig, HttpAuthConfig, ServerConfig
from ygg_mcp.manager import BridgeManager
from ygg_mcp.protocol import McpCancelled, McpError, McpTransportError
from ygg_mcp.streamable_http import McpAuthenticationError, McpStreamableHttpClient

from .helpers import FakeCancellation, FakeExtension, ROOT, limits, wait_for


@dataclass(frozen=True)
class _HttpRequest:
    method: str
    target: str
    headers: dict[str, str]
    body: bytes

    def header(self, name: str) -> Optional[str]:
        return self.headers.get(name.lower())

    def message(self) -> dict[str, Any]:
        return json.loads(self.body.decode("utf-8"))


@dataclass(frozen=True)
class _HttpReply:
    status: int = 200
    headers: dict[str, str] = field(default_factory=dict)
    body: bytes = b""
    include_content_length: bool = True


class _LoopbackHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, format: str, *args: object) -> None:
        del format, args

    def do_DELETE(self) -> None:
        self._handle()

    def do_GET(self) -> None:
        self._handle()

    def do_POST(self) -> None:
        self._handle()

    def _handle(self) -> None:
        fixture = self.server.fixture  # type: ignore[attr-defined]
        length = self.headers.get("Content-Length", "0")
        try:
            body_length = int(length)
            if body_length < 0:
                raise ValueError
        except ValueError:
            self.send_response(400)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        request = _HttpRequest(
            method=self.command,
            target=self.path,
            headers={name.lower(): value for name, value in self.headers.items()},
            body=self.rfile.read(body_length),
        )
        fixture._record(request)
        try:
            reply = fixture.responder(request)
        except Exception as error:  # Fixture errors must not leave the client hanging.
            fixture._record_error(error)
            reply = _HttpReply(status=500, body=b"fixture failure")

        headers = dict(reply.headers)
        lower_headers = {name.lower() for name in headers}
        if reply.include_content_length and "content-length" not in lower_headers:
            headers["Content-Length"] = str(len(reply.body))
        if "connection" not in lower_headers:
            headers["Connection"] = "close"
        self.close_connection = True
        self.send_response(reply.status)
        for name, value in headers.items():
            self.send_header(name, value)
        self.end_headers()
        if reply.body:
            try:
                self.wfile.write(reply.body)
                self.wfile.flush()
            except OSError:
                # Cancellation intentionally closes an in-flight loopback socket.
                pass


class _LoopbackFixture:
    def __init__(self, responder: Callable[[_HttpRequest], _HttpReply]) -> None:
        self.responder = responder
        self._lock = threading.Lock()
        self._requests: list[_HttpRequest] = []
        self._errors: list[BaseException] = []
        self._server = ThreadingHTTPServer(("127.0.0.1", 0), _LoopbackHandler)
        self._server.daemon_threads = True
        self._server.fixture = self  # type: ignore[attr-defined]
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)
        self._thread.start()

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self._server.server_port}/mcp"

    @property
    def errors(self) -> tuple[BaseException, ...]:
        with self._lock:
            return tuple(self._errors)

    @property
    def requests(self) -> tuple[_HttpRequest, ...]:
        with self._lock:
            return tuple(self._requests)

    def close(self) -> None:
        self._server.shutdown()
        self._server.server_close()
        self._thread.join(timeout=1)

    def _record(self, request: _HttpRequest) -> None:
        with self._lock:
            self._requests.append(request)

    def _record_error(self, error: BaseException) -> None:
        with self._lock:
            self._errors.append(error)


def _json_bytes(value: Any) -> bytes:
    return json.dumps(value, separators=(",", ":"), ensure_ascii=False).encode("utf-8")


def _json_result(
    request: _HttpRequest,
    result: Any,
    *,
    headers: Optional[dict[str, str]] = None,
) -> _HttpReply:
    message = request.message()
    return _HttpReply(
        headers={"Content-Type": "application/json", **(headers or {})},
        body=_json_bytes({"jsonrpc": "2.0", "id": message["id"], "result": result}),
    )


def _initialize_result(name: str = "loopback-mcp") -> dict[str, Any]:
    return {
        "protocolVersion": "2025-06-18",
        "capabilities": {"tools": {"listChanged": False}},
        "serverInfo": {"name": name, "version": "1.0.0"},
    }


def _tool() -> dict[str, Any]:
    return {
        "name": "echo",
        "description": "Echo a bounded fixture value",
        "inputSchema": {
            "type": "object",
            "properties": {"value": {"type": "string"}},
            "required": ["value"],
            "additionalProperties": False,
        },
        "outputSchema": {
            "type": "object",
            "properties": {"echo": {"type": "string"}},
            "required": ["echo"],
            "additionalProperties": False,
        },
        "annotations": {"readOnlyHint": True},
    }


def _sse_event(message: dict[str, Any], *, event_id: Optional[str] = None, retry: Optional[int] = None) -> bytes:
    lines: list[str] = []
    if event_id is not None:
        lines.append(f"id: {event_id}")
    if retry is not None:
        lines.append(f"retry: {retry}")
    lines.extend(("event: message", f"data: {_json_bytes(message).decode('utf-8')}", ""))
    return ("\n".join(lines) + "\n").encode("utf-8")


def _remote_config(
    url: str,
    *,
    auth: Optional[HttpAuthConfig] = None,
    request_timeout_ms: int = 1000,
    max_restarts: int = 1,
) -> ServerConfig:
    return ServerConfig(
        id="remote",
        label="Reviewed loopback remote",
        command="",
        args=(),
        cwd=ROOT,
        environment={},
        enabled=True,
        required=False,
        startup_timeout_ms=1000,
        request_timeout_ms=request_timeout_ms,
        max_restarts=max_restarts,
        scope="user",
        transport="streamable-http",
        url=url,
        auth=auth,
    )


def _server_node(snapshot: dict[str, Any], server_id: str) -> dict[str, Any]:
    for node in snapshot["collection"]["nodes"]:
        if node["id"] == f"server:{server_id}":
            return node
    raise AssertionError(f"missing server node {server_id}")


class _TokenProvider:
    def __init__(self, token: str) -> None:
        self.token = token
        self.calls: list[tuple[str, str]] = []

    def bearer_token(self, credential: str, *, server_id: str) -> Optional[str]:
        self.calls.append((credential, server_id))
        return self.token


class StreamableHttpTests(unittest.TestCase):
    def setUp(self) -> None:
        self._fixtures: list[_LoopbackFixture] = []

    def tearDown(self) -> None:
        for fixture in reversed(self._fixtures):
            fixture.close()

    def fixture(self, responder: Callable[[_HttpRequest], _HttpReply]) -> _LoopbackFixture:
        fixture = _LoopbackFixture(responder)
        self._fixtures.append(fixture)
        return fixture

    def client(
        self,
        fixture: _LoopbackFixture,
        *,
        auth: Optional[HttpAuthConfig] = None,
        credential_provider: Optional[_TokenProvider] = None,
        request_timeout_ms: int = 1000,
        max_restarts: int = 1,
    ) -> McpStreamableHttpClient:
        return McpStreamableHttpClient(
            _remote_config(
                fixture.url,
                auth=auth,
                request_timeout_ms=request_timeout_ms,
                max_restarts=max_restarts,
            ),
            limits(shutdown_timeout_ms=250),
            credential_provider=credential_provider,
        )

    def test_json_session_lifecycle_and_stdio_compatible_catalog_calls(self) -> None:
        session = "loopback-session"

        def responder(request: _HttpRequest) -> _HttpReply:
            if request.method == "DELETE":
                return _HttpReply()
            message = request.message()
            method = message.get("method")
            if method == "initialize":
                return _json_result(
                    request,
                    _initialize_result(),
                    headers={
                        "Mcp-Session-Id": session,
                        "Set-Cookie": "untrusted-cookie=ignored",
                    },
                )
            if method == "notifications/initialized":
                return _HttpReply(status=202)
            if method == "tools/list":
                return _json_result(request, {"tools": [_tool()]})
            if method == "tools/call":
                value = message["params"]["arguments"]["value"]
                return _json_result(
                    request,
                    {
                        "content": [{"type": "text", "text": f"echo: {value}"}],
                        "structuredContent": {"echo": value},
                        "isError": False,
                    },
                )
            return _HttpReply(status=400)

        fixture = self.fixture(responder)
        client = self.client(fixture)
        try:
            client.start()
            self.assertEqual([item["name"] for item in client.list_tools()], ["echo"])
            result = client.call_tool("echo", {"value": "hello"})
            self.assertEqual(result["structuredContent"], {"echo": "hello"})
            self.assertTrue(client.alive)
        finally:
            client.close()

        self.assertEqual(fixture.errors, ())
        requests = fixture.requests
        self.assertTrue(all(request.target == "/mcp" for request in requests))
        self.assertTrue(all(request.header("cookie") is None for request in requests))
        initialize = next(request for request in requests if request.method == "POST" and request.message()["method"] == "initialize")
        initialized = next(
            request
            for request in requests
            if request.method == "POST" and request.message()["method"] == "notifications/initialized"
        )
        tools_list = next(request for request in requests if request.method == "POST" and request.message()["method"] == "tools/list")
        tools_call = next(request for request in requests if request.method == "POST" and request.message()["method"] == "tools/call")
        delete = next(request for request in requests if request.method == "DELETE")
        self.assertEqual(initialize.header("accept"), "application/json, text/event-stream")
        self.assertIsNone(initialize.header("mcp-protocol-version"))
        for request in (initialized, tools_list, tools_call, delete):
            self.assertEqual(request.header("mcp-session-id"), session)
            self.assertEqual(request.header("mcp-protocol-version"), "2025-06-18")
        self.assertEqual(delete.header("accept"), "application/json, text/event-stream")
        self.assertIsNone(delete.header("content-type"))

    def test_session_identity_change_is_rejected(self) -> None:
        session = "initial-session"

        def responder(request: _HttpRequest) -> _HttpReply:
            if request.method == "DELETE":
                return _HttpReply()
            message = request.message()
            if message["method"] == "initialize":
                return _json_result(
                    request,
                    _initialize_result(),
                    headers={"Mcp-Session-Id": session},
                )
            if message["method"] == "notifications/initialized":
                return _HttpReply(status=202)
            if message["method"] == "tools/list":
                return _json_result(
                    request,
                    {"tools": []},
                    headers={"Mcp-Session-Id": "changed-session"},
                )
            return _HttpReply(status=400)

        fixture = self.fixture(responder)
        client = self.client(fixture)
        try:
            client.start()
            with self.assertRaises(McpError) as raised:
                client.list_tools()
            self.assertEqual(raised.exception.code, "session_identity_changed")
            self.assertFalse(client.alive)
        finally:
            client.close()

        self.assertEqual(fixture.errors, ())

    def test_bearer_adapter_is_request_scoped_and_remote_echo_is_redacted(self) -> None:
        token = "jsonrpc"
        provider = _TokenProvider(token)
        session = "auth-session"

        def responder(request: _HttpRequest) -> _HttpReply:
            if request.method == "DELETE":
                return _HttpReply()
            message = request.message()
            if message["method"] == "initialize":
                return _json_result(
                    request,
                    _initialize_result(name=token),
                    headers={"Mcp-Session-Id": session},
                )
            if message["method"] == "notifications/initialized":
                return _HttpReply(status=202)
            return _HttpReply(status=400)

        fixture = self.fixture(responder)
        client = self.client(
            fixture,
            auth=HttpAuthConfig(credential="reviewed_remote"),
            credential_provider=provider,
        )
        try:
            client.start()
            self.assertEqual(client.server_info["name"], "[redacted]")
            self.assertNotIn(token, repr(client.config))
            self.assertNotIn(token, " ".join(entry.text for entry in client.logs.snapshot()))
        finally:
            client.close()

        self.assertEqual(fixture.errors, ())
        self.assertGreaterEqual(len(provider.calls), 3)
        self.assertTrue(all(call == ("reviewed_remote", "remote") for call in provider.calls))
        self.assertTrue(all(request.header("authorization") == f"Bearer {token}" for request in fixture.requests))

    def test_sse_response_resumes_once_without_reposting_the_tool_call(self) -> None:
        session = "resume-session"
        state: dict[str, Any] = {"tool_request_id": None, "last_event_id": None}

        def responder(request: _HttpRequest) -> _HttpReply:
            if request.method == "DELETE":
                return _HttpReply()
            if request.method == "GET":
                request_id = state["tool_request_id"]
                if request.header("last-event-id") != "resume-1" or request_id is None:
                    return _HttpReply(status=400)
                state["last_event_id"] = request.header("last-event-id")
                body = _sse_event(
                    {
                        "jsonrpc": "2.0",
                        "id": request_id,
                        "result": {
                            "content": [{"type": "text", "text": "resumed"}],
                            "isError": False,
                        },
                    },
                    event_id="resume-2",
                )
                return _HttpReply(
                    headers={"Content-Type": "text/event-stream"},
                    body=body,
                    include_content_length=False,
                )
            message = request.message()
            method = message["method"]
            if method == "initialize":
                return _json_result(
                    request,
                    _initialize_result(),
                    headers={"Mcp-Session-Id": session},
                )
            if method == "notifications/initialized":
                return _HttpReply(status=202)
            if method == "tools/call":
                state["tool_request_id"] = message["id"]
                progress = {
                    "jsonrpc": "2.0",
                    "method": "notifications/progress",
                    "params": {
                        "progressToken": message["params"]["_meta"]["progressToken"],
                        "progress": 1,
                        "total": 2,
                    },
                }
                return _HttpReply(
                    headers={"Content-Type": "text/event-stream"},
                    body=_sse_event(progress, event_id="resume-1", retry=1),
                    include_content_length=False,
                )
            return _HttpReply(status=400)

        fixture = self.fixture(responder)
        client = self.client(fixture)
        progress: list[dict[str, Any]] = []
        try:
            client.start()
            result = client.call_tool("echo", {"value": "ignored"}, progress=progress.append)
        finally:
            client.close()

        self.assertEqual(result["content"][0]["text"], "resumed")
        self.assertEqual(state["last_event_id"], "resume-1")
        self.assertEqual(progress, [{"progressToken": "ygg-mcp:2", "progress": 1, "total": 2}])
        tool_posts = [
            request
            for request in fixture.requests
            if request.method == "POST" and request.message().get("method") == "tools/call"
        ]
        resume_gets = [request for request in fixture.requests if request.method == "GET"]
        self.assertEqual(len(tool_posts), 1)
        self.assertEqual(len(resume_gets), 1)
        self.assertEqual(resume_gets[0].header("accept"), "text/event-stream")
        self.assertEqual(resume_gets[0].header("mcp-session-id"), session)
        self.assertEqual(fixture.errors, ())

    def test_cancellation_aborts_socket_and_sends_one_cancel_notification(self) -> None:
        session = "cancel-session"
        call_received = threading.Event()
        release_call = threading.Event()
        cancelled_received = threading.Event()

        def responder(request: _HttpRequest) -> _HttpReply:
            if request.method == "DELETE":
                return _HttpReply()
            message = request.message()
            method = message["method"]
            if method == "initialize":
                return _json_result(
                    request,
                    _initialize_result(),
                    headers={"Mcp-Session-Id": session},
                )
            if method == "notifications/initialized":
                return _HttpReply(status=202)
            if method == "tools/call":
                call_received.set()
                release_call.wait(1)
                return _json_result(
                    request,
                    {"content": [{"type": "text", "text": "late"}], "isError": False},
                )
            if method == "notifications/cancelled":
                cancelled_received.set()
                return _HttpReply(status=202)
            return _HttpReply(status=400)

        fixture = self.fixture(responder)
        client = self.client(fixture, request_timeout_ms=1000)
        cancellation = FakeCancellation()
        outcome: list[BaseException] = []
        try:
            client.start()

            def call() -> None:
                try:
                    client.call_tool("echo", {"value": "ignored"}, cancellation=cancellation)
                except BaseException as error:
                    outcome.append(error)

            caller = threading.Thread(target=call)
            caller.start()
            self.assertTrue(call_received.wait(1), "loopback tool call was not received")
            cancellation.cancel("operator")
            caller.join(timeout=1)
            self.assertFalse(caller.is_alive())
            self.assertEqual(len(outcome), 1)
            self.assertIsInstance(outcome[0], McpCancelled)
            self.assertTrue(cancelled_received.wait(1), "cancellation notification was not received")
            self.assertTrue(client.alive)
        finally:
            release_call.set()
            wait_for(lambda: not client._operations, timeout=1, message="cancelled socket cleanup")
            client.close()

        cancellations = [
            request.message()
            for request in fixture.requests
            if request.method == "POST" and request.message().get("method") == "notifications/cancelled"
        ]
        self.assertEqual(len(cancellations), 1)
        self.assertEqual(cancellations[0]["params"], {"requestId": 2, "reason": "operator"})
        tool_posts = [
            request
            for request in fixture.requests
            if request.method == "POST" and request.message().get("method") == "tools/call"
        ]
        self.assertEqual(len(tool_posts), 1)
        self.assertEqual(fixture.errors, ())

    def test_status_content_type_and_payload_failures_are_safe_and_bounded(self) -> None:
        secret = "REMOTE_ERROR_BODY_SHOULD_NOT_ESCAPE"

        def responder(request: _HttpRequest) -> _HttpReply:
            if request.target == "/redirect":
                return _HttpReply(status=302, headers={"Location": "/other"}, body=secret.encode())
            if request.target == "/auth":
                return _HttpReply(
                    status=401,
                    headers={"Mcp-Session-Id": "untrusted-error-session"},
                    body=secret.encode(),
                )
            if request.target == "/rate":
                return _HttpReply(status=429, headers={"Retry-After": "2"}, body=secret.encode())
            if request.target == "/content-type":
                return _HttpReply(headers={"Content-Type": "text/plain"}, body=secret.encode())
            if request.target == "/large":
                return _HttpReply(
                    headers={"Content-Type": "application/json", "Content-Length": "2048"},
                    body=b"x" * 2048,
                    include_content_length=False,
                )
            if request.target == "/json":
                return _HttpReply(headers={"Content-Type": "application/json"}, body=b"{bad")
            if request.target == "/sse":
                return _HttpReply(headers={"Content-Type": "text/event-stream"}, body=b"data: {bad\n\n")
            return _HttpReply(status=500)

        fixture = self.fixture(responder)
        expected = {
            "/redirect": "redirect_rejected",
            "/auth": "authentication_required",
            "/rate": "http_rate_limited",
            "/content-type": "invalid_content_type",
            "/large": "http_body_too_large",
            "/json": "malformed_http_body",
            "/sse": "malformed_sse_event",
        }
        for path, code in expected.items():
            client = McpStreamableHttpClient(
                _remote_config(fixture.url.removesuffix("/mcp") + path),
                limits(max_frame_bytes=1024, max_result_bytes=1024, shutdown_timeout_ms=250),
            )
            with self.assertRaises(McpError) as raised:
                client.start()
            error = raised.exception
            self.assertEqual(error.code, code)
            self.assertNotIn(secret, str(error))
            self.assertFalse(client.alive)
            if path == "/rate":
                self.assertIsInstance(error, McpTransportError)
                self.assertEqual(error.retry_after_ms, 2000)
            if path == "/auth":
                self.assertIsInstance(error, McpAuthenticationError)
                self.assertIsNone(client._session_id)
            client.close()
        self.assertFalse(any(request.target == "/other" for request in fixture.requests))
        self.assertEqual(fixture.errors, ())

    def test_interrupted_sse_without_an_event_id_is_not_replayed(self) -> None:
        session = "interrupted-session"

        def responder(request: _HttpRequest) -> _HttpReply:
            if request.method == "DELETE":
                return _HttpReply()
            message = request.message()
            if message["method"] == "initialize":
                return _json_result(
                    request,
                    _initialize_result(),
                    headers={"Mcp-Session-Id": session},
                )
            if message["method"] == "notifications/initialized":
                return _HttpReply(status=202)
            if message["method"] == "tools/call":
                return _HttpReply(
                    headers={"Content-Type": "text/event-stream"},
                    body=_sse_event(
                        {
                            "jsonrpc": "2.0",
                            "method": "notifications/message",
                            "params": {"level": "info", "data": "partial"},
                        }
                    ),
                    include_content_length=False,
                )
            return _HttpReply(status=400)

        fixture = self.fixture(responder)
        client = self.client(fixture)
        try:
            client.start()
            with self.assertRaises(McpTransportError) as raised:
                client.call_tool("echo", {"value": "ignored"})
            self.assertEqual(raised.exception.code, "sse_response_interrupted")
            self.assertTrue(raised.exception.ambiguous)
        finally:
            client.close()

        tool_posts = [
            request
            for request in fixture.requests
            if request.method == "POST" and request.message().get("method") == "tools/call"
        ]
        self.assertEqual(len(tool_posts), 1)
        self.assertFalse(any(request.method == "GET" for request in fixture.requests))

    def test_manager_wires_credential_adapter_and_preserves_remote_transport_status(self) -> None:
        token = "MANAGER_ADAPTER_TOKEN"
        provider = _TokenProvider(token)
        session = "manager-session"

        def responder(request: _HttpRequest) -> _HttpReply:
            if request.method == "DELETE":
                return _HttpReply()
            message = request.message()
            method = message["method"]
            if method == "initialize":
                return _json_result(
                    request,
                    _initialize_result(),
                    headers={"Mcp-Session-Id": session},
                )
            if method == "notifications/initialized":
                return _HttpReply(status=202)
            if method == "tools/list":
                return _json_result(request, {"tools": [_tool()]})
            if method == "tools/call":
                value = message["params"]["arguments"]["value"]
                return _json_result(
                    request,
                    {
                        "content": [{"type": "text", "text": f"echo: {value}"}],
                        "structuredContent": {"echo": value},
                        "isError": False,
                    },
                )
            return _HttpReply(status=400)

        fixture = self.fixture(responder)
        with tempfile.TemporaryDirectory() as directory:
            extension = FakeExtension(Path(directory))
            manager = BridgeManager(
                extension,
                BridgeConfig(
                    servers=(
                        _remote_config(
                            fixture.url,
                            auth=HttpAuthConfig(credential="managed_remote"),
                        ),
                    ),
                    limits=limits(backoff_initial_ms=10, backoff_max_ms=20, shutdown_timeout_ms=250),
                ),
                scratch_directory=Path(directory),
                credential_provider=provider,
                experimental_streamable_http_mcp=True,
            )
            try:
                manager.start()
                wait_for(
                    lambda: _server_node(manager.snapshot(), "remote")["state"] == "active",
                    message="remote manager ready",
                )
                tool_name = next(iter(extension._tools))
                result = extension._tools[tool_name]["handler"]({"value": "managed"}, {})
                self.assertFalse(result["is_error"])
                self.assertEqual(result["structured_content"], {"echo": "managed"})
                encoded = json.dumps(manager.snapshot())
                self.assertIn("streamable-http", encoded)
                self.assertNotIn(token, encoded)
                self.assertNotIn(fixture.url, encoded)
            finally:
                manager.shutdown()

        self.assertEqual(fixture.errors, ())
        self.assertGreaterEqual(len(provider.calls), 5)
        self.assertTrue(all(request.header("authorization") == f"Bearer {token}" for request in fixture.requests))

    def test_manager_fails_closed_when_auth_has_no_adapter(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            extension = FakeExtension(Path(directory))
            manager = BridgeManager(
                extension,
                BridgeConfig(
                    servers=(
                        _remote_config(
                            "http://127.0.0.1:9/mcp",
                            auth=HttpAuthConfig(credential="missing_adapter"),
                            max_restarts=0,
                        ),
                    ),
                    limits=limits(backoff_initial_ms=10, backoff_max_ms=20, shutdown_timeout_ms=250),
                ),
                scratch_directory=Path(directory),
                experimental_streamable_http_mcp=True,
            )
            try:
                manager.start()
                wait_for(
                    lambda: _server_node(manager.snapshot(), "remote")["state"] == "unavailable",
                    message="unavailable auth parked",
                )
                detail = manager.execute_command(["show", "remote"])["text"]
                self.assertIn("authentication_unavailable", detail)
                self.assertEqual(extension._tools, {})
            finally:
                manager.shutdown()


if __name__ == "__main__":
    unittest.main()
