"""Bounded provider-backed search and public-web retrieval for ygg-web-search.

This module intentionally uses only the Python standard library.  It owns the
provider adapters, URL and destination validation, bounded HTTP transport,
normalization, citations, credential persistence, and an in-memory cache.
Retrieved bytes are always untrusted data; callers add the model-visible trust
frame.
"""

from __future__ import annotations

import codecs
import copy
import hashlib
import html
from html.parser import HTMLParser
import http.client
import ipaddress
import json
import math
import os
from pathlib import Path
import re
import secrets
import socket
import ssl
import stat
import threading
import time
import unicodedata
from collections import OrderedDict
from dataclasses import dataclass
from typing import Any, Callable, Dict, List, Mapping, Optional, Sequence, Tuple
from urllib.parse import parse_qsl, quote, urlencode, urljoin, urlsplit, urlunsplit


CONFIG_VERSION = 1
MAX_CONFIG_BYTES = 64 * 1024
MAX_URL_BYTES = 2048
MAX_QUERY_BYTES = 512
MAX_PATTERN_BYTES = 256
MAX_DOMAINS = 5
MAX_CONFIG_DOMAINS = 32
MAX_RESULTS = 10
MAX_FIND_MATCHES = 20
MAX_REDIRECTS = 3
MAX_TIMEOUT_SECONDS = 20.0
MIN_TIMEOUT_SECONDS = 0.1
MAX_PROVIDER_BYTES = 512 * 1024
MAX_DOWNLOAD_BYTES = 512 * 1024
MAX_CONTENT_BYTES = 128 * 1024
MAX_SNIPPET_BYTES = 2 * 1024
MAX_TITLE_BYTES = 512
MAX_PUBLICATION_BYTES = 128
MAX_LABEL_BYTES = 48
MAX_CACHE_ENTRIES = 64
MAX_CACHE_BYTES = 2 * 1024 * 1024
MAX_CACHE_TTL_SECONDS = 15 * 60
MAX_BRAVE_API_KEY_BYTES = 1024
BRAVE_SEARCH_ENDPOINT = "https://api.search.brave.com/res/v1/web/search"
BRAVE_SEARCH_KEY_URL = "https://api.search.brave.com/app/keys"
OPEN_PORTS = frozenset((80, 443))
USER_AGENT = "ygg-web-search/0.2 (+https://github.com/skaft-software/ygg)"
TRACKING_QUERY_NAMES = frozenset(
    (
        "fbclid",
        "gclid",
        "dclid",
        "msclkid",
        "mc_cid",
        "mc_eid",
        "ref_src",
    )
)
REDIRECT_STATUSES = frozenset((301, 302, 303, 307, 308))
HTML_TYPES = frozenset(("text/html", "application/xhtml+xml"))
PLAIN_TYPES = frozenset(("text/plain",))
SEARCH_TYPES = frozenset(("application/json", "text/json"))


class WebError(Exception):
    """A bounded, user-safe web operation failure."""

    outcome = "failed"

    def __init__(self, message: str) -> None:
        super().__init__(message)
        self.safe_message = message


class InvalidInput(WebError):
    outcome = "invalid_input"


class ConfigError(WebError):
    outcome = "unconfigured"


class Disabled(ConfigError):
    outcome = "disabled"


class CredentialRequired(ConfigError):
    outcome = "unconfigured"


class DestinationRejected(WebError):
    outcome = "blocked"


class RedirectRejected(DestinationRejected):
    pass


class RequestTimedOut(WebError):
    outcome = "timed_out"


class Offline(WebError):
    outcome = "offline"


class ProviderFailed(WebError):
    outcome = "provider_failed"


class AuthenticationFailed(ProviderFailed):
    outcome = "authentication_failed"


class RateLimited(WebError):
    outcome = "rate_limited"


class TooLarge(WebError):
    outcome = "too_large"


class UnsupportedContent(WebError):
    outcome = "unsupported_content"


@dataclass(frozen=True)
class ProviderConfig:
    endpoint: str
    label: str = "SearXNG"
    allow_private_endpoint: bool = False
    kind: str = "searxng"


@dataclass(frozen=True)
class Limits:
    allowed_domains: Tuple[str, ...] = ()
    default_results: int = 5
    default_timeout_seconds: float = 8.0
    max_redirects: int = MAX_REDIRECTS
    max_provider_bytes: int = MAX_PROVIDER_BYTES
    max_download_bytes: int = MAX_DOWNLOAD_BYTES
    default_content_bytes: int = 64 * 1024
    max_content_bytes: int = MAX_CONTENT_BYTES
    cache_ttl_seconds: int = 300
    cache_entries: int = MAX_CACHE_ENTRIES
    cache_bytes: int = MAX_CACHE_BYTES


@dataclass(frozen=True)
class Configuration:
    provider: ProviderConfig
    limits: Limits
    fingerprint: str


@dataclass(frozen=True)
class ResolvedAddress:
    family: int
    sockaddr: tuple
    address: ipaddress._BaseAddress


@dataclass(frozen=True)
class HttpPayload:
    final_url: str
    status: int
    headers: Mapping[str, str]
    body: bytes
    redirects: int


@dataclass
class CacheEntry:
    value: Any
    expires_at: float
    size: int


class Deadline:
    def __init__(self, seconds: float, cancellation: Any = None) -> None:
        self._end = time.monotonic() + seconds
        self._cancellation = cancellation

    def checkpoint(self) -> None:
        if self._cancellation is not None:
            self._cancellation.raise_if_cancelled()
        if time.monotonic() >= self._end:
            raise RequestTimedOut("the web request reached its time limit")

    def remaining(self) -> float:
        self.checkpoint()
        return max(0.001, self._end - time.monotonic())

    def socket_timeout(self) -> float:
        # API 0.2 gives a two-second cancellation grace.  No individual socket
        # operation is allowed to consume that whole interval.
        return max(0.05, min(0.75, self.remaining()))


class BoundedCache:
    """Small process-local TTL/LRU cache; no retrieved data is persisted."""

    def __init__(self) -> None:
        self._entries: "OrderedDict[str, CacheEntry]" = OrderedDict()
        self._bytes = 0
        self._lock = threading.RLock()
        self._limits = (0, 0, 0)

    def configure(self, entries: int, total_bytes: int, ttl_seconds: int) -> None:
        limits = (entries, total_bytes, ttl_seconds)
        with self._lock:
            if limits != self._limits:
                self._entries.clear()
                self._bytes = 0
                self._limits = limits

    def clear(self) -> None:
        with self._lock:
            self._entries.clear()
            self._bytes = 0

    def get(self, key: str) -> Optional[Any]:
        now = time.monotonic()
        with self._lock:
            entry = self._entries.get(key)
            if entry is None:
                return None
            if entry.expires_at <= now:
                self._remove(key)
                return None
            self._entries.move_to_end(key)
            return copy.deepcopy(entry.value)

    def put(self, key: str, value: Any) -> None:
        with self._lock:
            max_entries, max_bytes, ttl_seconds = self._limits
            if max_entries <= 0 or max_bytes <= 0 or ttl_seconds <= 0:
                return
            encoded = json.dumps(
                value,
                ensure_ascii=False,
                sort_keys=True,
                separators=(",", ":"),
            ).encode("utf-8")
            size = len(encoded)
            if size > max_bytes:
                return
            if key in self._entries:
                self._remove(key)
            self._entries[key] = CacheEntry(
                copy.deepcopy(value), time.monotonic() + ttl_seconds, size
            )
            self._bytes += size
            while len(self._entries) > max_entries or self._bytes > max_bytes:
                oldest = next(iter(self._entries))
                self._remove(oldest)

    def _remove(self, key: str) -> None:
        entry = self._entries.pop(key, None)
        if entry is not None:
            self._bytes -= entry.size


