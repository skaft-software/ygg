from __future__ import annotations

import base64
import os
from pathlib import Path
import signal
import subprocess
import sys
import threading
import time
import unittest

from ygg_ssh.manager import AdapterError, OwnerFence

from .helpers import CONTEXT, OWNER_FENCE, ManagerHarness


class Token:
    def __init__(self) -> None:
        self.cancelled = False


class ManagerTests(unittest.TestCase):
    def test_connect_uses_only_configured_alias_and_existing_agent(self):
        with ManagerHarness(agent=True) as harness:
            harness.connect()
            status = harness.manager.status(CONTEXT)
            self.assertTrue(status["connected"])
            self.assertTrue(status["agent_socket_available"])
            masters = [event for event in harness.events() if event["kind"] == "master"]
            self.assertEqual(masters[0]["alias"], "fixture-alias")
            self.assertTrue(masters[0]["agent_available"])
            self.assertNotIn(str(harness.root / "agent.sock"), str(masters[0]))

    def test_absent_agent_is_visible_without_collecting_credentials(self):
        with ManagerHarness(agent=False) as harness:
            harness.connect()
            status = harness.manager.status(CONTEXT)
            self.assertFalse(status["agent_socket_available"])
            master = next(event for event in harness.events() if event["kind"] == "master")
            self.assertFalse(master["agent_available"])

    def test_selection_requires_command_context_and_unknown_target_fails(self):
        with ManagerHarness() as harness:
            with self.assertRaisesRegex(AdapterError, "active Ygg session"):
                harness.manager.request_action("connect", "fixture", {})
            with self.assertRaisesRegex(AdapterError, "allowlist"):
                harness.manager.request_action("connect", "invented-host", CONTEXT)
            with self.assertRaisesRegex(AdapterError, "no SSH target"):
                harness.manager.read_file(CONTEXT, "a.txt")

    def test_read_is_bounded_and_control_bytes_are_base64(self):
        with ManagerHarness() as harness:
            (harness.remote / "data.bin").write_bytes(b"hello\x1b[31m-world")
            harness.connect()
            result = harness.manager.read_file(CONTEXT, "data.bin", max_bytes=8)
            self.assertEqual(result["encoding"], "base64")
            self.assertLessEqual(len(base64.b64decode(result["data"])), 8)
            self.assertTrue(result["truncated"])
            with self.assertRaisesRegex(AdapterError, "normalized"):
                harness.manager.read_file(CONTEXT, "../secret")

    def test_list_lists_cwd_subdirs_and_reports_missing_paths(self):
        with ManagerHarness() as harness:
            (harness.remote / "docs").mkdir()
            (harness.remote / "notes.txt").write_text("hello")
            harness.connect()
            root = harness.manager.list_dir(CONTEXT, "")
            self.assertIn("docs/", root["entries"])
            self.assertIn("notes.txt", root["entries"])
            self.assertTrue(root["resolved_path"].endswith("remote"))
            nested = harness.manager.list_dir(CONTEXT, "docs")
            self.assertEqual(nested["entries"], [])
            with self.assertRaisesRegex(AdapterError, "remote_not_found|does not exist"):
                harness.manager.list_dir(CONTEXT, "missing-dir")
            with self.assertRaisesRegex(AdapterError, "remote_not_found|does not exist"):
                harness.manager.list_dir(CONTEXT, "notes.txt")

    def test_context_contribution_reflects_connection_state(self):
        with ManagerHarness(authority="read-only") as harness:
            self.assertIsNone(harness.manager.context_contribution())
            harness.connect()
            contribution = harness.manager.context_contribution()
            self.assertIsNotNone(contribution)
            self.assertEqual(contribution["label"], "ygg-ssh")
            self.assertEqual(contribution["placement"], "prompt_suffix")
            self.assertIn("read-only", contribution["content"])
            self.assertIn(str(harness.remote), contribution["content"])

    def test_read_only_denies_exec_and_write_without_approval(self):
        with ManagerHarness(authority="read-only") as harness:
            harness.connect()
            with self.assertRaisesRegex(AdapterError, "read-only"):
                harness.manager.execute(CONTEXT, ["printf", "no"])
            with self.assertRaisesRegex(AdapterError, "read-only"):
                harness.manager.write_file(CONTEXT, "new.txt", "no")
            self.assertEqual(harness.approvals, [])

    def test_mutations_require_fresh_approval_and_atomic_write(self):
        with ManagerHarness(confirm=True) as harness:
            harness.connect()
            command = harness.manager.execute(CONTEXT, ["printf", "ok"])
            self.assertEqual(command["stdout"]["data"], "ok")
            write = harness.manager.write_file(CONTEXT, "new.txt", "hello")
            self.assertEqual(write["bytes_written"], 5)
            self.assertEqual((harness.remote / "new.txt").read_text(), "hello")
            self.assertEqual(len(harness.approvals), 2)
            self.assertTrue(all(item[2] for item in harness.approvals))
            self.assertNotIn("printf", " ".join(item[1] for item in harness.approvals))

        with ManagerHarness(confirm=False) as denied:
            denied.connect()
            with self.assertRaisesRegex(AdapterError, "not approved"):
                denied.manager.execute(CONTEXT, ["touch", "blocked"])
            self.assertFalse((denied.remote / "blocked").exists())

    def test_write_treats_option_like_paths_as_data_and_rejects_directory_destinations(self):
        with ManagerHarness(confirm=True) as harness:
            harness.connect()
            option_path = "--target-directory=.."
            written = harness.manager.write_file(CONTEXT, option_path, "bounded")
            self.assertEqual(written["bytes_written"], 7)
            self.assertEqual((harness.remote / option_path).read_text(), "bounded")
            self.assertFalse((harness.root / option_path).exists())

            destination = harness.remote / "existing-directory"
            destination.mkdir()
            with self.assertRaisesRegex(AdapterError, "destination already exists"):
                harness.manager.write_file(
                    CONTEXT,
                    "existing-directory",
                    "must-not-move-inside",
                    overwrite=True,
                )
            self.assertEqual(list(destination.iterdir()), [])

    def test_output_and_diagnostics_are_bounded_and_do_not_log_commands_or_banner(self):
        environment = dict(os.environ)
        environment["YGG_SSH_FAKE_BANNER"] = "SECRET-BANNER-CONTENT"
        with ManagerHarness(environment=environment) as harness:
            harness.connect()
            result = harness.manager.execute(CONTEXT, ["fake-output:10000", "SECRET-ARGUMENT"])
            self.assertLessEqual(len(result["stdout"]["data"]), harness.limits.max_output_bytes)
            self.assertTrue(result["stdout"]["truncated"])
            diagnostics = str(harness.manager.diagnostics)
            self.assertNotIn("SECRET-ARGUMENT", diagnostics)
            self.assertNotIn("SECRET-BANNER-CONTENT", diagnostics)
            snapshot = str(harness.manager.presentation_snapshot(OWNER_FENCE))
            self.assertNotIn("SECRET-ARGUMENT", snapshot)
            self.assertNotIn("SECRET-BANNER-CONTENT", snapshot)

    def test_disconnect_after_mutation_is_ambiguous_never_replayed_and_retry_is_explicit(self):
        with ManagerHarness() as harness:
            harness.connect()
            with self.assertRaises(AdapterError) as caught:
                harness.manager.execute(CONTEXT, ["fake-disconnect"])
            self.assertTrue(caught.exception.ambiguous)
            status = harness.manager.status(CONTEXT)
            self.assertEqual(status["state"], "degraded")
            self.assertTrue(status["ambiguous"])
            command_count = len([event for event in harness.events() if event["kind"] == "command"])
            with self.assertRaisesRegex(AdapterError, "retry"):
                harness.manager.execute(CONTEXT, ["printf", "not-replayed"])
            self.assertEqual(
                len([event for event in harness.events() if event["kind"] == "command"]),
                command_count,
            )
            message = harness.manager.request_action("retry", "fixture", CONTEXT)
            self.assertIn("generation 2", message)
            self.assertFalse(harness.manager.status(CONTEXT)["ambiguous"])

    def test_cancellation_kills_local_descendant_process_group_and_marks_mutation_ambiguous(self):
        with ManagerHarness() as harness:
            pid_file = harness.root / "descendant.pid"
            harness.backend.environment["YGG_SSH_FAKE_DESCENDANT_PID"] = str(pid_file)
            harness.connect()
            token = Token()
            errors: list[BaseException] = []

            def call() -> None:
                try:
                    harness.manager.execute(
                        CONTEXT, ["fake-descendant"], timeout_ms=1500, cancellation=token
                    )
                except BaseException as error:
                    errors.append(error)

            thread = threading.Thread(target=call)
            thread.start()
            deadline = time.monotonic() + 2
            descendant = None
            while descendant is None and time.monotonic() < deadline:
                try:
                    encoded = pid_file.read_text().strip()
                    if encoded:
                        descendant = int(encoded)
                except (FileNotFoundError, ValueError):
                    pass
                if descendant is None:
                    time.sleep(0.01)
            self.assertIsNotNone(descendant)
            assert descendant is not None
            token.cancelled = True
            thread.join(timeout=3)
            self.assertFalse(thread.is_alive())
            self.assertTrue(errors and isinstance(errors[0], AdapterError))
            self.assertTrue(errors[0].ambiguous)
            deadline = time.monotonic() + 2
            while _pid_alive(descendant) and time.monotonic() < deadline:
                time.sleep(0.02)
            self.assertFalse(_pid_alive(descendant), "local fake-ssh descendant survived cleanup")

    @unittest.skipUnless(os.name == "posix", "POSIX watchdog regression")
    def test_parent_crash_watchdog_kills_control_master_process_group(self):
        with ManagerHarness() as harness:
            pid_file = harness.root / "crash-master.pid"
            script = """
import os
from pathlib import Path
import sys
import time
from ygg_ssh.config import Limits
from ygg_ssh.process import OpenSshBackend
backend = OpenSshBackend(
    Limits(connect_timeout_ms=2000, termination_grace_ms=100),
    ssh_binary=sys.argv[1],
    runtime_directory=Path(sys.argv[2]),
    environment=dict(os.environ),
)
handle = backend.connect_master("fixture-alias", backend.control_path("crash-owner", 1))
Path(sys.argv[3]).write_text(str(handle.process.pid), encoding="ascii")
while True:
    time.sleep(1)
"""
            environment = dict(harness.environment)
            environment["PYTHONPATH"] = str(Path(__file__).resolve().parents[1])
            parent = subprocess.Popen(
                [
                    sys.executable,
                    "-c",
                    script,
                    str(harness.backend.ssh_binary),
                    str(harness.root / "crash-control"),
                    str(pid_file),
                ],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                env=environment,
            )
            master_pid = None
            try:
                deadline = time.monotonic() + 3
                while master_pid is None and time.monotonic() < deadline:
                    try:
                        master_pid = int(pid_file.read_text(encoding="ascii"))
                    except (FileNotFoundError, ValueError):
                        time.sleep(0.02)
                self.assertIsNotNone(master_pid)
                assert master_pid is not None
                parent.kill()
                parent.wait(timeout=2)
                deadline = time.monotonic() + 3
                while _pid_alive(master_pid) and time.monotonic() < deadline:
                    time.sleep(0.02)
                self.assertFalse(
                    _pid_alive(master_pid),
                    "ControlMaster survived an abrupt extension-process crash",
                )
            finally:
                if parent.poll() is None:
                    parent.kill()
                    parent.wait(timeout=2)
                if master_pid is not None and _pid_alive(master_pid):
                    try:
                        os.killpg(master_pid, signal.SIGKILL)
                    except OSError:
                        pass

    def test_timeout_marks_mutation_ambiguous_and_does_not_replay(self):
        with ManagerHarness() as harness:
            harness.connect()
            with self.assertRaises(AdapterError) as caught:
                harness.manager.execute(CONTEXT, ["fake-descendant"], timeout_ms=100)
            self.assertTrue(caught.exception.ambiguous)
            self.assertEqual(harness.manager.status(CONTEXT)["state"], "degraded")
            commands = [event for event in harness.events() if event["kind"] == "command"]
            self.assertEqual(len(commands), 1)

    def test_passive_health_failure_degrades_without_automatic_reconnect(self):
        with ManagerHarness(health_interval_ms=250) as harness:
            harness.connect()
            harness.backend.environment["YGG_SSH_FAKE_HEALTH_FAIL"] = "1"
            deadline = time.monotonic() + 2
            status = harness.manager.status(CONTEXT)
            while status["state"] != "degraded" and time.monotonic() < deadline:
                time.sleep(0.05)
                status = harness.manager.status(CONTEXT)
            self.assertEqual(status["state"], "degraded")
            masters = [event for event in harness.events() if event["kind"] == "master"]
            self.assertEqual(len(masters), 1)

    def test_session_settlement_uses_public_host_session_not_resource_owner_key(self):
        context = {
            "resource_owner": {
                "session_id": "session-sha256-owner-key",
                "extension_instance_id": "fixture-instance",
                "process_generation": 7,
            },
            "host": {"session_id": "public-session-id"},
        }
        with ManagerHarness() as harness:
            harness.connect(context)
            self.assertTrue(harness.manager.status(context)["connected"])
            harness.manager.settle_session("public-session-id")
            self.assertFalse(harness.manager.status(context)["connected"])

    def test_owner_fence_replaces_stale_connection_and_session_settlement_cleans_up(self):
        with ManagerHarness() as harness:
            harness.connect()
            replacement = {
                "resource_owner": {
                    "session_id": "fixture-session",
                    "extension_instance_id": "replacement-instance",
                    "process_generation": 8,
                },
                "host": {"session_id": "fixture-session"},
            }
            status = harness.manager.status(replacement)
            self.assertTrue(status["connected"])
            snapshot = harness.manager.presentation_snapshot(OwnerFence.from_context(replacement))
            connection_nodes = [
                node
                for node in snapshot["collection"]["nodes"]
                if node["id"].startswith("connection:")
            ]
            self.assertEqual(len(connection_nodes), 1)
            harness.manager.settle_session("fixture-session")
            self.assertIn("disconnected", harness.manager.format_status(replacement))


def _pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except OSError:
        return False
    return True


if __name__ == "__main__":
    unittest.main()
