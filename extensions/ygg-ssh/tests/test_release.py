from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest

try:
    import tomllib
except ImportError:  # pragma: no cover
    tomllib = None

from .helpers import ROOT


REPOSITORY = ROOT.parents[1]


class ReleaseSmokeTests(unittest.TestCase):
    def test_manifest_is_api_02_exact_ygg_release_metadata(self):
        self.assertIsNotNone(tomllib)
        manifest = tomllib.loads((ROOT / "extension.toml").read_text(encoding="utf-8"))
        self.assertEqual(manifest["name"], "ygg-ssh")
        self.assertEqual(manifest["version"], "0.1.0")
        self.assertEqual(manifest["api_version"], "0.2")
        self.assertEqual(manifest["requires_ygg"], "=0.6.0-dev")
        self.assertEqual(manifest["entrypoint"]["command"], "ygg-ssh")
        self.assertEqual(manifest["capabilities"]["environment"], ["SSH_AUTH_SOCK"])
        self.assertTrue(manifest["capabilities"]["network"])
        self.assertTrue(manifest["capabilities"]["process"])
        self.assertTrue(manifest["contributes"]["presentation"])
        self.assertTrue(manifest["contributes"]["confirmations"])
        self.assertFalse((ROOT / "package.toml").exists())

    def test_entrypoint_vendored_sdk_and_release_files_are_self_contained(self):
        self.assertTrue(os.access(ROOT / "ygg-ssh", os.X_OK))
        for relative in (
            "vendor/ygg_extension/__init__.py",
            "vendor/ygg_extension/extension.py",
            "vendor/ygg_extension/protocol.py",
            "ygg_ssh/runtime.py",
            "config.schema.json",
            "config.example.json",
            "fixtures/fake_ssh.py",
            "README.md",
            "CHANGELOG.md",
            "LICENSE",
        ):
            self.assertTrue((ROOT / relative).is_file(), relative)
        for filename in ("__init__.py", "extension.py", "protocol.py"):
            self.assertEqual(
                (ROOT / "vendor" / "ygg_extension" / filename).read_bytes(),
                (REPOSITORY / "sdk" / "python" / "ygg_extension" / filename).read_bytes(),
                f"vendored SDK drifted: {filename}",
            )

    def test_official_catalog_and_offline_config_check(self):
        catalog = (ROOT.parent / "release-catalog.txt").read_text(encoding="utf-8").splitlines()
        self.assertIn("ygg-ssh", catalog)
        with tempfile.TemporaryDirectory() as directory:
            staged_entrypoint = Path(directory) / "ygg-ssh"
            staged_entrypoint.write_bytes((ROOT / "ygg-ssh").read_bytes())
            staged_entrypoint.chmod(0o755)
            environment = os.environ.copy()
            environment["YGG_EXTENSION_DIR"] = str(ROOT)
            completed = subprocess.run(
                [str(staged_entrypoint), "--config", str(ROOT / "config.example.json"), "--check-config"],
                cwd=directory,
                env=environment,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                timeout=5,
                check=False,
            )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("valid SSH configuration", completed.stdout)

    def test_release_archive_is_deterministic_regular_and_has_no_application_manifest(self):
        script = REPOSITORY / "scripts" / "package-ygg-extension-release.sh"
        self.assertTrue(script.is_file())
        with tempfile.TemporaryDirectory() as directory:
            temp = Path(directory)
            source = temp / "source" / "ygg-ssh"
            shutil.copytree(
                ROOT,
                source,
                ignore=shutil.ignore_patterns("__pycache__", "*.pyc", ".pytest_cache"),
            )
            outputs = []
            for name in ("one", "two"):
                destination = temp / name
                environment = dict(os.environ)
                environment["SOURCE_DATE_EPOCH"] = "1700000000"
                completed = subprocess.run(
                    [str(script), "ygg-ssh", str(destination), "v0.6.0-dev", str(source)],
                    cwd=REPOSITORY,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                    timeout=20,
                    env=environment,
                    check=False,
                )
                self.assertEqual(completed.returncode, 0, completed.stderr)
                outputs.append(destination / "ygg-ssh-0.6.0-dev.tar.gz")
            self.assertEqual(hashlib.sha256(outputs[0].read_bytes()).digest(), hashlib.sha256(outputs[1].read_bytes()).digest())
            with tarfile.open(outputs[0], "r:gz") as archive:
                members = archive.getmembers()
                self.assertTrue(all(member.isdir() or member.isfile() for member in members))
                names = {member.name.rstrip("/") for member in members}
                self.assertIn("ygg-ssh/extension.toml", names)
                self.assertIn("ygg-ssh/vendor/ygg_extension/extension.py", names)
                self.assertNotIn("ygg-ssh/package.toml", names)
                entry = archive.getmember("ygg-ssh/ygg-ssh")
                self.assertTrue(entry.mode & 0o111)

    def test_release_tree_has_no_links_oversized_files_or_embedded_credentials(self):
        forbidden = (
            b"-----BEGIN " + b"OPENSSH PRIVATE KEY-----",
            b"-----BEGIN " + b"RSA PRIVATE KEY-----",
        )
        for path in ROOT.rglob("*"):
            if "__pycache__" in path.parts or path.suffix == ".pyc":
                continue
            self.assertFalse(path.is_symlink(), path)
            if path.is_file():
                self.assertLess(path.stat().st_size, 2 * 1024 * 1024, path)
                data = path.read_bytes()
                for marker in forbidden:
                    self.assertNotIn(marker, data, path)

    def test_all_json_fixtures_parse(self):
        for path in [ROOT / "config.schema.json", ROOT / "config.example.json", *(ROOT / "fixtures").rglob("*.json")]:
            with self.subTest(path=path):
                json.loads(path.read_text(encoding="utf-8"))


if __name__ == "__main__":
    unittest.main()
