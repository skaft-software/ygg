from __future__ import annotations

import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from ygg_browse.artifacts import (
    MAX_SCREENSHOTS,
    PNG_SIGNATURE,
    READ_IMAGE_LIMIT,
    ArtifactStore,
)
from ygg_browse.paths import BrowsePaths
from ygg_browse.safety import BrowseError


class FakeExtension:
    negotiated_features = frozenset({"artifacts"})

    def __init__(self, scratch: Path) -> None:
        self.scratch = scratch
        self.calls = []

    def publish_artifact(self, **arguments):
        self.calls.append(arguments)
        path = self.scratch / arguments["path"]
        self.assert_regular(path)
        self.last_bytes = path.read_bytes()
        return "artifact-owner-generation"

    @staticmethod
    def assert_regular(path: Path) -> None:
        if not path.is_file() or path.is_symlink():
            raise AssertionError("scratch publication is not a regular file")


class ArtifactTests(unittest.TestCase):
    def test_png_bound_retention_and_scratch_publication(self) -> None:
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as scratch_value:
            paths = BrowsePaths.for_home(Path(home))
            store = ArtifactStore(paths)
            records = [store.save_png(PNG_SIGNATURE + bytes([index]) * 32) for index in range(MAX_SCREENSHOTS + 3)]
            retained = list(paths.screenshots.glob("screenshot-*.png"))
            self.assertLessEqual(len(retained), MAX_SCREENSHOTS)
            latest = records[-1]
            self.assertTrue(latest.path.exists())
            scratch = Path(scratch_value)
            extension = FakeExtension(scratch)
            with patch.dict(os.environ, {"YGG_EXTENSION_SCRATCH": str(scratch)}):
                artifact_id = store.publish(extension, latest)
            self.assertEqual(artifact_id, "artifact-owner-generation")
            self.assertEqual(extension.last_bytes, latest.path.read_bytes())
            self.assertEqual(extension.calls[0]["sha256"], latest.sha256)
            self.assertFalse(any((scratch / "browse-screenshots").iterdir()))

    def test_invalid_or_five_mib_image_fails_without_file(self) -> None:
        with tempfile.TemporaryDirectory() as home:
            store = ArtifactStore(BrowsePaths.for_home(Path(home)))
            with self.assertRaises(BrowseError) as invalid:
                store.save_png(b"not png")
            self.assertEqual(invalid.exception.code, "screenshot_invalid")
            with self.assertRaises(BrowseError) as large:
                store.save_png(PNG_SIGNATURE + b"x" * (READ_IMAGE_LIMIT - len(PNG_SIGNATURE)))
            self.assertEqual(large.exception.code, "screenshot_too_large")

    def test_publication_requires_negotiated_artifacts_and_safe_scratch(self) -> None:
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as scratch_value:
            paths = BrowsePaths.for_home(Path(home))
            store = ArtifactStore(paths)
            record = store.save_png(PNG_SIGNATURE + b"small")

            class NoArtifacts:
                negotiated_features = frozenset()

            with self.assertRaises(BrowseError) as unavailable:
                store.publish(NoArtifacts(), record)
            self.assertEqual(unavailable.exception.code, "artifacts_unavailable")

            target = Path(scratch_value) / "actual"
            target.mkdir()
            linked = Path(scratch_value) / "linked"
            linked.symlink_to(target, target_is_directory=True)
            extension = FakeExtension(target)
            with patch.dict(os.environ, {"YGG_EXTENSION_SCRATCH": str(linked)}):
                with self.assertRaises(BrowseError):
                    store.publish(extension, record)


if __name__ == "__main__":
    unittest.main()
