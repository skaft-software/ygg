"""Executable fixture coverage for the Pi 0.84.4 conformance ledger."""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest

try:
    from .helpers import BridgeProcess, NODE
except ImportError:  # unittest discovery imports this file as a top-level module.
    from helpers import BridgeProcess, NODE


COMPAT_ROOT = Path(__file__).resolve().parents[1]
PROFILE_PATH = COMPAT_ROOT / "profiles/0.84.4.json"
FIXTURE_ROOT = COMPAT_ROOT / "tests/fixtures/conformance"
CONFORMANCE = COMPAT_ROOT / "conformance.py"


def fixture_document(name: str) -> dict:
    return json.loads((FIXTURE_ROOT / name).read_text(encoding="utf-8"))


def conformance_module():
    spec = importlib.util.spec_from_file_location("pi_conformance_test_module", CONFORMANCE)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def method_messages(messages: list[dict], method: str) -> list[dict]:
    return [message for message in messages if message.get("method") == method]


class ConformanceHarnessTests(unittest.TestCase):
    def test_checked_in_ledger_gate_is_machine_readable_and_not_a_real_runtime_claim(self) -> None:
        completed = subprocess.run(
            [sys.executable, str(CONFORMANCE), "--check", "--json"],
            cwd=COMPAT_ROOT,
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(0, completed.returncode, completed.stderr)
        report = json.loads(completed.stdout)
        self.assertTrue(report["ok"])
        self.assertEqual("not_supplied", report["real_runtime"])
        self.assertEqual(118, report["public_surface_rows"])
        self.assertEqual(78, report["official_examples"])
        self.assertEqual(33, report["tui_audit_rows"])
        self.assertEqual(6, report["plan_journeys"])

    def test_full_gate_refuses_before_loading_without_network_isolation(self) -> None:
        completed = subprocess.run(
            [sys.executable, str(CONFORMANCE), "--full", "--json"],
            cwd=COMPAT_ROOT,
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(1, completed.returncode)
        self.assertIn("--network-isolated", json.loads(completed.stdout)["error"])

    def test_full_gate_compares_the_entire_selected_package_payload(self) -> None:
        module = conformance_module()
        with tempfile.TemporaryDirectory() as temporary:
            temporary_path = Path(temporary)
            root = temporary_path / "pi-coding-agent"
            (root / "dist").mkdir(parents=True)
            (root / "package.json").write_text(
                json.dumps({"name": "@earendil-works/pi-coding-agent", "version": "0.84.4"}),
                encoding="utf-8",
            )
            (root / "dist/index.js").write_text("export {};\n", encoding="utf-8")
            tarball = temporary_path / "pi-coding-agent.tgz"
            with tarfile.open(tarball, "w:gz") as archive:
                for relative in ("package.json", "dist/index.js"):
                    archive.add(root / relative, arcname=f"package/{relative}")

            tui = root / "node_modules/@earendil-works/pi-tui"
            tui.mkdir(parents=True)
            self.assertEqual(
                tui,
                module.node_resolved_package(root, "@earendil-works/pi-tui"),
            )
            self.assertEqual(
                root.absolute(),
                module.verify_package_root(
                    tarball,
                    root,
                    "@earendil-works/pi-coding-agent",
                    "dist/index.js",
                ),
            )

            (root / "unexpected.js").write_text("export default null;\n", encoding="utf-8")
            with self.assertRaisesRegex(module.GateFailure, "file inventory differs"):
                module.verify_package_root(
                    tarball,
                    root,
                    "@earendil-works/pi-coding-agent",
                    "dist/index.js",
                )


@unittest.skipUnless(NODE, "node is required for Pi conformance fixture subprocesses")
class PublicSurfaceFixtureTests(unittest.TestCase):
    def test_every_non_event_surface_runs_its_declared_fixture(self) -> None:
        profile = json.loads(PROFILE_PATH.read_text(encoding="utf-8"))
        fixtures = fixture_document("public-surfaces.json")["fixtures"]
        expected = {
            f"{area}.{surface}"
            for area, surfaces in profile["public_surface"].items()
            if area != "events"
            for surface in surfaces
        }
        declared = {
            fixture["surface"]
            for fixture in fixtures
            if fixture["kind"] not in {"event_registration", "lifecycle_or_hook"}
        }
        self.assertEqual(expected, declared)

        with BridgeProcess() as bridge:
            bridge.handlers["input/request"] = lambda _message: {"value": "1"}
            bridge.handlers["confirmation/request"] = lambda _message: {"confirmed": True}
            bridge.initialize("runtime_commands")
            for surface in sorted(declared):
                response = bridge.request(
                    "command/execute",
                    {"name": "surface-probe", "arguments": [surface]},
                )
                self.assertNotIn("error", response, surface)
                self.assertTrue(
                    any(item.startswith(f"surface:{surface}:") for item in bridge.notifications()),
                    surface,
                )

    def test_unbridged_events_and_registration_surfaces_are_diagnosed_at_startup(self) -> None:
        fixtures = fixture_document("public-surfaces.json")["fixtures"]
        unbridged = [
            fixture["target"]
            for fixture in fixtures
            if fixture["kind"] == "event_registration"
        ]
        self.assertGreater(len(unbridged), 0)
        with BridgeProcess(fixture_events=unbridged) as bridge:
            bridge.initialize()
            for event in unbridged:
                self.assertTrue(
                    any(f"event {event} is unavailable" in line for line in bridge.stderr),
                    event,
                )

        with BridgeProcess(fixture_mode="registration") as bridge:
            bridge.initialize()
            for label in ("shortcuts", "flags", "message renderers", "entry renderers", "markdown transformer"):
                self.assertTrue(any(f"{label} is unavailable" in line for line in bridge.stderr), label)

    def test_every_bridged_event_has_an_executable_lifecycle_or_hook_path(self) -> None:
        fixtures = fixture_document("public-surfaces.json")["fixtures"]
        bridged = {
            fixture["target"]
            for fixture in fixtures
            if fixture["kind"] == "lifecycle_or_hook"
        }
        actions = {
            "session_start": lambda bridge: bridge.request("session/started", {}),
            "session_shutdown": lambda bridge: bridge.request("session/settled", {}),
            "context": lambda bridge: bridge.request("context/collect", {"prompt": "fixture"}),
            "before_agent_start": lambda bridge: bridge.request(
                "hook/run", {"hook": "before_prompt", "payload": {"prompt": "fixture"}}
            ),
            "agent_start": lambda bridge: bridge.request("turn/started", {}),
            "agent_end": lambda bridge: bridge.request("turn/settled", {"outcome": "cancelled"}),
            "agent_settled": lambda bridge: bridge.request("turn/settled", {"outcome": "cancelled"}),
            "turn_start": lambda bridge: bridge.request("turn/started", {}),
            "turn_end": lambda bridge: bridge.request("turn/settled", {"outcome": "cancelled"}),
            "tool_execution_start": lambda bridge: bridge.request(
                "tool/started", {"tool_call_id": "fixture-start", "tool_name": "bash"}
            ),
            "tool_execution_update": lambda bridge: bridge.request(
                "tool/call", {"name": "fixture_progress", "arguments": {}, "catalog_revision": 0}
            ),
            "tool_execution_end": lambda bridge: bridge.request(
                "tool/settled", {"tool_call_id": "fixture-end", "tool_name": "bash", "outcome": "completed"}
            ),
            "tool_call": lambda bridge: bridge.request(
                "hook/run", {"hook": "before_tool_call", "payload": {"name": "bash", "arguments": {}}}
            ),
            "tool_result": lambda bridge: bridge.request(
                "tool/call", {"name": "fixture_echo", "arguments": {}, "catalog_revision": 0}
            ),
        }
        self.assertEqual(set(actions), bridged)
        with BridgeProcess() as bridge:
            bridge.initialize("request_progress", "lifecycle_events")
            for event, action in actions.items():
                response = action(bridge)
                self.assertNotIn("error", response, event)
            notifications = bridge.notifications()
            for event in bridged:
                self.assertIn(f"event:{event}:start", notifications, event)


@unittest.skipUnless(NODE, "node is required for Pi conformance fixture subprocesses")
class DeferredPlanModeSurfaceTests(unittest.TestCase):
    DEFERRED_HOST_SEAMS = [
        "tool_policy",
        "session_state",
        "messages",
        "widgets",
        "editor",
        "shortcuts",
        "flags",
    ]

    def test_plan_mode_host_control_seams_remain_explicitly_deferred(self) -> None:
        plan = fixture_document("plan-mode-journey.json")
        self.assertEqual(self.DEFERRED_HOST_SEAMS, plan["deferred_host_seams"])
        self.assertTrue(
            all(row["assertion"].startswith(("Deferred:", "Supported:")) for row in plan["journeys"])
        )

        # A future host may define a bounded projection, but this integration
        # deliberately keeps the Pi bridge's shortcut, session-control, and
        # editor/widget remainder out of the live protocol. Supplying a shape
        # that resembles that future projection must not silently enable it.
        with BridgeProcess(fixture_mode="registration") as bridge:
            bridge.initialize(
                "runtime_commands",
                host={"pi_compat": {"features": self.DEFERRED_HOST_SEAMS}},
            )
            for label in ("shortcuts", "flags", "message renderers", "entry renderers", "markdown transformer"):
                self.assertTrue(any(f"{label} is unavailable" in line for line in bridge.stderr), label)
            for surface in (
                "extension_api.sendMessage",
                "extension_api.sendUserMessage",
                "extension_api.appendEntry",
                "extension_api.setSessionName",
                "extension_api.setActiveTools",
                "ui_context.setWidget",
                "ui_context.editor",
                "context.sessionManager",
            ):
                response = bridge.request(
                    "command/execute",
                    {"name": "surface-probe", "arguments": [surface]},
                )
                self.assertNotIn("error", response, surface)
                self.assertIn(f"surface:{surface}:explicit", bridge.notifications(), surface)


if __name__ == "__main__":
    unittest.main()
