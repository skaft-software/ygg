#!/usr/bin/env python3
"""Ygg LSP extension: read-only semantic code intelligence over the LSP.

Design notes (see issue #23):
- Text-first: every tool result is a typed, bounded dict; when no server is
  configured, installed, or healthy the model receives a structured
  unavailable result and falls back to `read`/search/build checks.
- No hidden mutation: the extension only sends didOpen/didChange to keep the
  server's view of a file current. It never accepts server-originated edits.
- Bounded waits and bounded results: every request has a deadline and every
  result list is capped before it can reach the model.
- Diagnostics are delivered once per change: the before_prompt hook injects
  only diagnostics not previously injected for the current file content.
"""

import atexit
import json
import os
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Any, Dict, List, Optional
from urllib.parse import unquote, urlparse

ROOT = Path(os.environ.get("YGG_EXTENSION_DIR", Path(__file__).resolve().parent)).resolve()
sys.path.insert(0, str(ROOT / "vendor"))
sys.path.insert(0, str(ROOT))

from ygg_extension import Extension  # noqa: E402

# --- Configuration ---------------------------------------------------------
# Keyed by file suffix. Servers are never downloaded or installed by Ygg; a
# missing binary surfaces as a typed unavailable result.
DEFAULT_SERVERS: Dict[str, List[str]] = {
    ".rs": ["rust-analyzer"],
    ".py": ["pyright-langserver", "--stdio"],
    ".ts": ["typescript-language-server", "--stdio"],
    ".tsx": ["typescript-language-server", "--stdio"],
    ".js": ["typescript-language-server", "--stdio"],
    ".jsx": ["typescript-language-server", "--stdio"],
}

INIT_TIMEOUT_S = 20.0
REQUEST_TIMEOUT_S = 15.0
# rust-analyzer and friends index the workspace after initialize; early
# navigation requests legitimately return empty results. Retry briefly so a
# warm-up window does not look like "symbol not found".
DEFINITION_WARMUP_RETRIES = 5
DEFINITION_WARMUP_DELAY_S = 1.0
MAX_DEFINITIONS = 10
MAX_REFERENCES = 100
MAX_DIAGNOSTICS_PER_FILE = 20
MAX_DIAGNOSTIC_INJECTIONS = 30
MAX_HOVER_CHARS = 2000
MAX_SYNC_BYTES = 2 * 1024 * 1024
MAX_CONSECUTIVE_START_FAILURES = 3
MAX_LIFETIME_STARTS = 10

SEVERITY_NAMES = {1: "error", 2: "warning", 3: "information", 4: "hint"}


def path_to_uri(path: Path) -> str:
    return path.resolve().as_uri()


def uri_to_path(uri: str) -> str:
    parsed = urlparse(uri)
    if parsed.scheme != "file":
        return uri
    return os.path.normpath(unquote(parsed.path))


# --- LSP client ------------------------------------------------------------


class ServerUnavailable(Exception):
    """Typed failure: the server cannot currently answer queries."""


