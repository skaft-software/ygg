"""Metadata-only Hermes directory and entry-point discovery."""

from __future__ import annotations

from dataclasses import dataclass, field
import hashlib
import importlib.metadata as importlib_metadata
import os
from pathlib import Path
import re
import stat
from typing import Any, List, Mapping, Optional, Sequence, Tuple

from .config import BridgeConfig, ProviderBehavior, current_environment_matches
from .constants import (
    HERMES_CONTRACT_ID,
    HERMES_CONTRACT_VERSION,
    HERMES_ENTRY_POINT_GROUP,
    MAX_PROVIDER_CODE_BYTES,
    MAX_PROVIDER_CODE_FILES,
    MAX_PROVIDER_METADATA_BYTES,
)
from .safety import safe_detail, safe_identifier, safe_label


_ENTRY_NAME_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_.-]{0,63}$")
_SIMPLE_YAML_SCALAR = re.compile(r"^([A-Za-z_][A-Za-z0-9_-]*):\s*(.*?)\s*$")


@dataclass(frozen=True)
class ProviderCandidate:
    """Safe metadata plus private loader coordinates for one provider."""

    id: str
    name: str
    label: str
    version: str
    source: str
    fingerprint: Optional[str]
    environment_id: str
    environment_version: Optional[str]
    network: str
    storage: str
    setup: str
    read_tools: Tuple[str, ...]
    write_tools: Tuple[str, ...]
    declared_hooks: Tuple[str, ...]
    availability: str
    reason_code: Optional[str]
    contract_id: str = HERMES_CONTRACT_ID
    path: Optional[Path] = field(default=None, repr=False, compare=False)
    entry_point: Any = field(default=None, repr=False, compare=False)
    entry_point_value: Optional[str] = field(default=None, repr=False, compare=False)
    distribution_name: Optional[str] = field(default=None, repr=False, compare=False)
    distribution_version: Optional[str] = field(default=None, repr=False, compare=False)

    def trusted_by(self, config: BridgeConfig, runtime_trust: Mapping[str, str]) -> bool:
        expected = runtime_trust.get(self.id) or config.trusted_fingerprint(self.id)
        return self.fingerprint is not None and expected == self.fingerprint

    def safe_metadata(self, *, trusted: bool) -> Mapping[str, Any]:
        return {
            "id": self.id,
            "name": self.name,
            "label": self.label,
            "version": self.version,
            "source": self.source,
            "fingerprint": self.fingerprint,
            "contract": self.contract_id,
            "environment": self.environment_id,
            "environmentVersion": self.environment_version,
            "network": self.network,
            "storage": self.storage,
            "setup": self.setup,
            "readTools": list(self.read_tools),
            "writeTools": list(self.write_tools),
            "declaredHooks": list(self.declared_hooks),
            "availability": self.availability,
            "reasonCode": self.reason_code,
            "trusted": trusted,
        }


@dataclass(frozen=True)
class DiscoverySnapshot:
    candidates: Tuple[ProviderCandidate, ...]
    environment_id: str
    environment_version: Optional[str]
    environment_state: str
    reason_code: Optional[str]

    def by_id(self, candidate_id: str) -> Optional[ProviderCandidate]:
        for candidate in self.candidates:
            if candidate.id == candidate_id:
                return candidate
        return None


