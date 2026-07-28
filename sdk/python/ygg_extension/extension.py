"""Public Python API for Ygg executable extensions."""

from __future__ import annotations

import inspect
import os
import threading
from collections.abc import Mapping
from dataclasses import dataclass
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


@dataclass
class _Tool:
    name: str
    description: str
    parameters: dict[str, Any]
    handler: Handler


@dataclass
class _Command:
    name: str
    description: str
    usage: Optional[str]
    handler: Handler


class Extension:
    """Define and run a Ygg executable extension.

    The host supplies the manifest and runtime context in the first
    ``initialize`` request.  Tool and command decorators must therefore use the
    same names as the manifest's ``[contributes]`` entries.  Other contribution
    points are optional and default to an empty/continue response when they do
    not have a handler.

    Handlers are synchronous functions.  A tool handler receives its argument
    object and may optionally accept the request context as a second argument::

        ext = Extension()

        @ext.tool(name="hello_world", description="Greet someone")
        def hello(args):
            return {"content": f"Hello, {args.get('name', 'world')}!"}

        ext.run()

    The SDK is deliberately dependency-free.  Install the package with
    ``python -m pip install sdk/python`` from a Ygg checkout, or vendor the
    ``ygg_extension`` package alongside an extension.
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
    ) -> None:
        self.api_version = api_version or os.environ.get(
            "YGG_EXTENSION_API_VERSION", DEFAULT_API_VERSION
        )
        self.stdin = stdin
        self.stdout = stdout
        self.logger = logger or Logger(stderr)
        self.log = self.logger
        self.max_message_bytes = max_message_bytes

        self._tools: dict[str, _Tool] = {}
        self._commands: dict[str, _Command] = {}
        self._hooks: dict[str, Handler] = {}
        self._context_handler: Optional[Handler] = None
        self._status_handlers: dict[str, Handler] = {}
        self._renderer_handlers: dict[str, Handler] = {}
        self._shutdown_handler: Optional[Handler] = None

        self._transport: Optional[JsonRpcTransport] = None
        self._initialized = False
        self._running = False
        self._closed = False
        self._stopping = False
        self._next_request_id = 1
        self._responses: list[dict[str, Any]] = []
        self._initialization: Optional[dict[str, Any]] = None
        self._declared: dict[str, Any] = {}
        self._lock = threading.RLock()

    @property
    def initialized(self) -> bool:
        """Whether the host initialize handshake has completed."""

        return self._initialized

    @property
    def running(self) -> bool:
        """Whether :meth:`run` is currently processing stdin."""

        return self._running

    @property
    def initialization(self) -> Optional[dict[str, Any]]:
        """The bounded initialize parameters received from the host."""

        return self._initialization

    @property
    def host(self) -> dict[str, Any]:
        """The host session/model state from the initialize request."""

        if not self._initialization:
            return {}
        value = self._initialization.get("host")
        return dict(value) if isinstance(value, Mapping) else {}

    @property
    def workspace(self) -> Optional[str]:
        """The active workspace from initialization, when supplied."""

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
    ) -> Callable[[Handler], Handler]:
        """Register a model-callable tool and return its decorator."""

        self._validate_name("tool", name)
        if not isinstance(description, str) or not description.strip():
            raise ValueError("tool description must be non-empty")
        schema = dict(parameters) if parameters is not None else {"type": "object"}
        if not isinstance(schema, dict):
            raise TypeError("tool parameters must be a JSON Schema object")

        def decorate(handler: Handler) -> Handler:
            if name in self._tools:
                raise ValueError(f"duplicate tool: {name}")
            self._tools[name] = _Tool(name, description, schema, handler)
            return handler

        return decorate

    def command(
        self,
        *,
        name: str,
        description: str,
        usage: Optional[str] = None,
    ) -> Callable[[Handler], Handler]:
        """Register a slash command (without the leading slash)."""

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
        """Register a lifecycle hook such as ``before_prompt``."""

        self._validate_name("hook", name)

        def decorate(handler: Handler) -> Handler:
            if name in self._hooks:
                raise ValueError(f"duplicate hook: {name}")
            self._hooks[name] = handler
            return handler

        return decorate

    def context(self, handler: Optional[Handler] = None) -> Any:
        """Register a prompt-context handler.

        The handler receives the full ``context/collect`` params object, which
        includes ``prompt`` and the ambient ``context`` object.
        """

        def decorate(callback: Handler) -> Handler:
            if self._context_handler is not None:
                raise ValueError("duplicate context handler")
            self._context_handler = callback
            return callback

        return decorate(handler) if handler is not None else decorate

    def status(self, surface: Any = "status") -> Any:
        """Register a semantic status/header/footer handler.

        The handler receives the full ``status/collect`` params object.
        ``@ext.status`` is shorthand for the ``status`` surface.
        """

        if callable(surface):
            handler = surface
            surface = "status"
            return self._register_status(surface, handler)
        self._validate_name("UI surface", surface)

        def decorate(handler: Handler) -> Handler:
            return self._register_status(surface, handler)

        return decorate

    def renderer(self, name: str) -> Callable[[Handler], Handler]:
        """Register a semantic renderer for a tool name."""

        self._validate_name("renderer", name)

        def decorate(handler: Handler) -> Handler:
            if name in self._renderer_handlers:
                raise ValueError(f"duplicate renderer: {name}")
            self._renderer_handlers[name] = handler
            return handler

        return decorate

    tool_renderer = renderer

    def on_shutdown(self, handler: Handler) -> Handler:
        """Register a callback run after the host acknowledges shutdown."""

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
        """Send a user-visible process-to-host notification."""

        if level not in {"info", "success", "warning", "error"}:
            raise ValueError(f"unknown notification level: {level}")
        self._require_capability("notifications")
        params: dict[str, Any] = {"level": level, "message": str(message)}
        if title is not None:
            params["title"] = str(title)
        self._send(
            {
                "jsonrpc": "2.0",
                "method": "notification",
                "params": params,
            }
        )

    send_notification = notify

    def request(self, method: str, params: Optional[Mapping[str, Any]] = None) -> Any:
        """Send a correlated request to the host and wait for its response.

        This is primarily useful for extension-originated confirmation flows.
        While waiting, host requests and notifications are dispatched normally,
        so a synchronous handler can safely ask the host a question.
        """

        if not isinstance(method, str) or not method:
            raise ValueError("request method must be non-empty")
        self._require_initialized()
        request_id = self._next_request_id
        self._next_request_id += 1
        self._send(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "method": method,
                "params": dict(params) if params is not None else {},
            }
        )
        response = self._wait_for_response(request_id)
        if "error" in response:
            raise RpcError.from_response(response)
        return response.get("result")

    def confirm(
        self,
        prompt: str,
        *,
        detail: Optional[str] = None,
        destructive: bool = False,
        default: bool = False,
    ) -> bool:
        """Ask the host for an interactive confirmation and return its answer."""

        self._require_capability("confirmations")
        params: dict[str, Any] = {
            "prompt": str(prompt),
            "destructive": bool(destructive),
            "default": bool(default),
        }
        if detail is not None:
            params["detail"] = str(detail)
        result = self.request("confirmation/request", params)
        if not isinstance(result, Mapping) or not isinstance(result.get("confirmed"), bool):
            raise RpcError(-32603, "invalid confirmation response")
        return bool(result["confirmed"])

    def run(self, *, stdin: Any = None, stdout: Any = None) -> None:
        """Run the blocking stdio loop until shutdown or stdin EOF."""

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
        self._transport = JsonRpcTransport(
            reader,
            writer,
            max_message_bytes=self.max_message_bytes,
        )
        self._running = True
        self._closed = False
        try:
            while self._running and not self._closed:
                try:
                    message = self._transport.read()
                except RpcError as error:
                    self.logger.error("invalid protocol input", code=error.code, error=error.message)
                    self._send_error(None, error)
                    continue
                if message is None:
                    self._closed = True
                    self.logger.info("extension stdin closed")
                    break
                if not self._process_message(message):
                    break
        except (BrokenPipeError, EOFError):
            self._closed = True
            self.logger.info("extension protocol stream closed")
        finally:
            self._running = False
            self._transport = None

    def handle_message(self, message: Mapping[str, Any]) -> bool:
        """Process one already-decoded message; useful for embedding/tests."""

        if self._transport is None:
            raise RuntimeError("run must establish a transport before handling messages")
        if not isinstance(message, Mapping):
            raise ProtocolError(-32600, "JSON-RPC message must be an object")
        return self._process_message(dict(message))

    def _process_message(self, message: dict[str, Any]) -> bool:
        if "method" not in message:
            if "id" in message and ("result" in message or "error" in message):
                self._responses.append(message)
                return True
            self._send_error(message.get("id"), ProtocolError(-32600, "invalid JSON-RPC request"))
            return True

        method = message.get("method")
        if not isinstance(method, str) or not method:
            self._send_error(message.get("id"), ProtocolError(-32600, "method must be a string"))
            return True
        request_id = message.get("id", _MISSING)
        try:
            result = self._dispatch(method, message.get("params", {}))
        except RpcError as error:
            if request_id is not _MISSING:
                self._send_error(request_id, error)
            self.logger.error("request failed", method=method, code=error.code, error=error.message)
            return not self._stopping
        except Exception as error:  # Extension code must not corrupt stdout.
            self.logger.error("request handler failed", method=method, error=str(error))
            if request_id is not _MISSING:
                self._send_error(request_id, RpcError(-32603, "internal error"))
            return not self._stopping

        if request_id is not _MISSING:
            self._send_result(request_id, result)
        return not self._stopping

    def _dispatch(self, method: str, params: Any) -> Any:
        if method == "initialize":
            return self._initialize(params)
        if method == "shutdown":
            self._require_initialized()
            if self._shutdown_handler is not None:
                self._invoke(self._shutdown_handler, params, self._context_from(params))
            self._stopping = True
            return {}
        self._require_initialized()
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
        host_version = params.get("api_version")
        if host_version != self.api_version:
            raise RpcError(
                -32000,
                f"unsupported API version: host requested {host_version!r}, SDK implements {self.api_version!r}",
            )
        contributes = params.get("contributes")
        if contributes is None:
            contributes = {}
        if not isinstance(contributes, Mapping):
            raise RpcError(-32602, "initialize contributes must be an object")
        self._declared = dict(contributes)
        self._validate_declarations()
        self._initialization = dict(params)
        self._initialized = True
        return {
            "api_version": self.api_version,
            "tools": [
                {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                }
                for tool in self._tools.values()
            ],
            "commands": [
                {
                    "name": command.name,
                    "description": command.description,
                    **({"usage": command.usage} if command.usage is not None else {}),
                }
                for command in self._commands.values()
            ],
        }

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
        tool = self._tools.get(name) if isinstance(name, str) else None
        if tool is None:
            raise RpcError(-32601, f"unknown tool: {name}")
        arguments = request.get("arguments", {})
        context = self._context_from(request)
        try:
            value = self._invoke(tool.handler, arguments, context)
        except Exception as error:
            self.logger.error("tool handler failed", tool=name, error=str(error))
            return {"content": str(error), "is_error": True, "metadata": {}}
        return self._tool_result(value)

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
        except RpcError:
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
            value = self._invoke(
                handler,
                request.get("payload", {}),
                self._context_from(request),
            )
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
        try:
            value = self._invoke(
                self._context_handler,
                request,
                self._context_from(request),
            )
        except Exception as error:
            self.logger.error("context handler failed", error=str(error))
            raise RpcError(-32603, "internal error") from error
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
        try:
            value = self._invoke(handler, request, self._context_from(request))
        except Exception as error:
            self.logger.error("status handler failed", surface=surface, error=str(error))
            raise RpcError(-32603, "internal error") from error
        return self._status_result(value)

    def _render_tool(self, params: Any) -> dict[str, Any]:
        request = self._object_params(params, "tool/render")
        name = request.get("name")
        if not isinstance(name, str) or not name:
            raise RpcError(-32602, "renderer name must be a string")
        self._require_declared_name("tool_renderers", name)
        handler = self._renderer_handlers.get(name)
        if handler is None:
            return {"segments": []}
        try:
            value = self._invoke(handler, request, self._context_from(request))
        except Exception as error:
            self.logger.error("renderer handler failed", renderer=name, error=str(error))
            raise RpcError(-32603, "internal error") from error
        return self._render_result(value)

    def _wait_for_response(self, request_id: Any) -> dict[str, Any]:
        response = self._take_response(request_id)
        while response is None:
            if self._transport is None:
                raise RpcError(-32000, "protocol transport is closed")
            try:
                message = self._transport.read()
            except RpcError as error:
                self.logger.error("invalid protocol input while waiting", error=error.message)
                continue
            if message is None:
                self._closed = True
                raise RpcError(-32000, "stdin closed while waiting for host response")
            if "method" in message:
                if not self._process_message(message):
                    raise RpcError(-32000, "host shut down the extension")
            elif "id" in message and ("result" in message or "error" in message):
                self._responses.append(message)
            else:
                self.logger.warning("ignored invalid response message")
            response = self._take_response(request_id)
        return response

    def _take_response(self, request_id: Any) -> Optional[dict[str, Any]]:
        for index, response in enumerate(self._responses):
            if self._same_id(response.get("id"), request_id):
                return self._responses.pop(index)
        return None

    @staticmethod
    def _same_id(left: Any, right: Any) -> bool:
        return type(left) is type(right) and left == right

    def _send(self, message: Mapping[str, Any]) -> None:
        if self._transport is None or self._closed:
            raise RpcError(-32000, "protocol transport is closed")
        try:
            self._transport.send(message)
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

    def _require_declared_name(self, key: str, name: str) -> None:
        value = self._declared.get(key, _MISSING)
        if value is not _MISSING and (not isinstance(value, list) or name not in value):
            raise RpcError(-32601, f"{name} is not a declared {key.rstrip('s')}")

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
    def _tool_result(value: Any) -> dict[str, Any]:
        if isinstance(value, Mapping):
            content = value.get("content", "")
            metadata = value.get("metadata", {})
            result = {
                "content": content if isinstance(content, str) else str(content),
                "is_error": bool(value.get("is_error", False)),
                "metadata": metadata,
            }
            return result
        return {"content": "" if value is None else str(value), "is_error": False, "metadata": {}}

    @staticmethod
    def _command_result(value: Any) -> dict[str, Any]:
        if value is None:
            return {"text": "", "notifications": [], "context": []}
        if isinstance(value, Mapping):
            return {
                "text": value.get("text", "") if isinstance(value.get("text", ""), str) else str(value.get("text")),
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
        if isinstance(value, Mapping):
            segments = value.get("segments", [])
        else:
            segments = [{"text": str(value), "style_role": None}]
        if not isinstance(segments, list):
            raise RpcError(-32603, "renderer must return a segments array")
        return {"segments": segments}

    def _register_status(self, surface: str, handler: Handler) -> Handler:
        if surface in self._status_handlers:
            raise ValueError(f"duplicate status handler: {surface}")
        self._status_handlers[surface] = handler
        return handler

    @staticmethod
    def _validate_name(kind: str, name: Any) -> None:
        if not isinstance(name, str) or not name.strip():
            raise ValueError(f"{kind} name must be non-empty")

    @staticmethod
    def _invoke(handler: Handler, *args: Any) -> Any:
        """Call one- or two-argument handlers without hiding handler errors."""

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
        if any(parameter.kind == inspect.Parameter.VAR_POSITIONAL for parameter in signature.parameters.values()):
            return handler(*args)
        if len(positional) >= len(args):
            return handler(*args)
        if len(positional) == 1:
            return handler(args[0])
        return handler()
