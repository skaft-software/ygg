from __future__ import annotations

import json
from pathlib import Path
import tempfile
import unittest

from ygg_ssh.config import ConfigError, SshConfig, load_config

from .helpers import config_document, write_json


class ConfigTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)

    def tearDown(self):
        self.temp.cleanup()

    def test_explicit_missing_file_is_an_error_and_empty_config_is_inert(self):
        with self.assertRaises(ConfigError):
            load_config(self.root / "absent.json")
        empty = SshConfig.empty(self.root / "absent.json")
        self.assertEqual(empty.targets, ())
        self.assertEqual(empty.enabled_targets(), ())

    def test_valid_document_parses_targets(self):
        path = write_json(self.root / "ssh.json", config_document())
        config = load_config(path)
        self.assertEqual(len(config.targets), 1)
        target = config.targets[0]
        self.assertEqual(target.id, "fixture")
        self.assertEqual(target.alias, "fixture-alias")
        self.assertEqual(target.cwd, "/srv/fixture")
        self.assertTrue(target.enabled)
        self.assertEqual(config.enabled_targets(), config.targets)

    def test_explicit_missing_path_is_an_error(self):
        with self.assertRaises(ConfigError):
            load_config(self.root / "absent.json")

    def test_unknown_root_and_target_fields_are_rejected(self):
        document = config_document()
        document["limits"] = {}
        path = write_json(self.root / "ssh.json", document)
        with self.assertRaises(ConfigError):
            load_config(path)
        document = config_document()
        document["targets"]["fixture"]["authority"] = "read-only"
        path = write_json(self.root / "ssh.json", document)
        with self.assertRaises(ConfigError):
            load_config(path)

    def test_wrong_version_is_rejected(self):
        document = config_document()
        document["version"] = 2
        path = write_json(self.root / "ssh.json", document)
        with self.assertRaises(ConfigError):
            load_config(path)

    def test_invalid_target_identifiers_are_rejected(self):
        document = config_document()
        document["targets"] = {"Bad_Id": {"alias": "ok-alias"}}
        path = write_json(self.root / "ssh.json", document)
        with self.assertRaises(ConfigError):
            load_config(path)

    def test_unsafe_alias_is_rejected(self):
        document = config_document()
        document["targets"]["fixture"]["alias"] = "-ProxyCommand evil"
        path = write_json(self.root / "ssh.json", document)
        with self.assertRaises(ConfigError):
            load_config(path)

    def test_duplicate_aliases_are_rejected(self):
        document = config_document()
        document["targets"]["other"] = {"alias": "FIXTURE-ALIAS"}
        path = write_json(self.root / "ssh.json", document)
        with self.assertRaises(ConfigError):
            load_config(path)

    def test_relative_or_traversing_cwd_is_rejected(self):
        for bad in ("srv/fixture", "/srv/../etc", "/srv//fixture", "/srv/fixture/.."):
            document = config_document()
            document["targets"]["fixture"]["cwd"] = bad
            path = write_json(self.root / "ssh.json", document)
            with self.subTest(cwd=bad):
                with self.assertRaises(ConfigError):
                    load_config(path)

    def test_symlinked_config_is_rejected(self):
        real = write_json(self.root / "real.json", config_document())
        link = self.root / "link.json"
        link.symlink_to(real)
        with self.assertRaises(ConfigError):
            load_config(link)

    def test_group_writable_config_is_rejected(self):
        path = write_json(self.root / "ssh.json", config_document())
        path.chmod(0o660)
        with self.assertRaises(ConfigError):
            load_config(path)

    def test_duplicate_json_keys_are_rejected(self):
        path = self.root / "ssh.json"
        path.write_text('{"version": 1, "version": 1, "targets": {}}\n', encoding="utf-8")
        path.chmod(0o600)
        with self.assertRaises(ConfigError):
            load_config(path)

    def test_oversized_config_is_rejected(self):
        path = self.root / "ssh.json"
        path.write_text('{"version": 1, "targets": {}, "pad": "' + "x" * 70_000 + '"}\n', encoding="utf-8")
        path.chmod(0o600)
        with self.assertRaises(ConfigError):
            load_config(path)

    def test_disabled_targets_are_filtered(self):
        document = config_document(enabled=False)
        path = write_json(self.root / "ssh.json", document)
        config = load_config(path)
        self.assertEqual(config.enabled_targets(), ())


if __name__ == "__main__":
    unittest.main()
