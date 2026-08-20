"""Confirmed, idempotent background installation of pinned Playwright."""

from __future__ import annotations

import json
import os
import shutil
import stat
import subprocess
import sys
import threading
import time
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Dict, IO, List, Mapping, Optional

from .paths import BrowsePaths, PLAYWRIGHT_VERSION, PathSafetyError
from .safety import BrowseError, ExclusiveFileLock


SETUP_SCHEMA = "ygg.browse.setup.v1"
INSTALL_SENTINEL = ".ygg-browse-installing.json"
COMPLETION_MARKER = ".ygg-browse-complete.json"
MAX_STATE_BYTES = 8192
StateCallback = Callable[[Mapping[str, Any]], None]
InstallerHook = Callable[[Path, threading.Event, IO[str]], None]


@dataclass(frozen=True)
class SetupStatus:
    state: str
    detail: str
    log_path: str

    def as_dict(self) -> Dict[str, str]:
        return {"state": self.state, "detail": self.detail, "log_path": self.log_path}


class SetupManager:
    """Own one background installer and its cross-process completion state."""

    def __init__(
        self,
        paths: BrowsePaths,
        *,
        on_state: Optional[StateCallback] = None,
        installer_hook: Optional[InstallerHook] = None,
        python_executable: Optional[str] = None,
    ) -> None:
        self.paths = paths
        self._on_state = on_state or (lambda _status: None)
        self._installer_hook = installer_hook
        self._python = python_executable or sys.executable
        self._mutex = threading.RLock()
        self._thread: Optional[threading.Thread] = None
        self._stop = threading.Event()
        self._process: Optional[subprocess.Popen[Any]] = None

    def status(self) -> SetupStatus:
        log = self.paths.display(self.paths.install_log)
        try:
            self.validate_runtime()
            return SetupStatus("ready", f"Playwright {PLAYWRIGHT_VERSION} and Chromium are ready.", log)
        except BrowseError as runtime_error:
            runtime_present = self.paths.runtime.exists() or self.paths.runtime.is_symlink()

        with self._mutex:
            if self._thread is not None and self._thread.is_alive():
                return SetupStatus("installing", "Pinned browser setup is running in the background.", log)

        if self._lock_is_held_elsewhere():
            return SetupStatus("installing", "Pinned browser setup is running in another Ygg process.", log)

        state = self._read_state()
        if state.get("state") == "installing":
            return SetupStatus(
                "degraded",
                "A previous browser setup was interrupted; run /browse setup to retry.",
                log,
            )
        if state.get("state") in {"failed", "cancelled"}:
            return SetupStatus(
                "degraded",
                "Pinned browser setup did not complete; run /browse setup to retry.",
                log,
            )
        if runtime_present:
            return SetupStatus("degraded", runtime_error.message, log)
        return SetupStatus("not_set_up", "Pinned browser dependencies are not installed.", log)

    def start(self) -> SetupStatus:
        """Acquire the install lock and return immediately after starting a thread."""

        current = self.status()
        if current.state in {"ready", "installing"}:
            return current
        try:
            self.paths.ensure_root()
            self.paths.ensure_directory(self.paths.runtime_parent)
        except PathSafetyError as error:
            raise BrowseError("unsafe_runtime", str(error)) from error
        with self._mutex:
            if self._thread is not None and self._thread.is_alive():
                return self.status()
            lock = ExclusiveFileLock(self.paths.install_lock)
            if not lock.acquire():
                return SetupStatus(
                    "installing",
                    "Pinned browser setup is running in another Ygg process.",
                    self.paths.display(self.paths.install_log),
                )
            self._stop.clear()
            self._write_state("installing")
            thread = threading.Thread(
                target=self._install_background,
                args=(lock,),
                name="ygg-browse-installer",
                daemon=True,
            )
            self._thread = thread
            try:
                thread.start()
            except BaseException as error:
                self._thread = None
                lock.release()
                self._write_state("failed")
                raise BrowseError(
                    "setup_failed",
                    "The background browser setup worker could not be started.",
                ) from error
        status = SetupStatus(
            "installing",
            "Pinned Playwright and Chromium setup started in the background.",
            self.paths.display(self.paths.install_log),
        )
        self._emit(status)
        return status

    def shutdown(self, timeout: float = 1.0) -> None:
        self._stop.set()
        with self._mutex:
            process = self._process
            thread = self._thread
        if process is not None and process.poll() is None:
            try:
                process.terminate()
            except OSError:
                pass
            try:
                process.wait(timeout=max(0.05, timeout / 2))
            except subprocess.TimeoutExpired:
                try:
                    process.kill()
                except OSError:
                    pass
        if thread is not None:
            thread.join(timeout=max(0.0, timeout))

    def site_packages(self) -> Path:
        self.validate_runtime()
        candidates = self._site_package_candidates(self.paths.runtime)
        if len(candidates) != 1:
            raise BrowseError(
                "runtime_invalid", "The isolated Playwright package layout is invalid; rerun setup."
            )
        return candidates[0]

    def validate_runtime(self, runtime: Optional[Path] = None) -> None:
        path = self.paths.runtime if runtime is None else runtime
        for owned_directory, label in (
            (self.paths.root, "Ygg Browse root"),
            (self.paths.runtime_parent, "Ygg Browse runtime parent"),
        ):
            try:
                owned_metadata = owned_directory.lstat()
            except FileNotFoundError as error:
                raise BrowseError("not_set_up", "Pinned browser dependencies are not installed.") from error
            if stat.S_ISLNK(owned_metadata.st_mode) or not stat.S_ISDIR(owned_metadata.st_mode):
                raise BrowseError("unsafe_runtime", f"{label} must be a non-symlink directory.")
        try:
            metadata = path.lstat()
        except FileNotFoundError as error:
            raise BrowseError("not_set_up", "Pinned browser dependencies are not installed.") from error
        if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
            raise BrowseError("unsafe_runtime", "The Ygg Browse runtime must be a non-symlink directory.")
        marker = self._read_bounded_json(path / COMPLETION_MARKER)
        expected = {
            "schema": SETUP_SCHEMA,
            "complete": True,
            "playwright_version": PLAYWRIGHT_VERSION,
        }
        if marker != expected:
            raise BrowseError(
                "runtime_incomplete", "The pinned browser runtime has no valid atomic completion marker."
            )
        candidates = self._site_package_candidates(path)
        if len(candidates) != 1:
            raise BrowseError("runtime_invalid", "The isolated Playwright package is missing or ambiguous.")
        site = candidates[0]
        package = site / "playwright"
        all_distributions = [
            candidate
            for candidate in site.glob("playwright-*.dist-info")
            if self._real_directory(candidate)
        ]
        distributions = [
            candidate
            for candidate in all_distributions
            if candidate.name == f"playwright-{PLAYWRIGHT_VERSION}.dist-info"
        ]
        if (
            not self._real_directory(package)
            or len(all_distributions) != 1
            or len(distributions) != 1
        ):
            raise BrowseError("runtime_invalid", "The exact pinned Playwright package is unavailable.")
        metadata_file = distributions[0] / "METADATA"
        try:
            metadata_stat = metadata_file.lstat()
            if stat.S_ISLNK(metadata_stat.st_mode) or not stat.S_ISREG(metadata_stat.st_mode):
                raise BrowseError("runtime_invalid", "The pinned Playwright package metadata is invalid.")
            flags = os.O_RDONLY | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
            fd = os.open(str(metadata_file), flags)
            try:
                metadata_text = os.read(fd, 65537).decode("utf-8", errors="strict")
            finally:
                os.close(fd)
            if len(metadata_text.encode("utf-8")) > 65536:
                raise BrowseError("runtime_invalid", "The pinned Playwright package metadata is invalid.")
        except BrowseError:
            raise
        except (OSError, UnicodeError) as error:
            raise BrowseError("runtime_invalid", "The pinned Playwright package metadata is invalid.") from error
        if f"\nVersion: {PLAYWRIGHT_VERSION}\n" not in "\n" + metadata_text + "\n":
            raise BrowseError("runtime_invalid", "The installed Playwright version is not exactly pinned.")
        browsers = path / "browsers"
        if not self._real_directory(browsers):
            raise BrowseError("runtime_invalid", "The isolated Chromium runtime is missing.")
        chromium_directories = [
            child
            for child in browsers.iterdir()
            if child.name.startswith("chromium-") and self._real_directory(child)
        ]
        if len(chromium_directories) != 1 or not self._contains_regular_file(chromium_directories[0]):
            raise BrowseError("runtime_invalid", "The isolated Chromium runtime is incomplete.")

    def _install_background(self, lock: ExclusiveFileLock) -> None:
        temporary: Optional[Path] = None
        try:
            self._cleanup_stale_temporary_directories()
            temporary = self.paths.runtime_parent / (
                f".playwright-{PLAYWRIGHT_VERSION}.tmp-{uuid.uuid4().hex}"
            )
            temporary.mkdir(mode=0o700)
            self._write_json(
                temporary / INSTALL_SENTINEL,
                {"schema": SETUP_SCHEMA, "playwright_version": PLAYWRIGHT_VERSION},
            )
            with self._open_install_log(append=False) as log:
                log.write(f"Ygg Browse setup: playwright=={PLAYWRIGHT_VERSION}\n")
                log.flush()
                if self._installer_hook is not None:
                    self._installer_hook(temporary, self._stop, log)
                else:
                    self._install_pinned_runtime(temporary, log)
            if self._stop.is_set():
                raise BrowseError("setup_cancelled", "Browser setup was cancelled during shutdown.")
            self._write_json(
                temporary / COMPLETION_MARKER,
                {
                    "schema": SETUP_SCHEMA,
                    "complete": True,
                    "playwright_version": PLAYWRIGHT_VERSION,
                },
            )
            self.validate_runtime(temporary)
            if self.paths.runtime.exists() or self.paths.runtime.is_symlink():
                try:
                    self.validate_runtime()
                except BrowseError as error:
                    raise BrowseError(
                        "unsafe_runtime",
                        "An invalid existing runtime was not replaced; remove it manually after inspection.",
                    ) from error
                self._safe_remove_temporary(temporary)
                temporary = None
            else:
                os.replace(str(temporary), str(self.paths.runtime))
                self._fsync_directory(self.paths.runtime_parent)
                temporary = None
            self._write_state("ready")
            self._emit(
                SetupStatus(
                    "ready",
                    f"Playwright {PLAYWRIGHT_VERSION} and Chromium are ready.",
                    self.paths.display(self.paths.install_log),
                )
            )
        except BaseException as error:
            cancelled = self._stop.is_set()
            self._write_state("cancelled" if cancelled else "failed")
            if not cancelled:
                try:
                    with self._open_install_log(append=True) as log:
                        log.write("setup failed; inspect this local log for details\n")
                except (OSError, BrowseError):
                    pass
            self._emit(
                SetupStatus(
                    "degraded",
                    "Pinned browser setup was cancelled."
                    if cancelled
                    else "Pinned browser setup failed; run /browse setup to retry.",
                    self.paths.display(self.paths.install_log),
                )
            )
            # Do not log or persist the exception text: pip/download diagnostics
            # remain only in the local install log and never enter protocol data.
            _ = error
        finally:
            if temporary is not None:
                try:
                    self._safe_remove_temporary(temporary)
                except BrowseError:
                    pass
            with self._mutex:
                self._process = None
                self._thread = None
            lock.release()

    def _open_install_log(self, *, append: bool) -> IO[str]:
        flags = os.O_WRONLY | os.O_CREAT
        flags |= os.O_APPEND if append else os.O_TRUNC
        flags |= getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
        fd = os.open(str(self.paths.install_log), flags, 0o600)
        try:
            metadata = os.fstat(fd)
            if not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1:
                raise BrowseError("unsafe_install_log", "The install log must be a regular owned file.")
            try:
                os.fchmod(fd, 0o600)
            except OSError:
                pass
            return os.fdopen(fd, "a" if append else "w", encoding="utf-8", errors="replace")
        except BaseException:
            os.close(fd)
            raise

    def _install_pinned_runtime(self, temporary: Path, log: IO[str]) -> None:
        venv = temporary / "venv"
        base_environment = self._install_environment()
        self._run_command(
            [self._python, "-m", "venv", str(venv)],
            log,
            env=base_environment,
        )
        python = self._venv_python(venv)
        self._run_command(
            [
                str(python),
                "-m",
                "pip",
                "install",
                "--disable-pip-version-check",
                "--no-input",
                "--no-cache-dir",
                f"playwright=={PLAYWRIGHT_VERSION}",
            ],
            log,
            env=base_environment,
        )
        environment = dict(base_environment)
        environment["PLAYWRIGHT_BROWSERS_PATH"] = str(temporary / "browsers")
        environment["PLAYWRIGHT_DOWNLOAD_CONNECTION_TIMEOUT"] = "120000"
        self._run_command(
            [str(python), "-m", "playwright", "install", "chromium"],
            log,
            env=environment,
        )

    @staticmethod
    def _install_environment() -> Dict[str, str]:
        environment = os.environ.copy()
        # A caller-controlled Python path could make the venv invoke an ambient
        # Playwright package despite the exact pip pin. Keep ordinary proxy/TLS
        # variables for the confirmed network download, but remove Python
        # package-selection overrides.
        for name in ("PYTHONPATH", "PYTHONHOME", "VIRTUAL_ENV", "PLAYWRIGHT_BROWSERS_PATH"):
            environment.pop(name, None)
        environment["PYTHONNOUSERSITE"] = "1"
        environment["PIP_DISABLE_PIP_VERSION_CHECK"] = "1"
        return environment

    def _run_command(
        self,
        arguments: List[str],
        log: IO[str],
        *,
        env: Optional[Mapping[str, str]],
    ) -> None:
        if self._stop.is_set():
            raise BrowseError("setup_cancelled", "Browser setup was cancelled during shutdown.")
        process = subprocess.Popen(
            arguments,
            stdin=subprocess.DEVNULL,
            stdout=log,
            stderr=subprocess.STDOUT,
            env=dict(env) if env is not None else None,
            close_fds=True,
        )
        with self._mutex:
            self._process = process
        try:
            while process.poll() is None:
                if self._stop.wait(0.1):
                    try:
                        process.terminate()
                    except OSError:
                        pass
                    try:
                        process.wait(timeout=0.5)
                    except subprocess.TimeoutExpired:
                        try:
                            process.kill()
                        except OSError:
                            pass
                    raise BrowseError("setup_cancelled", "Browser setup was cancelled during shutdown.")
            if process.returncode != 0:
                raise BrowseError("setup_failed", "A pinned dependency setup step failed.")
        finally:
            with self._mutex:
                if self._process is process:
                    self._process = None

    def _cleanup_stale_temporary_directories(self) -> None:
        try:
            children = list(self.paths.runtime_parent.iterdir())
        except FileNotFoundError:
            return
        prefix = f".playwright-{PLAYWRIGHT_VERSION}.tmp-"
        for child in children:
            if child.name.startswith(prefix):
                self._safe_remove_temporary(child)

    def _safe_remove_temporary(self, path: Path) -> None:
        try:
            metadata = path.lstat()
        except FileNotFoundError:
            return
        if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
            raise BrowseError("unsafe_runtime", "A setup staging path is not a real directory.")
        marker = self._read_bounded_json(path / INSTALL_SENTINEL)
        if marker != {"schema": SETUP_SCHEMA, "playwright_version": PLAYWRIGHT_VERSION}:
            raise BrowseError("unsafe_runtime", "A setup staging path has no valid ownership sentinel.")
        shutil.rmtree(path)

    def _lock_is_held_elsewhere(self) -> bool:
        if not self.paths.root.exists() or self.paths.root.is_symlink():
            return False
        lock = ExclusiveFileLock(self.paths.install_lock)
        try:
            acquired = lock.acquire()
        except BrowseError:
            return False
        if acquired:
            lock.release()
            return False
        return True

    def _write_state(self, state: str) -> None:
        try:
            self.paths.ensure_root()
            self._write_json(
                self.paths.setup_state,
                {"schema": SETUP_SCHEMA, "state": state, "updated_unix": int(time.time())},
            )
        except (OSError, PathSafetyError, BrowseError):
            pass

    def _read_state(self) -> Mapping[str, Any]:
        try:
            value = self._read_bounded_json(self.paths.setup_state)
        except BrowseError:
            return {}
        if not isinstance(value, Mapping) or value.get("schema") != SETUP_SCHEMA:
            return {}
        return value

    @staticmethod
    def _read_bounded_json(path: Path) -> Any:
        try:
            metadata = path.lstat()
        except FileNotFoundError as error:
            raise BrowseError("missing_file", "An expected Ygg Browse state file is missing.") from error
        if (
            stat.S_ISLNK(metadata.st_mode)
            or not stat.S_ISREG(metadata.st_mode)
            or metadata.st_nlink != 1
            or metadata.st_size > MAX_STATE_BYTES
        ):
            raise BrowseError("unsafe_state", "A Ygg Browse state file is invalid.")
        flags = os.O_RDONLY | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
        try:
            fd = os.open(str(path), flags)
            try:
                opened = os.fstat(fd)
                if (
                    not stat.S_ISREG(opened.st_mode)
                    or opened.st_nlink != 1
                    or (opened.st_dev, opened.st_ino) != (metadata.st_dev, metadata.st_ino)
                ):
                    raise BrowseError("unsafe_state", "A Ygg Browse state file changed during validation.")
                raw = os.read(fd, MAX_STATE_BYTES + 1)
            finally:
                os.close(fd)
            return json.loads(raw.decode("utf-8"))
        except (OSError, UnicodeError, json.JSONDecodeError) as error:
            raise BrowseError("unsafe_state", "A Ygg Browse state file is invalid.") from error

    @staticmethod
    def _write_json(path: Path, value: Mapping[str, Any]) -> None:
        temporary = path.with_name(f".{path.name}.{uuid.uuid4().hex}.tmp")
        payload = (json.dumps(dict(value), sort_keys=True, separators=(",", ":")) + "\n").encode(
            "utf-8"
        )
        if len(payload) > MAX_STATE_BYTES:
            raise BrowseError("state_too_large", "A Ygg Browse state record exceeded its bound.")
        flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
        flags |= getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
        fd = os.open(str(temporary), flags, 0o600)
        try:
            os.write(fd, payload)
            os.fsync(fd)
        finally:
            os.close(fd)
        os.replace(str(temporary), str(path))
        SetupManager._fsync_directory(path.parent)

    @staticmethod
    def _fsync_directory(path: Path) -> None:
        flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_CLOEXEC", 0)
        try:
            fd = os.open(str(path), flags)
        except OSError:
            return
        try:
            os.fsync(fd)
        except OSError:
            pass
        finally:
            os.close(fd)

    @staticmethod
    def _venv_python(venv: Path) -> Path:
        unix = venv / "bin" / "python"
        windows = venv / "Scripts" / "python.exe"
        return windows if windows.exists() else unix

    @staticmethod
    def _site_package_candidates(runtime: Path) -> List[Path]:
        candidates: List[Path] = []
        unix = runtime / "venv" / "lib"
        if unix.is_dir() and not unix.is_symlink():
            for candidate in unix.glob("python*/site-packages"):
                if candidate.is_dir() and not candidate.is_symlink():
                    candidates.append(candidate)
        windows = runtime / "venv" / "Lib" / "site-packages"
        if windows.is_dir() and not windows.is_symlink():
            candidates.append(windows)
        return candidates

    @staticmethod
    def _real_directory(path: Path) -> bool:
        try:
            metadata = path.lstat()
        except FileNotFoundError:
            return False
        return stat.S_ISDIR(metadata.st_mode) and not stat.S_ISLNK(metadata.st_mode)

    @staticmethod
    def _contains_regular_file(path: Path) -> bool:
        try:
            for candidate in path.rglob("*"):
                try:
                    metadata = candidate.lstat()
                except OSError:
                    continue
                if stat.S_ISREG(metadata.st_mode) and not stat.S_ISLNK(metadata.st_mode):
                    return True
        except OSError:
            return False
        return False

    def _emit(self, status: SetupStatus) -> None:
        try:
            self._on_state(status.as_dict())
        except Exception:
            pass
