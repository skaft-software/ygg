"""Strict, inert configuration for the Hermes MemoryProvider bridge.

The file contains only discovery metadata and trust decisions.  Credential
values are deliberately not part of this schema; providers read their normal
explicit environment/configuration after the user selects them.
"""

from __future__ import annotations

from dataclasses import dataclass, field
import json
import os
from pathlib import Path
import re
import stat
import sys
from typing import Any, Dict, Mapping, Optional, Tuple, Union

from .constants import (
    HERMES_CONTRACT_COMMIT,
    HERMES_CONTRACT_ID,
    HERMES_CONTRACT_VERSION,
    MAX_CONFIG_BYTES,
    MAX_CONTEXT_BYTES,
    MAX_DISCOVERED_PROVIDERS,
    MAX_PROVIDER_TOOLS,
    MAX_QUERY_BYTES,
    MAX_TOOL_RESULT_BYTES,
)


_ID_RE = re.compile(r"^[a-z][a-z0-9-]{0,31}$")
_CANDIDATE_ID_RE = re.compile(r"^(?:directory|entrypoint):[A-Za-z_][A-Za-z0-9_.-]{0,63}$")
_TOOL_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_.-]{0,63}$")
_DIGEST_RE = re.compile(r"^[0-9a-f]{64}$")
_ALLOWED_NETWORK = frozenset({"none", "optional", "required", "unknown"})
_ALLOWED_STORAGE = frozenset({"none", "local", "remote", "mixed", "unknown"})
_ALLOWED_SETUP = frozenset({"configured", "required", "unknown"})


class ConfigError(ValueError):
    """The user configuration failed a bounded trust or schema check."""


@dataclass(frozen=True)
class Limits:
    """User-tunable limits capped by package-owned maxima."""

    max_providers: int = 32
    max_tools: int = 32
    max_query_bytes: int = 16 * 1024
    max_context_bytes: int = 32 * 1024
    max_tool_result_bytes: int = 32 * 1024
    max_owners: int = 8
    max_queue_depth: int = 16
    availability_timeout_ms: int = 1000
    initialize_timeout_ms: int = 5000
    prefetch_timeout_ms: int = 3000
    tool_timeout_ms: int = 30_000
    sync_timeout_ms: int = 5000
    shutdown_timeout_ms: int = 1000


@dataclass(frozen=True)
class ProviderBehavior:
    """Safe, user-declared provider behavior shown in the picker."""

    label: Optional[str] = None
    network: str = "unknown"
    storage: str = "unknown"
    setup: str = "unknown"
    read_tools: Tuple[str, ...] = ()
    write_tools: Tuple[str, ...] = ()


@dataclass(frozen=True)
class DirectorySource:
    """One explicitly named directory provider; roots are never scanned."""

    id: str
    path: Path = field(repr=False)
    behavior: ProviderBehavior = ProviderBehavior()

    @property
    def candidate_id(self) -> str:
        return f"directory:{self.id}"


@dataclass(frozen=True)
class EnvironmentConfig:
    """Identity of the already-provisioned Hermes provider environment."""

    id: str
    python: Optional[Path] = field(default=None, repr=False)
    hermes_home: Optional[Path] = field(default=None, repr=False)
    provider_env_file: Optional[Path] = field(default=None, repr=False)
    include_entry_points: bool = False


@dataclass(frozen=True)
class BridgeConfig:
    """Complete bridge configuration with no provider credentials."""

    environment: Optional[EnvironmentConfig]
    directories: Tuple[DirectorySource, ...] = ()
    provider_metadata: Mapping[str, ProviderBehavior] = field(default_factory=dict)
    trusted_providers: Mapping[str, str] = field(default_factory=dict, repr=False)
    default_provider: Optional[str] = None
    limits: Limits = Limits()
    source: Optional[Path] = field(default=None, repr=False)
    contract_id: str = HERMES_CONTRACT_ID

    @classmethod
    def empty(cls, source: Optional[Path] = None) -> "BridgeConfig":
        return cls(environment=None, source=source)

    def trusted_fingerprint(self, candidate_id: str) -> Optional[str]:
        return self.trusted_providers.get(candidate_id)


