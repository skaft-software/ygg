from __future__ import annotations

import hashlib
import json
import os
import shutil
from pathlib import Path
import tempfile
import time
import unittest

try:
    from .helpers import (
        BridgeProcess,
        FAKE_PI,
        FIXTURES,
        NODE,
        PROVIDER_EXTENSION,
        PROVIDER_HEADERS_HOOK_EXTENSION,
        UNSAFE_PROVIDER_EXTENSION,
        v03_contract,
    )
except ImportError:  # unittest discovery imports this file as a top-level module.
    from helpers import (
        BridgeProcess,
        FAKE_PI,
        FIXTURES,
        NODE,
        PROVIDER_EXTENSION,
        PROVIDER_HEADERS_HOOK_EXTENSION,
        UNSAFE_PROVIDER_EXTENSION,
        v03_contract,
    )


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


def strict_initialize_response(bridge: BridgeProcess, workspace: Path | None = None) -> dict:
    return bridge.request(
        "initialize",
        {
            "workspace": str(workspace or Path.cwd()),
            "host": {},
            "protocol": {"optional_features": []},
            "ygg_version": bridge.ygg_version,
            "extension": {
                "name": bridge.command_name,
                "version": "fixture",
                "manifest_path": str(bridge.manifest_path),
                "source": "explicit",
            },
        },
    )


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
                lambda _messages: any("handler failed" in line for line in bridge.stderr),
                description="onError stderr diagnostic",
            )
            diagnostic = next(line for line in bridge.stderr if "handler failed" in line)
            self.assertIn("session_start", diagnostic)
            self.assertIn("#1", diagnostic)
            self.assertNotIn("fixture lifecycle failure", diagnostic)

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

    def test_runtime_commands_are_exposed_without_the_package_mux(self) -> None:
        with BridgeProcess() as bridge:
            bridge.handlers["tools/register"] = lambda _message: {
                "revision": 1,
                "tools": ["fixture_echo", "fixture_prompt", "fixture_progress", "fixture_dynamic"],
            }
            initialized = bridge.initialize("dynamic_tools", "runtime_commands")
            command_names = {command["name"] for command in initialized["commands"]}
            self.assertIn("add-tool", command_names)
            self.assertIn("ui-methods", command_names)
            self.assertNotIn("pi", command_names)

            response = bridge.request(
                "command/execute", {"name": "add-tool", "arguments": []}
            )
            self.assertNotIn("error", response)
            bridge.wait_for(
                lambda messages: next(
                    (message for message in messages if message.get("method") == "tools/register"),
                    None,
                ),
                description="direct-command dynamic tool registration",
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
                "command/execute", {"name": "pi", "arguments": ["ui-methods"]}
            )
            self.assertNotIn("error", ui)
            self.assertIn("ui-current-methods-explicit", bridge.notifications())
            state = bridge.request(
                "command/execute", {"name": "pi", "arguments": ["host-state"]}
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
                    {"workspace": str(root), "host": {}, "protocol": {"optional_features": []}},
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
                    {"workspace": str(root), "host": {}, "protocol": {"optional_features": []}},
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
                self.assertEqual("0.2", initialized["api_version"])

            marker = Path(directory).resolve() / "loaded"
            extension.write_text(
                "import { writeFileSync } from 'node:fs';\n"
                f"writeFileSync({json.dumps(str(marker))}, 'loaded');\n"
                "export default () => { throw new Error('changed'); };\n",
                encoding="utf-8",
            )
            with BridgeProcess(
                extension=extension,
                source_fingerprint=fingerprint,
            ) as bridge:
                response = bridge.request(
                    "initialize",
                    {
                        "workspace": str(extension.parent),
                        "host": {},
                        "protocol": {"optional_features": []},
                    },
                )
                self.assertEqual(-32000, response["error"]["code"])
                self.assertIn(
                    "changed after link publication",
                    response["error"]["message"],
                )
                self.assertFalse(marker.exists())

    def test_strict_aggregate_preserves_load_order_shared_globals_and_restart_state(self) -> None:
        sources = [FIXTURES / "aggregate" / "first.mjs", FIXTURES / "aggregate" / "second.mjs"]
        observed: list[dict] = []
        for _ in range(2):
            with BridgeProcess(
                extensions=sources,
                strict_identity=True,
                command_name="pi-aggregate",
            ) as bridge:
                initialized = bridge.initialize()
                self.assertTrue(any(tool["name"] == "aggregate_state" for tool in initialized["tools"]))
                result = bridge.request(
                    "tool/call",
                    {"name": "aggregate_state", "arguments": {}, "catalog_revision": 0},
                )
                observed.append(json.loads(result["result"]["content"][0]["text"]))
                self.assertTrue(any("pinned=yes" in line for line in bridge.stderr))
        self.assertEqual(
            {
                "loadOrder": ["first", "second"],
                "eventOrder": ["first-listener:second"],
                "globalMarker": "first",
            },
            observed[0],
        )
        self.assertEqual(observed[0], observed[1])

    def test_aggregate_rejects_partial_loader_success(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            broken = Path(directory).resolve() / "broken.mjs"
            broken.write_text("throw new Error('private loader detail');\n", encoding="utf-8")
            with BridgeProcess(
                extensions=[FIXTURES / "fixture-extension.mjs", broken],
                strict_identity=True,
                command_name="pi-partial",
            ) as bridge:
                response = strict_initialize_response(bridge, broken.parent)
                self.assertEqual(-32000, response["error"]["code"])
                self.assertIn("did not load every pinned source", response["error"]["message"])
                self.assertNotIn(str(broken.parent), response["error"]["message"])

    def test_strict_aggregate_cancellation_propagates_to_shared_runtime(self) -> None:
        sources = [FIXTURES / "aggregate" / "first.mjs", FIXTURES / "aggregate" / "second.mjs"]
        with BridgeProcess(
            extensions=sources,
            strict_identity=True,
            command_name="pi-aggregate",
        ) as bridge:
            bridge.initialize()
            request_id = bridge.send_request(
                "tool/call",
                {"name": "aggregate_wait", "arguments": {}, "catalog_revision": 0},
            )
            prompt = bridge.wait_for(
                lambda messages: next(
                    (message for message in messages if message.get("method") == "input/request"),
                    None,
                ),
                description="aggregate input request",
            )
            bridge.send(
                {"jsonrpc": "2.0", "method": "$/cancelRequest", "params": {"id": request_id}},
            )
            response = bridge.wait_response(request_id, timeout=1.0)
            self.assertEqual(-32800, response["error"]["code"])
            self.assertTrue(
                any(
                    message.get("method") == "$/cancelRequest" and message.get("params", {}).get("id") == prompt["id"]
                    for message in bridge.messages
                )
            )

    def test_strict_identity_rejects_stale_source_lock_runtime_and_manifest_binding(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source.mjs"
            source.write_text("export default () => {};\n", encoding="utf-8")
            lock = root / "package-lock.json"
            lock.write_text('{"lockfileVersion":3}\n', encoding="utf-8")

            bridge = BridgeProcess(extension=source, strict_identity=True, command_name="pi-stale")
            source.write_text("export default () => { return 'changed'; };\n", encoding="utf-8")
            try:
                response = strict_initialize_response(bridge, root)
                self.assertIn("error", response)
                self.assertIn("changed after link publication", response["error"]["message"])
                self.assertNotIn(str(root), response["error"]["message"])
            finally:
                bridge.close()

            source.write_text("export default () => {};\n", encoding="utf-8")
            bridge = BridgeProcess(extension=source, strict_identity=True, command_name="pi-stale")
            lock.write_text('{"lockfileVersion":4}\n', encoding="utf-8")
            try:
                response = strict_initialize_response(bridge, root)
                self.assertIn("dependency lock changed", response["error"]["message"])
            finally:
                bridge.close()

            lock.write_text('{"lockfileVersion":3}\n', encoding="utf-8")
            package = root / "pi-package"
            shutil.copytree(FAKE_PI, package)
            bridge = BridgeProcess(
                extension=source,
                pi_package=package,
                strict_identity=True,
                command_name="pi-stale",
            )
            (package / "dist" / "index.js").write_text("export {}; // changed runtime\n", encoding="utf-8")
            try:
                response = strict_initialize_response(bridge, root)
                self.assertIn("Pinned Pi runtime integrity changed", response["error"]["message"])
            finally:
                bridge.close()

            bridge = BridgeProcess(extension=source, strict_identity=True, command_name="pi-stale")
            try:
                response = bridge.request(
                    "initialize",
                    {
                        "workspace": str(root),
                        "host": {},
                        "protocol": {"optional_features": []},
                        "ygg_version": bridge.ygg_version,
                        "extension": {
                            "name": bridge.command_name,
                            "version": "fixture",
                            "manifest_path": str(root / "wrong" / "extension.toml"),
                            "source": "explicit",
                        },
                    },
                )
                self.assertIn("link identity does not match", response["error"]["message"])
            finally:
                bridge.close()

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
            initialized = bridge.initialize("runtime_commands")
            self.assertTrue(any(tool["name"] == "hello" for tool in initialized["tools"]))
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
        extension = package / "examples/extensions/plan-mode/index.ts"
        with BridgeProcess(pi_package=package, extension=extension) as bridge:
            initialized = bridge.initialize("runtime_commands")
            self.assertEqual(
                {"plan", "todos"},
                {command["name"] for command in initialized["commands"]},
            )
            response = bridge.request(
                "command/execute", {"name": "todos", "arguments": []}
            )
            self.assertNotIn("error", response)
            self.assertTrue(any("No todos" in item for item in bridge.notifications()))
            self.assertTrue(any("shortcuts is unavailable" in item for item in bridge.stderr))
            self.assertTrue(any("flags is unavailable" in item for item in bridge.stderr))

    def test_unsupported_command_context_is_an_explicit_error(self) -> None:
        with BridgeProcess() as bridge:
            bridge.initialize()
            response = bridge.request(
                "command/execute", {"name": "pi", "arguments": ["unsupported"]}
            )
            self.assertEqual(-32000, response["error"]["code"])
            self.assertIn("ctx.newSession", response["error"]["message"])


@unittest.skipUnless(NODE, "node is required for the API 0.3 bridge subprocess tests")
class Api03ProviderBridgeTests(unittest.TestCase):
    def provider_bridge(self) -> tuple[BridgeProcess, list[dict], list[dict]]:
        bridge = BridgeProcess(extension=PROVIDER_EXTENSION, api_version="0.3")
        catalog_requests: list[dict] = []
        auth_requests: list[dict] = []

        def catalog(message: dict) -> dict:
            catalog_requests.append(message)
            if message["method"] == "providers/unregister":
                return {
                    "revision": len(catalog_requests),
                    "provider_ids": [],
                    "model_ids": [],
                }
            return {
                "revision": len(catalog_requests),
                "provider_ids": ["fixture-provider"],
                "model_ids": ["fixture-provider/fixture-model"],
            }

        def authorization(message: dict) -> dict:
            auth_requests.append(message)
            # Deliberately return a lease: the bridge may receive it from the
            # host but must never retain or expose it to the Pi adapter.
            return {"status": "ready", "lease": "host/only=fixture:lease"}

        bridge.handlers["providers/register"] = catalog
        bridge.handlers["providers/update"] = catalog
        bridge.handlers["providers/unregister"] = catalog
        bridge.handlers["provider/auth/request"] = authorization
        bridge.handlers["provider/auth/revoke"] = authorization
        return bridge, catalog_requests, auth_requests

    @staticmethod
    def v03_tool(bridge: BridgeProcess, name: str) -> dict:
        return bridge.request(
            "tool/call",
            {
                "name": "pi",
                "arguments": {"tool_name": name, "arguments": {}},
                "context": {},
            },
        )

    @staticmethod
    def wait_for_provider_ready(
        bridge: BridgeProcess, catalog_requests: list[dict], auth_requests: list[dict]
    ) -> None:
        bridge.wait_for(
            lambda _messages: catalog_requests[0] if catalog_requests else None,
            description="initial provider registration",
        )
        bridge.wait_for(
            lambda _messages: auth_requests[0] if auth_requests else None,
            description="initial provider authorization",
        )

    def test_api_03_selects_host_owned_provider_catalog_and_auth(self) -> None:
        bridge, catalog_requests, auth_requests = self.provider_bridge()
        with bridge:
            initialized = bridge.initialize()
            self.assertEqual("0.3", initialized["api_version"])
            self.assertEqual("ygg-canonical-json-v1", initialized["contract"]["encoding"])
            self.assertIn("provider_stream", initialized["contract"]["capabilities"])
            self.assertIn("pi", {tool["name"] for tool in initialized["tools"]})

            registration = bridge.wait_for(
                lambda _messages: catalog_requests[0] if catalog_requests else None,
                description="initial API 0.3 provider registration",
            )
            params = registration["params"]
            self.assertEqual("fixture-provider", params["provider"]["id"])
            self.assertEqual(
                {"id", "label", "auth"}, set(params["provider"])
            )
            self.assertEqual(
                {"kind": "host_credential", "subject": "fixture-credential"},
                params["provider"]["auth"],
            )
            self.assertEqual(
                {"id", "api_name", "protocol", "context_window", "max_output_tokens", "capabilities", "display_name"},
                set(params["models"][0]),
            )
            self.assertNotIn("yggStream", json.dumps(params))
            self.assertNotIn("baseUrl", json.dumps(params))

            authorization = bridge.wait_for(
                lambda _messages: auth_requests[0] if auth_requests else None,
                description="host-owned provider authorization request",
            )
            self.assertEqual("authorize", authorization["params"]["action"])
            self.assertFalse(authorization["params"]["interactive"])
            self.assertNotIn("lease", authorization["params"])

    def test_api_03_delayed_initial_provider_registration_is_published_after_initialize(self) -> None:
        bridge = BridgeProcess(
            extension=PROVIDER_EXTENSION,
            api_version="0.3",
            fixture_environment={"YGG_PI_FIXTURE_PROVIDER_REGISTER_DELAY_MS": "75"},
        )
        catalog_requests: list[dict] = []
        bridge.handlers["providers/register"] = lambda message: (
            catalog_requests.append(message)
            or {
                "revision": 1,
                "provider_ids": ["fixture-provider"],
                "model_ids": ["fixture-provider/fixture-model"],
            }
        )
        bridge.handlers["provider/auth/request"] = lambda _message: {"status": "ready"}
        bridge.handlers["provider/auth/revoke"] = lambda _message: {"status": "revoked"}
        bridge.handlers["providers/unregister"] = lambda _message: {
            "revision": 2,
            "provider_ids": [],
            "model_ids": [],
        }
        with bridge:
            initialized = bridge.initialize()
            self.assertEqual("0.3", initialized["api_version"])
            registration = bridge.wait_for(
                lambda _messages: catalog_requests[0] if catalog_requests else None,
                description="delayed initial API 0.3 provider registration",
                timeout=1.0,
            )
            self.assertEqual("fixture-provider", registration["params"]["provider"]["id"])
            completion = bridge.wait_for(
                lambda messages: next(
                    (message for message in messages if message.get("method") == "providers/complete"),
                    None,
                ),
                description="delayed initial provider catalog completion",
                timeout=1.0,
            )
            self.assertGreater(bridge.messages.index(completion), bridge.messages.index(registration))

    def test_api_03_initial_provider_completion_follows_full_serial_batch(self) -> None:
        bridge = BridgeProcess(
            extension=PROVIDER_EXTENSION,
            api_version="0.3",
            fixture_environment={
                "YGG_PI_FIXTURE_PROVIDER_AUTH": "none",
                "YGG_PI_FIXTURE_INITIAL_PROVIDER_COUNT": "2",
            },
        )
        registrations: list[dict] = []

        def catalog(message: dict) -> dict:
            registrations.append(message)
            provider = message["params"]["provider"]
            models = message["params"]["models"]
            return {
                "revision": len(registrations),
                "provider_ids": [provider["id"]],
                "model_ids": [f'{provider["id"]}/{model["id"]}' for model in models],
            }

        bridge.handlers["providers/register"] = catalog
        with bridge:
            bridge.initialize()
            completion = bridge.wait_for(
                lambda messages: next(
                    (message for message in messages if message.get("method") == "providers/complete"),
                    None,
                ),
                description="initial provider catalog completion",
            )
            self.assertNotIn("id", completion)
            self.assertEqual({}, completion["params"])
            self.assertEqual(
                ["fixture-provider", "fixture-second-provider"],
                [message["params"]["provider"]["id"] for message in registrations],
            )
            completion_index = bridge.messages.index(completion)
            self.assertTrue(
                all(bridge.messages.index(message) < completion_index for message in registrations),
                "completion must follow every serialized registration",
            )

    def test_api_03_empty_initial_provider_catalog_completes(self) -> None:
        with BridgeProcess(api_version="0.3") as bridge:
            bridge.initialize()
            completion = bridge.wait_for(
                lambda messages: next(
                    (message for message in messages if message.get("method") == "providers/complete"),
                    None,
                ),
                description="empty provider catalog completion",
            )
            self.assertEqual({}, completion["params"])
            self.assertFalse(
                any(message.get("method") == "providers/register" for message in bridge.messages)
            )

    def test_api_03_tool_dispatcher_bounds_pi_content_parts(self) -> None:
        bridge, catalog_requests, auth_requests = self.provider_bridge()
        with bridge:
            bridge.initialize()
            self.wait_for_provider_ready(bridge, catalog_requests, auth_requests)
            response = self.v03_tool(bridge, "fixture_provider_many_parts")
            self.assertEqual(-32012, response["error"]["code"])
            self.assertEqual("extension resource exhausted", response["error"]["message"])

    def test_provider_stream_translates_events_without_leases_and_runs_safe_hooks(self) -> None:
        bridge, catalog_requests, auth_requests = self.provider_bridge()
        with bridge:
            bridge.initialize()
            self.wait_for_provider_ready(bridge, catalog_requests, auth_requests)
            unsafe_request = bridge.request(
                "provider/stream",
                {
                    "stream_id": "authority-stream",
                    "provider_id": "fixture-provider",
                    "model_id": "fixture-model",
                    "request": {"headers": {"authorization": "must-not-reach-pi"}},
                },
            )
            self.assertEqual(-32602, unsafe_request["error"]["code"])
            self.assertNotIn("must-not-reach-pi", json.dumps(unsafe_request))
            mutated_hook_request = bridge.request(
                "provider/stream",
                {
                    "stream_id": "mutated-hook-stream",
                    "provider_id": "fixture-provider",
                    "model_id": "fixture-model",
                    "request": {"fixture_unsafe_mutation": True},
                },
            )
            self.assertEqual(-32602, mutated_hook_request["error"]["code"])
            self.assertNotIn("must-not-reach-pi-adapter", json.dumps(mutated_hook_request))
            response = bridge.request(
                "provider/stream",
                {
                    "stream_id": "fixture-stream",
                    "provider_id": "fixture-provider",
                    "model_id": "fixture-model",
                    "request": {"prompt": "hello"},
                    "authorization_lease": "host/only=fixture:lease",
                },
            )
            self.assertEqual({"stream_id": "fixture-stream", "accepted": True}, response["result"])
            events = bridge.wait_for(
                lambda messages: [
                    message["params"]
                    for message in messages
                    if message.get("method") == "provider/event"
                    and message.get("params", {}).get("stream_id") == "fixture-stream"
                    and message.get("params", {}).get("kind") == "finished"
                ],
                description="finished provider event",
            )
            stream_events = [
                message["params"]
                for message in bridge.messages
                if message.get("method") == "provider/event"
                and message.get("params", {}).get("stream_id") == "fixture-stream"
            ]
            self.assertTrue(events)
            self.assertEqual(
                [
                    "started",
                    "text_start",
                    "text_delta",
                    "text_end",
                    "tool_call_start",
                    "tool_call_args_delta",
                    "tool_call_end",
                    "usage",
                    "finished",
                ],
                [event["kind"] for event in stream_events],
            )
            self.assertEqual(
                "hooked:hello",
                next(event["payload"]["delta"] for event in stream_events if event["kind"] == "text_delta"),
            )
            self.assertEqual(list(range(len(stream_events))), [event["sequence"] for event in stream_events])
            self.assertNotIn("host/only=fixture:lease", json.dumps(stream_events))

            direct_response = bridge.request(
                "provider/stream",
                {
                    "stream_id": "direct-fixture-stream",
                    "provider_id": "fixture-provider",
                    "model_id": "fixture-model",
                    "request": {"direct": True},
                },
            )
            self.assertEqual(
                {"stream_id": "direct-fixture-stream", "accepted": True}, direct_response["result"]
            )
            direct_finished = bridge.wait_for(
                lambda messages: next(
                    (
                        message["params"]
                        for message in messages
                        if message.get("method") == "provider/event"
                        and message.get("params", {}).get("stream_id") == "direct-fixture-stream"
                        and message.get("params", {}).get("kind") == "finished"
                    ),
                    None,
                ),
                description="finished direct API 0.3-shaped provider event",
            )
            self.assertEqual("end_turn", direct_finished["payload"]["stop_reason"])
            self.assertEqual(4, direct_finished["sequence"])

            hook_status = self.v03_tool(bridge, "fixture_provider_hook_status")
            self.assertEqual("200", hook_status["result"]["content"][0]["text"])
            self.assertIsNone(hook_status["result"]["metadata"])

    def test_provider_refresh_unsafe_mutation_and_shutdown_cleanup_are_explicit(self) -> None:
        bridge, catalog_requests, auth_requests = self.provider_bridge()
        with bridge:
            bridge.initialize()
            self.wait_for_provider_ready(bridge, catalog_requests, auth_requests)
            updated = self.v03_tool(bridge, "fixture_provider_update")
            self.assertEqual("provider updated", updated["result"]["content"][0]["text"])
            bridge.wait_for(
                lambda _messages: next(
                    (item for item in catalog_requests if item["method"] == "providers/update"), None
                ),
                description="provider update",
            )
            refresh = bridge.wait_for(
                lambda _messages: next(
                    (item for item in auth_requests if item["params"]["action"] == "refresh"), None
                ),
                description="provider authorization refresh",
            )
            self.assertEqual("fixture-provider", refresh["params"]["provider_id"])

            unsafe = self.v03_tool(bridge, "fixture_provider_unsafe")
            self.assertEqual(-32602, unsafe["error"]["code"])
            self.assertEqual("invalid params", unsafe["error"]["message"])
            self.assertNotIn("must-not-cross", json.dumps(unsafe))
            self.assertFalse(
                any(
                    item["params"].get("provider", {}).get("id") == "unsafe-provider"
                    for item in catalog_requests
                )
            )

            unregistered = self.v03_tool(bridge, "fixture_provider_unregister")
            self.assertEqual("provider unregistered", unregistered["result"]["content"][0]["text"])
            bridge.wait_for(
                lambda _messages: next(
                    (item for item in catalog_requests if item["method"] == "providers/unregister"), None
                ),
                description="provider unregister",
            )
            unavailable = bridge.request(
                "provider/stream",
                {
                    "stream_id": "retired-provider-stream",
                    "provider_id": "fixture-provider",
                    "model_id": "fixture-model",
                    "request": {"prompt": "retired"},
                },
            )
            self.assertFalse(unavailable["result"]["accepted"])

            shutdown = bridge.request("shutdown")
            self.assertEqual("shutdown", shutdown["result"]["terminal"])
            self.assertTrue(
                any(item["method"] == "providers/unregister" for item in catalog_requests)
            )
            self.assertTrue(
                any(item["params"]["action"] == "revoke" for item in auth_requests)
            )

    def test_provider_cancellation_uses_notification_and_cleans_stream_state(self) -> None:
        bridge, catalog_requests, auth_requests = self.provider_bridge()
        with bridge:
            bridge.initialize()
            self.wait_for_provider_ready(bridge, catalog_requests, auth_requests)
            response = bridge.request(
                "provider/stream",
                {
                    "stream_id": "held-stream",
                    "provider_id": "fixture-provider",
                    "model_id": "fixture-model",
                    "request": {"hold": True, "prompt": "wait"},
                },
            )
            self.assertTrue(response["result"]["accepted"])
            bridge.wait_for(
                lambda messages: next(
                    (
                        message
                        for message in messages
                        if message.get("method") == "provider/event"
                        and message.get("params", {}).get("stream_id") == "held-stream"
                        and message.get("params", {}).get("kind") == "text_delta"
                    ),
                    None,
                ),
                description="held provider stream progress",
            )
            bridge.send(
                {
                    "jsonrpc": "2.0",
                    "method": "provider/cancel",
                    "params": {"stream_id": "held-stream", "reason": "fixture cancellation"},
                }
            )
            deadline = time.monotonic() + 2.0
            cancellation_status: dict | None = None
            while time.monotonic() < deadline:
                cancellation_status = self.v03_tool(bridge, "fixture_provider_cancel_status")
                if cancellation_status.get("result", {}).get("content", [{}])[0].get("text") == "true":
                    break
                time.sleep(0.02)
            self.assertIsNotNone(cancellation_status)
            self.assertEqual("true", cancellation_status["result"]["content"][0]["text"])
            reopened = bridge.request(
                "provider/stream",
                {
                    "stream_id": "held-stream",
                    "provider_id": "fixture-provider",
                    "model_id": "fixture-model",
                    "request": {"hold": True, "prompt": "reopened"},
                },
            )
            self.assertTrue(reopened["result"]["accepted"])
            bridge.send(
                {
                    "jsonrpc": "2.0",
                    "method": "provider/cancel",
                    "params": {"stream_id": "held-stream", "reason": "fixture teardown"},
                }
            )
            self.assertFalse(
                any(
                    message.get("method") == "provider/event"
                    and message.get("params", {}).get("stream_id") == "held-stream"
                    and message.get("params", {}).get("kind") == "finished"
                    for message in bridge.messages
                )
            )

    def test_provider_stream_cancellation_does_not_wait_for_pending_catalog_sync(self) -> None:
        # Leave the initial authorization response outstanding, then cancel an
        # inbound stream request. Its cancellation must not be held behind the
        # unrelated host-owned authorization exchange.
        with BridgeProcess(extension=PROVIDER_EXTENSION, api_version="0.3") as bridge:
            bridge.initialize()
            registration = bridge.wait_for(
                lambda messages: next(
                    (message for message in messages if message.get("method") == "providers/register"),
                    None,
                ),
                description="initial provider registration awaiting a manual response",
            )
            bridge.send(
                {
                    "jsonrpc": "2.0",
                    "id": registration["id"],
                    "result": {
                        "revision": 1,
                        "provider_ids": ["fixture-provider"],
                        "model_ids": ["fixture-provider/fixture-model"],
                    },
                }
            )
            authorization = bridge.wait_for(
                lambda messages: next(
                    (
                        message
                        for message in messages
                        if message.get("method") == "provider/auth/request"
                    ),
                    None,
                ),
                description="provider authorization awaiting a manual response",
            )

            bridge.handlers["providers/unregister"] = lambda _message: {
                "revision": 2,
                "provider_ids": [],
                "model_ids": [],
            }
            bridge.handlers["provider/auth/revoke"] = lambda _message: {"status": "revoked"}
            stream_id = bridge.send_request(
                "provider/stream",
                {
                    "stream_id": "cancel-before-auth",
                    "provider_id": "fixture-provider",
                    "model_id": "fixture-model",
                    "request": {"prompt": "wait"},
                },
            )
            bridge.send(
                {
                    "jsonrpc": "2.0",
                    "method": "$/cancelRequest",
                    "params": {"id": stream_id},
                }
            )
            cancelled = bridge.wait_response(stream_id, timeout=1.0)
            self.assertEqual(-32800, cancelled["error"]["code"])
            self.assertEqual("request cancelled", cancelled["error"]["message"])

            # Release the initial synchronization so graceful shutdown can
            # revoke and unregister the retained host publication obligation.
            bridge.send(
                {
                    "jsonrpc": "2.0",
                    "id": authorization["id"],
                    "result": {"status": "ready"},
                }
            )

    def test_provider_replacement_and_unregistration_cancel_active_streams(self) -> None:
        for mutation_tool in ("fixture_provider_update", "fixture_provider_unregister"):
            with self.subTest(mutation_tool=mutation_tool):
                bridge, catalog_requests, auth_requests = self.provider_bridge()
                with bridge:
                    bridge.initialize()
                    self.wait_for_provider_ready(bridge, catalog_requests, auth_requests)
                    stream_id = f"{mutation_tool}-stream"
                    accepted = bridge.request(
                        "provider/stream",
                        {
                            "stream_id": stream_id,
                            "provider_id": "fixture-provider",
                            "model_id": "fixture-model",
                            "request": {"hold": True, "prompt": "wait"},
                        },
                    )
                    self.assertTrue(accepted["result"]["accepted"])
                    bridge.wait_for(
                        lambda messages: next(
                            (
                                message
                                for message in messages
                                if message.get("method") == "provider/event"
                                and message.get("params", {}).get("stream_id") == stream_id
                                and message.get("params", {}).get("kind") == "text_delta"
                            ),
                            None,
                        ),
                        description="provider stream before declaration mutation",
                    )
                    mutation = self.v03_tool(bridge, mutation_tool)
                    self.assertNotIn("error", mutation)
                    mutation_method = (
                        "providers/update"
                        if mutation_tool == "fixture_provider_update"
                        else "providers/unregister"
                    )
                    mutation_request = bridge.wait_for(
                        lambda messages: next(
                            (
                                message
                                for message in messages
                                if message.get("method") == mutation_method
                            ),
                            None,
                        ),
                        description=f"{mutation_method} after terminal stream event",
                    )
                    stream_events = [
                        message
                        for message in bridge.messages
                        if message.get("method") == "provider/event"
                        and message.get("params", {}).get("stream_id") == stream_id
                    ]
                    terminals = [
                        message
                        for message in stream_events
                        if message["params"]["kind"] in {"finished", "error"}
                    ]
                    self.assertEqual(1, len(terminals))
                    self.assertEqual("error", terminals[0]["params"]["kind"])
                    self.assertEqual(
                        list(range(len(stream_events))),
                        [message["params"]["sequence"] for message in stream_events],
                    )
                    self.assertLess(
                        bridge.messages.index(terminals[0]),
                        bridge.messages.index(mutation_request),
                    )
                    deadline = time.monotonic() + 2.0
                    cancellation_status: dict | None = None
                    while time.monotonic() < deadline:
                        cancellation_status = self.v03_tool(bridge, "fixture_provider_cancel_status")
                        if cancellation_status.get("result", {}).get("content", [{}])[0].get("text") == "true":
                            break
                        time.sleep(0.02)
                    self.assertIsNotNone(cancellation_status)
                    self.assertEqual("true", cancellation_status["result"]["content"][0]["text"])
                    final_terminals = [
                        message
                        for message in bridge.messages
                        if message.get("method") == "provider/event"
                        and message.get("params", {}).get("stream_id") == stream_id
                        and message["params"]["kind"] in {"finished", "error"}
                    ]
                    self.assertEqual([terminals[0]], final_terminals)

    def test_api_03_admission_limit_rejects_excess_and_cancels_queued_tools(self) -> None:
        with BridgeProcess(api_version="0.3") as bridge:
            bridge.initialize(contract=v03_contract(providers=False))
            params = {
                "name": "pi",
                "arguments": {"tool_name": "fixture_hold", "arguments": {}},
                "context": {},
            }
            active_ids = [bridge.send_request("tool/call", params) for _ in range(4)]
            rejected = bridge.wait_response(bridge.send_request("tool/call", params))
            self.assertEqual(-32012, rejected["error"]["code"])
            self.assertEqual("extension resource exhausted", rejected["error"]["message"])
            for request_id in active_ids:
                bridge.send(
                    {
                        "jsonrpc": "2.0",
                        "method": "$/cancelRequest",
                        "params": {"id": request_id},
                    }
                )
            for request_id in active_ids:
                response = bridge.wait_response(request_id)
                self.assertEqual(-32800, response["error"]["code"])
                self.assertEqual("request cancelled", response["error"]["message"])

    def test_provider_mutations_and_header_hooks_fail_when_not_representable(self) -> None:
        with BridgeProcess(extension=UNSAFE_PROVIDER_EXTENSION, api_version="0.3") as bridge:
            response = bridge.request(
                "initialize", bridge.initialization_params(contract=v03_contract())
            )
            self.assertEqual(-32602, response["error"]["code"])
            self.assertEqual("invalid params", response["error"]["message"])
            self.assertNotIn("must-not-cross", json.dumps(response))

        with BridgeProcess(extension=PROVIDER_HEADERS_HOOK_EXTENSION, api_version="0.3") as bridge:
            response = bridge.request(
                "initialize", bridge.initialization_params(contract=v03_contract())
            )
            self.assertEqual(-32602, response["error"]["code"])
            self.assertEqual("invalid params", response["error"]["message"])

        with BridgeProcess(extension=PROVIDER_EXTENSION, api_version="0.3") as bridge:
            response = bridge.request(
                "initialize", bridge.initialization_params(contract=v03_contract(providers=False))
            )
            self.assertEqual(-32011, response["error"]["code"])
            self.assertEqual("extension capability mismatch", response["error"]["message"])

    def test_api_03_rejects_invalid_params_and_noncanonical_input_framing(self) -> None:
        with BridgeProcess(api_version="0.3") as bridge:
            bridge.initialize()
            invalid = bridge.request(
                "tool/call",
                {"name": "fixture_echo", "arguments": {}, "context": {}, "extra": True},
            )
            self.assertEqual(-32602, invalid["error"]["code"])
            self.assertEqual("invalid params", invalid["error"]["message"])

        with BridgeProcess(api_version="0.3") as bridge:
            bridge.initialize()
            # Object-key order is intentionally not canonical (jsonrpc before id).
            bridge.send_raw(
                b'{"jsonrpc":"2.0","id":99,"method":"shutdown","params":{}}\n'
            )
            bridge.process.wait(timeout=2.0)
            self.assertEqual(1, bridge.process.returncode)
            self.assertTrue(any("not canonical JSON" in line for line in bridge.stderr))


if __name__ == "__main__":
    unittest.main()
