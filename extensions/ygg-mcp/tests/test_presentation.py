from __future__ import annotations

import json
import unittest

from .helpers import FIXTURES


PRESENTATION = FIXTURES / "presentation"
STATES = {
    "empty",
    "loading",
    "pending",
    "active",
    "running",
    "succeeded",
    "failed",
    "cancelled",
    "degraded",
    "stopped",
    "unavailable",
}


def load(name):
    return json.loads((PRESENTATION / name).read_text(encoding="utf-8"))


def validate_generic(snapshot):
    assert set(snapshot) <= {"revision", "status", "activities", "collection", "actions"}
    assert isinstance(snapshot["revision"], int) and snapshot["revision"] >= 0
    status = snapshot.get("status")
    if status is not None:
        assert set(status) <= {"state", "label", "detail"}
        assert status["state"] in STATES
        assert isinstance(status["label"], str)
    activities = snapshot.get("activities", [])
    assert len(activities) <= 128
    for activity in activities:
        assert set(activity) <= {
            "id",
            "kind",
            "state",
            "summary",
            "provenance",
            "started_at_ms",
            "completed_at_ms",
            "references",
        }
        assert activity["state"] in STATES
        assert activity["kind"] == "mcp_tool_call"
    collection = snapshot.get("collection")
    if collection is not None:
        assert collection["kind"] in {"list", "tree"}
        assert len(collection["nodes"]) <= 256
        ids = {node["id"] for node in collection["nodes"]}
        for node in collection["nodes"]:
            assert node["state"] in STATES
            if "parent_id" in node:
                assert node["parent_id"] in ids
    actions = snapshot.get("actions", [])
    assert len(actions) <= 64
    action_ids = {action["id"] for action in actions}
    assert all(action["command"] == "mcp" for action in actions)
    for node in (collection or {}).get("nodes", []):
        assert set(node.get("action_ids", [])).issubset(action_ids)


class PresentationFixtureTests(unittest.TestCase):
    def test_all_extension_snapshots_use_the_exact_generic_contract(self):
        names = [
            "empty.json",
            "connecting.json",
            "ready.json",
            "refreshing.json",
            "degraded.json",
            "parked.json",
            "restarted.json",
            "activity-running.json",
            "activity-success.json",
            "activity-failed.json",
            "activity-cancelled.json",
            "activity-ambiguous.json",
        ]
        for name in names:
            with self.subTest(name=name):
                validate_generic(load(name))

    def test_lifecycle_fixtures_cover_required_tui_and_serve_states(self):
        expected = {
            "empty.json": "empty",
            "connecting.json": "loading",
            "ready.json": "active",
            "refreshing.json": "running",
            "degraded.json": "degraded",
            "parked.json": "degraded",
            "restarted.json": "active",
        }
        for name, state in expected.items():
            self.assertEqual(load(name)["status"]["state"], state)
        outcomes = {
            "activity-running.json": "running",
            "activity-success.json": "succeeded",
            "activity-failed.json": "failed",
            "activity-cancelled.json": "cancelled",
            "activity-ambiguous.json": "degraded",
        }
        for name, state in outcomes.items():
            self.assertEqual(load(name)["activities"][-1]["state"], state)

    def test_server_tool_tree_detail_and_actions_are_safe_semantics(self):
        ready = load("ready.json")
        nodes = ready["collection"]["nodes"]
        server = next(node for node in nodes if node["id"] == "server:fixture")
        tools = [node for node in nodes if node.get("parent_id") == server["id"]]
        self.assertEqual(len(tools), 2)
        self.assertIn("parameters", tools[0]["secondary"])
        self.assertTrue(any("readOnly" in tool["secondary"] for tool in tools))
        detail = ready["collection"]["detail"]
        self.assertIn("Lifecycle: ready", detail["body"])
        self.assertIn("Transport: stdio", detail["body"])
        self.assertIn("Catalog revision: 3", detail["body"])
        self.assertEqual(
            {action["arguments"][0] for action in ready["actions"]},
            {"refresh", "restart", "stop"},
        )

    def test_reconnect_resync_is_a_complete_official_serve_projection(self):
        reconnect = load("reconnect-resync.json")
        self.assertEqual(reconnect["extension"], "ygg-mcp")
        self.assertGreater(reconnect["generation"], 0)
        validate_generic(reconnect["snapshot"])
        # Reapplying a complete snapshot is idempotent and carries no command.
        rebuilt = json.loads(json.dumps(reconnect["snapshot"]))
        self.assertEqual(rebuilt, reconnect["snapshot"])
        self.assertNotIn("command", reconnect["snapshot"])

    def test_stale_generation_removal_cannot_remove_replacement(self):
        fixture = load("stale-generation-removal.json")
        current = fixture["before"]
        current = fixture["replacement"]
        stale_generation = 6
        current = [
            item
            for item in current
            if not (
                item["extension"] == "ygg-mcp"
                and item["generation"] == stale_generation
            )
        ]
        self.assertEqual(current, fixture["afterStaleGenerationRemoval"])
        self.assertEqual(current[0]["generation"], 7)
        validate_generic(current[0]["snapshot"])

    def test_fixtures_never_carry_launch_arguments_environment_or_untrusted_description(self):
        for path in PRESENTATION.glob("*.json"):
            text = path.read_text(encoding="utf-8")
            self.assertNotIn("command =", text)
            self.assertNotIn("SECRET", text)
            self.assertNotIn("server-provided description", text)
            self.assertNotIn("/absolute/", text)


if __name__ == "__main__":
    unittest.main()