def discover_providers(
    config: BridgeConfig,
    *,
    metadata_module: Any = importlib_metadata,
) -> DiscoverySnapshot:
    """Enumerate configured directories and distribution entry-point metadata.

    No provider module is imported and no provider callback, constructor, or
    ``is_available`` method is called. Directory code is read only to derive the
    exact trust fingerprint; it is never parsed or executed.
    """

    environment = config.environment
    if environment is None:
        return DiscoverySnapshot((), "not-configured", None, "off", "environment_not_configured")

    environment_state = "compatible"
    reason_code = None
    if not current_environment_matches(environment):
        environment_state = "unavailable"
        reason_code = "python_environment_mismatch"

    installed_version: Optional[str]
    try:
        installed_version = str(metadata_module.version("hermes-agent"))
    except Exception:
        installed_version = None
    if installed_version != HERMES_CONTRACT_VERSION:
        environment_state = "unavailable"
        reason_code = "hermes_contract_version_mismatch"

    candidates: List[ProviderCandidate] = []
    names_seen = set()
    for descriptor in config.directories:
        behavior = config.provider_metadata.get(descriptor.candidate_id, descriptor.behavior)
        candidate = _directory_candidate(
            descriptor.candidate_id,
            descriptor.path,
            descriptor.id,
            behavior,
            environment_id=environment.id,
            environment_version=installed_version,
            environment_state=environment_state,
            environment_reason=reason_code,
        )
        candidates.append(candidate)
        names_seen.add(candidate.name)
        if len(candidates) >= config.limits.max_providers:
            break

    if environment.include_entry_points and len(candidates) < config.limits.max_providers:
        for entry_point in _entry_points(metadata_module):
            if len(candidates) >= config.limits.max_providers:
                break
            raw_name = getattr(entry_point, "name", None)
            if not isinstance(raw_name, str) or not _ENTRY_NAME_RE.fullmatch(raw_name):
                continue
            candidate_id = f"entrypoint:{raw_name}"
            # Hermes's first-seen directory precedence remains explicit. The
            # shadowed entry point is still metadata, but selecting it by name
            # would violate the upstream rule, so do not offer it.
            if raw_name in names_seen:
                continue
            behavior = config.provider_metadata.get(candidate_id, ProviderBehavior())
            candidate = _entry_point_candidate(
                candidate_id,
                entry_point,
                behavior,
                environment_id=environment.id,
                environment_version=installed_version,
                environment_state=environment_state,
                environment_reason=reason_code,
            )
            candidates.append(candidate)
            names_seen.add(candidate.name)

    candidates.sort(key=lambda item: (item.label.lower(), item.id))
    return DiscoverySnapshot(
        tuple(candidates),
        environment.id,
        installed_version,
        environment_state,
        reason_code,
    )


def directory_snapshot(
    candidate_id: str, path: Path, environment_id: str
) -> Tuple[str, Mapping[str, bytes]]:
    """Snapshot and hash provider files through no-follow descriptors."""

    if not hasattr(os, "O_NOFOLLOW") or not hasattr(os, "O_DIRECTORY"):
        raise ValueError("this platform cannot securely snapshot provider code")
    try:
        root_meta = path.lstat()
    except OSError as error:
        raise ValueError("provider directory is unavailable") from error
    if stat.S_ISLNK(root_meta.st_mode) or not stat.S_ISDIR(root_meta.st_mode):
        raise ValueError("provider directory must be a non-symlink directory")
    root = path.resolve(strict=True)
    directory_flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW
    file_flags = os.O_RDONLY | os.O_NOFOLLOW
    if hasattr(os, "O_CLOEXEC"):
        directory_flags |= os.O_CLOEXEC
        file_flags |= os.O_CLOEXEC

    files = {}
    examined = 0
    total = 0

    def read_file(directory_fd: int, name: str, metadata: os.stat_result) -> bytes:
        nonlocal total
        descriptor = None
        try:
            descriptor = os.open(name, file_flags, dir_fd=directory_fd)
            opened = os.fstat(descriptor)
            if (opened.st_dev, opened.st_ino) != (metadata.st_dev, metadata.st_ino):
                raise ValueError("provider file changed while it was opened")
            if not stat.S_ISREG(opened.st_mode) or getattr(opened, "st_nlink", 1) != 1:
                raise ValueError("provider directory contains linked or special files")
            if opened.st_size > MAX_PROVIDER_CODE_BYTES - total:
                raise ValueError("provider code exceeds the byte limit")
            chunks = []
            remaining = opened.st_size
            while remaining:
                chunk = os.read(descriptor, min(64 * 1024, remaining))
                if not chunk:
                    raise ValueError("provider file ended while it was snapshotted")
                chunks.append(chunk)
                remaining -= len(chunk)
            if os.read(descriptor, 1):
                raise ValueError("provider file grew while it was snapshotted")
            data = b"".join(chunks)
            total += len(data)
            return data
        except OSError as error:
            raise ValueError("provider code cannot be read safely") from error
        finally:
            if descriptor is not None:
                os.close(descriptor)

    def walk(directory_fd: int, relative: Path) -> None:
        nonlocal examined
        try:
            names = sorted(os.listdir(directory_fd))
        except OSError as error:
            raise ValueError("provider directory cannot be enumerated safely") from error
        examined += len(names)
        if examined > 4096:
            raise ValueError("provider directory contains too many entries")
        for name in names:
            if not isinstance(name, str) or name in {"", ".", ".."}:
                raise ValueError("provider directory contains an invalid name")
            try:
                metadata = os.stat(name, dir_fd=directory_fd, follow_symlinks=False)
            except OSError as error:
                raise ValueError("provider directory cannot be inspected") from error
            child_relative = relative / name
            if stat.S_ISLNK(metadata.st_mode):
                raise ValueError("provider directory cannot contain symlinks")
            if stat.S_ISDIR(metadata.st_mode):
                if name == "__pycache__":
                    continue
                child_fd = None
                try:
                    child_fd = os.open(name, directory_flags, dir_fd=directory_fd)
                    opened = os.fstat(child_fd)
                    if (opened.st_dev, opened.st_ino) != (metadata.st_dev, metadata.st_ino):
                        raise ValueError("provider directory changed while it was opened")
                    walk(child_fd, child_relative)
                except OSError as error:
                    raise ValueError("provider directory cannot be traversed safely") from error
                finally:
                    if child_fd is not None:
                        os.close(child_fd)
                continue
            if not stat.S_ISREG(metadata.st_mode):
                raise ValueError("provider directory contains a special file")
            if child_relative.suffix in {".pyc", ".pyo"}:
                continue
            key = child_relative.as_posix()
            key.encode("utf-8", errors="strict")
            files[key] = read_file(directory_fd, name, metadata)
            if len(files) > MAX_PROVIDER_CODE_FILES:
                raise ValueError("provider code exceeds the file-count limit")

    root_fd = None
    try:
        root_fd = os.open(root, directory_flags)
        walk(root_fd, Path())
    except OSError as error:
        raise ValueError("provider directory cannot be opened safely") from error
    finally:
        if root_fd is not None:
            os.close(root_fd)
    if "__init__.py" not in files:
        raise ValueError("provider directory has no __init__.py")

    digest = hashlib.sha256()
    digest.update(b"ygg-hermes-memory-directory-v2\0")
    digest.update(HERMES_CONTRACT_ID.encode("ascii") + b"\0")
    digest.update(environment_id.encode("utf-8") + b"\0")
    digest.update(candidate_id.encode("utf-8") + b"\0")
    digest.update(str(root).encode("utf-8") + b"\0")
    for relative in sorted(files):
        relative_bytes = relative.encode("utf-8")
        data = files[relative]
        digest.update(len(relative_bytes).to_bytes(4, "big") + relative_bytes)
        digest.update(len(data).to_bytes(8, "big") + data)
    return digest.hexdigest(), files


