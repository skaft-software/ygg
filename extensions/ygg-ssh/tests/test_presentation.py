from __future__ import annotations

import json
from pathlib import Path
import unittest

from ygg_ssh.config import Target
from ygg_ssh.presentation import build_presentation

from .helpers import CONTEXT, OWNER_FENCE, ManagerHarness


FIXTURES = Path(__file__).resolve().parents[1] / "fixtures" / "presentation"
ALLOWED_STATES = {
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


class PresentationTests(unittest.TestCase):
    def test_disconnected_and_connected_markers_are_generic_and_high_authority(self):
        with ManagerHarness(authority="read-only") as harness:
            disconnected = harness.manager.presentation_snapshot(OWNER_FENCE)
            self.assertEqual(disconnected["status"]["state"], "stopped")
            action = disconnected["actions"][0]
            self.assertEqual(action["command"], "ssh")
            self.assertEqual(action["arguments"], ["connect", "fixture"])
            harness.connect()
            connected = harness.manager.presentation_snapshot(OWNER_FENCE)
            self.assertEqual(connected["status"]["state"], "active")
            label = connected["status"]["label"]
            self.assertIn("fixture-alias", label)
            self.assertIn("read-only", label)
            self.assertIn("gen 1", label)
            self.assertIn(str(harness.remote), label)

    def test_presentation_actions_cannot_enable_write_mode_or_invent_host(self):
        targets = [
            Target("alpha", "configured-alpha", "Alpha", "/srv/alpha", "read-write")
        ]
        snapshot = build_presentation(
            revision=1,
            targets=targets,
            connections=[],
            activities=[],
            config_source="configured",
        )
        for action in snapshot["actions"]:
            self.assertEqual(action["command"], "ssh")
            self.assertNotIn("write", action["arguments"])
            self.assertEqual(action["arguments"][-1], "alpha")
        self.assertNotIn("hostname", json.dumps(snapshot).lower())

    def test_activity_contains_only_remote_provenance_not_command_or_output(self):
        with ManagerHarness() as harness:
            harness.connect()
            harness.manager.execute(CONTEXT, ["printf", "SENSITIVE-COMMAND-ARG"])
            snapshot = harness.manager.presentation_snapshot(OWNER_FENCE)
            activity = snapshot["activities"][-1]
            self.assertEqual(activity["kind"], "ssh_remote_operation")
            self.assertIn("remote · fixture-alias · mutation", activity["provenance"])
            self.assertNotIn("SENSITIVE", json.dumps(activity))

    def test_snapshots_publish_monotonic_revisions(self):
        with ManagerHarness() as harness:
            harness.manager.activate_presentation()
            harness.connect()
            harness.manager.read_file(CONTEXT, "missing") if False else None
            revisions = [snapshot["revision"] for snapshot in harness.snapshots]
            self.assertGreaterEqual(len(revisions), 2)
            self.assertEqual(revisions, sorted(set(revisions)))

    def test_all_package_fixtures_use_bounded_generic_contract(self):
        self.assertTrue(FIXTURES.is_dir())
        paths = sorted(FIXTURES.glob("*.json"))
        self.assertGreaterEqual(len(paths), 10)
        for path in paths:
            with self.subTest(path=path.name):
                value = json.loads(path.read_text(encoding="utf-8"))
                snapshots = _snapshots(value)
                self.assertTrue(snapshots)
                for snapshot in snapshots:
                    _validate_snapshot(snapshot)

    def test_scenarios_cover_required_tui_serve_and_headless_states(self):
        names = {path.stem for path in FIXTURES.glob("*.json")}
        self.assertTrue(
            {
                "disconnected",
                "connecting",
                "read-only",
                "read-write",
                "degraded",
                "activity-read",
                "activity-mutation",
                "cancelled",
                "ambiguous-disconnect",
                "reconnect-resync",
                "stale-generation-removal",
            }.issubset(names)
        )


def _snapshots(value: object) -> list[dict]:
    results = []
    if isinstance(value, dict):
        if {"revision", "status", "activities", "collection", "actions"}.issubset(value):
            results.append(value)
        for child in value.values():
            results.extend(_snapshots(child))
    elif isinstance(value, list):
        for child in value:
            results.extend(_snapshots(child))
    return results


def _validate_snapshot(snapshot: dict) -> None:
    self_keys = {"revision", "status", "activities", "collection", "actions"}
    if set(snapshot) != self_keys:
        raise AssertionError(f"unexpected snapshot fields: {set(snapshot) - self_keys}")
    if not isinstance(snapshot["revision"], int) or snapshot["revision"] < 0:
        raise AssertionError("invalid revision")
    if snapshot["status"]["state"] not in ALLOWED_STATES:
        raise AssertionError("invalid status state")
    if len(snapshot["activities"]) > 128 or len(snapshot["actions"]) > 64:
        raise AssertionError("presentation count bound exceeded")
    collection = snapshot["collection"]
    if len(collection["nodes"]) > 256:
        raise AssertionError("presentation node bound exceeded")
    action_ids = {action["id"] for action in snapshot["actions"]}
    for action in snapshot["actions"]:
        if action["command"] != "ssh" or len(action["arguments"]) > 32:
            raise AssertionError("unsafe action routing")
    for node in collection["nodes"]:
        if node["state"] not in ALLOWED_STATES:
            raise AssertionError("invalid node state")
        if not set(node.get("action_ids", [])).issubset(action_ids):
            raise AssertionError("node references missing action")
    encoded = json.dumps(snapshot, ensure_ascii=False).encode("utf-8")
    if len(encoded) > 256 * 1024 or b"\x1b" in encoded:
        raise AssertionError("snapshot bytes/control bound exceeded")


if __name__ == "__main__":
    unittest.main()
