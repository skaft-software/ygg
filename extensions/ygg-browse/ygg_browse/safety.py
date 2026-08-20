"""Boundary validation, redaction, ownership, and file-lock helpers."""

from __future__ import annotations

import errno
import os
import re
import stat
import unicodedata
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Mapping, Optional, Tuple
from urllib.parse import SplitResult, urlsplit, urlunsplit

try:
    import fcntl
except ImportError:  # pragma: no cover - Ygg currently targets Unix hosts.
    fcntl = None  # type: ignore[assignment]


MAX_URL_CHARS = 2048
MAX_TARGET_CHARS = 512
MAX_TAB_ID_CHARS = 64
_CONTROL_RE = re.compile(r"[\x00-\x1f\x7f]")
_WHITESPACE_RE = re.compile(r"\s+")
_TAB_ID_RE = re.compile(r"^tab_[A-Za-z0-9_-]{1,59}$")


class BrowseError(RuntimeError):
    """A bounded, model-safe browser failure."""

    def __init__(
        self,
        code: str,
        message: str,
        *,
        untrusted_detail: Optional[str] = None,
    ) -> None:
        super().__init__(message)
        self.code = code
        self.message = bounded_text(message, 2048)
        self.untrusted_detail = (
            bounded_text(untrusted_detail, 2048) if untrusted_detail else None
        )


@dataclass(frozen=True)
class ResourceOwner:
    session_id: str
    extension_instance_id: str
    process_generation: int

    @classmethod
    def from_context(cls, context: Mapping[str, Any]) -> "ResourceOwner":
        value = context.get("resource_owner")
        if not isinstance(value, Mapping):
            raise BrowseError(
                "owner_unavailable",
                "Browser state requires an active host-derived resource owner.",
            )
        session_id = value.get("session_id")
        instance_id = value.get("extension_instance_id")
        generation = value.get("process_generation")
        if (
            not isinstance(session_id, str)
            or not session_id
            or len(session_id.encode("utf-8")) > 256
            or not isinstance(instance_id, str)
            or not instance_id
            or len(instance_id.encode("utf-8")) > 256
            or not isinstance(generation, int)
            or isinstance(generation, bool)
            or generation < 1
            or generation > (2**64 - 1)
        ):
            raise BrowseError(
                "owner_unavailable",
                "Browser state requires a valid host-derived resource owner.",
            )
        return cls(session_id, instance_id, generation)

    @property
    def key(self) -> Tuple[str, str, int]:
        return (self.session_id, self.extension_instance_id, self.process_generation)

    def as_dict(self) -> dict[str, Any]:
        return {
            "session_id": self.session_id,
            "extension_instance_id": self.extension_instance_id,
            "process_generation": self.process_generation,
        }


class ExclusiveFileLock:
    """A non-blocking exclusive lock on a regular, non-symlink file."""

    def __init__(self, path: Path) -> None:
        self.path = path
        self.fd: Optional[int] = None

    def acquire(self) -> bool:
        if self.fd is not None:
            return True
        if fcntl is None:
            raise BrowseError(
                "locking_unavailable",
                "Ygg Browse requires Unix advisory file locking.",
            )
        flags = os.O_RDWR | os.O_CREAT
        flags |= getattr(os, "O_CLOEXEC", 0)
        flags |= getattr(os, "O_NOFOLLOW", 0)
        try:
            fd = os.open(str(self.path), flags, 0o600)
        except OSError as error:
            if error.errno in {errno.ELOOP, errno.EMLINK}:
                raise BrowseError(
                    "unsafe_lock_path", "The Ygg Browse lock path is a symbolic link."
                ) from error
            raise BrowseError("lock_failed", "The Ygg Browse lock could not be opened.") from error
        try:
            metadata = os.fstat(fd)
            if not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1:
                raise BrowseError(
                    "unsafe_lock_path", "The Ygg Browse lock must be a regular owned file."
                )
            try:
                os.fchmod(fd, 0o600)
            except OSError:
                pass
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except OSError as error:
                if error.errno in {errno.EACCES, errno.EAGAIN}:
                    os.close(fd)
                    return False
                raise
        except BaseException:
            try:
                os.close(fd)
            except OSError:
                pass
            raise
        self.fd = fd
        return True

    def release(self) -> None:
        fd, self.fd = self.fd, None
        if fd is None:
            return
        if fcntl is not None:
            try:
                fcntl.flock(fd, fcntl.LOCK_UN)
            except OSError:
                pass
        try:
            os.close(fd)
        except OSError:
            pass

    def __enter__(self) -> "ExclusiveFileLock":
        if not self.acquire():
            raise BrowseError("lock_held", "Another Ygg Browse owner holds the lock.")
        return self

    def __exit__(self, *_: Any) -> None:
        self.release()


