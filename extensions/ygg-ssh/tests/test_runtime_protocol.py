from __future__ import annotations

import json
from pathlib import Path
import queue
import subprocess
import tempfile
import threading
import time
import unittest

from .helpers import FAKE_SSH, ROOT, config_document, write_json


class RuntimeProtocolTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.state = Path(self.temp.name)
        self.remote = self.state / "remote"
        self.remote.mkdir()
        (self.remote / "hello.txt").write_text("hello from remote", encoding="utf-8")
        self.config = write_json(
            self.state / "ssh.json",
            config_document(self.remote, authority="read-write"),
        )
        environment = dict(__import__("os").environ)
        environment["SSH_AUTH_SOCK"] = str(self.state / "agent.sock")
        environment["YGG_EXTENSION_SCRATCH"] = str(self.state / "scratch")
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
        self.presentation_envelopes: list[dict[str, object]] = []
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
                        "version": "0.1.0",
                        "manifest_path": str(ROOT / "extension.toml"),
                        "source": "explicit",
                    },
                    "workspace": str(self.remote),
                    "capabilities": {
                        "filesystem": "unrestricted",
                        "process": True,
                        "network": True,
                        "environment": ["SSH_AUTH_SOCK"],
                    },
                    "contributes": {
                        "tools": [
                            "ssh_status",
                            "ssh_exec",
                            "ssh_read",
                            "ssh_write",
                            "ssh_list",
                        ],
                        "commands": ["ssh"],
                        "ui": ["status"],
                        "context": True,
                        "confirmations": True,
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
                            "lifecycle_events",
                        ],
                        "limits": {"max_concurrent_requests": 4},
                    },
                },
            }
        )
        initialized = self.receive()
        self.assertEqual(initialized["id"], 1)
        self.assertEqual(
            sorted(tool["name"] for tool in initialized["result"]["tools"]),
            ["ssh_exec", "ssh_list", "ssh_read", "ssh_status", "ssh_write"],
        )
        return initialized

    def wait_for_id(self, request_id, *, approve=False):
        presentations = []
        deadline = time.monotonic() + 6
        while time.monotonic() < deadline:
            message = self.receive(timeout=max(0.1, deadline - time.monotonic()))
            if message.get("method") == "presentation/update":
                params = message["params"]
                self.presentation_envelopes.append(dict(params))
                presentations.append(params["snapshot"])
                continue
            if message.get("method") == "confirmation/request":
                self.assertTrue(approve)
                self.assertNotIn("printf", json.dumps(message))
                self.send(
                    {
                        "jsonrpc": "2.0",
                        "id": message["id"],
                        "result": {"confirmed": True},
                    }
                )
                continue
            if message.get("id") == request_id:
                return message, presentations
        self.fail(f"timed out waiting for response {request_id}")

    def test_api_02_connect_read_approved_exec_presentation_and_shutdown(self):
        initialized = self.initialize()
        features = initialized["result"]["protocol"]["features"]
        self.assertIn("request_cancellation", features)
        self.assertIn("content_parts", features)

        self.send(
            {
                "jsonrpc": "2.0",
                "id": 2,
                "method": "command/execute",
                "params": {
                    "name": "ssh",
                    "arguments": ["connect", "fixture"],
                    "context": self.context(),
                },
            }
        )
        connected, presentations = self.wait_for_id(2)
        self.assertIn("SSH connected", connected["result"]["text"])

        self.send(
            {
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tool/call",
                "params": {
                    "name": "ssh_read",
                    "arguments": {"path": "hello.txt"},
                    "context": self.context(),
                },
            }
        )
        read, more = self.wait_for_id(3)
        presentations.extend(more)
        self.assertEqual(read["result"]["structured_content"]["data"], "hello from remote")
        self.assertIn("UNTRUSTED REMOTE DATA", read["result"]["content"][0]["text"])

        self.send(
            {
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tool/call",
                "params": {
                    "name": "ssh_exec",
                    "arguments": {"argv": ["printf", "wire-ok"]},
                    "context": self.context(),
                },
            }
        )
        executed, more = self.wait_for_id(4, approve=True)
        presentations.extend(more)
        self.assertEqual(executed["result"]["structured_content"]["exit_status"], 0)
        self.assertEqual(executed["result"]["structured_content"]["stdout"]["data"], "wire-ok")
        self.assertTrue(presentations)
        self.assertTrue(self.presentation_envelopes)
        scoped_envelopes = [
            envelope
            for envelope in self.presentation_envelopes
            if "resource_owner" in envelope
        ]
        self.assertTrue(scoped_envelopes)
        self.assertTrue(
            all(
                envelope["resource_owner"] == self.context()["resource_owner"]
                for envelope in scoped_envelopes
            )
        )
        self.assertTrue(
            all(
                set(envelope) == {"snapshot"}
                for envelope in self.presentation_envelopes
                if "resource_owner" not in envelope
            )
        )
        self.assertTrue(
            all(
                set(item) == {"revision", "status", "activities", "collection", "actions"}
                for item in presentations
            )
        )
        self.assertTrue(any("remote" in str(item.get("activities")) for item in presentations))

        self.send({"jsonrpc": "2.0", "id": 5, "method": "shutdown", "params": {}})
        shutdown, _ = self.wait_for_id(5)
        self.assertEqual(shutdown["result"], {})
        self.process.wait(timeout=4)
        self.assertEqual(self.process.returncode, 0, "".join(self.stderr))

    def test_status_has_no_host_selection_argument(self):
        initialized = self.initialize()["result"]
        definitions = {item["name"]: item for item in initialized["tools"]}
        self.assertEqual(definitions["ssh_status"]["parameters"]["properties"], {})
        for name in ("ssh_exec", "ssh_read", "ssh_write"):
            properties = definitions[name]["parameters"]["properties"]
            self.assertFalse({"host", "alias", "user", "port", "proxy_jump"} & set(properties))
        self.send({"jsonrpc": "2.0", "id": 9, "method": "shutdown", "params": {}})
        self.wait_for_id(9)


if __name__ == "__main__":
    unittest.main()
