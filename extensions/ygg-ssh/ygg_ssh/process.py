"""Bounded system OpenSSH subprocess ownership and process-tree cleanup."""

from __future__ import annotations

import hashlib
import math
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from dataclasses import dataclass
from typing import Any, Mapping, Optional, Sequence, Union

from .config import Limits


_POSIX_WATCHDOG = r"""
import os, signal, sys, time
parent = int(sys.argv[1])
target = int(sys.argv[2])
def cleanup(*_args):
    try:
        os.killpg(target, signal.SIGKILL)
    except OSError:
        pass
    raise SystemExit(0)
signal.signal(signal.SIGTERM, cleanup)
signal.signal(signal.SIGINT, cleanup)
while True:
    try:
        os.kill(parent, 0)
        os.kill(target, 0)
    except OSError:
        cleanup()
    if os.getppid() != parent:
        cleanup()
    time.sleep(0.05)
"""


# Darwin's sockaddr_un.sun_path has 104 bytes including its terminating NUL.
# OpenSSH first binds ControlPath with a randomized suffix before renaming it,
# so validate the exact adapter basename with conservative suffix headroom.
_UNIX_SOCKET_PATH_BYTES = 104
_OPENSSH_CONTROL_PATH_SUFFIX_HEADROOM = 24
_CONTROL_PATH_BASENAME_SAMPLE = "cm-" + "0" * 24


def _control_path_fits(directory: Path) -> bool:
    control_path = directory / _CONTROL_PATH_BASENAME_SAMPLE
    return (
        len(os.fsencode(control_path)) + _OPENSSH_CONTROL_PATH_SUFFIX_HEADROOM
        < _UNIX_SOCKET_PATH_BYTES
    )


class SshProcessError(RuntimeError):
    """A bounded local OpenSSH operation failed."""

    def __init__(self, code: str, summary: str) -> None:
        super().__init__(summary)
        self.code = code
        self.safe_summary = summary


class SshCancelled(SshProcessError):
    def __init__(self) -> None:
        super().__init__("cancelled", "remote operation was cancelled")


@dataclass(frozen=True)
class ProcessResult:
    exit_status: int
    stdout: bytes
    stderr: bytes
    duration_ms: int
    stdout_truncated: bool = False
    stderr_truncated: bool = False


@dataclass
class MasterHandle:
    alias: str
    control_path: Path
    process: subprocess.Popen[bytes]
    stderr_reader: "_BoundedReader"
    started_at_ms: int


class _BoundedReader:
    def __init__(self, stream: Any, limit: int, name: str) -> None:
        self._stream = stream
        self._limit = max(0, limit)
        self._buffer = bytearray()
        self.truncated = False
        self.thread = threading.Thread(target=self._run, name=name, daemon=True)

    @property
    def data(self) -> bytes:
        return bytes(self._buffer)

    def start(self) -> None:
        self.thread.start()

    def _run(self) -> None:
        try:
            while True:
                chunk = self._stream.read(8192)
                if not chunk:
                    return
                remaining = self._limit - len(self._buffer)
                if remaining > 0:
                    self._buffer.extend(chunk[:remaining])
                if len(chunk) > max(remaining, 0):
                    self.truncated = True
        except (OSError, ValueError):
            return


