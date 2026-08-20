from __future__ import annotations

import unittest

try:
    from .helpers import (
        RunningExtension,
        initialize_request,
        rpc_request,
        tool_context,
    )
except ImportError:
    from helpers import RunningExtension, initialize_request, rpc_request, tool_context

from fake_agent_sessions import FakeHostState, fake_session_reference
from ygg_subagents.runtime import create_runtime


class ServiceResponder:
    def __init__(self) -> None:
        self.host = FakeHostState()
        self.client = self.host.client()
        self.reverse = []
        self.ignore_wait = False

    def __call__(self, message):
        method = message.get("method")
        if not isinstance(method, str) or not method.startswith("agent/"):
            return None
        self.reverse.append(message)
        params = dict(message.get("params", {}))
        params.pop("parent_request_id", None)
        if method == "agent/spawn":
            policy = params.pop("policy")
            result = self.client.spawn_agent(**params, **policy)
        elif method == "agent/list":
            result = self.client.list_agents()
        elif method == "agent/wait":
            if self.ignore_wait:
                return None
            result = self.client.wait_agents(**params)
        elif method == "agent/interrupt":
            result = self.client.interrupt_agent(params["target"])
        else:
            raise AssertionError("unexpected reverse method %s" % method)
        return {"jsonrpc": "2.0", "id": message["id"], "result": result}