_LIMIT_FIELDS = {
    "maxProviders": ("max_providers", 1, MAX_DISCOVERED_PROVIDERS),
    "maxTools": ("max_tools", 1, MAX_PROVIDER_TOOLS),
    "maxQueryBytes": ("max_query_bytes", 256, MAX_QUERY_BYTES),
    "maxContextBytes": ("max_context_bytes", 256, MAX_CONTEXT_BYTES),
    "maxToolResultBytes": ("max_tool_result_bytes", 256, MAX_TOOL_RESULT_BYTES),
    "maxOwners": ("max_owners", 1, 32),
    "maxQueueDepth": ("max_queue_depth", 1, 64),
    "availabilityTimeoutMs": ("availability_timeout_ms", 10, 5000),
    "initializeTimeoutMs": ("initialize_timeout_ms", 10, 30_000),
    "prefetchTimeoutMs": ("prefetch_timeout_ms", 10, 30_000),
    "toolTimeoutMs": ("tool_timeout_ms", 10, 120_000),
    "syncTimeoutMs": ("sync_timeout_ms", 10, 30_000),
    "shutdownTimeoutMs": ("shutdown_timeout_ms", 10, 5000),
}
_TOP_LEVEL_FIELDS = {
    "version",
    "contract",
    "environment",
    "directories",
    "providerMetadata",
    "trustedProviders",
    "defaultProvider",
    "limits",
}
_BEHAVIOR_FIELDS = {"label", "network", "storage", "setup", "readTools", "writeTools"}


def default_config_path() -> Path:
    """Return the inert user configuration location without creating it."""

    return Path.home() / ".ygg" / "hermes-memory.json"


def load_config(
    path: Optional[Union[os.PathLike, str]] = None,
    *,
    require_private: bool = True,
) -> BridgeConfig:
    """Read a bounded user-owned JSON file.

    A missing default file is a healthy, disabled bridge.  Supplying an
    explicit missing path is an error.  Loading this file never imports a
    provider, invokes Python from the configured environment, or creates a
    provider store.
    """

    explicit = path is not None
    selected = Path(path) if path is not None else default_config_path()
    if not selected.exists():
        if explicit:
            raise ConfigError("the requested Hermes memory configuration does not exist")
        return BridgeConfig.empty(selected)

    root, canonical = _read_json_file(selected, require_private=require_private)
    _require_keys(root, _TOP_LEVEL_FIELDS, "config")
    if root.get("version") != 1:
        raise ConfigError("Hermes memory configuration version must be 1")
    _parse_contract(root.get("contract"))
    environment = _parse_environment(root.get("environment"))
    limits = _parse_limits(root.get("limits", {}))
    directories = _parse_directories(root.get("directories", []), limits)
    provider_metadata = _parse_provider_metadata(root.get("providerMetadata", {}))
    trusted = _parse_trust(root.get("trustedProviders", {}))
    default_provider = root.get("defaultProvider")
    if default_provider is not None:
        _candidate_id(default_provider, "defaultProvider")
    return BridgeConfig(
        environment=environment,
        directories=directories,
        provider_metadata=provider_metadata,
        trusted_providers=trusted,
        default_provider=default_provider,
        limits=limits,
        source=canonical,
    )


def current_environment_matches(environment: Optional[EnvironmentConfig]) -> bool:
    """Whether this interpreter is the explicitly configured provider Python."""

    if environment is None or environment.python is None:
        return False
    try:
        configured = Path(os.path.abspath(str(environment.python.expanduser())))
        running = Path(os.path.abspath(sys.executable))
        return running == configured and configured.exists()
    except OSError:
        return False