def directory_fingerprint(candidate_id: str, path: Path, environment_id: str) -> str:
    """Hash a private immutable snapshot without importing provider code."""

    fingerprint, _ = directory_snapshot(candidate_id, path, environment_id)
    return fingerprint


def entry_point_snapshot(
    entry_point: Any, environment_id: str
) -> Tuple[str, Mapping[str, bytes]]:
    """Snapshot and hash the selected entry-point distribution."""

    name = str(getattr(entry_point, "name", ""))
    value = str(getattr(entry_point, "value", ""))
    group = str(getattr(entry_point, "group", HERMES_ENTRY_POINT_GROUP))
    dist_name, dist_version = _distribution_identity(entry_point)
    distribution = getattr(entry_point, "dist", None)
    if distribution is None:
        raise ValueError("entry-point provider has no owning distribution")
    try:
        selected = sorted(list(getattr(distribution, "files", None) or []), key=str)
    except Exception as error:
        raise ValueError("entry-point distribution files cannot be enumerated") from error
    if not selected or len(selected) > MAX_PROVIDER_CODE_FILES:
        raise ValueError("entry-point provider code exceeds the file-count limit")

    flags = os.O_RDONLY
    if hasattr(os, "O_CLOEXEC"):
        flags |= os.O_CLOEXEC
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    else:
        raise ValueError("this platform cannot securely snapshot entry-point code")
    files = {}
    total = 0
    for relative_value in selected:
        relative = Path(str(relative_value))
        if relative.is_absolute() or not relative.parts or any(
            part in {"", ".", ".."} for part in relative.parts
        ):
            raise ValueError("entry-point provider contains an invalid file path")
        if relative.suffix in {".pyc", ".pyo"} or "__pycache__" in relative.parts:
            continue
        try:
            path = Path(distribution.locate_file(relative_value))
            before = path.lstat()
        except Exception as error:
            raise ValueError("entry-point provider file cannot be inspected") from error
        if stat.S_ISLNK(before.st_mode) or not stat.S_ISREG(before.st_mode):
            raise ValueError("entry-point provider contains a linked or special file")
        descriptor = None
        try:
            descriptor = os.open(path, flags)
            opened = os.fstat(descriptor)
            if (opened.st_dev, opened.st_ino) != (before.st_dev, before.st_ino):
                raise ValueError("entry-point provider changed while it was opened")
            if getattr(opened, "st_nlink", 1) != 1:
                raise ValueError("entry-point provider contains hard-linked code")
            if opened.st_size > MAX_PROVIDER_CODE_BYTES - total:
                raise ValueError("entry-point provider code exceeds the byte limit")
            chunks = []
            remaining = opened.st_size
            while remaining:
                chunk = os.read(descriptor, min(64 * 1024, remaining))
                if not chunk:
                    raise ValueError("entry-point provider file ended during snapshot")
                chunks.append(chunk)
                remaining -= len(chunk)
            if os.read(descriptor, 1):
                raise ValueError("entry-point provider file grew during snapshot")
            data = b"".join(chunks)
        except OSError as error:
            raise ValueError("entry-point provider file cannot be read safely") from error
        finally:
            if descriptor is not None:
                os.close(descriptor)
        key = relative.as_posix()
        key.encode("utf-8", errors="strict")
        files[key] = data
        total += len(data)
    if not files:
        raise ValueError("entry-point provider distribution has no snapshot files")

    digest = hashlib.sha256()
    for item in (
        "ygg-hermes-memory-entrypoint-v2",
        HERMES_CONTRACT_ID,
        environment_id,
        group,
        name,
        value,
        dist_name or "unknown-distribution",
        dist_version or "unknown-version",
    ):
        digest.update(item.encode("utf-8", errors="replace") + b"\0")
    for relative in sorted(files):
        relative_bytes = relative.encode("utf-8")
        data = files[relative]
        digest.update(len(relative_bytes).to_bytes(4, "big") + relative_bytes)
        digest.update(len(data).to_bytes(8, "big") + data)
    return digest.hexdigest(), files


