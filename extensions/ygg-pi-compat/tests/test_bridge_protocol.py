from __future__ import annotations

import hashlib
import unittest

try:
    from .helpers import BridgeProcess, NODE
except ImportError:  # unittest discovery imports this file as a top-level module.
    from helpers import BridgeProcess, NODE


@unittest.skipUnless(NODE, "node is required for the Pi compatibility subprocess tests")
class BridgeProtocolTests(unittest.TestCase):
    def test_console_output_stays_off_protocol_stdout(self) -> None:
        with BridgeProcess() as bridge:
            bridge.initialize()
            response = bridge.request(
                "tool/call",
                {"name": "fixture_echo", "arguments": {"value": "safe"}, "catalog_revision": 0},
            )

            self.assertNotIn("error", response)
            self.assertEqual([], bridge.protocol_errors)
            self.assertTrue(any("fixture loader wrote" in line for line in bridge.stderr))
            self.assertTrue(any("fixture tool console output safe" in line for line in bridge.stderr))

    def test_feature_negotiation_is_exact(self) -> None:
        with BridgeProcess() as bridge:
            initialized = bridge.initialize("artifacts", "lifecycle_events", "unknown_feature")
            self.assertEqual(
                {
                    "request_cancellation",
                    "content_parts",
                    "artifacts",
                    "lifecycle_events",
                },
                set(initialized["protocol"]["features"]),
            )

    def test_negotiated_artifact_is_published_exactly_once(self) -> None:
        with BridgeProcess() as bridge:
            bridge.initialize("artifacts")
            published: list[dict] = []

            def publish(message: dict) -> dict:
                published.append(message["params"])
                return {"artifact_id": "artifact-fixture"}

            bridge.handlers["artifact/publish"] = publish
            response = bridge.request(
                "tool/call",
                {"name": "fixture_echo", "arguments": {"value": "image"}, "catalog_revision": 0},
            )
            self.assertEqual(1, len(published))
            self.assertEqual("image/png", published[0]["mime_type"])
            self.assertEqual(5, published[0]["size"])
            self.assertEqual(hashlib.sha256(b"hello").hexdigest(), published[0]["sha256"])
            self.assertEqual(
                "artifact-fixture",
                response["result"]["content"][1]["artifact_id"],
            )

    def test_image_falls_back_to_text_without_artifact_negotiation(self) -> None:
        with BridgeProcess() as bridge:
            bridge.initialize()
            response = bridge.request(
                "tool/call",
                {"name": "fixture_echo", "arguments": {}, "catalog_revision": 0},
            )
            self.assertFalse(
                any(message.get("method") == "artifact/publish" for message in bridge.messages)
            )
            self.assertIn("fixture", response["result"]["content"][1]["text"].lower())

    def test_cancelling_a_tool_waiting_on_input_does_not_hang(self) -> None:
        with BridgeProcess() as bridge:
            bridge.initialize()
            tool_request = bridge.send_request(
                "tool/call",
                {"name": "fixture_prompt", "arguments": {}, "catalog_revision": 0},
            )
            prompt = bridge.wait_for(
                lambda messages: next(
                    (message for message in messages if message.get("method") == "input/request"),
                    None,
                ),
                description="fixture input request",
            )
            bridge.send(
                {
                    "jsonrpc": "2.0",
                    "method": "$/cancelRequest",
                    "params": {"id": tool_request, "reason": "test"},
                }
            )
            response = bridge.wait_response(tool_request, timeout=1.0)
            self.assertEqual(-32800, response["error"]["code"])
            bridge.wait_for(
                lambda messages: any(
                    message.get("method") == "$/cancelRequest"
                    and message.get("params", {}).get("id") == prompt["id"]
                    for message in messages
                ),
                timeout=1.0,
                description="nested prompt cancellation",
            )

    def test_before_agent_context_crosses_hook_and_context_wires(self) -> None:
        with BridgeProcess() as bridge:
            bridge.initialize()
            hook = bridge.request(
                "hook/run", {"hook": "before_prompt", "payload": {"prompt": "hello"}}
            )["result"]
            self.assertEqual(
                ["system_suffix", "prompt_suffix"],
                [item["placement"] for item in hook["context"]],
            )
            self.assertIn("system context for hello", hook["context"][0]["content"])
            collected = bridge.request("context/collect", {"prompt": "hello"})["result"]
            self.assertTrue(
                any(item["label"] == "pi-context" and "context event" in item["content"] for item in collected)
            )

    def test_lifecycle_is_serialized_and_handler_errors_are_on_stderr(self) -> None:
        with BridgeProcess() as bridge:
            bridge.initialize("lifecycle_events")
            first = bridge.send_request("turn/started", {})
            second = bridge.send_request("session/started", {})
            bridge.wait_response(first)
            bridge.wait_response(second)
            notifications = bridge.notifications()
            relevant = [
                item
                for item in notifications
                if item
                in {
                    "event:turn_start:start",
                    "event:turn_start:end",
                    "event:agent_start:start",
                    "event:agent_start:end",
                    "event:session_start:start",
                    "event:session_start:end",
                }
            ]
            self.assertEqual(
                [
                    "event:turn_start:start",
                    "event:turn_start:end",
                    "event:agent_start:start",
                    "event:agent_start:end",
                    "event:session_start:start",
                    "event:session_start:end",
                ],
                relevant,
            )
            bridge.wait_for(
                lambda _messages: any("fixture lifecycle failure" in line for line in bridge.stderr),
                description="onError stderr diagnostic",
            )
            diagnostic = next(line for line in bridge.stderr if "fixture lifecycle failure" in line)
            self.assertIn("session_start", diagnostic)
            self.assertIn("fixture-extension.mjs", diagnostic)

    def test_local_tool_and_turn_terminal_events_are_not_duplicated(self) -> None:
        with BridgeProcess() as bridge:
            bridge.initialize("lifecycle_events")
            bridge.request(
                "hook/run",
                {
                    "hook": "before_tool_call",
                    "payload": {"name": "fixture_echo", "arguments": {"value": "once"}},
                },
            )
            bridge.request(
                "tool/started", {"tool_call_id": "host-tool-1", "tool_name": "fixture_echo"}
            )
            bridge.request(
                "tool/call",
                {"name": "fixture_echo", "arguments": {"value": "once"}, "catalog_revision": 0},
            )
            bridge.request(
                "hook/run",
                {"hook": "after_tool_call", "payload": {"name": "fixture_echo", "output": "once"}},
            )
            bridge.request(
                "tool/settled",
                {"tool_call_id": "host-tool-1", "tool_name": "fixture_echo", "outcome": "completed"},
            )
            bridge.request("turn/started", {})
            bridge.request("turn/settled", {"outcome": "completed"})
            bridge.request(
                "hook/run", {"hook": "after_response", "payload": {"response": "done"}}
            )
            bridge.request(
                "hook/run", {"hook": "after_response", "payload": {"response": "done again"}}
            )

            notifications = bridge.notifications()
            self.assertEqual(1, sum(item.startswith("terminal:tool_result:") for item in notifications))
            self.assertEqual(1, notifications.count("event:tool_execution_end:start"))
            terminal_events = [
                item
                for item in notifications
                if item
                in {
                    "event:turn_end:start",
                    "event:agent_end:start",
                    "event:agent_settled:start",
                }
            ]
            self.assertEqual(
                [
                    "event:turn_end:start",
                    "event:agent_end:start",
                    "event:agent_settled:start",
                ],
                terminal_events,
            )

    def test_session_shutdown_is_emitted_once(self) -> None:
        with BridgeProcess() as bridge:
            bridge.initialize("lifecycle_events")
            bridge.request("session/settled", {"outcome": "completed"})
            bridge.request("shutdown")
            self.assertEqual(
                1,
                bridge.notifications().count("event:session_shutdown:start"),
            )

    def test_dynamic_tools_publish_and_new_revision_executes(self) -> None:
        with BridgeProcess() as bridge:
            bridge.handlers["tools/register"] = lambda message: {
                "revision": 1,
                "tools": ["fixture_echo", "fixture_prompt", "fixture_dynamic"],
            }
            bridge.initialize("dynamic_tools")
            command = bridge.request(
                "command/execute", {"name": "pi", "arguments": ["add-tool"]}
            )
            self.assertNotIn("error", command)
            registration = bridge.wait_for(
                lambda messages: next(
                    (message for message in messages if message.get("method") == "tools/register"),
                    None,
                ),
                description="dynamic tool registration",
            )
            self.assertEqual(["fixture_dynamic"], [tool["name"] for tool in registration["params"]["tools"]])
            response = bridge.request(
                "tool/call",
                {"name": "fixture_dynamic", "arguments": {}, "catalog_revision": 1},
            )
            self.assertEqual("dynamic", response["result"]["content"][0]["text"])

    def test_unsupported_command_context_is_an_explicit_error(self) -> None:
        with BridgeProcess() as bridge:
            bridge.initialize()
            response = bridge.request(
                "command/execute", {"name": "pi", "arguments": ["unsupported"]}
            )
            self.assertEqual(-32000, response["error"]["code"])
            self.assertIn("ctx.newSession", response["error"]["message"])


if __name__ == "__main__":
    unittest.main()