class LspClient:
    """One language server subprocess with a bounded, thread-safe JSON-RPC bridge."""

    def __init__(self, command: List[str]) -> None:
        self.command = command
        self.process: Optional[subprocess.Popen] = None
        self.capabilities: Dict[str, Any] = {}
        self.diagnostics: Dict[str, List[dict]] = {}
        self.documents: Dict[str, dict] = {}
        self._write_lock = threading.Lock()
        self._state_lock = threading.Lock()
        self._pending: Dict[int, dict] = {}
        self._next_id = 1
        self._start_lock = threading.Lock()
        self._consecutive_failures = 0
        self._lifetime_starts = 0

    # -- lifecycle ----------------------------------------------------------

    def is_running(self) -> bool:
        return self.process is not None and self.process.poll() is None

    def ensure_started(self, workspace: Optional[str]) -> bool:
        """Start and initialize the server. Returns False on typed, bounded failure."""
        with self._start_lock:
            if self.is_running():
                return True
            if self._consecutive_failures >= MAX_CONSECUTIVE_START_FAILURES:
                return False
            if self._lifetime_starts >= MAX_LIFETIME_STARTS:
                return False
            self._lifetime_starts += 1
            try:
                self.process = subprocess.Popen(
                    self.command,
                    stdin=subprocess.PIPE,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.DEVNULL,
                    start_new_session=True,
                )
            except OSError:
                self._consecutive_failures += 1
                self.process = None
                return False
            self._reader = threading.Thread(target=self._read_loop, daemon=True)
            self._reader.start()
            try:
                root = str(Path(workspace).resolve()) if workspace else str(Path.cwd())
                result = self._request(
                    "initialize",
                    {
                        "processId": os.getpid(),
                        "rootUri": Path(root).as_uri(),
                        "workspaceFolders": [{"uri": Path(root).as_uri(), "name": Path(root).name}],
                        "capabilities": {},
                    },
                    timeout=INIT_TIMEOUT_S,
                )
                self.capabilities = result or {}
                self._notify("initialized", {})
                self._consecutive_failures = 0
                return True
            except Exception:
                self._kill()
                self._consecutive_failures += 1
                return False

    def shutdown(self) -> None:
        if not self.is_running():
            return
        try:
            self._request("shutdown", None, timeout=2.0)
            self._notify("exit", None)
        except Exception:
            pass
        self._kill()

    def _kill(self) -> None:
        process, self.process = self.process, None
        if process is None:
            return
        try:
            os.killpg(os.getpgid(process.pid), signal.SIGKILL)
        except (ProcessLookupError, PermissionError, OSError):
            try:
                process.kill()
            except OSError:
                pass
        try:
            process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            pass
        self._fail_all_pending("server stopped")

    # -- wire protocol ------------------------------------------------------

    def _framed(self, message: dict) -> bytes:
        body = json.dumps(message).encode("utf-8")
        return b"Content-Length: " + str(len(body)).encode("ascii") + b"\r\n\r\n" + body

    def _send(self, message: dict) -> None:
        if not self.is_running():
            raise ServerUnavailable("language server is not running")
        with self._write_lock:
            assert self.process is not None and self.process.stdin is not None
            self.process.stdin.write(self._framed(message))
            self.process.stdin.flush()

    def _request(self, method: str, params: Any, timeout: float) -> Any:
        with self._state_lock:
            rid = self._next_id
            self._next_id += 1
            entry = {"event": threading.Event(), "result": None, "error": None}
            self._pending[rid] = entry
        try:
            self._send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        except Exception:
            with self._state_lock:
                self._pending.pop(rid, None)
            raise
        if not entry["event"].wait(timeout):
            with self._state_lock:
                self._pending.pop(rid, None)
            raise ServerUnavailable(f"{method} timed out after {timeout:.0f}s")
        if entry["error"] is not None:
            error = entry["error"]
            raise ServerUnavailable(f"{method} failed: {error.get('message', error)}")
        return entry["result"]

    def _notify(self, method: str, params: Any) -> None:
        self._send({"jsonrpc": "2.0", "method": method, "params": params})

    def _read_loop(self) -> None:
        assert self.process is not None and self.process.stdout is not None
        stdout = self.process.stdout
        try:
            while True:
                headers: Dict[str, str] = {}
                while True:
                    line = stdout.readline()
                    if not line:
                        self._fail_all_pending("server exited")
                        return
                    if line in (b"\r\n", b"\n"):
                        break
                    key, sep, value = line.decode("ascii", "replace").partition(":")
                    if sep:
                        headers[key.strip().lower()] = value.strip()
                try:
                    length = int(headers["content-length"])
                except (KeyError, ValueError):
                    self._fail_all_pending("malformed LSP frame")
                    return
                body = stdout.read(length)
                if body is None or len(body) < length:
                    self._fail_all_pending("server exited mid-frame")
                    return
                try:
                    message = json.loads(body)
                except ValueError:
                    continue
                self._dispatch(message)
        except (OSError, ValueError):
            self._fail_all_pending("server stream failed")

    def _dispatch(self, message: dict) -> None:
        if "method" not in message and "id" in message:
            with self._state_lock:
                entry = self._pending.get(message["id"])
                if entry is not None:
                    self._pending.pop(message["id"], None)
            if entry is not None:
                entry["result"] = message.get("result")
                entry["error"] = message.get("error")
                entry["event"].set()
            return
        method = message.get("method")
        if method == "textDocument/publishDiagnostics":
            params = message.get("params") or {}
            path = uri_to_path(str(params.get("uri", "")))
            self.diagnostics[path] = list(params.get("diagnostics") or [])

    def _fail_all_pending(self, reason: str) -> None:
        with self._state_lock:
            pending = list(self._pending.values())
            self._pending.clear()
        for entry in pending:
            entry["error"] = {"message": reason}
            entry["event"].set()

    # -- documents ------------------------------------------------------------

    def ensure_document_synced(self, path: Path) -> None:
        """didOpen on first sight, didChange (full text) whenever the file changed."""
        try:
            raw = path.read_bytes()
        except OSError as error:
            raise ServerUnavailable(f"cannot read {path}: {error}") from error
        if len(raw) > MAX_SYNC_BYTES:
            raise ServerUnavailable(f"{path} exceeds the {MAX_SYNC_BYTES // (1024 * 1024)}MB sync cap")
        text = raw.decode("utf-8", errors="replace")
        uri = path_to_uri(path)
        document = self.documents.get(uri)
        if document is None:
            self._notify(
                "textDocument/didOpen",
                {
                    "textDocument": {
                        "uri": uri,
                        "languageId": path.suffix.lstrip("."),
                        "version": 1,
                        "text": text,
                    }
                },
            )
            self.documents[uri] = {"version": 1, "text": text}
        elif document["text"] != text:
            version = int(document["version"]) + 1
            self._notify(
                "textDocument/didChange",
                {
                    "textDocument": {"uri": uri, "version": version},
                    "contentChanges": [{"text": text}],
                },
            )
            self.documents[uri] = {"version": version, "text": text}