def default_config_path() -> Path:
    return Path.home() / ".config" / "ygg" / "ygg-web-search.json"


def default_brave_credentials_path() -> Path:
    return Path.home() / ".ygg" / "credentials" / "ygg-web-search-brave.key"


def validate_brave_api_key(value: Any) -> str:
    if not isinstance(value, str):
        raise ConfigError("Brave Search API key must be text")
    key = value.strip()
    try:
        encoded = key.encode("ascii", errors="strict")
    except UnicodeEncodeError as error:
        raise ConfigError("Brave Search API key must contain ASCII characters only") from error
    if not key or len(encoded) > MAX_BRAVE_API_KEY_BYTES:
        raise ConfigError("Brave Search API key must contain 1 to 1024 bytes")
    if any(character.isspace() or ord(character) < 0x20 or ord(character) == 0x7F for character in key):
        raise ConfigError("Brave Search API key cannot contain whitespace or control characters")
    return key


def _ensure_private_parent(path: Path) -> None:
    parent = path.parent
    try:
        parent.mkdir(parents=True, mode=0o700, exist_ok=True)
    except OSError as error:
        raise ConfigError("web search state directory could not be created safely") from error
    try:
        info = os.lstat(str(parent))
    except OSError as error:
        raise ConfigError("web search state directory could not be inspected safely") from error
    if stat.S_ISLNK(info.st_mode) or not stat.S_ISDIR(info.st_mode):
        raise ConfigError("web search state directory must be a regular directory")
    if hasattr(os, "getuid") and info.st_uid != os.getuid():
        raise ConfigError("web search state directory must be owned by the current user")


def _atomic_write(path: Path, data: bytes, mode: int) -> None:
    _ensure_private_parent(path)
    try:
        existing = os.lstat(str(path))
    except FileNotFoundError:
        existing = None
    except OSError as error:
        raise ConfigError("web search state file could not be inspected safely") from error
    if existing is not None:
        if stat.S_ISLNK(existing.st_mode) or not stat.S_ISREG(existing.st_mode):
            raise ConfigError("web search state file must be a regular non-symlink file")
        if hasattr(os, "getuid") and existing.st_uid != os.getuid():
            raise ConfigError("web search state file must be owned by the current user")

    temporary = path.parent / (".%s.%s.tmp" % (path.name, secrets.token_hex(8)))
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    flags |= getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
    descriptor: Optional[int] = None
    try:
        descriptor = os.open(str(temporary), flags, mode)
        os.fchmod(descriptor, mode)
        view = memoryview(data)
        written = 0
        while written < len(view):
            count = os.write(descriptor, view[written:])
            if count <= 0:
                raise OSError("short write")
            written += count
        os.fsync(descriptor)
        os.close(descriptor)
        descriptor = None
        os.replace(str(temporary), str(path))
        try:
            directory = os.open(str(path.parent), os.O_RDONLY | getattr(os, "O_CLOEXEC", 0))
        except OSError:
            directory = None
        if directory is not None:
            try:
                os.fsync(directory)
            finally:
                os.close(directory)
    except OSError as error:
        raise ConfigError("web search state file could not be written safely") from error
    finally:
        if descriptor is not None:
            os.close(descriptor)
        try:
            temporary.unlink()
        except FileNotFoundError:
            pass


def load_brave_api_key(path: Optional[Path] = None) -> str:
    credential_path = default_brave_credentials_path() if path is None else Path(path)
    try:
        before = os.lstat(str(credential_path))
        if stat.S_ISLNK(before.st_mode):
            raise ConfigError("Brave Search credential cannot be a symbolic link")
    except FileNotFoundError as error:
        raise CredentialRequired(
            "Brave Search needs an API key; get one at %s" % BRAVE_SEARCH_KEY_URL
        ) from error
    except OSError as error:
        raise ConfigError("Brave Search credential could not be inspected safely") from error
    flags = os.O_RDONLY | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(str(credential_path), flags)
    except FileNotFoundError as error:
        raise CredentialRequired(
            "Brave Search needs an API key; get one at %s" % BRAVE_SEARCH_KEY_URL
        ) from error
    except OSError as error:
        raise ConfigError("Brave Search credential could not be opened safely") from error
    try:
        info = os.fstat(descriptor)
        if (before.st_dev, before.st_ino) != (info.st_dev, info.st_ino):
            raise ConfigError("Brave Search credential changed while it was opened")
        if not stat.S_ISREG(info.st_mode):
            raise ConfigError("Brave Search credential must be a regular file")
        if hasattr(os, "getuid") and info.st_uid != os.getuid():
            raise ConfigError("Brave Search credential must be owned by the current user")
        if info.st_mode & 0o077:
            raise ConfigError("Brave Search credential must not be accessible by group or other users")
        if info.st_size > MAX_BRAVE_API_KEY_BYTES + 1:
            raise ConfigError("Brave Search credential exceeds 1024 bytes")
        data = os.read(descriptor, MAX_BRAVE_API_KEY_BYTES + 2)
        if len(data) > MAX_BRAVE_API_KEY_BYTES + 1:
            raise ConfigError("Brave Search credential exceeds 1024 bytes")
        try:
            return validate_brave_api_key(data.decode("utf-8").rstrip("\n"))
        except UnicodeDecodeError as error:
            raise ConfigError("Brave Search credential is not valid UTF-8") from error
    finally:
        os.close(descriptor)


def store_brave_api_key(value: Any, path: Optional[Path] = None) -> None:
    credential_path = default_brave_credentials_path() if path is None else Path(path)
    key = validate_brave_api_key(value)
    _atomic_write(credential_path, key.encode("utf-8") + b"\n", 0o600)


def remove_brave_api_key(path: Optional[Path] = None) -> bool:
    credential_path = default_brave_credentials_path() if path is None else Path(path)
    try:
        info = os.lstat(str(credential_path))
    except FileNotFoundError:
        return False
    except OSError as error:
        raise ConfigError("Brave Search credential could not be inspected safely") from error
    if stat.S_ISLNK(info.st_mode) or not stat.S_ISREG(info.st_mode):
        raise ConfigError("Brave Search credential must be a regular non-symlink file")
    if hasattr(os, "getuid") and info.st_uid != os.getuid():
        raise ConfigError("Brave Search credential must be owned by the current user")
    try:
        credential_path.unlink()
    except OSError as error:
        raise ConfigError("Brave Search credential could not be removed safely") from error
    return True


def _bounded_regular_file(path: Path) -> bytes:
    try:
        before = os.lstat(str(path))
        if stat.S_ISLNK(before.st_mode):
            raise ConfigError("web search configuration cannot be a symbolic link")
    except FileNotFoundError as error:
        raise Disabled("web search is Off; its configuration file is absent") from error
    except OSError as error:
        raise ConfigError("web search configuration could not be inspected safely") from error
    flags = os.O_RDONLY
    if hasattr(os, "O_CLOEXEC"):
        flags |= os.O_CLOEXEC
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    try:
        descriptor = os.open(str(path), flags)
    except FileNotFoundError as error:
        raise Disabled("web search is Off; its configuration file is absent") from error
    except OSError as error:
        raise ConfigError("web search configuration could not be opened safely") from error
    try:
        info = os.fstat(descriptor)
        if (before.st_dev, before.st_ino) != (info.st_dev, info.st_ino):
            raise ConfigError("web search configuration changed while it was opened")
        if not stat.S_ISREG(info.st_mode):
            raise ConfigError("web search configuration must be a regular file")
        if hasattr(os, "getuid") and info.st_uid != os.getuid():
            raise ConfigError("web search configuration must be owned by the current user")
        if info.st_mode & (stat.S_IWGRP | stat.S_IWOTH):
            raise ConfigError(
                "web search configuration cannot be group- or world-writable"
            )
        if info.st_size > MAX_CONFIG_BYTES:
            raise ConfigError("web search configuration exceeds 64 KiB")
        chunks: List[bytes] = []
        remaining = MAX_CONFIG_BYTES + 1
        while remaining:
            chunk = os.read(descriptor, min(16384, remaining))
            if not chunk:
                break
            chunks.append(chunk)
            remaining -= len(chunk)
        data = b"".join(chunks)
        if len(data) > MAX_CONFIG_BYTES:
            raise ConfigError("web search configuration exceeds 64 KiB")
        return data
    finally:
        os.close(descriptor)


