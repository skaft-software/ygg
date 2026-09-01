from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import tempfile
import unittest

try:
    from .helpers import API_V03_REQUIRED_FEATURES, BridgeProcess, NODE
except ImportError:  # unittest discovery imports this file as a top-level module.
    from helpers import API_V03_REQUIRED_FEATURES, BridgeProcess, NODE


def single_file_source_fingerprint(path: Path) -> str:
    content = path.read_bytes()
    digest = hashlib.sha256()
    digest.update(b"ygg-pi-source-fingerprint\0")
    digest.update((1).to_bytes(4, "big"))
    digest.update(b"f")
    digest.update(b"f")
    digest.update((1).to_bytes(8, "big"))
    digest.update(b".")
    digest.update(len(content).to_bytes(8, "big"))
    digest.update(content)
    return digest.hexdigest()


def api_v03_initialize_params(workspace: Path) -> dict:
    return {
        "api_version": "0.3",
        "workspace": str(workspace),
        "host": {},
        "protocol": {
            "version": "0.3",
            "required_features": API_V03_REQUIRED_FEATURES,
            "optional_features": [],
            "limits": {"max_concurrent_requests": 64},
            "host_services": [],
        },
    }


class CompatibilityProfileTests(unittest.TestCase):
    def test_machine_profiles_pin_the_complete_0_84_4_inventory(self) -> None:
        compat_root = Path(__file__).resolve().parents[1]
        repository = compat_root.parents[1]
        profile = json.loads((compat_root / "profiles/0.84.4.json").read_text())
        tui_profile = json.loads(
            (repository / "crates/sexy-tui-rs/upstream/pi-tui-0.84.4.json").read_text()
        )

        revision = "b79e4cc834970cca69daebffab7df1da7d1e52c4"
        self.assertEqual(1, profile["schema_version"])
        self.assertEqual(revision, profile["source"]["revision"])
        self.assertEqual("0.84.4", profile["packages"]["coding_agent"]["version"])
        self.assertEqual("0.84.4", profile["packages"]["tui"]["version"])
        self.assertEqual("22.19.0", profile["node"]["minimum_version"])
        self.assertEqual(36, len(profile["public_surface"]["events"]))
        self.assertEqual(27, len(profile["public_surface"]["extension_api"]))
        self.assertEqual(28, len(profile["public_surface"]["ui_context"]))
        self.assertEqual(27, len(profile["public_surface"]["context"]))
        examples = profile["official_extension_examples"]
        self.assertEqual(78, len(examples))
        self.assertEqual(78, len(set(examples)))
        self.assertIn("plan-mode", examples)
        self.assertNotIn("README.md", examples)

        tests = tui_profile["test_files"]
        self.assertEqual(revision, tui_profile["source"]["revision"])
        self.assertEqual(33, len(tests))
        self.assertEqual(33, len({entry["upstream"] for entry in tests}))
        upstream_tests = {entry["upstream"] for entry in tests}
        self.assertIn("test/tui-render.test.ts", upstream_tests)
        self.assertIn("test/editor-history-keybindings.test.ts", upstream_tests)

    def test_real_pi_example_inventory_matches_profile_when_selected(self) -> None:
        selected = os.environ.get("YGG_PI_REAL_PACKAGE")
        if not selected:
            self.skipTest("YGG_PI_REAL_PACKAGE is not set")
        package = Path(selected).resolve()
        manifest = json.loads((package / "package.json").read_text())
        self.assertEqual("@earendil-works/pi-coding-agent", manifest["name"])
        self.assertEqual("0.84.4", manifest["version"])

        profile_path = Path(__file__).resolve().parents[1] / "profiles/0.84.4.json"
        profile = json.loads(profile_path.read_text())
        examples_root = package / "examples/extensions"
        actual = sorted(
            path.name for path in examples_root.iterdir() if path.name != "README.md"
        )
        self.assertEqual(profile["official_extension_examples"], actual)

    @unittest.skipUnless(NODE, "node is required for the Pi compatibility subprocess tests")
    def test_all_78_official_examples_execute_unchanged_when_selected(self) -> None:
        selected = os.environ.get("YGG_PI_REAL_PACKAGE")
        if not selected:
            self.skipTest("YGG_PI_REAL_PACKAGE is not set")
        package = Path(selected).resolve()
        profile_path = Path(__file__).resolve().parents[1] / "profiles/0.84.4.json"
        profile = json.loads(profile_path.read_text())
        examples_root = package / "examples/extensions"
        dependency_root = os.environ.get("YGG_PI_DEPENDENCY_EXAMPLES_ROOT")
        loaded: list[str] = []
        for relative in profile["official_extension_examples"]:
            extension = examples_root / relative
            if dependency_root and relative in {"gondolin", "sandbox"}:
                extension = Path(dependency_root).resolve() / relative
            with self.subTest(example=relative):
                with BridgeProcess(pi_package=package, extension=extension) as bridge:
                    initialized = bridge.initialize(
                        "ui_remote_components",
                        "provider_runtime",
                        "process_runtime",
                        "theme_runtime",
                    )
                    catalog = initialized["protocol"]["catalog"]
                    self.assertEqual(0, catalog["revision"])
                    self.assertEqual([], bridge.protocol_errors)
                    loaded.append(relative)
        self.assertEqual(profile["official_extension_examples"], loaded)


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
            self.assertTrue(any("directly to stdout" in line for line in bridge.stderr))
            self.assertTrue(any("fixture tool console output safe" in line for line in bridge.stderr))

    def test_feature_negotiation_is_exact(self) -> None:
        with BridgeProcess() as bridge:
            initialized = bridge.initialize("artifacts", "lifecycle_events", "unknown_feature")
            self.assertEqual(
                set(API_V03_REQUIRED_FEATURES) | {"artifacts"},
                set(initialized["protocol"]["features"]),
            )

    def test_provider_stream_and_oauth_callbacks_are_operation_fenced(self) -> None:
        services = [
            {
                "name": "providers",
                "version": 1,
                "scopes": ["custom-stream", "oauth"],
                "limits": {
                    "max_concurrent_requests": 4,
                    "max_request_bytes": 524288,
                    "max_response_bytes": 524288,
                    "max_items": 128,
                    "timeout_ms": 30000,
                },
            }
        ]
        with BridgeProcess() as bridge:
            initialized = bridge.initialize("request_progress", host_services=services)
            provider = next(
                item
                for item in initialized["protocol"]["catalog"]["providers"]
                if item["id"] == "fixture-provider"
            )
            self.assertEqual("Fixture Provider", provider["config"]["name"])
            self.assertIn("custom_stream_handle", provider["config"])
            self.assertEqual("Fixture OAuth", provider["config"]["oauth"]["name"])

            streamed = bridge.request(
                "provider/callback",
                {
                    "provider": "fixture-provider",
                    "action": "custom_stream",
                    "model": {"id": "fixture-model"},
                    "context": {"messages": []},
                    "options": {},
                },
            )["result"]
            self.assertEqual(
                ["start", "text_delta", "done"],
                [event["type"] for event in streamed["events"]],
            )
            self.assertEqual([], streamed["effects"]["effects"])

            oauth_calls: list[str] = []

            def oauth_host(message: dict) -> dict:
                payload = message["params"]["payload"]
                oauth_calls.append(payload["action"])
                value = {"value": "1234"} if payload["action"] == "prompt" else {}
                return {"status": "success", "value": value}

            bridge.handlers["host/call"] = oauth_host
            login = bridge.request(
                "provider/callback",
                {"provider": "fixture-provider", "action": "oauth_login"},
            )["result"]
            self.assertEqual(["authorize", "prompt"], oauth_calls)
            self.assertEqual("access-1234", login["credentials"]["access"])
            refreshed = bridge.request(
                "provider/callback",
                {
                    "provider": "fixture-provider",
                    "action": "oauth_refresh",
                    "credentials": login["credentials"],
                },
            )["result"]
            self.assertEqual("access-refreshed", refreshed["credentials"]["access"])
            projected = bridge.request(
                "provider/callback",
                {
                    "provider": "fixture-provider",
                    "action": "oauth_api_key",
                    "credentials": refreshed["credentials"],
                },
            )["result"]
            self.assertEqual("access-refreshed", projected["api_key"])

    def test_api_v03_catalog_effects_and_ordered_events_are_live(self) -> None:
        with BridgeProcess() as bridge:
            initialized = bridge.initialize()
            catalog = initialized["protocol"]["catalog"]
            self.assertEqual(0, catalog["revision"])
            self.assertIn("session_start", catalog["events"])
            self.assertIn("mutations", {command["name"] for command in catalog["commands"]})

            mutation = bridge.request(
                "command/execute", {"name": "mutations", "arguments": []}
            )
            self.assertNotIn("error", mutation)
            effects = mutation["result"]["effects"]["effects"]
            self.assertEqual(
                ["set_session_name", "append_custom", "set_active_tools", "set_ui_state"],
                [effect["type"] for effect in effects],
            )

            ordered = bridge.request(
                "event/handle",
                {
                    "sequence": 1,
                    "event": "session_start",
                    "payload": {"reason": "startup"},
                    "barrier": True,
                },
            )
            self.assertNotIn("error", ordered)
            self.assertEqual(1, ordered["result"]["sequence"])
            self.assertEqual([], ordered["result"]["effects"]["effects"])
            self.assertIn("event:session_start:start", bridge.notifications())

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

    def test_unrepresentable_tool_interception_fails_explicitly(self) -> None:
        with BridgeProcess() as bridge:
            bridge.initialize()
            mutated = bridge.request(
                "hook/run",
                {
                    "hook": "before_tool_call",
                    "payload": {"name": "bash", "arguments": {"mutateNative": True}},
                },
            )
            self.assertEqual(-32000, mutated["error"]["code"])
            self.assertIn("input mutation", mutated["error"]["message"])

            terminated = bridge.request(
                "hook/run",
                {
                    "hook": "before_tool_call",
                    "payload": {"name": "bash", "arguments": {"terminate": True}},
                },
            )
            self.assertEqual(-32000, terminated["error"]["code"])
            self.assertIn("tool_call.terminate", terminated["error"]["message"])

            transformed = bridge.request(
                "hook/run",
                {
                    "hook": "after_tool_call",
                    "payload": {
                        "name": "bash",
                        "arguments": {"value": "transform"},
                        "output": "original",
                    },
                },
            )
            self.assertEqual(-32000, transformed["error"]["code"])
            self.assertIn("tool_result mutation", transformed["error"]["message"])

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
            bridge.initialize()
            command = bridge.request(
                "command/execute", {"name": "add-tool", "arguments": []}
            )
            self.assertNotIn("error", command)
            registration = bridge.wait_for(
                lambda messages: next(
                    (message for message in messages if message.get("method") == "catalog/replace"),
                    None,
                ),
                description="dynamic catalog replacement",
            )
            self.assertIn(
                "fixture_dynamic",
                {tool["name"] for tool in registration["params"]["catalog"]["tools"]},
            )
            response = bridge.request(
                "tool/call",
                {"name": "fixture_dynamic", "arguments": {}, "catalog_revision": 1},
            )
            self.assertEqual("dynamic", response["result"]["content"][0]["text"])

    def test_runtime_commands_are_exposed_without_the_package_mux(self) -> None:
        with BridgeProcess() as bridge:
            initialized = bridge.initialize()
            command_names = {
                command["name"]
                for command in initialized["protocol"]["catalog"]["commands"]
            }
            self.assertIn("add-tool", command_names)
            self.assertIn("ui-methods", command_names)
            self.assertNotIn("pi", command_names)

            response = bridge.request(
                "command/execute", {"name": "add-tool", "arguments": []}
            )
            self.assertNotIn("error", response)
            bridge.wait_for(
                lambda messages: next(
                    (message for message in messages if message.get("method") == "catalog/replace"),
                    None,
                ),
                description="direct-command catalog replacement",
            )

    def test_progress_is_emitted_only_when_negotiated(self) -> None:
        with BridgeProcess() as bridge:
            bridge.initialize()
            response = bridge.request(
                "tool/call",
                {"name": "fixture_progress", "arguments": {}, "catalog_revision": 0},
            )
            self.assertNotIn("error", response)
            self.assertFalse(any(message.get("method") == "$/progress" for message in bridge.messages))

        with BridgeProcess() as bridge:
            bridge.initialize("request_progress")
            response = bridge.request(
                "tool/call",
                {"name": "fixture_progress", "arguments": {}, "catalog_revision": 0},
            )
            self.assertNotIn("error", response)
            self.assertTrue(any(message.get("method") == "$/progress" for message in bridge.messages))

    def test_transformed_tool_result_preserves_details_usage_and_error(self) -> None:
        with BridgeProcess() as bridge:
            bridge.initialize()
            result = bridge.request(
                "tool/call",
                {
                    "name": "fixture_echo",
                    "arguments": {"value": "transform"},
                    "catalog_revision": 0,
                },
            )["result"]
            self.assertEqual("transformed", result["content"][0]["text"])
            self.assertTrue(result["is_error"])
            self.assertEqual(
                {
                    "details": {"transformed": True},
                    "usage": {"input": 1, "output": 2},
                },
                result["metadata"],
            )

    def test_current_ui_names_fail_explicitly_and_host_state_is_visible(self) -> None:
        with BridgeProcess() as bridge:
            bridge.initialize(host={"session_name": "fixture session", "reasoning": "High"})
            ui = bridge.request(
                "command/execute", {"name": "ui-methods", "arguments": []}
            )
            self.assertNotIn("error", ui)
            self.assertIn("ui-current-methods-explicit", bridge.notifications())
            state = bridge.request(
                "command/execute", {"name": "host-state", "arguments": []}
            )
            self.assertNotIn("error", state)

    def test_explicit_runtime_selector_rejects_an_unpinned_pi_version(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "package.json").write_text(
                json.dumps(
                    {
                        "name": "@earendil-works/pi-coding-agent",
                        "version": "0.85.0",
                        "type": "module",
                    }
                ),
                encoding="utf-8",
            )
            with BridgeProcess(pi_package=root) as bridge:
                response = bridge.request(
                    "initialize",
                    api_v03_initialize_params(root),
                )
                self.assertEqual(-32000, response["error"]["code"])
                self.assertIn("expected exactly 0.84.4", response["error"]["message"])

    def test_explicit_runtime_selector_bounds_package_metadata(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "package.json").write_bytes(b" " * (256 * 1024 + 1))
            with BridgeProcess(pi_package=root) as bridge:
                response = bridge.request(
                    "initialize",
                    api_v03_initialize_params(root),
                )
                self.assertEqual(-32000, response["error"]["code"])
                self.assertIn("262144-byte limit", response["error"]["message"])

    def test_generated_source_fingerprint_is_enforced_before_loading(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            extension = Path(directory).resolve() / "extension.mjs"
            extension.write_text("export default () => {};\n", encoding="utf-8")
            fingerprint = single_file_source_fingerprint(extension)
            with BridgeProcess(
                extension=extension,
                source_fingerprint=fingerprint,
            ) as bridge:
                initialized = bridge.initialize()
                self.assertEqual("0.3", initialized["api_version"])

            extension.write_text(
                "export default () => { throw new Error('changed'); };\n",
                encoding="utf-8",
            )
            with BridgeProcess(
                extension=extension,
                source_fingerprint=fingerprint,
            ) as bridge:
                response = bridge.request(
                    "initialize",
                    api_v03_initialize_params(extension.parent),
                )
                self.assertEqual(-32000, response["error"]["code"])
                self.assertIn(
                    "source changed after aggregate locking",
                    response["error"]["message"],
                )

    @unittest.skipUnless(
        os.environ.get("YGG_PI_REAL_PACKAGE"),
        "set YGG_PI_REAL_PACKAGE to run the pinned real-Pi smoke test",
    )
    def test_real_pi_0844_hello_extension_smoke(self) -> None:
        package = Path(os.environ["YGG_PI_REAL_PACKAGE"]).resolve()
        extension = Path(
            os.environ.get(
                "YGG_PI_REAL_EXTENSION",
                package / "examples/extensions/hello.ts",
            )
        ).resolve()
        with BridgeProcess(pi_package=package, extension=extension) as bridge:
            initialized = bridge.initialize()
            self.assertTrue(any(
                tool["name"] == "hello"
                for tool in initialized["protocol"]["catalog"]["tools"]
            ))
            response = bridge.request(
                "tool/call",
                {
                    "name": "hello",
                    "arguments": {"name": "Ygg"},
                    "catalog_revision": 0,
                },
            )
            self.assertEqual("Hello, Ygg!", response["result"]["content"][0]["text"])

    @unittest.skipUnless(
        os.environ.get("YGG_PI_REAL_PACKAGE"),
        "set YGG_PI_REAL_PACKAGE to run the pinned real-Pi smoke test",
    )
    def test_real_pi_0844_plan_mode_load_smoke(self) -> None:
        package = Path(os.environ["YGG_PI_REAL_PACKAGE"]).resolve()
        extension = package / "examples/extensions/plan-mode"
        host = {
            "active_tools": ["read", "bash", "edit", "write"],
            "all_tools": [
                {"name": name, "description": name, "parameters": {"type": "object"}}
                for name in ["read", "bash", "edit", "write"]
            ],
        }
        with BridgeProcess(pi_package=package, extension=extension) as bridge:
            initialized = bridge.initialize(host=host)
            self.assertEqual(
                {"plan", "todos"},
                {
                    command["name"]
                    for command in initialized["protocol"]["catalog"]["commands"]
                },
            )
            response = bridge.request(
                "command/execute", {"name": "plan", "arguments": []}
            )
            self.assertNotIn("error", response)
            effects = response["result"]["effects"]["effects"]
            active = next(effect for effect in effects if effect["type"] == "set_active_tools")
            self.assertNotIn("edit", active["tools"])
            self.assertNotIn("write", active["tools"])
            persisted = next(effect for effect in effects if effect["type"] == "append_custom")
            self.assertEqual("plan-mode", persisted["custom_type"])
            self.assertTrue(persisted["details"]["enabled"])
            blocked = bridge.request(
                "event/handle",
                {
                    "sequence": 1,
                    "event": "tool_call",
                    "payload": {
                        "toolCallId": "plan-bash",
                        "toolName": "bash",
                        "input": {"command": "rm -rf /tmp/unsafe"},
                    },
                    "barrier": True,
                },
            )
            self.assertTrue(blocked["result"]["result"]["block"])
            self.assertIn("not allowlisted", blocked["result"]["result"]["reason"])
            self.assertTrue(initialized["protocol"]["catalog"]["shortcuts"])
            self.assertTrue(initialized["protocol"]["catalog"]["flags"])

    def test_unsupported_command_context_is_an_explicit_error(self) -> None:
        with BridgeProcess() as bridge:
            bridge.initialize()
            response = bridge.request(
                "command/execute", {"name": "unsupported", "arguments": []}
            )
            self.assertEqual(-32000, response["error"]["code"])
            self.assertIn("fixture host service is unavailable", response["error"]["message"])


if __name__ == "__main__":
    unittest.main()
