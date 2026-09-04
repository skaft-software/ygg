"""Strict, bounded configuration loading for explicit MCP transports."""

from __future__ import annotations

import hashlib
import ipaddress
import json
import os
from dataclasses import dataclass, field
from pathlib import Path
import re
import stat
from typing import Any, Mapping, Optional, Union
from urllib.parse import urlsplit


MAX_CONFIG_BYTES = 256 * 1024
MAX_TRUSTED_PROJECTS = 8
MAX_SERVER_ID_BYTES = 32
MAX_LABEL_BYTES = 64
MAX_COMMAND_BYTES = 4096
MAX_URL_BYTES = 4096
MAX_CREDENTIAL_REFERENCE_BYTES = 64
MAX_ARGS = 64
MAX_ARGUMENT_BYTES = 16 * 1024
MAX_ENVIRONMENT_ENTRIES = 32
MAX_ENVIRONMENT_BYTES = 64 * 1024
_SERVER_ID = re.compile(r"^[a-z][a-z0-9-]{0,31}$")
_ENVIRONMENT_NAME = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")
_CREDENTIAL_REFERENCE = re.compile(r"^[A-Za-z_][A-Za-z0-9_.-]{0,63}$")
_HOSTNAME = re.compile(r"^[A-Za-z0-9](?:[A-Za-z0-9.-]{0,251}[A-Za-z0-9])?$")
_HEX_DIGEST = re.compile(r"^[0-9a-f]{64}$")
STREAMABLE_HTTP_GATE_ERROR = (
    "enabled Streamable HTTP MCP requires --experimental-streamable-http-mcp "
    "from the process owner"
)


class ConfigError(ValueError):
    """A configuration file failed a bounded trust or schema check."""


@dataclass(frozen=True)
class Limits:
    """Bridge-wide resource ceilings, each capped by a package maximum."""

    max_servers: int = 16
    max_tools_per_server: int = 64
    max_total_tools: int = 256
    max_catalog_pages: int = 8
    max_frame_bytes: int = 8 * 1024 * 1024
    max_result_bytes: int = 8 * 1024 * 1024
    max_log_entries: int = 128
    max_log_line_bytes: int = 4096
    max_pending_requests_per_server: int = 16
    max_concurrent_calls: int = 8
    startup_timeout_ms: int = 5000
    request_timeout_ms: int = 30_000
    shutdown_timeout_ms: int = 1500
    cancellation_grace_ms: int = 250
    max_restarts: int = 5
    backoff_initial_ms: int = 250
    backoff_max_ms: int = 30_000


@dataclass(frozen=True)
class HttpAuthConfig:
    """A non-secret reference resolved only by a runtime credential adapter."""

    credential: str


@dataclass(frozen=True)
class ServerConfig:
    """One explicitly trusted MCP server descriptor for one supported transport."""

    id: str
    label: str
    command: str
    args: tuple[str, ...]
    cwd: Path
    environment: Mapping[str, str] = field(repr=False)
    enabled: bool = True
    required: bool = False
    startup_timeout_ms: int = 5000
    request_timeout_ms: int = 30_000
    max_restarts: int = 5
    scope: str = "user"
    transport: str = "stdio"
    url: Optional[str] = field(default=None, repr=False)
    auth: Optional[HttpAuthConfig] = field(default=None, repr=False)


@dataclass(frozen=True)
class BridgeConfig:
    """A complete merged user and digest-pinned project configuration."""

    servers: tuple[ServerConfig, ...]
    limits: Limits = Limits()
    source: Optional[Path] = None

    @classmethod
    def empty(cls, source: Optional[Path] = None) -> "BridgeConfig":
        return cls(servers=(), source=source)