def load_configuration(path: Optional[Path] = None) -> Configuration:
    config_path = default_config_path() if path is None else Path(path)
    raw = _bounded_regular_file(config_path)
    try:
        value = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ConfigError("web search configuration is not valid UTF-8 JSON") from error
    return parse_configuration(value)


def _parse_searxng_provider(value: Any, label_prefix: str) -> ProviderConfig:
    provider_value = _object(
        value,
        label_prefix,
        ("kind", "endpoint", "label", "allow_private_endpoint"),
    )
    if provider_value.get("kind") != "searxng":
        raise ConfigError("%s.kind must be 'searxng'" % label_prefix)
    endpoint_value = provider_value.get("endpoint")
    if not isinstance(endpoint_value, str) or not endpoint_value.strip():
        raise ConfigError("%s.endpoint must be a non-empty URL" % label_prefix)
    try:
        endpoint_with_fragment = sanitize_url(endpoint_value, keep_fragment=True)
    except WebError as error:
        raise ConfigError(
            "%s.endpoint must be a credential-free HTTP(S) URL" % label_prefix
        ) from error
    parsed_endpoint = urlsplit(endpoint_with_fragment)
    if parsed_endpoint.query:
        raise ConfigError("%s.endpoint cannot contain a query" % label_prefix)
    if parsed_endpoint.fragment:
        raise ConfigError("%s.endpoint cannot contain a fragment" % label_prefix)
    endpoint = sanitize_url(endpoint_with_fragment, keep_fragment=False)
    label = _bounded_label(provider_value.get("label", "SearXNG"))
    allow_private = provider_value.get("allow_private_endpoint", False)
    if not isinstance(allow_private, bool):
        raise ConfigError("%s.allow_private_endpoint must be a boolean" % label_prefix)
    return ProviderConfig(endpoint, label, allow_private, "searxng")


def _parse_active_provider(value: Any) -> ProviderConfig:
    if not isinstance(value, dict):
        raise ConfigError("configuration.provider must be an object")
    kind = value.get("kind")
    if kind == "searxng":
        return _parse_searxng_provider(value, "configuration.provider")
    if kind == "brave":
        provider_value = _object(
            value,
            "configuration.provider",
            ("kind", "label"),
        )
        label = _bounded_label(provider_value.get("label", "Brave Search"))
        return ProviderConfig(BRAVE_SEARCH_ENDPOINT, label, False, "brave")
    raise ConfigError("configuration.provider.kind must be 'brave' or 'searxng'")


def parse_configuration(value: Any) -> Configuration:
    root = _object(
        value,
        "configuration",
        ("version", "provider", "provider_settings", "limits"),
    )
    version = root.get("version")
    if isinstance(version, bool) or version != CONFIG_VERSION:
        raise ConfigError("configuration.version must be 1")

    provider = _parse_active_provider(root.get("provider"))
    settings = _object(
        root.get("provider_settings", {}),
        "configuration.provider_settings",
        ("searxng",),
    )
    if "searxng" in settings:
        _parse_searxng_provider(
            settings["searxng"],
            "configuration.provider_settings.searxng",
        )

    limits_value = _object(
        root.get("limits", {}),
        "configuration.limits",
        (
            "allowed_domains",
            "default_results",
            "default_timeout_seconds",
            "max_redirects",
            "max_provider_bytes",
            "max_download_bytes",
            "default_content_bytes",
            "max_content_bytes",
            "cache_ttl_seconds",
            "cache_entries",
            "cache_bytes",
        ),
    )
    allowed_value = limits_value.get("allowed_domains", [])
    if not isinstance(allowed_value, list) or len(allowed_value) > MAX_CONFIG_DOMAINS:
        raise ConfigError("configuration.limits.allowed_domains must contain at most 32 domains")
    try:
        allowed_domains = tuple(sorted(set(normalize_domain(item) for item in allowed_value)))
    except InvalidInput as error:
        raise ConfigError(str(error)) from error

    default_results = _integer(
        limits_value.get("default_results", 5),
        "configuration.limits.default_results",
        1,
        MAX_RESULTS,
    )
    default_timeout = _number(
        limits_value.get("default_timeout_seconds", 8.0),
        "configuration.limits.default_timeout_seconds",
        MIN_TIMEOUT_SECONDS,
        MAX_TIMEOUT_SECONDS,
    )
    max_redirects = _integer(
        limits_value.get("max_redirects", MAX_REDIRECTS),
        "configuration.limits.max_redirects",
        0,
        MAX_REDIRECTS,
    )
    max_provider_bytes = _integer(
        limits_value.get("max_provider_bytes", MAX_PROVIDER_BYTES),
        "configuration.limits.max_provider_bytes",
        1024,
        MAX_PROVIDER_BYTES,
    )
    max_download_bytes = _integer(
        limits_value.get("max_download_bytes", MAX_DOWNLOAD_BYTES),
        "configuration.limits.max_download_bytes",
        1024,
        MAX_DOWNLOAD_BYTES,
    )
    max_content_bytes = _integer(
        limits_value.get("max_content_bytes", MAX_CONTENT_BYTES),
        "configuration.limits.max_content_bytes",
        1024,
        MAX_CONTENT_BYTES,
    )
    default_content_bytes = _integer(
        limits_value.get("default_content_bytes", min(64 * 1024, max_content_bytes)),
        "configuration.limits.default_content_bytes",
        1024,
        max_content_bytes,
    )
    cache_ttl = _integer(
        limits_value.get("cache_ttl_seconds", 300),
        "configuration.limits.cache_ttl_seconds",
        0,
        MAX_CACHE_TTL_SECONDS,
    )
    cache_entries = _integer(
        limits_value.get("cache_entries", MAX_CACHE_ENTRIES),
        "configuration.limits.cache_entries",
        0,
        MAX_CACHE_ENTRIES,
    )
    cache_bytes = _integer(
        limits_value.get("cache_bytes", MAX_CACHE_BYTES),
        "configuration.limits.cache_bytes",
        0,
        MAX_CACHE_BYTES,
    )

    canonical = json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
    fingerprint = hashlib.sha256(canonical.encode("utf-8")).hexdigest()
    return Configuration(
        provider=provider,
        limits=Limits(
            allowed_domains=allowed_domains,
            default_results=default_results,
            default_timeout_seconds=default_timeout,
            max_redirects=max_redirects,
            max_provider_bytes=max_provider_bytes,
            max_download_bytes=max_download_bytes,
            default_content_bytes=default_content_bytes,
            max_content_bytes=max_content_bytes,
            cache_ttl_seconds=cache_ttl,
            cache_entries=cache_entries,
            cache_bytes=cache_bytes,
        ),
        fingerprint=fingerprint,
    )


