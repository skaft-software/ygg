"""Strict unique semantic target parsing and safety classification."""

from __future__ import annotations

import re
import unicodedata
from dataclasses import dataclass
from typing import Any, Dict, List, Mapping, Optional

from .safety import BrowseError, MAX_TARGET_CHARS, bounded_text


ROLE_ALLOWLIST = {
    "button",
    "link",
    "textbox",
    "checkbox",
    "radio",
    "combobox",
    "option",
    "tab",
    "menuitem",
    "switch",
    "searchbox",
    "spinbutton",
}
_ROLE_RE = re.compile(
    r"^role=(?P<role>[a-z][a-z0-9_-]*)(?:\[name=(?P<quote>[\"'])(?P<name>.*)(?P=quote)\])?$",
    re.DOTALL,
)
_REF_RE = re.compile(r"^(?:ref=)?(?P<ref>e[1-9][0-9]{0,3})$")
_CREDENTIAL_RE = re.compile(
    r"\b(password|passcode|one[ -]?time|otp|authentication|verification[ -]?code|security[ -]?code|"
    r"username|user[ -]?name|sign[ -]?in|log[ -]?in|login|credential|credit[ -]?card|debit[ -]?card|"
    r"card[ -]?number|cvv|cvc|payment|billing|checkout)\b",
    re.IGNORECASE,
)
_CONSEQUENTIAL_RE = re.compile(
    r"\b(buy|purchase|place[ -]?order|pay|checkout|send|publish|post|submit|save|create|update|"
    r"delete|remove|erase|grant|authorize|consent|accept|agree|confirm[ -]?order|unsubscribe|"
    r"transfer|donate|book|register|subscribe|sign[ -]?up|follow|like|vote)\b",
    re.IGNORECASE,
)
_DESTRUCTIVE_RE = re.compile(
    r"\b(delete|remove|erase|purchase|buy|pay|place[ -]?order|transfer|unsubscribe)\b",
    re.IGNORECASE,
)


@dataclass(frozen=True)
class TargetQuery:
    kind: str
    value: str
    role: Optional[str] = None
    name: Optional[str] = None


@dataclass
class TargetMetadata:
    role: str
    name: str
    href: Optional[str]
    form_action: Optional[str]
    credential_like: bool
    consequential: bool
    destructive: bool
    in_form: bool
    fillable: bool
    manual_value_possible: bool


@dataclass
class SnapshotReference:
    handle: Any
    metadata: TargetMetadata

    def dispose(self) -> None:
        try:
            self.handle.dispose()
        except Exception:
            pass


@dataclass
class ResolvedTarget:
    target: Any
    metadata: TargetMetadata


class TargetingError(BrowseError):
    pass


def parse_target(value: Any) -> TargetQuery:
    if not isinstance(value, str) or not value or len(value) > MAX_TARGET_CHARS:
        raise TargetingError("invalid_target", "The target must be a bounded non-empty string.")
    if any(unicodedata.category(character).startswith("C") for character in value):
        raise TargetingError("invalid_target", "The target contains control characters.")
    reference = _REF_RE.fullmatch(value)
    if reference:
        return TargetQuery("ref", reference.group("ref"))
    role = _ROLE_RE.fullmatch(value)
    if role:
        role_name = role.group("role")
        if role_name not in ROLE_ALLOWLIST:
            raise TargetingError(
                "invalid_target", "The requested semantic role is not in the documented allowlist."
            )
        name = role.group("name")
        if name is not None and (not name or len(name) > 256):
            raise TargetingError("invalid_target", "The role name must be between 1 and 256 characters.")
        return TargetQuery("role", value, role=role_name, name=name)
    if value.startswith("text="):
        text = value[5:]
        if not text:
            raise TargetingError("invalid_target", "text= requires exact semantic text.")
        return TargetQuery("text", text)
    if value.startswith("css="):
        selector = value[4:]
        if not selector:
            raise TargetingError("invalid_target", "css= requires a selector.")
        return TargetQuery("css", selector)
    if "=" in value and value.split("=", 1)[0] in {"xpath", "js", "ref", "role"}:
        raise TargetingError("invalid_target", "The target syntax is unsupported or malformed.")
    return TargetQuery("text", value)