def entry_point_fingerprint(entry_point: Any, environment_id: str) -> str:
    """Bind trust to an immutable entry-point distribution snapshot."""

    fingerprint, _ = entry_point_snapshot(entry_point, environment_id)
    return fingerprint


def _directory_candidate(
    candidate_id: str,
    path: Path,
    configured_name: str,
    behavior: ProviderBehavior,
    *,
    environment_id: str,
    environment_version: Optional[str],
    environment_state: str,
    environment_reason: Optional[str],
) -> ProviderCandidate:
    metadata = {}
    fingerprint = None
    availability = "discoverable"
    reason = None
    try:
        metadata = _read_plugin_yaml(path / "plugin.yaml")
        fingerprint = directory_fingerprint(candidate_id, path, environment_id)
    except Exception:
        availability = "unavailable"
        reason = "directory_metadata_invalid"
    if environment_state != "compatible":
        availability = "unavailable"
        reason = environment_reason
    raw_name = metadata.get("name", configured_name)
    name = safe_identifier(raw_name, fallback=configured_name, maximum=64)
    if not _ENTRY_NAME_RE.fullmatch(name):
        name = configured_name
    label = safe_label(behavior.label or raw_name, fallback=configured_name, maximum=128)
    version = safe_label(metadata.get("version", "unknown"), fallback="unknown", maximum=64)
    hooks_value = metadata.get("hooks", ())
    hooks = tuple(
        safe_identifier(item, fallback="hook", maximum=64)
        for item in hooks_value
        if isinstance(item, str)
    )[:32]
    return ProviderCandidate(
        id=candidate_id,
        name=name,
        label=label,
        version=version,
        source="directory",
        fingerprint=fingerprint,
        environment_id=environment_id,
        environment_version=environment_version,
        network=behavior.network,
        storage=behavior.storage,
        setup=behavior.setup,
        read_tools=behavior.read_tools,
        write_tools=behavior.write_tools,
        declared_hooks=hooks,
        availability=availability,
        reason_code=reason,
        path=path.resolve(strict=False),
    )


