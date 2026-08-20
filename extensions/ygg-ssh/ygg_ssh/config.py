"""Strict, bounded configuration for the authenticated OpenSSH adapter."""

from __future__ import annotations

import hashlib
import json
import os
import re
import stat
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any, Mapping, Optional, Union


MAX_CONFIG_BYTES = 256 * 1024
MAX_TRUSTED_PROJECTS = 8
MAX_TARGETS = 32
MAX_TARGET_ID_BYTES = 32
MAX_ALIAS_BYTES = 128
MAX_LABEL_BYTES = 96
MAX_REMOTE_PATH_BYTES = 4096
_TARGET_ID = re.compile(r"^[a-z][a-z0-9-]{0,31}$")
# A deliberately conservative OpenSSH destination alias. User, port, ProxyJump,
# and identity selection remain in the user's OpenSSH configuration and cannot
# be introduced through model or tool arguments.
_SSH_ALIAS = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$")
_HEX_DIGEST = re.compile(r"^[0-9a-f]{64}$")


class ConfigError(ValueError):
    """Configuration failed a bounded schema or trust check."""


@dataclass(frozen=True)
class Limits:
    max_sessions: int = 8
    connect_timeout_ms: int = 10_000
    operation_timeout_ms: int = 30_000
    max_output_bytes: int = 128 * 1024
    max_file_bytes: int = 128 * 1024
    max_command_args: int = 64
    max_command_bytes: int = 32 * 1024
    max_activities: int = 64
    health_interval_ms: int = 2_000
    shutdown_timeout_ms: int = 1_500
    termination_grace_ms: int = 250


@dataclass(frozen=True)
class Target:
    """One explicit OpenSSH alias and fixed remote working directory."""

    id: str
    alias: str
    label: str
    remote_cwd: str
    authority: str = "read-only"
    enabled: bool = True
    scope: str = "user"


@dataclass(frozen=True)
class SshConfig:
    targets: tuple[Target, ...]
    limits: Limits = Limits()
    source: Optional[Path] = None

    @classmethod
    def empty(cls, source: Optional[Path] = None) -> "SshConfig":
        return cls(targets=(), source=source)

    def target(self, target_id: str) -> Optional[Target]:
        return next((target for target in self.targets if target.id == target_id), None)


_LIMIT_FIELDS: dict[str, tuple[str, int, int]] = {
    "maxSessions": ("max_sessions", 1, 16),
    "connectTimeoutMs": ("connect_timeout_ms", 100, 30_000),
    "operationTimeoutMs": ("operation_timeout_ms", 100, 120_000),
    "maxOutputBytes": ("max_output_bytes", 1024, 128 * 1024),
    "maxFileBytes": ("max_file_bytes", 1024, 128 * 1024),
    "maxCommandArgs": ("max_command_args", 1, 128),
    "maxCommandBytes": ("max_command_bytes", 256, 64 * 1024),
    "maxActivities": ("max_activities", 1, 128),
    "healthIntervalMs": ("health_interval_ms", 250, 60_000),
    "shutdownTimeoutMs": ("shutdown_timeout_ms", 100, 5_000),
    "terminationGraceMs": ("termination_grace_ms", 25, 2_000),
}
_TARGET_FIELDS = {"alias", "label", "remoteCwd", "authority", "enabled"}


def default_config_path() -> Path:
    """Return the inert user configuration location."""

    return Path.home() / ".ygg" / "ssh.json"


def load_config(
    path: Optional[Union[os.PathLike[str], str]] = None,
    *,
    workspace: Optional[Union[os.PathLike[str], str]] = None,
) -> SshConfig:
    """Load a user file and explicitly digest-pinned project files.

    Missing default configuration is valid and inert. An explicit path must
    exist. Project configuration is considered only when the user file names an
    absolute file below the active workspace's ``.ygg`` directory and pins the
    exact bytes.
    """

    explicit = path is not None
    config_path = Path(path) if path is not None else default_config_path()
    if not config_path.exists():
        if explicit:
            raise ConfigError("the requested SSH configuration does not exist")
        return SshConfig.empty(config_path)

    root, _root_bytes, canonical_path = _read_json_file(config_path)
    _require_keys(root, {"version", "limits", "targets", "trustedProjects"}, "config")
    _require_version(root)
    limits = _parse_limits(root.get("limits", {}))
    if limits.max_file_bytes > limits.max_output_bytes:
        raise ConfigError("maxFileBytes cannot exceed maxOutputBytes")
    targets = _parse_targets(root.get("targets", {}), scope="user")

    workspace_path = Path(workspace).resolve() if workspace is not None else None
    projects = root.get("trustedProjects", [])
    if not isinstance(projects, list):
        raise ConfigError("trustedProjects must be an array")
    if len(projects) > MAX_TRUSTED_PROJECTS:
        raise ConfigError(f"trustedProjects exceeds the {MAX_TRUSTED_PROJECTS}-file limit")
    if projects and workspace_path is None:
        raise ConfigError("trusted project configuration requires an active workspace")

    seen_ids = {target.id for target in targets}
    seen_aliases = {target.alias.casefold() for target in targets}
    for index, descriptor in enumerate(projects):
        project_path, expected_digest = _trusted_project_descriptor(descriptor, index)
        assert workspace_path is not None
        project_root = (workspace_path / ".ygg").resolve(strict=False)
        try:
            canonical_project = project_path.resolve(strict=True)
        except OSError as error:
            raise ConfigError("cannot resolve trusted project SSH configuration") from error
        try:
            canonical_project.relative_to(project_root)
        except ValueError as error:
            raise ConfigError("a trusted project SSH configuration is outside workspace/.ygg") from error
        project, project_bytes, _project_file = _read_json_file(project_path)
        if hashlib.sha256(project_bytes).hexdigest() != expected_digest:
            raise ConfigError("a trusted project SSH configuration digest does not match")
        _require_keys(project, {"version", "targets"}, "trusted project config")
        _require_version(project)
        project_targets = _parse_targets(project.get("targets", {}), scope="project")
        if seen_ids.intersection(target.id for target in project_targets):
            raise ConfigError("target identifiers must be unique across user and project config")
        if seen_aliases.intersection(target.alias.casefold() for target in project_targets):
            raise ConfigError("OpenSSH aliases must be unique across user and project config")
        targets.extend(project_targets)
        seen_ids.update(target.id for target in project_targets)
        seen_aliases.update(target.alias.casefold() for target in project_targets)

    if len(targets) > MAX_TARGETS:
        raise ConfigError("configured SSH targets exceed the bounded target limit")
    return SshConfig(targets=tuple(targets), limits=limits, source=canonical_path)


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


