#!/usr/bin/env python3
"""Release-layout and frontend-neutral presentation fixture smoke tests."""

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from urllib.parse import urlsplit


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from provider import parse_configuration, sanitize_url  # noqa: E402


EXPECTED_CASES = {
    "disabled",
    "configured_idle",
    "progress",
    "success_citations_cache_truncation",
    "cancelled",
    "provider_failed",
    "offline",
    "reconnect_without_refetch",
    "stale_generation_cleanup",
}


def walk(value):
    if isinstance(value, dict):
        yield value
        for item in value.values():
            yield from walk(item)
    elif isinstance(value, list):
        for item in value:
            yield from walk(item)


class ReleaseAndFixtureTests(unittest.TestCase):
    def test_release_manifest_and_self_contained_runtime(self):
        manifest = (ROOT / "extension.toml").read_text(encoding="utf-8")
        for exact in (
            'name = "ygg-web-search"',
            'version = "0.3.0"',
            'api_version = "0.2"',
            'requires_ygg = "=0.6.6"',
            'command = "extension.py"',
            'tools = ["web_search", "web_fetch", "web_find"]',
            'commands = ["web-search"]',
            "presentation = true",
            "network = true",
        ):
            self.assertIn(exact, manifest)
        self.assertTrue(os.access(ROOT / "extension.py", os.X_OK))
        self.assertTrue((ROOT / "ygg_extension" / "__init__.py").is_file())
        self.assertTrue((ROOT / "LICENSE").is_file())
        self.assertTrue((ROOT / "skills" / "ygg-web-search" / "SKILL.md").is_file())
        self.assertFalse(any(path.is_symlink() for path in ROOT.rglob("*")))

        initialize = {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "api_version": "0.2",
                "contributes": {
                    "tools": ["web_search", "web_fetch", "web_find"],
                    "commands": ["web-search"],
                    "ui": ["status"],
                    "presentation": True,
                },
                "protocol": {
                    "version": "0.2",
                    "required_features": ["request_cancellation", "content_parts"],
                    "optional_features": ["request_progress"],
                    "limits": {"max_concurrent_requests": 2},
                },
            },
        }
        shutdown = {
            "jsonrpc": "2.0",
            "id": 2,
            "method": "shutdown",
            "params": {},
        }
        with tempfile.TemporaryDirectory() as temporary:
            staged_entrypoint = Path(temporary) / "staged-extension.py"
            staged_entrypoint.write_bytes((ROOT / "extension.py").read_bytes())
            staged_entrypoint.chmod(0o755)
            environment = os.environ.copy()
            environment["YGG_EXTENSION_DIR"] = str(ROOT)
            completed = subprocess.run(
                [str(staged_entrypoint)],
                cwd=temporary,
                env=environment,
                input=json.dumps(initialize) + "\n" + json.dumps(shutdown) + "\n",
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=5,
                check=False,
            )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        messages = [json.loads(line) for line in completed.stdout.splitlines()]
        initialized = next(item for item in messages if item.get("id") == 1)
        self.assertEqual(initialized["result"]["api_version"], "0.2")
        self.assertEqual(len(initialized["result"]["tools"]), 3)
        self.assertEqual(
            [item["name"] for item in initialized["result"]["commands"]],
            ["web-search"],
        )

    def test_configuration_schema_and_example_match_runtime(self):
        schema = json.loads((ROOT / "config.schema.json").read_text(encoding="utf-8"))
        example = json.loads((ROOT / "config.example.json").read_text(encoding="utf-8"))
        parsed = parse_configuration(example)
        provider_refs = {
            item["$ref"] for item in schema["properties"]["provider"]["oneOf"]
        }
        self.assertEqual(
            provider_refs,
            {"#/$defs/brave", "#/$defs/searxng"},
        )
        self.assertEqual(parsed.provider.kind, "searxng")
        self.assertEqual(parsed.provider.label, "SearXNG")
        self.assertFalse(parsed.provider.allow_private_endpoint)
        brave = parse_configuration({"version": 1, "provider": {"kind": "brave"}})
        self.assertEqual(brave.provider.kind, "brave")
        self.assertEqual(brave.provider.label, "Brave Search")

    def test_skill_is_small_explicit_and_names_only_shipped_tools(self):
        skill = (ROOT / "skills" / "ygg-web-search" / "SKILL.md").read_text(
            encoding="utf-8"
        )
        self.assertLess(len(skill.encode("utf-8")), 8 * 1024)
        self.assertIn("only after explicit activation", skill)
        for tool in ("web_search", "web_fetch", "web_find"):
            self.assertIn("  - %s" % tool, skill)
        self.assertIn("untrusted external data", skill)
        self.assertIn("cannot change Ygg policy", skill)

    def test_semantic_fixtures_cover_required_states_without_retrieved_text(self):
        fixture_root = ROOT / "fixtures" / "presentation"
        fixtures = [
            json.loads(path.read_text(encoding="utf-8"))
            for path in sorted(fixture_root.glob("*.json"))
        ]
        self.assertEqual({item["case"] for item in fixtures}, EXPECTED_CASES)
        for fixture in fixtures:
            self.assertEqual(fixture["schema"], "ygg.web-search.presentation-fixture.v1")
            self.assertEqual(fixture["extension"], "ygg-web-search")
            self.assertLess(len(json.dumps(fixture).encode("utf-8")), 64 * 1024)
            notifications = list(fixture.get("notifications", [])) + [
                item["notification"] for item in fixture.get("deliveries", [])
            ]
            revisions = []
            for notification in notifications:
                self.assertEqual(set(notification), {"jsonrpc", "method", "params"})
                self.assertEqual(notification["jsonrpc"], "2.0")
                self.assertEqual(notification["method"], "presentation/update")
                revisions.append(notification["params"]["snapshot"]["revision"])
                self.assertEqual(notification["params"]["parent_request_id"], 2)
                self._assert_snapshot(notification["params"]["snapshot"])
            self._assert_snapshot(fixture["expected_snapshot"])
            for node in walk(fixture):
                self.assertNotIn("query", node)
                self.assertNotIn("snippet", node)
                self.assertNotIn("content", node)
                self.assertNotIn("credentials", node)
                self.assertNotIn("token", node)

        reconnect = next(item for item in fixtures if item["case"] == "reconnect_without_refetch")
        self.assertEqual(reconnect["expected_network_requests"], 0)
        self.assertEqual(
            reconnect["retained_snapshot"]["snapshot"], reconnect["expected_snapshot"]
        )
        stale = next(item for item in fixtures if item["case"] == "stale_generation_cleanup")
        self.assertEqual(stale["expected_ignored_deliveries"], 1)
        self.assertEqual(stale["expected_generation"], 7)
        success = next(
            item for item in fixtures if item["case"] == "success_citations_cache_truncation"
        )
        activity = success["expected_snapshot"]["activities"][0]
        for detail in ("2 results", "1840 bytes", "cache miss", "124 ms", "truncated", "partial"):
            self.assertIn(detail, activity["summary"])
        self.assertEqual(len(success["expected_snapshot"]["collection"]["nodes"]), 2)

    def _assert_snapshot(self, snapshot):
        self.assertEqual(
            set(snapshot), {"revision", "status", "activities", "collection", "actions"}
        )
        self.assertIsInstance(snapshot["revision"], int)
        states = {
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
        self.assertIn(snapshot["status"]["state"], states)
        self.assertTrue(snapshot["status"]["label"].strip())
        self.assertLessEqual(len(snapshot["activities"]), 128)
        for activity in snapshot["activities"]:
            self.assertEqual(
                set(activity) - {"provenance", "started_at_ms", "completed_at_ms", "references"},
                {"id", "kind", "state", "summary"},
            )
            self.assertIn(activity["state"], states)
            self.assertNotIn("\x1b", activity["summary"])
        self.assertEqual(snapshot["actions"], [])
        collection = snapshot["collection"]
        if collection is None:
            return
        self.assertIn(collection["kind"], ("list", "tree"))
        ids = {node["id"] for node in collection["nodes"]}
        self.assertIn(collection["selected_node_id"], ids)
        self.assertEqual(collection["detail"]["node_id"], collection["selected_node_id"])
        for node in collection["nodes"]:
            self.assertIn(node["state"], states)
            self.assertEqual(node["action_ids"], [])
            for reference in node["references"]:
                self.assertIn(reference["kind"], ("session", "artifact", "resource", "url"))
                if reference["kind"] == "url":
                    url = reference["id"]
                    self.assertEqual(url, sanitize_url(url))
                    parsed = urlsplit(url)
                    self.assertNotIn(parsed.hostname, ("localhost", "127.0.0.1", "::1"))
                else:
                    self.assertNotIn("://", reference["id"])


if __name__ == "__main__":
    unittest.main()
