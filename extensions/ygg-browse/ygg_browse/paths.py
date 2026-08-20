"""Filesystem locations owned exclusively by Ygg Browse."""

from __future__ import annotations

import os
import stat
import unicodedata
from dataclasses import dataclass
from pathlib import Path
from typing import Optional


PLAYWRIGHT_VERSION = "1.57.0"
RUNTIME_DIRECTORY_NAME = "playwright-1.57.0"


class PathSafetyError(RuntimeError):
    """An owned path failed a no-link or file-type safety check."""


@dataclass(frozen=True)
class BrowsePaths:
    """Resolved Ygg-owned state paths.

    Constructing this object is inert. Directories are created only by explicit
    setup, launch, screenshot, or reset operations.
    """

    root: Path

    @classmethod
    def for_home(cls, home: Optional[Path] = None) -> "BrowsePaths":
        base = Path.home() if home is None else Path(home)
        return cls(base.expanduser().absolute() / ".ygg" / "browse")

    @property
    def profile(self) -> Path:
        return self.root / "profile"

    @property
    def profile_lock(self) -> Path:
        return self.root / "profile.lock"

    @property
    def runtime_parent(self) -> Path:
        return self.root / "runtime"

    @property
    def runtime(self) -> Path:
        return self.runtime_parent / RUNTIME_DIRECTORY_NAME

    @property
    def screenshots(self) -> Path:
        return self.root / "artifacts" / "screenshots"

    @property
    def install_lock(self) -> Path:
        return self.root / "install.lock"

    @property
    def install_log(self) -> Path:
        return self.root / "install.log"

    @property
    def setup_state(self) -> Path:
        return self.root / "setup-state.json"

    def ensure_root(self) -> None:
        """Create the private browse root without accepting a linked root."""

        parent = self.root.parent
        parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        _require_directory(parent, "Ygg state parent")
        if self.root.exists() or self.root.is_symlink():
            _require_directory(self.root, "Ygg Browse root")
        else:
            self.root.mkdir(mode=0o700)
        try:
            self.root.chmod(0o700)
        except OSError:
            pass

    def ensure_directory(self, path: Path) -> None:
        self.ensure_root()
        try:
            relative = path.relative_to(self.root)
        except ValueError as error:
            raise PathSafetyError("owned directory escaped the Ygg Browse root") from error
        current = self.root
        for component in relative.parts:
            current = current / component
            if current.exists() or current.is_symlink():
                _require_directory(current, "owned directory")
            else:
                current.mkdir(mode=0o700)
            try:
                current.chmod(0o700)
            except OSError:
                pass

    def display(self, path: Path) -> str:
        """Return a stable, bounded user-facing path with the home abbreviated."""

        absolute = str(path.absolute())
        home = str(Path.home().absolute())
        if absolute == home:
            displayed = "~"
        else:
            prefix = home + os.sep
            displayed = "~" + os.sep + absolute[len(prefix) :] if absolute.startswith(prefix) else absolute
        displayed = "".join(
            "_" if unicodedata.category(character).startswith("C") else character
            for character in displayed
        )
        return displayed[:1024]


def _require_directory(path: Path, label: str) -> None:
    try:
        metadata = path.lstat()
    except FileNotFoundError as error:
        raise PathSafetyError(f"{label} is missing") from error
    if stat.S_ISLNK(metadata.st_mode):
        raise PathSafetyError(f"{label} must not be a symbolic link")
    if not stat.S_ISDIR(metadata.st_mode):
        raise PathSafetyError(f"{label} must be a directory")
