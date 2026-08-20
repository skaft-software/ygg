"""Opt-in real Playwright tests against a loopback-only HTTP fixture server."""

from __future__ import annotations

import os
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from ygg_browse.artifacts import PNG_SIGNATURE, READ_IMAGE_LIMIT
from ygg_browse.paths import BrowsePaths
from ygg_browse.profile import ProfileManager
from ygg_browse.safety import BrowseError, ResourceOwner
from ygg_browse.setup import SetupManager
from ygg_browse.worker import BrowserEngine, PlaywrightWorker


HTML = b"""<!doctype html><html><head><title>Local fixture</title></head><body>
<h1>Local fixture page</h1>
<a id="popup" href="/popup" target="_blank">Open popup</a>
<a id="download" href="/download" download>Download fixture</a>
<input aria-label="Search" type="text">
<input aria-label="Password" type="password" autocomplete="current-password">
<form method="post" action="/publish"><button aria-label="Publish" type="submit">Publish</button></form>
</body></html>"""


class Handler(BaseHTTPRequestHandler):
    def do_GET(self) -> None:
        if self.path == "/redirect":
            self.send_response(302)
            self.send_header("Location", "/final")
            self.end_headers()
            return
        if self.path == "/bad-redirect":
            self.send_response(302)
            self.send_header("Location", "file:///etc/passwd")
            self.end_headers()
            return
        if self.path == "/download":
            self.send_response(200)
            self.send_header("Content-Type", "application/octet-stream")
            self.send_header("Content-Disposition", "attachment; filename=fixture.bin")
            self.end_headers()
            self.wfile.write(b"blocked download")
            return
        body = HTML if self.path == "/" else b"<html><head><title>Second</title></head><body>Second page</body></html>"
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self) -> None:
        body = b"<html><body>Published</body></html>"
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, _format: str, *_arguments: object) -> None:
        pass


@unittest.skipUnless(
    os.environ.get("YGG_BROWSE_PLAYWRIGHT_TESTS") == "1",
    "set YGG_BROWSE_PLAYWRIGHT_TESTS=1 after /browse setup for real headful integration",
)
class PlaywrightIntegrationTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.runtime_paths = BrowsePaths.for_home()
        cls.setup = SetupManager(cls.runtime_paths)
        try:
            cls.setup.validate_runtime()
        except BrowseError as error:
            raise unittest.SkipTest(
                f"pinned runtime unavailable ({error.code}); run confirmed /browse setup first"
            )
        cls.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        cls.server_thread = threading.Thread(target=cls.server.serve_forever, daemon=True)
        cls.server_thread.start()
        cls.origin = f"http://127.0.0.1:{cls.server.server_port}"

    @classmethod
    def tearDownClass(cls) -> None:
        if hasattr(cls, "server"):
            cls.server.shutdown()
            cls.server.server_close()
            cls.server_thread.join(timeout=2)

    def test_local_navigation_tabs_refs_auth_download_screenshot_and_cleanup(self) -> None:
        with tempfile.TemporaryDirectory() as home:
            profile_paths = BrowsePaths.for_home(Path(home))
            profiles = ProfileManager(profile_paths)
            worker = PlaywrightWorker(
                lambda: BrowserEngine(self.runtime_paths, self.setup, profiles)
            )
            owner = ResourceOwner("integration-session", "integration-instance", 1)
            try:
                launch = worker.call("launch", owner, timeout=25)
                self.assertTrue(launch["open"])
                tab_id = launch["selected_tab_id"]
                self.assertIsInstance(tab_id, str)

                opened = worker.call("open_url", owner, self.origin + "/", tab_id, timeout=20)
                self.assertEqual(opened["affected_tab_id"], tab_id)
                snapshot = worker.call("snapshot", owner, tab_id)
                self.assertIn("BEGIN UNTRUSTED BROWSER CONTENT", snapshot["text"])
                self.assertIn("snapshot_generation", snapshot)
                generation = snapshot["snapshot_generation"]

                typed = worker.call(
                    "type_text",
                    owner,
                    tab_id,
                    'role=textbox[name="Search"]',
                    None,
                    "private integration value",
                )
                self.assertNotIn("private integration value", typed["text"])
                with self.assertRaises(BrowseError) as auth:
                    worker.call(
                        "type_text",
                        owner,
                        tab_id,
                        "css=input[type=password]",
                        None,
                        "never type this",
                    )
                self.assertEqual(auth.exception.code, "manual_auth_required")

                popup = worker.call(
                    "click", owner, tab_id, "css=#popup", None, lambda *_: True, timeout=15
                )
                self.assertEqual(len(popup["created_tab_ids"]), 1)
                popup_id = popup["created_tab_ids"][0]
                self.assertNotEqual(popup_id, tab_id)

                download = worker.call(
                    "click", owner, tab_id, "css=#download", None, lambda *_: True
                )
                self.assertTrue(download["download_blocked"])

                with self.assertRaises(BrowseError) as denied:
                    worker.call(
                        "click",
                        owner,
                        tab_id,
                        'role=button[name="Publish"]',
                        None,
                        lambda *_: False,
                    )
                self.assertEqual(denied.exception.code, "confirmation_denied")

                redirected = worker.call(
                    "open_url", owner, self.origin + "/redirect", tab_id, timeout=20
                )
                self.assertEqual(redirected["affected_tab_id"], tab_id)
                with self.assertRaises(BrowseError) as stale:
                    worker.call("click", owner, tab_id, "ref=e1", generation, lambda *_: True)
                self.assertIn(stale.exception.code, {"stale_snapshot", "stale_reference"})

                with self.assertRaises(BrowseError) as blocked:
                    worker.call(
                        "open_url", owner, self.origin + "/bad-redirect", tab_id, timeout=20
                    )
                self.assertIn(blocked.exception.code, {"navigation_blocked", "navigation_failed"})

                screenshot = worker.call("screenshot", owner, popup_id)
                self.assertTrue(screenshot["data"].startswith(PNG_SIGNATURE))
                self.assertLess(len(screenshot["data"]), READ_IMAGE_LIMIT)

                closed = worker.call("close_tab", owner, popup_id)
                self.assertEqual(closed["closed_tab_ids"], [popup_id])
                worker.call("close", owner)
                self.assertTrue(profiles.reset())
                self.assertFalse(profile_paths.profile.exists())
            finally:
                worker.shutdown(timeout=2)


if __name__ == "__main__":
    unittest.main()
