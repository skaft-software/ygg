from __future__ import annotations

import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path

from ygg_browse.paths import BrowsePaths, PLAYWRIGHT_VERSION
from ygg_browse.profile import ProfileManager, SENTINEL_NAME
from ygg_browse.safety import BrowseError
from ygg_browse.setup import SetupManager


def create_fake_runtime(temporary: Path) -> None:
    site = temporary / "venv" / "lib" / "python3.11" / "site-packages"
    (site / "playwright").mkdir(parents=True)
    (site / "playwright" / "__init__.py").write_text("", encoding="utf-8")
    metadata = site / f"playwright-{PLAYWRIGHT_VERSION}.dist-info"
    metadata.mkdir()
    (metadata / "METADATA").write_text(
        f"Metadata-Version: 2.1\nName: playwright\nVersion: {PLAYWRIGHT_VERSION}\n",
        encoding="utf-8",
    )
    browser = temporary / "browsers" / "chromium-123"
    browser.mkdir(parents=True)
    executable = browser / "chrome"
    executable.write_bytes(b"fake chromium")
    executable.chmod(0o700)


class ProfileTests(unittest.TestCase):
    def test_create_lock_and_sentinel_guarded_reset(self) -> None:
        with tempfile.TemporaryDirectory() as home:
            paths = BrowsePaths.for_home(Path(home))
            manager = ProfileManager(paths)
            self.assertEqual(manager.inspect(), "absent")
            lease = manager.acquire(create=True)
            self.assertTrue((paths.profile / SENTINEL_NAME).is_file())
            self.assertEqual(manager.inspect(), "ready")
            with self.assertRaises(BrowseError) as raised:
                manager.acquire(create=True)
            self.assertEqual(raised.exception.code, "profile_locked")
            with self.assertRaises(BrowseError):
                manager.reset()
            lease.release()
            self.assertTrue(manager.reset())
            self.assertFalse(paths.profile.exists())
            self.assertFalse(manager.reset())

    def test_invalid_sentinel_and_symlink_are_never_removed(self) -> None:
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as outside:
            paths = BrowsePaths.for_home(Path(home))
            paths.ensure_root()
            external = Path(outside) / "normal-browser-profile"
            external.mkdir()
            (external / "keep").write_text("never delete", encoding="utf-8")
            paths.profile.symlink_to(external, target_is_directory=True)
            manager = ProfileManager(paths)
            self.assertEqual(manager.inspect(), "invalid")
            with self.assertRaises(BrowseError):
                manager.acquire(create=True)
            with self.assertRaises(BrowseError):
                manager.reset()
            self.assertEqual((external / "keep").read_text(encoding="utf-8"), "never delete")

    def test_missing_or_modified_sentinel_refuses_reset(self) -> None:
        with tempfile.TemporaryDirectory() as home:
            paths = BrowsePaths.for_home(Path(home))
            paths.ensure_root()
            paths.profile.mkdir()
            marker = paths.profile / SENTINEL_NAME
            marker.write_text('{"owner":"someone-else"}', encoding="utf-8")
            manager = ProfileManager(paths)
            with self.assertRaises(BrowseError) as raised:
                manager.reset()
            self.assertEqual(raised.exception.code, "invalid_profile_sentinel")
            self.assertTrue(paths.profile.exists())


