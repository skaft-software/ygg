from __future__ import annotations

import json
import os
import time
import unittest

from .helpers import FakeExtension, load_fixture_config, mock_descriptor, owner_context, temporary_directory, wait_until
from .test_loader_manager import fixture_mode
from ygg_hermes_memory.manager import MemoryBridge


def active_bridge(directory, *, limits=None):
    config = load_fixture_config(directory, providers=[mock_descriptor()], limits=limits or {})
    extension = FakeExtension()
    bridge = MemoryBridge(extension, config)
    bridge.start({"host": {"session_id": "lifecycle"}})
    context = owner_context("lifecycle")
    candidate = bridge._discovery.by_id("directory:mock")
    bridge.execute_command(["trust", candidate.id, candidate.fingerprint], context)
    bridge.execute_command(["select", candidate.id], context)
    return bridge, extension, context


class LifecycleTests(unittest.TestCase):
    def test_successful_turn_maps_user_assistant_sync_and_next_prefetch(self):
        with temporary_directory() as directory:
            bridge, _, context = active_bridge(directory)
            owner = bridge.owner_for_context(context)
            provider = owner.provider.provider
            bridge.before_prompt({"prompt": "user secret token=abcdef123456"}, context)
            bridge.collect_context({"prompt": "user secret token=abcdef123456"}, context)
            bridge.after_response({"response": "assistant answer password=hidden-value"}, context)
            bridge.lifecycle(
                "turn/settled",
                {"session_id": "lifecycle", "turn_id": "turn-1", "outcome": "completed"},
            )
            self.assertTrue(wait_until(lambda: owner.queue_depth == 0))
            sync = [item for item in provider.events if item["event"] == "sync_turn"]
            self.assertEqual(len(sync), 1)
            self.assertNotIn("abcdef123456", sync[0]["user"])
            self.assertNotIn("hidden-value", sync[0]["assistant"])
            self.assertEqual(sync[0]["session_id"], "lifecycle")
            self.assertTrue(any(item["event"] == "queue_prefetch" for item in provider.events))
            self.assertEqual(owner.last_sync["outcome"], "accepted")
            bridge.shutdown()

    def test_lifecycle_display_session_aliases_full_resource_owner(self):
        with temporary_directory() as directory:
            config = load_fixture_config(directory, providers=[mock_descriptor()])
            extension = FakeExtension()
            bridge = MemoryBridge(extension, config)
            bridge.start({"host": {"session_id": "display-session"}})
            context = dict(owner_context("durable-resource-owner"))
            context["host"] = {"session_id": "display-session", "active_skills": []}
            candidate = bridge._discovery.by_id("directory:mock")
            bridge.execute_command(["trust", candidate.id, candidate.fingerprint], context)
            bridge.execute_command(["select", candidate.id], context)
            owner = bridge.owner_for_context(context)
            self.assertEqual(owner.session_id, "durable-resource-owner")
            bridge.before_prompt({"prompt": "aliased"}, context)
            bridge.after_response({"response": "response"}, context)
            bridge.lifecycle(
                "turn/settled",
                {"session_id": "display-session", "outcome": "completed"},
            )
            self.assertIs(bridge.owner_for_context(context), owner)
            self.assertFalse(owner.turn_open)
            self.assertTrue(wait_until(lambda: owner.queue_depth == 0))
            bridge.shutdown()

    def test_failed_cancelled_turn_never_invents_or_syncs_assistant_text(self):
        with temporary_directory() as directory:
            bridge, _, context = active_bridge(directory)
            owner = bridge.owner_for_context(context)
            provider = owner.provider.provider
            bridge.before_prompt({"prompt": "will fail"}, context)
            bridge.lifecycle(
                "turn/settled",
                {"session_id": "lifecycle", "turn_id": "turn-2", "outcome": "cancelled"},
            )
            self.assertFalse(any(item["event"] == "sync_turn" for item in provider.events))
            self.assertFalse(owner.turn_open)
            self.assertEqual(owner.activities[-1].state, "cancelled")
            bridge.shutdown()

    def test_session_settlement_calls_bounded_end_hook_then_shutdown(self):
        with temporary_directory() as directory:
            bridge, extension, context = active_bridge(directory)
            owner = bridge.owner_for_context(context)
            provider = owner.provider.provider
            bridge.before_prompt({"prompt": "hello"}, context)
            bridge.after_response({"response": "world"}, context)
            self.assertTrue(wait_until(lambda: owner.queue_depth == 0))
            bridge.lifecycle(
                "session/settled",
                {"session_id": "lifecycle", "outcome": "completed"},
            )
            self.assertTrue(wait_until(lambda: owner.provider is None))
            self.assertTrue(wait_until(lambda: owner.state == "stopped"))
            names = [item["event"] for item in provider.events]
            self.assertIn("on_session_end", names)
            self.assertIn("shutdown", names)
            self.assertLess(names.index("on_session_end"), names.index("shutdown"))
            self.assertEqual(owner.state, "stopped")
            self.assertEqual(list(owner.messages), [])
            self.assertEqual(owner.user_text, "")
            self.assertEqual(extension.tools, {})
            bridge.shutdown()

    def test_session_settlement_bypasses_a_full_background_queue(self):
        with fixture_mode("slow-sync"):
            with temporary_directory() as directory:
                bridge, _, context = active_bridge(
                    directory, limits={"syncTimeoutMs": 200, "maxQueueDepth": 1}
                )
                owner = bridge.owner_for_context(context)
                provider = owner.provider.provider
                bridge.before_prompt({"prompt": "queued"}, context)
                bridge.after_response({"response": "slow response"}, context)
                self.assertEqual(owner.queue_depth, 1)

                bridge.lifecycle(
                    "session/settled",
                    {"session_id": "lifecycle", "outcome": "completed"},
                )
                self.assertIsNone(owner.provider)
                self.assertTrue(wait_until(lambda: owner.state == "stopped"))
                names = [item["event"] for item in provider.events]
                self.assertIn("on_session_end", names)
                self.assertIn("shutdown", names)
                self.assertLess(names.index("on_session_end"), names.index("shutdown"))
                bridge.shutdown()

    def test_committed_builtin_memory_write_maps_on_memory_write_only_after_success(self):
        with temporary_directory() as directory:
            bridge, _, context = active_bridge(directory)
            owner = bridge.owner_for_context(context)
            provider = owner.provider.provider
            payload = {
                "name": "memory",
                "arguments": {"action": "add", "target": "memory", "content": "fact token=secret-value"},
                "output": json.dumps({"success": True, "staged": False}),
                "is_error": False,
            }
            bridge.after_tool_call(payload, context)
            self.assertTrue(wait_until(lambda: owner.queue_depth == 0))
            writes = [item for item in provider.events if item["event"] == "on_memory_write"]
            self.assertEqual(len(writes), 1)
            self.assertNotIn("secret-value", writes[0]["content"])
            self.assertTrue(writes[0]["has_metadata"])
            self.assertEqual(owner.activities[-1].state, "degraded")
            self.assertIn("durability unreported", owner.activities[-1].summary.lower())

            payload["output"] = json.dumps({"success": False})
            bridge.after_tool_call(payload, context)
            self.assertEqual(len([item for item in provider.events if item["event"] == "on_memory_write"]), 1)
            bridge.shutdown()

    def test_slow_failing_sync_is_bounded_and_health_is_degraded(self):
        with fixture_mode("slow-sync"):
            with temporary_directory() as directory:
                bridge, _, context = active_bridge(
                    directory, limits={"syncTimeoutMs": 30, "maxQueueDepth": 2}
                )
                owner = bridge.owner_for_context(context)
                bridge.before_prompt({"prompt": "user"}, context)
                started = time.monotonic()
                bridge.after_response({"response": "assistant"}, context)
                self.assertLess(time.monotonic() - started, 0.2)
                self.assertTrue(wait_until(lambda: owner.queue_depth == 0))
                self.assertEqual(owner.last_sync["outcome"], "timeout")
                self.assertEqual(owner.state, "degraded")
                presentation = bridge.presentation_snapshot(owner)
                self.assertEqual(presentation["status"]["state"], "degraded")
                bridge.shutdown()

    def test_queue_depth_is_bounded_and_late_work_fails_soft(self):
        with fixture_mode("slow-sync"):
            with temporary_directory() as directory:
                bridge, _, context = active_bridge(
                    directory, limits={"syncTimeoutMs": 200, "maxQueueDepth": 1}
                )
                owner = bridge.owner_for_context(context)
                bridge.before_prompt({"prompt": "first"}, context)
                bridge.after_response({"response": "first response"}, context)
                # A second completed response cannot grow this owner's queue.
                owner.turn_synced = False
                owner.assistant_text = "second response"
                bridge._queue_turn_sync(owner)
                self.assertLessEqual(owner.queue_depth, 1)
                self.assertEqual(owner.state, "degraded")
                self.assertEqual(owner.last_error_code, "background_queue_full")
                self.assertTrue(any("rejected" in item.summary.lower() for item in owner.activities))
                bridge.shutdown()

    def test_optional_hooks_without_api_boundary_are_reported_not_invoked(self):
        with temporary_directory() as directory:
            bridge, _, context = active_bridge(directory)
            owner = bridge.owner_for_context(context)
            report = bridge.execute_command(["lifecycle"], context)["text"]
            self.assertIn("on_pre_compress:no_api_boundary", report)
            self.assertIn("on_delegation:no_api_boundary", report)
            self.assertFalse(any(item["event"] == "on_pre_compress" for item in owner.provider.provider.events))
            detail = bridge.execute_command(["show", "directory:mock"], context)["text"]
            self.assertIn("Unsupported/no equivalent", detail)
            self.assertIn("hermes-memory.mock-memory.prefetch", detail)
            self.assertIn("remember_mock", detail)
            bridge.shutdown()

    def test_shutdown_cancels_background_work_and_never_waits_for_slow_provider(self):
        with fixture_mode("slow-sync"):
            with temporary_directory() as directory:
                bridge, _, context = active_bridge(
                    directory, limits={"syncTimeoutMs": 1000, "shutdownTimeoutMs": 50}
                )
                bridge.before_prompt({"prompt": "user"}, context)
                bridge.after_response({"response": "assistant"}, context)
                started = time.monotonic()
                bridge.shutdown()
                self.assertLess(time.monotonic() - started, 0.5)
                owner = bridge.owner_for_context(context)
                self.assertEqual(owner.state, "stopped")
                self.assertEqual(owner.queue_depth, 0)

    def test_multiple_slow_provider_shutdowns_share_one_total_deadline(self):
        with fixture_mode("slow-shutdown"):
            with temporary_directory() as directory:
                bridge, _, first_context = active_bridge(
                    directory, limits={"shutdownTimeoutMs": 60}
                )
                candidate = bridge._discovery.by_id("directory:mock")
                second_context = owner_context("second-owner")
                bridge.execute_command(["select", candidate.id], second_context)
                started = time.monotonic()
                bridge.shutdown()
                self.assertLess(time.monotonic() - started, 0.3)
                self.assertEqual(bridge.owner_for_context(first_context).state, "stopped")
                self.assertEqual(bridge.owner_for_context(second_context).state, "stopped")

    def test_status_exposes_only_safe_operational_measurements(self):
        with temporary_directory() as directory:
            bridge, _, context = active_bridge(directory)
            owner = bridge.owner_for_context(context)
            view = bridge._owner_view(owner)
            self.assertGreaterEqual(view["measurements"]["cpuSeconds"], 0)
            self.assertGreaterEqual(view["measurements"]["rssKiB"], 0)
            text = bridge.execute_command(["status"], context)["text"]
            self.assertNotIn("/home/", text)
            self.assertNotIn("password", text.lower())
            bridge.shutdown()


if __name__ == "__main__":
    unittest.main()
