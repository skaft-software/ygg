from __future__ import annotations

import os
import tempfile
import unittest
from pathlib import Path

from ygg_browse.safety import (
    BrowseError,
    ExclusiveFileLock,
    ResourceOwner,
    bounded_text,
    sanitize_url,
    url_origin,
    validate_http_url,
)


class UrlSafetyTests(unittest.TestCase):
    def test_accepts_only_absolute_http_and_https(self) -> None:
        self.assertEqual(validate_http_url("https://Example.COM/path?q=secret#frag"), "https://example.com/path?q=secret#frag")
        self.assertEqual(validate_http_url("http://localhost:8080/"), "http://localhost:8080/")
        for value in (
            "/relative",
            "example.com",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/plain,x",
            "about:blank",
            "ftp://example.com/a",
            "https://user:pass@example.com/",
            "https://user@example.com/",
            "https://example.com\\@evil.test/",
            " https://example.com/",
            "https://example.com/\nnext",
        ):
            with self.subTest(value=value), self.assertRaises(BrowseError):
                validate_http_url(value)

    def test_display_url_removes_sensitive_parts_and_controls(self) -> None:
        self.assertEqual(
            sanitize_url("https://user:secret@Example.com/private?q=token#fragment"),
            "https://example.com/private",
        )
        self.assertEqual(url_origin("https://example.com/private?q=token"), "https://example.com")
        self.assertEqual(sanitize_url("file:///tmp/no"), "unavailable")
        self.assertEqual(sanitize_url("about:blank"), "blank")
        self.assertNotIn("\x00", sanitize_url("https://example.com/a\x00b?q=x"))

    def test_bounded_text_removes_controls(self) -> None:
        value = bounded_text("a\x00 b\n c\u202e spoof", 20)
        self.assertNotIn("\x00", value)
        self.assertNotIn("\u202e", value)
        self.assertLessEqual(len(value), 20)


class OwnerAndLockTests(unittest.TestCase):
    def test_owner_comes_only_from_complete_context(self) -> None:
        owner = ResourceOwner.from_context(
            {
                "resource_owner": {
                    "session_id": "s",
                    "extension_instance_id": "i",
                    "process_generation": 2,
                }
            }
        )
        self.assertEqual(owner.key, ("s", "i", 2))
        for context in (
            {},
            {"resource_owner": {}},
            {"resource_owner": {"session_id": "s", "extension_instance_id": "i", "process_generation": -1}},
            {"resource_owner": {"session_id": "s", "extension_instance_id": "i", "process_generation": 0}},
        ):
            with self.assertRaises(BrowseError):
                ResourceOwner.from_context(context)

    @unittest.skipIf(os.name == "nt", "fcntl lock semantics are Unix-only")
    def test_lock_is_exclusive_and_rejects_symlink(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            first = ExclusiveFileLock(root / "owner.lock")
            second = ExclusiveFileLock(root / "owner.lock")
            self.assertTrue(first.acquire())
            self.assertFalse(second.acquire())
            first.release()
            self.assertTrue(second.acquire())
            second.release()
            target = root / "target"
            target.write_text("x", encoding="utf-8")
            link = root / "linked.lock"
            link.symlink_to(target)
            with self.assertRaises(BrowseError):
                ExclusiveFileLock(link).acquire()


if __name__ == "__main__":
    unittest.main()