_LIMIT_FIELDS: dict[str, tuple[str, int, int]] = {
    "maxServers": ("max_servers", 1, 32),
    "maxToolsPerServer": ("max_tools_per_server", 1, 128),
    "maxTotalTools": ("max_total_tools", 1, 256),
    "maxCatalogPages": ("max_catalog_pages", 1, 32),
    "maxFrameBytes": ("max_frame_bytes", 1024, 16 * 1024 * 1024),
    "maxResultBytes": ("max_result_bytes", 1024, 16 * 1024 * 1024),
    "maxLogEntries": ("max_log_entries", 1, 1024),
    "maxLogLineBytes": ("max_log_line_bytes", 128, 16 * 1024),
    "maxPendingRequestsPerServer": ("max_pending_requests_per_server", 1, 64),
    "maxConcurrentCalls": ("max_concurrent_calls", 1, 32),
    "startupTimeoutMs": ("startup_timeout_ms", 10, 30_000),
    "requestTimeoutMs": ("request_timeout_ms", 10, 120_000),
    "shutdownTimeoutMs": ("shutdown_timeout_ms", 10, 5000),
    "cancellationGraceMs": ("cancellation_grace_ms", 1, 2000),
    "maxRestarts": ("max_restarts", 0, 8),
    "backoffInitialMs": ("backoff_initial_ms", 1, 5000),
    "backoffMaxMs": ("backoff_max_ms", 1, 60_000),
}
_SERVER_FIELDS = {
    "transport",
    "label",
    "command",
    "args",
    "cwd",
    "env",
    "url",
    "auth",
    "enabled",
    "required",
    "startupTimeoutMs",
    "requestTimeoutMs",
    "maxRestarts",
}
_AUTH_FIELDS = {"type", "credential"}


def default_config_path() -> Path:
    """Return the inert user configuration location.

    Merely installing or discovering the extension never creates this file and
    therefore never launches an MCP server.
    """

    return Path.home() / ".ygg" / "mcp.json"


def load_config(
    path: Optional[Union[os.PathLike[str], str]] = None,
    *,
    workspace: Optional[Union[os.PathLike[str], str]] = None,
    experimental_streamable_http_mcp: bool = False,
) -> BridgeConfig:
    """Load one user file and its explicitly digest-pinned project files.

    A missing default user file is a valid empty configuration. An explicitly
    supplied missing file is an error. Project files are considered trusted
    only when the user file names an absolute path beneath the active
    workspace's ``.ygg`` directory and pins its exact SHA-256 digest. Enabled
    remote Streamable HTTP descriptors are accepted only when the trusted
    extension process received the one-shot process-owner CLI opt-in; no
    configuration, environment, or session field can supply that value.
    """

    explicit = path is not None
    config_path = Path(path) if path is not None else default_config_path()
    if not config_path.exists():
        if explicit:
            raise ConfigError("the requested MCP configuration does not exist")
        return BridgeConfig.empty(config_path)

    root, root_bytes, canonical_path = _read_json_file(config_path)
    _require_keys(root, {"version", "limits", "servers", "trustedProjects"}, "config")
    _require_version(root)
    limits = _parse_limits(root.get("limits", {}))
    workspace_path = Path(workspace).resolve() if workspace is not None else None
    servers = _parse_servers(
        root.get("servers", {}),
        limits=limits,
        scope="user",
        config_dir=canonical_path.parent,
        default_cwd=workspace_path or canonical_path.parent,
    )

    projects = root.get("trustedProjects", [])
    if not isinstance(projects, list):
        raise ConfigError("trustedProjects must be an array")
    if len(projects) > MAX_TRUSTED_PROJECTS:
        raise ConfigError(f"trustedProjects exceeds the {MAX_TRUSTED_PROJECTS}-file limit")
    if projects and workspace_path is None:
        raise ConfigError("trusted project configuration requires an active workspace")

    seen_ids = {server.id for server in servers}
    for index, descriptor in enumerate(projects):
        project_path, digest = _trusted_project_descriptor(descriptor, index)
        assert workspace_path is not None
        project, project_bytes, project_file = _read_project_json_file(
            project_path, workspace_path
        )
        actual_digest = hashlib.sha256(project_bytes).hexdigest()
        if actual_digest != digest:
            raise ConfigError("a trusted project configuration digest does not match")
        _require_keys(project, {"version", "servers"}, "trusted project config")
        _require_version(project)
        project_servers = _parse_servers(
            project.get("servers", {}),
            limits=limits,
            scope="project",
            config_dir=project_file.parent,
            default_cwd=workspace_path,
        )
        duplicates = seen_ids.intersection(server.id for server in project_servers)
        if duplicates:
            raise ConfigError("server identifiers must be unique across user and project config")
        seen_ids.update(server.id for server in project_servers)
        servers.extend(project_servers)

    if (
        any(
            server.enabled and server.transport == "streamable-http"
            for server in servers
        )
        and not experimental_streamable_http_mcp
    ):
        raise ConfigError(STREAMABLE_HTTP_GATE_ERROR)
    if len(servers) > limits.max_servers:
        raise ConfigError(f"configured servers exceed the {limits.max_servers}-server limit")
    if limits.max_result_bytes > limits.max_frame_bytes:
        raise ConfigError("maxResultBytes cannot exceed maxFrameBytes")
    if limits.backoff_initial_ms > limits.backoff_max_ms:
        raise ConfigError("backoffInitialMs cannot exceed backoffMaxMs")
    # Retain this read to make it explicit that the root itself was bounded and
    # hashed before any launch descriptor was considered. It is not persisted.
    del root_bytes
    return BridgeConfig(servers=tuple(servers), limits=limits, source=canonical_path)