def _configuration_value_for_update(path: Path) -> Dict[str, Any]:
    try:
        raw = _bounded_regular_file(path)
    except Disabled:
        return {"version": CONFIG_VERSION, "limits": {}}
    try:
        value = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ConfigError("web search configuration is not valid UTF-8 JSON") from error
    parse_configuration(value)
    return copy.deepcopy(value)


def select_provider(
    kind: str,
    *,
    path: Optional[Path] = None,
    searxng_endpoint: Optional[str] = None,
) -> Configuration:
    if kind not in ("brave", "searxng"):
        raise ConfigError("web search provider must be 'brave' or 'searxng'")
    config_path = default_config_path() if path is None else Path(path)
    value = _configuration_value_for_update(config_path)
    current = value.get("provider")
    settings_value = value.get("provider_settings", {})
    settings = copy.deepcopy(settings_value) if isinstance(settings_value, dict) else {}
    if isinstance(current, dict) and current.get("kind") == "searxng":
        settings["searxng"] = copy.deepcopy(current)

    if kind == "brave":
        provider: Dict[str, Any] = {"kind": "brave", "label": "Brave Search"}
    else:
        saved = settings.get("searxng")
        if isinstance(current, dict) and current.get("kind") == "searxng":
            provider = copy.deepcopy(current)
        elif isinstance(saved, dict):
            provider = copy.deepcopy(saved)
        elif searxng_endpoint is not None:
            provider = {
                "kind": "searxng",
                "endpoint": searxng_endpoint,
                "label": "SearXNG",
                "allow_private_endpoint": False,
            }
        else:
            raise ConfigError("SearXNG setup needs a search endpoint URL")
        settings["searxng"] = copy.deepcopy(provider)

    updated: Dict[str, Any] = {
        "version": CONFIG_VERSION,
        "provider": provider,
    }
    if settings:
        updated["provider_settings"] = settings
    limits = value.get("limits")
    if isinstance(limits, dict) and limits:
        updated["limits"] = copy.deepcopy(limits)
    config = parse_configuration(updated)
    encoded = (json.dumps(updated, ensure_ascii=False, indent=2) + "\n").encode("utf-8")
    if len(encoded) > MAX_CONFIG_BYTES:
        raise ConfigError("web search configuration exceeds 64 KiB")
    _atomic_write(config_path, encoded, 0o600)
    return config


def _object(value: Any, label: str, fields: Sequence[str]) -> Dict[str, Any]:
    if not isinstance(value, dict):
        raise ConfigError("%s must be an object" % label)
    unknown = set(value) - set(fields)
    if unknown:
        # Do not echo unknown field names: a mistaken credential key must not
        # enter diagnostics or model-visible configuration errors.
        raise ConfigError("%s has unsupported fields" % label)
    return dict(value)


def _integer(value: Any, label: str, minimum: int, maximum: int) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        raise ConfigError("%s must be an integer" % label)
    if value < minimum or value > maximum:
        raise ConfigError("%s must be between %d and %d" % (label, minimum, maximum))
    return value


def _number(value: Any, label: str, minimum: float, maximum: float) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ConfigError("%s must be a number" % label)
    result = float(value)
    if not math.isfinite(result) or result < minimum or result > maximum:
        raise ConfigError("%s must be between %.1f and %.1f" % (label, minimum, maximum))
    return result


def _bounded_label(value: Any) -> str:
    if not isinstance(value, str):
        raise ConfigError("configuration.provider.label must be a string")
    label = collapse_whitespace(value)
    if not label or len(label.encode("utf-8")) > MAX_LABEL_BYTES:
        raise ConfigError("configuration.provider.label must be 1..48 UTF-8 bytes")
    if strip_controls(label) != label:
        raise ConfigError("configuration.provider.label contains control characters")
    return label


def bounded_query(value: Any) -> str:
    if not isinstance(value, str):
        raise InvalidInput("query must be a string")
    if any(unicodedata.category(character).startswith("C") for character in value):
        raise InvalidInput("query contains control characters")
    query = collapse_whitespace(value)
    if not query:
        raise InvalidInput("query cannot be empty")
    if len(query.encode("utf-8")) > MAX_QUERY_BYTES:
        raise InvalidInput("query exceeds 512 UTF-8 bytes")
    return query


def bounded_pattern(value: Any) -> str:
    if not isinstance(value, str):
        raise InvalidInput("pattern must be a string")
    if any(unicodedata.category(character).startswith("C") for character in value):
        raise InvalidInput("pattern contains control characters")
    pattern = collapse_whitespace(value)
    if not pattern:
        raise InvalidInput("pattern cannot be empty")
    if len(pattern.encode("utf-8")) > MAX_PATTERN_BYTES:
        raise InvalidInput("pattern exceeds 256 UTF-8 bytes")
    return pattern


def bounded_int(value: Any, default: int, minimum: int, maximum: int, label: str) -> int:
    if value is None:
        return default
    if isinstance(value, bool) or not isinstance(value, int):
        raise InvalidInput("%s must be an integer" % label)
    if value < minimum or value > maximum:
        raise InvalidInput("%s must be between %d and %d" % (label, minimum, maximum))
    return value


def bounded_timeout(value: Any, default: float) -> float:
    if value is None:
        return default
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise InvalidInput("timeout_seconds must be a number")
    timeout = float(value)
    if not math.isfinite(timeout) or timeout < MIN_TIMEOUT_SECONDS or timeout > MAX_TIMEOUT_SECONDS:
        raise InvalidInput("timeout_seconds must be between 0.1 and 20")
    return timeout


def requested_domains(value: Any, configured: Sequence[str]) -> Tuple[str, ...]:
    if value is None:
        return tuple(configured)
    if not isinstance(value, list) or len(value) > MAX_DOMAINS:
        raise InvalidInput("domains must contain at most 5 domain names")
    domains = tuple(sorted(set(normalize_domain(item) for item in value)))
    if configured and not domains:
        return tuple(configured)
    if configured:
        for domain in domains:
            if not any(domain_matches(domain, allowed) for allowed in configured):
                raise DestinationRejected("a requested domain is outside the configured allowlist")
    return domains


def normalize_domain(value: Any) -> str:
    if not isinstance(value, str):
        raise InvalidInput("domain names must be strings")
    domain = value.strip().rstrip(".").lower()
    if not domain or len(domain.encode("utf-8")) > 253:
        raise InvalidInput("domain names must contain 1..253 UTF-8 bytes")
    if "://" in domain or any(character in domain for character in "/?#@:%"):
        raise InvalidInput("domains must be bare host names without schemes, ports, or paths")
    try:
        ascii_domain = domain.encode("idna").decode("ascii")
    except UnicodeError as error:
        raise InvalidInput("domain name is not valid IDNA") from error
    labels = ascii_domain.split(".")
    if any(
        not label
        or len(label) > 63
        or label.startswith("-")
        or label.endswith("-")
        or not re.fullmatch(r"[a-z0-9-]+", label)
        for label in labels
    ):
        raise InvalidInput("domain name is invalid")
    if ascii_domain == "localhost" or ascii_domain.endswith(".localhost") or ascii_domain.endswith(".local"):
        raise InvalidInput("local domain names are not allowed")
    return ascii_domain


def domain_matches(host: str, domain: str) -> bool:
    host = host.lower().rstrip(".")
    domain = domain.lower().rstrip(".")
    return host == domain or host.endswith("." + domain)


def collapse_whitespace(value: str) -> str:
    return " ".join(str(value).split())


def truncate_utf8(value: str, maximum: int) -> Tuple[str, bool]:
    encoded = value.encode("utf-8")
    if len(encoded) <= maximum:
        return value, False
    clipped = encoded[:maximum]
    while clipped:
        try:
            return clipped.decode("utf-8").rstrip(), True
        except UnicodeDecodeError as error:
            clipped = clipped[: error.start]
    return "", True


