#!/usr/bin/env python3
"""Local HTTP fixture tests for ygg-web-search's provider boundary."""

from __future__ import annotations

import http.server
import ipaddress
import json
import os
from pathlib import Path
import socket
import sys
import tempfile
import threading
import time
import unittest
from unittest import mock
from urllib.parse import parse_qs, urlsplit


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from provider import (  # noqa: E402
    BoundedCache,
    ConfigError,
    DestinationRejected,
    HttpClient,
    InvalidInput,
    ProviderFailed,
    RequestTimedOut,
    ResolvedAddress,
    TooLarge,
    UnsupportedContent,
    WebService,
    citation_id,
    load_configuration,
    parse_configuration,
    requested_domains,
    sanitize_url,
)


class FixtureServer(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, address, handler):
        super().__init__(address, handler)
        self.counts = {}
        self.queries = []
        self.lock = threading.Lock()

    def record(self, path, query):
        with self.lock:
            self.counts[path] = self.counts.get(path, 0) + 1
            self.queries.append(query)

    def handle_error(self, request, client_address):
        # Cancellation and timeout deliberately close sockets mid-fixture.
        return


class FixtureHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        parsed = urlsplit(self.path)
        self.server.record(parsed.path, parsed.query)
        if parsed.path == "/search":
            body = json.dumps(
                {
                    "results": [
                        {
                            "title": "  Example <b>result</b>  ",
                            "url": "http://public.test/page?utm_source=tracker",
                            "content": "A normalized &amp; bounded snippet.",
                            "publishedDate": "2026-08-20",
                            "engines": ["fixture", "fixture"],
                        },
                        {
                            "title": "Duplicate title",
                            "url": "http://public.test/page",
                            "content": "duplicate",
                        },
                        {
                            "title": "Private",
                            "url": "http://127.0.0.1/private",
                            "content": "must be dropped",
                        },
                        {
                            "title": "Credential URL",
                            "url": "http://user:secret@public.test/unsafe",
                            "content": "must be dropped",
                        },
                        {
                            "title": "Second source",
                            "url": "http://public.test/second?b=2&a=1#fragment",
                            "content": "A second result.",
                            "engine": "fixture-two",
                        },
                    ]
                }
            ).encode("utf-8")
            self.respond(200, body, "application/json; charset=utf-8")
        elif parsed.path == "/search-failure":
            self.respond(503, b"provider internals must not escape", "text/plain")
        elif parsed.path == "/search-invalid":
            self.respond(200, b"{not json", "application/json")
        elif parsed.path == "/page":
            body = (
                "<!doctype html><html><head><title> Fixture Page </title>"
                '<meta property="article:published_time" content="2026-08-19">'
                "<style>secret style</style><script>ignore_injection()</script></head>"
                "<body><h1>Evidence</h1><p>alpha needle omega</p>"
                + ("<p>bounded content needle tail</p>" * 300)
                + "</body></html>"
            ).encode("utf-8")
            self.respond(200, body, "text/html; charset=utf-8")
        elif parsed.path == "/second":
            self.respond(200, b"second plain source", "text/plain; charset=utf-8")
        elif parsed.path == "/redirect":
            self.redirect("http://public.test/page")
        elif parsed.path == "/redirect-private":
            self.redirect("http://private.test/page")
        elif parsed.path == "/loop":
            self.redirect("http://public.test/loop")
        elif parsed.path == "/unsupported":
            self.respond(200, b"%PDF-fixture", "application/pdf")
        elif parsed.path == "/oversized":
            self.send_response(200)
            self.send_header("Content-Type", "text/plain")
            self.send_header("Content-Length", "600000")
            self.send_header("Connection", "close")
            self.end_headers()
        elif parsed.path == "/slow":
            time.sleep(0.35)
            self.respond(200, b"eventual response", "text/plain")
        elif parsed.path == "/slow-body":
            body = b"a" * 32768
            self.send_response(200)
            self.send_header("Content-Type", "text/plain")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Connection", "close")
            self.end_headers()
            try:
                self.wfile.write(body[:64])
                self.wfile.flush()
                time.sleep(1.0)
                self.wfile.write(body[64:])
            except (BrokenPipeError, ConnectionResetError):
                pass
        else:
            self.respond(404, b"not found", "text/plain")

    def respond(self, status, body, content_type):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def redirect(self, location):
        self.send_response(302)
        self.send_header("Location", location)
        self.send_header("Content-Length", "0")
        self.send_header("Connection", "close")
        self.end_headers()

    def log_message(self, format, *args):
        return


