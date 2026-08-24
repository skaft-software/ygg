from __future__ import annotations

import json
import os
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import unittest
from pathlib import Path

from ygg_browse.presentation import BrowsePresentation, PresentationPublisher
from tests.helpers import OWNER_CONTEXT


PACKAGE = Path(__file__).resolve().parents[1]
REPOSITORY = PACKAGE.parents[1]
TOOLS = {
    "browser_status",
    "browser_launch",
    "browser_tabs",
    "browser_open_url",
    "browser_snapshot",
    "browser_click",
    "browser_type",
    "browser_press",
    "browser_scroll",
    "browser_wait",
    "browser_screenshot",
    "browser_tab_close",
    "browser_close",
}


class PresentationTests(unittest.TestCase):
    def test_model_emits_generic_setup_tabs_actions_and_artifact(self) -> None:
        snapshots = []
        model = BrowsePresentation(lambda snapshot, _owner=None: snapshots.append(snapshot))
        model.update_setup(
            {
                "state": "ready",
                "detail": "Pinned browser ready.",
                "log_path": "~/.ygg/browse/install.log",
            }
        )
        model.update_browser(
            {
                "open": True,
                "selected_tab_id": "tab_123",
                "tabs": [
                    {
                        "tab_id": "tab_123",
                        "title": "Untrusted fixture title",
                        "url": "https://example.test/path",
                        "origin": "https://example.test",
                        "snapshot_generation": 3,
                        "selected": True,
                    }
                ],
            }
        )
        model.activity(
            "browse:screenshot",
            kind="artifact",
            state="succeeded",
            summary="Viewport screenshot published",
            artifact_id="artifact-123",
        )
        snapshot = snapshots[-1]
        self.assertEqual(snapshot["status"]["state"], "active")
        self.assertIn("https://example.test", snapshot["status"]["label"])
        node = snapshot["collection"]["nodes"][0]
        self.assertEqual(node["label"], "Tab tab_123")
        self.assertNotIn("Untrusted fixture title", node["label"])
        self.assertIn("Untrusted fixture title", snapshot["collection"]["detail"]["body"])
        self.assertNotIn("example.test", model.process_status())
        self.assertNotIn("tab", model.process_status())
        model.update_browser({"open": False, "tabs": [], "degraded": True})
        self.assertEqual(snapshots[-1]["status"]["state"], "degraded")
        self.assertEqual(model.process_status(), "Browse · degraded")
        self.assertEqual(snapshot["activities"][-1]["references"][0]["kind"], "artifact")
        self.assertEqual({action["command"] for action in snapshot["actions"]}, {"browse"})
        setup = next(action for action in snapshot["actions"] if action["id"] == "setup")
        self.assertTrue(setup["destructive"])
        reset = next(action for action in snapshot["actions"] if action["id"] == "reset-profile")
        self.assertTrue(reset["destructive"])

    def test_publisher_uses_background_owner_and_handler_auto_correlation(self) -> None:
        class Extension:
            initialized = True

            def __init__(self) -> None:
                self.request_id = None
                self.calls = []

            def publish_presentation(self, snapshot, **keywords):
                self.calls.append((snapshot, keywords))

        extension = Extension()
        publisher = PresentationPublisher(extension)
        owner = {
            "session_id": "session",
            "extension_instance_id": "instance",
            "process_generation": 1,
        }
        publisher({"status": {"state": "active", "label": "ready"}}, owner)
        self.assertEqual(extension.calls[-1][1], {"resource_owner": owner})
        extension.request_id = 7
        publisher({"status": {"state": "active", "label": "handler"}}, owner)
        self.assertEqual(extension.calls[-1][1], {"resource_owner": owner})
        self.assertEqual(extension.calls[-1][0]["revision"], 1)
        publisher({"status": {"state": "active", "label": "auto"}})
        self.assertEqual(extension.calls[-1][1], {})

    def test_owner_specific_browser_snapshots_carry_complete_background_scope(self) -> None:
        published = []
        model = BrowsePresentation(
            lambda snapshot, owner=None: published.append((snapshot, owner))
        )
        owner = {
            "session_id": "session",
            "extension_instance_id": "instance",
            "process_generation": 2,
        }
        model.update_browser(
            {
                "open": True,
                "selected_tab_id": "tab_owner",
                "tabs": [
                    {
                        "tab_id": "tab_owner",
                        "title": "Title",
                        "url": "https://example.test/",
                        "origin": "https://example.test",
                        "selected": True,
                    }
                ],
            },
            resource_owner=owner,
        )
        model.activity(
            "browse:confirmation",
            kind="confirmation",
            state="pending",
            summary="Waiting for confirmation",
            resource_owner=owner,
        )
        self.assertEqual(published[-1][1], owner)
        self.assertNotIn("session", json.dumps(published[-1][0]))
        second_owner = {
            "session_id": "other-session",
            "extension_instance_id": "other-instance",
            "process_generation": 3,
        }
        model.activity(
            "browse:status",
            kind="browser",
            state="running",
            summary="Inspecting browser state",
            resource_owner=second_owner,
        )
        switched_snapshot, switched_owner = published[-1]
        self.assertEqual(switched_owner, second_owner)
        encoded = json.dumps(switched_snapshot)
        self.assertNotIn("tab_owner", encoded)
        self.assertNotIn("browse:confirmation", encoded)

    def test_all_required_fixture_states_are_complete_and_bounded(self) -> None:
        expected = {
            "not-set-up",
            "setup-confirmation",
            "installing",
            "ready",
            "open-tab-list-detail",
            "navigation-running",
            "consequential-confirmation",
            "artifact-published",
            "degraded-profile-lock",
            "restarted",
            "stale-generation-cleared",
            "closed",
        }
        fixtures = {path.stem: path for path in (PACKAGE / "presentation-fixtures").glob("*.json")}
        self.assertEqual(set(fixtures), expected)
        for name, path in fixtures.items():
            with self.subTest(name=name):
                value = json.loads(path.read_text(encoding="utf-8"))
                self.assertIsInstance(value["revision"], int)
                self.assertIn("status", value)
                self.assertIsInstance(value["activities"], list)
                self.assertIn(value["collection"]["kind"], {"list", "tree"})
                self.assertEqual({action["command"] for action in value["actions"]}, {"browse"})
                setup = next(action for action in value["actions"] if action["id"] == "setup")
                self.assertTrue(setup["destructive"])
                encoded = json.dumps(value)
                self.assertLess(len(encoded.encode("utf-8")), 256 * 1024)
                self.assertNotIn("?token=", encoded)
                self.assertNotIn("typed-secret", encoded)
                self.assertNotIn("password-value", encoded)