def resolve_target(
    page: Any,
    tab: Any,
    value: Any,
    snapshot_generation: Any,
) -> ResolvedTarget:
    query = parse_target(value)
    if query.kind == "ref":
        if (
            not isinstance(snapshot_generation, int)
            or isinstance(snapshot_generation, bool)
            or snapshot_generation < 0
            or snapshot_generation > (2**53) - 1
        ):
            raise TargetingError(
                "snapshot_generation_required",
                "Reference targets require the matching snapshot_generation.",
            )
        if snapshot_generation != tab.generation:
            raise TargetingError(
                "stale_snapshot",
                "The snapshot generation is stale; take a new browser_snapshot.",
            )
        reference = tab.references.get(query.value)
        if reference is None:
            raise TargetingError(
                "stale_reference",
                "The element reference is absent or stale; take a new browser_snapshot.",
            )
        try:
            if hasattr(reference.handle, "is_visible") and not reference.handle.is_visible():
                raise TargetingError(
                    "stale_reference",
                    "The referenced element is no longer visible; take a new browser_snapshot.",
                )
        except TargetingError:
            raise
        except Exception as error:
            raise TargetingError(
                "stale_reference",
                "The referenced element is detached; take a new browser_snapshot.",
            ) from error
        return ResolvedTarget(reference.handle, reference.metadata)

    try:
        if query.kind == "role":
            if query.name is None:
                locator = page.get_by_role(query.role)
            else:
                locator = page.get_by_role(query.role, name=query.name, exact=True)
        elif query.kind == "css":
            locator = page.locator("css=" + query.value)
        else:
            locator = page.get_by_text(query.value, exact=True)
        count = locator.count()
    except Exception as error:
        raise TargetingError("invalid_target", "The semantic target could not be resolved.") from error
    if count == 0:
        raise TargetingError(
            "target_missing",
            "No visible element uniquely matches the target; take a new browser_snapshot.",
        )
    if count > 1:
        candidates = _candidate_guidance(locator, min(count, 5), getattr(tab, "redact", str))
        raise TargetingError(
            "target_ambiguous",
            f"The target matches {min(count, 1000)} elements; refine role=, text=, css=, or use a snapshot ref.",
            untrusted_detail=candidates,
        )
    target = locator.nth(0)
    try:
        if not target.is_visible():
            raise TargetingError(
                "target_missing", "The uniquely matched element is not visible; take a new snapshot."
            )
    except TargetingError:
        raise
    except Exception as error:
        raise TargetingError("target_missing", "The matched element is unavailable.") from error
    return ResolvedTarget(target, inspect_target(target, role_hint=query.role))


