"""Persistent-profile sentinel, exclusive ownership, and safe reset."""

from __future__ import annotations

import json
import os
import shutil
import stat
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from .paths import BrowsePaths, PathSafetyError
from .safety import BrowseError, ExclusiveFileLock


PROFILE_SCHEMA = "ygg.browse.profile.v1"
SENTINEL_NAME = ".ygg-browse-profile.json"
MAX_SENTINEL_BYTES = 4096
_SENTINEL = {
    "schema": PROFILE_SCHEMA,
    "owner": "ygg-browse",
    "version": 1,
}


@dataclass
class ProfileLease:
    path: Path
    exists: bool
    _lock: ExclusiveFileLock

    def release(self) -> None:
        self._lock.release()


class ProfileManager:
    def __init__(self, paths: BrowsePaths) -> None:
        self.paths = paths

    def inspect(self) -> str:
        path = self.paths.profile
        if not path.exists() and not path.is_symlink():
            return "absent"
        try:
            self._validate_profile(path)
        except BrowseError:
            return "invalid"
        return "ready"

    def acquire(self, *, create: bool) -> ProfileLease:
        try:
            self.paths.ensure_root()
        except PathSafetyError as error:
            raise BrowseError("unsafe_profile", str(error)) from error
        lock = ExclusiveFileLock(self.paths.profile_lock)
        if not lock.acquire():
            raise BrowseError(
                "profile_locked",
                "Another Ygg Browse process owns the isolated profile.",
            )
        try:
            path = self.paths.profile
            exists = path.exists() or path.is_symlink()
            if not exists:
                if not create:
                    return ProfileLease(path, False, lock)
                self._create_profile(path)
                exists = True
            self._validate_profile(path)
            return ProfileLease(path, exists, lock)
        except BaseException:
            lock.release()
            raise

    def reset(self) -> bool:
        """Delete only a locked, sentinel-verified Ygg Browse profile."""

        lease = self.acquire(create=False)
        try:
            if not lease.exists:
                return False
            profile = lease.path
            before = profile.lstat()
            self._validate_profile(profile)
            tombstone = self.paths.root / f".profile-reset-{uuid.uuid4().hex}"
            if tombstone.exists() or tombstone.is_symlink():  # practically impossible, fail closed
                raise BrowseError("unsafe_profile", "The profile reset staging path already exists.")
            current = profile.lstat()
            if (before.st_dev, before.st_ino) != (current.st_dev, current.st_ino):
                raise BrowseError("unsafe_profile", "The profile changed during reset validation.")
            os.replace(str(profile), str(tombstone))
            metadata = tombstone.lstat()
            if not stat.S_ISDIR(metadata.st_mode) or stat.S_ISLNK(metadata.st_mode):
                raise BrowseError("unsafe_profile", "The staged profile is not an owned directory.")
            self._validate_profile(tombstone)
            shutil.rmtree(tombstone)
            return True
        finally:
            lease.release()

    def _create_profile(self, path: Path) -> None:
        created = False
        try:
            path.mkdir(mode=0o700)
            created = True
        except FileExistsError:
            pass
        try:
            metadata = path.lstat()
            if not stat.S_ISDIR(metadata.st_mode) or stat.S_ISLNK(metadata.st_mode):
                raise BrowseError(
                    "unsafe_profile", "The Ygg Browse profile path must be a real directory."
                )
            sentinel = path / SENTINEL_NAME
            temporary = path / f".{SENTINEL_NAME}.{uuid.uuid4().hex}.tmp"
            payload = (json.dumps(_SENTINEL, sort_keys=True, separators=(",", ":")) + "\n").encode(
                "utf-8"
            )
            flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
            flags |= getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
            fd = os.open(str(temporary), flags, 0o600)
            try:
                os.write(fd, payload)
                os.fsync(fd)
            finally:
                os.close(fd)
            os.replace(str(temporary), str(sentinel))
            _fsync_directory(path)
        except BaseException:
            # Only remove a directory created by this method when its sentinel
            # was never published. Never clean an existing unverified path.
            try:
                if (
                    created
                    and path.is_dir()
                    and not path.is_symlink()
                    and not (path / SENTINEL_NAME).exists()
                ):
                    path.rmdir()
            except OSError:
                pass
            raise

    @staticmethod
    def _validate_profile(path: Path) -> None:
        try:
            metadata = path.lstat()
        except FileNotFoundError as error:
            raise BrowseError("profile_missing", "The Ygg Browse profile is missing.") from error
        if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
            raise BrowseError(
                "unsafe_profile", "The Ygg Browse profile path must be a non-symlink directory."
            )
        sentinel = path / SENTINEL_NAME
        try:
            sentinel_metadata = sentinel.lstat()
        except FileNotFoundError as error:
            raise BrowseError(
                "invalid_profile_sentinel",
                "The isolated profile sentinel is absent; launch and reset were refused.",
            ) from error
        if (
            stat.S_ISLNK(sentinel_metadata.st_mode)
            or not stat.S_ISREG(sentinel_metadata.st_mode)
            or sentinel_metadata.st_nlink != 1
            or sentinel_metadata.st_size > MAX_SENTINEL_BYTES
        ):
            raise BrowseError(
                "invalid_profile_sentinel",
                "The isolated profile sentinel is invalid; launch and reset were refused.",
            )
        flags = os.O_RDONLY | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
        try:
            fd = os.open(str(sentinel), flags)
            try:
                opened = os.fstat(fd)
                if (
                    not stat.S_ISREG(opened.st_mode)
                    or opened.st_nlink != 1
                    or (opened.st_dev, opened.st_ino)
                    != (sentinel_metadata.st_dev, sentinel_metadata.st_ino)
                ):
                    raise BrowseError(
                        "invalid_profile_sentinel",
                        "The isolated profile sentinel changed during validation.",
                    )
                raw = os.read(fd, MAX_SENTINEL_BYTES + 1)
            finally:
                os.close(fd)
            value: Any = json.loads(raw.decode("utf-8"))
        except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
            raise BrowseError(
                "invalid_profile_sentinel",
                "The isolated profile sentinel is invalid; launch and reset were refused.",
            ) from error
        if value != _SENTINEL:
            raise BrowseError(
                "invalid_profile_sentinel",
                "The isolated profile sentinel is invalid; launch and reset were refused.",
            )


def _fsync_directory(path: Path) -> None:
    flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_CLOEXEC", 0)
    try:
        fd = os.open(str(path), flags)
    except OSError:
        return
    try:
        os.fsync(fd)
    except OSError:
        pass
    finally:
        os.close(fd)