# --- Manager ---------------------------------------------------------------


class LspManager:
    def __init__(self) -> None:
        self.clients: Dict[str, LspClient] = {}
        self.workspace: Optional[str] = None
        self._lock = threading.Lock()

    def for_path(self, path: Path) -> Optional[LspClient]:
        command = DEFAULT_SERVERS.get(path.suffix)
        if command is None:
            return None
        with self._lock:
            client = self.clients.get(path.suffix)
            if client is None:
                client = LspClient(command)
                self.clients[path.suffix] = client
            return client

    def shutdown_all(self) -> None:
        with self._lock:
            clients = list(self.clients.values())
        for client in clients:
            client.shutdown()


manager = LspManager()
atexit.register(manager.shutdown_all)


# --- Result shaping ---------------------------------------------------------


def _location_to_line(location: Any) -> Optional[str]:
    if not isinstance(location, dict):
        return None
    uri = location.get("uri") or (location.get("targetUri") if isinstance(location.get("targetUri"), str) else None)
    if not isinstance(uri, str):
        return None
    range_ = location.get("range") or location.get("targetSelectionRange") or {}
    start = range_.get("start") or {}
    line = int(start.get("line", 0)) + 1
    character = int(start.get("character", 0))
    return f"{uri_to_path(uri)}:{line}:{character}"


def _format_hover(hover: Any) -> str:
    if not hover:
        return "No hover information."
    contents = hover.get("contents")
    parts: List[str] = []

    def render(value: Any) -> None:
        if isinstance(value, str):
            parts.append(value)
        elif isinstance(value, dict):
            value = value.get("value")
            if isinstance(value, str):
                parts.append(value)
        elif isinstance(value, list):
            for item in value:
                render(item)

    render(contents)
    text = "\n".join(part for part in parts if part).strip()
    if len(text) > MAX_HOVER_CHARS:
        text = text[:MAX_HOVER_CHARS] + "… (truncated)"
    return text or "No hover information."


