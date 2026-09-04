"""Focused contract test for the hermetic Pi runtime evidence driver."""

from __future__ import annotations

import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
HARNESS = ROOT / "scripts/bench-pi-runtime.py"


class PiRuntimeEvidenceHarnessTests(unittest.TestCase):
    def test_one_repetition_emits_bounded_all_profile_evidence(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "evidence"
            completed = subprocess.run(
                [
                    sys.executable,
                    str(HARNESS),
                    "--candidate",
                    "fixture-contract-test",
                    "--repetitions",
                    "1",
                    "--max-resource-samples",
                    "4",
                    "--output",
                    str(output),
                ],
                cwd=ROOT,
                check=True,
                capture_output=True,
                text=True,
                timeout=30,
            )
            self.assertIn('"decision": "hold"', completed.stdout)
            artifact = json.loads((output / "results.json").read_text(encoding="utf-8"))
            self.assertEqual("ygg.pi.runtime.evidence.v1", artifact["schema"])
            self.assertEqual("0.3", artifact["api"]["version"])
            self.assertEqual(
                {"no_extension", "legacy_eager", "lazy", "shared_workspace", "pi_aggregate"},
                set(artifact["profiles"]),
            )
            self.assertEqual("hold", artifact["release_decision"]["status"])
            self.assertFalse(artifact["inference_server"]["included"])
            self.assertEqual("hermetic_fixture", artifact["inputs"]["adapter"])
            self.assertEqual("checked_in_fake_pi", artifact["inputs"]["pi_runtime"]["kind"])
            self.assertRegex(artifact["inputs"]["bridge"]["sha256"], r"^[0-9a-f]{64}$")
            for profile in artifact["profiles"].values():
                self.assertEqual(1, len(profile["runs"]))
                for samples in profile["runs"][0]["raw_resource_samples"].values():
                    self.assertLessEqual(len(samples), 4)
            self.assertTrue((output / "SHA256SUMS").is_file())


if __name__ == "__main__":
    unittest.main()
