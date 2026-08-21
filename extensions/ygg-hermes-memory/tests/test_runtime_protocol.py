from __future__ import annotations

import json
import os
from pathlib import Path
import select
import subprocess
import sys
import time
import unittest

from .helpers import HERMES_ENV, ROOT, load_fixture_config, mock_descriptor, owner_context, temporary_directory, write_config
from ygg_hermes_memory.discovery import discover_providers


class RuntimeHarness:
    def __init__(self, config: Path, *, mode: str = "normal") -> None:
        environment = os.environ.copy()
        environment.update(
            {
                "YGG_HERMES_PYTHON": sys.executable,
                "YGG_EXTENSION_API_VERSION": "0.2",
                "YGG_EXTENSION_NAME": "ygg-hermes-memory",
                "YGG_EXTENSION_DIR": str(ROOT),
                "YGG_EXTENSION_MANIFEST": str(ROOT / "extension.toml"),
                "YGG_WORKSPACE": str(ROOT),
                "PYTHONPATH": str(HERMES_ENV),
                "YGG_MEMORY_FIXTURE_MODE": mode,
            }
        )
        self.process = subprocess.Popen(
            [str(ROOT / "ygg-hermes-memory"), "--config", str(config)],
            cwd=ROOT,
            env=environment,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
        )
        self.catalog_revision = 0
        self.presentations = []
        self.presentation_owners = []
        self.registrations = []

    def send(self, message):
        self.process.stdin.write(json.dumps(message, separators=(",", ":")) + "\n")
        self.process.stdin.flush()

    def read(self, timeout=3.0):
        ready, _, _ = select.select([self.process.stdout], [], [], timeout)
        if not ready:
            stderr = self.process.stderr.read() if self.process.poll() is not None else ""
            raise AssertionError(f"timed out waiting for extension frame; stderr={stderr}")
        line = self.process.stdout.readline()
        if not line:
            raise AssertionError(f"extension exited {self.process.poll()}: {self.process.stderr.read()}")
        return json.loads(line)

    def next(self, predicate, timeout=5.0):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            message = self.read(max(0.01, deadline - time.monotonic()))
            if message.get("method") == "presentation/update":
                params = message["params"]
                self.presentations.append(params["snapshot"])
                self.presentation_owners.append(
                    params.get("resource_owner") or {"parent_request_id": params.get("parent_request_id")}
                )
                continue
            if message.get("method") in {"tools/register", "tools/unregister"} and "id" in message:
                self.catalog_revision += 1
                if message["method"] == "tools/register":
                    names = [tool["name"] for tool in message["params"]["tools"]]
                    self.registrations.append(message)
                else:
                    removed = set(message["params"]["names"])
                    previous = self.presentations[-1].get("publishedTools", []) if self.presentations else []
                    names = [name for name in previous if name not in removed]
                self.send(
                    {
                        "jsonrpc": "2.0",
                        "id": message["id"],
                        "result": {"revision": self.catalog_revision, "tools": names},
                    }
                )
                if predicate(message):
                    return message
                continue
            if predicate(message):
                return message
        raise AssertionError("expected protocol message was not observed")

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
                        "name": "ygg-hermes-memory",
                        "version": "0.1.0",
                        "manifest_path": str(ROOT / "extension.toml"),
                        "source": "explicit",
                    },
                    "workspace": str(ROOT),
                    "capabilities": {
                        "filesystem": "unrestricted",
                        "process": True,
                        "network": True,
                    },
                    "contributes": {
                        "commands": ["memory"],
                        "hooks": ["before_prompt", "after_response", "after_tool_call"],
                        "ui": ["status"],
                        "context": True,
                        "presentation": True,
                    },
                    "host": {
                        "session_id": "wire-session",
                        "session_name": None,
                        "model": "test-model",
                        "reasoning": None,
                        "active_skills": [],
                    },
                    "protocol": {
                        "version": "0.2",
                        "required_features": ["request_cancellation", "content_parts"],
                        "optional_features": [
                            "request_progress",
                            "lifecycle_events",
                            "dynamic_tools",
                        ],
                        "limits": {"max_concurrent_requests": 8},
                    },
                },
            }
        )
        response = self.next(lambda item: item.get("id") == 1)
        return response

    def shutdown(self):
        if self.process.poll() is not None:
            return
        self.send({"jsonrpc": "2.0", "id": 99, "method": "shutdown", "params": {}})
        response = self.next(lambda item: item.get("id") == 99, timeout=4.0)
        assert "result" in response
        self.process.wait(timeout=4)

    def close(self):
        if self.process.poll() is None:
            self.process.kill()
            self.process.wait(timeout=2)
        for stream in (self.process.stdin, self.process.stdout, self.process.stderr):
            if stream is not None and not stream.closed:
                stream.close()