class FixtureResolver:
    def __init__(self, port):
        self.port = port

    def __call__(self, host, port, deadline):
        deadline.checkpoint()
        if host in ("provider.test", "public.test"):
            policy_address = ipaddress.ip_address("93.184.216.34")
        elif host == "private.test":
            policy_address = ipaddress.ip_address("10.0.0.8")
        else:
            try:
                policy_address = ipaddress.ip_address(host)
            except ValueError:
                policy_address = ipaddress.ip_address("93.184.216.34")
        return [
            ResolvedAddress(
                socket.AF_INET,
                ("127.0.0.1", self.port),
                policy_address,
            )
        ]


class FakeCancelled(Exception):
    pass


class Cancellation:
    def __init__(self):
        self.cancelled = threading.Event()

    def raise_if_cancelled(self):
        if self.cancelled.is_set():
            raise FakeCancelled("cancelled")


class ProviderTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = FixtureServer(("127.0.0.1", 0), FixtureHandler)
        cls.thread = threading.Thread(target=cls.server.serve_forever, daemon=True)
        cls.thread.start()

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()
        cls.thread.join(timeout=2)

    def setUp(self):
        self.http = HttpClient(resolver=FixtureResolver(self.server.server_port))
        self.service = WebService(http=self.http, cache=BoundedCache())
        self.config = parse_configuration(
            {
                "version": 1,
                "provider": {
                    "kind": "searxng",
                    "endpoint": "http://provider.test/search",
                    "label": "Fixture Search",
                },
                "limits": {
                    "default_timeout_seconds": 2,
                    "default_content_bytes": 4096,
                    "max_content_bytes": 16384,
                    "max_download_bytes": 131072,
                    "cache_ttl_seconds": 60,
                },
            }
        )

    def test_search_normalizes_bounds_citations_and_uses_cache(self):
        before = self.server.counts.get("/search", 0)
        first = self.service.search(self.config, query=" bounded   evidence ", max_results=5)
        second = self.service.search(self.config, query="bounded evidence", max_results=5)

        self.assertEqual(first["result_count"], 2)
        self.assertTrue(first["truncated"])
        self.assertEqual(first["cache"], "miss")
        self.assertEqual(second["cache"], "hit")
        self.assertEqual(self.server.counts.get("/search", 0), before + 1)
        self.assertEqual(first["results"][0]["url"], "http://public.test/page")
        self.assertEqual(first["results"][0]["title"], "Example result")
        self.assertEqual(first["results"][0]["published_at"], "2026-08-20")
        self.assertNotIn("&amp;", first["results"][0]["snippet"])
        self.assertEqual(
            first["results"][0]["citation_id"], second["results"][0]["citation_id"]
        )
        query = parse_qs(self.server.queries[-1])
        self.assertEqual(query["format"], ["json"])
        self.assertEqual(query["safesearch"], ["1"])

    def test_citation_is_stable_for_sanitized_url(self):
        one = sanitize_url("https://Example.COM/a?utm_source=x&b=2&a=1#frag")
        two = sanitize_url("https://example.com/a?a=1&b=2")
        self.assertEqual(one, two)
        self.assertEqual(citation_id(one), citation_id(two))
        self.assertTrue(citation_id(one).startswith("web-"))

    def test_open_revalidates_redirects_and_rejects_private_target(self):
        with self.assertRaises(DestinationRejected):
            self.service.open(self.config, url="http://public.test/redirect-private")
        opened = self.service.open(self.config, url="http://public.test/redirect")
        self.assertEqual(opened["document"]["url"], "http://public.test/page")
        self.assertEqual(opened["document"]["redirects"], 1)

    def test_open_normalizes_html_publication_and_truncates_content(self):
        result = self.service.open(
            self.config,
            url="http://public.test/page",
            max_bytes=1024,
        )
        document = result["document"]
        self.assertEqual(document["title"], "Fixture Page")
        self.assertEqual(document["published_at"], "2026-08-19")
        self.assertTrue(document["truncated"])
        self.assertLessEqual(document["normalized_bytes"], 1024)
        self.assertNotIn("ignore_injection", document["content"])
        self.assertNotIn("secret style", document["content"])

    def test_find_returns_bounded_excerpts_and_reuses_open_cache(self):
        first = self.service.find(
            self.config,
            url="http://public.test/page",
            pattern="needle",
            max_matches=2,
            max_bytes=4096,
        )
        second = self.service.find(
            self.config,
            url="http://public.test/page",
            pattern="needle",
            max_matches=2,
            max_bytes=4096,
        )
        self.assertEqual(first["match_count"], 2)
        self.assertTrue(first["truncated"])
        self.assertEqual(second["cache"], "hit")
        self.assertNotIn("content", first["document"])
        self.assertTrue(all(len(item["excerpt"].encode("utf-8")) <= 512 for item in first["matches"]))

    def test_rejects_oversized_and_unsupported_content(self):
        with self.assertRaises(TooLarge):
            self.service.open(self.config, url="http://public.test/oversized")
        with self.assertRaises(UnsupportedContent):
            self.service.open(self.config, url="http://public.test/unsupported")

    def test_timeout_is_bounded(self):
        started = time.monotonic()
        with self.assertRaises(RequestTimedOut):
            self.service.open(
                self.config,
                url="http://public.test/slow",
                timeout_seconds=0.1,
            )
        self.assertLess(time.monotonic() - started, 1.0)

    def test_cancellation_wins_during_streaming_read(self):
        cancellation = Cancellation()
        observed = []

        def run():
            try:
                self.service.open(
                    self.config,
                    url="http://public.test/slow-body",
                    cancellation=cancellation,
                    timeout_seconds=2,
                )
            except BaseException as error:
                observed.append(error)

        worker = threading.Thread(target=run)
        worker.start()
        time.sleep(0.1)
        cancellation.cancelled.set()
        worker.join(timeout=1.5)
        self.assertFalse(worker.is_alive())
        self.assertEqual(len(observed), 1)
        self.assertIsInstance(observed[0], FakeCancelled)

    def test_provider_failure_and_invalid_json_are_explicit(self):
        failed = parse_configuration(
            {
                "version": 1,
                "provider": {
                    "kind": "searxng",
                    "endpoint": "http://provider.test/search-failure",
                },
            }
        )
        invalid = parse_configuration(
            {
                "version": 1,
                "provider": {
                    "kind": "searxng",
                    "endpoint": "http://provider.test/search-invalid",
                },
            }
        )
        with self.assertRaises(ProviderFailed):
            self.service.search(failed, query="evidence")
        with self.assertRaises(ProviderFailed):
            self.service.search(invalid, query="evidence")

    def test_configuration_rejects_credentials_unknown_fields_and_bad_domains(self):
        for provider in (
            {"kind": "searxng", "endpoint": "https://user:secret@example.com"},
            {"kind": "searxng", "endpoint": "https://example.com", "api_key": "no"},
        ):
            with self.assertRaises(ConfigError):
                parse_configuration({"version": 1, "provider": provider})
        with self.assertRaises((ConfigError, InvalidInput)):
            parse_configuration(
                {
                    "version": 1,
                    "provider": {"kind": "searxng", "endpoint": "https://example.com"},
                    "limits": {"allowed_domains": ["localhost"]},
                }
            )

    def test_empty_request_domain_list_cannot_widen_configuration(self):
        self.assertEqual(
            requested_domains([], ("example.com",)),
            ("example.com",),
        )
        with self.assertRaises(DestinationRejected):
            requested_domains(["other.example"], ("example.com",))

    def test_configuration_file_is_bounded_and_regular(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "config.json"
            path.write_text(
                json.dumps(
                    {
                        "version": 1,
                        "provider": {
                            "kind": "searxng",
                            "endpoint": "https://example.com/search",
                        },
                    }
                ),
                encoding="utf-8",
            )
            self.assertEqual(load_configuration(path).provider.label, "SearXNG")
            path.write_bytes(b"x" * (64 * 1024 + 1))
            with self.assertRaises(ConfigError):
                load_configuration(path)
            path.unlink()
            target = Path(temporary) / "target.json"
            target.write_text("{}", encoding="utf-8")
            try:
                path.symlink_to(target)
            except (OSError, NotImplementedError):
                return
            with self.assertRaises(ConfigError):
                load_configuration(path)

    def test_configuration_file_requires_current_owner_and_private_write_mode(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "config.json"
            path.write_text(
                json.dumps(
                    {
                        "version": 1,
                        "provider": {
                            "kind": "searxng",
                            "endpoint": "http://127.0.0.1/search",
                            "allow_private_endpoint": True,
                        },
                    }
                ),
                encoding="utf-8",
            )
            path.chmod(0o666)
            with self.assertRaises(ConfigError):
                load_configuration(path)

            path.chmod(0o600)
            if hasattr(os, "getuid"):
                with mock.patch("provider.os.getuid", return_value=os.getuid() + 1):
                    with self.assertRaises(ConfigError):
                        load_configuration(path)


if __name__ == "__main__":
    unittest.main()