class SetupTests(unittest.TestCase):
    def test_constructor_and_status_are_inert_before_setup(self) -> None:
        with tempfile.TemporaryDirectory() as home:
            paths = BrowsePaths.for_home(Path(home))
            manager = SetupManager(paths, installer_hook=lambda *_: None)
            self.assertFalse(paths.root.exists())
            status = manager.status()
            self.assertEqual(status.state, "not_set_up")
            self.assertFalse(paths.root.exists())

    def test_confirmed_background_style_install_is_atomic_and_idempotent(self) -> None:
        with tempfile.TemporaryDirectory() as home:
            paths = BrowsePaths.for_home(Path(home))
            entered = threading.Event()
            release = threading.Event()
            events = []

            def installer(temporary: Path, stop: threading.Event, _log: object) -> None:
                entered.set()
                self.assertTrue(release.wait(3))
                self.assertFalse(stop.is_set())
                create_fake_runtime(temporary)

            manager = SetupManager(paths, installer_hook=installer, on_state=events.append)
            started_at = time.monotonic()
            first = manager.start()
            self.assertLess(time.monotonic() - started_at, 1.0)
            self.assertEqual(first.state, "installing")
            self.assertTrue(entered.wait(2))
            self.assertEqual(manager.start().state, "installing")
            self.assertFalse(paths.runtime.exists())
            release.set()
            deadline = time.monotonic() + 5
            while manager.status().state == "installing" and time.monotonic() < deadline:
                time.sleep(0.02)
            self.assertEqual(manager.status().state, "ready")
            manager.validate_runtime()
            self.assertEqual(manager.start().state, "ready")
            self.assertTrue(any(event["state"] == "ready" for event in events))
            self.assertIn(f"playwright=={PLAYWRIGHT_VERSION}", paths.install_log.read_text(encoding="utf-8"))
            manager.shutdown()

    def test_install_log_symlink_is_refused_without_touching_target(self) -> None:
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as outside:
            paths = BrowsePaths.for_home(Path(home))
            paths.ensure_root()
            target = Path(outside) / "unrelated.log"
            target.write_text("keep", encoding="utf-8")
            paths.install_log.symlink_to(target)
            manager = SetupManager(paths, installer_hook=lambda *_: None)
            manager.start()
            deadline = time.monotonic() + 3
            while manager.status().state == "installing" and time.monotonic() < deadline:
                time.sleep(0.02)
            self.assertEqual(target.read_text(encoding="utf-8"), "keep")
            self.assertNotEqual(manager.status().state, "ready")

    def test_runtime_root_symlink_is_never_treated_as_ready(self) -> None:
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as outside:
            paths = BrowsePaths.for_home(Path(home))
            paths.root.parent.mkdir(parents=True)
            paths.root.symlink_to(Path(outside), target_is_directory=True)
            manager = SetupManager(paths)
            self.assertNotEqual(manager.status().state, "ready")
            with self.assertRaises(BrowseError):
                manager.validate_runtime()
            with self.assertRaises(BrowseError):
                manager.start()

    def test_failure_restart_and_partial_runtime_are_never_ready(self) -> None:
        with tempfile.TemporaryDirectory() as home:
            paths = BrowsePaths.for_home(Path(home))

            def fail(_temporary: Path, _stop: threading.Event, _log: object) -> None:
                raise RuntimeError("secret environment detail")

            manager = SetupManager(paths, installer_hook=fail)
            manager.start()
            deadline = time.monotonic() + 3
            while manager.status().state == "installing" and time.monotonic() < deadline:
                time.sleep(0.02)
            status = manager.status()
            self.assertEqual(status.state, "degraded")
            self.assertNotIn("secret environment", status.detail)
            self.assertFalse(paths.runtime.exists())
            restarted = SetupManager(paths, installer_hook=fail)
            self.assertEqual(restarted.status().state, "degraded")

            paths.runtime.mkdir(parents=True)
            self.assertEqual(restarted.status().state, "degraded")
            with self.assertRaises(BrowseError):
                restarted.validate_runtime()

    def test_default_setup_commands_pin_package_and_browser_directory(self) -> None:
        with tempfile.TemporaryDirectory() as home:
            paths = BrowsePaths.for_home(Path(home))
            manager = SetupManager(paths, python_executable="/isolated/host-python")
            temporary = Path(home) / "temporary-runtime"
            temporary.mkdir()
            calls = []

            def record(arguments, _log, *, env):
                calls.append((list(arguments), dict(env) if env is not None else None))

            manager._run_command = record  # type: ignore[method-assign]
            manager._install_pinned_runtime(temporary, object())  # type: ignore[arg-type]
            self.assertEqual(calls[0][0], ["/isolated/host-python", "-m", "venv", str(temporary / "venv")])
            self.assertEqual(calls[0][1]["PYTHONNOUSERSITE"], "1")
            self.assertNotIn("PYTHONPATH", calls[0][1])
            self.assertIn(f"playwright=={PLAYWRIGHT_VERSION}", calls[1][0])
            self.assertEqual(calls[1][0][1:4], ["-m", "pip", "install"])
            self.assertEqual(calls[2][0][-3:], ["playwright", "install", "chromium"])
            self.assertEqual(calls[2][1]["PLAYWRIGHT_BROWSERS_PATH"], str(temporary / "browsers"))

    def test_shutdown_terminates_background_setup_and_marks_not_ready(self) -> None:
        with tempfile.TemporaryDirectory() as home:
            paths = BrowsePaths.for_home(Path(home))
            entered = threading.Event()

            def wait_forever(_temporary: Path, stop: threading.Event, _log: object) -> None:
                entered.set()
                while not stop.wait(0.02):
                    pass

            manager = SetupManager(paths, installer_hook=wait_forever)
            manager.start()
            self.assertTrue(entered.wait(2))
            manager.shutdown(timeout=1)
            self.assertNotEqual(manager.status().state, "ready")
    def test_shutdown_terminates_real_installer_child(self) -> None:
        with tempfile.TemporaryDirectory() as home:
            paths = BrowsePaths.for_home(Path(home))
            child_started = threading.Event()
            manager: SetupManager

            def child_installer(_temporary: Path, _stop: threading.Event, log: object) -> None:
                child_started.set()
                manager._run_command(
                    [sys.executable, "-c", "import time; time.sleep(30)"],
                    log,  # type: ignore[arg-type]
                    env=SetupManager._install_environment(),
                )

            manager = SetupManager(paths, installer_hook=child_installer)
            manager.start()
            self.assertTrue(child_started.wait(2))
            deadline = time.monotonic() + 2
            while manager._process is None and time.monotonic() < deadline:
                time.sleep(0.01)
            process = manager._process
            self.assertIsNotNone(process)
            manager.shutdown(timeout=1)
            self.assertIsNotNone(process.poll())
            self.assertNotEqual(manager.status().state, "ready")


if __name__ == "__main__":
    unittest.main()
