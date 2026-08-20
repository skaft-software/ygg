"""Bounded, value-redacting semantic browser snapshots and references."""

from __future__ import annotations

from dataclasses import dataclass, field
import unicodedata
from typing import Any, Dict, List, Sequence, Tuple
from urllib.parse import quote, quote_plus

from .safety import BrowseError, bounded_text, sanitize_url
from .targeting import SnapshotReference, inspect_target


MAX_SNAPSHOT_CHARS = 20_000
MAX_INTERACTIVE_ELEMENTS = 100
MAX_BODY_SOURCE_CHARS = 30_000
MAX_SNAPSHOT_GENERATION = (2**53) - 1
UNTRUSTED_BEGIN = "BEGIN UNTRUSTED BROWSER CONTENT"
UNTRUSTED_END = "END UNTRUSTED BROWSER CONTENT"

# Native selectors precede ARIA fallbacks and explicitly exclude their native
# equivalents so one element is not assigned multiple references.
INTERACTIVE_GROUPS: Sequence[Tuple[str, str]] = (
    ("a[href]", "link"),
    ("button", "button"),
    ('input:not([type="hidden"])', "textbox"),
    ("textarea", "textbox"),
    ("select", "combobox"),
    ('[role="button"]:not(button)', "button"),
    ('[role="link"]:not(a)', "link"),
    ('[role="textbox"]:not(input):not(textarea)', "textbox"),
    ('[role="searchbox"]:not(input)', "searchbox"),
    ('[role="checkbox"]:not(input)', "checkbox"),
    ('[role="radio"]:not(input)', "radio"),
    ('[role="combobox"]:not(select)', "combobox"),
    ('[role="switch"]', "switch"),
    ('[role="tab"]', "tab"),
    ('[role="menuitem"]', "menuitem"),
)


@dataclass
class TabState:
    tab_id: str
    page: Any
    generation: int = 0
    references: Dict[str, SnapshotReference] = field(default_factory=dict)
    last_url: str = "about:blank"
    title: str = ""
    _typed_values: List[str] = field(default_factory=list, repr=False)

    def invalidate(self) -> None:
        for reference in self.references.values():
            reference.dispose()
        self.references.clear()
        if self.generation >= MAX_SNAPSHOT_GENERATION:
            raise BrowseError(
                "snapshot_generation_exhausted",
                "The portable snapshot-generation range is exhausted; close and relaunch the browser.",
            )
        self.generation += 1

    def close_references(self) -> None:
        for reference in self.references.values():
            reference.dispose()
        self.references.clear()

    def remember_typed_value(self, value: str) -> None:
        if not value:
            return
        encoded_candidates = [quote(value, safe=""), quote_plus(value)]
        candidates = [item for item in encoded_candidates if len(item.encode("utf-8")) <= 16_384]
        candidates.append(value)  # Keep the exact value newest so budget pruning retains it.
        for candidate in candidates:
            if not candidate:
                continue
            self._typed_values = [item for item in self._typed_values if item != candidate]
            self._typed_values.append(candidate)
        while (
            len(self._typed_values) > 24
            or sum(len(item.encode("utf-8")) for item in self._typed_values) > 24_576
        ):
            self._typed_values.pop(0)

    @property
    def has_typed_values(self) -> bool:
        return bool(self._typed_values)

    def redact(self, value: Any) -> str:
        text = str(value)
        for typed in sorted(self._typed_values, key=len, reverse=True):
            text = text.replace(typed, "[typed value withheld]")
        return text


@dataclass(frozen=True)
class SnapshotResult:
    tab_id: str
    generation: int
    text: str
    element_count: int
    truncated: bool


