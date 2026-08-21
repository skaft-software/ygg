from __future__ import annotations

import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock

from ygg_ssh.config import Limits
from ygg_ssh.process import (
    _CONTROL_PATH_BASENAME_SAMPLE,
    _OPENSSH_CONTROL_PATH_SUFFIX_HEADROOM,
    _UNIX_SOCKET_PATH_BYTES,
    OpenSshBackend,
    SshProcessError,
    _control_path_fits,
)


class ControlPathTests(unittest.TestCase):
    def setUp(self) -> None:
        self.limits = Limits(
            connect_timeout_ms=1000,
            operation_timeout_ms=1000,
            max_output_bytes=1000,
            max_file_bytes=1000,
            health_interval_ms=1000,
            shutdown_timeout_ms=1000,
            termination_grace_ms=100,
        )

    def test_101_byte_base_path_is_rejected_before_openssh_adds_its_suffix(self) -> None:
        basename_bytes = len(os.fsencode(_CONTROL_PATH_BASENAME_SAMPLE))
        directory_bytes = 101 - basename_bytes - 1
        directory = Path("/" + "x" * (directory_bytes - 1))
        control_path = directory / _CONTROL_PATH_BASENAME_SAMPLE

        self.assertEqual(len(os.fsencode(control_path)), 101)
        self.assertFalse(_control_path_fits(directory))

    @unittest.skipUnless(os.name == "posix", "POSIX control-socket regression")
    def test_long_macos_temp_path_uses_private_short_fallback_and_cleans_it(self) -> None:
        with tempfile.TemporaryDirectory(dir="/tmp", prefix="ygg-ssh-test-") as root:
            long_temp = Path(root) / ("t" * 100)
            long_temp.mkdir()
            requested = long_temp / "extension-scratch"

            with mock.patch.dict(os.environ, {"TMPDIR": str(long_temp)}):
                with mock.patch.object(
                    OpenSshBackend, "_resolve_binary", return_value="/usr/bin/ssh"
                ):
                    backend = OpenSshBackend(
                        self.limits,
                        runtime_directory=requested,
                    )

            runtime_directory = backend.runtime_directory
            control_path = backend.control_path("owner-fence", 1)
            self.assertEqual(runtime_directory.parent, Path("/tmp"))
            self.assertTrue(backend._temporary_runtime)
            self.assertLess(
                len(os.fsencode(control_path))
                + _OPENSSH_CONTROL_PATH_SUFFIX_HEADROOM,
                _UNIX_SOCKET_PATH_BYTES,
            )
            self.assertEqual(runtime_directory.stat().st_mode & 0o777, 0o700)
            self.assertFalse(requested.exists())

            backend.close()
            self.assertFalse(runtime_directory.exists())

    @unittest.skipUnless(os.name == "posix", "POSIX control-socket regression")
    def test_short_requested_directory_is_private_and_retained(self) -> None:
        with tempfile.TemporaryDirectory(dir="/tmp", prefix="ygg-ssh-test-") as root:
            requested = Path(root) / "control"
            with mock.patch.object(
                OpenSshBackend, "_resolve_binary", return_value="/usr/bin/ssh"
            ):
                backend = OpenSshBackend(
                    self.limits,
                    runtime_directory=requested,
                )

            self.assertEqual(backend.runtime_directory, requested)
            self.assertFalse(backend._temporary_runtime)
            self.assertEqual(requested.stat().st_mode & 0o777, 0o700)
            backend.close()
            self.assertTrue(requested.exists())

    @unittest.skipUnless(os.name == "posix", "POSIX control-socket regression")
    def test_rejected_temporary_candidates_are_removed_before_specific_failure(self) -> None:
        with tempfile.TemporaryDirectory(dir="/tmp", prefix="ygg-ssh-test-") as root:
            candidates = [Path(root) / (character * 100) for character in ("a", "b")]
            for candidate in candidates:
                candidate.mkdir()

            with mock.patch.object(
                OpenSshBackend, "_resolve_binary", return_value="/usr/bin/ssh"
            ):
                with mock.patch(
                    "tempfile.mkdtemp",
                    side_effect=[str(candidate) for candidate in candidates],
                ):
                    with self.assertRaises(SshProcessError) as caught:
                        OpenSshBackend(self.limits)

            self.assertEqual(caught.exception.code, "control_path_too_long")
            self.assertTrue(all(not candidate.exists() for candidate in candidates))

    @unittest.skipUnless(os.name == "posix", "POSIX control-socket regression")
    def test_default_temp_fallback_is_used_when_short_root_is_unavailable(self) -> None:
        with tempfile.TemporaryDirectory(dir="/tmp", prefix="ygg-ssh-test-") as root:
            fallback = Path(root) / "fallback"
            fallback.mkdir()

            with mock.patch.object(
                OpenSshBackend, "_resolve_binary", return_value="/usr/bin/ssh"
            ):
                with mock.patch(
                    "tempfile.mkdtemp",
                    side_effect=[OSError("short root unavailable"), str(fallback)],
                ):
                    backend = OpenSshBackend(self.limits)

            self.assertEqual(backend.runtime_directory, fallback)
            self.assertEqual(fallback.stat().st_mode & 0o777, 0o700)
            backend.close()
            self.assertFalse(fallback.exists())


if __name__ == "__main__":
    unittest.main()