def _entry_point_candidate(
    candidate_id: str,
    entry_point: Any,
    behavior: ProviderBehavior,
    *,
    environment_id: str,
    environment_version: Optional[str],
    environment_state: str,
    environment_reason: Optional[str],
) -> ProviderCandidate:
    raw_name = str(getattr(entry_point, "name", "provider"))
    raw_value = str(getattr(entry_point, "value", ""))
    dist_name, dist_version = _distribution_identity(entry_point)
    try:
        fingerprint = entry_point_fingerprint(entry_point, environment_id)
    except Exception:
        fingerprint = None
    availability = "discoverable" if raw_value and fingerprint is not None else "unavailable"
    reason = None if availability == "discoverable" else "entry_point_metadata_invalid"
    if environment_state != "compatible":
        availability = "unavailable"
        reason = environment_reason
    return ProviderCandidate(
        id=candidate_id,
        name=raw_name,
        label=safe_label(behavior.label or raw_name, fallback="provider", maximum=128),
        version=safe_label(dist_version or "unknown", fallback="unknown", maximum=64),
        source="entrypoint",
        fingerprint=fingerprint,
        environment_id=environment_id,
        environment_version=environment_version,
        network=behavior.network,
        storage=behavior.storage,
        setup=behavior.setup,
        read_tools=behavior.read_tools,
        write_tools=behavior.write_tools,
        declared_hooks=(),
        availability=availability,
        reason_code=reason,
        entry_point=entry_point,
        entry_point_value=raw_value,
        distribution_name=dist_name,
        distribution_version=dist_version,
    )


def _entry_points(metadata_module: Any) -> Sequence[Any]:
    try:
        entry_points = metadata_module.entry_points()
        if hasattr(entry_points, "select"):
            selected = list(entry_points.select(group=HERMES_ENTRY_POINT_GROUP))
        elif isinstance(entry_points, Mapping):
            selected = list(entry_points.get(HERMES_ENTRY_POINT_GROUP, []))
        else:
            selected = [
                item
                for item in entry_points
                if getattr(item, "group", None) == HERMES_ENTRY_POINT_GROUP
            ]
    except Exception:
        return []
    return sorted(
        selected,
        key=lambda item: (
            str(getattr(item, "name", "")),
            str(getattr(item, "value", "")),
            _distribution_identity(item),
        ),
    )


def _distribution_identity(entry_point: Any) -> Tuple[Optional[str], Optional[str]]:
    distribution = getattr(entry_point, "dist", None)
    if distribution is None:
        return None, None
    name = None
    version = None
    try:
        metadata = getattr(distribution, "metadata", None)
        if metadata is not None:
            name = metadata.get("Name")
    except Exception:
        name = None
    try:
        version = getattr(distribution, "version", None)
    except Exception:
        version = None
    return (
        safe_label(name, fallback="unknown", maximum=128) if name else None,
        safe_label(version, fallback="unknown", maximum=64) if version else None,
    )


def _read_plugin_yaml(path: Path) -> Mapping[str, Any]:
    """Read the tiny metadata subset without importing PyYAML or provider code."""

    if not path.exists():
        return {}
    try:
        metadata = path.lstat()
    except OSError as error:
        raise ValueError("plugin metadata cannot be inspected") from error
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISREG(metadata.st_mode):
        raise ValueError("plugin metadata must be a regular non-symlink file")
    if metadata.st_size > MAX_PROVIDER_METADATA_BYTES:
        raise ValueError("plugin metadata exceeds the byte limit")
    try:
        raw = path.read_bytes()
        text = raw.decode("utf-8-sig")
    except (OSError, UnicodeDecodeError) as error:
        raise ValueError("plugin metadata is not strict UTF-8") from error
    result: dict = {}
    hooks: List[str] = []
    in_hooks = False
    for original in text.splitlines()[:512]:
        line = original.rstrip()
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        if in_hooks and (line.startswith(" ") or line.startswith("\t")):
            if stripped.startswith("-"):
                item = _yaml_scalar(stripped[1:].strip())
                if item:
                    hooks.append(item)
            continue
        in_hooks = False
        match = _SIMPLE_YAML_SCALAR.match(stripped)
        if not match:
            continue
        key, raw_value = match.groups()
        if key == "hooks":
            in_hooks = True
            if raw_value.startswith("[") and raw_value.endswith("]"):
                for item in raw_value[1:-1].split(","):
                    value = _yaml_scalar(item.strip())
                    if value:
                        hooks.append(value)
            continue
        if key in {"name", "version", "description"}:
            result[key] = _yaml_scalar(raw_value)
    if hooks:
        result["hooks"] = tuple(hooks[:32])
    return result


def _yaml_scalar(value: str) -> str:
    value = value.strip()
    if len(value.encode("utf-8", errors="replace")) > 4096:
        value = value.encode("utf-8", errors="replace")[:4096].decode("utf-8", errors="ignore")
    if len(value) >= 2 and value[0] == value[-1] and value[0] in {"'", '"'}:
        value = value[1:-1]
    return safe_detail(value, maximum=4096) if value else ""
