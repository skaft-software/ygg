from __future__ import annotations

import json
import os
import subprocess
import unittest

try:
    import tomllib
except ImportError:  # pragma: no cover - package runtime itself supports Python 3.9+
    tomllib = None

from ygg_mcp.config import load_config

from .helpers import ROOT


class ReleaseSmokeTests(unittest.TestCase):
    def test_manifest_is_exact_api_02_first_party_release_metadata(self):
        self.assertIsNotNone(tomllib)
        manifest = tomllib.loads((ROOT / "extension.toml").read_text(encoding="utf-8"))
        self.assertEqual(manifest["name"], "ygg-mcp")
        self.assertEqual(manifest["version"], "0.1.0")
        self.assertEqual(manifest["api_version"], "0.2")
        self.assertEqual(manifest["requires_ygg"], "=0.5.0")
        self.assertEqual(manifest["entrypoint"]["command"], "ygg-mcp")
        self.assertTrue(manifest["capabilities"]["process"])
        self.assertFalse(manifest["capabilities"]["network"])
        self.assertTrue(manifest["contributes"]["presentation"])
        self.assertEqual(manifest["contributes"]["commands"], ["mcp"])

    def test_release_catalog_and_executable_include_the_self_contained_runtime(self):
        catalog = (ROOT.parent / "release-catalog.txt").read_text(encoding="utf-8").splitlines()
        self.assertIn("ygg-mcp", catalog)
        executable = ROOT / "ygg-mcp"
        self.assertTrue(os.access(executable, os.X_OK))
        for relative in (
            "vendor/ygg_extension/__init__.py",
            "vendor/ygg_extension/extension.py",
            "vendor/ygg_extension/protocol.py",
            "ygg_mcp/runtime.py",
            "config.schema.json",
            "config.example.json",
            "fixtures/configs/real-local.json",
            "README.md",
            "CHANGELOG.md",
        ):
            self.assertTrue((ROOT / relative).is_file(), relative)

    def test_release_root_has_only_regular_files_and_no_oversized_fixture(self):
        for path in ROOT.rglob("*"):
            if "__pycache__" in path.parts:
                continue
            self.assertFalse(path.is_symlink(), path)
            if path.is_file():
                self.assertLess(path.stat().st_size, 2 * 1024 * 1024, path)

    def test_example_and_real_fixture_configuration_smoke(self):
        example = load_config(ROOT / "config.example.json")
        self.assertFalse(example.servers[0].enabled)
        real = load_config(ROOT / "fixtures" / "configs" / "real-local.json")
        self.assertTrue(real.servers[0].enabled)
        completed = subprocess.run(
            [
                str(ROOT / "ygg-mcp"),
                "--config",
                str(ROOT / "config.example.json"),
                "--check-config",
            ],
            cwd=ROOT,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=5,
            check=False,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("valid MCP configuration", completed.stdout)

    def test_json_schemas_and_every_fixture_are_strict_json(self):
        json.loads((ROOT / "config.schema.json").read_text(encoding="utf-8"))
        json.loads((ROOT / "config.example.json").read_text(encoding="utf-8"))
        for path in (ROOT / "fixtures").rglob("*.json"):
            with self.subTest(path=path):
                json.loads(path.read_text(encoding="utf-8"))


if __name__ == "__main__":
    unittest.main()
