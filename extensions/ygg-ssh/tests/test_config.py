from __future__ import annotations

import hashlib
import json
from pathlib import Path
import tempfile
import unittest

from ygg_ssh.config import ConfigError, default_config_path, load_config

from .helpers import config_document, write_json


class ConfigTests(unittest.TestCase):
    def test_missing_default_is_inert_and_explicit_missing_fails(self):
        self.assertEqual(default_config_path().name, "ssh.json")
        with self.assertRaisesRegex(ConfigError, "does not exist"):
            load_config("/definitely/missing/ygg-ssh.json")

    def test_explicit_alias_remote_cwd_and_read_only_default(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            remote = root / "remote"
            remote.mkdir()
            document = config_document(remote)
            del document["targets"]["fixture"]["authority"]
            config = load_config(write_json(root / "ssh.json", document))
        target = config.targets[0]
        self.assertEqual(target.alias, "fixture-alias")
        self.assertEqual(target.authority, "read-only")
        self.assertEqual(target.remote_cwd, str(remote))

    def test_rejects_model_style_destinations_credentials_and_unknown_fields(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            remote = root / "remote"
            remote.mkdir()
            for field, value in (
                ("alias", "user@example.com"),
                ("alias", "-oProxyCommand=bad"),
                ("identityFile", "/tmp/key"),
                ("port", 22),
                ("proxyJump", "jump"),
            ):
                document = config_document(remote)
                document["targets"]["fixture"][field] = value
                if field != "alias":
                    document["targets"]["fixture"]["alias"] = "fixture"
                path = write_json(root / f"{field}.json", document)
                with self.subTest(field=field), self.assertRaises(ConfigError):
                    load_config(path)

    def test_rejects_duplicate_keys_controls_traversal_and_unsafe_permissions(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            duplicate = root / "duplicate.json"
            duplicate.write_text('{"version":1,"version":1,"targets":{}}', encoding="utf-8")
            duplicate.chmod(0o600)
            with self.assertRaisesRegex(ConfigError, "duplicate"):
                load_config(duplicate)

            document = config_document(root)
            document["targets"]["fixture"]["remoteCwd"] = "/srv/../secret"
            with self.assertRaisesRegex(ConfigError, "normalized"):
                load_config(write_json(root / "traversal.json", document))

            unsafe = write_json(root / "unsafe.json", config_document(root))
            unsafe.chmod(0o666)
            with self.assertRaisesRegex(ConfigError, "writable"):
                load_config(unsafe)

    def test_digest_pinned_project_config_is_merged_and_edit_fails_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            workspace = Path(directory) / "workspace"
            project_dir = workspace / ".ygg"
            project_dir.mkdir(parents=True)
            project = write_json(
                project_dir / "ssh-project.json",
                {
                    "version": 1,
                    "targets": {
                        "project": {
                            "alias": "project-alias",
                            "remoteCwd": "/srv/project",
                            "authority": "read-only",
                        }
                    },
                },
            )
            digest = hashlib.sha256(project.read_bytes()).hexdigest()
            user = write_json(
                Path(directory) / "user.json",
                {
                    "version": 1,
                    "targets": {},
                    "trustedProjects": [{"path": str(project), "sha256": digest}],
                },
            )
            config = load_config(user, workspace=workspace)
            self.assertEqual(config.targets[0].scope, "project")
            project.write_text(project.read_text(encoding="utf-8") + "\n", encoding="utf-8")
            with self.assertRaisesRegex(ConfigError, "digest"):
                load_config(user, workspace=workspace)

    def test_limits_are_capped_and_file_cannot_exceed_output(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            document = config_document(root)
            document["limits"]["maxSessions"] = 17
            with self.assertRaisesRegex(ConfigError, "between"):
                load_config(write_json(root / "large.json", document))
            document = config_document(root)
            document["limits"]["maxOutputBytes"] = 1024
            document["limits"]["maxFileBytes"] = 2048
            with self.assertRaisesRegex(ConfigError, "cannot exceed"):
                load_config(write_json(root / "mismatch.json", document))

    def test_schema_and_example_are_valid_json_and_example_is_disabled(self):
        root = Path(__file__).resolve().parents[1]
        json.loads((root / "config.schema.json").read_text(encoding="utf-8"))
        example = json.loads((root / "config.example.json").read_text(encoding="utf-8"))
        self.assertFalse(example["targets"]["example-read-only"]["enabled"])


if __name__ == "__main__":
    unittest.main()
