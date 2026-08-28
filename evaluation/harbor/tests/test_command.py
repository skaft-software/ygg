"""Unit tests for the Harbor adapter's dependency-free command layer."""

from __future__ import annotations

import shlex
import unittest

from evaluation.harbor.config import DEFAULT_REASONING, PINNED_YGG_VERSION
from evaluation.harbor.command import (
    FailureKind,
    build_ygg_argv,
    build_ygg_command,
    classify_failure,
    parse_ygg_version,
    terminate_process_group_command,
    wrap_in_process_group,
    wrap_with_timeout,
)


class CommandTests(unittest.TestCase):
    def test_command_keeps_instruction_as_one_shell_argument(self) -> None:
        instruction = "create 'quoted' file; do not run $(echo secret)"
        command = build_ygg_command(
            "/tmp/ygg",
            instruction,
            model="gpt-5.6-sol",
            reasoning=DEFAULT_REASONING,
            session_dir="/logs/agent/sessions",
            max_turns=8,
            bash_timeout_secs=600,
            telemetry_path="/logs/agent/ygg-telemetry.jsonl",
        )

        self.assertEqual(shlex.split(command.shell), list(command.argv))
        self.assertEqual(command.argv[-1], instruction)
        self.assertIn("--print", command.argv)
        self.assertIn("--workspace-trusted", command.argv)
        self.assertIn("--telemetry", command.argv)
        self.assertEqual(
            command.argv[command.argv.index("--telemetry") + 1],
            "/logs/agent/ygg-telemetry.jsonl",
        )
        self.assertEqual(
            command.argv[command.argv.index("--bash-timeout-secs") + 1],
            "600",
        )

    def test_command_accepts_instruction_starting_with_dash(self) -> None:
        instruction = "- "
        command = build_ygg_command(
            "/tmp/ygg",
            instruction,
            model=None,
            reasoning=None,
            session_dir="/logs/agent/sessions",
        )

        self.assertEqual(shlex.split(command.shell), list(command.argv))
        self.assertEqual(command.argv[-2:], ("--", instruction))

    def test_argv_rejects_invalid_limits(self) -> None:
        for kwargs in (
            {"max_turns": 0},
            {"bash_timeout_secs": 0},
            {"bash_timeout_secs": 3_601},
            {"telemetry_path": ""},
        ):
            with self.subTest(kwargs=kwargs), self.assertRaises(ValueError):
                build_ygg_argv(
                    "/tmp/ygg",
                    "prompt",
                    model=None,
                    reasoning=None,
                    session_dir="/logs/agent/sessions",
                    **kwargs,
                )

    def test_timeout_runs_inside_an_independently_cleanable_process_group(self) -> None:
        command = build_ygg_command(
            "/tmp/ygg",
            "task",
            model=None,
            reasoning=None,
            session_dir="/logs/agent/sessions",
        )
        timed = wrap_with_timeout(command, 2)
        grouped = wrap_in_process_group(timed, "/logs/agent/ygg-process-group.pid")
        cleanup = terminate_process_group_command("/logs/agent/ygg-process-group.pid")

        self.assertEqual(grouped.argv[:3], ("setsid", "sh", "-c"))
        self.assertIn("timeout", grouped.argv)
        self.assertIn("--kill-after=5s", grouped.argv)
        self.assertEqual(grouped.argv[-1], "task")
        self.assertEqual(cleanup.argv[:3], ("bash", "-c", cleanup.argv[2]))
        self.assertIn("ygg-process-group-cleanup", cleanup.argv)
        self.assertIn('kill -KILL -- "-$pgid"', cleanup.argv[2])

    def test_version_parser_accepts_ygg_output(self) -> None:
        self.assertEqual(
            parse_ygg_version(f"ygg {PINNED_YGG_VERSION}\n"), PINNED_YGG_VERSION
        )
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