class PackageTests(unittest.TestCase):
    def test_manifest_surface_metadata_and_executable(self) -> None:
        manifest = tomllib.loads((PACKAGE / "extension.toml").read_text(encoding="utf-8"))
        self.assertEqual(manifest["name"], "ygg-browse")
        self.assertEqual(manifest["version"], "0.1.0")
        self.assertEqual(manifest["api_version"], "0.2")
        self.assertEqual(manifest["requires_ygg"], "=0.6.0")
        self.assertEqual(set(manifest["contributes"]["tools"]), TOOLS)
        self.assertEqual(manifest["contributes"]["commands"], ["browse"])
        self.assertTrue(manifest["contributes"]["confirmations"])
        self.assertTrue(manifest["contributes"]["presentation"])
        self.assertEqual(manifest["capabilities"]["filesystem"], "unrestricted")
        self.assertTrue(manifest["capabilities"]["process"])
        self.assertTrue(manifest["capabilities"]["network"])
        entrypoint = PACKAGE / manifest["entrypoint"]["command"]
        self.assertTrue(entrypoint.is_file())
        self.assertTrue(os.access(entrypoint, os.X_OK))
        self.assertTrue((PACKAGE / "README.md").is_file())
        self.assertTrue((PACKAGE / "LICENSE").is_file())
        self.assertTrue((PACKAGE / "CHANGELOG.md").is_file())
        worker_source = (PACKAGE / "ygg_browse" / "worker.py").read_text(encoding="utf-8")
        self.assertIn("headless=False", worker_source)
        self.assertIn("launch_persistent_context", worker_source)
        self.assertNotIn("headless=True", worker_source)

    def test_packaged_skill_declares_all_tools_plus_read_and_limitation(self) -> None:
        skill = (PACKAGE / "skills" / "ygg-browse" / "SKILL.md").read_text(encoding="utf-8")
        required_section = skill.split("required-tools:", 1)[1].split("tags:", 1)[0]
        required = {
            line.strip()[2:]
            for line in required_section.splitlines()
            if line.strip().startswith("- ")
        }
        self.assertEqual(required, TOOLS | {"read"})
        self.assertIn("Do not activate it for a partial or failed setup", skill)
        self.assertIn("refuses this skill invocation", skill)
        self.assertIn("BEGIN UNTRUSTED BROWSER CONTENT", skill)

    def test_vendored_sdk_is_byte_synchronized_and_has_revision_guard(self) -> None:
        shared = REPOSITORY / "sdk" / "python" / "ygg_extension"
        vendor = PACKAGE / "vendor" / "ygg_extension"
        for name in ("__init__.py", "extension.py", "protocol.py"):
            self.assertEqual((vendor / name).read_bytes(), (shared / name).read_bytes(), name)
        self.assertIn(
            b"MAX_PRESENTATION_REVISION = (2**53) - 1",
            (vendor / "extension.py").read_bytes(),
        )

    def test_import_is_inert_without_playwright_or_owned_state(self) -> None:
        with tempfile.TemporaryDirectory() as home:
            environment = os.environ.copy()
            environment["HOME"] = home
            environment["PYTHONDONTWRITEBYTECODE"] = "1"
            environment["PYTHONPATH"] = os.pathsep.join(
                [str(PACKAGE / "vendor"), str(PACKAGE)]
            )
            process = subprocess.run(
                [
                    sys.executable,
                    "-c",
                    "import pathlib,sys,threading,ygg_browse.runtime; "
                    "assert 'playwright' not in sys.modules; "
                    "assert [t.name for t in threading.enumerate()] == ['MainThread']; "
                    "assert not (pathlib.Path.home()/'.ygg'/'browse').exists()",
                ],
                env=environment,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                timeout=10,
            )
            self.assertEqual(process.returncode, 0, process.stderr)
            self.assertFalse((Path(home) / ".ygg" / "browse").exists())

    def test_entrypoint_handshake_and_shutdown_do_not_install_or_launch(self) -> None:
        with tempfile.TemporaryDirectory() as home:
            initialize = {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "api_version": "0.2",
                    "ygg_version": "0.6.0",
                    "extension": {
                        "name": "ygg-browse",
                        "version": "0.1.0",
                        "manifest_path": str(PACKAGE / "extension.toml"),
                        "source": "explicit",
                    },
                    "workspace": str(REPOSITORY),
                    "capabilities": {
                        "filesystem": "unrestricted",
                        "process": True,
                        "network": True,
                    },
                    "contributes": {
                        "tools": sorted(TOOLS),
                        "commands": ["browse"],
                        "ui": ["status"],
                        "confirmations": True,
                        "presentation": True,
                    },
                    "host": {},
                    "protocol": {
                        "version": "0.2",
                        "required_features": ["request_cancellation", "content_parts"],
                        "optional_features": ["artifacts"],
                        "limits": {"max_concurrent_requests": 8},
                    },
                },
            }
            status_request = {
                "jsonrpc": "2.0",
                "id": 2,
                "method": "command/execute",
                "params": {
                    "name": "browse",
                    "arguments": ["status"],
                    "context": OWNER_CONTEXT,
                },
            }
            shutdown = {"jsonrpc": "2.0", "id": 3, "method": "shutdown", "params": {}}
            environment = os.environ.copy()
            environment["HOME"] = home
            environment["PYTHONDONTWRITEBYTECODE"] = "1"
            environment["YGG_EXTENSION_DIR"] = str(PACKAGE)
            staged_entrypoint = Path(home) / "extension.py"
            staged_entrypoint.write_bytes((PACKAGE / "extension.py").read_bytes())
            staged_entrypoint.chmod(0o755)
            process = subprocess.run(
                [str(staged_entrypoint)],
                input=(
                    json.dumps(initialize)
                    + "\n"
                    + json.dumps(status_request)
                    + "\n"
                    + json.dumps(shutdown)
                    + "\n"
                ),
                env=environment,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                timeout=10,
            )
            self.assertEqual(process.returncode, 0, process.stderr)
            messages = [json.loads(line) for line in process.stdout.splitlines() if line]
            by_id = {message.get("id"): message for message in messages if "id" in message}
            self.assertEqual(by_id[1]["result"]["api_version"], "0.2")
            self.assertEqual({tool["name"] for tool in by_id[1]["result"]["tools"]}, TOOLS)
            self.assertIn("Browse setup: not_set_up", by_id[2]["result"]["text"])
            self.assertEqual(by_id[3]["result"], {})
            self.assertFalse((Path(home) / ".ygg" / "browse").exists())

    def test_local_release_packaging_smoke_contains_runtime_skill_and_fixtures(self) -> None:
        script = REPOSITORY / "scripts" / "package-ygg-extension-release.sh"
        if not script.is_file():
            self.skipTest("generic extension release packager is not present")
        with tempfile.TemporaryDirectory() as output:
            process = subprocess.run(
                [str(script), "ygg-browse", output, "v0.6.0", str(PACKAGE)],
                cwd=REPOSITORY,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                timeout=30,
            )
            self.assertEqual(process.returncode, 0, process.stderr)
            archive = Path(output) / "ygg-browse-0.6.0.tar.gz"
            self.assertTrue(archive.is_file())
            with tarfile.open(archive, "r:gz") as bundle:
                names = set(bundle.getnames())
            required = {
                "ygg-browse/extension.toml",
                "ygg-browse/extension.py",
                "ygg-browse/ygg_browse/runtime.py",
                "ygg-browse/ygg_browse/worker.py",
                "ygg-browse/vendor/ygg_extension/extension.py",
                "ygg-browse/skills/ygg-browse/SKILL.md",
                "ygg-browse/presentation-fixtures/installing.json",
                "ygg-browse/presentation-fixtures/stale-generation-cleared.json",
                "ygg-browse/README.md",
                "ygg-browse/LICENSE",
            }
            self.assertTrue(required <= names, sorted(required - names))


if __name__ == "__main__":
    unittest.main()
