"""Tests for credential-safe Harbor evidence."""

from __future__ import annotations

import json
import unittest

from evaluation.harbor.redaction import redact_jsonl, redact_text


class RedactionTests(unittest.TestCase):
    def test_redacts_configured_and_recognized_tokens(self) -> None:
        output = "Authorization: Bearer configured-secret sk-abcdefghijklmnop"
        redacted = redact_text(output, ["configured-secret"])
        self.assertNotIn("configured-secret", redacted)
        self.assertNotIn("sk-abcdefghijklmnop", redacted)
        self.assertIn("<redacted>", redacted)

    def test_jsonl_redaction_keeps_records_parseable(self) -> None:
        source = json.dumps(
            {
                "type": "entry",
                "value": {"text": "api_key=configured-secret"},
            }
        ) + "\n"
        redacted = redact_jsonl(source, ["configured-secret"])
        record = json.loads(redacted)
        self.assertEqual(record["value"]["text"], "api_key=<redacted>")

    def test_torn_tail_is_retained_for_session_recovery(self) -> None:
        source = '{"complete":"configured-secret"}\n{"partial":"configured-secret"'
        redacted = redact_jsonl(source, ["configured-secret"])
        self.assertIn('{"complete":"<redacted>"}', redacted)
        self.assertIn('{"partial":"<redacted>"', redacted)


if __name__ == "__main__":
    unittest.main()