def inspect_target(
    target: Any,
    *,
    role_hint: Optional[str] = None,
    value_control_hint: bool = False,
) -> TargetMetadata:
    attributes: Dict[str, str] = {}
    for name in (
        "type",
        "autocomplete",
        "aria-label",
        "aria-description",
        "placeholder",
        "name",
        "id",
        "title",
        "role",
        "href",
        "target",
        "formaction",
        "contenteditable",
        "aria-labelledby",
    ):
        try:
            value = target.get_attribute(name)
        except Exception:
            value = None
        if isinstance(value, str):
            attributes[name] = bounded_text(value, 256)
    tag = ""
    try:
        value = target.evaluate("(element) => element.tagName.toLowerCase()")
        if isinstance(value, str):
            tag = bounded_text(value.lower(), 32)
    except Exception:
        pass
    text = ""
    input_type = attributes.get("type", "").lower()
    contenteditable = attributes.get("contenteditable", "").lower() in {
        "true",
        "plaintext-only",
    }
    value_bearing_control = (
        value_control_hint
        or tag in {"input", "textarea"}
        or contenteditable
        or (
            not tag
            and input_type
            in {"text", "search", "email", "url", "tel", "number", "password"}
        )
    )
    if not value_bearing_control:
        try:
            text = bounded_text(target.inner_text(), 256)
        except Exception:
            pass
    name = attributes.get("aria-label") or text or attributes.get("placeholder") or attributes.get("title")
    name = bounded_text(name or "unnamed", 160)
    if contenteditable:
        name = "editable field (manual value withheld)"
    native_role = _role_from_attributes(attributes, tag)
    role = attributes.get("role") or native_role or role_hint or "control"

    surrounding = ""
    form_action: Optional[str] = attributes.get("formaction")
    form_method = ""
    in_form = False
    try:
        ancestor = target.locator("xpath=ancestor::form[1]")
        if ancestor.count() == 1:
            in_form = True
            surrounding = bounded_text(ancestor.inner_text(), 1024)
            if not form_action:
                form_action = ancestor.get_attribute("action")
            form_method = ancestor.get_attribute("method") or ""
            surrounding += " " + bounded_text(form_method, 16)
    except Exception:
        # ElementHandle references do not expose Locator.locator in all
        # Playwright releases. Snapshot-time metadata remains authoritative.
        pass
    try:
        parent = target.locator("xpath=..")
        if parent.count() == 1:
            surrounding += " " + bounded_text(parent.inner_text(), 512)
    except Exception:
        pass

    semantic_blob = " ".join(
        [
            attributes.get("type", ""),
            attributes.get("autocomplete", ""),
            attributes.get("aria-label", ""),
            attributes.get("aria-description", ""),
            attributes.get("aria-labelledby", ""),
            attributes.get("placeholder", ""),
            attributes.get("name", ""),
            attributes.get("id", ""),
            attributes.get("title", ""),
            text,
            surrounding,
        ]
    )
    autocomplete = attributes.get("autocomplete", "").lower().split()
    credential_autocomplete = any(
        token in {"username", "current-password", "new-password", "one-time-code"}
        or token.startswith("cc-")
        for token in autocomplete
    )
    credential_like = (
        attributes.get("type", "").lower() == "password"
        or credential_autocomplete
        or bool(_CREDENTIAL_RE.search(semantic_blob))
    )
    submit_like = in_form and (
        input_type in {"submit", "image"}
        or (tag == "button" and input_type not in {"button", "reset"})
    )
    # Fake/test locators and some custom wrappers do not expose tagName; retain
    # the conservative semantic-button fallback for those cases.
    if in_form and not tag and input_type == "" and role == "button":
        submit_like = True
    consequential = submit_like or bool(_CONSEQUENTIAL_RE.search(semantic_blob))
    destructive = bool(_DESTRUCTIVE_RE.search(semantic_blob))
    text_input_types = {"", "text", "search", "email", "url", "tel", "number", "password"}
    fillable = (
        (tag == "textarea")
        or (tag == "input" and input_type in text_input_types)
        or (not tag and role in {"textbox", "searchbox", "spinbutton"} and input_type in text_input_types)
    ) and not contenteditable
    return TargetMetadata(
        role=bounded_text(role, 32),
        name=name,
        href=attributes.get("href"),
        form_action=bounded_text(form_action, 1024, collapse_whitespace=False) if form_action else None,
        credential_like=credential_like,
        consequential=consequential,
        destructive=destructive,
        in_form=in_form,
        fillable=fillable,
        manual_value_possible=contenteditable,
    )


def _candidate_guidance(locator: Any, count: int, redact: Any) -> str:
    lines: List[str] = []
    for index in range(count):
        candidate = locator.nth(index)
        try:
            if not candidate.is_visible():
                continue
        except Exception:
            continue
        metadata = inspect_target(candidate)
        candidate_name = (
            "editable field (manual value withheld)"
            if metadata.manual_value_possible
            else bounded_text(redact(metadata.name), 160)
        )
        lines.append(
            f"- candidate {index + 1}: role={metadata.role}, name={candidate_name}"
        )
    return "\n".join(lines[:5]) or "Candidate labels were unavailable."


def _role_from_attributes(attributes: Mapping[str, str], tag: str) -> Optional[str]:
    kind = attributes.get("type", "").lower()
    if tag == "a" and attributes.get("href") is not None:
        return "link"
    if tag == "button" or kind in {"button", "submit", "reset", "image"}:
        return "button"
    if tag == "select":
        return "combobox"
    if tag == "textarea":
        return "textbox"
    if tag == "input" and kind in {"checkbox", "radio"}:
        return kind
    if tag == "input" and kind == "search":
        return "searchbox"
    if tag == "input" and kind == "number":
        return "spinbutton"
    if tag == "input" and kind in {"email", "tel", "text", "url", "password", ""}:
        return "textbox"
    # Tests and non-DOM adapters may expose attributes without tagName.
    if not tag:
        if kind in {"checkbox", "radio"}:
            return kind
        if kind in {"button", "submit", "reset", "image"}:
            return "button"
        if kind in {"email", "number", "search", "tel", "text", "url", "password"}:
            return "textbox"
    return None
