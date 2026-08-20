from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest

try:
    import tomllib
except ImportError:  # pragma: no cover
    tomllib = None

try:
    from .helpers import FIXTURES, REPOSITORY, ROOT, initialize_request, rpc_request
except ImportError:
    from helpers import FIXTURES, REPOSITORY, ROOT, initialize_request, rpc_request


class ReleaseTests(unittest.TestCase):
    def test_manifest_is_exact_api_02_bundle_metadata_without_package_toml(self):
        self.assertIsNotNone(tomllib)
        manifest = tomllib.loads((ROOT / "extension.toml").read_text(encoding="utf-8"))
        self.assertEqual(manifest["name"], "ygg-subagents")
        self.assertEqual(manifest["version"], "0.1.0")
        self.assertEqual(manifest["api_version"], "0.2")
        self.assertEqual(manifest["requires_ygg"], "=0.5.0")
        self.assertEqual(manifest["entrypoint"]["command"], "ygg-subagents")
        self.assertEqual(manifest["capabilities"]["filesystem"], "none")
        self.assertFalse(manifest["capabilities"]["process"])
        self.assertFalse(manifest["capabilities"]["network"])
        self.assertEqual(
            manifest["contributes"]["tools"],
            ["subagent_spawn", "subagent_status", "subagent_wait", "subagent_stop"],
        )
        self.assertEqual(manifest["contributes"]["commands"], ["subagents"])
        self.assertTrue(manifest["contributes"]["presentation"])
        self.assertFalse((ROOT / "package.toml").exists())

    def test_bundle_is_self_contained_and_vendored_sdk_is_synchronized(self):
        required = (
            "ygg-subagents",
            "ygg_subagents/runtime.py",
            "ygg_subagents/orchestrator.py",
            "ygg_subagents/presentation.py",
            "vendor/ygg_extension/__init__.py",
            "vendor/ygg_extension/extension.py",
            "vendor/ygg_extension/protocol.py",
            "skills/ygg-subagents/SKILL.md",
            "fixtures/fake_agent_sessions.py",
            "release-smoke.py",
            "README.md",
            "CHANGELOG.md",
            "LICENSE",
        )
        for relative in required:
            self.assertTrue((ROOT / relative).is_file(), relative)
        for name in ("__init__.py", "extension.py", "protocol.py"):
            source = REPOSITORY / "sdk" / "python" / "ygg_extension" / name
            vendored = ROOT / "vendor" / "ygg_extension" / name
            self.assertEqual(
                hashlib.sha256(source.read_bytes()).hexdigest(),
                hashlib.sha256(vendored.read_bytes()).hexdigest(),
                "vendored SDK drift: %s" % name,
            )

    def test_entrypoint_release_handshake_and_shutdown_smoke(self):
        frames = [initialize_request(), rpc_request(900, "shutdown", {})]
        completed = subprocess.run(
            [str(ROOT / "ygg-subagents")],
            cwd=ROOT,
            input="".join(json.dumps(frame, separators=(",", ":")) + "\n" for frame in frames),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=8,
            check=False,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        messages = [json.loads(line) for line in completed.stdout.splitlines() if line]
        initialized = next(message for message in messages if message.get("id") == 1)
        shutdown = next(message for message in messages if message.get("id") == 900)
        self.assertEqual(initialized["result"]["api_version"], "0.2")
        self.assertIn("agent_sessions", initialized["result"]["protocol"]["features"])
        self.assertEqual(shutdown["result"], {})
        self.assertTrue(os.access(ROOT / "ygg-subagents", os.X_OK))
        self.assertTrue(os.access(ROOT / "release-smoke.py", os.X_OK))

    def test_release_archive_has_one_regular_portable_root(self):
        with tempfile.TemporaryDirectory() as directory:
            archive = Path(directory) / "ygg-subagents-0.1.0.tar.gz"
            with tarfile.open(archive, "w:gz") as bundle:
                for path in sorted(ROOT.rglob("*")):
                    if "__pycache__" in path.parts or ".pytest_cache" in path.parts:
                        continue
                    relative = path.relative_to(ROOT.parent)
                    bundle.add(path, arcname=relative, recursive=False)
            with tarfile.open(archive, "r:gz") as bundle:
                members = bundle.getmembers()
            self.assertTrue(members)
            self.assertEqual({member.name.split("/", 1)[0] for member in members}, {"ygg-subagents"})
            self.assertTrue(all(member.isfile() or member.isdir() for member in members))
            self.assertTrue(all(not member.issym() and not member.islnk() for member in members))
            self.assertTrue(all(".." not in Path(member.name).parts for member in members))
            self.assertLess(archive.stat().st_size, 2 * 1024 * 1024)

    def test_every_json_fixture_is_strict_and_covers_acceptance_states(self):
        fixtures = {}
        for path in FIXTURES.rglob("*.json"):
            with self.subTest(path=path):
                fixtures[str(path.relative_to(FIXTURES))] = json.loads(
                    path.read_text(encoding="utf-8")
                )
        self.assertIn("presentation/live-tree.json", fixtures)
        lifecycle = fixtures["presentation/lifecycle-states.json"]
        states = {item["extension"] for item in lifecycle["states"]}
        self.assertTrue(
            {"queued", "running", "done", "failed", "stopped", "timed_out", "cancelled", "orphaned", "restarted"}.issubset(states)
        )
        reconnect = fixtures["presentation/reconnect-resync.json"]
        self.assertEqual(reconnect["completion_delivery"]["duplicate_parent_turns"], 0)

    def test_packaged_skill_is_explicit_read_only_and_non_recursive(self):
        skill = (ROOT / "skills" / "ygg-subagents" / "SKILL.md").read_text(encoding="utf-8")
        self.assertTrue(skill.startswith("---\n"))
        for tool in ("subagent_spawn", "subagent_status", "subagent_wait", "subagent_stop"):
            self.assertIn("  - %s" % tool, skill)
        self.assertIn("read-only", skill)
        self.assertIn("V1 has no writer profile", skill)
        self.assertIn("shared filesystem is not isolation", skill)
        self.assertIn("Do not use this skill to create team chat", skill)

    def test_release_smoke_reports_quality_resource_duplicates_and_failures(self):
        completed = subprocess.run(
            [
                str(ROOT / "release-smoke.py"),
                "--direct",
                str(FIXTURES / "smoke" / "direct.json"),
                "--subagents",
                str(FIXTURES / "smoke" / "subagents.json"),
                "--require-gain",
            ],
            cwd=ROOT,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=5,
            check=False,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        report = json.loads(completed.stdout)
        self.assertGreater(report["quality_gain"], 0)
        self.assertIn("tokens", report["direct"])
        self.assertIn("wall_time_ms", report["subagents"])
        self.assertIn("cpu_time_ms", report["subagents"])
        self.assertIn("peak_rss_bytes", report["subagents"])
        self.assertEqual(report["subagents"]["duplicate_findings"], 1)
        self.assertIn("one_duplicate_finding", report["failure_classes"])

    def test_readme_documents_trust_inheritance_limits_and_generic_host_boundary(self):
        readme = (ROOT / "README.md").read_text(encoding="utf-8")
        for required in (
            "Explicitly enable and trust",
            "shared cwd/filesystem is **not isolation**",
            "There is no writer profile in V1",
            "host-owned `agent_sessions` service",
            "completion mailbox claim/ack",
            "presentation/update",
            "/subagents",
            "host binds API `0.2` command requests",
            "complete process-host rebuild creates a new service boundary",
            "release-smoke.py",
        ):
            self.assertIn(required, readme)
        catalog = (ROOT.parent / "release-catalog.txt").read_text(encoding="utf-8").splitlines()
        self.assertIn("ygg-subagents", catalog)


if __name__ == "__main__":
    unittest.main()