def strip_controls(value: str) -> str:
    return "".join(
        character
        for character in value
        if character in "\n\t" or not unicodedata.category(character).startswith("C")
    )


def sanitize_text(value: Any, maximum: int, collapse: bool = True) -> str:
    text = strip_controls(html.unescape(str(value or "")))
    if collapse:
        text = collapse_whitespace(text)
    text, _ = truncate_utf8(text, maximum)
    return text


def sanitize_url(value: Any, keep_fragment: bool = False) -> str:
    if not isinstance(value, str):
        raise InvalidInput("URL must be a string")
    raw = value.strip()
    if not raw or len(raw.encode("utf-8")) > MAX_URL_BYTES:
        raise InvalidInput("URL must contain 1..2048 UTF-8 bytes")
    if any(unicodedata.category(character).startswith("C") for character in raw):
        raise InvalidInput("URL contains control characters")
    try:
        parsed = urlsplit(raw)
    except ValueError as error:
        raise InvalidInput("URL is malformed") from error
    if parsed.scheme.lower() not in ("http", "https"):
        raise DestinationRejected("only http and https URLs are supported")
    if parsed.username is not None or parsed.password is not None:
        raise DestinationRejected("URLs containing credentials are rejected")
    hostname = parsed.hostname
    if not hostname:
        raise InvalidInput("URL must contain a host")
    try:
        host = hostname.encode("idna").decode("ascii").lower().rstrip(".")
    except UnicodeError as error:
        raise InvalidInput("URL host is not valid IDNA") from error
    if not host or len(host) > 253:
        raise InvalidInput("URL host is invalid")
    try:
        port = parsed.port
    except ValueError as error:
        raise InvalidInput("URL port is invalid") from error
    default_port = 443 if parsed.scheme.lower() == "https" else 80
    if port is not None and port != default_port:
        netloc = "[%s]:%d" % (host, port) if ":" in host else "%s:%d" % (host, port)
    else:
        netloc = "[%s]" % host if ":" in host else host
    path = quote(parsed.path or "/", safe="/%:@!$&'()*+,;=-._~")
    query_pairs = []
    try:
        parsed_pairs = parse_qsl(parsed.query, keep_blank_values=True, max_num_fields=128)
    except ValueError as error:
        raise InvalidInput("URL query contains too many fields") from error
    for key, item in parsed_pairs:
        lowered = key.lower()
        if lowered.startswith("utm_") or lowered in TRACKING_QUERY_NAMES:
            continue
        query_pairs.append((key, item))
    query_pairs.sort()
    query = urlencode(query_pairs, doseq=True)
    fragment = quote(parsed.fragment, safe="-._~") if keep_fragment else ""
    result = urlunsplit((parsed.scheme.lower(), netloc, path, query, fragment))
    if len(result.encode("utf-8")) > MAX_URL_BYTES:
        raise InvalidInput("sanitized URL exceeds 2048 UTF-8 bytes")
    return result


def citation_id(url: str) -> str:
    canonical = sanitize_url(url, keep_fragment=False)
    digest = hashlib.sha256(canonical.encode("utf-8")).hexdigest()[:16]
    return "web-" + digest


def origin_for(url: str) -> str:
    parsed = urlsplit(url)
    host = parsed.hostname or "unknown"
    if parsed.port is not None and parsed.port not in (80, 443):
        return "%s://%s:%d" % (parsed.scheme, host, parsed.port)
    return "%s://%s" % (parsed.scheme, host)


def validate_url_policy(
    url: str,
    domains: Sequence[str],
    allowed_ports: Optional[Sequence[int]],
) -> Tuple[str, str, int]:
    sanitized = sanitize_url(url, keep_fragment=False)
    parsed = urlsplit(sanitized)
    host = parsed.hostname or ""
    port = parsed.port or (443 if parsed.scheme == "https" else 80)
    if allowed_ports is not None and port not in allowed_ports:
        raise DestinationRejected("the destination port is not allowed")
    if domains and not any(domain_matches(host, domain) for domain in domains):
        raise DestinationRejected("the destination is outside the allowed domains")
    if host == "localhost" or host.endswith(".localhost") or host.endswith(".local"):
        raise DestinationRejected("local destinations are rejected")
    return sanitized, host, port


def system_resolver(host: str, port: int, deadline: Deadline) -> List[ResolvedAddress]:
    deadline.checkpoint()
    try:
        records = socket.getaddrinfo(host, port, type=socket.SOCK_STREAM)
    except socket.gaierror as error:
        deadline.checkpoint()
        raise Offline("the destination host could not be resolved") from error
    deadline.checkpoint()
    addresses: List[ResolvedAddress] = []
    seen = set()
    for family, socktype, _protocol, _canonname, sockaddr in records:
        if socktype != socket.SOCK_STREAM or family not in (socket.AF_INET, socket.AF_INET6):
            continue
        try:
            address = ipaddress.ip_address(sockaddr[0])
        except ValueError:
            continue
        marker = (family, sockaddr)
        if marker in seen:
            continue
        seen.add(marker)
        addresses.append(ResolvedAddress(family, sockaddr, address))
    if not addresses:
        raise Offline("the destination host has no usable address")
    return addresses


def _address_allowed(address: ipaddress._BaseAddress, allow_private: bool) -> bool:
    if address.is_unspecified or address.is_multicast:
        return False
    if allow_private:
        return True
    return address.is_global


class _PinnedHTTPConnection(http.client.HTTPConnection):
    def __init__(
        self,
        host: str,
        port: int,
        resolved: ResolvedAddress,
        timeout: float,
    ) -> None:
        super().__init__(host, port=port, timeout=timeout)
        self._resolved = resolved

    def connect(self) -> None:
        sock = socket.socket(self._resolved.family, socket.SOCK_STREAM)
        try:
            sock.settimeout(self.timeout)
            sock.connect(self._resolved.sockaddr)
            self.sock = sock
        except BaseException:
            sock.close()
            raise


class _PinnedHTTPSConnection(http.client.HTTPSConnection):
    def __init__(
        self,
        host: str,
        port: int,
        resolved: ResolvedAddress,
        timeout: float,
        context: ssl.SSLContext,
    ) -> None:
        super().__init__(host, port=port, timeout=timeout, context=context)
        self._resolved = resolved

    def connect(self) -> None:
        raw = socket.socket(self._resolved.family, socket.SOCK_STREAM)
        try:
            raw.settimeout(self.timeout)
            raw.connect(self._resolved.sockaddr)
            self.sock = self._context.wrap_socket(raw, server_hostname=self.host)
        except BaseException:
            raw.close()
            raise


