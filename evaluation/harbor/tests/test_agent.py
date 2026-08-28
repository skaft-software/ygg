"""Harbor adapter behavior tests (skipped when Harbor is not installed)."""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

from evaluation.harbor.config import DEFAULT_MODEL, PINNED_YGG_VERSION

try:
    from harbor.models.agent.context import AgentContext

    from evaluation.harbor.ygg_agent import (
        Ygg,
        YggBenchmarkTimeoutError,
        YggProviderError,
        YggSetupError,
    )
except ModuleNotFoundError:  # pragma: no cover - exercised in the dependency-free job
    AgentContext = None  # type: ignore[assignment,misc]
    Ygg = None  # type: ignore[assignment]
    YggBenchmarkTimeoutError = None  # type: ignore[assignment,misc]
    YggProviderError = None  # type: ignore[assignment,misc]
    YggSetupError = None  # type: ignore[assignment,misc]


@unittest.skipIf(Ygg is None, "Harbor is not installed")
class AgentTests(unittest.IsolatedAsyncioTestCase):
    def test_defaults_pin_model(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            self.assertEqual(
                Ygg(Path(directory), model_name=None).model_name,
                DEFAULT_MODEL,
            )

    async def test_setup_logs_and_verifies_pinned_version(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            logs_dir = Path(directory)
            environment = FakeEnvironment(logs_dir, SimpleNamespace())
            agent = Ygg(logs_dir)
            await agent.setup(environment)

            self.assertIn("install -m 0755", environment.calls[0]["command"])
            self.assertIn("mkdir -p /logs/agent/sessions", environment.calls[0]["command"])
            self.assertEqual(
                (logs_dir / "setup-stdout.txt").read_text(), f"ygg {PINNED_YGG_VERSION}\n"
            )
            self.assertEqual(agent.version(), PINNED_YGG_VERSION)

    async def test_telemetry_flag_is_explicit_in_the_recorded_command(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            logs_dir = Path(directory)
            environment = FakeEnvironment(
                logs_dir,
                SimpleNamespace(return_code=0, stdout="completed\n", stderr=""),
            )
            agent = Ygg(logs_dir, telemetry=True)
            await agent.setup(environment)
            await agent.run("task", environment, AgentContext())

            run_call = environment.calls[-1]
            self.assertIn("--telemetry /logs/agent/ygg-telemetry.jsonl", run_call["command"])
            invocation = json.loads((logs_dir / "invocation.json").read_text())
            self.assertTrue(invocation["telemetry"])

    async def test_run_writes_metrics_redacted_output_and_native_artifacts(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            logs_dir = Path(directory)
            environment = FakeEnvironment(
                logs_dir,
                SimpleNamespace(
                    return_code=0,
                    stdout="completed with secret-value\n",
                    stderr="",
                ),
            )
            agent = Ygg(
                logs_dir,
                model_name="gpt-5.6-sol",
                extra_env={"OPENAI_API_KEY": "secret-value"},
            )
            await agent.setup(environment)
            context = AgentContext()
            await agent.run("task secret-value", environment, context)
            agent.populate_context_post_run(context)

            self.assertNotIn("secret-value", (logs_dir / "stdout.txt").read_text())
            native_session = logs_dir / "sessions" / "session.jsonl"
            self.assertNotIn("secret-value", native_session.read_text())
            trajectory = json.loads((logs_dir / "trajectory.json").read_text())
            self.assertEqual(trajectory["agent"]["name"], "ygg")
            self.assertEqual(context.n_input_tokens, 12)
            self.assertEqual(context.n_cache_tokens, 2)
            self.assertEqual(context.n_output_tokens, 4)
            self.assertEqual(context.cost_usd, 0.1)
            manifest = json.loads(
                (logs_dir / "native-session-manifest.json").read_text()
            )
            self.assertEqual(len(manifest["files"]), 1)

    async def test_provider_failure_is_distinguished_from_agent_failure(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            logs_dir = Path(directory)
            environment = FakeEnvironment(
                logs_dir,
                SimpleNamespace(
                    return_code=1,
                    stdout="",
                    stderr="provider returned HTTP 429 rate limit",
                ),
            )
            agent = Ygg(logs_dir)
            await agent.setup(environment)
            with self.assertRaises(YggProviderError):
                await agent.run("task", environment, AgentContext())
            self.assertEqual(
                (logs_dir / "failure-classification.txt").read_text(),
                "provider_failure\n",
            )

    async def test_process_timeout_is_wrapped_and_classified(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            logs_dir = Path(directory)
            environment = FakeEnvironment(
                logs_dir,
                SimpleNamespace(return_code=124, stdout="", stderr="Terminated"),
            )
            agent = Ygg(logs_dir, agent_timeout_sec=2, bash_timeout_secs=600)
            await agent.setup(environment)
            with self.assertRaises(YggBenchmarkTimeoutError):
                await agent.run("task", environment, AgentContext())
            run_call = environment.calls[-1]
            self.assertTrue(run_call["command"].startswith("timeout --signal=TERM"))
            self.assertIn(" 2s /tmp/ygg ", run_call["command"])
            self.assertIn("--bash-timeout-secs 600", run_call["command"])
            self.assertEqual(run_call["timeout_sec"], 17)
            self.assertEqual(
                (logs_dir / "failure-classification.txt").read_text(),
                "benchmark_timeout\n",
            )

    async def test_setup_rejects_version_mismatch(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            logs_dir = Path(directory)
            environment = FakeEnvironment(logs_dir, SimpleNamespace(version="9.9.9"))
            agent = Ygg(logs_dir)
            with self.assertRaises(YggSetupError):
                await agent.setup(environment)
            self.assertIn("ygg 9.9.9", (logs_dir / "setup-stdout.txt").read_text())


class FakeEnvironment:
    def __init__(self, logs_dir: Path, run_result: SimpleNamespace) -> None:
        self.logs_dir = logs_dir
        self.run_result = run_result
        self.calls: list[dict[str, object]] = []

    async def exec(
        self,
        *,
        command: str,
        env: dict[str, str] | None = None,
        timeout_sec: int | None = None,
    ) -> SimpleNamespace:
        self.calls.append(
            {"command": command, "env": env, "timeout_sec": timeout_sec}
        )
        if "--version" in command:
            version = getattr(self.run_result, "version", PINNED_YGG_VERSION)
            return SimpleNamespace(
                return_code=0,
                stdout=f"ygg {version}\n",
                stderr="",
            )

        session_dir = self.logs_dir / "sessions"
        session_dir.mkdir(parents=True, exist_ok=True)
        session_path = session_dir / "session.jsonl"
        session_path.write_text(
            "\n".join(json.dumps(record) for record in self._records()) + "\n"
        )
        return self.run_result

    def _records(self) -> list[dict[str, object]]:
        return [
            {
                "type": "entry",
                "id": "001",
                "parent": None,
                "value": {
                    "type": "message",
                    "User": {"content": [{"Text": "task secret-value"}]},
                },
            },
            {
                "type": "entry",
                "id": "002",
                "parent": "001",
                "value": {
                    "type": "message",
                    "Assistant": {
                        "content": [{"Text": "completed"}],
                        "model": "gpt-5.6-sol",
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
            {"type": "head", "id": "002"},
        ]


if __name__ == "__main__":
    unittest.main()
