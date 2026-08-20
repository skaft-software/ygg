"""Explicit, bounded loading of provider-owned credential environment files."""

from __future__ import annotations

import os
from pathlib import Path
import re
import stat
from typing import Dict


MAX_PROVIDER_ENV_BYTES = 64 * 1024
MAX_PROVIDER_ENV_ENTRIES = 128
MAX_PROVIDER_ENV_VALUE_BYTES = 16 * 1024
_NAME = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")
_FORBIDDEN = frozenset({"PATH", "HOME", "USER", "SHELL", "TMPDIR", "SYSTEMROOT"})
_FORBIDDEN_PREFIXES = ("YGG_", "PYTHON", "LD_", "DYLD_")


class ProviderEnvironmentError(ValueError):
    """The explicitly configured provider credential file is unsafe."""


def read_provider_environment(path: Path) -> Dict[str, str]:
    """Read a private dotenv-like file without expansion, imports, or logging."""

    try:
        metadata = path.lstat()
    except OSError as error:
        raise ProviderEnvironmentError("provider_environment_unavailable") from error
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISREG(metadata.st_mode):
        raise ProviderEnvironmentError("provider_environment_not_regular")
    if metadata.st_size > MAX_PROVIDER_ENV_BYTES:
        raise ProviderEnvironmentError("provider_environment_too_large")
    if hasattr(os, "getuid") and metadata.st_uid != os.getuid():
        raise ProviderEnvironmentError("provider_environment_wrong_owner")
    if metadata.st_mode & (stat.S_IRWXG | stat.S_IRWXO):
        raise ProviderEnvironmentError("provider_environment_permissions")
    try:
        raw = path.read_bytes()
        text = raw.decode("utf-8")
    except (OSError, UnicodeDecodeError) as error:
        raise ProviderEnvironmentError("provider_environment_invalid_utf8") from error
    if len(raw) > MAX_PROVIDER_ENV_BYTES or "\x00" in text:
        raise ProviderEnvironmentError("provider_environment_invalid")

    result: Dict[str, str] = {}
    for line_number, original in enumerate(text.splitlines(), start=1):
        if line_number > 512:
            raise ProviderEnvironmentError("provider_environment_too_many_lines")
        line = original.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("export "):
            line = line[7:].lstrip()
        if "=" not in line:
            raise ProviderEnvironmentError("provider_environment_invalid_line")
        name, raw_value = line.split("=", 1)
        name = name.strip()
        if (
            not _NAME.fullmatch(name)
            or name in _FORBIDDEN
            or name.startswith(_FORBIDDEN_PREFIXES)
        ):
            raise ProviderEnvironmentError("provider_environment_name_forbidden")
        if name in result:
            raise ProviderEnvironmentError("provider_environment_duplicate_name")
        value = _parse_value(raw_value.strip())
        if len(value.encode("utf-8")) > MAX_PROVIDER_ENV_VALUE_BYTES:
            raise ProviderEnvironmentError("provider_environment_value_too_large")
        result[name] = value
        if len(result) > MAX_PROVIDER_ENV_ENTRIES:
            raise ProviderEnvironmentError("provider_environment_too_many_entries")
    return result


def _parse_value(value: str) -> str:
    if not value:
        return ""
    if value[0] in {"'", '"'}:
        quote = value[0]
        if len(value) < 2 or value[-1] != quote:
            raise ProviderEnvironmentError("provider_environment_unterminated_quote")
        value = value[1:-1]
        if quote == '"':
            value = (
                value.replace(r"\n", "\n")
                .replace(r"\r", "\r")
                .replace(r"\t", "\t")
                .replace(r'\"', '"')
                .replace(r"\\", "\\")
            )
    else:
        marker = value.find(" #")
        if marker >= 0:
            value = value[:marker].rstrip()
    if "\x00" in value:
        raise ProviderEnvironmentError("provider_environment_invalid_value")
    return value
