from __future__ import annotations

import json
import unittest

try:
    from .helpers import FIXTURES, owner
except ImportError:
    from helpers import FIXTURES, owner

from fake_agent_sessions import FakeHostState, ManualClock
from ygg_subagents.model import Worker
from ygg_subagents.orchestrator import Orchestrator
from ygg_subagents.presentation import build_snapshot


class PresentationTests(unittest.TestCase):
    def worker(self, state: str = "running", **values):
        defaults = dict(
            agent_id="agent-1",
            agent_path="/root/fixture",
            parent_id="root",
            depth=1,
            name="fixture-worker",
            profile="explore",
            requested_model="inherit",
            effective_model="claude-sonnet-test",
            tools=("read", "search"),
            state=state,
            phase="searching",
            created_at_ms=1_700_000_000_000,
            started_at_ms=1_700_000_000_000,
            deadline_at_ms=1_700_000_300_000,
            timeout_seconds=300,
            max_turns=8,
            max_output_bytes=8192,
            max_tokens=None,
            max_cost_microdollars=200000,
            session="/sessions/fixture.jsonl",
            generation=1,
        )
        defaults.update(values)
        return Worker(**defaults)

    def test_tree_is_content_free_while_detail_carries_terminal_summary(self):
        worker = self.worker(
            "done",
            phase="completed",
            completed_at_ms=1_700_000_068_000,
            summary="Exact child prose that belongs only in selected detail.",
            turn_count=3,
            tokens_used=1400,
            cost_microdollars=1800,
        )
        snapshot = build_snapshot(
            [worker], selected_agent_id=worker.agent_id, now_ms=1_700_000_068_000
        )
        node = snapshot["collection"]["nodes"][0]
        activity = snapshot["activities"][0]
        detail = snapshot["collection"]["detail"]
        self.assertNotIn(worker.summary, json.dumps(node))
        self.assertNotIn(worker.summary, json.dumps(activity))
        self.assertIn(worker.summary, detail["body"])
        self.assertEqual(node["references"][0]["kind"], "session")
        self.assertEqual(detail["title"], "parent > fixture-worker")
        self.assertIn("read-only [read, search]", detail["body"])
        self.assertIn("shared", detail["body"].lower())
        self.assertIn("no session ceiling", detail["body"])

    def test_terminal_summary_controls_are_escaped_before_generic_presentation(self):
        worker = self.worker(
            "done",
            phase="completed",
            completed_at_ms=1_700_000_001_000,
            summary="result\\u001b[31m\\u0000tail",
        )
        # Simulate the production boundary's sanitized storage representation.
        from ygg_subagents.model import sanitize_document

        worker.summary = sanitize_document("result\x1b[31m\x00tail", 8192)
        snapshot = build_snapshot(
            [worker], selected_agent_id=worker.agent_id, now_ms=1_700_000_001_000
        )
        encoded = json.dumps(snapshot)
        self.assertNotIn("\x1b", snapshot["collection"]["detail"]["body"])
        self.assertIn("\\\\u001b[31m", encoded)
        self.assertIn("\\\\u0000", encoded)

    def test_every_internal_state_maps_to_a_generic_host_state(self):
        expected = {
            "queued": "pending",
            "running": "running",
            "waiting": "running",
            "stopping": "cancelled",
            "done": "succeeded",
            "failed": "failed",
            "stopped": "stopped",
            "timed_out": "failed",
            "cancelled": "cancelled",
            "orphaned": "unavailable",
            "restarted": "degraded",
        }
        for index, (state, generic) in enumerate(expected.items(), 1):
            with self.subTest(state=state):
                worker = self.worker(
                    state,
                    agent_id="agent-%d" % index,
                    agent_path="/root/worker-%d" % index,
                    name="worker-%d" % index,
                )
                snapshot = build_snapshot(
                    [worker], selected_agent_id=worker.agent_id, now_ms=1_700_000_010_000
                )
                self.assertEqual(snapshot["collection"]["nodes"][0]["state"], generic)

    def test_actions_route_only_to_declared_command_and_stop_is_destructive(self):
        worker = self.worker()
        snapshot = build_snapshot(
            [worker], selected_agent_id=worker.agent_id, now_ms=1_700_000_010_000
        )
        actions = {action["id"]: action for action in snapshot["actions"]}
        node = snapshot["collection"]["nodes"][0]
        self.assertEqual(set(node["action_ids"]), {"inspect:agent-1", "stop:agent-1"})
        self.assertTrue(actions["stop:agent-1"]["destructive"])
        self.assertEqual(actions["stop:agent-1"]["command"], "subagents")
        self.assertEqual(actions["stop-all"]["arguments"], ["stop", "all"])
        self.assertTrue(all(action["command"] == "subagents" for action in actions.values()))

    def test_parentage_uses_only_nodes_present_in_the_same_tree(self):
        root = self.worker(agent_id="agent-1", name="root-worker")
        child = self.worker(
            agent_id="agent-2",
            agent_path="/root/root-worker/child",
            parent_id="agent-1",
            depth=2,
            name="child-worker",
            state="orphaned",
        )
        snapshot = build_snapshot(
            [root, child], selected_agent_id=child.agent_id, now_ms=1_700_000_010_000
        )
        nodes = {node["id"]: node for node in snapshot["collection"]["nodes"]}
        self.assertNotIn("parent_id", nodes["worker:agent-1"])
        self.assertEqual(nodes["worker:agent-2"]["parent_id"], "worker:agent-1")

    def test_narrow_command_fixture_and_stop_fallback_fail_closed(self):
        clock = ManualClock()
        host = FakeHostState(clock)
        client = host.client()
        snapshots = []
        orchestrator = Orchestrator(publish=snapshots.append, now_ms=clock)
        current_owner = owner()
        result = orchestrator.spawn(
            client,
            current_owner,
            {"name": "explore-auth", "task": "Inspect auth."},
        )
        host.start(result["worker"]["id"])
        orchestrator.status(client, current_owner, {})
        listed = orchestrator.command([], {"host": {"session_id": "parent-session"}})
        self.assertIn("Subagents · 1 running", listed["text"])
        self.assertIn("/subagents inspect", listed["text"])
        stopped = orchestrator.command(
            ["stop", result["worker"]["id"]],
            {"host": {"session_id": "parent-session"}},
        )
        self.assertIn("Stop was not issued", stopped["text"])
        self.assertEqual(host.agents[result["worker"]["id"]].status["state"], "running")
        self.assertTrue(stopped["notifications"])

    def test_checked_in_presentation_fixtures_cover_live_tree_resync_and_inspection(self):
        live = json.loads(
            (FIXTURES / "presentation" / "live-tree.json").read_text(encoding="utf-8")
        )
        actions = {action["id"] for action in live["actions"]}
        nodes = {node["id"] for node in live["collection"]["nodes"]}
        self.assertEqual(live["collection"]["kind"], "tree")
        self.assertIn(live["collection"]["selected_node_id"], nodes)
        self.assertTrue(
            all(set(node.get("action_ids", [])).issubset(actions) for node in live["collection"]["nodes"])
        )
        self.assertNotIn("prompt", json.dumps(live).lower())
        reconnect = json.loads(
            (FIXTURES / "presentation" / "reconnect-resync.json").read_text(encoding="utf-8")
        )
        self.assertFalse(reconnect["stale_update"]["accepted"])
        self.assertEqual(reconnect["spawn_count_before"], reconnect["spawn_count_after"])
        self.assertEqual(reconnect["completion_delivery"]["duplicate_parent_turns"], 0)
        inspection = json.loads(
            (FIXTURES / "presentation" / "session-inspection.json").read_text(encoding="utf-8")
        )
        self.assertTrue(inspection["worker"]["read_only"])
        self.assertIsNone(inspection["worker"]["budget"]["tokens"]["limit"])
        self.assertEqual(
            inspection["worker"]["budget"]["tokens"]["source"], "inherited_parent"
        )
        self.assertFalse(inspection["parent_head_changed_by_inspection"])
        narrow = (FIXTURES / "presentation" / "narrow-terminal.txt").read_text(encoding="utf-8")
        self.assertIn("/subagents inspect", narrow)
        self.assertIn("subagent_stop", narrow)


if __name__ == "__main__":
    unittest.main()
