"""Strict, bounded configuration for the ygg-ssh portal registry.

The registry only maps stable target identifiers to OpenSSH aliases that the
user already configured and authenticated in their own ``~/.ssh/config``.
It deliberately holds no limits, authority levels, or project trust chains:
the agent operates the remote host through its normal shell tool, and the
remote account's own OpenSSH-enforced permissions are the boundary.
"""

from __future__ import annotations

import json
import os
import re
import stat
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any, Mapping, Optional, Union


MAX_CONFIG_BYTES = 64 * 1024
MAX_TARGETS = 32
MAX_TARGET_ID_BYTES = 32
MAX_ALIAS_BYTES = 128
MAX_LABEL_BYTES = 96
MAX_REMOTE_PATH_BYTES = 4096
TARGET_ID_PATTERN = r"[a-z][a-z0-9-]{0,31}"
_TARGET_ID = re.compile(rf"^{TARGET_ID_PATTERN}$")
# A deliberately conservative OpenSSH destination alias. User, port, ProxyJump,
# and identity selection remain in the user's OpenSSH configuration and cannot
# be introduced through model or command arguments.
_SSH_ALIAS = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$")


class ConfigError(ValueError):
    """Configuration failed a bounded schema or file-trust check."""


@dataclass(frozen=True)
class Target:
    """One explicit OpenSSH alias with an optional working-directory hint."""

    id: str
    alias: str
    label: str
    cwd: Optional[str] = None
    enabled: bool = True


@dataclass(frozen=True)
class SshConfig:
    targets: tuple[Target, ...]
    source: Optional[Path] = None

    @classmethod
    def empty(cls, source: Optional[Path] = None) -> "SshConfig":
        return cls(targets=(), source=source)

    def target(self, target_id: str) -> Optional[Target]:
        return next((target for target in self.targets if target.id == target_id), None)

    def enabled_targets(self) -> tuple[Target, ...]:
        return tuple(target for target in self.targets if target.enabled)


_TARGET_FIELDS = {"alias", "label", "cwd", "enabled"}


def default_config_path() -> Path:
    """Return the inert user configuration location."""

    return Path.home() / ".ygg" / "ssh.json"


def load_config(path: Optional[Union[os.PathLike[str], str]] = None) -> SshConfig:
    """Load the user registry. A missing default file is valid and inert."""

    explicit = path is not None
    config_path = Path(path) if path is not None else default_config_path()
    if not config_path.exists():
        if explicit:
            raise ConfigError("the requested SSH configuration does not exist")
        return SshConfig.empty(config_path)

    root, _root_bytes, canonical_path = _read_json_file(config_path)
    _require_keys(root, {"version", "targets"}, "config")
    if root.get("version") != 1:
        raise ConfigError("SSH configuration version must be 1")
    targets = _parse_targets(root.get("targets", {}))
    return SshConfig(targets=tuple(targets), source=canonical_path)


def _read_json_file(path: Path) -> tuple[dict[str, Any], bytes, Path]:
    try:
        metadata = path.lstat()
    except OSError as error:
        raise ConfigError("cannot inspect SSH configuration") from error
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISREG(metadata.st_mode):
        raise ConfigError("SSH configuration must be a regular, non-symlink file")
    if metadata.st_size > MAX_CONFIG_BYTES:
        raise ConfigError(f"SSH configuration exceeds the {MAX_CONFIG_BYTES}-byte limit")
    if hasattr(os, "getuid") and metadata.st_uid != os.getuid():
        raise ConfigError("SSH configuration must be owned by the current user")
    if metadata.st_mode & (stat.S_IWGRP | stat.S_IWOTH):
        raise ConfigError("SSH configuration cannot be group- or world-writable")
    try:
        with path.open("rb") as handle:
            data = handle.read(MAX_CONFIG_BYTES + 1)
    except OSError as error:
        raise ConfigError("cannot read SSH configuration") from error
    if len(data) > MAX_CONFIG_BYTES:
        raise ConfigError(f"SSH configuration exceeds the {MAX_CONFIG_BYTES}-byte limit")

    def unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
        value: dict[str, Any] = {}
        for key, item in pairs:
            if key in value:
                raise ConfigError("SSH configuration contains a duplicate object key")
            value[key] = item
        return value

    try:
        value = json.loads(data.decode("utf-8"), object_pairs_hook=unique_object)
    except ConfigError:
        raise
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ConfigError("SSH configuration is not valid strict UTF-8 JSON") from error
    if not isinstance(value, dict):
        raise ConfigError("SSH configuration root must be an object")
    return value, data, path.resolve(strict=True)


def _parse_targets(value: Any) -> list[Target]:
    if not isinstance(value, dict):
        raise ConfigError("targets must be an object")
    if len(value) > MAX_TARGETS:
        raise ConfigError(f"targets exceed the {MAX_TARGETS}-target limit")
    parsed: list[Target] = []
    aliases: set[str] = set()
    for target_id, descriptor in value.items():
        if not isinstance(target_id, str) or not _TARGET_ID.fullmatch(target_id):
            raise ConfigError("target identifiers must match [a-z][a-z0-9-]{0,31}")
        if len(target_id.encode("utf-8")) > MAX_TARGET_ID_BYTES:
            raise ConfigError("target identifier is too long")
        if not isinstance(descriptor, dict):
            raise ConfigError(f"target {target_id} must be an object")
        _require_keys(descriptor, _TARGET_FIELDS, f"target {target_id}")
        alias = _bounded_text(descriptor.get("alias"), f"target {target_id} alias", MAX_ALIAS_BYTES)
        if not _SSH_ALIAS.fullmatch(alias):
            raise ConfigError(
                f"target {target_id} alias must contain only letters, digits, dot, underscore, or hyphen"
            )
        folded = alias.casefold()
        if folded in aliases:
            raise ConfigError("OpenSSH aliases must be unique")
        aliases.add(folded)
        label = _bounded_text(
            descriptor.get("label", alias), f"target {target_id} label", MAX_LABEL_BYTES
        )
        cwd = descriptor.get("cwd")
        if cwd is not None:
            cwd = _bounded_text(cwd, f"target {target_id} cwd", MAX_REMOTE_PATH_BYTES)
            remote_path = PurePosixPath(cwd)
            if (
                not remote_path.is_absolute()
                or str(remote_path) != cwd
                or any(part in {"", ".", ".."} for part in remote_path.parts[1:])
            ):
                raise ConfigError(f"target {target_id} cwd must be a normalized absolute POSIX path")
        enabled = descriptor.get("enabled", True)
        if not isinstance(enabled, bool):
            raise ConfigError(f"target {target_id} enabled must be a boolean")
        parsed.append(Target(id=target_id, alias=alias, label=label, cwd=cwd, enabled=enabled))
    return parsed


def _require_keys(value: Mapping[str, Any], allowed: set[str], label: str) -> None:
    if set(value) - allowed:
        raise ConfigError(f"{label} contains unknown fields")


def _bounded_text(value: Any, label: str, maximum: int) -> str:
    if not isinstance(value, str) or not value.strip():
        raise ConfigError(f"{label} must be a non-empty string")
    if "\x00" in value or any(ord(character) < 32 or 127 <= ord(character) <= 159 for character in value):
        raise ConfigError(f"{label} contains a control character")
    if len(value.encode("utf-8")) > maximum:
        raise ConfigError(f"{label} exceeds the {maximum}-byte limit")
    return value