def _read_json_file(path: Path) -> tuple[dict[str, Any], bytes, Path]:
    try:
        canonical_parent = path.parent.resolve(strict=True)
    except OSError as error:
        raise ConfigError("cannot resolve MCP configuration directory") from error
    canonical_path = canonical_parent / path.name
    descriptor = _open_regular_descriptor(canonical_path)
    metadata = os.fstat(descriptor)
    data = _read_config_descriptor(descriptor)
    decoded = _decode_json_config(data)
    _validate_sensitive_config_permissions(metadata, decoded)
    return decoded, data, canonical_path


def _read_project_json_file(
    path: Path, workspace: Path
) -> tuple[dict[str, Any], bytes, Path]:
    """Read a project file through no-follow directory descriptors.

    Resolving ``workspace/.ygg`` before the containment check would bless a
    linked directory outside the workspace. Walking from the real workspace
    directory instead keeps containment, metadata, and bytes bound to the same
    opened objects.
    """

    project_root = workspace / ".ygg"
    absolute_path = Path(os.path.abspath(os.fspath(path)))
    try:
        canonical_parent = absolute_path.parent.resolve(strict=True)
        relative = (canonical_parent / absolute_path.name).relative_to(project_root)
    except OSError as error:
        raise ConfigError("cannot resolve trusted project configuration directory") from error
    except ValueError as error:
        raise ConfigError("a trusted project configuration is outside workspace/.ygg") from error
    if not relative.parts or any(part in {"", ".", ".."} for part in relative.parts):
        raise ConfigError("a trusted project configuration path is invalid")
    if not hasattr(os, "O_NOFOLLOW") or not hasattr(os, "O_DIRECTORY"):
        raise ConfigError("this platform cannot securely open trusted project configuration")
    if os.open not in getattr(os, "supports_dir_fd", set()):
        raise ConfigError("this platform cannot securely traverse trusted project configuration")

    directories: list[int] = []
    try:
        current = _open_directory_descriptor(project_root)
        directories.append(current)
        for component in relative.parts[:-1]:
            current = _open_directory_descriptor(component, directory_fd=current)
            directories.append(current)
        descriptor = _open_regular_descriptor(relative.parts[-1], directory_fd=current)
        metadata = os.fstat(descriptor)
        data = _read_config_descriptor(descriptor)
    finally:
        for directory in reversed(directories):
            try:
                os.close(directory)
            except OSError:
                pass
    canonical_path = project_root.joinpath(*relative.parts)
    decoded = _decode_json_config(data)
    _validate_sensitive_config_permissions(metadata, decoded)
    return decoded, data, canonical_path


def _open_directory_descriptor(
    path: Union[os.PathLike[str], str], *, directory_fd: Optional[int] = None
) -> int:
    flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW
    if hasattr(os, "O_CLOEXEC"):
        flags |= os.O_CLOEXEC
    descriptor: Optional[int] = None
    try:
        descriptor = os.open(path, flags, dir_fd=directory_fd)
        metadata = os.fstat(descriptor)
        if not stat.S_ISDIR(metadata.st_mode):
            raise ConfigError("trusted project configuration ancestors must be directories")
        return descriptor
    except ConfigError:
        if descriptor is not None:
            os.close(descriptor)
        raise
    except OSError as error:
        if descriptor is not None:
            try:
                os.close(descriptor)
            except OSError:
                pass
        raise ConfigError(
            "trusted project configuration cannot traverse links or non-directories"
        ) from error


