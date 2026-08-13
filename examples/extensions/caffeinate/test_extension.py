#!/usr/bin/env python3
"""Protocol and process-lifecycle tests for the caffeinate example."""

import importlib.util
import io
import json
from pathlib import Path
import subprocess
import sys
import unittest
from unittest import mock


EXTENSION_PATH = Path(__file__).with_name("extension.py")
SDK_PATH = EXTENSION_PATH.parents[3] / "sdk" / "python"
sys.path.insert(0, str(SDK_PATH))


class ExecutablePath:
    def is_file(self):
        return True

    def __str__(self):
        return "/usr/bin/caffeinate"


class FakeProcess:
    def __init__(self, pid=4321):
        self.pid = pid
        self.returncode = None
        self.terminated = False
        self.killed = False

    def poll(self):
        return self.returncode

    def terminate(self):
        self.terminated = True
        self.returncode = -15

    def wait(self, timeout=None):
        if self.returncode is None:
            raise subprocess.TimeoutExpired("caffeinate", timeout)
        return self.returncode

    def kill(self):
        self.killed = True
        self.returncode = -9


def load_extension():
    name = "ygg_caffeinate_example"
    sys.modules.pop(name, None)
    spec = importlib.util.spec_from_file_location(name, EXTENSION_PATH)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def request(request_id, method, params=None):
    return {
        "jsonrpc": "2.0",
        "id": request_id,
        "method": method,
        "params": {} if params is None else params,
    }


def initialize():
    return request(
        1,
        "initialize",
        {
            "api_version": "0.1",
            "contributes": {
                "commands": ["caffeinate"],
                "hooks": ["before_prompt", "after_response"],
                "ui": ["status"],
                "notifications": True,
            },
        },
    )


class CaffeinateTests(unittest.TestCase):
    def tearDown(self):
        sys.modules.pop("ygg_caffeinate_example", None)

    def test_start_is_idempotent_and_stop_releases_inhibitor(self):
        module = load_extension()
        process = FakeProcess()
        with (
            mock.patch.object(module.sys, "platform", "darwin"),
            mock.patch.object(module, "CAFFEINATE", ExecutablePath()),
            mock.patch.object(
                module.subprocess,
                "Popen",
                return_value=process,
            ) as popen,
        ):
            self.assertTrue(module.start_inhibitor())
            self.assertTrue(module.start_inhibitor())
            popen.assert_called_once_with(
                ["/usr/bin/caffeinate", "-i", "-w", str(module.os.getpid())],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            module.stop_inhibitor()

        self.assertTrue(process.terminated)
        self.assertIsNone(module.inhibitor)

    def test_unsupported_platform_reports_a_warning_without_spawning(self):
        module = load_extension()
        with (
            mock.patch.object(module.sys, "platform", "linux"),
            mock.patch.object(module.subprocess, "Popen") as popen,
        ):
            result = module.before_prompt({})

        self.assertEqual(
            result["notifications"][0]["message"],
            "unsupported platform (macOS only)",
        )
        popen.assert_not_called()

    def test_stop_kills_an_inhibitor_that_does_not_terminate(self):
        module = load_extension()
        process = FakeProcess()

        def timeout_wait(timeout=None):
            if not process.killed:
                raise subprocess.TimeoutExpired("caffeinate", timeout)
            return process.returncode

        process.wait = timeout_wait
        module.inhibitor = process
        module.stop_inhibitor()

        self.assertTrue(process.terminated)
        self.assertTrue(process.killed)
        self.assertIsNone(module.inhibitor)

    def test_protocol_hooks_command_status_and_shutdown(self):
        module = load_extension()
        process = FakeProcess()
        messages = [
            initialize(),
            request(
                2,
                "hook/run",
                {"hook": "before_prompt", "payload": {}, "context": {}},
            ),
            request(3, "status/collect", {"surface": "status", "context": {}}),
            request(
                4,
                "command/execute",
                {"name": "caffeinate", "arguments": [], "context": {}},
            ),
            request(
                5,
                "hook/run",
                {"hook": "after_response", "payload": {}, "context": {}},
            ),
            request(6, "status/collect", {"surface": "status", "context": {}}),
            request(7, "shutdown"),
        ]
        request_lines = "\n".join(json.dumps(message) for message in messages)
        input_stream = io.StringIO(request_lines + "\n")
        output = io.StringIO()

        with (
            mock.patch.object(module.sys, "platform", "darwin"),
            mock.patch.object(module, "CAFFEINATE", ExecutablePath()),
            mock.patch.object(module.subprocess, "Popen", return_value=process),
        ):
            module.ext.run(stdin=input_stream, stdout=output)

        replies = [json.loads(line) for line in output.getvalue().splitlines()]
        self.assertEqual(replies[0]["result"]["commands"][0]["name"], "caffeinate")
        self.assertEqual(replies[2]["result"]["text"], "awake")
        self.assertEqual(
            replies[3]["result"]["text"],
            "Caffeinate is active (pid 4321).",
        )
        self.assertIsNone(replies[5]["result"])
        self.assertTrue(process.terminated)


if __name__ == "__main__":
    unittest.main()
