from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import sys
import unittest

from .helpers import (
    HERMES_ENV,
    ROOT,
    load_fixture_config,
    mock_descriptor,
    temporary_directory,
    write_config,
)
from ygg_hermes_memory.config import (
    BridgeConfig,
    ConfigError,
    current_environment_matches,
    load_config,
)
from ygg_hermes_memory.credentials import ProviderEnvironmentError, read_provider_environment
from ygg_hermes_memory.discovery import directory_fingerprint, discover_providers


class ConfigAndDiscoveryTests(unittest.TestCase):
    def test_missing_default_configuration_is_inert(self):
        with temporary_directory() as directory:
            original = os.environ.get("HOME")
            os.environ["HOME"] = str(directory)
            try:
                config = load_config()
            finally:
                if original is None:
                    os.environ.pop("HOME", None)
                else:
                    os.environ["HOME"] = original
        self.assertIsNone(config.environment)
        self.assertEqual(config.directories, ())
        snapshot = discover_providers(config)
        self.assertEqual(snapshot.candidates, ())
        self.assertEqual(snapshot.environment_state, "off")

    def test_exact_contract_unknown_keys_duplicates_and_permissions_are_rejected(self):
        with temporary_directory() as directory:
            path = write_config(directory, providers=[mock_descriptor()])
            value = json.loads(path.read_text(encoding="utf-8"))
            value["contract"]["hermesVersion"] = "0.20.2"
            path.write_text(json.dumps(value), encoding="utf-8")
            with self.assertRaisesRegex(ConfigError, "0.20.1"):
                load_config(path)

            value["contract"]["hermesVersion"] = "0.20.1"
            value["credential"] = "secret"
            path.write_text(json.dumps(value), encoding="utf-8")
            with self.assertRaisesRegex(ConfigError, "unknown"):
                load_config(path)

            path.write_text('{"version":1,"version":1}', encoding="utf-8")
            with self.assertRaisesRegex(ConfigError, "duplicate"):
                load_config(path)

            path = write_config(directory, providers=[])
            os.chmod(path, 0o666)
            with self.assertRaisesRegex(ConfigError, "group- or world-writable"):
                load_config(path)

            path = write_config(directory, providers=[])
            linked = directory / "hard-linked-config.json"
            os.link(path, linked)
            with self.assertRaisesRegex(ConfigError, "hard links"):
                load_config(path)

    def test_symlink_config_and_relative_environment_paths_are_rejected(self):
        with temporary_directory() as directory:
            path = write_config(directory, providers=[])
            link = directory / "linked.json"
            link.symlink_to(path)
            with self.assertRaisesRegex(ConfigError, "non-symlink"):
                load_config(link)
            value = json.loads(path.read_text(encoding="utf-8"))
            value["environment"]["python"] = "relative/python"
            path.write_text(json.dumps(value), encoding="utf-8")
            with self.assertRaisesRegex(ConfigError, "absolute"):
                load_config(path)

    def test_directory_discovery_hashes_metadata_without_importing(self):
        with temporary_directory() as directory:
            sentinel = directory / "imported"
            previous = os.environ.get("YGG_MEMORY_IMPORT_SENTINEL")
            os.environ["YGG_MEMORY_IMPORT_SENTINEL"] = str(sentinel)
            try:
                config = load_fixture_config(directory, providers=[mock_descriptor()])
                snapshot = discover_providers(config)
            finally:
                if previous is None:
                    os.environ.pop("YGG_MEMORY_IMPORT_SENTINEL", None)
                else:
                    os.environ["YGG_MEMORY_IMPORT_SENTINEL"] = previous
            candidate = snapshot.by_id("directory:mock")
            self.assertIsNotNone(candidate)
            self.assertFalse(sentinel.exists())
            self.assertEqual(candidate.name, "mock-memory")
            self.assertEqual(candidate.version, "1.0.0")
            self.assertEqual(candidate.network, "none")
            self.assertEqual(len(candidate.fingerprint), 64)
            self.assertNotIn("password", candidate.label.lower())

    def test_entry_point_discovery_does_not_import_module(self):
        sys.modules.pop("offline_entrypoint", None)
        with temporary_directory() as directory:
            config = load_fixture_config(directory, providers=[], include_entry_points=True)
            snapshot = discover_providers(config)
        candidate = snapshot.by_id("entrypoint:entrypoint-memory")
        self.assertIsNotNone(candidate)
        self.assertEqual(candidate.distribution_name, "offline-memory-provider")
        self.assertEqual(candidate.version, "1.0.0")
        self.assertNotIn("offline_entrypoint", sys.modules)

    def test_directory_fingerprint_is_stable_and_changes_with_code(self):
        with temporary_directory() as directory:
            source = ROOT / "fixtures" / "providers" / "mock_provider"
            copied = directory / "provider"
            shutil.copytree(source, copied, ignore=shutil.ignore_patterns("__pycache__"))
            first = directory_fingerprint("directory:copy", copied, "env")
            second = directory_fingerprint("directory:copy", copied, "env")
            self.assertEqual(first, second)
            with (copied / "__init__.py").open("a", encoding="utf-8") as handle:
                handle.write("\n# reviewed change\n")
            self.assertNotEqual(first, directory_fingerprint("directory:copy", copied, "env"))

    def test_symlink_inside_provider_is_unavailable(self):
        with temporary_directory() as directory:
            provider = directory / "provider"
            provider.mkdir()
            (provider / "__init__.py").write_text("class X: pass\n", encoding="utf-8")
            (provider / "linked.py").symlink_to(provider / "__init__.py")
            descriptor = dict(mock_descriptor(), id="linked", path=str(provider))
            config = load_fixture_config(directory, providers=[descriptor])
            candidate = discover_providers(config).by_id("directory:linked")
            self.assertEqual(candidate.availability, "unavailable")
            self.assertEqual(candidate.reason_code, "directory_metadata_invalid")
            self.assertIsNone(candidate.fingerprint)

    def test_environment_and_contract_version_mismatch_are_metadata_health_only(self):
        class WrongMetadata:
            @staticmethod
            def version(name):
                self.assertEqual(name, "hermes-agent")
                return "9.9.9"

            @staticmethod
            def entry_points():
                return []

        with temporary_directory() as directory:
            config = load_fixture_config(directory, providers=[mock_descriptor()])
            snapshot = discover_providers(config, metadata_module=WrongMetadata)
        self.assertEqual(snapshot.environment_state, "unavailable")
        self.assertEqual(snapshot.reason_code, "hermes_contract_version_mismatch")
        self.assertTrue(all(item.availability == "unavailable" for item in snapshot.candidates))

    def test_provider_count_and_behavior_values_are_bounded(self):
        with temporary_directory() as directory:
            path = write_config(directory, providers=[mock_descriptor()])
            value = json.loads(path.read_text(encoding="utf-8"))
            value["directories"][0]["network"] = "ambient"
            path.write_text(json.dumps(value), encoding="utf-8")
            with self.assertRaisesRegex(ConfigError, "unsupported"):
                load_config(path)
            value["directories"][0]["network"] = "none"
            value["directories"][0]["writeTools"] = ["bad tool"]
            path.write_text(json.dumps(value), encoding="utf-8")
            with self.assertRaisesRegex(ConfigError, "invalid tool"):
                load_config(path)
    def test_environment_identity_distinguishes_venv_symlink_paths(self):
        with temporary_directory() as directory:
            alias = directory / "venv-python"
            alias.symlink_to(sys.executable)
            path = write_config(directory, providers=[])
            value = json.loads(path.read_text(encoding="utf-8"))
            value["environment"]["python"] = str(alias)
            path.write_text(json.dumps(value), encoding="utf-8")
            config = load_config(path)
            self.assertFalse(current_environment_matches(config.environment))

    def test_provider_environment_file_is_private_bounded_and_never_expands_values(self):
        with temporary_directory() as directory:
            path = directory / ".env"
            path.write_text(
                "# provider-owned credentials\nAPI_TOKEN='literal-$HOME'\nREGION=offline # comment\n",
                encoding="utf-8",
            )
            os.chmod(path, 0o600)
            values = read_provider_environment(path)
            self.assertEqual(values["API_TOKEN"], "literal-$HOME")
            self.assertEqual(values["REGION"], "offline")
            os.chmod(path, 0o644)
            with self.assertRaisesRegex(ProviderEnvironmentError, "permissions"):
                read_provider_environment(path)
            os.chmod(path, 0o600)
            path.write_text("PYTHONPATH=forbidden\n", encoding="utf-8")
            with self.assertRaisesRegex(ProviderEnvironmentError, "forbidden"):
                read_provider_environment(path)


if __name__ == "__main__":
    unittest.main()
