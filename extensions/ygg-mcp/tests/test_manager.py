from __future__ import annotations

import json
from pathlib import Path
import random
import tempfile
import unittest
from unittest import mock

from ygg_mcp.config import BridgeConfig, HttpAuthConfig, ServerConfig
from ygg_mcp.manager import BridgeManager

from .helpers import (
    FakeExtension,
    limits,
    real_server_config,
    server_config,
    wait_for,
)


def root_node(snapshot, server_id):
    for node in snapshot["collection"]["nodes"]:
        if node["id"] == f"server:{server_id}":
            return node
    raise AssertionError(f"missing server node {server_id}")


class ManagerTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.scratch = Path(self.temporary.name)
        self.managers = []

    def tearDown(self):
        for manager in reversed(self.managers):
            manager.shutdown()
        self.temporary.cleanup()

    def manager(self, server, bridge_limits=None, *, policy="deny"):
        selected_limits = bridge_limits or limits(
            backoff_initial_ms=10,
            backoff_max_ms=20,
            shutdown_timeout_ms=500,
        )
        extension = FakeExtension(self.scratch, policy=policy)
        manager = BridgeManager(
            extension,
            BridgeConfig(servers=(server,), limits=selected_limits),
            scratch_directory=self.scratch,
            random_source=random.Random(1),
        )
        self.managers.append(manager)
        manager.start()
        return extension, manager

    def test_real_fixture_end_to_end_preserves_structured_and_media_results(self):
        extension, manager = self.manager(real_server_config())
        wait_for(
            lambda: root_node(manager.snapshot(), "real-fixture")["state"] == "active",
            message="real fixture ready",
        )
        names = sorted(extension._tools)
        self.assertEqual(len(names), 3)
        echo_name = next(name for name in names if "fixture_echo" in name)
        media_name = next(name for name in names if "fixture_media" in name)
        unknown_name = next(name for name in names if "fixture_unknown_effect" in name)

        echo = extension._tools[echo_name]["handler"]({"value": "hello"}, {})
        self.assertFalse(echo["is_error"])
        self.assertEqual(echo["structured_content"], {"echo": "hello"})
        self.assertEqual(echo["content"][0]["text"], "fixture echo: hello")

        media = extension._tools[media_name]["handler"]({}, {})
        self.assertFalse(media["is_error"])
        self.assertEqual([part["type"] for part in media["content"]], ["text", "image", "audio"])
        self.assertEqual(len(extension.artifacts), 2)

        denied = extension._tools[unknown_name]["handler"]({}, {})
        self.assertTrue(denied["is_error"])
        self.assertIn("denied", denied["content"][0]["text"].lower())
        self.assertTrue(extension.presentations)

    def test_catalog_add_replace_remove_and_epoch_pinned_schema_handlers(self):
        extension, manager = self.manager(server_config("catalog"))
        wait_for(
            lambda: root_node(manager.snapshot(), "fixture")["state"] == "active",
            message="initial catalog",
        )
        revision_one = extension.catalogs[1]
        versioned_name = next(name for name in revision_one if "versioned" in name)
        removed_name = next(name for name in revision_one if "removed" in name)
        old_handler = revision_one[versioned_name]["handler"]

        self.assertTrue(manager.refresh_server("fixture"))
        current = extension.catalogs[extension._revision]
        added_name = next(name for name in current if "added" in name)
        self.assertNotIn(removed_name, current)
        new_handler = current[versioned_name]["handler"]

        old_result = old_handler({"value": "x", "extra": 1}, {})
        new_result = new_handler({"value": "x", "extra": 1}, {})
        self.assertTrue(old_result["is_error"], "old epoch must enforce its old schema")
        self.assertFalse(new_result["is_error"], "replacement epoch must use its new schema")
        self.assertEqual(new_result["structured_content"]["version"], "v2")

        self.assertTrue(manager.refresh_server("fixture"))
        final = extension.catalogs[extension._revision]
        self.assertNotIn(added_name, final)
        self.assertIn(versioned_name, final)

    def test_crash_restarts_once_then_parks_without_replaying_calls(self):
        bridge_limits = limits(
            backoff_initial_ms=10,
            backoff_max_ms=20,
            shutdown_timeout_ms=300,
        )
        extension, manager = self.manager(
            server_config("crash", max_restarts=1), bridge_limits
        )
        wait_for(
            lambda: root_node(manager.snapshot(), "fixture")["state"] == "active",
            message="crash fixture ready",
        )
        initial_revision = extension._revision
        first_name = next(iter(extension._tools))
        first = extension._tools[first_name]["handler"]({"value": "first"}, {})
        self.assertTrue(first["is_error"])
        self.assertIn("not replayed", first["content"][0]["text"])
        wait_for(
            lambda: extension._revision >= initial_revision + 2
            and root_node(manager.snapshot(), "fixture")["state"] == "active",
            message="one automatic restart",
        )

        second_name = next(iter(extension._tools))
        second = extension._tools[second_name]["handler"]({"value": "second"}, {})
        self.assertTrue(second["is_error"])
        wait_for(
            lambda: root_node(manager.snapshot(), "fixture")["state"] == "unavailable",
            message="parked after restart budget",
        )
        server = root_node(manager.snapshot(), "fixture")
        self.assertIn("catalog", server["secondary"])

    def test_permanent_protocol_failure_parks_and_publishes_no_tool(self):
        extension, manager = self.manager(server_config("malformed", max_restarts=8))
        wait_for(
            lambda: root_node(manager.snapshot(), "fixture")["state"] == "unavailable",
            message="permanent malformed frame parking",
        )
        self.assertEqual(extension._tools, {})
        detail = manager.execute_command(["show", "fixture"])["text"]
        self.assertIn("parked", detail)
        self.assertIn("malformed_frame", detail)

    def test_compact_and_generic_state_never_expose_launch_or_environment_values(self):
        secret = "TOP_SECRET_CONFIG_VALUE"
        configured = server_config(
            "stable",
            environment={"FIXTURE_TOKEN": secret},
            extra_args=("--shutdown-marker", "/sensitive/path/marker"),
        )
        extension, manager = self.manager(configured)
        snapshot = manager.snapshot()
        encoded = json.dumps(snapshot)
        status = manager.status_contribution()["text"]
        self.assertNotIn(secret, encoded)
        self.assertNotIn("shutdown-marker", encoded)
        self.assertNotIn("/sensitive", encoded)
        self.assertNotIn(secret, status)
        self.assertRegex(status, r"^mcp \d+/1 · \d+ tools")
        self.assertEqual(snapshot["revision"], extension.presentations[-1]["revision"])

    def test_remote_gate_rejects_before_credentials_dns_or_workers(self):
        remote = ServerConfig(
            id="remote",
            label="Remote fixture",
            command="",
            args=(),
            cwd=self.scratch,
            environment={},
            transport="streamable-http",
            url="https://mcp.example.invalid/mcp",
            auth=HttpAuthConfig(credential="remote_fixture"),
        )
        credential_provider = mock.Mock()
        client_factory = mock.Mock(side_effect=AssertionError("remote client must not be built"))
        extension = FakeExtension(self.scratch)
        with mock.patch("socket.getaddrinfo", side_effect=AssertionError("DNS must not run")), mock.patch(
            "ygg_mcp.manager.ThreadPoolExecutor",
            side_effect=AssertionError("manager worker must not be constructed"),
        ):
            manager = BridgeManager(
                extension,
                BridgeConfig(servers=(remote,), limits=limits()),
                scratch_directory=self.scratch,
                credential_provider=credential_provider,
                client_factory=client_factory,
            )
            self.managers.append(manager)
            manager.start()
            self.assertTrue(manager.request_action("refresh").result())
            with self.assertRaisesRegex(ValueError, "process-owner experimental CLI opt-in"):
                manager.request_action("restart", "remote")

        self.assertIsNone(manager._executor)
        self.assertEqual(manager._servers["remote"].state, "parked")
        client_factory.assert_not_called()
        credential_provider.bearer_token.assert_not_called()

    def test_safe_user_actions_route_through_declared_mcp_command(self):
        extension, manager = self.manager(real_server_config())
        wait_for(
            lambda: root_node(manager.snapshot(), "real-fixture")["state"] == "active"
        )
        snapshot = manager.snapshot()
        actions = {action["id"]: action for action in snapshot["actions"]}
        self.assertEqual(actions["refresh:real-fixture"]["command"], "mcp")
        self.assertEqual(
            actions["restart:real-fixture"]["arguments"],
            ["restart", "real-fixture"],
        )
        response = manager.execute_command(["stop", "real-fixture"])
        self.assertIn("requested", response["text"])
        wait_for(
            lambda: root_node(manager.snapshot(), "real-fixture")["state"] == "stopped"
        )


if __name__ == "__main__":
    unittest.main()
