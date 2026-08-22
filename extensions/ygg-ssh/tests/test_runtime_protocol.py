from __future__ import annotations

import json
import os
from pathlib import Path
import queue
import subprocess
import tempfile
import threading
import unittest

from .helpers import FAKE_SSH, ROOT, config_document, write_json


class RuntimeProtocolTests(unittest.TestCase):
    """End-to-end smoke: launch the entrypoint and drive API 0.2 over stdio."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.state = Path(self.temp.name)
        self.config = write_json(
            self.state / "ssh.json",
            config_document(),
        )
        environment = dict(os.environ)
        environment["FAKE_SSH_EXIT"] = "0"
        self.process = subprocess.Popen(
            [
                str(ROOT / "ygg-ssh"),
                "--config",
                str(self.config),
                "--ssh-binary",
                str(FAKE_SSH),
            ],
            cwd=ROOT,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
            env=environment,
        )
        self.messages: queue.Queue[object] = queue.Queue()
        self.stderr: list[str] = []
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
        self.temp.cleanup()

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

    def receive(self, timeout=5):
        value = self.messages.get(timeout=timeout)
        if isinstance(value, Exception):
            raise value
        return value

    @staticmethod
    def context():
        return {
            "workspace": "/fixture/workspace",
            "resource_owner": {
                "session_id": "fixture-session",
                "extension_instance_id": "fixture-instance",
                "process_generation": 7,
            },
            "host": {"session_id": "fixture-session"},
        }

    def initialize(self):
        self.send(
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "api_version": "0.2",
                    "ygg_version": "0.6.0-dev",
                    "extension": {
                        "name": "ygg-ssh",
                        "version": "0.2.0",
                        "manifest_path": str(ROOT / "extension.toml"),
                        "source": "explicit",
                    },
                    "workspace": "/fixture/workspace",
                    "capabilities": {
                        "filesystem": "unrestricted",
                        "process": True,
                        "network": True,
                        "environment": ["SSH_AUTH_SOCK"],
                    },
                    "contributes": {
                        "commands": ["ssh"],
                        "ui": ["status"],
                        "context": True,
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
                        "required_features": ["content_parts"],
                        "optional_features": ["lifecycle_events"],
                        "limits": {"max_concurrent_requests": 4},
                    },
                },
            }
        )
        initialized = self.receive()
        self.assertEqual(initialized["id"], 1)
        return initialized

    def wait_for_id(self, request_id):
        deadline = __import__("time").monotonic() + 6
        while __import__("time").monotonic() < deadline:
            message = self.receive(timeout=max(0.1, deadline - __import__("time").monotonic()))
            if message.get("id") == request_id:
                return message
        self.fail(f"timed out waiting for response {request_id}")

    def command(self, request_id, arguments):
        self.send(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "method": "command/execute",
                "params": {
                    "name": "ssh",
                    "arguments": arguments,
                    "context": self.context(),
                },
            }
        )
        return self.wait_for_id(request_id)

    def collect_context(self, request_id):
        self.send(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "method": "context/collect",
                "params": {},
            }
        )
        return self.wait_for_id(request_id)

    def test_portal_lifecycle_over_the_wire(self):
        initialized = self.initialize()
        # The portal registers zero model tools by design.
        self.assertEqual(initialized["result"]["tools"], [])

        connected = self.command(2, ["connect", "fixture"])
        self.assertIn("SSH portal active", connected["result"]["text"])

        collected = self.collect_context(3)
        contributions = collected["result"]
        self.assertEqual(len(contributions), 1)
        content = " ".join(str(contributions[0]["content"]).split())
        self.assertIn("SSH tunnel", content)
        self.assertIn("fixture-alias", content)
        self.assertIn("untrusted", content)

        status = self.command(4, ["status"])
        self.assertIn("fixture-alias", status["result"]["text"])

        disconnected = self.command(5, ["disconnect", "fixture"])
        self.assertIn("disconnected", disconnected["result"]["text"])

        empty = self.collect_context(6)
        self.assertEqual(empty["result"], [])

        self.send({"jsonrpc": "2.0", "id": 7, "method": "shutdown", "params": {}})
        shutdown = self.wait_for_id(7)
        self.assertEqual(shutdown["result"], {})
        self.process.wait(timeout=4)
        self.assertEqual(self.process.returncode, 0, "".join(self.stderr))

    def test_connect_failure_reports_recovery_without_selection(self):
        self.process.kill()
        self.process.wait(timeout=3)
        for stream in (self.process.stdin, self.process.stdout, self.process.stderr):
            if stream is not None:
                stream.close()
        self.stdout_thread.join(timeout=1)
        # Relaunch with a failing probe.
        environment = dict(os.environ)
        environment["FAKE_SSH_EXIT"] = "255"
        self.messages = queue.Queue()
        self.process = subprocess.Popen(
            [
                str(ROOT / "ygg-ssh"),
                "--config",
                str(self.config),
                "--ssh-binary",
                str(FAKE_SSH),
            ],
            cwd=ROOT,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
            env=environment,
        )
        self.stdout_thread = threading.Thread(target=self._read_stdout, daemon=True)
        self.stderr_thread = threading.Thread(target=self._read_stderr, daemon=True)
        self.stdout_thread.start()
        self.stderr_thread.start()
        try:
            self.initialize()
            failed = self.command(2, ["connect", "fixture"])
            self.assertIn("failed", failed["result"]["text"])
            empty = self.collect_context(3)
            self.assertEqual(empty["result"], [])
        finally:
            self.send({"jsonrpc": "2.0", "id": 9, "method": "shutdown", "params": {}})
            shutdown = self.wait_for_id(9)
            self.assertEqual(shutdown["result"], {})
            self.process.wait(timeout=4)


if __name__ == "__main__":
    unittest.main()