def validate_http_url(value: Any) -> str:
    """Validate and normalize an explicit absolute HTTP(S) navigation URL."""

    if not isinstance(value, str):
        raise BrowseError("invalid_url", "Navigation requires an absolute HTTP(S) URL.")
    if (
        not value
        or len(value) > MAX_URL_CHARS
        or len(value.encode("utf-8")) > 8192
        or value != value.strip()
    ):
        raise BrowseError("invalid_url", "Navigation requires a bounded absolute HTTP(S) URL.")
    if _CONTROL_RE.search(value) or "\\" in value:
        raise BrowseError("invalid_url", "The navigation URL contains unsafe characters.")
    try:
        parsed = urlsplit(value)
        port = parsed.port
    except ValueError as error:
        raise BrowseError("invalid_url", "The navigation URL is malformed.") from error
    if parsed.scheme.lower() not in {"http", "https"} or not parsed.netloc:
        raise BrowseError("invalid_url", "Only explicit absolute HTTP(S) URLs are allowed.")
    if parsed.username is not None or parsed.password is not None:
        raise BrowseError("invalid_url", "URLs containing credentials are not allowed.")
    hostname = parsed.hostname
    if (
        not hostname
        or not hostname.isascii()
        or _CONTROL_RE.search(hostname)
        or any(character.isspace() for character in hostname)
        or re.fullmatch(r"[A-Za-z0-9.:-]+", hostname) is None
    ):
        raise BrowseError("invalid_url", "The navigation URL has an invalid host.")
    if parsed.scheme != parsed.scheme.lower():
        parsed = parsed._replace(scheme=parsed.scheme.lower())
    host = _format_host(hostname.lower())
    if port is not None:
        host = f"{host}:{port}"
    normalized = SplitResult(parsed.scheme, host, parsed.path or "/", parsed.query, parsed.fragment)
    return urlunsplit(normalized)


def sanitize_url(value: Any, *, origin_only: bool = False) -> str:
    """Remove credentials, query/fragment data, controls, and excessive length."""

    if not isinstance(value, str) or not value:
        return "unavailable"
    cleaned = _CONTROL_RE.sub("", value).strip()
    try:
        parsed = urlsplit(cleaned)
        port = parsed.port
    except ValueError:
        return "unavailable"
    if parsed.scheme.lower() not in {"http", "https"} or not parsed.hostname:
        return "blank" if cleaned == "about:blank" else "unavailable"
    host = _format_host(parsed.hostname.lower())
    if port is not None:
        host = f"{host}:{port}"
    path = "" if origin_only else (parsed.path or "/")
    path = bounded_text(path, 384, collapse_whitespace=False)
    return bounded_text(urlunsplit((parsed.scheme.lower(), host, path, "", "")), 512)


def url_origin(value: Any) -> str:
    return sanitize_url(value, origin_only=True)


def bounded_text(
    value: Any,
    limit: int,
    *,
    collapse_whitespace: bool = True,
    suffix: str = "…",
) -> str:
    text = str(value)
    text = "".join(
        " " if unicodedata.category(character).startswith("C") else character
        for character in text
    )
    if collapse_whitespace:
        text = _WHITESPACE_RE.sub(" ", text).strip()
    if len(text) <= limit:
        return text
    if limit <= len(suffix):
        return suffix[:limit]
    return text[: limit - len(suffix)] + suffix


def valid_tab_id(value: Any) -> bool:
    return isinstance(value, str) and _TAB_ID_RE.fullmatch(value) is not None


def require_string(
    arguments: Mapping[str, Any],
    name: str,
    *,
    minimum: int = 1,
    maximum: int,
) -> str:
    value = arguments.get(name)
    if not isinstance(value, str) or not minimum <= len(value) <= maximum:
        raise BrowseError("invalid_arguments", f"{name} must be a bounded string.")
    if _CONTROL_RE.search(value) and name not in {"text"}:
        raise BrowseError("invalid_arguments", f"{name} contains control characters.")
    return value


def require_integer(
    arguments: Mapping[str, Any],
    name: str,
    *,
    minimum: int,
    maximum: int,
    default: Optional[int] = None,
) -> int:
    value = arguments.get(name, default)
    if (
        not isinstance(value, int)
        or isinstance(value, bool)
        or not minimum <= value <= maximum
    ):
        raise BrowseError(
            "invalid_arguments", f"{name} must be between {minimum} and {maximum}."
        )
    return value


def _format_host(hostname: str) -> str:
    return f"[{hostname}]" if ":" in hostname and not hostname.startswith("[") else hostname
