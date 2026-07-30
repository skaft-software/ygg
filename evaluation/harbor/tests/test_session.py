"""Tests for native Ygg session conversion."""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from evaluation.harbor.session import convert_session_file


class SessionConversionTests(unittest.TestCase):
    def test_converts_active_branch_tools_and_usage(self) -> None:
        records = [
            {
                "type": "entry",
                "id": "001",
                "parent": None,
                "value": {
                    "type": "message",
                    "User": {"content": [{"Text": "inspect the project"}]},
                },
            },
            {
                "type": "entry",
                "id": "002",
                "parent": "001",
                "value": {
                    "type": "message",
                    "Assistant": {
                        "content": [
                            {"Text": "I will inspect it."},
                            {
                                "ToolCall": {
                                    "id": "call-1",
                                    "name": "shell",
                                    "arguments_json": '{"cmd":"pwd"}',
                                }
                            },
                        ],
                        "model": "gpt-5.4",
                        "protocol": "open_ai_responses",
                    },
                },
            },
            {
                "type": "usage",
                "record": {
                    "kind": {"kind": "assistant_turn", "assistant": "002"},
                    "usage": {
                        "input_tokens": 10,
                        "cache_read_tokens": 2,
                        "output_tokens": 4,
                    },
                    "cost": {"total": 100_000},
                },
            },
            {
                "type": "entry",
                "id": "003",
                "parent": "002",
                "value": {
                    "type": "message",
                    "User": {
                        "content": [
                            {
                                "ToolResult": {
                                    "tool_call_id": "call-1",
                                    "content": [{"Text": "/workspace"}],
                                    "is_error": False,
                                }
                            }
                        ]
                    },
                },
            },
            {
                "type": "entry",
                "id": "004",
                "parent": "003",
                "value": {
                    "type": "message",
                    "Assistant": {
                        "content": [{"Text": "The workspace is ready."}],
                        "model": "gpt-5.4",
                        "protocol": "open_ai_responses",
                    },
                },
            },
            {"type": "head", "id": "004"},
        ]
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "session.jsonl"
            path.write_text("\n".join(json.dumps(record) for record in records) + "\n")
            conversion = convert_session_file(
                path,
                agent_name="ygg",
                agent_version="0.3.2-alpha",
                model_name="openai/gpt-5.4",
                reasoning="medium",
            )

        self.assertEqual(
            [step["source"] for step in conversion.trajectory["steps"]],
            ["user", "agent", "agent"],
        )
        agent_step = conversion.trajectory["steps"][1]
        self.assertEqual(agent_step["tool_calls"][0]["function_name"], "shell")
        self.assertEqual(agent_step["metrics"]["prompt_tokens"], 12)
        self.assertEqual(
            conversion.trajectory["steps"][1]["observation"]["results"][0][
                "source_call_id"
            ],
            "call-1",
        )
        self.assertEqual(conversion.metrics.input_tokens, 12)
        self.assertEqual(conversion.metrics.cache_tokens, 2)
        self.assertEqual(conversion.metrics.output_tokens, 4)
        self.assertEqual(conversion.metrics.cost_usd, 0.1)

    def test_ignores_inactive_branch(self) -> None:
        records = [
            {
                "type": "entry",
                "id": "001",
                "parent": None,
                "value": {
                    "type": "message",
                    "User": {"content": [{"Text": "root"}]},
                },
            },
            {
                "type": "entry",
                "id": "002",
                "parent": "001",
                "value": {
                    "type": "message",
                    "Assistant": {"content": [{"Text": "active"}], "model": "m"},
                },
            },
            {
                "type": "entry",
                "id": "003",
                "parent": "001",
                "value": {
                    "type": "message",
                    "Assistant": {"content": [{"Text": "inactive"}], "model": "m"},
                },
            },
            {"type": "head", "id": "002"},
        ]
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "session.jsonl"
            path.write_text("\n".join(json.dumps(record) for record in records) + "\n")
            conversion = convert_session_file(
                path,
                agent_name="ygg",
                agent_version="0.3.2-alpha",
                model_name=None,
                reasoning=None,
            )

        messages = [step["message"] for step in conversion.trajectory["steps"]]
        self.assertIn("active", messages)
        self.assertNotIn("inactive", messages)


if __name__ == "__main__":
    unittest.main()
