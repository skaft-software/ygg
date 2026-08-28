"""Linux/Docker proof for authoritative external-harness termination."""

from __future__ import annotations

import asyncio
import hashlib
import json
import shutil
import subprocess
import tempfile
import unittest
import uuid
from pathlib import Path

try:
    from harbor.models.agent.context import AgentContext

    from evaluation.harbor.ygg_agent import Ygg
except ModuleNotFoundError:  # pragma: no cover - dependency-free unit job
    AgentContext = None  # type: ignore[assignment,misc]
    Ygg = None  # type: ignore[assignment]


_IMAGE = "debian:bookworm-slim"
_FAKE_YGG = """#!/bin/sh
set -u
mkdir -p /logs/agent/sessions
if [ "${FAKE_MODE:-fixture-hang}" = fixture-control ]; then
  printf '%s\\n' '{"type":"normal_control"}' >> /logs/agent/sessions/session.jsonl
  printf '%s\\n' '{"type":"normal_control"}' >> /logs/agent/ygg-telemetry.jsonl
  printf '%s\\n' normal-complete
  exit 0
fi
(
  trap '' TERM
  (
    trap '' TERM
    while :; do
      printf '%s\\n' grandchild >> /logs/agent/workspace-heartbeat.txt
      sleep 0.05
    done
  ) &
  while :; do
    printf '%s\\n' child >> /logs/agent/tool-child-heartbeat.txt
    sleep 0.05
  done
) &
trap '' TERM
i=0
while :; do
  i=$((i + 1))
  printf '{"type":"session_heartbeat","sequence":%s}\\n' "$i" >> /logs/agent/sessions/session.jsonl
  printf '{"type":"telemetry_heartbeat","sequence":%s}\\n' "$i" >> /logs/agent/ygg-telemetry.jsonl
  sleep 0.05
done
"""