def _read_json_file(path: Path, *, require_private: bool) -> Tuple[Dict[str, Any], Path]:
    absolute = Path(os.path.abspath(os.fspath(path.expanduser())))
    try:
        canonical_parent = absolute.parent.resolve(strict=True)
    except OSError as error:
        raise ConfigError("cannot resolve Hermes memory configuration directory") from error
    canonical = canonical_parent / absolute.name
    if os.open not in getattr(os, "supports_dir_fd", set()):
        raise ConfigError("this platform cannot securely open Hermes memory configuration")
    parent_descriptor = None
    try:
        directory_flags = os.O_RDONLY
        if hasattr(os, "O_DIRECTORY"):
            directory_flags |= os.O_DIRECTORY
        if hasattr(os, "O_NOFOLLOW"):
            directory_flags |= os.O_NOFOLLOW
        if hasattr(os, "O_CLOEXEC"):
            directory_flags |= os.O_CLOEXEC
        parent_descriptor = os.open(canonical_parent, directory_flags)
        descriptor = _open_config_descriptor(
            absolute.name,
            require_private=require_private,
            directory_fd=parent_descriptor,
        )
    except OSError as error:
        raise ConfigError("cannot open Hermes memory configuration directory safely") from error
    finally:
        if parent_descriptor is not None:
            os.close(parent_descriptor)
    data = _read_config_descriptor(descriptor)

    def unique_object(pairs: Any) -> Dict[str, Any]:
        value: Dict[str, Any] = {}
        for key, item in pairs:
            if key in value:
                raise ConfigError("Hermes memory configuration contains a duplicate key")
            value[key] = item
        return value

    try:
        decoded = data.decode("utf-8")
        value = json.loads(decoded, object_pairs_hook=unique_object)
    except ConfigError:
        raise
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ConfigError("Hermes memory configuration is not strict UTF-8 JSON") from error
    if not isinstance(value, dict):
        raise ConfigError("Hermes memory configuration root must be an object")
    return value, canonical


def _open_config_descriptor(
    path: Union[os.PathLike, str],
    *,
    require_private: bool,
    directory_fd: Optional[int] = None,
) -> int:
    flags = os.O_RDONLY
    if hasattr(os, "O_CLOEXEC"):
        flags |= os.O_CLOEXEC
    before = None
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    else:  # pragma: no cover - release hosts provide O_NOFOLLOW
        try:
            before = os.stat(path, dir_fd=directory_fd, follow_symlinks=False)
        except OSError as error:
            raise ConfigError("cannot inspect Hermes memory configuration") from error
        if stat.S_ISLNK(before.st_mode):
            raise ConfigError("Hermes memory configuration must be a regular non-symlink file")
    descriptor = None
    try:
        descriptor = os.open(path, flags, dir_fd=directory_fd)
        metadata = os.fstat(descriptor)
    except OSError as error:
        if descriptor is not None:
            os.close(descriptor)
        raise ConfigError(
            "cannot open Hermes memory configuration safely; it must be a regular non-symlink file"
        ) from error
    assert descriptor is not None
    if before is not None and (before.st_dev, before.st_ino) != (
        metadata.st_dev,
        metadata.st_ino,
    ):
        os.close(descriptor)
        raise ConfigError("Hermes memory configuration changed while it was opened")
    try:
        if not stat.S_ISREG(metadata.st_mode):
            raise ConfigError("Hermes memory configuration must be a regular non-symlink file")
        if metadata.st_size > MAX_CONFIG_BYTES:
            raise ConfigError(f"Hermes memory configuration exceeds {MAX_CONFIG_BYTES} bytes")
        if hasattr(os, "getuid") and metadata.st_uid != os.getuid():
            raise ConfigError("Hermes memory configuration must be owned by the current user")
        if require_private and metadata.st_mode & (stat.S_IWGRP | stat.S_IWOTH):
            raise ConfigError("Hermes memory configuration cannot be group- or world-writable")
        if getattr(metadata, "st_nlink", 1) != 1:
            raise ConfigError("Hermes memory configuration cannot have additional hard links")
    except ConfigError:
        os.close(descriptor)
        raise
    return descriptor