def _open_regular_descriptor(
    path: Union[os.PathLike[str], str], *, directory_fd: Optional[int] = None
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
            raise ConfigError("cannot inspect MCP configuration") from error
        if stat.S_ISLNK(before.st_mode):
            raise ConfigError("MCP configuration must be a regular, non-symlink file")
    descriptor: Optional[int] = None
    try:
        descriptor = os.open(path, flags, dir_fd=directory_fd)
        metadata = os.fstat(descriptor)
    except OSError as error:
        if descriptor is not None:
            try:
                os.close(descriptor)
            except OSError:
                pass
        raise ConfigError("cannot open MCP configuration safely") from error
    assert descriptor is not None
    if before is not None and (before.st_dev, before.st_ino) != (
        metadata.st_dev,
        metadata.st_ino,
    ):
        os.close(descriptor)
        raise ConfigError("MCP configuration changed while it was opened")
    try:
        _validate_config_metadata(metadata)
    except ConfigError:
        os.close(descriptor)
        raise
    return descriptor


def _validate_config_metadata(metadata: os.stat_result) -> None:
    if not stat.S_ISREG(metadata.st_mode):
        raise ConfigError("MCP configuration must be a regular, non-symlink file")
    if metadata.st_size > MAX_CONFIG_BYTES:
        raise ConfigError(f"MCP configuration exceeds the {MAX_CONFIG_BYTES}-byte limit")
    if hasattr(os, "getuid") and metadata.st_uid != os.getuid():
        raise ConfigError("MCP configuration must be owned by the current user")
    if metadata.st_mode & (stat.S_IWGRP | stat.S_IWOTH):
        raise ConfigError("MCP configuration cannot be group- or world-writable")


def _validate_sensitive_config_permissions(
    metadata: os.stat_result, value: Mapping[str, Any]
) -> None:
    if not hasattr(os, "getuid"):
        return
    servers = value.get("servers")
    if not isinstance(servers, Mapping):
        return
    has_explicit_environment = any(
        isinstance(descriptor, Mapping)
        and isinstance(descriptor.get("env"), Mapping)
        and bool(descriptor["env"])
        for descriptor in servers.values()
    )
    if has_explicit_environment and metadata.st_mode & (stat.S_IRWXG | stat.S_IRWXO):
        raise ConfigError(
            "MCP configuration with explicit environment values must not be accessible "
            "by group or other users"
        )


def _read_config_descriptor(descriptor: int) -> bytes:
    chunks: list[bytes] = []
    remaining = MAX_CONFIG_BYTES + 1
    try:
        while remaining:
            chunk = os.read(descriptor, min(16 * 1024, remaining))
            if not chunk:
                break
            chunks.append(chunk)
            remaining -= len(chunk)
    except OSError as error:
        raise ConfigError("cannot read MCP configuration") from error
    finally:
        os.close(descriptor)
    data = b"".join(chunks)
    if len(data) > MAX_CONFIG_BYTES:
        raise ConfigError(f"MCP configuration exceeds the {MAX_CONFIG_BYTES}-byte limit")
    return data


def _decode_json_config(data: bytes) -> dict[str, Any]:
    def unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
        value: dict[str, Any] = {}
        for key, item in pairs:
            if key in value:
                raise ConfigError("MCP configuration contains a duplicate object key")
            value[key] = item
        return value

    try:
        decoded = data.decode("utf-8")
        value = json.loads(decoded, object_pairs_hook=unique_object)
    except ConfigError:
        raise
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ConfigError("MCP configuration is not valid strict UTF-8 JSON") from error
    if not isinstance(value, dict):
        raise ConfigError("MCP configuration root must be an object")
    return value


def _require_version(value: Mapping[str, Any]) -> None:
    if value.get("version") != 1:
        raise ConfigError("MCP configuration version must be 1")


def _parse_limits(value: Any) -> Limits:
    if not isinstance(value, dict):
        raise ConfigError("limits must be an object")
    _require_keys(value, set(_LIMIT_FIELDS), "limits")
    parsed = Limits().__dict__.copy()
    for public_name, item in value.items():
        field_name, minimum, maximum = _LIMIT_FIELDS[public_name]
        parsed[field_name] = _bounded_integer(item, public_name, minimum, maximum)
    return Limits(**parsed)


def _parse_servers(
    value: Any,
    *,
    limits: Limits,
    scope: str,
    config_dir: Path,
    default_cwd: Path,
) -> list[ServerConfig]:
    if not isinstance(value, dict):
        raise ConfigError("servers must be an object")
    parsed: list[ServerConfig] = []
    for server_id, descriptor in value.items():
        if not isinstance(server_id, str) or not _SERVER_ID.fullmatch(server_id):
            raise ConfigError("server identifiers must match [a-z][a-z0-9-]{0,31}")
        if len(server_id.encode("utf-8")) > MAX_SERVER_ID_BYTES:
            raise ConfigError("server identifier is too long")
        if not isinstance(descriptor, dict):
            raise ConfigError(f"server {server_id} must be an object")
        _require_keys(descriptor, _SERVER_FIELDS, f"server {server_id}")
        transport = descriptor.get("transport", "stdio")
        if not isinstance(transport, str) or transport not in {"stdio", "streamable-http"}:
            raise ConfigError(
                f"server {server_id} transport must be stdio or streamable-http"
            )
        label = _bounded_text(
            descriptor.get("label", server_id),
            f"server {server_id} label",
            MAX_LABEL_BYTES,
        )
        enabled = _boolean(descriptor.get("enabled", True), f"server {server_id} enabled")
        required = _boolean(descriptor.get("required", False), f"server {server_id} required")
        startup = _optional_bounded_integer(
            descriptor,
            "startupTimeoutMs",
            limits.startup_timeout_ms,
            10,
            30_000,
        )
        request = _optional_bounded_integer(
            descriptor,
            "requestTimeoutMs",
            limits.request_timeout_ms,
            10,
            120_000,
        )
        restarts = _optional_bounded_integer(
            descriptor,
            "maxRestarts",
            limits.max_restarts,
            0,
            8,
        )

        if transport == "stdio":
            _reject_transport_fields(descriptor, server_id, {"url", "auth"}, "stdio")
            command = _bounded_text(
                descriptor.get("command"),
                f"server {server_id} command",
                MAX_COMMAND_BYTES,
            )
            args_value = descriptor.get("args", [])
            if not isinstance(args_value, list) or not all(
                isinstance(item, str) for item in args_value
            ):
                raise ConfigError(f"server {server_id} args must be an array of strings")
            if len(args_value) > MAX_ARGS:
                raise ConfigError(f"server {server_id} args exceed the {MAX_ARGS}-argument limit")
            args = tuple(
                _bounded_text(
                    item,
                    f"server {server_id} argument",
                    MAX_ARGUMENT_BYTES,
                    allow_empty=True,
                )
                for item in args_value
            )
            environment = _parse_environment(descriptor.get("env", {}), server_id)
            cwd_value = descriptor.get("cwd")
            if cwd_value is None:
                cwd = default_cwd
            else:
                cwd_text = _bounded_text(
                    cwd_value, f"server {server_id} cwd", MAX_COMMAND_BYTES
                )
                candidate = Path(cwd_text)
                cwd = (candidate if candidate.is_absolute() else config_dir / candidate).resolve()
            url = None
            auth = None
        else:
            _reject_transport_fields(
                descriptor,
                server_id,
                {"command", "args", "cwd", "env"},
                "streamable-http",
            )
            command = ""
            args = ()
            cwd = default_cwd
            environment = {}
            url = _parse_streamable_http_url(descriptor.get("url"), server_id)
            auth = (
                _parse_http_auth(descriptor["auth"], server_id)
                if "auth" in descriptor
                else None
            )

        parsed.append(
            ServerConfig(
                id=server_id,
                label=label,
                command=command,
                args=args,
                cwd=cwd,
                environment=environment,
                enabled=enabled,
                required=required,
                startup_timeout_ms=startup,
                request_timeout_ms=request,
                max_restarts=restarts,
                scope=scope,
                transport=transport,
                url=url,
                auth=auth,
            )
        )
    return parsed


def _reject_transport_fields(
    descriptor: Mapping[str, Any], server_id: str, fields: set[str], transport: str
) -> None:
    present = sorted(fields.intersection(descriptor))
    if present:
        joined = ", ".join(present)
        raise ConfigError(
            f"server {server_id} {transport} transport does not allow {joined}"
        )


def _parse_streamable_http_url(value: Any, server_id: str) -> str:
    label = f"server {server_id} url"
    url = _bounded_text(value, label, MAX_URL_BYTES)
    if any(character.isspace() for character in url):
        raise ConfigError(f"{label} must not contain whitespace")
    try:
        parts = urlsplit(url)
        port = parts.port
    except ValueError as error:
        raise ConfigError(f"{label} is not a valid absolute HTTP(S) URL") from error
    if parts.scheme not in {"http", "https"} or not parts.netloc or not parts.hostname:
        raise ConfigError(f"{label} must be an absolute http or https URL")
    if parts.username is not None or parts.password is not None:
        raise ConfigError(f"{label} must not contain credentials")
    if "?" in url or "#" in url or parts.query or parts.fragment:
        raise ConfigError(f"{label} must not contain a query or fragment")
    if port is not None and not 1 <= port <= 65535:
        raise ConfigError(f"{label} has an invalid port")
    host = parts.hostname
    assert host is not None
    try:
        literal = ipaddress.ip_address(host)
    except ValueError:
        if not host.isascii() or not _HOSTNAME.fullmatch(host):
            raise ConfigError(f"{label} has an invalid host")
        loopback = False
    else:
        if literal.is_unspecified or literal.is_multicast:
            raise ConfigError(f"{label} must not target an unspecified or multicast address")
        loopback = literal.is_loopback
    if parts.scheme == "http" and not loopback:
        raise ConfigError(
            f"{label} must use https; cleartext http is limited to a numeric loopback address"
        )
    return url


def _parse_http_auth(value: Any, server_id: str) -> HttpAuthConfig:
    if not isinstance(value, dict):
        raise ConfigError(f"server {server_id} auth must be an object")
    _require_keys(value, _AUTH_FIELDS, f"server {server_id} auth")
    if value.get("type") != "bearer":
        raise ConfigError(f"server {server_id} auth type must be bearer")
    credential = _bounded_text(
        value.get("credential"),
        f"server {server_id} auth credential",
        MAX_CREDENTIAL_REFERENCE_BYTES,
    )
    if not _CREDENTIAL_REFERENCE.fullmatch(credential):
        raise ConfigError(
            f"server {server_id} auth credential must be a bounded logical reference"
        )
    return HttpAuthConfig(credential=credential)


def _parse_environment(value: Any, server_id: str) -> dict[str, str]:
    if not isinstance(value, dict):
        raise ConfigError(f"server {server_id} env must be an object")
    if len(value) > MAX_ENVIRONMENT_ENTRIES:
        raise ConfigError(
            f"server {server_id} env exceeds the {MAX_ENVIRONMENT_ENTRIES}-entry limit"
        )
    result: dict[str, str] = {}
    total = 0
    for name, secret_value in value.items():
        if not isinstance(name, str) or not _ENVIRONMENT_NAME.fullmatch(name):
            raise ConfigError(f"server {server_id} has an invalid environment name")
        if not isinstance(secret_value, str) or "\x00" in secret_value:
            raise ConfigError(f"server {server_id} environment values must be strings without NUL")
        total += len(name.encode("utf-8")) + len(secret_value.encode("utf-8"))
        if total > MAX_ENVIRONMENT_BYTES:
            raise ConfigError(
                f"server {server_id} env exceeds the {MAX_ENVIRONMENT_BYTES}-byte limit"
            )
        result[name] = secret_value
    return result


def _trusted_project_descriptor(value: Any, index: int) -> tuple[Path, str]:
    if not isinstance(value, dict):
        raise ConfigError(f"trustedProjects[{index}] must be an object")
    _require_keys(value, {"path", "sha256"}, f"trustedProjects[{index}]")
    path_value = _bounded_text(value.get("path"), "trusted project path", MAX_COMMAND_BYTES)
    project_path = Path(path_value)
    if not project_path.is_absolute():
        raise ConfigError("trusted project configuration paths must be absolute")
    digest = value.get("sha256")
    if not isinstance(digest, str) or not _HEX_DIGEST.fullmatch(digest):
        raise ConfigError("trusted project sha256 must be 64 lowercase hexadecimal characters")
    return project_path, digest


def _require_keys(value: Mapping[str, Any], allowed: set[str], label: str) -> None:
    unknown = set(value) - allowed
    if unknown:
        raise ConfigError(f"{label} contains unknown fields")


def _boolean(value: Any, label: str) -> bool:
    if not isinstance(value, bool):
        raise ConfigError(f"{label} must be a boolean")
    return value


def _bounded_integer(value: Any, label: str, minimum: int, maximum: int) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        raise ConfigError(f"{label} must be an integer")
    if not minimum <= value <= maximum:
        raise ConfigError(f"{label} must be between {minimum} and {maximum}")
    return value


def _optional_bounded_integer(
    value: Mapping[str, Any],
    key: str,
    default: int,
    minimum: int,
    maximum: int,
) -> int:
    if key not in value:
        return default
    return _bounded_integer(value[key], key, minimum, maximum)


def _bounded_text(value: Any, label: str, maximum: int, *, allow_empty: bool = False) -> str:
    if not isinstance(value, str):
        raise ConfigError(f"{label} must be a string")
    if not allow_empty and not value.strip():
        raise ConfigError(f"{label} must be non-empty")
    if "\x00" in value or any(
        ord(character) < 32 or 127 <= ord(character) <= 159 for character in value
    ):
        raise ConfigError(f"{label} contains a control character")
    if len(value.encode("utf-8")) > maximum:
        raise ConfigError(f"{label} exceeds the {maximum}-byte limit")
    return value
