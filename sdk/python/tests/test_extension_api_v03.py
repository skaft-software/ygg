"""Cross-language conformance coverage for generated extension API 0.3 models."""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
import unittest

from ygg_extension import api_v03 as api


ROOT = Path(__file__).resolve().parents[3]
FIXTURES = ROOT / "protocol" / "fixtures" / "extension-api-v0.3"
NEGATIVE = FIXTURES / "negative"


class ExtensionApiV03ConformanceTests(unittest.TestCase):
    def fixture(self, name: str) -> object:
        return json.loads((FIXTURES / f"{name}.json").read_text(encoding="utf-8"))

    def error_code(self, callback: object) -> int:
        with self.assertRaises(api.ContractError) as caught:
            callback()  # type: ignore[operator]
        return caught.exception.code

    def test_canonical_fixtures_are_byte_exact_and_manifest_hashed(self) -> None:
        manifest = self.fixture("manifest")
        self.assertEqual(manifest["api_version"], api.API_VERSION)  # type: ignore[index]
        self.assertEqual(manifest["canonical_encoding"], api.CANONICAL_ENCODING)  # type: ignore[index]
        for entry in manifest["fixtures"]:  # type: ignore[index]
            raw = (FIXTURES / f"{entry['name']}.json").read_bytes()
            self.assertEqual(api.canonical_json(json.loads(raw)), raw.decode("utf-8"))
            self.assertEqual(hashlib.sha256(raw).hexdigest(), entry["sha256"])

    def test_generated_models_cover_foundation_shapes_and_presence(self) -> None:
        api.validate_initialize_request(api.parse_initialize_request(self.fixture("initialize-request")))
        api.validate_initialize_response(api.parse_initialize_response(self.fixture("initialize-response")))
        api.validate_tool_call_params(api.parse_tool_call_params(self.fixture("tool-call-params")))
        tool_result = api.parse_tool_call_result(self.fixture("tool-call-result"))
        api.validate_tool_call_result(tool_result)
        self.assertEqual(tool_result.structured_content.kind, "value")
        self.assertEqual(tool_result.structured_content.value, {"value": "hello"})
        api.validate_cancel_request_params(api.parse_cancel_request_params(self.fixture("cancel-request-params")))
        api.validate_shutdown_params(api.parse_shutdown_params(self.fixture("shutdown-params")))
        api.validate_shutdown_result(api.parse_shutdown_result(self.fixture("shutdown-result")))
        api.validate_error_object(api.parse_error_object(self.fixture("error-data-absent")))
        api.validate_error_object(api.parse_error_object(self.fixture("error-data-null")))
        api.validate_disposition(api.parse_disposition(self.fixture("continue-disposition")))
        for name in ("request-envelope", "notification-envelope", "success-envelope", "error-envelope"):
            api.parse_json_rpc_envelope(self.fixture(name))

        absent = api.parse_error_object(self.fixture("error-data-absent"))
        explicit_null = api.parse_error_object(self.fixture("error-data-null"))
        self.assertTrue(absent.data.is_absent())
        self.assertEqual(explicit_null.data.kind, "null")
        self.assertEqual(
            self.error_code(lambda: api.parse_disposition({"kind": "continue", "reason": None})),
            -32602,
        )
        self.assertEqual(
            self.error_code(
                lambda: api.parse_tool_call_result(
                    {
                        "content": [{"type": "image", "artifact_id": "a", "mime_type": "image/png"}],
                        "is_error": False,
                        "metadata": None,
                    }
                )
            ),
            -32011,
        )

    def test_contract_versions_and_canonical_bounds_fail_closed(self) -> None:
        offer = api.host_offer(api.MAX_FRAME_BYTES * 2, api.MAX_CONCURRENT_REQUESTS * 2)
        self.assertEqual(offer.limits.max_frame_bytes, api.MAX_FRAME_BYTES)
        self.assertEqual(offer.limits.max_concurrent_requests, api.MAX_CONCURRENT_REQUESTS)
        negotiated = api.negotiate(offer, api.select_required(offer))
        api.require_method(negotiated, "initialize", "host_to_extension")
        self.assertEqual(
            self.error_code(lambda: api.require_method(negotiated, "future/call", "host_to_extension")),
            -32601,
        )
        self.assertEqual(
            self.error_code(lambda: api.require_method(negotiated, "context/collect", "host_to_extension")),
            -32601,
        )
        self.assertEqual(
            self.error_code(lambda: api.validate_offer(api.ContractOffer(
                schema=offer.schema,
                encoding=offer.encoding,
                required_capabilities=offer.required_capabilities,
                optional_capabilities=offer.optional_capabilities,
                required_methods=["initialize"],
                optional_methods=offer.optional_methods,
                limits=offer.limits,
            ))),
            -32011,
        )
        wrong = dict(self.fixture("initialize-request"))
        wrong["api_version"] = "0.2"
        self.assertEqual(self.error_code(lambda: api.validate_initialize_request(api.parse_initialize_request(wrong))), -32602)
        self.assertEqual(
            self.error_code(lambda: api.validate_error_object(api.ErrorObject(-32601, "wrong"))),
            -32602,
        )
        self.assertEqual(self.error_code(lambda: api.canonical_json({"float": 1.0})), -32602)
        self.assertEqual(
            self.error_code(lambda: api.canonical_json({"large": api.MAX_PORTABLE_JSON_INTEGER + 1})),
            -32602,
        )
        self.assertEqual(self.error_code(lambda: api.canonical_frame({"x": "y"}, 1)), -32012)
        self.assertEqual(
            self.error_code(lambda: api.parse_json_rpc_envelope({"jsonrpc": "2.0", "id": None, "result": {}})),
            -32600,
        )
        self.assertEqual(
            self.error_code(lambda: api.parse_json_rpc_envelope({"jsonrpc": "2.0", "id": 1, "result": {}, "error": {}})),
            -32600,
        )
        self.assertEqual(self.error_code(lambda: api.canonical_json({"\ud800": "bad"})), -32602)
        self.assertTrue(api.runtime_supports_api_version("0.1"))
        self.assertFalse(api.bundle_supports_api_version("0.1"))
        self.assertTrue(api.bundle_supports_api_version("0.2"))
        self.assertTrue(api.bundle_supports_api_version("0.3"))
        self.assertEqual(api.LEGACY_ADAPTERS[0], ("0.1", "frozen", "legacy-json-rpc"))
        self.assertEqual(api.LEGACY_ADAPTERS[1], ("0.2", "supported", "legacy-json-rpc"))

    def test_hostile_fixture_corpus_is_rejected(self) -> None:
        manifest = json.loads((NEGATIVE / "manifest.json").read_text(encoding="utf-8"))
        for entry in manifest["fixtures"]:
            name = entry["name"]
            raw = (NEGATIVE / f"{name}.json").read_text(encoding="utf-8")
            with self.subTest(name=name):
                if name == "duplicate-key":
                    self.assertNotEqual(api.canonical_json(json.loads(raw)), raw)
                elif "surrogate" in name:
                    self.assertEqual(self.error_code(lambda: api.canonical_json(json.loads(raw))), -32602)
                elif name == "optional-reason-null":
                    self.assertEqual(self.error_code(lambda: api.parse_disposition(json.loads(raw))), -32602)
                else:
                    self.assertEqual(self.error_code(lambda: api.parse_json_rpc_envelope(json.loads(raw))), -32600)


if __name__ == "__main__":
    unittest.main()
