from __future__ import annotations

import json
import tempfile
import time
import unittest
from pathlib import Path
from typing import Any, Mapping, Optional

from ygg_browse.artifacts import PNG_SIGNATURE, ScreenshotRecord
from ygg_browse.runtime import create_runtime
from ygg_browse.safety import BrowseError

from tests.helpers import MemoryProtocol, OWNER_CONTEXT


TOOLS = [
    "browser_status",
    "browser_launch",
    "browser_tabs",
    "browser_open_url",
    "browser_snapshot",
    "browser_click",
    "browser_type",
    "browser_press",
    "browser_scroll",
    "browser_wait",
    "browser_screenshot",
    "browser_tab_close",
    "browser_close",
]


class FakePaths:
    def __init__(self, root: Path) -> None:
        self.root = root

    def display(self, path: Path) -> str:
        return str(path)


class FakeArtifacts:
    def __init__(self, data: bytes) -> None:
        self.data = data

    def publish(self, extension: Any, record: ScreenshotRecord) -> str:
        return extension.publish_artifact(mime_type="image/png", data=self.data)


class FakeController:
    def __init__(self, _presentation: Any, root: Path) -> None:
        self.root = root
        self.paths = FakePaths(root)
        self.data = PNG_SIGNATURE + b"fixture"
        self.artifacts = FakeArtifacts(self.data)
        self.typed_values = []
        self.closed = False
        self.confirmation_parent_seen = False

    @staticmethod
    def result(text: str = "ok") -> dict[str, Any]:
        return {
            "text": text,
            "open": True,
            "tabs": [],
            "tab_count": 0,
            "selected_tab_id": None,
        }

    def browser_status(self, *_: Any, **__: Any) -> dict[str, Any]:
        return self.result("ready")

    browser_launch = browser_status
    browser_tabs = browser_status

    def browser_open_url(self, _owner: Any, _url: str, tab_id: Optional[str], **_: Any) -> dict[str, Any]:
        return {**self.result("navigated"), "affected_tab_id": tab_id or "tab_new"}

    def browser_snapshot(self, _owner: Any, tab_id: str, **_: Any) -> dict[str, Any]:
        return {**self.result("BEGIN UNTRUSTED BROWSER CONTENT\nfixture\nEND UNTRUSTED BROWSER CONTENT"), "affected_tab_id": tab_id, "snapshot_generation": 2}

    def browser_click(
        self,
        _owner: Any,
        tab_id: str,
        _target: str,
        _generation: Any,
        confirmation: Any,
        **_: Any,
    ) -> dict[str, Any]:
        if not confirmation("send or publish content", None, True):
            raise BrowseError("confirmation_denied", "Consequential action denied.")
        return {**self.result("clicked"), "affected_tab_id": tab_id}

    def browser_type(
        self,
        _owner: Any,
        tab_id: str,
        _target: str,
        _generation: Any,
        value: str,
        **_: Any,
    ) -> dict[str, Any]:
        self.typed_values.append(value)
        return {**self.result("Typed value withheld."), "affected_tab_id": tab_id, "value_echoed": False}

    def browser_press(
        self,
        _owner: Any,
        tab_id: str,
        _target: str,
        _generation: Any,
        _key: str,
        _confirmation: Any,
        **_: Any,
    ) -> dict[str, Any]:
        return {**self.result("pressed"), "affected_tab_id": tab_id}

    def browser_scroll(self, _owner: Any, tab_id: str, _dx: int, _dy: int, **_: Any) -> dict[str, Any]:
        return {**self.result("scrolled"), "affected_tab_id": tab_id}

    def browser_wait(self, _owner: Any, tab_id: str, _milliseconds: int, **_: Any) -> dict[str, Any]:
        return {**self.result("waited"), "affected_tab_id": tab_id}

    def browser_tab_close(self, _owner: Any, tab_id: str, **_: Any) -> dict[str, Any]:
        return {**self.result("closed tab"), "affected_tab_id": tab_id, "closed_tab_ids": [tab_id]}

    def browser_close(self, *_: Any, **__: Any) -> dict[str, Any]:
        return {**self.result("closed"), "open": False}

    def browser_screenshot(self, _owner: Any, _tab_id: str, **_: Any) -> ScreenshotRecord:
        path = self.root / "screenshot.png"
        path.write_bytes(self.data)
        import hashlib

        return ScreenshotRecord(path, len(self.data), hashlib.sha256(self.data).hexdigest())

    def screenshot_published(self, _owner: Any, _artifact_id: str, _record: ScreenshotRecord) -> None:
        pass

    def command(self, arguments: Any, _context: Mapping[str, Any], confirmation: Any, **_: Any) -> str:
        if arguments == ["setup"]:
            return "setup started" if confirmation("Install?", "Pinned path", False) else "setup denied"
        return "status"

    def shutdown(self) -> None:
        self.closed = True


class ProtocolTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        root = Path(self.temporary.name)
        holder = {}

        def factory(presentation: Any) -> FakeController:
            controller = FakeController(presentation, root)
            holder["controller"] = controller
            return controller

        self.extension, _, _, _ = create_runtime(controller_factory=factory)
        self.controller: FakeController = holder["controller"]
        self.protocol = MemoryProtocol(self.extension)
        self.protocol.reader.send(
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "api_version": "0.2",
                    "ygg_version": "0.6.3",
                    "extension": {
                        "name": "ygg-browse",
                        "version": "0.1.0",
                        "manifest_path": "/tmp/ygg-browse/extension.toml",
                        "source": "global",
                    },
                    "workspace": "/tmp/workspace",
                    "capabilities": {
                        "filesystem": "unrestricted",
                        "process": True,
                        "network": True,
                    },
                    "contributes": {
                        "tools": TOOLS,
                        "commands": ["browse"],
                        "ui": ["status"],
                        "confirmations": True,
                        "presentation": True,
                    },
                    "host": {},
                    "protocol": {
                        "version": "0.2",
                        "required_features": ["request_cancellation", "content_parts"],
                        "optional_features": ["artifacts", "request_progress", "policy_intents"],
                        "limits": {"max_concurrent_requests": 8},
                    },
                },
            }
        )
        response = self.protocol.writer.receive()
        self.assertEqual(response["id"], 1)
        self.initialize = response["result"]

    def tearDown(self) -> None:
        if self.extension.running:
            self.protocol.reader.send({"jsonrpc": "2.0", "id": 99, "method": "shutdown", "params": {}})
            deadline = time.monotonic() + 3
            while self.extension.running and time.monotonic() < deadline:
                try:
                    self.protocol.writer.receive(timeout=0.05)
                except Exception:
                    pass
        self.protocol.reader.close()
        self.protocol.thread.join(timeout=3)
        self.temporary.cleanup()

    def test_initialize_has_exact_surface_and_bounded_schemas(self) -> None:
        self.assertEqual(self.initialize["api_version"], "0.2")
        self.assertEqual([tool["name"] for tool in self.initialize["tools"]], TOOLS)
        self.assertEqual([command["name"] for command in self.initialize["commands"]], ["browse"])
        self.assertEqual(
            set(self.initialize["protocol"]["features"]),
            {"request_cancellation", "content_parts", "artifacts"},
        )
        schemas = {tool["name"]: tool["parameters"] for tool in self.initialize["tools"]}
        self.assertFalse(schemas["browser_click"]["additionalProperties"])
        self.assertEqual(schemas["browser_wait"]["properties"]["milliseconds"]["maximum"], 5000)
        self.assertNotIn("browser_evaluate", schemas)
        self.assertNotIn("browser_download", schemas)

    def test_every_noninteractive_declared_tool_dispatches_with_exact_arguments(self) -> None:
        cases = [
            ("browser_status", {}),
            ("browser_launch", {}),
            ("browser_tabs", {}),
            ("browser_open_url", {"url": "https://example.test/"}),
            ("browser_snapshot", {"tab_id": "tab_1"}),
            ("browser_press", {"tab_id": "tab_1", "target": "text=Field", "key": "Tab"}),
            ("browser_scroll", {"tab_id": "tab_1", "delta_x": 0, "delta_y": 100}),
            ("browser_wait", {"tab_id": "tab_1", "milliseconds": 10}),
            ("browser_tab_close", {"tab_id": "tab_1"}),
            ("browser_close", {}),
        ]
        for offset, (name, arguments) in enumerate(cases, start=10):
            with self.subTest(name=name):
                response = self.protocol.request(
                    offset,
                    "tool/call",
                    {"name": name, "arguments": arguments, "context": OWNER_CONTEXT},
                )
                self.assertIn("result", response)
                self.assertFalse(response["result"]["is_error"], response)

    def test_typed_value_never_appears_on_protocol_output(self) -> None:
        secret_value = "typed-value-must-not-echo"
        response = self.protocol.request(
            2,
            "tool/call",
            {
                "name": "browser_type",
                "arguments": {"tab_id": "tab_1", "target": "text=Search", "text": secret_value},
                "context": OWNER_CONTEXT,
            },
        )
        self.assertIn("result", response)
        encoded = json.dumps(response)
        self.assertNotIn(secret_value, encoded)
        self.assertEqual(self.controller.typed_values, [secret_value])
        self.assertFalse(response["result"]["metadata"]["value_echoed"])

    def test_consequential_action_uses_parent_correlated_confirmation_and_denies(self) -> None:
        self.protocol.reader.send(
            {
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tool/call",
                "params": {
                    "name": "browser_click",
                    "arguments": {"tab_id": "tab_1", "target": "text=Publish"},
                    "context": OWNER_CONTEXT,
                },
            }
        )
        child = self.protocol.writer.receive()
        self.assertEqual(child["method"], "confirmation/request")
        self.assertEqual(child["params"]["parent_request_id"], 3)
        self.assertFalse(child["params"]["default"])
        self.assertNotIn("Publish", child["params"]["prompt"])
        self.protocol.reader.send(
            {"jsonrpc": "2.0", "id": child["id"], "result": {"confirmed": False}}
        )
        response = self.protocol.writer.receive()
        self.assertEqual(response["id"], 3)
        self.assertTrue(response["result"]["is_error"])
        self.assertIn("confirmation_denied", response["result"]["metadata"]["code"])

    def test_screenshot_publishes_owner_scoped_artifact_and_image_part(self) -> None:
        self.protocol.reader.send(
            {
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tool/call",
                "params": {
                    "name": "browser_screenshot",
                    "arguments": {"tab_id": "tab_1"},
                    "context": OWNER_CONTEXT,
                },
            }
        )
        child = self.protocol.writer.receive()
        self.assertEqual(child["method"], "artifact/publish")
        self.assertEqual(child["params"]["parent_request_id"], 4)
        self.assertEqual(child["params"]["mime_type"], "image/png")
        self.assertLess(child["params"]["size"], 5 * 1024 * 1024)
        self.protocol.reader.send(
            {"jsonrpc": "2.0", "id": child["id"], "result": {"artifact_id": "artifact-4"}}
        )
        response = self.protocol.writer.receive()
        self.assertEqual(response["id"], 4)
        parts = response["result"]["content"]
        self.assertEqual(parts[1]["type"], "image")
        self.assertEqual(parts[1]["artifact_id"], "artifact-4")
        self.assertIn("Read-compatible local reference", parts[0]["text"])

    def test_setup_command_confirmation_is_explicit_and_returns_promptly(self) -> None:
        self.protocol.reader.send(
            {
                "jsonrpc": "2.0",
                "id": 5,
                "method": "command/execute",
                "params": {"name": "browse", "arguments": ["setup"], "context": OWNER_CONTEXT},
            }
        )
        child = self.protocol.writer.receive()
        self.assertEqual(child["method"], "confirmation/request")
        self.assertEqual(child["params"]["parent_request_id"], 5)
        self.protocol.reader.send(
            {"jsonrpc": "2.0", "id": child["id"], "result": {"confirmed": True}}
        )
        response = self.protocol.writer.receive()
        self.assertEqual(response["id"], 5)
        self.assertEqual(response["result"]["text"], "setup started")


if __name__ == "__main__":
    unittest.main()
