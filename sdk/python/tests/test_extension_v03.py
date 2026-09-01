from __future__ import annotations

import unittest

from ygg_extension import Extension, RpcError, text_content


REQUIRED_FEATURES = [
    "request_cancellation",
    "content_parts",
    "owner_context",
    "ordered_events",
    "catalog_transactions",
    "effect_transactions",
    "document_streams",
]


def invocation(request_id: int, kind: str) -> dict:
    process = {"instance_id": "python-sdk-test", "generation": 1}
    return {
        "principal": {"name": "python-test", "sha256": "1" * 64},
        "session_owner": {"sha256": "2" * 64},
        "process": process,
        "operation": {
            "process": process,
            "request_id": request_id,
            "kind": kind,
            "mode": "rpc",
            "deadline_unix_ms": 4_000_000_000_000,
            "cancellation_owner": f"python-test-{request_id}",
        },
    }


class ExtensionApiV03ContractTests(unittest.TestCase):
    def test_defined_api_v03_initializes_with_epoch_zero_catalog_and_effects(self) -> None:
        extension = Extension(api_version="0.3")

        @extension.tool(name="echo", description="Echo text")
        def echo(arguments: dict) -> dict:
            return {"content": [text_content(arguments.get("value", ""))]}

        initialized = extension._initialize(  # noqa: SLF001 - protocol contract fixture
            {
                "api_version": "0.3",
                "contributes": {"tools": ["echo"], "commands": []},
                "protocol": {
                    "version": "0.3",
                    "required_features": REQUIRED_FEATURES,
                    "optional_features": [],
                    "limits": {"max_concurrent_requests": 4},
                    "host_services": [],
                },
            }
        )

        self.assertTrue(extension.initialized)
        self.assertEqual("0.3", initialized["api_version"])
        self.assertEqual([], initialized["tools"])
        catalog = initialized["protocol"]["catalog"]
        self.assertEqual(0, catalog["revision"])
        self.assertEqual(["echo"], [tool["name"] for tool in catalog["tools"]])

        result = extension._call_tool(  # noqa: SLF001 - protocol contract fixture
            {
                "name": "echo",
                "arguments": {"value": "ready"},
                "catalog_revision": 0,
                "invocation": invocation(1, "tool"),
                "context": {},
            }
        )
        self.assertEqual("ready", result["content"][0]["text"])
        self.assertEqual([], result["effects"]["effects"])
        self.assertEqual(1, result["effects"]["operation_token"]["request_id"])

    def test_api_v03_rejects_an_incomplete_mandatory_feature_set(self) -> None:
        extension = Extension(api_version="0.3")
        with self.assertRaises(RpcError) as raised:
            extension._initialize(  # noqa: SLF001
                {
                    "api_version": "0.3",
                    "protocol": {
                        "version": "0.3",
                        "required_features": REQUIRED_FEATURES[:-1],
                        "limits": {"max_concurrent_requests": 1},
                        "host_services": [],
                    },
                }
            )
        self.assertEqual(-32000, raised.exception.code)
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