def trusted_config(directory: Path, *, limits=None) -> Path:
    path = write_config(
        directory,
        providers=[mock_descriptor()],
        include_entry_points=False,
        limits=limits or {},
    )
    config = load_fixture_config(
        directory,
        providers=[mock_descriptor()],
        include_entry_points=False,
        limits=limits or {},
    )
    candidate = discover_providers(config).by_id("directory:mock")
    value = json.loads(path.read_text(encoding="utf-8"))
    value["trustedProviders"] = {candidate.id: candidate.fingerprint}
    value["defaultProvider"] = candidate.id
    path.write_text(json.dumps(value), encoding="utf-8")
    os.chmod(path, 0o600)
    return path


class RuntimeProtocolTests(unittest.TestCase):
    def test_generation_fence_uses_exit_bound_before_provider_import(self):
        environment = os.environ.copy()
        environment["PYTHONPATH"] = os.pathsep.join(
            [str(ROOT / "vendor"), str(ROOT), environment.get("PYTHONPATH", "")]
        )
        process = subprocess.run(
            [
                sys.executable,
                "-c",
                (
                    "from ygg_hermes_memory import runtime; "
                    "runtime.os._exit=lambda code: (_ for _ in ()).throw(RuntimeError(code)); "
                    "runtime._abort_provider_generation('fixture')"
                ),
            ],
            cwd=ROOT,
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=5,
            check=False,
        )
        self.assertEqual(process.returncode, 70, process.stderr)

    def test_api_02_dynamic_tools_context_lifecycle_presentation_and_shutdown(self):
        with temporary_directory() as directory:
            config = trusted_config(directory)
            harness = RuntimeHarness(config)
            try:
                initialized = harness.initialize()
                result = initialized["result"]
                self.assertEqual(result["api_version"], "0.2")
                self.assertEqual(result["tools"], [])
                self.assertIn("dynamic_tools", result["protocol"]["features"])
                self.assertIn("lifecycle_events", result["protocol"]["features"])

                context = owner_context("wire-session")
                harness.send(
                    {
                        "jsonrpc": "2.0",
                        "id": 2,
                        "method": "hook/run",
                        "params": {"hook": "before_prompt", "payload": {"prompt": "wire query"}, "context": context},
                    }
                )
                self.assertIn("result", harness.next(lambda item: item.get("id") == 2))
                if not harness.registrations:
                    harness.next(lambda item: item.get("method") == "tools/register")
                self.assertTrue(harness.registrations)
                names = {
                    item["name"]
                    for item in harness.registrations[-1]["params"]["tools"]
                }
                self.assertEqual(names, {"recall_mock", "remember_mock"})
                harness.send(
                    {
                        "jsonrpc": "2.0",
                        "id": 3,
                        "method": "context/collect",
                        "params": {"prompt": "wire query", "context": context},
                    }
                )
                collected = harness.next(lambda item: item.get("id") == 3)["result"]
                self.assertTrue(collected)
                self.assertTrue(all("YGG_UNTRUSTED_MEMORY_BEGIN" in item["content"] for item in collected))

                harness.send(
                    {
                        "jsonrpc": "2.0",
                        "id": 4,
                        "method": "tool/call",
                        "params": {
                            "name": "remember_mock",
                            "arguments": {"content": "wire fact"},
                            "catalog_revision": harness.catalog_revision,
                            "context": context,
                        },
                    }
                )
                tool = harness.next(lambda item: item.get("id") == 4)["result"]
                self.assertFalse(tool["is_error"])
                self.assertEqual(tool["metadata"]["durability"], "committed")

                harness.send(
                    {
                        "jsonrpc": "2.0",
                        "id": 5,
                        "method": "hook/run",
                        "params": {"hook": "after_response", "payload": {"response": "wire answer"}, "context": context},
                    }
                )
                harness.next(lambda item: item.get("id") == 5)
                harness.send(
                    {
                        "jsonrpc": "2.0",
                        "method": "turn/settled",
                        "params": {"session_id": "wire-session", "turn_id": "wire-turn", "outcome": "completed", "duration_ms": 1, "reason": None},
                    }
                )
                harness.send(
                    {
                        "jsonrpc": "2.0",
                        "id": 6,
                        "method": "command/execute",
                        "params": {"name": "memory", "arguments": ["snapshot"], "context": context},
                    }
                )
                snapshot_text = harness.next(lambda item: item.get("id") == 6)["result"]["text"]
                snapshot = json.loads(snapshot_text)
                self.assertEqual(set(snapshot), {"revision", "status", "activities", "collection", "actions"})
                self.assertTrue(any(item["kind"] == "memory_read" for item in snapshot["activities"]))
                self.assertTrue(any(item["kind"] == "memory_write" for item in snapshot["activities"]))
                serialized = json.dumps(snapshot)
                self.assertNotIn("wire fact", serialized)
                self.assertNotIn("wire query", serialized)
                time.sleep(0.08)
                harness.send(
                    {
                        "jsonrpc": "2.0",
                        "id": 7,
                        "method": "command/execute",
                        "params": {"name": "memory", "arguments": ["status"], "context": context},
                    }
                )
                harness.next(lambda item: item.get("id") == 7)
                self.assertTrue(harness.presentations)
                self.assertTrue(
                    any(
                        owner.get("session_id") == "wire-session"
                        or owner.get("parent_request_id") in {2, 3, 4, 5, 6}
                        for owner in harness.presentation_owners
                    )
                )
                harness.shutdown()
            finally:
                harness.close()

    def test_request_cancellation_fences_uncooperative_provider_generation(self):
        with temporary_directory() as directory:
            config = trusted_config(directory, limits={"prefetchTimeoutMs": 1000})
            harness = RuntimeHarness(config, mode="slow-prefetch")
            try:
                harness.initialize()
                context = owner_context("wire-session")
                harness.send(
                    {
                        "jsonrpc": "2.0",
                        "id": 10,
                        "method": "hook/run",
                        "params": {"hook": "before_prompt", "payload": {"prompt": "slow"}, "context": context},
                    }
                )
                harness.next(lambda item: item.get("id") == 10)
                if not harness.registrations:
                    harness.next(lambda item: item.get("method") == "tools/register")
                # The reverse registration response unblocks activation; wait for
                # that owner assignment before starting the cancellable call.
                time.sleep(0.1)
                harness.send(
                    {
                        "jsonrpc": "2.0",
                        "id": 11,
                        "method": "context/collect",
                        "params": {"prompt": "slow", "context": context},
                    }
                )
                time.sleep(0.05)
                harness.send(
                    {
                        "jsonrpc": "2.0",
                        "method": "$/cancelRequest",
                        "params": {"id": 11, "reason": "test"},
                    }
                )
                harness.process.wait(timeout=2)
                self.assertEqual(harness.process.returncode, 70)
            finally:
                harness.close()


if __name__ == "__main__":
    unittest.main()