class OpenSshBackend:
    """Own system ``ssh`` masters and every local descendant process group.

    The backend never invokes a shell locally. It never adds a hostname, user,
    port, jump, identity, or agent option: destination/authentication resolution
    comes only from the exact configured alias and the user's normal OpenSSH
    configuration. Batch mode prevents credential prompts.
    """

    def __init__(
        self,
        limits: Limits,
        *,
        ssh_binary: Optional[Union[os.PathLike[str], str]] = None,
        runtime_directory: Optional[Union[os.PathLike[str], str]] = None,
        environment: Optional[Mapping[str, str]] = None,
    ) -> None:
        self.limits = limits
        self.ssh_binary = self._resolve_binary(ssh_binary)
        self.environment = dict(os.environ if environment is None else environment)
        # Never allow a GUI askpass helper. Existing agent/config/key selection
        # still works, and a missing credential fails closed in BatchMode.
        self.environment["SSH_ASKPASS_REQUIRE"] = "never"
        self.agent_socket_available = bool(self.environment.get("SSH_AUTH_SOCK"))

        requested = Path(runtime_directory) if runtime_directory is not None else None
        self.runtime_directory, self._temporary_runtime = self._find_runtime_directory(
            requested
        )
        self._active: set[subprocess.Popen[bytes]] = set()
        self._watchdogs: dict[subprocess.Popen[bytes], subprocess.Popen[bytes]] = {}
        self._active_lock = threading.RLock()
        self._closed = False

    @staticmethod
    def _find_runtime_directory(requested: Optional[Path]) -> tuple[Path, bool]:
        if requested is not None and _control_path_fits(requested):
            try:
                requested.mkdir(mode=0o700, parents=True, exist_ok=True)
                requested.chmod(0o700)
            except OSError:
                pass
            else:
                return requested, False

        temporary_roots: list[Optional[str]] = []
        if os.name == "posix":
            # macOS's default TMPDIR is long enough to exhaust sun_path after
            # OpenSSH adds its temporary suffix. A private mkdtemp directory
            # below the short /tmp spelling avoids that expansion.
            temporary_roots.append("/tmp")
        temporary_roots.append(None)

        for temporary_root in temporary_roots:
            candidate: Optional[Path] = None
            try:
                candidate = Path(
                    tempfile.mkdtemp(prefix="ygg-ssh-", dir=temporary_root)
                )
                if _control_path_fits(candidate):
                    candidate.chmod(0o700)
                    return candidate, True
            except OSError:
                pass
            if candidate is not None:
                shutil.rmtree(candidate, ignore_errors=True)

        raise SshProcessError(
            "control_path_too_long",
            "no private local path fits the OpenSSH control socket limit",
        )

    @staticmethod
    def _resolve_binary(value: Optional[Union[os.PathLike[str], str]]) -> str:
        if value is not None:
            candidate = os.fspath(value)
        elif Path("/usr/bin/ssh").is_file():
            candidate = "/usr/bin/ssh"
        else:
            candidate = shutil.which("ssh") or ""
        if not candidate:
            raise SshProcessError("ssh_unavailable", "the system OpenSSH client is unavailable")
        if os.path.sep in candidate and not Path(candidate).is_file():
            raise SshProcessError("ssh_unavailable", "the configured OpenSSH client is unavailable")
        return candidate

    def control_path(self, fence: str, generation: int) -> Path:
        digest = hashlib.sha256(f"{fence}:{generation}".encode("utf-8")).hexdigest()[:24]
        return self.runtime_directory / f"cm-{digest}"

    def connect_master(
        self,
        alias: str,
        control_path: Path,
        *,
        cancellation: Any = None,
    ) -> MasterHandle:
        if self._closed:
            raise SshProcessError("shutting_down", "SSH adapter is shutting down")
        try:
            control_path.unlink(missing_ok=True)
        except OSError:
            raise SshProcessError("control_path", "SSH control path could not be prepared")
        timeout_seconds = max(1, math.ceil(self.limits.connect_timeout_ms / 1000))
        arguments = [
            self.ssh_binary,
            "-M",
            "-N",
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "NumberOfPasswordPrompts=0",
            "-o",
            "RequestTTY=no",
            "-o",
            "ClearAllForwardings=yes",
            "-o",
            "ForwardAgent=no",
            "-o",
            "PermitLocalCommand=no",
            "-o",
            "RemoteCommand=none",
            "-o",
            "ControlMaster=yes",
            "-o",
            f"ControlPath={control_path}",
            "-o",
            "ControlPersist=no",
            "-o",
            f"ConnectTimeout={timeout_seconds}",
            "--",
            alias,
        ]
        process = self._spawn(arguments, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL)
        stderr_reader = _BoundedReader(process.stderr, 4096, "ygg-ssh-master-stderr")
        stderr_reader.start()
        handle = MasterHandle(
            alias=alias,
            control_path=control_path,
            process=process,
            stderr_reader=stderr_reader,
            started_at_ms=int(time.time() * 1000),
        )
        deadline = time.monotonic() + self.limits.connect_timeout_ms / 1000
        while True:
            if _cancelled(cancellation):
                self._terminate(process)
                raise SshCancelled()
            if process.poll() is not None:
                self._forget(process)
                stderr_reader.thread.join(timeout=0.1)
                self._close_process_streams(process)
                raise SshProcessError(
                    "connect_failed",
                    "OpenSSH connection setup failed without prompting for credentials",
                )
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                self._terminate(process)
                raise SshProcessError("connect_timeout", "OpenSSH connection setup timed out")
            try:
                probe = self.control(
                    alias,
                    control_path,
                    "check",
                    timeout_ms=min(300, max(50, int(remaining * 1000))),
                    cancellation=cancellation,
                )
            except SshCancelled:
                self._terminate(process)
                raise
            except SshProcessError:
                # A missing/not-yet-ready control socket is expected while the
                # replay-safe master handshake is still within its deadline.
                time.sleep(min(0.05, max(0.0, remaining)))
                continue
            if probe.exit_status == 0:
                return handle
            time.sleep(min(0.05, max(0.0, remaining)))

    def control(
        self,
        alias: str,
        control_path: Path,
        operation: str,
        *,
        timeout_ms: int = 1_000,
        cancellation: Any = None,
    ) -> ProcessResult:
        if operation not in {"check", "exit"}:
            raise ValueError("unsupported OpenSSH control operation")
        arguments = [
            self.ssh_binary,
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "RequestTTY=no",
            "-o",
            "PermitLocalCommand=no",
            "-S",
            str(control_path),
            "-O",
            operation,
            "--",
            alias,
        ]
        return self._run(
            arguments,
            input_bytes=b"",
            timeout_ms=timeout_ms,
            capture_limit=4096,
            cancellation=cancellation,
        )

    def run_remote(
        self,
        alias: str,
        control_path: Path,
        remote_command: str,
        *,
        input_bytes: bytes = b"",
        timeout_ms: Optional[int] = None,
        cancellation: Any = None,
        capture_limit: Optional[int] = None,
    ) -> ProcessResult:
        arguments = [
            self.ssh_binary,
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "NumberOfPasswordPrompts=0",
            "-o",
            "RequestTTY=no",
            "-o",
            "ClearAllForwardings=yes",
            "-o",
            "ForwardAgent=no",
            "-o",
            "PermitLocalCommand=no",
            "-o",
            "RemoteCommand=none",
            "-o",
            "ControlMaster=no",
            "-S",
            str(control_path),
            "--",
            alias,
            remote_command,
        ]
        return self._run(
            arguments,
            input_bytes=input_bytes,
            timeout_ms=timeout_ms or self.limits.operation_timeout_ms,
            capture_limit=(
                self.limits.max_output_bytes if capture_limit is None else capture_limit
            ),
            cancellation=cancellation,
        )

    def master_healthy(self, handle: MasterHandle) -> bool:
        if handle.process.poll() is not None:
            self._forget(handle.process)
            return False
        try:
            return self.control(
                handle.alias,
                handle.control_path,
                "check",
                timeout_ms=min(1_000, self.limits.health_interval_ms),
            ).exit_status == 0
        except SshProcessError:
            return False

    def disconnect_master(self, handle: MasterHandle) -> None:
        if handle.process.poll() is None:
            try:
                self.control(
                    handle.alias,
                    handle.control_path,
                    "exit",
                    timeout_ms=self.limits.shutdown_timeout_ms,
                )
            except SshProcessError:
                pass
        self._terminate(handle.process)
        handle.stderr_reader.thread.join(timeout=0.2)
        try:
            handle.control_path.unlink(missing_ok=True)
        except OSError:
            pass

    def close(self) -> None:
        with self._active_lock:
            self._closed = True
            active = list(self._active)
        for process in active:
            self._terminate(process)
        if self._temporary_runtime:
            shutil.rmtree(self.runtime_directory, ignore_errors=True)

    def _run(
        self,
        arguments: Sequence[str],
        *,
        input_bytes: bytes,
        timeout_ms: int,
        capture_limit: int,
        cancellation: Any,
    ) -> ProcessResult:
        if self._closed:
            raise SshProcessError("shutting_down", "SSH adapter is shutting down")
        started = time.monotonic()
        process = self._spawn(arguments, stdin=subprocess.PIPE, stdout=subprocess.PIPE)
        stdout_reader = _BoundedReader(process.stdout, capture_limit, "ygg-ssh-stdout")
        stderr_reader = _BoundedReader(process.stderr, capture_limit, "ygg-ssh-stderr")
        stdout_reader.start()
        stderr_reader.start()

        def write_input() -> None:
            try:
                if input_bytes:
                    process.stdin.write(input_bytes)
                    process.stdin.flush()
            except (BrokenPipeError, OSError, ValueError):
                pass
            finally:
                try:
                    process.stdin.close()
                except (OSError, ValueError):
                    pass

        writer = threading.Thread(target=write_input, name="ygg-ssh-stdin", daemon=True)
        writer.start()
        deadline = started + timeout_ms / 1000
        failure: Optional[SshProcessError] = None
        while process.poll() is None:
            if _cancelled(cancellation):
                failure = SshCancelled()
                break
            if time.monotonic() >= deadline:
                failure = SshProcessError("timeout", "remote operation timed out")
                break
            time.sleep(0.01)
        if failure is not None:
            self._terminate(process)
        else:
            # The direct ssh process has settled. Nothing local in its private
            # process group is allowed to outlive it (for example a configured
            # ProxyCommand or adversarial fixture descendant).
            self._kill_group(process, signal.SIGKILL)
            self._forget(process)

        writer.join(timeout=self.limits.termination_grace_ms / 1000)
        stdout_reader.thread.join(timeout=self.limits.termination_grace_ms / 1000)
        stderr_reader.thread.join(timeout=self.limits.termination_grace_ms / 1000)
        if stdout_reader.thread.is_alive() or stderr_reader.thread.is_alive():
            # A descendant inherited a pipe after the direct ssh process exited.
            # Kill the original group and close our descriptors so it cannot keep
            # the extension alive or grow retained output.
            self._kill_group(process, signal.SIGKILL)
            for stream in (process.stdout, process.stderr):
                try:
                    stream.close()
                except (OSError, ValueError):
                    pass
            stdout_reader.thread.join(timeout=0.1)
            stderr_reader.thread.join(timeout=0.1)
        if failure is not None:
            self._close_process_streams(process)
            raise failure
        status = process.returncode
        if status is None:
            status = process.wait(timeout=0.1)
        response = ProcessResult(
            exit_status=int(status),
            stdout=stdout_reader.data,
            stderr=stderr_reader.data,
            duration_ms=max(0, int((time.monotonic() - started) * 1000)),
            stdout_truncated=stdout_reader.truncated,
            stderr_truncated=stderr_reader.truncated,
        )
        self._close_process_streams(process)
        return response

    def _spawn(
        self,
        arguments: Sequence[str],
        *,
        stdin: Any,
        stdout: Any,
    ) -> subprocess.Popen[bytes]:
        kwargs: dict[str, Any] = {}
        if os.name == "posix":
            kwargs["start_new_session"] = True
        elif os.name == "nt":  # pragma: no cover - exercised on Windows hosts
            kwargs["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
        try:
            process = subprocess.Popen(
                list(arguments),
                stdin=stdin,
                stdout=stdout,
                stderr=subprocess.PIPE,
                env=self.environment,
                close_fds=True,
                **kwargs,
            )
        except OSError as error:
            raise SshProcessError("ssh_spawn_failed", "the system OpenSSH client could not be started") from error
        watchdog = None
        if os.name == "posix":
            try:
                watchdog = subprocess.Popen(
                    [sys.executable, "-c", _POSIX_WATCHDOG, str(os.getpid()), str(process.pid)],
                    stdin=subprocess.DEVNULL,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                    close_fds=True,
                )
            except OSError as error:
                self._terminate(process)
                raise SshProcessError(
                    "watchdog_failed",
                    "the local SSH process watchdog could not be started",
                ) from error
        with self._active_lock:
            if self._closed:
                if watchdog is not None:
                    watchdog.terminate()
                self._terminate(process)
                raise SshProcessError("shutting_down", "SSH adapter is shutting down")
            self._active.add(process)
            if watchdog is not None:
                self._watchdogs[process] = watchdog
        return process

    def _forget(self, process: subprocess.Popen[bytes]) -> None:
        with self._active_lock:
            self._active.discard(process)
            watchdog = self._watchdogs.pop(process, None)
        if watchdog is not None and watchdog.poll() is None:
            try:
                watchdog.terminate()
                watchdog.wait(timeout=0.2)
            except (OSError, subprocess.TimeoutExpired):
                try:
                    watchdog.kill()
                    watchdog.wait(timeout=0.2)
                except (OSError, subprocess.TimeoutExpired):
                    pass

    def _terminate(self, process: subprocess.Popen[bytes]) -> None:
        if process.poll() is None:
            self._kill_group(process, signal.SIGTERM)
            try:
                process.wait(timeout=self.limits.termination_grace_ms / 1000)
            except subprocess.TimeoutExpired:
                self._kill_group(process, signal.SIGKILL)
                try:
                    process.wait(timeout=self.limits.termination_grace_ms / 1000)
                except subprocess.TimeoutExpired:
                    pass
        else:
            # poll()/wait() may have reaped the group leader while local
            # descendants still hold the private process group.
            self._kill_group(process, signal.SIGKILL)
        self._forget(process)
        self._close_process_streams(process)

    @staticmethod
    def _close_process_streams(process: subprocess.Popen[bytes]) -> None:
        for stream in (process.stdin, process.stdout, process.stderr):
            if stream is None:
                continue
            try:
                stream.close()
            except (OSError, ValueError):
                pass

    @staticmethod
    def _kill_group(process: subprocess.Popen[bytes], sig: signal.Signals) -> None:
        try:
            if os.name == "posix":
                os.killpg(process.pid, sig)
            elif process.poll() is None:  # pragma: no cover - Windows fallback
                if sig == signal.SIGTERM:
                    process.terminate()
                else:
                    process.kill()
        except (OSError, ProcessLookupError):
            pass


def _cancelled(token: Any) -> bool:
    return bool(token is not None and getattr(token, "cancelled", False))