class RuntimeProtocolTests(unittest.TestCase):
    def setUp(self):
        self.extension, self.orchestrator, self.publisher = create_runtime()
        self.responder = ServiceResponder()
        self.running = RunningExtension(self.extension, self.responder)

    def tearDown(self):
        if self.running.thread.is_alive():
            self.running.shutdown()

    def test_negotiates_only_bounded_features_and_exact_manifest_surface(self):
        initialized = self.running.start()
        result = initialized["result"]
        self.assertEqual(result["api_version"], "0.2")
        self.assertEqual(
            [tool["name"] for tool in result["tools"]],
            ["subagent_spawn", "subagent_status", "subagent_wait", "subagent_stop"],
        )
        self.assertEqual([command["name"] for command in result["commands"]], ["subagents"])
        self.assertEqual(
            result["protocol"]["features"],
            [
                "request_cancellation",
                "content_parts",
                "lifecycle_events",
                "agent_sessions",
            ],
        )
        self.assertEqual(result["protocol"]["lifecycle_events"], ["session/settled"])
        self.assertEqual(result["protocol"]["limits"]["max_concurrent_requests"], 4)

    def test_spawn_is_owner_correlated_idempotent_and_publishes_tree(self):
        self.running.start()
        arguments = {
            "name": "inspect-auth",
            "task": "Inspect authentication ownership.",
            "idempotency_key": "protocol-auth-v1",
        }
        self.running.reader.feed(
            rpc_request(
                42,
                "tool/call",
                {
                    "name": "subagent_spawn",
                    "arguments": arguments,
                    "context": tool_context(),
                },
            )
        )
        response = self.running.writer.wait_for(lambda message: message.get("id") == 42)
        self.assertFalse(response["result"]["is_error"])
        text = response["result"]["content"][0]["text"]
        self.assertIn("Started background subagent", text)
        self.assertIn("agent-1", text)
        calls = [message for message in self.responder.reverse if message["method"] == "agent/spawn"]
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0]["params"]["parent_request_id"], 42)
        self.assertNotIn("resource_owner", calls[0]["params"])
        self.assertEqual(
            calls[0]["params"]["policy"],
            {
                "tools": ["read", "search"],
                "max_depth": 1,
                "max_concurrent_children": 2,
                "max_turns": 8,
                "max_tokens": 32000,
                "max_cost_microdollars": 200000,
                "max_output_bytes": 8192,
                "timeout_ms": 300000,
            },
        )
        self.assertIn("Use only these exact requested tools: read, search", calls[0]["params"]["message"])
        self.assertIn("Never use shell/process/bash, edit, write", calls[0]["params"]["message"])
        presentation = self.running.writer.wait_for(
            lambda message: message.get("method") == "presentation/update"
            and message.get("params", {}).get("snapshot", {}).get("collection", {}).get("nodes")
        )
        snapshot = presentation["params"]["snapshot"]
        self.assertEqual(
            presentation["params"]["resource_owner"],
            tool_context()["resource_owner"],
        )
        self.assertGreaterEqual(snapshot["revision"], 0)
        self.assertEqual(
            snapshot["collection"]["nodes"][0]["id"], "worker:agent-1"
        )
        self.assertEqual(
            snapshot["collection"]["nodes"][0]["references"][0]["kind"],
            "session",
        )

        self.running.reader.feed(
            rpc_request(
                43,
                "tool/call",
                {
                    "name": "subagent_spawn",
                    "arguments": arguments,
                    "context": tool_context(),
                },
            )
        )
        duplicate = self.running.writer.wait_for(lambda message: message.get("id") == 43)
        self.assertFalse(duplicate["result"]["is_error"])
        self.assertTrue(duplicate["result"]["metadata"]["duplicate"])
        self.assertEqual(
            len([call for call in self.responder.reverse if call["method"] == "agent/spawn"]),
            1,
        )

    def test_completion_wait_returns_exact_summary_usage_and_artifact_reference(self):
        self.running.start()
        self.running.reader.feed(
            rpc_request(
                10,
                "tool/call",
                {
                    "name": "subagent_spawn",
                    "arguments": {"name": "tests", "task": "Inspect tests."},
                    "context": tool_context(),
                },
            )
        )
        self.running.writer.wait_for(lambda message: message.get("id") == 10)
        self.responder.host.complete(
            "agent-1",
            "The owner tests cover duplicate spawn.",
            turns=4,
            artifacts=[{"artifact_id": "artifact-test", "label": "Test evidence"}],
        )
        self.running.reader.feed(
            rpc_request(
                11,
                "tool/call",
                {
                    "name": "subagent_wait",
                    "arguments": {"target": "agent-1", "timeout_seconds": 2},
                    "context": tool_context(),
                },
            )
        )
        response = self.running.writer.wait_for(lambda message: message.get("id") == 11)
        self.assertFalse(response["result"]["is_error"])
        text = response["result"]["content"][0]["text"]
        self.assertIn("The owner tests cover duplicate spawn.", text)
        worker = response["result"]["metadata"]["worker"]
        self.assertEqual(worker["turn_count"], 4)
        self.assertEqual(worker["artifacts"], [])
        self.assertEqual(worker["session"], fake_session_reference("agent-1"))
        reverse = [call for call in self.responder.reverse if call["params"]["parent_request_id"] == 11]
        self.assertTrue(reverse)
        self.assertTrue(all(call["method"] in {"agent/list", "agent/wait", "agent/interrupt"} for call in reverse))
        presentations = self.running.writer.matching(
            lambda message: message.get("method") == "presentation/update"
        )
        revisions = [message["params"]["snapshot"]["revision"] for message in presentations]
        self.assertEqual(revisions, sorted(set(revisions)))
        terminal = presentations[-1]["params"]["snapshot"]
        self.assertNotIn("The owner tests cover", terminal["collection"]["nodes"][0]["secondary"])
        self.assertIn("The owner tests cover", terminal["collection"]["detail"]["body"])

    def test_missing_agent_sessions_feature_fails_tool_without_reverse_request(self):
        initialized = self.running.start(initialize_request(agent_sessions=False))
        self.assertNotIn("agent_sessions", initialized["result"]["protocol"]["features"])
        self.running.reader.feed(
            rpc_request(
                20,
                "tool/call",
                {
                    "name": "subagent_status",
                    "arguments": {},
                    "context": tool_context(),
                },
            )
        )
        response = self.running.writer.wait_for(lambda message: message.get("id") == 20)
        self.assertTrue(response["result"]["is_error"])
        self.assertIn("did not offer API 0.2 agent_sessions", response["result"]["content"][0]["text"])
        self.assertEqual(self.responder.reverse, [])

    def test_owner_is_never_accepted_from_model_arguments_or_missing_context(self):
        self.running.start()
        self.running.reader.feed(
            rpc_request(
                30,
                "tool/call",
                {
                    "name": "subagent_spawn",
                    "arguments": {
                        "name": "bad-owner",
                        "task": "x",
                        "resource_owner": "model-supplied",
                    },
                    "context": tool_context(),
                },
            )
        )
        unknown = self.running.writer.wait_for(lambda message: message.get("id") == 30)
        self.assertTrue(unknown["result"]["is_error"])
        self.assertIn("unknown subagent_spawn fields", unknown["result"]["content"][0]["text"])
        self.running.reader.feed(
            rpc_request(
                31,
                "tool/call",
                {
                    "name": "subagent_status",
                    "arguments": {},
                    "context": {"workspace": "/workspace", "host": {}},
                },
            )
        )
        missing = self.running.writer.wait_for(lambda message: message.get("id") == 31)
        self.assertTrue(missing["result"]["is_error"])
        self.assertIn("host-derived resource owner", missing["result"]["content"][0]["text"])

    def test_headless_command_lists_and_inspects_but_stop_never_reuses_parent_id(self):
        self.running.start()
        self.running.reader.feed(
            rpc_request(
                40,
                "tool/call",
                {
                    "name": "subagent_spawn",
                    "arguments": {"name": "headless", "task": "Inspect fallback."},
                    "context": tool_context(),
                },
            )
        )
        self.running.writer.wait_for(lambda message: message.get("id") == 40)
        command_context = {
            "workspace": "/workspace",
            "host": {"session_id": "parent-session", "model": "claude-sonnet-test"},
        }
        self.running.reader.feed(
            rpc_request(
                41,
                "command/execute",
                {"name": "subagents", "arguments": [], "context": command_context},
            )
        )
        listed = self.running.writer.wait_for(lambda message: message.get("id") == 41)
        self.assertIn("Subagents · 1 queued", listed["result"]["text"])
        reverse_before = len(self.responder.reverse)
        self.running.reader.feed(
            rpc_request(
                42,
                "command/execute",
                {
                    "name": "subagents",
                    "arguments": ["stop", "agent-1"],
                    "context": command_context,
                },
            )
        )
        stopped = self.running.writer.wait_for(lambda message: message.get("id") == 42)
        self.assertIn("Stop was not issued", stopped["result"]["text"])
        self.assertEqual(len(self.responder.reverse), reverse_before)
        self.assertEqual(self.responder.host.agents["agent-1"].status["state"], "pending")

        # The generic host action route may attach an authenticated operation
        # owner. The extension then uses the ordinary owner-checked service;
        # headless command contexts without it remain read-only above.
        self.running.reader.feed(
            rpc_request(
                43,
                "command/execute",
                {
                    "name": "subagents",
                    "arguments": ["stop", "agent-1"],
                    "context": tool_context(),
                },
            )
        )
        authenticated = self.running.writer.wait_for(
            lambda message: message.get("id") == 43
        )
        self.assertIn("interrupt_requested=true", authenticated["result"]["text"])
        action_calls = [
            call
            for call in self.responder.reverse[reverse_before:]
            if call["params"].get("parent_request_id") == 43
        ]
        self.assertEqual(
            [call["method"] for call in action_calls],
            ["agent/list", "agent/interrupt"],
        )
        self.assertEqual(
            self.responder.host.agents["agent-1"].status["state"], "interrupted"
        )

    def test_wait_tool_cancellation_is_cooperative_and_child_survives(self):
        self.running.start()
        self.running.reader.feed(
            rpc_request(
                50,
                "tool/call",
                {
                    "name": "subagent_spawn",
                    "arguments": {"name": "long", "task": "Inspect slowly."},
                    "context": tool_context(),
                },
            )
        )
        self.running.writer.wait_for(lambda message: message.get("id") == 50)
        self.responder.host.start("agent-1")
        self.responder.ignore_wait = True
        self.running.reader.feed(
            rpc_request(
                51,
                "tool/call",
                {
                    "name": "subagent_wait",
                    "arguments": {"target": "agent-1", "timeout_seconds": 60},
                    "context": tool_context(),
                },
            )
        )
        self.running.writer.wait_for(
            lambda message: message.get("method") == "agent/wait"
            and message.get("params", {}).get("parent_request_id") == 51
        )
        self.running.reader.feed(
            {"jsonrpc": "2.0", "method": "$/cancelRequest", "params": {"id": 51, "reason": "user"}}
        )
        cancelled = self.running.writer.wait_for(lambda message: message.get("id") == 51)
        self.assertEqual(cancelled["error"]["code"], -32800)
        self.assertEqual(self.responder.host.agents["agent-1"].status["state"], "running")

    def test_session_settled_and_shutdown_are_bounded(self):
        self.running.start()
        self.running.reader.feed(
            rpc_request(
                60,
                "tool/call",
                {
                    "name": "subagent_spawn",
                    "arguments": {"name": "shutdown", "task": "Inspect cleanup."},
                    "context": tool_context(),
                },
            )
        )
        self.running.writer.wait_for(lambda message: message.get("id") == 60)
        self.running.reader.feed(
            {
                "jsonrpc": "2.0",
                "method": "session/settled",
                "params": {
                    "session_id": "parent-session",
                    "outcome": "shutdown",
                    "duration_ms": 10,
                },
            }
        )
        self.running.writer.wait_for(
            lambda message: message.get("method") == "presentation/update"
            and any(
                node.get("state") == "unavailable"
                for node in message.get("params", {})
                .get("snapshot", {})
                .get("collection", {})
                .get("nodes", [])
                if isinstance(node, dict)
            )
        )
        reply = self.running.shutdown()
        self.assertEqual(reply["result"], {})


if __name__ == "__main__":
    unittest.main()