def _docker_ready() -> bool:
    if shutil.which("docker") is None:
        return False
    daemon = subprocess.run(
        ["docker", "info"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
    )
    image = subprocess.run(
        ["docker", "image", "inspect", _IMAGE],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    return daemon.returncode == 0 and image.returncode == 0


def _snapshot(paths: list[Path]) -> dict[str, tuple[int, str]]:
    return {
        str(path): (path.stat().st_size, hashlib.sha256(path.read_bytes()).hexdigest())
        for path in paths
    }


class DockerExecEnvironment:
    """Mimic Harbor Docker exec cancellation without stopping the remote exec."""

    def __init__(self, container: str, telemetry: Path) -> None:
        self.container = container
        self.telemetry = telemetry
        self.remote_survived_cancel = False

    async def exec(
        self,
        *,
        command: str,
        env: dict[str, str] | None = None,
        timeout_sec: int | None = None,
    ):
        argv = ["docker", "exec"]
        for key, value in (env or {}).items():
            argv.extend(("-e", f"{key}={value}"))
        argv.extend((self.container, "bash", "-lc", command))
        process = await asyncio.create_subprocess_exec(
            *argv,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        before_cancel = self.telemetry.stat().st_size if self.telemetry.exists() else 0
        try:
            operation = process.communicate()
            if timeout_sec is None:
                stdout, stderr = await operation
            else:
                stdout, stderr = await asyncio.wait_for(operation, timeout_sec)
        except asyncio.CancelledError:
            # This terminates only the local Docker CLI, matching Harbor's old
            # cancellation boundary. The container command remains alive until
            # the adapter issues its independent process-group cleanup exec.
            process.terminate()
            await process.wait()
            await asyncio.sleep(0.25)
            after_cancel = (
                self.telemetry.stat().st_size if self.telemetry.exists() else 0
            )
            self.remote_survived_cancel = after_cancel > before_cancel
            raise
        except asyncio.TimeoutError as error:
            process.terminate()
            await process.wait()
            raise RuntimeError(
                f"Command timed out after {timeout_sec} seconds"
            ) from error

        return type(
            "ExecResult",
            (),
            {
                "return_code": process.returncode or 0,
                "stdout": stdout.decode(errors="replace") or None,
                "stderr": stderr.decode(errors="replace") or None,
            },
        )()


@unittest.skipIf(Ygg is None, "Harbor is not installed")
@unittest.skipUnless(_docker_ready(), f"Docker daemon and cached {_IMAGE} are required")
class ProcessGroupTerminationTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        root = Path(self.temporary.name)
        self.logs = root / "agent"
        self.fixture = root / "fixture"
        self.logs.mkdir()
        self.fixture.mkdir()
        script = self.fixture / "fake-ygg"
        script.write_text(_FAKE_YGG, encoding="utf-8")
        script.chmod(0o755)
        self.container = f"ygg-harbor-termination-{uuid.uuid4().hex[:12]}"
        subprocess.run(
            [
                "docker",
                "run",
                "--detach",
                "--rm",
                "--init",
                "--network",
                "none",
                "--name",
                self.container,
                "--mount",
                f"type=bind,src={self.logs},dst=/logs/agent",
                "--mount",
                f"type=bind,src={self.fixture},dst=/fixture,readonly",
                _IMAGE,
                "sleep",
                "infinity",
            ],
            check=True,
            stdout=subprocess.DEVNULL,
        )

    async def asyncTearDown(self) -> None:
        subprocess.run(
            ["docker", "rm", "--force", self.container],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        self.temporary.cleanup()

    async def _wait_for_growth(self, path: Path) -> None:
        deadline = asyncio.get_running_loop().time() + 3
        while asyncio.get_running_loop().time() < deadline:
            if path.exists() and path.stat().st_size > 0:
                return
            await asyncio.sleep(0.05)
        self.fail(f"fixture never wrote {path}")

    def _assert_no_fixture_processes(self) -> None:
        result = subprocess.run(
            [
                "docker",
                "exec",
                self.container,
                "sh",
                "-c",
                "needle=/fixture/fake-; needle=${needle}ygg; "
                "for f in /proc/[0-9]*/cmdline; do "
                "tr '\\0' ' ' < \"$f\" 2>/dev/null | "
                'grep -Fq "$needle" && exit 0; '
                "done; exit 1",
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        self.assertNotEqual(
            result.returncode, 0, "fixture process survived finalization"
        )

    async def test_outer_cancellation_kills_group_before_immutable_finalization(
        self,
    ) -> None:
        telemetry = self.logs / "ygg-telemetry.jsonl"
        environment = DockerExecEnvironment(self.container, telemetry)
        agent = Ygg(
            self.logs,
            model_name="local-fixture",
            telemetry=True,
            extra_env={"FAKE_MODE": "fixture-hang"},
        )
        agent._binary = "/fixture/fake-ygg"

        operation = asyncio.create_task(
            agent.run("bounded cancellation fixture", environment, AgentContext())
        )
        await self._wait_for_growth(telemetry)
        operation.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await operation

        self.assertTrue(
            environment.remote_survived_cancel,
            "fixture did not reproduce external exec cancellation leaving remote work",
        )
        self.assertFalse((self.logs / "ygg-process-group.pid").exists())
        self._assert_no_fixture_processes()
        self.assertEqual(
            (self.logs / "failure-classification.txt").read_text(),
            "benchmark_timeout\n",
        )
        metadata = json.loads((self.logs / "run-metadata.json").read_text())
        self.assertTrue(metadata["timed_out"])
        self.assertTrue((self.logs / "native-session-manifest.json").is_file())
        self.assertFalse((self.logs / "termination-error.txt").exists())

        mutable = [
            self.logs / "sessions" / "session.jsonl",
            telemetry,
            self.logs / "workspace-heartbeat.txt",
            self.logs / "tool-child-heartbeat.txt",
        ]
        finalized = _snapshot(mutable)
        # This represents verifier execution after agent_execution.finished_at:
        # every provider/session/workspace artifact must remain byte-identical.
        await asyncio.sleep(7)
        self.assertEqual(_snapshot(mutable), finalized)
        self._assert_no_fixture_processes()

    async def test_normal_completion_preserves_output_and_exits_successfully(
        self,
    ) -> None:
        telemetry = self.logs / "ygg-telemetry.jsonl"
        environment = DockerExecEnvironment(self.container, telemetry)
        agent = Ygg(
            self.logs,
            model_name="local-fixture",
            telemetry=True,
            extra_env={"FAKE_MODE": "fixture-control"},
        )
        agent._binary = "/fixture/fake-ygg"

        await agent.run("normal completion control", environment, AgentContext())

        self.assertIn("normal-complete", (self.logs / "stdout.txt").read_text())
        self.assertEqual(
            (self.logs / "failure-classification.txt").read_text(), "success\n"
        )
        self.assertFalse((self.logs / "ygg-process-group.pid").exists())
        self._assert_no_fixture_processes()
        mutable = [self.logs / "sessions" / "session.jsonl", telemetry]
        finalized = _snapshot(mutable)
        await asyncio.sleep(0.25)
        self.assertEqual(_snapshot(mutable), finalized)


if __name__ == "__main__":
    unittest.main()
