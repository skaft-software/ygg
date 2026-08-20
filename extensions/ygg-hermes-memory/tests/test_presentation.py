from __future__ import annotations

import json
from pathlib import Path
import time
import unittest

from .helpers import FakeExtension, load_fixture_config, mock_descriptor, owner_context, temporary_directory
from ygg_hermes_memory.config import BridgeConfig
from ygg_hermes_memory.constants import GENERIC_PRESENTATION_STATES
from ygg_hermes_memory.manager import MemoryBridge
from ygg_hermes_memory.presentation import build_presentation


ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / "fixtures" / "presentation"


class PresentationTests(unittest.TestCase):
    def test_all_frontend_neutral_fixtures_follow_generic_contract(self):
        expected = {
            "off-discovery.json",
            "selected.json",
            "switched.json",
            "read-success.json",
            "write-queued.json",
            "write-committed.json",
            "write-failed.json",
            "sync-queued.json",
            "sync-succeeded.json",
            "degraded-redacted.json",
            "reconnect-no-replay.json",
            "stale-generation.json",
            "owner-isolation.json",
        }
        self.assertEqual({path.name for path in FIXTURES.glob("*.json")}, expected)
        for path in sorted(FIXTURES.glob("*.json")):
            with self.subTest(path=path.name):
                snapshot = json.loads(path.read_text(encoding="utf-8"))
                self.assertEqual(
                    set(snapshot),
                    {"revision", "status", "activities", "collection", "actions"},
                )
                self.assertIn(snapshot["status"]["state"], GENERIC_PRESENTATION_STATES)
                self.assertEqual(snapshot["collection"]["kind"], "list")
                nodes = snapshot["collection"]["nodes"]
                node_ids = {item["id"] for item in nodes}
                self.assertIn("off", node_ids)
                self.assertIn(snapshot["collection"]["selected_node_id"], node_ids)
                action_ids = {item["id"] for item in snapshot["actions"]}
                self.assertTrue(
                    all(set(node.get("action_ids", [])).issubset(action_ids) for node in nodes)
                )
                for action in snapshot["actions"]:
                    self.assertEqual(action["command"], "memory")
                for activity in snapshot["activities"]:
                    self.assertIn(activity["state"], GENERIC_PRESENTATION_STATES)
                    self.assertNotIn("content", activity)
                encoded = json.dumps(snapshot).lower()
                for forbidden in (
                    "hunter2",
                    "sk-",
                    "bearer ",
                    "/home/",
                    "/users/",
                    "private memory text",
                    "embedding",
                ):
                    self.assertNotIn(forbidden, encoded)
                self.assertLess(len(json.dumps(snapshot).encode("utf-8")), 256 * 1024)

    def test_write_provenance_never_claims_commit_while_queued_or_failed(self):
        queued = json.loads((FIXTURES / "write-queued.json").read_text())
        committed = json.loads((FIXTURES / "write-committed.json").read_text())
        failed = json.loads((FIXTURES / "write-failed.json").read_text())
        self.assertEqual(queued["activities"][0]["state"], "pending")
        self.assertNotIn("committed", queued["activities"][0]["provenance"])
        self.assertEqual(committed["activities"][0]["state"], "succeeded")
        self.assertIn("committed", committed["activities"][0]["provenance"])
        self.assertEqual(failed["activities"][0]["state"], "failed")

    def test_sync_provenance_reports_queue_and_provider_acceptance(self):
        queued = json.loads((FIXTURES / "sync-queued.json").read_text())
        succeeded = json.loads((FIXTURES / "sync-succeeded.json").read_text())
        self.assertEqual(queued["activities"][0]["kind"], "memory_sync")
        self.assertEqual(queued["activities"][0]["state"], "pending")
        self.assertIn("queue 1", queued["activities"][0]["provenance"])
        self.assertEqual(succeeded["activities"][0]["state"], "succeeded")
        self.assertIn("provider accepted", succeeded["activities"][0]["provenance"])
        self.assertNotIn("committed", succeeded["activities"][0]["provenance"])

    def test_reconnect_retains_provenance_without_repeating_activity(self):
        before = json.loads((FIXTURES / "read-success.json").read_text())
        reconnect = json.loads((FIXTURES / "reconnect-no-replay.json").read_text())
        self.assertGreater(reconnect["revision"], before["revision"])
        self.assertEqual(reconnect["activities"], before["activities"])
        self.assertEqual(reconnect["collection"]["nodes"], before["collection"]["nodes"])

    def test_owner_isolation_uses_opaque_distinct_resource_references(self):
        first = json.loads((FIXTURES / "read-success.json").read_text())
        second = json.loads((FIXTURES / "owner-isolation.json").read_text())
        first_ref = first["activities"][0]["references"][0]["id"]
        second_ref = second["activities"][0]["references"][0]["id"]
        self.assertNotEqual(first_ref, second_ref)
        self.assertNotIn("session", first_ref)
        self.assertNotIn("session", second_ref)

    def test_invalid_configuration_is_unavailable_but_direct_product_remains_usable(self):
        extension = FakeExtension()
        bridge = MemoryBridge(
            extension,
            BridgeConfig.empty(),
            config_error_code="invalid_config",
        )
        bridge.start({"host": {"session_id": "invalid"}})
        context = owner_context("invalid")
        status = bridge.status_contribution(context, "status")
        self.assertEqual(status["text"], "memory unavailable")
        self.assertEqual(bridge.presentation_snapshot()["status"]["state"], "unavailable")
        self.assertEqual(bridge.collect_context({"prompt": "direct coding"}, context), [])
        bridge.shutdown()

    def test_snapshot_and_headless_fallback_are_side_effect_free(self):
        with temporary_directory() as directory:
            config = load_fixture_config(directory, providers=[mock_descriptor()])
            extension = FakeExtension()
            bridge = MemoryBridge(extension, config)
            bridge.start({"host": {"session_id": "presentation"}})
            context = owner_context("presentation")
            candidate = bridge._discovery.by_id("directory:mock")
            bridge.execute_command(["trust", candidate.id, candidate.fingerprint], context)
            bridge.execute_command(["select", candidate.id], context)
            owner = bridge.owner_for_context(context)
            provider = owner.provider.provider
            events_before = list(provider.events)
            first = bridge.presentation_snapshot(owner)
            second = bridge.presentation_snapshot(owner)
            fallback = bridge.execute_command(["snapshot"], context)["text"]
            self.assertEqual(first, second)
            self.assertEqual(json.loads(fallback), first)
            self.assertEqual(provider.events, events_before)
            self.assertNotIn("Static memory context", fallback)
            self.assertNotIn("provider-owned", fallback)
            bridge.shutdown()

    def test_background_snapshots_are_scoped_to_each_complete_owner_triple(self):
        with temporary_directory() as directory:
            config = load_fixture_config(directory, providers=[mock_descriptor()])
            extension = FakeExtension()
            bridge = MemoryBridge(extension, config)
            bridge.start({"host": {"session_id": "display-a"}})
            first_context = owner_context("owner-a", generation=1)
            second_context = owner_context("owner-b", generation=1)
            first = bridge.owner_for_context(first_context)
            second = bridge.owner_for_context(second_context)
            bridge._add_activity(
                first,
                "memory_read",
                "succeeded",
                "Memory read",
                "Owner A metadata only",
                terminal=True,
            )
            bridge._add_activity(
                second,
                "memory_write",
                "pending",
                "Memory write queued",
                "Owner B metadata only",
                terminal=False,
            )
            bridge._changed(first)
            bridge._changed(second)
            time.sleep(0.12)
            scoped = [item for item in extension.presentations if item["resource_owner"]]
            self.assertGreaterEqual(len(scoped), 2)
            sessions = {item["resource_owner"]["session_id"] for item in scoped}
            self.assertIn("owner-a", sessions)
            self.assertIn("owner-b", sessions)
            for item in scoped:
                encoded = json.dumps(item["snapshot"])
                session = item["resource_owner"]["session_id"]
                if session == "owner-a":
                    self.assertNotIn("Owner B", encoded)
                if session == "owner-b":
                    self.assertNotIn("Owner A", encoded)
            bridge.shutdown()

    def test_rapid_state_changes_coalesce_below_host_update_ceiling(self):
        extension = FakeExtension()
        bridge = MemoryBridge(extension, BridgeConfig.empty())
        bridge.start({"host": {"session_id": "rate"}})
        for _ in range(100):
            bridge._changed()
        time.sleep(0.08)
        self.assertLessEqual(len(extension.presentations), 4)
        self.assertEqual(extension.presentations[-1]["snapshot"]["revision"], bridge._revision)
        bridge.shutdown()

    def test_maximum_picker_filters_action_references_with_host_limits(self):
        providers = []
        for index in range(64):
            providers.append(
                {
                    "id": f"entrypoint:provider-{index}",
                    "name": f"provider-{index}",
                    "label": f"Provider {index}",
                    "version": "1.0.0",
                    "source": "entrypoint",
                    "fingerprint": f"{index:064x}",
                    "contract": "contract",
                    "environment": "env",
                    "network": "unknown",
                    "storage": "unknown",
                    "setup": "unknown",
                    "availability": "discoverable",
                    "trusted": False,
                }
            )
        snapshot = build_presentation(
            revision=1,
            discovery={
                "providers": providers,
                "environment": "env",
                "environmentState": "compatible",
                "contractVersion": "contract",
            },
            owner={
                "selectedId": None,
                "inspectedId": "off",
                "state": "off",
                "activities": [],
                "measurements": {},
            },
        )
        self.assertLessEqual(len(snapshot["actions"]), 64)
        retained = {action["id"] for action in snapshot["actions"]}
        self.assertTrue(
            all(
                set(node.get("action_ids", [])).issubset(retained)
                for node in snapshot["collection"]["nodes"]
            )
        )

    def test_picker_actions_are_literal_declared_memory_commands(self):
        snapshot = json.loads((FIXTURES / "off-discovery.json").read_text())
        trust = next(action for action in snapshot["actions"] if action["id"].startswith("trust:"))
        self.assertTrue(trust["destructive"])
        self.assertEqual(trust["arguments"][:2], ["trust", "directory:offline"])
        self.assertEqual(len(trust["arguments"][2]), 64)


if __name__ == "__main__":
    unittest.main()