def snapshot_page(tab: TabState) -> SnapshotResult:
    """Replace refs atomically and return a bounded untrusted-content envelope."""

    tab.invalidate()
    generation = tab.generation
    page = tab.page
    new_references: Dict[str, SnapshotReference] = {}
    interactive_lines: List[str] = []
    truncated_elements = False
    sequence = 1

    try:
        for selector, role_hint in INTERACTIVE_GROUPS:
            locator = page.locator(selector)
            count = min(locator.count(), MAX_INTERACTIVE_ELEMENTS + 1)
            for index in range(count):
                if len(new_references) >= MAX_INTERACTIVE_ELEMENTS:
                    truncated_elements = True
                    break
                element = locator.nth(index)
                try:
                    if not element.is_visible():
                        continue
                    handle = element.element_handle()
                    if handle is None:
                        continue
                    metadata = inspect_target(
                        element,
                        role_hint=role_hint,
                        value_control_hint=selector.startswith("input")
                        or selector == "textarea",
                    )
                    if metadata.credential_like:
                        display_name = "manual credential field"
                    elif metadata.manual_value_possible:
                        display_name = "editable field (manual value withheld)"
                    else:
                        display_name = bounded_text(tab.redact(metadata.name), 72)
                    states = _element_states(element)
                    reference_name = f"e{sequence}"
                    sequence += 1
                    new_references[reference_name] = SnapshotReference(handle, metadata)
                    suffix = f" ({', '.join(states)})" if states else ""
                    interactive_lines.append(
                        f"[ref={reference_name}] role={metadata.role} name={display_name}{suffix}"
                    )
                except Exception:
                    continue
            if truncated_elements:
                break

        body_text = ""
        editable_text_present = False
        try:
            editables = page.locator(
                'css=textarea, [contenteditable="true"], [contenteditable="plaintext-only"]'
            )
            for index in range(min(editables.count(), 100)):
                if editables.nth(index).is_visible():
                    editable_text_present = True
                    break
        except Exception:
            editable_text_present = True
        if not editable_text_present:
            try:
                body_text = page.locator("body").inner_text(timeout=3000)
            except Exception:
                body_text = ""
        # innerText excludes input/textarea value properties. Supplied values
        # are never queried back or serialized; a small in-memory redaction set
        # suppresses later page/title echoes until the tab closes.
        body_text = _sanitize_multiline(tab.redact(body_text), MAX_BODY_SOURCE_CHARS)
        title = ""
        try:
            title = bounded_text(tab.redact(page.title()), 256)
        except Exception:
            pass
        tab.title = title
        tab.last_url = str(getattr(page, "url", "about:blank"))

        content_lines = [f"URL: {tab.redact(sanitize_url(tab.last_url))}"]
        if title:
            content_lines.append(f"Title: {title}")
        # Keep every returned ref visible even when long page text is truncated;
        # an undisclosed but actionable reference would be unsafe and confusing.
        if interactive_lines:
            content_lines.extend(["", "Interactive elements:", *interactive_lines])
        if truncated_elements:
            content_lines.append(
                f"[truncated: more than {MAX_INTERACTIVE_ELEMENTS} interactive elements]"
            )
        if editable_text_present:
            content_lines.append(
                "[visible body text omitted: editable content could contain manually entered values]"
            )
        elif body_text:
            content_lines.extend(["", "Visible text:", body_text])
        untrusted = "\n".join(content_lines)
        prefix = (
            f"Browser snapshot for tab {tab.tab_id}; snapshot_generation={generation}.\n"
            f"{UNTRUSTED_BEGIN}\n"
        )
        suffix = f"\n{UNTRUSTED_END}"
        available = MAX_SNAPSHOT_CHARS - len(prefix) - len(suffix)
        truncated_text = len(untrusted) > available
        if truncated_text:
            notice = "\n[truncated: browser content exceeded the 20000-character snapshot bound]"
            untrusted = untrusted[: max(0, available - len(notice))] + notice
        text = prefix + untrusted + suffix
        tab.references = new_references
        return SnapshotResult(
            tab_id=tab.tab_id,
            generation=generation,
            text=text,
            element_count=len(new_references),
            truncated=truncated_elements or truncated_text,
        )
    except BaseException as error:
        for reference in new_references.values():
            reference.dispose()
        tab.references = {}
        if isinstance(error, BrowseError):
            raise
        raise BrowseError("snapshot_failed", "The bounded browser snapshot could not be created.") from error


def _element_states(element: Any) -> List[str]:
    states: List[str] = []
    try:
        if not element.is_enabled():
            states.append("disabled")
    except Exception:
        pass
    for attribute, label in (
        ("aria-checked", "checked"),
        ("aria-selected", "selected"),
        ("aria-expanded", "expanded"),
        ("checked", "checked"),
        ("selected", "selected"),
    ):
        try:
            value = element.get_attribute(attribute)
        except Exception:
            value = None
        if value is not None and value.lower() not in {"false", "0", "off"}:
            states.append(label)
    return list(dict.fromkeys(states))[:4]


def _sanitize_multiline(value: Any, limit: int) -> str:
    text = str(value)
    output: List[str] = []
    for character in text:
        if character in {"\n", "\t"}:
            output.append(character)
        elif unicodedata.category(character).startswith("C"):
            output.append(" ")
        else:
            output.append(character)
        if len(output) >= limit:
            break
    normalized = "".join(output)
    lines = [" ".join(line.split()) for line in normalized.splitlines()]
    return "\n".join(line for line in lines if line).strip()