def _require_version(value: Mapping[str, Any]) -> None:
    if value.get("version") != 1:
        raise ConfigError("SSH configuration version must be 1")


def _parse_limits(value: Any) -> Limits:
    if not isinstance(value, dict):
        raise ConfigError("limits must be an object")
    _require_keys(value, set(_LIMIT_FIELDS), "limits")
    parsed = Limits().__dict__.copy()
    for public_name, item in value.items():
        internal_name, minimum, maximum = _LIMIT_FIELDS[public_name]
        parsed[internal_name] = _bounded_integer(item, public_name, minimum, maximum)
    return Limits(**parsed)


def _parse_targets(value: Any, *, scope: str) -> list[Target]:
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
        remote_cwd = _bounded_text(
            descriptor.get("remoteCwd"),
            f"target {target_id} remoteCwd",
            MAX_REMOTE_PATH_BYTES,
        )
        remote_path = PurePosixPath(remote_cwd)
        if (
            not remote_path.is_absolute()
            or str(remote_path) != remote_cwd
            or any(part in {"", ".", ".."} for part in remote_path.parts[1:])
        ):
            raise ConfigError(f"target {target_id} remoteCwd must be a normalized absolute POSIX path")
        authority = descriptor.get("authority", "read-only")
        if authority not in {"read-only", "read-write"}:
            raise ConfigError(f"target {target_id} authority must be read-only or read-write")
        enabled = descriptor.get("enabled", True)
        if not isinstance(enabled, bool):
            raise ConfigError(f"target {target_id} enabled must be a boolean")
        parsed.append(
            Target(
                id=target_id,
                alias=alias,
                label=label,
                remote_cwd=remote_cwd,
                authority=authority,
                enabled=enabled,
                scope=scope,
            )
        )
    return parsed


def _trusted_project_descriptor(value: Any, index: int) -> tuple[Path, str]:
    if not isinstance(value, dict):
        raise ConfigError(f"trustedProjects[{index}] must be an object")
    _require_keys(value, {"path", "sha256"}, f"trustedProjects[{index}]")
    path_value = _bounded_text(value.get("path"), "trusted project path", MAX_REMOTE_PATH_BYTES)
    project_path = Path(path_value)
    if not project_path.is_absolute():
        raise ConfigError("trusted project configuration paths must be absolute")
    digest = value.get("sha256")
    if not isinstance(digest, str) or not _HEX_DIGEST.fullmatch(digest):
        raise ConfigError("trusted project sha256 must be 64 lowercase hexadecimal characters")
    return project_path, digest


def _require_keys(value: Mapping[str, Any], allowed: set[str], label: str) -> None:
    if set(value) - allowed:
        raise ConfigError(f"{label} contains unknown fields")


def _bounded_integer(value: Any, label: str, minimum: int, maximum: int) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        raise ConfigError(f"{label} must be an integer")
    if not minimum <= value <= maximum:
        raise ConfigError(f"{label} must be between {minimum} and {maximum}")
    return value


def _bounded_text(value: Any, label: str, maximum: int) -> str:
    if not isinstance(value, str) or not value.strip():
        raise ConfigError(f"{label} must be a non-empty string")
    if "\x00" in value or any(ord(character) < 32 or 127 <= ord(character) <= 159 for character in value):
        raise ConfigError(f"{label} contains a control character")
    if len(value.encode("utf-8")) > maximum:
        raise ConfigError(f"{label} exceeds the {maximum}-byte limit")
    return value