def _format_diagnostics(diagnostics: List[dict], path: Path) -> List[str]:
    lines = []
    for diagnostic in diagnostics[:MAX_DIAGNOSTICS_PER_FILE]:
        start = (diagnostic.get("range") or {}).get("start") or {}
        line = int(start.get("line", 0)) + 1
        severity = SEVERITY_NAMES.get(diagnostic.get("severity"), "diagnostic")
        source = diagnostic.get("source")
        prefix = f"{source}: " if isinstance(source, str) and source else ""
        message = str(diagnostic.get("message", "")).replace("\n", " ")
        lines.append(f"{path.name}:{line}: {severity}: {prefix}{message}")
    if len(diagnostics) > MAX_DIAGNOSTICS_PER_FILE:
        lines.append(
            f"... {len(diagnostics) - MAX_DIAGNOSTICS_PER_FILE} more diagnostics omitted"
        )
    return lines


# --- Ygg tool ----------------------------------------------------------------

ext = Extension()

_TOOL_DESCRIPTION = (
    "Query language-server code intelligence for a file. Operations: "
    "definition (where is the symbol at position defined), references (who "
    "uses it), hover (type/signature/docs), diagnostics (current errors and "
    "warnings for the file). Positions are 1-based line, 0-based character. "
    "Query before editing when uncertain; wait for the result before emitting "
    "a dependent edit."
)


@ext.tool(
    name="code_intelligence",
    description=_TOOL_DESCRIPTION,
    parameters={
        "type": "object",
        "properties": {
            "operation": {
                "type": "string",
                "enum": ["definition", "references", "hover", "diagnostics"],
            },
            "file": {"type": "string", "description": "Path to the file"},
            "line": {"type": "integer", "description": "1-based line number"},
            "character": {"type": "integer", "description": "0-based character offset"},
        },
        "required": ["operation", "file"],
        "additionalProperties": False,
    },
)
def code_intelligence(arguments: dict, context: dict) -> dict:
    args = arguments or {}
    operation = args.get("operation")
    file_arg = args.get("file")
    if not isinstance(file_arg, str) or not file_arg.strip():
        return {"content": "code_intelligence failed: file must be a non-empty path", "is_error": True}
    path = Path(file_arg).expanduser()
    if not path.is_file():
        return {"content": f"code_intelligence failed: file does not exist: {path}", "is_error": True}

    client = manager.for_path(path)
    if client is None:
        return {
            "content": (
                f"No language server is configured for {path.suffix} files. "
                "Use read and search instead."
            ),
            "metadata": {"status": "unconfigured", "suffix": path.suffix},
        }

    if context and isinstance(context.get("workspace"), str):
        manager.workspace = context["workspace"]

    needs_position = operation in ("definition", "references", "hover")
    if needs_position and ("line" not in args or "character" not in args):
        return {
            "content": f"code_intelligence failed: {operation} requires line and character",
            "is_error": True,
        }

    if not client.ensure_started(manager.workspace):
        return {
            "content": (
                f"The {path.suffix} language server could not be started "
                f"({' '.join(client.command)}). Use read and search instead."
            ),
            "metadata": {"status": "unavailable", "command": client.command},
        }

    try:
        client.ensure_document_synced(path)
        if needs_position:
            params = {
                "textDocument": {"uri": path_to_uri(path)},
                "position": {
                    "line": max(int(args["line"]) - 1, 0),
                    "character": max(int(args["character"]), 0),
                },
            }
            if operation == "definition":
                locations = None
                # Bounded warm-up retry: an indexing server returns empty
                # results, which must not be reported as "not found".
                for _ in range(DEFINITION_WARMUP_RETRIES + 1):
                    locations = client._request(
                        "textDocument/definition", params, REQUEST_TIMEOUT_S
                    )
                    if locations:
                        break
                    time.sleep(DEFINITION_WARMUP_DELAY_S)
                if locations is None:
                    locations = []
                if isinstance(locations, dict):
                    locations = [locations]
                formatted = [line for line in map(_location_to_line, locations) if line][
                    :MAX_DEFINITIONS
                ]
                body = "\n".join(formatted) if formatted else "No definition found."
                return {"content": body, "metadata": {"count": len(formatted)}}
            if operation == "references":
                params["context"] = {"includeDeclaration": True}
                locations = client._request("textDocument/references", params, REQUEST_TIMEOUT_S) or []
                formatted = [line for line in map(_location_to_line, locations) if line]
                omitted = max(len(formatted) - MAX_REFERENCES, 0)
                formatted = formatted[:MAX_REFERENCES]
                lines = formatted + (
                    [f"... {omitted} additional references omitted"] if omitted else []
                )
                body = "\n".join(lines) if lines else "No references found."
                return {"content": body, "metadata": {"count": len(formatted), "omitted": omitted}}
            hover = None
            # Same bounded warm-up as definition.
            for _ in range(DEFINITION_WARMUP_RETRIES + 1):
                hover = client._request("textDocument/hover", params, REQUEST_TIMEOUT_S)
                if hover:
                    break
                time.sleep(DEFINITION_WARMUP_DELAY_S)
            return {"content": _format_hover(hover)}
        diagnostics = client.diagnostics.get(str(path.resolve()), [])
        lines = _format_diagnostics(list(diagnostics), path)
        body = "\n".join(lines) if lines else "No diagnostics for this file."
        return {"content": body, "metadata": {"count": len(diagnostics)}}
    except ServerUnavailable as error:
        return {
            "content": f"code_intelligence unavailable: {error}. Use read and search instead.",
            "metadata": {"status": "unavailable"},
        }
    except (OSError, ValueError) as error:
        return {"content": f"code_intelligence failed: {error}", "is_error": True}


