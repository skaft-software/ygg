"""Secret redaction for captured Harbor evidence."""

from __future__ import annotations

import json
import re
from collections.abc import Iterable
from typing import Any

_ASSIGNMENT_RE = re.compile(
    r"(?ix)"
    r"(\b(?:api[_-]?key|auth(?:orization)?|access[_-]?token|secret|password)\b"
    r"\s*[:=]\s*(?:bearer\s+)?)"
    r"([^\s,;\"'}]+)"
)
_BEARER_RE = re.compile(r"(?i)(\bBearer\s+)([^\s,;\"'}]+)")
_KNOWN_TOKEN_RE = re.compile(
    r"\b(?:sk-[A-Za-z0-9_-]{12,}|sk-ant-[A-Za-z0-9_-]{12,}|"
    r"gh[pousr]_[A-Za-z0-9_]{12,}|github_pat_[A-Za-z0-9_]{12,}|"
    r"xox[baprs]-[A-Za-z0-9-]{12,}|AKIA[0-9A-Z]{16})\b"
)


def redact_text(text: str | None, secrets: Iterable[str] = ()) -> str:
    """Redact configured credentials and common credential-shaped values.

    This deliberately preserves the surrounding output and line count. It is
    evidence redaction, not output truncation; native Ygg session files are
    redacted structurally so they remain valid JSONL artifacts.
    """

    if not text:
        return ""

    redacted = text
    configured = sorted(
        {secret for secret in secrets if isinstance(secret, str) and len(secret) >= 4},
        key=len,
        reverse=True,
    )
    for secret in configured:
        redacted = redacted.replace(secret, "<redacted>")

    redacted = _ASSIGNMENT_RE.sub(r"\1<redacted>", redacted)
    redacted = _BEARER_RE.sub(r"\1<redacted>", redacted)
    return _KNOWN_TOKEN_RE.sub("<redacted>", redacted)


def redact_json_value(value: Any, secrets: Iterable[str] = ()) -> Any:
    """Return a JSON-compatible value with every string redacted."""

    normalized_secrets = tuple(secrets)
    if isinstance(value, str):
        return redact_text(value, normalized_secrets)
    if isinstance(value, list):
        return [redact_json_value(item, normalized_secrets) for item in value]
    if isinstance(value, dict):
        return {
            key: redact_json_value(item, normalized_secrets)
            for key, item in value.items()
        }
    return value


def redact_jsonl(text: str, secrets: Iterable[str] = ()) -> str:
    """Redact JSONL records while preserving malformed/torn lines verbatim-ish."""

    normalized_secrets = tuple(secrets)
    redacted_lines: list[str] = []
    for line in text.splitlines(keepends=True):
        newline = ""
        payload = line
        if payload.endswith("\r\n"):
            payload, newline = payload[:-2], "\r\n"
        elif payload.endswith(("\n", "\r")):
            payload, newline = payload[:-1], payload[-1]
        try:
            value = json.loads(payload)
        except json.JSONDecodeError:
            redacted_lines.append(redact_text(payload, normalized_secrets) + newline)
        else:
            redacted_lines.append(
                json.dumps(
                    redact_json_value(value, normalized_secrets),
                    ensure_ascii=False,
                    separators=(",", ":"),
                )
                + newline
            )
    if not redacted_lines and text:
        return redact_text(text, normalized_secrets)
    return "".join(redacted_lines)