class HttpClient:
    def __init__(
        self,
        resolver: Callable[[str, int, Deadline], List[ResolvedAddress]] = system_resolver,
        ssl_context: Optional[ssl.SSLContext] = None,
    ) -> None:
        self._resolver = resolver
        self._ssl_context = ssl_context or ssl.create_default_context()

    def fetch(
        self,
        url: str,
        *,
        deadline: Deadline,
        max_bytes: int,
        max_redirects: int,
        allowed_domains: Sequence[str] = (),
        allowed_ports: Optional[Sequence[int]] = OPEN_PORTS,
        allow_private: bool = False,
        accept: str = "text/html, text/plain;q=0.9, application/xhtml+xml;q=0.8",
        headers: Optional[Mapping[str, str]] = None,
    ) -> HttpPayload:
        current = sanitize_url(url, keep_fragment=False)
        redirects = 0
        previous_scheme: Optional[str] = None
        while True:
            deadline.checkpoint()
            current, host, port = validate_url_policy(current, allowed_domains, allowed_ports)
            parsed = urlsplit(current)
            if previous_scheme == "https" and parsed.scheme == "http":
                raise RedirectRejected("an HTTPS-to-HTTP redirect was rejected")
            addresses = self._resolver(host, port, deadline)
            if any(not _address_allowed(item.address, allow_private) for item in addresses):
                raise DestinationRejected("loopback, private, link-local, and reserved destinations are rejected")
            payload = self._fetch_once(
                current,
                addresses,
                deadline=deadline,
                max_bytes=max_bytes,
                accept=accept,
                headers=headers,
            )
            if payload.status not in REDIRECT_STATUSES:
                return HttpPayload(current, payload.status, payload.headers, payload.body, redirects)
            location = payload.headers.get("location")
            if not location:
                raise RedirectRejected("redirect response did not include a Location header")
            if redirects >= max_redirects:
                raise RedirectRejected("the redirect limit was reached")
            if len(location.encode("utf-8", errors="replace")) > MAX_URL_BYTES:
                raise RedirectRejected("redirect target exceeds the URL limit")
            previous_scheme = parsed.scheme
            try:
                current = sanitize_url(urljoin(current, location), keep_fragment=False)
            except WebError as error:
                raise RedirectRejected("redirect target was rejected") from error
            redirects += 1

    def _fetch_once(
        self,
        url: str,
        addresses: Sequence[ResolvedAddress],
        *,
        deadline: Deadline,
        max_bytes: int,
        accept: str,
        headers: Optional[Mapping[str, str]],
    ) -> HttpPayload:
        parsed = urlsplit(url)
        host = parsed.hostname or ""
        port = parsed.port or (443 if parsed.scheme == "https" else 80)
        target = parsed.path or "/"
        if parsed.query:
            target += "?" + parsed.query
        request_headers = {
            "Accept": accept,
            "Accept-Encoding": "identity",
            "Connection": "close",
            "User-Agent": USER_AGENT,
        }
        for name, value in (headers or {}).items():
            if not isinstance(name, str) or not isinstance(value, str):
                raise ProviderFailed("the provider request headers are invalid")
            if "\r" in name or "\n" in name or "\r" in value or "\n" in value:
                raise ProviderFailed("the provider request headers are invalid")
            request_headers[name] = value
        last_error: Optional[BaseException] = None
        for resolved in addresses:
            deadline.checkpoint()
            timeout = deadline.socket_timeout()
            if parsed.scheme == "https":
                connection: http.client.HTTPConnection = _PinnedHTTPSConnection(
                    host, port, resolved, timeout, self._ssl_context
                )
            else:
                connection = _PinnedHTTPConnection(host, port, resolved, timeout)
            response: Optional[http.client.HTTPResponse] = None
            try:
                connection.request(
                    "GET",
                    target,
                    headers=request_headers,
                )
                response = connection.getresponse()
                deadline.checkpoint()
                headers = {key.lower(): value for key, value in response.getheaders()}
                encoding = headers.get("content-encoding", "identity").strip().lower()
                if encoding not in ("", "identity"):
                    raise UnsupportedContent("compressed HTTP responses are not supported")
                declared = headers.get("content-length")
                if declared is not None:
                    try:
                        declared_size = int(declared)
                    except ValueError as error:
                        raise ProviderFailed("the server returned an invalid Content-Length") from error
                    if declared_size < 0:
                        raise ProviderFailed("the server returned an invalid Content-Length")
                    if declared_size > max_bytes:
                        raise TooLarge("the HTTP response exceeds the download byte limit")
                chunks: List[bytes] = []
                size = 0
                while True:
                    deadline.checkpoint()
                    try:
                        chunk = response.read(min(16384, max_bytes + 1 - size))
                    except socket.timeout as error:
                        deadline.checkpoint()
                        raise RequestTimedOut("the web request reached its time limit") from error
                    if not chunk:
                        break
                    size += len(chunk)
                    if size > max_bytes:
                        raise TooLarge("the HTTP response exceeds the download byte limit")
                    chunks.append(chunk)
                return HttpPayload(url, response.status, headers, b"".join(chunks), 0)
            except (WebError,):
                raise
            except socket.timeout as error:
                deadline.checkpoint()
                last_error = error
            except (ssl.SSLError, OSError, http.client.HTTPException) as error:
                deadline.checkpoint()
                last_error = error
            finally:
                if response is not None:
                    response.close()
                connection.close()
        if isinstance(last_error, socket.timeout):
            raise RequestTimedOut("the web request reached its time limit") from last_error
        raise Offline("the destination could not be reached") from last_error


class PageExtractor(HTMLParser):
    BLOCKS = frozenset(
        (
            "address",
            "article",
            "aside",
            "blockquote",
            "br",
            "div",
            "dl",
            "dt",
            "dd",
            "figcaption",
            "figure",
            "footer",
            "h1",
            "h2",
            "h3",
            "h4",
            "h5",
            "h6",
            "header",
            "hr",
            "li",
            "main",
            "nav",
            "ol",
            "p",
            "pre",
            "section",
            "table",
            "td",
            "th",
            "tr",
            "ul",
        )
    )
    SKIP = frozenset(("script", "style", "noscript", "svg", "canvas", "template"))
    PUBLICATION_KEYS = frozenset(
        (
            "article:published_time",
            "date",
            "datepublished",
            "dc.date",
            "dc.date.issued",
            "publish-date",
            "published",
            "pubdate",
        )
    )

    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self.parts: List[str] = []
        self.title_parts: List[str] = []
        self.publication: Optional[str] = None
        self._skip_depth = 0
        self._title_depth = 0

    def handle_starttag(self, tag: str, attrs: List[Tuple[str, Optional[str]]]) -> None:
        lowered = tag.lower()
        if lowered in self.SKIP:
            self._skip_depth += 1
            return
        if self._skip_depth:
            return
        if lowered == "title":
            self._title_depth += 1
        if lowered in self.BLOCKS:
            self.parts.append("\n")
        attributes = {str(key).lower(): value for key, value in attrs if key}
        if lowered == "meta" and self.publication is None:
            key = str(attributes.get("property") or attributes.get("name") or "").lower()
            content = attributes.get("content")
            if key in self.PUBLICATION_KEYS and content:
                self.publication = sanitize_text(content, MAX_PUBLICATION_BYTES)
        elif lowered == "time" and self.publication is None:
            value = attributes.get("datetime")
            if value:
                self.publication = sanitize_text(value, MAX_PUBLICATION_BYTES)

    def handle_startendtag(self, tag: str, attrs: List[Tuple[str, Optional[str]]]) -> None:
        self.handle_starttag(tag, attrs)
        if tag.lower() in self.SKIP:
            self.handle_endtag(tag)

    def handle_endtag(self, tag: str) -> None:
        lowered = tag.lower()
        if lowered in self.SKIP:
            if self._skip_depth:
                self._skip_depth -= 1
            return
        if self._skip_depth:
            return
        if lowered == "title" and self._title_depth:
            self._title_depth -= 1
        if lowered in self.BLOCKS:
            self.parts.append("\n")

    def handle_data(self, data: str) -> None:
        if self._skip_depth:
            return
        self.parts.append(data)
        if self._title_depth:
            self.title_parts.append(data)


def _sanitize_search_fragment(value: Any, maximum: int) -> str:
    parser = PageExtractor()
    try:
        parser.feed(str(value or ""))
        parser.close()
        text = " ".join(parser.parts)
    except (ValueError, AssertionError):
        text = str(value or "")
    return sanitize_text(text, maximum)


