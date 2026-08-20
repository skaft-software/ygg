from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock

from ygg_mcp.config import ConfigError, load_config

from .helpers import ROOT


class ConfigTests(unittest.TestCase):
    def write_json(self, path: Path, value) -> bytes:
        data = json.dumps(value, separators=(",", ":")).encode("utf-8")
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        path.chmod(0o600)
        return data

    def test_missing_default_is_inert_empty_configuration(self):
        with tempfile.TemporaryDirectory() as directory:
            missing = Path(directory) / "missing.json"
            with mock.patch("ygg_mcp.config.default_config_path", return_value=missing):
                config = load_config()
        self.assertEqual(config.servers, ())
        self.assertEqual(config.source, missing)

    def test_example_is_strict_and_disabled_until_user_edits_it(self):
        config = load_config(ROOT / "config.example.json")
        self.assertEqual(len(config.servers), 1)
        self.assertFalse(config.servers[0].enabled)
        self.assertEqual(config.servers[0].command, "/absolute/path/to/an-installed-mcp-server")

    def test_unknown_duplicate_and_oversized_configuration_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            unknown = root / "unknown.json"
            self.write_json(unknown, {"version": 1, "servers": {}, "surprise": True})
            with self.assertRaises(ConfigError):
                load_config(unknown)

            duplicate = root / "duplicate.json"
            duplicate.write_text('{"version":1,"servers":{},"servers":{}}', encoding="utf-8")
            duplicate.chmod(0o600)
            with self.assertRaises(ConfigError):
                load_config(duplicate)

            oversized = root / "oversized.json"
            oversized.write_bytes(b" " * (256 * 1024 + 1))
            oversized.chmod(0o600)
            with self.assertRaises(ConfigError):
                load_config(oversized)

    def test_project_configuration_requires_workspace_containment_and_exact_digest(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            workspace = root / "workspace"
            project = workspace / ".ygg" / "mcp.json"
            project_bytes = self.write_json(
                project,
                {
                    "version": 1,
                    "servers": {
                        "project-fixture": {
                            "command": "fixture-server",
                            "args": [],
                            "env": {},
                        }
                    },
                },
            )
            user = root / "user.json"
            self.write_json(
                user,
                {
                    "version": 1,
                    "servers": {},
                    "trustedProjects": [
                        {
                            "path": str(project),
                            "sha256": hashlib.sha256(project_bytes).hexdigest(),
                        }
                    ],
                },
            )
            config = load_config(user, workspace=workspace)
            self.assertEqual(config.servers[0].scope, "project")

            project.write_text('{"version":1,"servers":{}}', encoding="utf-8")
            project.chmod(0o600)
            with self.assertRaises(ConfigError):
                load_config(user, workspace=workspace)

            outside = root / "outside.json"
            outside_bytes = self.write_json(outside, {"version": 1, "servers": {}})
            self.write_json(
                user,
                {
                    "version": 1,
                    "servers": {},
                    "trustedProjects": [
                        {
                            "path": str(outside),
                            "sha256": hashlib.sha256(outside_bytes).hexdigest(),
                        }
                    ],
                },
            )
            with self.assertRaises(ConfigError):
                load_config(user, workspace=workspace)

    @unittest.skipUnless(hasattr(os, "O_NOFOLLOW"), "requires no-follow file opens")
    def test_configuration_swap_to_symlink_at_open_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / "config.json"
            outside = root / "outside.json"
            self.write_json(path, {"version": 1, "servers": {}})
            self.write_json(
                outside,
                {
                    "version": 1,
                    "servers": {"escaped": {"command": "outside-server"}},
                },
            )
            canonical_path = path.parent.resolve(strict=True) / path.name
            real_lstat = Path.lstat
            real_open = os.open
            swapped = False

            def swap_to_link():
                nonlocal swapped
                if swapped:
                    return
                swapped = True
                path.unlink()
                path.symlink_to(outside)

            def racing_lstat(candidate, *args, **kwargs):
                metadata = real_lstat(candidate, *args, **kwargs)
                if Path(candidate) == path:
                    swap_to_link()
                return metadata

            def racing_open(candidate, flags, mode=0o777, *, dir_fd=None):
                if dir_fd is None and Path(candidate) in {path, canonical_path}:
                    swap_to_link()
                return real_open(candidate, flags, mode, dir_fd=dir_fd)

            with mock.patch.object(Path, "lstat", racing_lstat), mock.patch(
                "ygg_mcp.config.os.open", side_effect=racing_open
            ):
                with self.assertRaises(ConfigError):
                    load_config(path)
            self.assertTrue(swapped)

    def test_linked_workspace_ygg_directory_cannot_escape_project_containment(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            workspace = root / "workspace"
            workspace.mkdir()
            outside = root / "outside-ygg"
            outside.mkdir()
            project = outside / "mcp.json"
            project_bytes = self.write_json(
                project,
                {
                    "version": 1,
                    "servers": {
                        "escaped": {
                            "command": "fixture-server",
                            "cwd": "outside-cwd",
                        }
                    },
                },
            )
            try:
                (workspace / ".ygg").symlink_to(outside, target_is_directory=True)
            except (OSError, NotImplementedError):
                self.skipTest("directory symlinks are unavailable")
            user = root / "user.json"
            self.write_json(
                user,
                {
                    "version": 1,
                    "servers": {},
                    "trustedProjects": [
                        {
                            "path": str(workspace / ".ygg" / "mcp.json"),
                            "sha256": hashlib.sha256(project_bytes).hexdigest(),
                        }
                    ],
                },
            )

            with self.assertRaises(ConfigError):
                load_config(user, workspace=workspace)

    def test_launch_environment_is_explicit_and_bounded(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "config.json"
            self.write_json(
                path,
                {
                    "version": 1,
                    "servers": {
                        "fixture": {
                            "command": "server",
                            "env": {"TOKEN": "sensitive"},
                        }
                    },
                },
            )
            config = load_config(path)
            self.assertEqual(config.servers[0].environment["TOKEN"], "sensitive")
            self.assertNotIn("sensitive", repr(config.servers[0]))


if __name__ == "__main__":
    unittest.main()
