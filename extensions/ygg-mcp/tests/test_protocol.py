from __future__ import annotations

import os
from pathlib import Path
import signal
import tempfile
import threading
import time
import unittest

from ygg_mcp.protocol import (
    McpCancelled,
    McpError,
    McpProtocolError,
    McpStdioClient,
    McpTimeout,
    McpTransportError,
)

from .helpers import FakeCancellation, limits, server_config, wait_for


class ProtocolTests(unittest.TestCase):
    def clients(self):
        if not hasattr(self, "_clients"):
            self._clients = []
        return self._clients

    def tearDown(self):
        for client in reversed(getattr(self, "_clients", [])):
            client.close()

    def make_client(self, config, bridge_limits, **callbacks):
        client = McpStdioClient(config, bridge_limits, **callbacks)
        self.clients().append(client)
        return client

    def test_real_stdio_initialize_catalog_and_call(self):
        from .helpers import real_server_config

        client = self.make_client(real_server_config(), limits())
        client.start()
        self.assertEqual(client.protocol_version, "2025-06-18")
        self.assertEqual(
            [tool["name"] for tool in client.list_tools()],
            ["fixture_echo", "fixture_media", "fixture_unknown_effect"],
        )
        result = client.call_tool("fixture_echo", {"value": "hello"})
        self.assertEqual(result["structuredContent"], {"echo": "hello"})

    def test_malformed_and_oversized_frames_are_permanent_protocol_failures(self):
        malformed = self.make_client(server_config("malformed"), limits())
        malformed.start()
        with self.assertRaises(McpProtocolError) as malformed_error:
            malformed.list_tools()
        self.assertTrue(malformed_error.exception.permanent)
        self.assertEqual(malformed_error.exception.code, "malformed_frame")

        bounded = limits(max_frame_bytes=4096, max_result_bytes=4096)
        oversized = self.make_client(
            server_config(
                "oversized",
                extra_args=("--oversized-bytes", "8192"),
            ),
            bounded,
        )
        oversized.start()
        with self.assertRaises(McpProtocolError) as oversized_error:
            oversized.list_tools()
        self.assertTrue(oversized_error.exception.permanent)
        self.assertEqual(oversized_error.exception.code, "oversized_frame")

    def test_cancellation_is_forwarded_without_closing_a_healthy_session(self):
        cancellation = FakeCancellation()
        client = self.make_client(
            server_config("stable", request_timeout_ms=1000),
            limits(cancellation_grace_ms=100),
        )
        client.start()
        timer = threading.Timer(0.05, cancellation.cancel)
        timer.start()
        with self.assertRaises(McpCancelled):
            client.call_tool("sleep", {"value": "wait"}, cancellation=cancellation)
        timer.join()
        wait_for(lambda: client.alive, message="healthy client after cancellation")
        result = client.call_tool("echo", {"value": "still-ready"})
        self.assertEqual(result["structuredContent"]["value"], "still-ready")

    def test_timeout_is_bounded_and_never_replays_the_tool(self):
        client = self.make_client(
            server_config("timeout", request_timeout_ms=40),
            limits(),
        )
        client.start()
        started = time.monotonic()
        with self.assertRaises(McpTimeout) as captured:
            client.call_tool("sleep", {"value": "wait"})
        self.assertLess(time.monotonic() - started, 1.0)
        self.assertTrue(captured.exception.ambiguous)
        self.assertTrue(client.alive)

    def test_crash_reports_failure_and_does_not_expose_server_text(self):
        failures = []
        event = threading.Event()

        def failed(client, error):
            del client
            failures.append(error)
            event.set()

        client = self.make_client(
            server_config("crash"),
            limits(),
            on_failure=failed,
        )
        client.start()
        client.list_tools()
        with self.assertRaises(McpTransportError):
            client.call_tool("echo", {"value": "boom"})
        self.assertTrue(event.wait(2))
        self.assertEqual(failures[0].code, "server_exited")
        self.assertNotIn("boom", failures[0].safe_summary)

    def test_stderr_log_ring_is_bounded_and_redacts_explicit_environment_values(self):
        config = server_config(
            "logs",
            environment={"FIXTURE_SECRET": "SECRET_FIXTURE_VALUE"},
        )
        client = self.make_client(
            config,
            limits(max_log_entries=8, max_log_line_bytes=128),
        )
        client.start()
        wait_for(lambda: client.logs.dropped > 0, message="bounded log drops")
        entries = client.logs.snapshot()
        self.assertLessEqual(len(entries), 8)
        self.assertTrue(all("SECRET_FIXTURE_VALUE" not in entry.text for entry in entries))
        self.assertTrue(any("[redacted]" in entry.text for entry in entries))

    def test_blocked_server_stdin_times_out_without_blocking_the_host_thread(self):
        client = self.make_client(
            server_config("blocked", request_timeout_ms=100),
            limits(max_frame_bytes=4 * 1024 * 1024),
        )
        client.start()
        started = time.monotonic()
        with self.assertRaises(McpTimeout) as captured:
            client.call_tool("echo", {"value": "x" * (2 * 1024 * 1024)})
        self.assertLess(time.monotonic() - started, 1.0)
        self.assertEqual(captured.exception.code, "write_timeout")
        self.assertTrue(captured.exception.ambiguous)
        self.assertFalse(client.alive)

    @unittest.skipUnless(os.name == "posix", "POSIX process-group regression")
    def test_close_kills_server_descendants_that_retain_stdio(self):
        with tempfile.TemporaryDirectory() as directory:
            pid_path = Path(directory) / "descendant.pid"
            client = self.make_client(
                server_config(
                    "stable",
                    extra_args=("--descendant-pid", str(pid_path)),
                ),
                limits(shutdown_timeout_ms=300),
            )
            client.start()
            descendant_pid = int(pid_path.read_text(encoding="ascii"))
            try:
                client.close()

                def descendant_stopped() -> bool:
                    try:
                        os.kill(descendant_pid, 0)
                    except OSError:
                        return True
                    return False

                wait_for(descendant_stopped, message="MCP descendant cleanup")
            finally:
                try:
                    os.kill(descendant_pid, signal.SIGKILL)
                except OSError:
                    pass

    def test_close_reaches_fixture_eof_and_shutdown_marker(self):
        with tempfile.TemporaryDirectory() as directory:
            marker = Path(directory) / "closed.marker"
            config = server_config(
                "stable",
                extra_args=("--shutdown-marker", str(marker)),
            )
            client = self.make_client(config, limits(shutdown_timeout_ms=1000))
            client.start()
            client.close()
            wait_for(marker.exists, message="fixture graceful EOF marker")


if __name__ == "__main__":
    unittest.main()
