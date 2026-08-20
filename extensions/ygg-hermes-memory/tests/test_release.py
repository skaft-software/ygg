from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
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
    def test_manifest_is_exact_current_installable_bundle_contract(self):
        self.assertIsNotNone(tomllib)
        manifest = tomllib.loads((ROOT / "extension.toml").read_text(encoding="utf-8"))
        self.assertEqual(manifest["name"], "ygg-hermes-memory")
        self.assertEqual(manifest["version"], "0.1.0")
        self.assertEqual(manifest["api_version"], "0.2")
        self.assertEqual(manifest["requires_ygg"], "=0.5.0")
        self.assertEqual(manifest["entrypoint"]["command"], "ygg-hermes-memory")
        self.assertTrue(manifest["capabilities"]["network"])
        self.assertTrue(manifest["capabilities"]["process"])
        self.assertEqual(manifest["capabilities"]["filesystem"], "unrestricted")
        self.assertEqual(manifest["contributes"]["commands"], ["memory"])
        self.assertTrue(manifest["contributes"]["context"])
        self.assertTrue(manifest["contributes"]["presentation"])
        self.assertFalse((ROOT / "package.toml").exists())

    def test_official_catalog_executable_and_self_contained_runtime(self):
        catalog = (ROOT.parent / "release-catalog.txt").read_text(encoding="utf-8").splitlines()
        self.assertIn("ygg-hermes-memory", catalog)
        self.assertTrue(os.access(ROOT / "ygg-hermes-memory", os.X_OK))
        self.assertTrue(os.access(ROOT / "extension.py", os.X_OK))
        expected = (
            "vendor/ygg_extension/__init__.py",
            "vendor/ygg_extension/extension.py",
            "vendor/ygg_extension/protocol.py",
            "ygg_hermes_memory/runtime.py",
            "config.schema.json",
            "config.example.json",
            "fixtures/providers/mock_provider/__init__.py",
            "fixtures/providers/offline_provider/__init__.py",
            "fixtures/hermes_environment/offline_memory_provider-1.0.0.dist-info/entry_points.txt",
            "README.md",
            "CHANGELOG.md",
            "LICENSE",
        )
        for relative in expected:
            self.assertTrue((ROOT / relative).is_file(), relative)

    def test_vendored_sdk_is_byte_for_byte_synced_with_shared_sdk(self):
        shared = REPOSITORY / "sdk" / "python" / "ygg_extension"
        vendored = ROOT / "vendor" / "ygg_extension"
        shared_files = sorted(path.relative_to(shared) for path in shared.glob("*.py"))
        vendor_files = sorted(path.relative_to(vendored) for path in vendored.glob("*.py"))
        self.assertEqual(vendor_files, shared_files)
        for relative in shared_files:
            self.assertEqual(
                hashlib.sha256((shared / relative).read_bytes()).hexdigest(),
                hashlib.sha256((vendored / relative).read_bytes()).hexdigest(),
                relative,
            )

    def test_entrypoint_config_and_contract_smoke_do_not_install_or_import_provider(self):
        contract = subprocess.run(
            [str(ROOT / "ygg-hermes-memory"), "--contract"],
            cwd=ROOT,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=5,
            check=False,
        )
        self.assertEqual(contract.returncode, 0, contract.stderr)
        self.assertIn("7095e23eb2066fe9a2f93b99cdbfe0e2b5ece397", contract.stdout)
        checked = subprocess.run(
            [
                str(ROOT / "ygg-hermes-memory"),
                "--config",
                str(ROOT / "config.example.json"),
                "--check-config",
            ],
            cwd=ROOT,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=5,
            check=False,
        )
        self.assertEqual(checked.returncode, 0, checked.stderr)
        self.assertIn("valid Hermes memory configuration", checked.stdout)
        combined = (checked.stdout + checked.stderr).lower()
        self.assertNotIn("pip install", combined)
        self.assertNotIn("download", combined)

    def test_untrusted_bootstrap_config_cannot_select_an_executable(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            sentinel = root / "executed"
            interpreter = root / "provider-python"
            interpreter.write_text(
                f"#!/bin/sh\nprintf executed > \"{sentinel}\"\n",
                encoding="utf-8",
            )
            os.chmod(interpreter, 0o755)
            value = json.loads((ROOT / "config.example.json").read_text(encoding="utf-8"))
            value["environment"]["python"] = str(interpreter)
            config = root / "config.json"
            config.write_text(json.dumps(value), encoding="utf-8")
            os.chmod(config, 0o666)

            completed = subprocess.run(
                [sys.executable, str(ROOT / "extension.py"), "--config", str(config), str(sentinel)],
                cwd=ROOT,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                timeout=5,
                check=False,
            )
            self.assertFalse(sentinel.exists(), completed.stdout + completed.stderr)

    def test_json_files_are_strict_and_release_root_has_only_regular_bounded_files(self):
        json.loads((ROOT / "config.schema.json").read_text(encoding="utf-8"))
        json.loads((ROOT / "config.example.json").read_text(encoding="utf-8"))
        for path in ROOT.rglob("*"):
            if "__pycache__" in path.parts:
                continue
            self.assertFalse(path.is_symlink(), path)
            if path.is_file():
                self.assertLess(path.stat().st_size, 2 * 1024 * 1024, path)
        for path in (ROOT / "fixtures" / "presentation").glob("*.json"):
            json.loads(path.read_text(encoding="utf-8"))

    def test_release_archive_is_reproducible_and_contains_no_application_manifest_or_store(self):
        with tempfile.TemporaryDirectory() as temporary:
            temporary_path = Path(temporary)
            source = temporary_path / "source"
            shutil.copytree(
                ROOT,
                source,
                ignore=shutil.ignore_patterns("__pycache__", "*.pyc"),
            )
            first = temporary_path / "first"
            second = temporary_path / "second"
            environment = os.environ.copy()
            environment["SOURCE_DATE_EPOCH"] = "1700000000"
            command = [
                str(REPOSITORY / "scripts" / "package-ygg-extension-release.sh"),
                "ygg-hermes-memory",
                str(first),
                "v0.5.0",
                str(source),
            ]
            completed = subprocess.run(
                command,
                cwd=REPOSITORY,
                env=environment,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                timeout=30,
                check=False,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            command[2] = str(second)
            completed = subprocess.run(
                command,
                cwd=REPOSITORY,
                env=environment,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                timeout=30,
                check=False,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            archive_one = next(first.glob("*.tar.gz"))
            archive_two = next(second.glob("*.tar.gz"))
            self.assertEqual(archive_one.read_bytes(), archive_two.read_bytes())
            with tarfile.open(archive_one, "r:gz") as archive:
                names = {member.name.rstrip("/") for member in archive.getmembers()}
                self.assertIn("ygg-hermes-memory/extension.toml", names)
                self.assertIn("ygg-hermes-memory/ygg-hermes-memory", names)
                self.assertNotIn("ygg-hermes-memory/package.toml", names)
                self.assertFalse(any(name.endswith("install.json") for name in names))
                self.assertFalse(any("offline-recall-fixture.json" in name for name in names))
                self.assertTrue(all(member.isdir() or member.isfile() for member in archive.getmembers()))


if __name__ == "__main__":
    unittest.main()
