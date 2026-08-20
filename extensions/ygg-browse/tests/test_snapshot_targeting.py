from __future__ import annotations

import unittest

from ygg_browse.safety import BrowseError
from ygg_browse.snapshot import (
    MAX_INTERACTIVE_ELEMENTS,
    MAX_SNAPSHOT_CHARS,
    MAX_SNAPSHOT_GENERATION,
    TabState,
    snapshot_page,
)
from ygg_browse.targeting import TargetingError, inspect_target, parse_target, resolve_target

from tests.helpers import FakeElement, FakePage


class SnapshotTests(unittest.TestCase):
    def test_snapshot_is_bounded_marked_and_never_reads_input_value(self) -> None:
        page = FakePage(
            body="Visible body text\n" + ("x" * 25_000),
            title="Fixture title",
            url="https://example.test/path?token=secret#frag",
        )
        button = FakeElement("Save", attrs={"aria-label": "Save"})
        password = FakeElement(
            "",
            attrs={
                "type": "password",
                "aria-label": "Password",
                "value": "typed-secret-never-read",
            },
        )
        page.selector_elements["button"] = [button]
        page.selector_elements['input:not([type="hidden"])'] = [password]
        tab = TabState("tab_opaque", page)
        result = snapshot_page(tab)
        self.assertIn("BEGIN UNTRUSTED BROWSER CONTENT", result.text)
        self.assertIn("END UNTRUSTED BROWSER CONTENT", result.text)
        self.assertIn("snapshot_generation=1", result.text)
        self.assertIn("[ref=e1]", result.text)
        self.assertIn("manual credential field", result.text)
        self.assertNotIn("typed-secret-never-read", result.text)
        self.assertNotIn("token=secret", result.text)
        self.assertLessEqual(len(result.text), MAX_SNAPSHOT_CHARS)
        self.assertTrue(result.truncated)

    def test_contenteditable_body_text_is_omitted_as_possible_manual_input(self) -> None:
        page = FakePage(body="manually typed editable secret")
        editable = FakeElement(
            "manually typed editable secret",
            attrs={"contenteditable": "true", "role": "textbox"},
        )
        page.selector_elements['css=textarea, [contenteditable="true"], [contenteditable="plaintext-only"]'] = [editable]
        page.selector_elements['[role="textbox"]:not(input):not(textarea)'] = [editable]
        result = snapshot_page(TabState("tab_editable", page))
        self.assertNotIn("manually typed editable secret", result.text)
        self.assertIn("editable field (manual value withheld)", result.text)
        self.assertIn("editable content could contain manually entered values", result.text)

    def test_textarea_initial_or_manual_text_is_not_exposed(self) -> None:
        page = FakePage(body="textarea secret value")
        textarea = FakeElement("textarea secret value", attrs={"placeholder": "Notes"})
        page.selector_elements["textarea"] = [textarea]
        page.selector_elements['css=textarea, [contenteditable="true"], [contenteditable="plaintext-only"]'] = [textarea]
        result = snapshot_page(TabState("tab_textarea", page))
        self.assertNotIn("textarea secret value", result.text)
        self.assertIn("name=Notes", result.text)

    def test_previously_typed_values_are_redacted_from_all_snapshot_text(self) -> None:
        value = "private typed phrase"
        page = FakePage(
            body=f"Page echoed {value}",
            title=f"Results for {value}",
            url="https://example.test/search/private%20typed%20phrase?q=private+typed+phrase",
        )
        button = FakeElement(value, attrs={"aria-label": value})
        page.selector_elements["button"] = [button]
        tab = TabState("tab_redact", page)
        tab.remember_typed_value(value)
        result = snapshot_page(tab)
        self.assertNotIn(value, result.text)
        self.assertNotIn("private%20typed%20phrase", result.text)
        self.assertIn("[typed value withheld]", result.text)

    def test_new_snapshot_and_navigation_invalidate_refs(self) -> None:
        page = FakePage(body="body")
        button = FakeElement("Go", attrs={"aria-label": "Go"})
        page.selector_elements["button"] = [button]
        tab = TabState("tab_a", page)
        first = snapshot_page(tab)
        resolved = resolve_target(page, tab, "ref=e1", first.generation)
        self.assertIs(resolved.target, button)
        second = snapshot_page(tab)
        self.assertTrue(button.disposed)
        with self.assertRaises(TargetingError) as raised:
            resolve_target(page, tab, "ref=e1", first.generation)
        self.assertEqual(raised.exception.code, "stale_snapshot")
        self.assertGreater(second.generation, first.generation)
        tab.invalidate()
        with self.assertRaises(TargetingError):
            resolve_target(page, tab, "ref=e1", second.generation)

    def test_snapshot_generation_stays_in_portable_json_range(self) -> None:
        tab = TabState("tab_max", FakePage(), generation=MAX_SNAPSHOT_GENERATION)
        with self.assertRaises(BrowseError) as raised:
            tab.invalidate()
        self.assertEqual(raised.exception.code, "snapshot_generation_exhausted")

    def test_interactive_elements_are_capped_at_one_hundred(self) -> None:
        page = FakePage()
        page.selector_elements["button"] = [
            FakeElement("Button " + ("x" * 200) + str(index))
            for index in range(MAX_INTERACTIVE_ELEMENTS + 25)
        ]
        result = snapshot_page(TabState("tab_many", page))
        self.assertEqual(result.element_count, MAX_INTERACTIVE_ELEMENTS)
        self.assertIn("[ref=e100]", result.text)
        self.assertIn("more than 100 interactive elements", result.text)


