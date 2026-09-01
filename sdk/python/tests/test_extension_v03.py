from __future__ import annotations

import unittest

from ygg_extension import Extension, RpcError


class ExtensionApiV03ContractTests(unittest.TestCase):
    def test_defined_api_v03_fails_before_runtime_initialization(self) -> None:
        extension = Extension(api_version="0.3")

        with self.assertRaises(RpcError) as raised:
            extension._initialize(  # noqa: SLF001 - protocol contract fixture
                {
                    "api_version": "0.3",
                    "protocol": {
                        "version": "0.3",
                        "required_features": [
                            "request_cancellation",
                            "content_parts",
                            "owner_context",
                            "ordered_events",
                            "catalog_transactions",
                            "effect_transactions",
                            "document_streams",
                        ],
                    },
                }
            )

        self.assertEqual(-32000, raised.exception.code)
        self.assertIn("defined but not runtime-ready", str(raised.exception))
        self.assertFalse(extension.initialized)

    def test_unknown_api_version_is_rejected_explicitly(self) -> None:
        extension = Extension(api_version="9.9")

        with self.assertRaises(RpcError) as raised:
            extension._initialize({"api_version": "9.9"})  # noqa: SLF001

        self.assertEqual(-32000, raised.exception.code)
        self.assertIn("unsupported extension API version", str(raised.exception))
        self.assertFalse(extension.initialized)


if __name__ == "__main__":
    unittest.main()
