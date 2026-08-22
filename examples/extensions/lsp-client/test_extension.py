#!/usr/bin/env python3
"""Protocol, lifecycle, and diagnostics tests for the lsp-client example."""

import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import textwrap
import unittest
from unittest import mock


EXTENSION_PATH = Path(__file__).with_name("extension.py")
SDK_PATH = EXTENSION_PATH.parents[3] / "sdk" / "python"
sys.path.insert(0, str(SDK_PATH))

spec = importlib.util.spec_from_file_location("lsp_client_extension", EXTENSION_PATH)
extension = importlib.util.module_from_spec(spec)
spec.loader.exec_module(extension)

# A compliant LSP server used to exercise the real wire protocol. It answers
# initialize/definition/references/hover, pushes publishDiagnostics after
# didOpen/didChange, and logs every notification for assertions.
FAKE_SERVER = textwrap.dedent(
    """
    import json
    import sys

    log_path = sys.argv[1]
    log = open(log_path, "a", encoding="utf-8")

    def send(message):
        body = json.dumps(message).encode("utf-8")
        sys.stdout.buffer.write(
            b"Content-Length: " + str(len(body)).encode() + b"\\r\\n\\r\\n" + body
        )
        sys.stdout.buffer.flush()

    def read_message():
        headers = {}
        while True:
            line = sys.stdin.buffer.readline()
            if not line or line in (b"\\r\\n", b"\\n"):
                if not line:
                    return None
                break
            key, _, value = line.decode("ascii").partition(":")
            headers[key.strip().lower()] = value.strip()
        body = sys.stdin.buffer.read(int(headers["content-length"]))
        return json.loads(body)

    def location(uri):
        return {"uri": uri, "range": {"start": {"line": 4, "character": 7}}}

    while True:
        message = read_message()
        if message is None:
            break
        method = message.get("method")
        params = message.get("params") or {}
        if method == "initialize":
            send({"jsonrpc": "2.0", "id": message["id"], "result": {"capabilities": {}}})
        elif method == "textDocument/didOpen":
            log.write(json.dumps({"method": method, "version": 1}) + "\\n")
            log.flush()
            uri = params["textDocument"]["uri"]
            send({
                "jsonrpc": "2.0",
                "method": "textDocument/publishDiagnostics",
                "params": {"uri": uri, "diagnostics": [
                    {"range": {"start": {"line": 2, "character": 0}},
                     "severity": 1, "message": "fake error"},
                ]},
            })
        elif method == "textDocument/didChange":
            version = params["textDocument"]["version"]
            log.write(json.dumps({"method": method, "version": version,
                                  "text": params["contentChanges"][0]["text"]}) + "\\n")
            log.flush()
            uri = params["textDocument"]["uri"]
            count = int(params["textDocument"].get("version", 1))
            send({
                "jsonrpc": "2.0",
                "method": "textDocument/publishDiagnostics",
                "params": {"uri": uri, "diagnostics": [
                    {"range": {"start": {"line": 2, "character": 0}},
                     "severity": 1, "message": f"fake error v{count}"},
                ]},
            })
        elif method == "textDocument/definition":
            send({"jsonrpc": "2.0", "id": message["id"], "result":
                  location(params["textDocument"]["uri"])})
        elif method == "textDocument/references":
            send({"jsonrpc": "2.0", "id": message["id"], "result":
                  [location("file:///a.rs"), location("file:///b.rs")]})
        elif method == "textDocument/hover":
            send({"jsonrpc": "2.0", "id": message["id"], "result":
                  {"contents": {"kind": "markdown", "value": "**fn sample**"}}})
        else:
            if "id" in message:
                send({"jsonrpc": "2.0", "id": message["id"], "result": None})
    """
)


