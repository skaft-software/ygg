from __future__ import annotations

import os
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest.mock import patch

from ygg_browse.paths import BrowsePaths
from ygg_browse.safety import BrowseError, ResourceOwner
from ygg_browse.worker import BrowserEngine, MAX_TABS, OperationContext, PlaywrightWorker

from tests.helpers import FakeElement, FakePage


class RecordingEngine:
    def __init__(self) -> None:
        self.thread_ids = []
        self.active = 0
        self.overlap = False
        self.lock = threading.Lock()
        self.effects = 0
        self.shutdown_called = False

    def record(self, operation: OperationContext, value: int, delay: float = 0.02) -> int:
        operation.check()
        with self.lock:
            self.active += 1
            if self.active > 1:
                self.overlap = True
        self.thread_ids.append(threading.get_ident())
        time.sleep(delay)
        operation.check()
        self.effects += 1
        with self.lock:
            self.active -= 1
        return value

    def shutdown(self) -> None:
        self.shutdown_called = True


class PlaywrightWorkerTests(unittest.TestCase):
    def test_every_operation_is_serialized_on_one_owner_thread(self) -> None:
        engine = RecordingEngine()
        worker = PlaywrightWorker(lambda: engine, capacity=8)
        outputs = []
        threads = [
            threading.Thread(target=lambda value=value: outputs.append(worker.call("record", value)))
            for value in range(6)
        ]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(3)
        self.assertCountEqual(outputs, range(6))
        self.assertFalse(engine.overlap)
        self.assertEqual(len(set(engine.thread_ids)), 1)
        self.assertNotEqual(engine.thread_ids[0], threading.get_ident())
        worker.shutdown()
        self.assertTrue(engine.shutdown_called)

    def test_timeout_abandons_queued_side_effect_before_it_runs(self) -> None:
        engine = RecordingEngine()
        worker = PlaywrightWorker(lambda: engine, capacity=2)
        errors = []

        blocking = threading.Thread(
            target=lambda: worker.call("record", 1, delay=0.25, timeout=1)
        )
        blocking.start()
        time.sleep(0.03)
        with self.assertRaises(BrowseError) as raised:
            worker.call("record", 2, timeout=0.05)
        self.assertEqual(raised.exception.code, "operation_timeout")
        blocking.join(2)
        time.sleep(0.05)
        self.assertEqual(engine.effects, 1)
        worker.shutdown()


class FakeContext:
    def __init__(self, pages: list[FakePage]):
        self.pages = pages


class BrowserActionSafetyTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary_home = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary_home.cleanup)
        environment = patch.dict(os.environ, {"YGG_BROWSE_ALLOW_FORM_SCREENSHOTS": ""})
        environment.start()
        self.addCleanup(environment.stop)

        # setup/profile are not used by already-open fake-engine actions.
        self.engine = BrowserEngine.__new__(BrowserEngine)
        self.engine.paths = BrowsePaths.for_home(Path(self.temporary_home.name))
        self.engine.setup = None
        self.engine.profiles = None
        self.engine._tab_id_factory = lambda: "tab_fixture"
        self.engine._playwright = object()
        self.engine._profile_lease = None
        self.owner = ResourceOwner("session", "instance", 1)
        self.engine._owner = self.owner.key
        self.engine._tabs = {}
        self.engine._page_ids = {}
        self.engine._allowed_blank_pages = set()
        self.engine._selected_tab_id = None
        self.engine._download_events = 0
        self.engine._blocked_navigation = False
        self.engine._degraded = False

    def _attach(self, page: FakePage) -> str:
        self.engine._context = FakeContext([page])
        tab = self.engine._register_page(page)
        return tab.tab_id

    @staticmethod
    def operation() -> OperationContext:
        return OperationContext(time.monotonic() + 2)

    def test_type_refuses_credentials_and_never_echoes_safe_value(self) -> None:
        page = FakePage()
        password = FakeElement(
            attrs={"type": "password", "aria-label": "Password"}
        )
        safe = FakeElement(attrs={"type": "text", "aria-label": "Search"})
        page.role_elements["textbox"] = [password]
        tab_id = self._attach(page)
        with self.assertRaises(BrowseError) as raised:
            self.engine.type_text(
                self.operation(), self.owner, tab_id, 'role=textbox[name="Password"]', None, "top-secret"
            )
        self.assertEqual(raised.exception.code, "manual_auth_required")
        self.assertEqual(password.filled, [])
        self.assertNotIn("top-secret", raised.exception.message)

        page.role_elements["textbox"] = [safe]
        result = self.engine.type_text(
            self.operation(), self.owner, tab_id, 'role=textbox[name="Search"]', None, "private-query"
        )
        self.assertEqual(safe.filled, ["private-query"])
        self.assertNotIn("private-query", result["text"])
        self.assertFalse(result["value_echoed"])
        with self.assertRaises(BrowseError) as screenshot:
            self.engine.screenshot(self.operation(), self.owner, tab_id)
        self.assertEqual(screenshot.exception.code, "screenshot_typed_values")

    def test_screenshot_refuses_any_visible_editable_field(self) -> None:
        page = FakePage()
        search = FakeElement(attrs={"type": "text", "aria-label": "Search"})
        page.selector_elements['css=input:not([type="hidden"]), textarea, [contenteditable="true"]'] = [search]
        tab_id = self._attach(page)
        with self.assertRaises(BrowseError) as raised:
            self.engine.screenshot(self.operation(), self.owner, tab_id)
        self.assertEqual(raised.exception.code, "screenshot_form_values")

    def test_screenshot_form_override_preserves_hard_blocks(self) -> None:
        self.engine.paths.ensure_root()
        (self.engine.paths.root / "allow-form-screenshots").touch()
        selector = 'css=input:not([type="hidden"]), textarea, [contenteditable="true"]'
        page = FakePage()
        page.selector_elements[selector] = [
            FakeElement(attrs={"type": "text", "aria-label": "Search"})
        ]
        page.screenshot = lambda **_: b"fixture-png"
        tab_id = self._attach(page)

        result = self.engine.screenshot(self.operation(), self.owner, tab_id)
        self.assertEqual(result["data"], b"fixture-png")

        page.selector_elements[selector] = [
            FakeElement(attrs={"type": "password", "aria-label": "Password"})
        ]
        with self.assertRaises(BrowseError) as credential:
            self.engine.screenshot(self.operation(), self.owner, tab_id)
        self.assertEqual(credential.exception.code, "screenshot_manual_auth")

        self.engine._tabs[tab_id].remember_typed_value("withheld")
        with self.assertRaises(BrowseError) as typed:
            self.engine.screenshot(self.operation(), self.owner, tab_id)
        self.assertEqual(typed.exception.code, "screenshot_typed_values")

    def test_screenshot_refuses_visible_manual_auth_fields(self) -> None:
        page = FakePage()
        password = FakeElement(attrs={"type": "password", "aria-label": "Password"})
        page.selector_elements['css=input:not([type="hidden"]), textarea, [contenteditable="true"]'] = [password]
        tab_id = self._attach(page)
        with self.assertRaises(BrowseError) as raised:
            self.engine.screenshot(self.operation(), self.owner, tab_id)
        self.assertEqual(raised.exception.code, "screenshot_manual_auth")

    def test_consequential_click_requires_confirmation(self) -> None:
        form = FakeElement("Publish post", attrs={"method": "post", "action": "/publish"})
        button = FakeElement("Publish", attrs={"type": "submit", "aria-label": "Publish"}, form=form)
        page = FakePage()
        page.role_elements["button"] = [button]
        tab_id = self._attach(page)
        with self.assertRaises(BrowseError) as raised:
            self.engine.click(
                self.operation(),
                self.owner,
                tab_id,
                'role=button[name="Publish"]',
                None,
                lambda *_: False,
            )
        self.assertEqual(raised.exception.code, "confirmation_denied")
        self.assertEqual(button.clicked, 0)
        result = self.engine.click(
            self.operation(),
            self.owner,
            tab_id,
            'role=button[name="Publish"]',
            None,
            lambda *_: True,
        )
        self.assertEqual(button.clicked, 1)
        self.assertEqual(result["affected_tab_id"], tab_id)

    def test_invalid_link_scheme_is_blocked_before_click(self) -> None:
        link = FakeElement("Local file", attrs={"href": "file:///etc/passwd", "aria-label": "Local file"})
        page = FakePage()
        page.role_elements["link"] = [link]
        tab_id = self._attach(page)
        with self.assertRaises(BrowseError) as raised:
            self.engine.click(
                self.operation(),
                self.owner,
                tab_id,
                'role=link[name="Local file"]',
                None,
                lambda *_: True,
            )
        self.assertEqual(raised.exception.code, "navigation_blocked")
        self.assertEqual(link.clicked, 0)

    def test_tab_count_is_bounded_and_excess_page_is_closed(self) -> None:
        first = FakePage()
        self._attach(first)
        ids = iter(f"tab_{index}" for index in range(1, MAX_TABS + 2))
        self.engine._tab_id_factory = lambda: next(ids)
        for index in range(1, MAX_TABS):
            self.engine._register_page(FakePage(title=str(index)))
        excess = FakePage(title="excess")
        with self.assertRaises(BrowseError) as raised:
            self.engine._register_page(excess)
        self.assertEqual(raised.exception.code, "tab_limit")
        self.assertTrue(excess.is_closed())
        self.assertEqual(len(self.engine._tabs), MAX_TABS)

    def test_top_level_route_policy_blocks_non_http_and_allows_subresources(self) -> None:
        class Frame:
            def __init__(self, parent):
                self.parent_frame = parent

        class Request:
            def __init__(self, url: str, parent=None):
                self.url = url
                self.frame = Frame(parent)

            def is_navigation_request(self):
                return True

        class Route:
            def __init__(self):
                self.continued = False
                self.aborted = False

            def continue_(self):
                self.continued = True

            def abort(self, _reason):
                self.aborted = True

        blocked = Route()
        self.engine._route_request(blocked, Request("file:///etc/passwd"))
        self.assertTrue(blocked.aborted)
        self.assertFalse(blocked.continued)
        allowed = Route()
        self.engine._route_request(allowed, Request("https://example.test/"))
        self.assertTrue(allowed.continued)
        subresource = Route()
        self.engine._route_request(subresource, Request("data:image/png,x", parent=Frame(None)))
        self.assertTrue(subresource.continued)

    def test_context_failure_closes_state_and_reports_degraded(self) -> None:
        class BrokenContext:
            @property
            def pages(self):
                raise RuntimeError("browser crashed")

            def close(self):
                pass

        self.engine._context = BrokenContext()
        status = self.engine.status(self.operation(), self.owner)
        self.assertFalse(status["open"])
        self.assertTrue(status["degraded"])
        self.assertEqual(status["tabs"], [])

    def test_owner_mismatch_cannot_enumerate_or_operate_tabs(self) -> None:
        page = FakePage()
        tab_id = self._attach(page)
        other = ResourceOwner("other", "instance", 1)
        status = self.engine.status(self.operation(), other)
        self.assertTrue(status["open"])
        self.assertFalse(status["owner_matches"])
        self.assertEqual(status["tabs"], [])
        unscoped = self.engine.status(self.operation(), None)
        self.assertFalse(unscoped["owner_matches"])
        self.assertEqual(unscoped["tabs"], [])
        with self.assertRaises(BrowseError) as raised:
            self.engine.tabs(self.operation(), other)
        self.assertEqual(raised.exception.code, "owner_mismatch")


if __name__ == "__main__":
    unittest.main()
