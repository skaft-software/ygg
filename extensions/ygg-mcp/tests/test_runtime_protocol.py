from __future__ import annotations

import json
from pathlib import Path
import queue
import subprocess
import threading
import time
import unittest

from .helpers import FIXTURES, ROOT


class RuntimeProtocolTests(unittest.TestCase):
    def setUp(self):
        self.process = subprocess.Popen(
            [
                str(ROOT / "ygg-mcp"),
                "--config",
                str(FIXTURES / "configs" / "real-local.json"),
            ],
            cwd=ROOT,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
        )
        self.messages = queue.Queue()
        self.stderr = []
        self.stdout_thread = threading.Thread(target=self._read_stdout, daemon=True)
        self.stderr_thread = threading.Thread(target=self._read_stderr, daemon=True)
        self.stdout_thread.start()
        self.stderr_thread.start()

    def tearDown(self):
        if self.process.poll() is None:
            self.process.kill()
        self.process.wait(timeout=3)
        for stream in (self.process.stdin, self.process.stdout, self.process.stderr):
            if stream is not None:
                stream.close()
        self.stdout_thread.join(timeout=1)
        self.stderr_thread.join(timeout=1)

    def _read_stdout(self):
        assert self.process.stdout is not None
        for line in self.process.stdout:
            try:
                self.messages.put(json.loads(line))
            except json.JSONDecodeError as error:
                self.messages.put(error)

    def _read_stderr(self):
        assert self.process.stderr is not None
        for line in self.process.stderr:
            if len(self.stderr) < 128:
                self.stderr.append(line)

    def send(self, value):
        assert self.process.stdin is not None
        self.process.stdin.write(json.dumps(value, separators=(",", ":")) + "\n")
        self.process.stdin.flush()

    def receive(self, timeout=4):
        value = self.messages.get(timeout=timeout)
        if isinstance(value, Exception):
            raise value
        return value

    def test_api_02_dynamic_catalog_tool_call_presentation_and_shutdown(self):
        with self.assertRaises(queue.Empty):
            self.messages.get(timeout=0.15)

        self.send(
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "api_version": "0.2",
                    "ygg_version": "0.7.0-dev",
                    "extension": {
                        "name": "ygg-mcp",
                        "version": "0.1.0",
                        "manifest_path": str(ROOT / "extension.toml"),
                        "source": "explicit",
                    },
                    "workspace": str(ROOT),
                    "capabilities": {
                        "filesystem": "unrestricted",
                        "process": True,
                        "network": False,
                    },
                    "contributes": {
                        "tools": [],
                        "commands": ["mcp"],
                        "ui": ["status"],
                        "presentation": True,
                    },
                    "host": {
                        "session_id": "fixture-session",
                        "session_name": None,
                        "model": "fixture-model",
                        "reasoning": None,
                        "active_skills": [],
                    },
                    "protocol": {
                        "version": "0.2",
                        "required_features": ["request_cancellation", "content_parts"],
                        "optional_features": [
                            "request_progress",
                            "artifacts",
                            "policy_intents",
                            "dynamic_tools",
                        ],
                        "limits": {"max_concurrent_requests": 4},
                    },
                },
            }
        )
        initialized = self.receive()
        self.assertEqual(initialized["id"], 1)
        self.assertEqual(initialized["result"]["tools"], [])
        self.assertIn("dynamic_tools", initialized["result"]["protocol"]["features"])

        registered = None
        presentations = []
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline and registered is None:
            message = self.receive()
            if message.get("method") == "presentation/update":
                presentations.append(message["params"]["snapshot"])
            elif message.get("method") == "tools/register":
                registered = message
        self.assertIsNotNone(registered)
        names = sorted(tool["name"] for tool in registered["params"]["tools"])
        self.assertEqual(len(names), 3)
        self.send(
            {
                "jsonrpc": "2.0",
                "id": registered["id"],
                "result": {"revision": 1, "tools": names},
            }
        )

        echo_name = next(name for name in names if "fixture_echo" in name)
        self.send(
            {
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tool/call",
                "params": {
                    "name": echo_name,
                    "arguments": {"value": "wire"},
                    "catalog_revision": 1,
                    "context": {
                        "workspace": str(ROOT),
                        "resource_owner": {
                            "session_id": "fixture-owner",
                            "extension_instance_id": "fixture-instance",
                            "process_generation": 1,
                        },
                        "host": {},
                    },
                },
            }
        )
        tool_response = None
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline and tool_response is None:
            message = self.receive()
            if message.get("method") == "presentation/update":
                presentations.append(message["params"]["snapshot"])
            elif message.get("id") == 2:
                tool_response = message
        self.assertIsNotNone(tool_response)
        self.assertEqual(tool_response["result"]["structured_content"], {"echo": "wire"})
        self.assertEqual(tool_response["result"]["content"][0]["type"], "text")
        self.assertTrue(presentations)
        self.assertTrue(all(set(item) <= {"revision", "status", "activities", "collection", "actions"} for item in presentations))
        self.assertTrue(any(item.get("collection", {}).get("nodes") for item in presentations))

        self.send({"jsonrpc": "2.0", "id": 3, "method": "shutdown", "params": {}})
        shutdown = None
        deadline = time.monotonic() + 4
        while time.monotonic() < deadline and shutdown is None:
            message = self.receive()
            if message.get("id") == 3:
                shutdown = message
        self.assertEqual(shutdown["result"], {})
        self.process.wait(timeout=4)
        self.assertEqual(self.process.returncode, 0, "".join(self.stderr))


if __name__ == "__main__":
    unittest.main()
