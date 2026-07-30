"""Unit tests for the Harbor adapter's dependency-free command layer."""

from __future__ import annotations

import shlex
import unittest

from evaluation.harbor.command import (
    FailureKind,
    build_ygg_argv,
    build_ygg_command,
    classify_failure,
    parse_ygg_version,
)


class CommandTests(unittest.TestCase):
    def test_command_keeps_instruction_as_one_shell_argument(self) -> None:
        instruction = "create 'quoted' file; do not run $(echo secret)"
        command = build_ygg_command(
            "/tmp/ygg",
            instruction,
            model="openai/gpt-5.4",
            reasoning="medium",
            session_dir="/logs/agent/sessions",
            max_turns=8,
        )

        self.assertEqual(shlex.split(command.shell), list(command.argv))
        self.assertEqual(command.argv[-1], instruction)
        self.assertIn("--print", command.argv)
        self.assertIn("--workspace-trusted", command.argv)

    def test_argv_rejects_invalid_limits(self) -> None:
        with self.assertRaises(ValueError):
            build_ygg_argv(
                "/tmp/ygg",
                "prompt",
                model=None,
                reasoning=None,
                session_dir="/logs/agent/sessions",
                max_turns=0,
            )

    def test_version_parser_accepts_ygg_output(self) -> None:
        self.assertEqual(parse_ygg_version("ygg 0.3.2-alpha\n"), "0.3.2-alpha")
        self.assertEqual(parse_ygg_version("ygg version 1.2.3"), "1.2.3")
        self.assertIsNone(parse_ygg_version("not a version"))

    def test_failure_categories_do_not_equate_exit_with_task_success(self) -> None:
        self.assertIs(FailureKind.SUCCESS, classify_failure(0))
        self.assertIs(
            FailureKind.PROVIDER,
            classify_failure(1, stderr="provider returned HTTP 429 rate limit"),
        )
        self.assertIs(FailureKind.AGENT, classify_failure(1, stderr="run failed"))
        self.assertIs(FailureKind.TIMEOUT, classify_failure(124, timed_out=True))


if __name__ == "__main__":
    unittest.main()