def _media_type(headers: Mapping[str, str]) -> Tuple[str, Optional[str]]:
    value = headers.get("content-type", "")
    sections = [section.strip() for section in value.split(";")]
    media_type = sections[0].lower()
    charset = None
    for section in sections[1:]:
        key, separator, item = section.partition("=")
        if separator and key.strip().lower() == "charset":
            charset = item.strip().strip('"\'')
    return media_type, charset


def _decode_text(body: bytes, charset: Optional[str]) -> str:
    encoding = charset or "utf-8"
    try:
        codecs.lookup(encoding)
    except LookupError as error:
        raise UnsupportedContent("the response uses an unsupported character encoding") from error
    return body.decode(encoding, errors="replace")


def normalize_page(payload: HttpPayload, content_bytes: int) -> Dict[str, Any]:
    media_type, charset = _media_type(payload.headers)
    if media_type not in HTML_TYPES and media_type not in PLAIN_TYPES:
        raise UnsupportedContent("the response is not supported HTML or plain text")
    decoded = _decode_text(payload.body, charset)
    title = ""
    publication = None
    if media_type in HTML_TYPES:
        parser = PageExtractor()
        try:
            parser.feed(decoded)
            parser.close()
        except (ValueError, AssertionError) as error:
            raise UnsupportedContent("the HTML response could not be normalized") from error
        content = " ".join("".join(parser.parts).split())
        title = sanitize_text(" ".join(parser.title_parts), MAX_TITLE_BYTES)
        publication = parser.publication
    else:
        content = " ".join(decoded.split())
    content = strip_controls(content)
    content, truncated = truncate_utf8(content, content_bytes)
    url = sanitize_url(payload.final_url, keep_fragment=False)
    if not title:
        title = urlsplit(url).hostname or "Untitled source"
    result: Dict[str, Any] = {
        "citation_id": citation_id(url),
        "title": title,
        "url": url,
        "origin": origin_for(url),
        "content": content,
        "mime_type": media_type,
        "normalized_bytes": len(content.encode("utf-8")),
        "truncated": truncated,
        "redirects": payload.redirects,
    }
    if publication:
        result["published_at"] = publication
    return result


def _search_url(endpoint: str, query: str) -> str:
    parsed = urlsplit(endpoint)
    path = parsed.path
    if not path or path == "/":
        path = "/search"
    pairs = parse_qsl(parsed.query, keep_blank_values=True)
    pairs.extend((("q", query), ("format", "json"), ("safesearch", "1")))
    return urlunsplit((parsed.scheme, parsed.netloc, path, urlencode(pairs), ""))


def _brave_search_url(query: str, count: int) -> str:
    parsed = urlsplit(BRAVE_SEARCH_ENDPOINT)
    pairs = (("q", query), ("count", str(count)), ("safesearch", "moderate"))
    return urlunsplit((parsed.scheme, parsed.netloc, parsed.path, urlencode(pairs), ""))


def _provider_query(query: str, domains: Sequence[str]) -> str:
    if not domains:
        return query
    selectors = " OR ".join("site:%s" % domain for domain in domains)
    combined = "%s (%s)" % (query, selectors)
    if len(combined.encode("utf-8")) > MAX_QUERY_BYTES + 512:
        raise InvalidInput("query plus domain filters exceeds the provider query limit")
    return combined


def _raw_search_results(value: Any, provider_kind: str) -> List[Any]:
    if not isinstance(value, dict):
        raise ProviderFailed("the configured provider returned an invalid result shape")
    if provider_kind == "brave":
        web = value.get("web")
        results = web.get("results") if isinstance(web, dict) else None
    else:
        results = value.get("results")
    if not isinstance(results, list):
        raise ProviderFailed("the configured provider returned an invalid result shape")
    return results


def _normalize_search_results(
    payload: HttpPayload,
    domains: Sequence[str],
    max_results: int,
    provider_kind: str,
) -> Tuple[List[Dict[str, Any]], List[Dict[str, Any]], int]:
    media_type, charset = _media_type(payload.headers)
    if media_type not in SEARCH_TYPES:
        raise ProviderFailed("the configured provider did not return JSON")
    try:
        value = json.loads(_decode_text(payload.body, charset))
    except (json.JSONDecodeError, UnicodeError) as error:
        raise ProviderFailed("the configured provider returned invalid JSON") from error
    raw_results = _raw_search_results(value, provider_kind)
    normalized: List[Dict[str, Any]] = []
    sources: List[Dict[str, Any]] = []
    seen = set()
    dropped = 0
    for raw in raw_results[: max(MAX_RESULTS * 5, max_results)]:
        if len(normalized) >= max_results:
            break
        if not isinstance(raw, dict):
            dropped += 1
            continue
        try:
            url = sanitize_url(raw.get("url"), keep_fragment=False)
            parsed = urlsplit(url)
            host = parsed.hostname or ""
            if host == "localhost" or host.endswith(".localhost") or host.endswith(".local"):
                raise DestinationRejected("local search result")
            try:
                literal = ipaddress.ip_address(host)
            except ValueError:
                literal = None
            if literal is not None and not literal.is_global:
                raise DestinationRejected("private search result")
            if domains and not any(domain_matches(host, domain) for domain in domains):
                raise DestinationRejected("out-of-domain search result")
            identifier = citation_id(url)
            if identifier in seen:
                dropped += 1
                continue
            seen.add(identifier)
            title = _sanitize_search_fragment(raw.get("title"), MAX_TITLE_BYTES)
            if not title:
                title = host or "Untitled source"
            snippet = _sanitize_search_fragment(
                raw.get("content")
                or raw.get("description")
                or raw.get("snippet")
                or "",
                MAX_SNIPPET_BYTES,
            )
            result: Dict[str, Any] = {
                "citation_id": identifier,
                "title": title,
                "url": url,
                "origin": origin_for(url),
                "snippet": snippet,
            }
            published = (
                raw.get("publishedDate")
                or raw.get("published_date")
                or raw.get("page_age")
                or raw.get("age")
                or raw.get("date")
            )
            if published:
                result["published_at"] = sanitize_text(published, MAX_PUBLICATION_BYTES)
            normalized.append(result)
            engine_values = raw.get("engines")
            if provider_kind == "brave":
                engine_values = ["Brave Search"]
            elif not isinstance(engine_values, list):
                engine_values = [raw.get("engine")] if raw.get("engine") else []
            engines = []
            for engine in engine_values[:8]:
                safe = sanitize_text(engine, 64)
                if safe and safe not in engines:
                    engines.append(safe)
            sources.append({"citation_id": identifier, "engines": engines})
        except (WebError, ValueError, TypeError):
            dropped += 1
    return normalized, sources, dropped


def _check_status(
    payload: HttpPayload,
    provider: bool,
    provider_kind: Optional[str] = None,
) -> None:
    status = payload.status
    if 200 <= status < 300:
        return
    if status == 429:
        raise RateLimited("the configured search provider is rate limited")
    if provider_kind == "brave" and status in (401, 403):
        raise AuthenticationFailed("Brave Search rejected the configured API key")
    if status in (408, 504):
        raise RequestTimedOut("the remote service reached its time limit")
    if provider:
        raise ProviderFailed("the configured search provider returned HTTP %d" % status)
    raise WebError("the web source returned HTTP %d" % status)


def _cache_key(kind: str, fingerprint: str, value: Mapping[str, Any]) -> str:
    canonical = json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
    return "%s:%s:%s" % (
        kind,
        fingerprint,
        hashlib.sha256(canonical.encode("utf-8")).hexdigest(),
    )