def _read_config_descriptor(descriptor: int) -> bytes:
    chunks = []
    remaining = MAX_CONFIG_BYTES + 1
    try:
        while remaining:
            chunk = os.read(descriptor, min(16 * 1024, remaining))
            if not chunk:
                break
            chunks.append(chunk)
            remaining -= len(chunk)
    except OSError as error:
        raise ConfigError("cannot read Hermes memory configuration") from error
    finally:
        os.close(descriptor)
    data = b"".join(chunks)
    if len(data) > MAX_CONFIG_BYTES:
        raise ConfigError(f"Hermes memory configuration exceeds {MAX_CONFIG_BYTES} bytes")
    return data


def _parse_contract(value: Any) -> None:
    if not isinstance(value, dict):
        raise ConfigError("contract must declare the pinned Hermes version and commit")
    _require_keys(value, {"hermesVersion", "commit"}, "contract")
    if value.get("hermesVersion") != HERMES_CONTRACT_VERSION:
        raise ConfigError(f"contract.hermesVersion must be {HERMES_CONTRACT_VERSION}")
    if value.get("commit") != HERMES_CONTRACT_COMMIT:
        raise ConfigError(f"contract.commit must be {HERMES_CONTRACT_COMMIT}")


def _parse_environment(value: Any) -> EnvironmentConfig:
    if not isinstance(value, dict):
        raise ConfigError("environment must be an object")
    _require_keys(
        value,
        {"id", "python", "hermesHome", "providerEnvFile", "includeEntryPoints"},
        "environment",
    )
    environment_id = _text(value.get("id"), "environment.id", 64)
    python = _absolute_path(value.get("python"), "environment.python")
    hermes_home = _absolute_path(value.get("hermesHome"), "environment.hermesHome")
    provider_env_value = value.get("providerEnvFile")
    provider_env_file = (
        None
        if provider_env_value is None
        else _absolute_path(provider_env_value, "environment.providerEnvFile")
    )
    include = value.get("includeEntryPoints", False)
    if not isinstance(include, bool):
        raise ConfigError("environment.includeEntryPoints must be a boolean")
    return EnvironmentConfig(
        id=environment_id,
        python=python,
        hermes_home=hermes_home,
        provider_env_file=provider_env_file,
        include_entry_points=include,
    )


def _parse_limits(value: Any) -> Limits:
    if not isinstance(value, dict):
        raise ConfigError("limits must be an object")
    _require_keys(value, set(_LIMIT_FIELDS), "limits")
    parsed = dict(Limits().__dict__)
    for public, item in value.items():
        field_name, minimum, maximum = _LIMIT_FIELDS[public]
        if not isinstance(item, int) or isinstance(item, bool) or not minimum <= item <= maximum:
            raise ConfigError(f"limits.{public} must be an integer from {minimum} to {maximum}")
        parsed[field_name] = item
    return Limits(**parsed)


def _parse_directories(value: Any, limits: Limits) -> Tuple[DirectorySource, ...]:
    if not isinstance(value, list):
        raise ConfigError("directories must be an array")
    if len(value) > limits.max_providers:
        raise ConfigError(f"directories exceed the {limits.max_providers}-provider limit")
    result = []
    seen = set()
    for index, descriptor in enumerate(value):
        if not isinstance(descriptor, dict):
            raise ConfigError(f"directories[{index}] must be an object")
        _require_keys(descriptor, {"id", "path"}.union(_BEHAVIOR_FIELDS), f"directories[{index}]")
        identifier = descriptor.get("id")
        if not isinstance(identifier, str) or not _ID_RE.fullmatch(identifier):
            raise ConfigError("directory ids must match [a-z][a-z0-9-]{0,31}")
        if identifier in seen:
            raise ConfigError("directory ids must be unique")
        seen.add(identifier)
        path = _absolute_path(descriptor.get("path"), f"directories[{index}].path")
        behavior = _parse_behavior(descriptor, f"directories[{index}]", omit={"id", "path"})
        result.append(DirectorySource(identifier, path, behavior))
    return tuple(result)