# --- before_prompt hook ------------------------------------------------------

# Injected-once tracking: signature -> file content marker at injection time.
_injected: Dict[str, dict] = {}
_injected_lock = threading.Lock()


def _diagnostic_signature(path: str, diagnostic: dict) -> str:
    start = (diagnostic.get("range") or {}).get("start") or {}
    return "|".join(
        str(part)
        for part in (
            path,
            start.get("line"),
            start.get("character"),
            diagnostic.get("severity"),
            diagnostic.get("message"),
        )
    )


@ext.hook("before_prompt")
def before_prompt(payload: dict, context: dict) -> dict:
    """Inject only diagnostics that are new since the last injection."""
    contributions: List[dict] = []
    lines: List[str] = []
    with _injected_lock:
        for client in list(manager.clients.values()):
            for path, diagnostics in list(client.diagnostics.items()):
                marker = {"version": len(diagnostics)}
                known = _injected.get(path)
                fresh = [
                    diagnostic
                    for diagnostic in diagnostics
                    if known is None or _diagnostic_signature(path, diagnostic) not in known
                ]
                if not diagnostics:
                    # Fixed: forget delivered signatures so a regression re-reports.
                    _injected.pop(path, None)
                    continue
                if not fresh:
                    continue
                for diagnostic in fresh:
                    if len(lines) >= MAX_DIAGNOSTIC_INJECTIONS:
                        break
                    formatted = _format_diagnostics([diagnostic], Path(path))
                    if formatted:
                        lines.append(formatted[0])
                _injected[path] = known or {}
                for diagnostic in fresh:
                    _injected[path][_diagnostic_signature(path, diagnostic)] = marker
        if lines:
            contributions.append(
                {
                    "label": "lsp-client",
                    "content": (
                        "New language-server diagnostics in edited files:\n"
                        + "\n".join(lines)
                    ),
                    "placement": "system_suffix",
                }
            )
    return {"disposition": {"action": "continue"}, "context": contributions, "notifications": []}


if __name__ == "__main__":
    ext.run()