class WebService:
    """One selected search provider plus bounded public fetch operations."""

    def __init__(self, http: Optional[HttpClient] = None, cache: Optional[BoundedCache] = None) -> None:
        self.http = http or HttpClient()
        self.cache = cache or BoundedCache()

    def configure_cache(self, config: Configuration) -> None:
        limits = config.limits
        self.cache.configure(limits.cache_entries, limits.cache_bytes, limits.cache_ttl_seconds)

    def search(
        self,
        config: Configuration,
        *,
        query: Any,
        domains: Any = None,
        max_results: Any = None,
        timeout_seconds: Any = None,
        api_key: Optional[str] = None,
        cancellation: Any = None,
        progress: Optional[Callable[[str, Optional[int], Optional[int], Optional[str]], None]] = None,
    ) -> Dict[str, Any]:
        self.configure_cache(config)
        clean_query = bounded_query(query)
        selected_domains = requested_domains(domains, config.limits.allowed_domains)
        result_limit = bounded_int(
            max_results, config.limits.default_results, 1, MAX_RESULTS, "max_results"
        )
        timeout = bounded_timeout(timeout_seconds, config.limits.default_timeout_seconds)
        validated_api_key: Optional[str] = None
        credential_scope: Optional[str] = None
        if config.provider.kind == "brave":
            if api_key is None:
                raise CredentialRequired(
                    "Brave Search needs an API key; get one at %s" % BRAVE_SEARCH_KEY_URL
                )
            validated_api_key = validate_brave_api_key(api_key)
            credential_scope = hashlib.sha256(validated_api_key.encode("ascii")).hexdigest()
        key = _cache_key(
            "search",
            config.fingerprint,
            {
                "query": clean_query,
                "domains": selected_domains,
                "results": result_limit,
                "credential_scope": credential_scope,
            },
        )
        cached = self.cache.get(key)
        if cached is not None:
            cached["cache"] = "hit"
            return cached
        if progress:
            progress("searching", 0, result_limit, "results")
        deadline = Deadline(timeout, cancellation)
        provider_query = _provider_query(clean_query, selected_domains)
        request_headers: Optional[Mapping[str, str]] = None
        max_redirects = config.limits.max_redirects
        if config.provider.kind == "brave":
            assert validated_api_key is not None
            endpoint = _brave_search_url(provider_query, result_limit)
            request_headers = {"X-Subscription-Token": validated_api_key}
            # Never forward a credential across even a same-origin redirect.
            max_redirects = 0
        else:
            endpoint = _search_url(config.provider.endpoint, provider_query)
        provider_host = urlsplit(config.provider.endpoint).hostname or ""
        payload = self.http.fetch(
            endpoint,
            deadline=deadline,
            max_bytes=config.limits.max_provider_bytes,
            max_redirects=max_redirects,
            allowed_domains=(provider_host,),
            allowed_ports=None,
            allow_private=config.provider.allow_private_endpoint,
            accept="application/json",
            headers=request_headers,
        )
        _check_status(payload, provider=True, provider_kind=config.provider.kind)
        deadline.checkpoint()
        results, sources, dropped = _normalize_search_results(
            payload,
            selected_domains,
            result_limit,
            config.provider.kind,
        )
        if progress:
            progress("normalizing", len(results), result_limit, "results")
        normalized_bytes = sum(
            len((item["title"] + item["snippet"] + item["url"]).encode("utf-8"))
            for item in results
        )
        value: Dict[str, Any] = {
            "results": results,
            "sources": sources,
            "result_count": len(results),
            "normalized_bytes": normalized_bytes,
            "truncated": dropped > 0
            or len(value_results(payload, config.provider.kind)) > len(results),
            "dropped_results": dropped,
            "cache": "miss",
            "redirects": payload.redirects,
        }
        stored = copy.deepcopy(value)
        stored.pop("cache", None)
        self.cache.put(key, stored)
        return value

    def open(
        self,
        config: Configuration,
        *,
        url: Any,
        max_bytes: Any = None,
        timeout_seconds: Any = None,
        max_redirects: Any = None,
        cancellation: Any = None,
        progress: Optional[Callable[[str, Optional[int], Optional[int], Optional[str]], None]] = None,
    ) -> Dict[str, Any]:
        self.configure_cache(config)
        clean_url = sanitize_url(url, keep_fragment=False)
        content_limit = bounded_int(
            max_bytes,
            config.limits.default_content_bytes,
            1024,
            config.limits.max_content_bytes,
            "max_bytes",
        )
        timeout = bounded_timeout(timeout_seconds, config.limits.default_timeout_seconds)
        redirects = bounded_int(
            max_redirects,
            config.limits.max_redirects,
            0,
            config.limits.max_redirects,
            "max_redirects",
        )
        key = _cache_key(
            "open",
            config.fingerprint,
            {"url": clean_url, "bytes": content_limit, "redirects": redirects},
        )
        cached = self.cache.get(key)
        if cached is not None:
            cached["cache"] = "hit"
            return cached
        if progress:
            progress("fetching", 0, None, "bytes")
        payload = self.http.fetch(
            clean_url,
            deadline=Deadline(timeout, cancellation),
            max_bytes=config.limits.max_download_bytes,
            max_redirects=redirects,
            allowed_domains=config.limits.allowed_domains,
            allowed_ports=OPEN_PORTS,
            allow_private=False,
        )
        _check_status(payload, provider=False)
        document = normalize_page(payload, content_limit)
        if progress:
            progress("normalizing", document["normalized_bytes"], content_limit, "bytes")
        value = {"document": document, "cache": "miss"}
        stored = copy.deepcopy(value)
        stored.pop("cache", None)
        self.cache.put(key, stored)
        return value

    def find(
        self,
        config: Configuration,
        *,
        url: Any,
        pattern: Any,
        max_matches: Any = None,
        max_bytes: Any = None,
        timeout_seconds: Any = None,
        max_redirects: Any = None,
        cancellation: Any = None,
        progress: Optional[Callable[[str, Optional[int], Optional[int], Optional[str]], None]] = None,
    ) -> Dict[str, Any]:
        clean_pattern = bounded_pattern(pattern)
        match_limit = bounded_int(max_matches, 8, 1, MAX_FIND_MATCHES, "max_matches")
        opened = self.open(
            config,
            url=url,
            max_bytes=max_bytes,
            timeout_seconds=timeout_seconds,
            max_redirects=max_redirects,
            cancellation=cancellation,
            progress=progress,
        )
        document = opened["document"]
        content = document["content"]
        expression = re.compile(re.escape(clean_pattern), re.IGNORECASE)
        matches = []
        for found in expression.finditer(content):
            if len(matches) >= match_limit:
                break
            start = max(0, found.start() - 160)
            end = min(len(content), found.end() + 160)
            excerpt = collapse_whitespace(content[start:end])
            excerpt, _ = truncate_utf8(excerpt, 512)
            matches.append(
                {
                    "match_index": len(matches) + 1,
                    "character_offset": found.start(),
                    "excerpt": excerpt,
                }
            )
        total_matches = sum(1 for _ in expression.finditer(content))
        return {
            "document": {
                key: value
                for key, value in document.items()
                if key not in ("content", "redirects", "normalized_bytes", "truncated")
            },
            "matches": matches,
            "match_count": len(matches),
            "truncated": total_matches > len(matches) or document["truncated"],
            "normalized_bytes": sum(len(item["excerpt"].encode("utf-8")) for item in matches),
            "cache": opened["cache"],
            "source_truncated": document["truncated"],
            "redirects": document["redirects"],
        }


def value_results(payload: HttpPayload, provider_kind: str = "searxng") -> List[Any]:
    """Best-effort raw result count used only for truncation metadata."""

    try:
        value = json.loads(payload.body.decode("utf-8"))
        return _raw_search_results(value, provider_kind)
    except (UnicodeError, json.JSONDecodeError, ProviderFailed):
        return []