def _parse_provider_metadata(value: Any) -> Mapping[str, ProviderBehavior]:
    if not isinstance(value, dict):
        raise ConfigError("providerMetadata must be an object")
    if len(value) > MAX_DISCOVERED_PROVIDERS:
        raise ConfigError("providerMetadata has too many entries")
    result = {}
    for candidate_id, descriptor in value.items():
        _candidate_id(candidate_id, "providerMetadata key")
        if not isinstance(descriptor, dict):
            raise ConfigError(f"providerMetadata.{candidate_id} must be an object")
        result[candidate_id] = _parse_behavior(
            descriptor, f"providerMetadata.{candidate_id}"
        )
    return result


def _parse_trust(value: Any) -> Mapping[str, str]:
    if not isinstance(value, dict):
        raise ConfigError("trustedProviders must be an object")
    if len(value) > MAX_DISCOVERED_PROVIDERS:
        raise ConfigError("trustedProviders has too many entries")
    result = {}
    for candidate_id, digest in value.items():
        _candidate_id(candidate_id, "trustedProviders key")
        if not isinstance(digest, str) or not _DIGEST_RE.fullmatch(digest):
            raise ConfigError("trusted provider fingerprints must be 64 lowercase hex characters")
        result[candidate_id] = digest
    return result


def _parse_behavior(
    value: Mapping[str, Any],
    where: str,
    *,
    omit: Optional[set] = None,
) -> ProviderBehavior:
    fields = set(value)
    if omit:
        fields -= omit
    unknown = fields - _BEHAVIOR_FIELDS
    if unknown:
        raise ConfigError(f"{where} contains unknown fields: {sorted(unknown)}")
    label_value = value.get("label")
    label = None if label_value is None else _text(label_value, f"{where}.label", 128)
    network = value.get("network", "unknown")
    storage = value.get("storage", "unknown")
    setup = value.get("setup", "unknown")
    if network not in _ALLOWED_NETWORK:
        raise ConfigError(f"{where}.network has an unsupported value")
    if storage not in _ALLOWED_STORAGE:
        raise ConfigError(f"{where}.storage has an unsupported value")
    if setup not in _ALLOWED_SETUP:
        raise ConfigError(f"{where}.setup has an unsupported value")
    return ProviderBehavior(
        label=label,
        network=network,
        storage=storage,
        setup=setup,
        read_tools=_tool_list(value.get("readTools", []), f"{where}.readTools"),
        write_tools=_tool_list(value.get("writeTools", []), f"{where}.writeTools"),
    )


def _tool_list(value: Any, where: str) -> Tuple[str, ...]:
    if not isinstance(value, list) or len(value) > MAX_PROVIDER_TOOLS:
        raise ConfigError(f"{where} must be a bounded array")
    if not all(isinstance(item, str) and _TOOL_RE.fullmatch(item) for item in value):
        raise ConfigError(f"{where} contains an invalid tool name")
    if len(set(value)) != len(value):
        raise ConfigError(f"{where} contains duplicate tool names")
    return tuple(value)


def _absolute_path(value: Any, where: str) -> Path:
    text = _text(value, where, 4096)
    path = Path(text).expanduser()
    if not path.is_absolute():
        raise ConfigError(f"{where} must be an absolute path")
    return path


def _candidate_id(value: Any, where: str) -> str:
    if not isinstance(value, str) or not _CANDIDATE_ID_RE.fullmatch(value):
        raise ConfigError(f"{where} must be a directory: or entrypoint: provider id")
    return value


def _text(value: Any, where: str, maximum: int) -> str:
    if not isinstance(value, str) or not value.strip():
        raise ConfigError(f"{where} must be non-empty text")
    if len(value.encode("utf-8")) > maximum or "\x00" in value:
        raise ConfigError(f"{where} is too long or contains NUL")
    if any(ord(character) < 32 for character in value):
        raise ConfigError(f"{where} contains control characters")
    return value


def _require_keys(value: Mapping[str, Any], allowed: set, where: str) -> None:
    unknown = set(value) - allowed
    if unknown:
        raise ConfigError(f"{where} contains unknown fields: {sorted(unknown)}")