class TargetingTests(unittest.TestCase):
    def test_documented_syntax_and_malformed_escape_hatches(self) -> None:
        self.assertEqual(parse_target("ref=e12").kind, "ref")
        role = parse_target('role=button[name="Publish"]')
        self.assertEqual((role.kind, role.role, role.name), ("role", "button", "Publish"))
        self.assertEqual(parse_target("text=Exact").kind, "text")
        self.assertEqual(parse_target("css=.save").kind, "css")
        self.assertEqual(parse_target("Plain text").kind, "text")
        for target in ("ref=bad", "role=unknown", "xpath=//button", "js=alert(1)", ""):
            with self.subTest(target=target), self.assertRaises(TargetingError):
                parse_target(target)

    def test_unique_resolution_never_silently_uses_first(self) -> None:
        page = FakePage()
        one = FakeElement("One", attrs={"aria-label": "One"})
        two = FakeElement("Two", attrs={"aria-label": "Two"})
        page.role_elements["button"] = [one, two]
        tab = TabState("tab_x", page)
        with self.assertRaises(TargetingError) as raised:
            resolve_target(page, tab, "role=button", None)
        self.assertEqual(raised.exception.code, "target_ambiguous")
        self.assertIn("candidate 1", raised.exception.untrusted_detail or "")
        self.assertEqual(one.clicked + two.clicked, 0)
        resolved = resolve_target(page, tab, 'role=button[name="Two"]', None)
        self.assertIs(resolved.target, two)

    def test_credential_and_consequential_classification_uses_form_semantics(self) -> None:
        login_form = FakeElement(
            "Sign in securely",
            attrs={"action": "/login", "method": "post"},
        )
        username = FakeElement(
            "",
            attrs={"autocomplete": "username", "aria-label": "Account"},
            form=login_form,
        )
        self.assertTrue(inspect_target(username, role_hint="textbox").credential_like)

        payment_form = FakeElement(
            "Place order and pay",
            attrs={"action": "/purchase", "method": "post"},
        )
        submit = FakeElement(
            "Continue",
            attrs={"type": "submit"},
            form=payment_form,
        )
        metadata = inspect_target(submit, role_hint="button")
        self.assertTrue(metadata.consequential)
        self.assertTrue(metadata.destructive)

        ordinary = FakeElement("Expand details", attrs={"type": "button"})
        safe = inspect_target(ordinary, role_hint="button")
        self.assertFalse(safe.credential_like)
        self.assertFalse(safe.consequential)

        editable = FakeElement("manual draft", attrs={"contenteditable": "true", "role": "textbox"})
        editable_metadata = inspect_target(editable, role_hint="textbox")
        self.assertTrue(editable_metadata.manual_value_possible)
        self.assertFalse(editable_metadata.fillable)


if __name__ == "__main__":
    unittest.main()
