from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parents[1] / "scripts"
sys.path.insert(0, str(SCRIPT_DIR))

import analyze  # noqa: E402
import common  # noqa: E402


class PipelineTests(unittest.TestCase):
    def test_public_task_contains_no_evaluator_fields(self) -> None:
        row = {
            "repo": "owner/repo",
            "instance_id": "owner__repo-1",
            "base_commit": "a" * 40,
            "problem_statement": "issue",
            "patch": "gold",
            "test_patch": "hidden",
            "FAIL_TO_PASS": ["test"],
            "PASS_TO_PASS": [],
            "test_cmds": ["pytest"],
        }
        public = common.public_task(row)
        self.assertEqual(set(public), set(common.AGENT_VISIBLE_FIELDS))
        self.assertNotIn("patch", public)

    def test_image_name_matches_upstream_separator(self) -> None:
        self.assertEqual(
            common.image_reference("Owner__Repo-123"),
            "starryzhang/sweb.eval.x86_64.owner_1776_repo-123:latest",
        )

    def test_concurrency_does_not_count_a_call_as_overlapping_itself(self) -> None:
        self.assertEqual(analyze.concurrency([(0.0, 1.0)])["concurrent_tool_calls"], 0)
        result = analyze.concurrency([(0.0, 1.0), (0.5, 1.5)])
        self.assertEqual(result["max_concurrent_tool_count"], 2)
        self.assertEqual(result["concurrent_tool_calls"], 2)

    def test_telemetry_reader_uses_disjoint_request_buckets(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "telemetry.jsonl"
            path.write_text(
                "\n".join(
                    [
                        json.dumps({"schema": "ygg.telemetry.v1", "record": "model_request_started", "logical_turn": 1}),
                        json.dumps({
                            "schema": "ygg.telemetry.v1",
                            "record": "model_request_finished",
                            "logical_turn": 1,
                            "timestamp_unix_ms": 1000,
                            "elapsed_ms": 10,
                            "usage_scope": "request",
                            "provider_input_tokens": 7,
                            "uncached_input_tokens": 3,
                            "cache_read_tokens": 4,
                            "cache_write_tokens": 0,
                            "output_tokens": 2,
                            "reasoning_tokens": 1,
                            "total_tokens": 9,
                        }),
                    ]
                )
                + "\n",
                encoding="utf-8",
            )
            result = analyze.telemetry_data(path)
            self.assertEqual(result["usage"]["provider_input_tokens"], 7)
            self.assertEqual(result["usage"]["cache_read_tokens"], 4)
            self.assertEqual(result["model_call_attempts"], 1)


if __name__ == "__main__":
    unittest.main()
