"""Bounded screenshot retention and API 0.2 artifact publication."""

from __future__ import annotations

import hashlib
import os
import stat
import threading
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Optional

from .paths import BrowsePaths, PathSafetyError
from .safety import BrowseError


READ_IMAGE_LIMIT = 5 * 1024 * 1024
MAX_SCREENSHOTS = 20
MAX_SCREENSHOT_BYTES = 80 * 1024 * 1024
PNG_SIGNATURE = b"\x89PNG\r\n\x1a\n"


@dataclass(frozen=True)
class ScreenshotRecord:
    path: Path
    size: int
    sha256: str
    mime_type: str = "image/png"


class ArtifactStore:
    def __init__(self, paths: BrowsePaths) -> None:
        self.paths = paths
        self._lock = threading.Lock()

    def save_png(self, data: bytes) -> ScreenshotRecord:
        if not isinstance(data, bytes) or not data.startswith(PNG_SIGNATURE):
            raise BrowseError("screenshot_invalid", "The browser did not return a valid PNG screenshot.")
        if len(data) >= READ_IMAGE_LIMIT:
            raise BrowseError(
                "screenshot_too_large",
                "The viewport screenshot is at least 5 MiB and cannot be consumed by read.",
            )
        with self._lock:
            try:
                self.paths.ensure_directory(self.paths.screenshots)
            except PathSafetyError as error:
                raise BrowseError("unsafe_artifact_path", str(error)) from error
            destination = self.paths.screenshots / f"screenshot-{uuid.uuid4().hex}.png"
            flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
            flags |= getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
            try:
                fd = os.open(str(destination), flags, 0o600)
                try:
                    view = memoryview(data)
                    while view:
                        written = os.write(fd, view)
                        view = view[written:]
                    os.fsync(fd)
                finally:
                    os.close(fd)
            except OSError as error:
                raise BrowseError("artifact_write_failed", "The screenshot could not be saved.") from error
            record = ScreenshotRecord(destination, len(data), hashlib.sha256(data).hexdigest())
            self._prune_locked(keep=destination)
            return record

    def publish(self, extension: Any, record: ScreenshotRecord) -> str:
        """Copy to generation scratch, publish, then remove the scratch copy."""

        if "artifacts" not in extension.negotiated_features:
            raise BrowseError(
                "artifacts_unavailable",
                "The host did not negotiate API 0.2 artifacts; no screenshot was captured.",
            )
        scratch_value = os.environ.get("YGG_EXTENSION_SCRATCH")
        if not scratch_value:
            raise BrowseError("artifacts_unavailable", "The host artifact scratch directory is unavailable.")
        scratch = Path(scratch_value).absolute()
        try:
            metadata = scratch.lstat()
        except FileNotFoundError as error:
            raise BrowseError("artifacts_unavailable", "The host artifact scratch directory is unavailable.") from error
        if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
            raise BrowseError("artifacts_unavailable", "The host artifact scratch directory is unsafe.")
        directory = scratch / "browse-screenshots"
        if directory.exists() or directory.is_symlink():
            directory_metadata = directory.lstat()
            if stat.S_ISLNK(directory_metadata.st_mode) or not stat.S_ISDIR(directory_metadata.st_mode):
                raise BrowseError("artifacts_unavailable", "The screenshot scratch path is unsafe.")
        else:
            directory.mkdir(mode=0o700)
        filename = f"screenshot-{uuid.uuid4().hex}.png"
        destination = directory / filename
        try:
            path_metadata = record.path.lstat()
        except FileNotFoundError as error:
            raise BrowseError("artifact_missing", "The retained screenshot is unavailable.") from error
        if (
            stat.S_ISLNK(path_metadata.st_mode)
            or not stat.S_ISREG(path_metadata.st_mode)
            or path_metadata.st_nlink != 1
        ):
            raise BrowseError("artifact_missing", "The retained screenshot is unsafe.")
        source_flags = os.O_RDONLY | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
        output_flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
        output_flags |= getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
        try:
            source_fd = os.open(str(record.path), source_flags)
            try:
                source_metadata = os.fstat(source_fd)
                if (
                    not stat.S_ISREG(source_metadata.st_mode)
                    or source_metadata.st_nlink != 1
                    or source_metadata.st_size != record.size
                    or (source_metadata.st_dev, source_metadata.st_ino)
                    != (path_metadata.st_dev, path_metadata.st_ino)
                ):
                    raise BrowseError("artifact_missing", "The retained screenshot changed before publication.")
                output = os.open(str(destination), output_flags, 0o600)
                digest = hashlib.sha256()
                copied = 0
                try:
                    while True:
                        chunk = os.read(source_fd, 64 * 1024)
                        if not chunk:
                            break
                        digest.update(chunk)
                        copied += len(chunk)
                        view = memoryview(chunk)
                        while view:
                            written = os.write(output, view)
                            view = view[written:]
                    os.fsync(output)
                finally:
                    os.close(output)
            finally:
                os.close(source_fd)
            if copied != record.size or digest.hexdigest() != record.sha256:
                raise BrowseError("artifact_missing", "The retained screenshot changed before publication.")
            relative = destination.relative_to(scratch).as_posix()
            artifact_id = extension.publish_artifact(
                mime_type=record.mime_type,
                path=relative,
                size=record.size,
                sha256=record.sha256,
            )
            return artifact_id
        except BrowseError:
            raise
        except Exception as error:
            raise BrowseError("artifact_publish_failed", "The screenshot artifact was not published.") from error
        finally:
            try:
                destination.unlink()
            except FileNotFoundError:
                pass
            except OSError:
                pass

    def _prune_locked(self, *, keep: Optional[Path] = None) -> None:
        entries = []
        try:
            children = list(self.paths.screenshots.iterdir())
        except OSError:
            return
        for child in children:
            try:
                metadata = child.lstat()
            except OSError:
                continue
            if (
                child.suffix == ".png"
                and child.name.startswith("screenshot-")
                and stat.S_ISREG(metadata.st_mode)
                and not stat.S_ISLNK(metadata.st_mode)
            ):
                entries.append((metadata.st_mtime_ns, child, metadata.st_size))
        entries.sort(reverse=True)
        retained_count = 0
        retained_bytes = 0
        for _, path, size in entries:
            retain = (
                path == keep
                or (retained_count < MAX_SCREENSHOTS and retained_bytes + size <= MAX_SCREENSHOT_BYTES)
            )
            if retain:
                retained_count += 1
                retained_bytes += size
                continue
            try:
                path.unlink()
            except OSError:
                pass
