from __future__ import annotations

from copy import deepcopy
from pathlib import Path
import tempfile
import unittest

from qualification.check import load, validate


class QualificationLedgerTests(unittest.TestCase):
    def setUp(self):
        self.ledger = load(Path(__file__).resolve().parents[1] / "qualification/baseline.json")

    def test_source_matrix_integrity_does_not_claim_qualification(self):
        validate(self.ledger)
        self.assertEqual(self.ledger["qualification_result"]["status"], "not-qualified")
        self.assertFalse(self.ledger["qualification_result"]["production_http"])
        self.assertFalse(self.ledger["qualification_result"]["full_codex_parity"])

    def test_mutable_source_and_invalid_content_pin_are_rejected(self):
        for key, value in (("url", "https://github.com/openai/codex/blob/main/source.rs"),
                           ("sha256", "not-a-digest"), ("line_ranges", [[0, 2]])):
            ledger = deepcopy(self.ledger)
            ledger["sources"][0][key] = value
            with self.subTest(key=key), self.assertRaises(ValueError):
                validate(ledger)

    def test_duplicate_ids_and_unresolved_references_are_rejected(self):
        self.ledger["sources"][1]["id"] = self.ledger["sources"][0]["id"]
        with self.assertRaises(ValueError):
            validate(self.ledger)
        self.setUp()
        self.ledger["matrix"][0]["journeys"] = ["J-INVENTED"]
        with self.assertRaises(ValueError):
            validate(self.ledger)

    def test_pinned_scope_and_defects_cannot_be_silently_removed(self):
        for collection in ("matrix", "retained_http_defects"):
            ledger = deepcopy(self.ledger)
            ledger[collection].pop()
            with self.subTest(collection=collection), self.assertRaises(ValueError):
                validate(ledger)

    def test_passed_journey_and_closed_gate_require_candidate_evidence(self):
        for collection, key, value in (("journeys", "result", "passed"),
                                       ("release_gates", "status", "closed")):
            ledger = deepcopy(self.ledger)
            ledger[collection][0][key] = value
            with self.subTest(collection=collection), self.assertRaises(ValueError):
                validate(ledger)

    def test_open_gates_prevent_qualification_claims(self):
        for key, value in (("status", "qualified"), ("production_http", True),
                           ("full_codex_parity", True)):
            ledger = deepcopy(self.ledger)
            ledger["qualification_result"][key] = value
            with self.subTest(key=key), self.assertRaises(ValueError):
                validate(ledger)

    def test_file_and_string_budgets(self):
        self.ledger["baseline_policy"]["scope"] = "x" * 4097
        with self.assertRaises(ValueError):
            validate(self.ledger)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "ledger.json"
            path.write_bytes(b" " * 131073)
            with self.assertRaises(ValueError):
                load(path)

    def test_duplicate_json_keys_and_nonfinite_numbers_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "ledger.json"
            for raw in ('{"a":1,"a":2}', '{"a":NaN}', '{"a":Infinity}'):
                path.write_text(raw)
                with self.subTest(raw=raw), self.assertRaises(ValueError):
                    load(path)