class LspClientTests(unittest.TestCase):
    def setUp(self):
        self.workspace = Path(tempfile.mkdtemp())
        self.log_path = self.workspace / "server_log.jsonl"
        self.server_script = self.workspace / "fake_lsp_server.py"
        self.server_script.write_text(FAKE_SERVER)
        extension.manager.clients.clear()
        extension.manager.workspace = str(self.workspace)
        extension._injected.clear()
        self.sample = self.workspace / "sample.rs"
        self.sample.write_text("fn main() {}\n")

    def server_command(self):
        return [sys.executable, str(self.server_script), str(self.log_path)]

    def client_for_sample(self):
        client = extension.manager.for_path(self.sample)
        assert client is not None
        return client

    def tool_call(self, **arguments):
        arguments.setdefault("file", str(self.sample))
        return extension.code_intelligence(arguments, {"workspace": str(self.workspace)})

    def read_log(self):
        if not self.log_path.exists():
            return []
        return [
            json.loads(line)
            for line in self.log_path.read_text().splitlines()
            if line.strip()
        ]

    # -- tool surface -------------------------------------------------------

    def test_definition_returns_bounded_location(self):
        with mock.patch.object(extension, "DEFAULT_SERVERS", {".rs": self.server_command()}):
            result = self.tool_call(operation="definition", line=1, character=4)
        self.assertNotIn("is_error", result)
        self.assertIn(f"{self.sample.resolve()}:5:7", result["content"])
        self.assertEqual(result["metadata"], {"count": 1})

    def test_references_lists_locations(self):
        with mock.patch.object(extension, "DEFAULT_SERVERS", {".rs": self.server_command()}):
            result = self.tool_call(operation="references", line=1, character=4)
        self.assertIn("a.rs:5:7", result["content"])
        self.assertIn("b.rs:5:7", result["content"])
        self.assertEqual(result["metadata"]["count"], 2)

    def test_hover_formats_markdown_content(self):
        with mock.patch.object(extension, "DEFAULT_SERVERS", {".rs": self.server_command()}):
            result = self.tool_call(operation="hover", line=1, character=4)
        self.assertEqual(result["content"], "**fn sample**")

    def test_position_required_for_navigation(self):
        result = self.tool_call(operation="definition")
        self.assertTrue(result.get("is_error"))

    def test_unconfigured_suffix_is_typed_not_an_error(self):
        plain = self.workspace / "notes.txt"
        plain.write_text("hello\n")
        result = extension.code_intelligence(
            {"operation": "definition", "file": str(plain), "line": 1, "character": 0},
            {"workspace": str(self.workspace)},
        )
        self.assertNotIn("is_error", result)
        self.assertEqual(result["metadata"]["status"], "unconfigured")

    def test_missing_file_is_error(self):
        result = self.tool_call(operation="definition", file="/nonexistent/x.rs")
        self.assertTrue(result.get("is_error"))

    # -- document synchronization --------------------------------------------

    def test_edited_file_is_resynced_with_new_version(self):
        with mock.patch.object(extension, "DEFAULT_SERVERS", {".rs": self.server_command()}):
            self.tool_call(operation="definition", line=1, character=4)
            self.sample.write_text("fn edited_main() {}\n")
            self.tool_call(operation="hover", line=1, character=4)

        versions = [entry["version"] for entry in self.read_log()]
        self.assertEqual(versions, [1, 2])

    # -- diagnostics pipeline --------------------------------------------------

    def test_diagnostics_operation_reports_pushed_diagnostics(self):
        with mock.patch.object(extension, "DEFAULT_SERVERS", {".rs": self.server_command()}):
            self.tool_call(operation="definition", line=1, character=4)
            result = self.tool_call(operation="diagnostics")
        self.assertIn("sample.rs:3: error: fake error", result["content"])
        self.assertEqual(result["metadata"]["count"], 1)

    def test_hook_injects_each_diagnostic_exactly_once(self):
        with mock.patch.object(extension, "DEFAULT_SERVERS", {".rs": self.server_command()}):
            self.tool_call(operation="definition", line=1, character=4)

            first = extension.before_prompt({}, {})
            self.assertEqual(len(first["context"]), 1)
            self.assertIn("fake error", first["context"][0]["content"])
            self.assertEqual(first["context"][0]["placement"], "system_suffix")

            second = extension.before_prompt({}, {})
            self.assertEqual(second["context"], [])

            # An updated diagnostic push must be injected again.
            client = self.client_for_sample()
            client.diagnostics[str(self.sample)] = [
                {
                    "range": {"start": {"line": 6, "character": 0}},
                    "severity": 2,
                    "message": "new warning",
                }
            ]
            third = extension.before_prompt({}, {})
            self.assertEqual(len(third["context"]), 1)
            self.assertIn("new warning", third["context"][0]["content"])

    def test_hook_forgets_fixed_files_so_regressions_reappear(self):
        with mock.patch.object(extension, "DEFAULT_SERVERS", {".rs": self.server_command()}):
            self.tool_call(operation="definition", line=1, character=4)
            first = extension.before_prompt({}, {})
            self.assertEqual(len(first["context"]), 1)

            client = self.client_for_sample()
            path_key = client.diagnostics and next(iter(client.diagnostics))
            client.diagnostics[path_key] = []
            extension.before_prompt({}, {})  # observes the fixed file

            client.diagnostics[path_key] = [
                {
                    "range": {"start": {"line": 9, "character": 0}},
                    "severity": 1,
                    "message": "regression",
                }
            ]
            third = extension.before_prompt({}, {})
            self.assertIn("regression", third["context"][0]["content"])

    # -- failure paths -----------------------------------------------------------

    def test_dead_server_fails_fast_and_stays_bounded(self):
        dead = [sys.executable, "-c", "import sys; sys.exit(1)"]
        with mock.patch.object(extension, "DEFAULT_SERVERS", {".rs": dead}):
            for _ in range(extension.MAX_CONSECUTIVE_START_FAILURES + 1):
                result = self.tool_call(operation="definition", line=1, character=4)
                self.assertEqual(result["metadata"]["status"], "unavailable")

    def test_silent_server_times_out_with_typed_result(self):
        silent = [sys.executable, "-c", "import time; time.sleep(30)"]
        with mock.patch.object(extension, "DEFAULT_SERVERS", {".rs": silent}):
            with mock.patch.object(extension, "INIT_TIMEOUT_S", 2.0), mock.patch.object(
                extension, "REQUEST_TIMEOUT_S", 0.5
            ):
                result = self.tool_call(operation="definition", line=1, character=4)
        self.assertEqual(result["metadata"]["status"], "unavailable")
        self.assertIn("read and search", result["content"])

    def test_shutdown_terminates_servers(self):
        with mock.patch.object(extension, "DEFAULT_SERVERS", {".rs": self.server_command()}):
            self.tool_call(operation="definition", line=1, character=4)
            client = self.client_for_sample()
            self.assertTrue(client.is_running())
            extension.manager.shutdown_all()
            self.assertFalse(client.is_running())


if __name__ == "__main__":
    unittest.main(verbosity=2)
