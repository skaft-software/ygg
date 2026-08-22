"""Owner-fenced SSH sessions, bounded operations, health, and recovery."""

from __future__ import annotations

import base64
from collections import deque
from dataclasses import dataclass, field
import hashlib
import json
from pathlib import PurePosixPath
import shlex
import threading
import time
from typing import Any, Callable, Mapping, Optional, Sequence

from .config import SshConfig, Target
from .process import MasterHandle, OpenSshBackend, ProcessResult, SshCancelled, SshProcessError


class AdapterError(RuntimeError):
    """A safe, bounded error suitable for a tool result."""

    def __init__(self, code: str, summary: str, *, ambiguous: bool = False) -> None:
        super().__init__(summary)
        self.code = code
        self.safe_summary = summary
        self.ambiguous = ambiguous


@dataclass(frozen=True, order=True)
class OwnerFence:
    session_id: str
    extension_instance_id: str
    process_generation: int
    host_session_id: Optional[str] = field(default=None, compare=False)

    @classmethod
    def from_context(cls, context: Mapping[str, Any]) -> "OwnerFence":
        value = context.get("resource_owner")
        if not isinstance(value, Mapping):
            raise AdapterError(
                "owner_required",
                "SSH operations require a host-derived API 0.2 resource owner",
            )
        session_id = value.get("session_id")
        instance_id = value.get("extension_instance_id")
        generation = value.get("process_generation")
        if (
            not isinstance(session_id, str)
            or not session_id
            or len(session_id.encode("utf-8")) > 512
            or not isinstance(instance_id, str)
            or not instance_id
            or len(instance_id.encode("utf-8")) > 512
            or isinstance(generation, bool)
            or not isinstance(generation, int)
            or generation < 0
            or generation > 2**64 - 1
        ):
            raise AdapterError("owner_invalid", "the host-derived SSH resource owner is invalid")
        host = context.get("host")
        host_session_id = host.get("session_id") if isinstance(host, Mapping) else None
        if (
            not isinstance(host_session_id, str)
            or not host_session_id
            or len(host_session_id.encode("utf-8")) > 512
        ):
            host_session_id = None
        return cls(session_id, instance_id, generation, host_session_id)

    @property
    def wire(self) -> dict[str, Any]:
        return {
            "session_id": self.session_id,
            "extension_instance_id": self.extension_instance_id,
            "process_generation": self.process_generation,
        }

    @property
    def opaque_id(self) -> str:
        value = f"{self.session_id}\0{self.extension_instance_id}\0{self.process_generation}"
        return hashlib.sha256(value.encode("utf-8")).hexdigest()[:16]

    @property
    def process_fence(self) -> str:
        value = f"{self.extension_instance_id}:{self.process_generation}"
        return hashlib.sha256(value.encode("utf-8")).hexdigest()[:12]


@dataclass
class Selection:
    target_id: str
    desired_connected: bool = True


@dataclass
class Connection:
    owner: OwnerFence
    target: Target
    generation: int
    state: str = "connecting"
    master: Optional[MasterHandle] = None
    connected_at_ms: Optional[int] = None
    last_health_ms: Optional[int] = None
    last_error_code: Optional[str] = None
    last_error: Optional[str] = None
    ambiguous_mutation: bool = False
    operation_lock: threading.RLock = field(default_factory=threading.RLock, repr=False)


@dataclass
class Activity:
    id: str
    owner_id: str
    alias: str
    command_class: str
    state: str
    connection_generation: int
    started_at_ms: int
    completed_at_ms: Optional[int] = None
    exit_status: Optional[int] = None
    outcome: Optional[str] = None


READ_SCRIPT = r'''set -eu
cd "$1"
[ -f "$2" ] || exit 66
exec dd if="$2" bs=1 skip="$3" count="$4" 2>/dev/null
'''

# Directory listing. Exit 66 means the path is missing or not a directory;
# the manager maps that to a structured remote_not_found error instead of a
# generic failure so models can self-correct without blind re-probing.
LIST_SCRIPT = r'''set -eu
cd "$1"
[ -d "$2" ] || exit 66
cd "$2"
pwd
ls -Ap
'''

WRITE_SCRIPT = r'''set -eu
cd "$1"
path=./$2
overwrite=$3
dir=${path%/*}
[ "$dir" != "$path" ] || dir=.
[ -d "$dir" ] || exit 66
[ ! -d "$path" ] || exit 73
if [ "$overwrite" != 1 ] && { [ -e "$path" ] || [ -L "$path" ]; }; then exit 73; fi
tmp=$(mktemp "$dir/.ygg-ssh.XXXXXX") || exit 74
trap 'rm -f "$tmp"' EXIT HUP INT TERM
cat >"$tmp"
if [ "$overwrite" != 1 ]; then
    if ! ln "$tmp" "$path"; then exit 73; fi
    rm -f "$tmp"
else
    [ ! -d "$path" ] || exit 73
    mv -f "$tmp" "$path"
fi
trap - EXIT HUP INT TERM
'''


class SshManager:
    """Sole owner of all authenticated OpenSSH connection state."""

    def __init__(
        self,
        config: SshConfig,
        backend: OpenSshBackend,
        *,
        confirm: Optional[Callable[[str, str, bool], bool]] = None,
        publisher: Optional[
            Callable[[Mapping[str, Any], Optional[Mapping[str, Any]]], None]
        ] = None,
        logger: Any = None,
        configuration_error: Optional[str] = None,
        clock: Optional[Callable[[], int]] = None,
    ) -> None:
        self.config = config
        self.configuration_error = configuration_error
        self.backend = backend
        self._confirm = confirm
        self._publisher = publisher
        self._logger = logger
        self._clock = clock or (lambda: int(time.time() * 1000))
        self._targets = {target.id: target for target in config.targets}
        self._selections: dict[str, Selection] = {}
        self._latest_owner: dict[str, OwnerFence] = {}
        self._connections: dict[OwnerFence, Connection] = {}
        self._generation: dict[tuple[OwnerFence, str, str], int] = {}
        self._activities: deque[Activity] = deque(maxlen=config.limits.max_activities)
        self._activity_sequence = 0
        self._presentation_revision = 0
        self._presentation_active = False
        self._diagnostics: deque[dict[str, Any]] = deque(maxlen=256)
        self._lock = threading.RLock()
        self._publish_lock = threading.Lock()
        self._stop = threading.Event()
        self._shutting_down = False
        self._health_thread = threading.Thread(
            target=self._health_loop, name="ygg-ssh-health", daemon=True
        )
        self._health_thread.start()

    @property
    def diagnostics(self) -> tuple[dict[str, Any], ...]:
        with self._lock:
            return tuple(dict(item) for item in self._diagnostics)

    def activate_presentation(self) -> None:
        with self._lock:
            self._presentation_active = True
        self._publish_current()

    def target_ids(self) -> list[str]:
        return sorted(target.id for target in self.config.targets if target.enabled)

    def status(self, context: Mapping[str, Any], *, cancellation: Any = None) -> dict[str, Any]:
        owner = OwnerFence.from_context(context)
        self._admit_owner(owner)
        connection = self._connection_for_owner(owner, establish_pending=True, cancellation=cancellation)
        if connection is None:
            with self._lock:
                selection = self._selections.get(owner.session_id)
            return {
                "connected": False,
                "state": "disconnected",
                "target_id": selection.target_id if selection else None,
                "alias": self._target_alias(selection.target_id) if selection else None,
                "authority": None,
                "remote_cwd": None,
                "connection_generation": None,
                "ambiguous": False,
                "health": "idle",
                "last_error": None,
                "configured_targets": self.target_ids(),
                "agent_socket_available": self.backend.agent_socket_available,
            }
        with connection.operation_lock:
            self._observe_master_exit(connection)
            return self._connection_status(connection)

    def execute(
        self,
        context: Mapping[str, Any],
        argv: Sequence[str],
        *,
        timeout_ms: Optional[int] = None,
        cancellation: Any = None,
    ) -> dict[str, Any]:
        command = self._validate_argv(argv)
        timeout = self._validate_timeout(timeout_ms)
        owner = OwnerFence.from_context(context)
        connection = self._require_ready(owner, cancellation=cancellation)
        if connection.target.authority != "read-write":
            raise AdapterError(
                "read_only",
                "the selected SSH target is configured read-only; ssh_exec is disabled",
            )
        self._require_mutation_approval(connection, "remote command execution")
        remote_command = "cd " + shlex.quote(connection.target.remote_cwd) + " && exec " + " ".join(
            shlex.quote(item) for item in command
        )
        return self._run_operation(
            connection,
            command_class="mutation",
            summary="Remote command",
            remote_command=remote_command,
            input_bytes=b"",
            timeout_ms=timeout,
            cancellation=cancellation,
            mutation=True,
        )

    def read_file(
        self,
        context: Mapping[str, Any],
        path: str,
        *,
        offset: int = 0,
        max_bytes: Optional[int] = None,
        timeout_ms: Optional[int] = None,
        cancellation: Any = None,
    ) -> dict[str, Any]:
        safe_path = self._validate_relative_path(path)
        if isinstance(offset, bool) or not isinstance(offset, int) or not 0 <= offset <= 2**63 - 1:
            raise AdapterError("invalid_offset", "remote file offset is outside the supported bound")
        limit = self.config.limits.max_file_bytes if max_bytes is None else max_bytes
        if isinstance(limit, bool) or not isinstance(limit, int) or not 1 <= limit <= self.config.limits.max_file_bytes:
            raise AdapterError(
                "invalid_limit",
                f"remote read max_bytes must be between 1 and {self.config.limits.max_file_bytes}",
            )
        timeout = self._validate_timeout(timeout_ms)
        owner = OwnerFence.from_context(context)
        connection = self._require_ready(owner, cancellation=cancellation)
        remote_command = _remote_sh(
            READ_SCRIPT,
            [connection.target.remote_cwd, safe_path, str(offset), str(limit)],
        )
        activity = self._start_activity(connection, "read", "Remote file read")
        try:
            result = self.backend.run_remote(
                connection.target.alias,
                connection.master.control_path,
                remote_command,
                timeout_ms=timeout,
                cancellation=cancellation,
                capture_limit=limit,
            )
        except SshCancelled:
            self._finish_activity(activity, "cancelled", None)
            self._diagnose(connection, "read", "cancelled", None, None)
            raise AdapterError("cancelled", "remote file read was cancelled")
        except SshProcessError as error:
            self._finish_activity(activity, "failed", None)
            self._diagnose(connection, "read", error.code, None, None)
            raise AdapterError(error.code, error.safe_summary) from error
        if result.exit_status == 255 or result.exit_status < 0:
            self._degrade_connection(connection, "connection_lost", "OpenSSH connection was lost")
            self._finish_activity(activity, "failed", result.exit_status)
            self._diagnose(
                connection,
                "read",
                "connection_lost",
                result.duration_ms,
                result.exit_status,
            )
            raise AdapterError("connection_lost", "OpenSSH connection was lost during a remote read")
        if result.exit_status == 66:
            self._finish_activity(activity, "failed", result.exit_status)
            self._diagnose(connection, "read", "remote_not_found", result.duration_ms, result.exit_status)
            raise AdapterError(
                "remote_not_found",
                f"remote path {safe_path!r} does not exist below the configured cwd",
            )
        if result.exit_status != 0:
            self._finish_activity(activity, "failed", result.exit_status)
            self._diagnose(connection, "read", "remote_read_failed", result.duration_ms, result.exit_status)
            raise AdapterError("remote_read_failed", "the bounded remote file read failed")
        encoded = _encode_bytes(result.stdout)
        truncated = result.stdout_truncated or len(result.stdout) >= limit
        self._finish_activity(activity, "succeeded", result.exit_status)
        self._diagnose(connection, "read", "succeeded", result.duration_ms, result.exit_status)
        return {
            "ok": True,
            "remote": True,
            "alias": connection.target.alias,
            "command_class": "read",
            "connection_generation": connection.generation,
            "offset": offset,
            "bytes": len(result.stdout),
            "encoding": encoded[0],
            "data": encoded[1],
            "truncated": truncated,
            "untrusted": True,
        }

    def list_dir(
        self,
        context: Mapping[str, Any],
        path: str,
        *,
        timeout_ms: Optional[int] = None,
        cancellation: Any = None,
    ) -> dict[str, Any]:
        """List one directory below the configured remote cwd (read class).

        An omitted or empty path lists the configured remote cwd itself;
        every other value must be a normalized relative subpath.
        """
        if path in (None, ""):
            safe_path = "."
        else:
            safe_path = self._validate_relative_path(path)
        limit = self.config.limits.max_file_bytes
        timeout = self._validate_timeout(timeout_ms)
        owner = OwnerFence.from_context(context)
        connection = self._require_ready(owner, cancellation=cancellation)
        remote_command = _remote_sh(
            LIST_SCRIPT,
            [connection.target.remote_cwd, safe_path],
        )
        activity = self._start_activity(connection, "read", "Remote directory listing")
        try:
            result = self.backend.run_remote(
                connection.target.alias,
                connection.master.control_path,
                remote_command,
                timeout_ms=timeout,
                cancellation=cancellation,
                capture_limit=limit,
            )
        except SshCancelled:
            self._finish_activity(activity, "cancelled", None)
            self._diagnose(connection, "list", "cancelled", None, None)
            raise AdapterError("cancelled", "remote directory listing was cancelled") from None
        except SshProcessError as error:
            self._finish_activity(activity, "failed", None)
            self._diagnose(connection, "list", error.code, None, None)
            raise AdapterError(error.code, error.safe_summary) from error
        if result.exit_status == 255 or result.exit_status < 0:
            self._degrade_connection(connection, "connection_lost", "OpenSSH connection was lost")
            self._finish_activity(activity, "failed", result.exit_status)
            self._diagnose(
                connection,
                "list",
                "connection_lost",
                result.duration_ms,
                result.exit_status,
            )
            raise AdapterError(
                "connection_lost", "OpenSSH connection was lost during a remote listing"
            )
        if result.exit_status == 66:
            self._finish_activity(activity, "failed", result.exit_status)
            self._diagnose(connection, "list", "remote_not_found", result.duration_ms, result.exit_status)
            raise AdapterError(
                "remote_not_found",
                f"remote path {safe_path!r} does not exist or is not a directory below the configured cwd",
            )
        if result.exit_status != 0:
            self._finish_activity(activity, "failed", result.exit_status)
            self._diagnose(connection, "list", "remote_list_failed", result.duration_ms, result.exit_status)
            raise AdapterError("remote_list_failed", "the remote directory listing failed")
        encoded = _encode_bytes(result.stdout)
        lines = encoded[1].splitlines()
        resolved_cwd = lines[0] if lines else ""
        entries = [line for line in lines[1:] if line]
        truncated = result.stdout_truncated or len(result.stdout) >= limit
        self._finish_activity(activity, "succeeded", result.exit_status)
        self._diagnose(connection, "list", "succeeded", result.duration_ms, result.exit_status)
        return {
            "ok": True,
            "remote": True,
            "alias": connection.target.alias,
            "command_class": "read",
            "connection_generation": connection.generation,
            "path": safe_path,
            "resolved_path": resolved_cwd,
            "entries": entries,
            "count": len(entries),
            "truncated": truncated,
            "untrusted": True,
        }

    def write_file(
        self,
        context: Mapping[str, Any],
        path: str,
        data: str,
        *,
        encoding: str = "utf8",
        overwrite: bool = False,
        timeout_ms: Optional[int] = None,
        cancellation: Any = None,
    ) -> dict[str, Any]:
        safe_path = self._validate_relative_path(path)
        payload = self._decode_write_data(data, encoding)
        if not isinstance(overwrite, bool):
            raise AdapterError("invalid_overwrite", "remote write overwrite must be a boolean")
        timeout = self._validate_timeout(timeout_ms)
        owner = OwnerFence.from_context(context)
        connection = self._require_ready(owner, cancellation=cancellation)
        if connection.target.authority != "read-write":
            raise AdapterError(
                "read_only",
                "the selected SSH target is configured read-only; remote writes are disabled",
            )
        self._require_mutation_approval(connection, "remote file write")
        remote_command = _remote_sh(
            WRITE_SCRIPT,
            [connection.target.remote_cwd, safe_path, "1" if overwrite else "0"],
        )
        result = self._run_operation(
            connection,
            command_class="mutation",
            summary="Remote file write",
            remote_command=remote_command,
            input_bytes=payload,
            timeout_ms=timeout,
            cancellation=cancellation,
            mutation=True,
            return_output=False,
        )
        if not result["ok"]:
            if result.get("exit_status") == 73:
                raise AdapterError(
                    "destination_exists",
                    "the remote destination already exists; set overwrite only after review",
                )
            raise AdapterError("remote_write_failed", "the bounded atomic remote file write failed")
        return {
            "ok": True,
            "remote": True,
            "alias": connection.target.alias,
            "command_class": "mutation",
            "connection_generation": connection.generation,
            "bytes_written": len(payload),
            "atomic_replace": True,
            "untrusted": True,
        }

    def execute_command(self, arguments: list[str], context: Mapping[str, Any]) -> dict[str, Any]:
        """Implement `/ssh` as the narrow/headless inspector and safe action path."""

        if not arguments or arguments in (["status"], ["list"]):
            text = self.format_status(context)
        elif arguments == ["snapshot"]:
            owner = self._owner_from_command_context(
                context, self._optional_session_id(context)
            )
            text = json.dumps(
                self.presentation_snapshot(owner), sort_keys=True, separators=(",", ":")
            )
        elif len(arguments) == 2 and arguments[0] == "show":
            text = self.format_target_detail(arguments[1], context)
        elif len(arguments) == 2 and arguments[0] in {"connect", "retry", "disconnect"}:
            action, target_id = arguments
            try:
                text = self.request_action(action, target_id, context)
            except AdapterError as error:
                text = f"SSH action rejected: {error.safe_summary}"
        else:
            text = self.command_usage()
        return {"text": text, "notifications": [], "context": []}

    @staticmethod
    def command_usage() -> str:
        return (
            "Usage: /ssh [status|list|snapshot|show <target>|connect <target>|"
            "retry <target>|disconnect <target>]"
        )

    def request_action(self, action: str, target_id: str, context: Mapping[str, Any]) -> str:
        target = self._enabled_target(target_id)
        session_id = self._session_id_from_command_context(context)
        owner = self._owner_from_command_context(context, session_id)
        if action == "connect":
            with self._lock:
                self._selections[session_id] = Selection(target.id, True)
            if owner is None:
                self._changed()
                return (
                    f"SSH target {target.id} selected. The replay-safe connection setup will run "
                    "only when this Ygg session supplies its owner fence to an SSH tool."
                )
            self._admit_owner(owner)
            self._replace_owner_target(owner, target)
            connection = self._connect(owner, target, explicit_retry=False)
            if connection.state != "ready":
                raise AdapterError(
                    "retry_required",
                    "the existing SSH connection is degraded; use /ssh retry explicitly",
                    ambiguous=connection.ambiguous_mutation,
                )
            return f"SSH connected to configured alias {target.alias} (generation {connection.generation})."
        if action == "retry":
            if owner is None:
                raise AdapterError("owner_required", "retry requires an owner-fenced SSH tool context first")
            with self._lock:
                selection = self._selections.get(session_id)
            if selection is None or selection.target_id != target.id:
                raise AdapterError("not_selected", "select this configured target with /ssh connect first")
            self._replace_owner_target(owner, target, disconnect_existing=True)
            connection = self._connect(owner, target, explicit_retry=True)
            return f"SSH reconnected to configured alias {target.alias} (generation {connection.generation})."
        if action == "disconnect":
            with self._lock:
                selection = self._selections.get(session_id)
                if selection is not None and selection.target_id == target.id:
                    selection.desired_connected = False
                matches = [
                    connection
                    for candidate, connection in self._connections.items()
                    if candidate.session_id == session_id and connection.target.id == target.id
                ]
            for connection in matches:
                self._disconnect(connection, state="stopped")
            self._changed()
            return f"SSH disconnected from configured alias {target.alias}."
        raise AdapterError("invalid_action", "unknown SSH action")

    def format_status(self, context: Mapping[str, Any]) -> str:
        session_id = self._optional_session_id(context)
        with self._lock:
            selection = self._selections.get(session_id) if session_id else None
            connections = [
                connection
                for owner, connection in self._connections.items()
                if session_id is None or owner.session_id == session_id
            ]
        lines = [f"SSH configured targets: {len(self.target_ids())}"]
        for target in sorted(self.config.targets, key=lambda item: item.id):
            if not target.enabled:
                continue
            related = [item for item in connections if item.target.id == target.id]
            connection = max(related, key=lambda item: item.generation) if related else None
            state = connection.state if connection else "disconnected"
            marker = " · selected" if selection and selection.target_id == target.id else ""
            generation = f" · generation {connection.generation}" if connection else ""
            ambiguous = " · ambiguous mutation" if connection and connection.ambiguous_mutation else ""
            lines.append(
                f"- {target.id}: {target.alias} · {target.authority} · {state}{generation}{ambiguous}{marker}"
            )
        if len(lines) == 1:
            lines.append("No aliases are configured; installation and discovery are inert.")
        return "\n".join(lines)

    def format_target_detail(self, target_id: str, context: Mapping[str, Any]) -> str:
        target = self._targets.get(target_id)
        if target is None:
            return "Unknown configured SSH target."
        session_id = self._optional_session_id(context)
        with self._lock:
            related = [
                connection
                for owner, connection in self._connections.items()
                if connection.target.id == target_id
                and (session_id is None or owner.session_id == session_id)
            ]
        connection = max(related, key=lambda item: item.generation) if related else None
        lines = [
            f"{target.id} · configured alias {target.alias}",
            f"authority: {target.authority}",
            f"remote cwd: {target.remote_cwd}",
            f"scope: {target.scope}",
        ]
        if connection is None:
            lines.extend(["state: disconnected", "actions: connect"])
        else:
            lines.extend(
                [
                    f"owner: {connection.owner.opaque_id}",
                    f"state: {connection.state}",
                    f"connection generation: {connection.generation}",
                    f"ambiguous mutation: {'yes' if connection.ambiguous_mutation else 'no'}",
                    f"last error: {connection.last_error or 'none'}",
                    "actions: "
                    + ("retry, disconnect" if connection.state == "degraded" else "disconnect"),
                ]
            )
        return "\n".join(lines)

    def status_contribution(self) -> dict[str, Any]:
        if self.configuration_error:
            return {
                "surface": "status",
                "text": "ssh configuration degraded",
                "style_role": "extension.ygg_ssh.degraded",
                "priority": 30,
            }
        with self._lock:
            active = any(
                connection.state in {"ready", "connecting", "degraded"}
                for connection in self._connections.values()
            )
        text = (
            "ssh active · remote authority · inspect the owner-scoped view"
            if active
            else f"ssh disconnected · {len(self.target_ids())} configured"
        )
        role = (
            "extension.ygg_ssh.high_authority"
            if active
            else "extension.ygg_ssh.idle"
        )
        return {"surface": "status", "text": text, "style_role": role, "priority": 30}

    def context_contribution(self) -> Optional[dict[str, Any]]:
        """Process-scoped prompt-context guidance for the connected workspace.

        Returns None when no session is active so the model's context stays
        clean. This is intentionally owner-free: context/collect is
        process-scoped, so it describes the live connection without exposing
        owner-fenced handles.
        """
        if self.configuration_error:
            return None
        with self._lock:
            active = [
                connection
                for connection in self._connections.values()
                if connection.state in {"ready", "connecting", "degraded"}
            ]
        if not active:
            return None
        lines = [
            "Active remote workspace (ygg-ssh): a user-configured OpenSSH session is",
            "connected. Treat the remote host as your working machine for this task:",
            "",
        ]
        for connection in active:
            authority = connection.target.authority
            state = connection.state
            lines.append(
                f"- {connection.target.id} (alias {connection.target.alias}) · "
                f"cwd {connection.target.remote_cwd} · authority {authority} · state {state}"
            )
        lines.extend(
            [
                "",
                "Use the ssh_* tools for all work on this host: ssh_list to discover the",
                "directory layout, ssh_read to read files, ssh_exec to run commands (when",
                "the target is read-write), and ssh_write for file changes. Do not probe",
                "local paths or re-derive the connection state with ssh_status every turn.",
            ]
        )
        return {
            "label": "ygg-ssh",
            "content": "\n".join(lines),
            "placement": "prompt_suffix",
        }

    def presentation_snapshot(self, owner: Optional[OwnerFence] = None) -> dict[str, Any]:
        from .presentation import build_presentation

        with self._lock:
            revision = self._presentation_revision
            targets = list(self.config.targets)
            connections = [
                self._connection_record(item)
                for candidate, item in self._connections.items()
                if owner is not None and candidate == owner
            ]
            owner_id = owner.opaque_id if owner is not None else None
            activities = [
                self._activity_record(item)
                for item in self._activities
                if owner_id is not None and item.owner_id == owner_id
            ]
            config_source = "configured" if self.config.source else "absent"
            configuration_error = self.configuration_error
        return build_presentation(
            revision=revision,
            targets=targets,
            connections=connections,
            activities=activities,
            config_source=config_source,
            configuration_error=configuration_error,
        )

    def settle_session(self, session_id: Any) -> None:
        if not isinstance(session_id, str) or not session_id:
            return
        with self._lock:
            matches = [
                connection
                for owner, connection in self._connections.items()
                if owner.host_session_id == session_id or owner.session_id == session_id
            ]
            owner_keys = [
                owner_key
                for owner_key, owner in self._latest_owner.items()
                if owner.host_session_id == session_id or owner.session_id == session_id
            ]
            for owner_key in owner_keys:
                self._selections.pop(owner_key, None)
                self._latest_owner.pop(owner_key, None)
        for connection in matches:
            self._disconnect(connection, state="stopped")
            with self._lock:
                self._connections.pop(connection.owner, None)
        self._changed()

    def shutdown(self) -> None:
        with self._lock:
            if self._shutting_down:
                return
            self._shutting_down = True
            self._stop.set()
            connections = list(self._connections.values())
        # Kill every registered local group first. This is bounded even when a
        # proxy or remote endpoint is unresponsive; Ygg's outer process-group
        # cleanup remains the final fence.
        self.backend.close()
        for connection in connections:
            with connection.operation_lock:
                connection.master = None
                connection.state = "stopped"
        self._health_thread.join(timeout=0.2)
        self._changed()

    def _run_operation(
        self,
        connection: Connection,
        *,
        command_class: str,
        summary: str,
        remote_command: str,
        input_bytes: bytes,
        timeout_ms: int,
        cancellation: Any,
        mutation: bool,
        return_output: bool = True,
    ) -> dict[str, Any]:
        with connection.operation_lock:
            if connection.state != "ready" or connection.master is None:
                raise AdapterError("not_ready", "the selected SSH connection is not ready")
            activity = self._start_activity(connection, command_class, summary)
            try:
                result = self.backend.run_remote(
                    connection.target.alias,
                    connection.master.control_path,
                    remote_command,
                    input_bytes=input_bytes,
                    timeout_ms=timeout_ms,
                    cancellation=cancellation,
                    capture_limit=self.config.limits.max_output_bytes,
                )
            except SshCancelled as error:
                if mutation:
                    self._mark_ambiguous(connection, "cancelled")
                self._finish_activity(activity, "cancelled" if not mutation else "ambiguous", None)
                self._diagnose(connection, command_class, "cancelled", None, None)
                raise AdapterError(
                    "ambiguous_mutation" if mutation else "cancelled",
                    (
                        "remote mutation was cancelled after dispatch; its outcome is ambiguous and "
                        "the connection will not reconnect automatically"
                        if mutation
                        else error.safe_summary
                    ),
                    ambiguous=mutation,
                ) from error
            except SshProcessError as error:
                if mutation:
                    self._mark_ambiguous(connection, error.code)
                self._finish_activity(activity, "ambiguous" if mutation else "failed", None)
                self._diagnose(connection, command_class, error.code, None, None)
                raise AdapterError(
                    "ambiguous_mutation" if mutation else error.code,
                    (
                        "remote mutation did not settle before transport failure; its outcome is "
                        "ambiguous and it will not be replayed"
                        if mutation
                        else error.safe_summary
                    ),
                    ambiguous=mutation,
                ) from error
            if result.exit_status == 255 or result.exit_status < 0:
                if mutation:
                    self._mark_ambiguous(connection, "connection_lost")
                else:
                    self._degrade_connection(connection, "connection_lost", "OpenSSH connection was lost")
                self._finish_activity(
                    activity,
                    "ambiguous" if mutation else "failed",
                    result.exit_status,
                )
                self._diagnose(
                    connection,
                    command_class,
                    "connection_lost",
                    result.duration_ms,
                    result.exit_status,
                )
                raise AdapterError(
                    "ambiguous_mutation" if mutation else "connection_lost",
                    (
                        "OpenSSH disconnected after a remote mutation; its outcome is ambiguous and "
                        "it will not be replayed"
                        if mutation
                        else "OpenSSH connection was lost"
                    ),
                    ambiguous=mutation,
                )
            outcome = "succeeded" if result.exit_status == 0 else "failed"
            self._finish_activity(activity, outcome, result.exit_status)
            self._diagnose(
                connection,
                command_class,
                outcome,
                result.duration_ms,
                result.exit_status,
            )
            response: dict[str, Any] = {
                "ok": result.exit_status == 0,
                "remote": True,
                "alias": connection.target.alias,
                "command_class": command_class,
                "connection_generation": connection.generation,
                "exit_status": result.exit_status,
                "duration_ms": result.duration_ms,
                "untrusted": True,
            }
            if return_output:
                stdout, stderr, stdout_truncated, stderr_truncated = self._bounded_streams(result)
                stdout_encoding, stdout_data = _encode_bytes(stdout)
                stderr_encoding, stderr_data = _encode_bytes(stderr)
                response.update(
                    {
                        "stdout": {
                            "encoding": stdout_encoding,
                            "data": stdout_data,
                            "truncated": stdout_truncated,
                        },
                        "stderr": {
                            "encoding": stderr_encoding,
                            "data": stderr_data,
                            "truncated": stderr_truncated,
                        },
                    }
                )
            return response

    def _require_ready(self, owner: OwnerFence, *, cancellation: Any) -> Connection:
        self._admit_owner(owner)
        connection = self._connection_for_owner(owner, establish_pending=True, cancellation=cancellation)
        if connection is None:
            raise AdapterError(
                "not_selected",
                "no SSH target is selected; use /ssh connect <configured-target> first",
            )
        with connection.operation_lock:
            self._observe_master_exit(connection)
            if connection.state == "degraded":
                suffix = (
                    " after an ambiguous mutation"
                    if connection.ambiguous_mutation
                    else ""
                )
                raise AdapterError(
                    "retry_required",
                    f"the SSH connection is degraded{suffix}; use /ssh retry explicitly",
                    ambiguous=connection.ambiguous_mutation,
                )
            if connection.state != "ready" or connection.master is None:
                raise AdapterError("not_ready", "the selected SSH connection is not ready")
            return connection

    def _connection_for_owner(
        self,
        owner: OwnerFence,
        *,
        establish_pending: bool,
        cancellation: Any,
    ) -> Optional[Connection]:
        with self._lock:
            connection = self._connections.get(owner)
            selection = self._selections.get(owner.session_id)
        if connection is not None:
            return connection
        if not establish_pending or selection is None or not selection.desired_connected:
            return None
        target = self._enabled_target(selection.target_id)
        return self._connect(owner, target, explicit_retry=False, cancellation=cancellation)

    def _connect(
        self,
        owner: OwnerFence,
        target: Target,
        *,
        explicit_retry: bool,
        cancellation: Any = None,
    ) -> Connection:
        with self._lock:
            if self._shutting_down:
                raise AdapterError("shutting_down", "SSH adapter is shutting down")
            existing = self._connections.get(owner)
            if existing is not None:
                if existing.state == "ready" and existing.target.id == target.id:
                    return existing
                if existing.state == "degraded" and not explicit_retry:
                    return existing
            active_count = sum(
                1 for item in self._connections.values() if item.state not in {"stopped"}
            )
            if existing is None and active_count >= self.config.limits.max_sessions:
                raise AdapterError("session_limit", "the configured SSH session limit is reached")
            key = (owner, target.alias, target.remote_cwd)
            generation = self._generation.get(key, 0) + 1
            self._generation[key] = generation
            connection = Connection(owner=owner, target=target, generation=generation)
            self._connections[owner] = connection
            self._latest_owner[owner.session_id] = owner
        self._changed()
        control_path = self.backend.control_path(
            f"{owner.opaque_id}:{target.alias}:{target.remote_cwd}", generation
        )
        started = time.monotonic()
        try:
            master = self.backend.connect_master(
                target.alias, control_path, cancellation=cancellation
            )
        except SshProcessError as error:
            with connection.operation_lock:
                connection.state = "degraded"
                connection.last_error_code = error.code
                connection.last_error = error.safe_summary[:512]
            self._diagnose(
                connection,
                "connection_setup",
                error.code,
                max(0, int((time.monotonic() - started) * 1000)),
                None,
            )
            self._changed()
            raise AdapterError(error.code, error.safe_summary) from error
        with connection.operation_lock:
            connection.master = master
            connection.state = "ready"
            connection.connected_at_ms = self._clock()
            connection.last_health_ms = self._clock()
            connection.last_error_code = None
            connection.last_error = None
            connection.ambiguous_mutation = False
        self._diagnose(
            connection,
            "connection_setup",
            "succeeded",
            max(0, int((time.monotonic() - started) * 1000)),
            0,
        )
        self._changed()
        return connection

    def _replace_owner_target(
        self,
        owner: OwnerFence,
        target: Target,
        *,
        disconnect_existing: bool = False,
    ) -> None:
        with self._lock:
            existing = self._connections.get(owner)
        if existing is not None and (
            disconnect_existing or existing.target.id != target.id
        ):
            self._disconnect(existing, state="stopped")
            with self._lock:
                self._connections.pop(owner, None)

    def _disconnect(self, connection: Connection, *, state: str) -> None:
        with connection.operation_lock:
            master, connection.master = connection.master, None
            connection.state = state
            if master is not None:
                self.backend.disconnect_master(master)
        self._diagnose(connection, "connection_setup", "disconnected", None, None)
        self._changed()

    def _admit_owner(self, owner: OwnerFence) -> None:
        with self._lock:
            stale = [
                connection
                for candidate, connection in self._connections.items()
                if candidate.session_id == owner.session_id and candidate != owner
            ]
            self._latest_owner[owner.session_id] = owner
        for connection in stale:
            self._disconnect(connection, state="stopped")
            with self._lock:
                self._connections.pop(connection.owner, None)

    def _observe_master_exit(self, connection: Connection) -> None:
        master = connection.master
        if master is not None and master.process.poll() is not None:
            self._degrade_connection(connection, "connection_lost", "OpenSSH connection exited")

    def _degrade_connection(self, connection: Connection, code: str, summary: str) -> None:
        with connection.operation_lock:
            master, connection.master = connection.master, None
            connection.state = "degraded"
            connection.last_error_code = code
            connection.last_error = summary[:512]
            if master is not None:
                self.backend.disconnect_master(master)
        self._changed()

    def _mark_ambiguous(self, connection: Connection, reason: str) -> None:
        connection.ambiguous_mutation = True
        self._degrade_connection(
            connection,
            "ambiguous_mutation",
            f"remote mutation outcome is ambiguous ({reason}); explicit retry is required",
        )

    def _require_mutation_approval(self, connection: Connection, operation: str) -> None:
        if self._confirm is None:
            raise AdapterError(
                "approval_unavailable",
                "remote mutation requires an interactive action-time approval",
            )
        prompt = f"Approve {operation} on configured SSH alias {connection.target.alias}?"
        detail = (
            f"Remote authority: {connection.target.authority}; cwd: "
            f"{connection.target.remote_cwd}; arguments and content are not displayed."
        )
        try:
            approved = bool(self._confirm(prompt, detail, True))
        except Exception as error:
            raise AdapterError(
                "approval_unavailable",
                "remote mutation approval was unavailable or cancelled",
            ) from error
        if not approved:
            raise AdapterError("approval_denied", "remote mutation was not approved")

    def _health_loop(self) -> None:
        interval = self.config.limits.health_interval_ms / 1000
        while not self._stop.wait(interval):
            with self._lock:
                if self._shutting_down:
                    return
                connections = [
                    item for item in self._connections.values() if item.state == "ready"
                ]
            for connection in connections:
                if not connection.operation_lock.acquire(blocking=False):
                    continue
                try:
                    master = connection.master
                    if master is None:
                        continue
                    if self.backend.master_healthy(master):
                        connection.last_health_ms = self._clock()
                    else:
                        self._degrade_connection(
                            connection, "health_failed", "OpenSSH control connection is degraded"
                        )
                        self._diagnose(
                            connection, "connection_setup", "health_failed", None, None
                        )
                finally:
                    connection.operation_lock.release()

    def _start_activity(
        self, connection: Connection, command_class: str, summary: str
    ) -> Activity:
        with self._lock:
            self._activity_sequence += 1
            activity = Activity(
                id=f"ssh-activity-{self._activity_sequence}",
                owner_id=connection.owner.opaque_id,
                alias=connection.target.alias,
                command_class=command_class,
                state="running",
                connection_generation=connection.generation,
                started_at_ms=self._clock(),
            )
            self._activities.append(activity)
        self._changed()
        return activity

    def _finish_activity(
        self, activity: Activity, outcome: str, exit_status: Optional[int]
    ) -> None:
        with self._lock:
            activity.state = "settled"
            activity.outcome = outcome
            activity.exit_status = exit_status
            activity.completed_at_ms = self._clock()
        self._changed()

    def _diagnose(
        self,
        connection: Connection,
        command_class: str,
        outcome: str,
        duration_ms: Optional[int],
        exit_status: Optional[int],
    ) -> None:
        record = {
            "host_alias": connection.target.alias,
            "owner": connection.owner.opaque_id,
            "connection_generation": connection.generation,
            "command_class": command_class,
            "outcome": outcome,
            "duration_ms": duration_ms,
            "exit_status": exit_status,
        }
        with self._lock:
            self._diagnostics.append(record)
        if self._logger is not None:
            try:
                self._logger.info("ssh operation", **record)
            except Exception:
                pass

    def _changed(self) -> None:
        with self._lock:
            self._presentation_revision += 1
            active = self._presentation_active
        if active:
            self._publish_current()

    def _publish_current(self) -> None:
        if self._publisher is None:
            return
        with self._publish_lock:
            try:
                with self._lock:
                    owners = sorted(
                        set(self._connections).union(self._latest_owner.values()),
                        key=lambda owner: (
                            owner.session_id,
                            owner.extension_instance_id,
                            owner.process_generation,
                        ),
                    )
                if not owners:
                    self._publisher(self.presentation_snapshot(), None)
                    return
                for index, owner in enumerate(owners):
                    if index:
                        with self._lock:
                            self._presentation_revision += 1
                    self._publisher(self.presentation_snapshot(owner), owner.wire)
            except Exception:
                # Status and /ssh remain the bounded fallback when the generic
                # host primitive is unavailable or the process is draining.
                return

    def _connection_status(self, connection: Connection) -> dict[str, Any]:
        return {
            "connected": connection.state == "ready",
            "state": connection.state,
            "target_id": connection.target.id,
            "alias": connection.target.alias,
            "authority": connection.target.authority,
            "remote_cwd": connection.target.remote_cwd,
            "connection_generation": connection.generation,
            "ambiguous": connection.ambiguous_mutation,
            "health": "ready" if connection.state == "ready" else "degraded",
            "last_error": connection.last_error,
            "configured_targets": self.target_ids(),
            "agent_socket_available": self.backend.agent_socket_available,
            "owner_fence": connection.owner.process_fence,
        }

    def _connection_record(self, connection: Connection) -> dict[str, Any]:
        return {
            "owner": connection.owner.opaque_id,
            "ownerFence": connection.owner.process_fence,
            "targetId": connection.target.id,
            "alias": connection.target.alias,
            "state": connection.state,
            "authority": connection.target.authority,
            "remoteCwd": connection.target.remote_cwd,
            "generation": connection.generation,
            "ambiguous": connection.ambiguous_mutation,
            "lastError": connection.last_error,
            "lastErrorCode": connection.last_error_code,
            "connectedAtMs": connection.connected_at_ms,
            "lastHealthMs": connection.last_health_ms,
        }

    @staticmethod
    def _activity_record(activity: Activity) -> dict[str, Any]:
        return {
            "id": activity.id,
            "owner": activity.owner_id,
            "alias": activity.alias,
            "commandClass": activity.command_class,
            "state": activity.state,
            "connectionGeneration": activity.connection_generation,
            "startedAtMs": activity.started_at_ms,
            "completedAtMs": activity.completed_at_ms,
            "exitStatus": activity.exit_status,
            "outcome": activity.outcome,
        }

    def _bounded_streams(
        self, result: ProcessResult
    ) -> tuple[bytes, bytes, bool, bool]:
        limit = self.config.limits.max_output_bytes
        stdout = result.stdout[:limit]
        remaining = max(0, limit - len(stdout))
        stderr = result.stderr[:remaining]
        return (
            stdout,
            stderr,
            result.stdout_truncated or len(result.stdout) > len(stdout),
            result.stderr_truncated or len(result.stderr) > len(stderr),
        )

    def _validate_argv(self, value: Sequence[str]) -> tuple[str, ...]:
        if isinstance(value, (str, bytes)) or not isinstance(value, Sequence):
            raise AdapterError("invalid_command", "ssh_exec argv must be an array of strings")
        if not 1 <= len(value) <= self.config.limits.max_command_args:
            raise AdapterError(
                "invalid_command",
                f"ssh_exec argv must contain 1 to {self.config.limits.max_command_args} items",
            )
        total = 0
        result = []
        for argument in value:
            if not isinstance(argument, str) or "\x00" in argument:
                raise AdapterError("invalid_command", "ssh_exec arguments must be strings without NUL")
            encoded = argument.encode("utf-8")
            total += len(encoded)
            if total > self.config.limits.max_command_bytes:
                raise AdapterError("invalid_command", "ssh_exec command exceeds the configured byte limit")
            result.append(argument)
        if not result[0]:
            raise AdapterError("invalid_command", "ssh_exec executable must be non-empty")
        return tuple(result)

    def _validate_relative_path(self, value: Any) -> str:
        if not isinstance(value, str) or not value or "\x00" in value:
            raise AdapterError("invalid_path", "remote path must be a non-empty string without NUL")
        if len(value.encode("utf-8")) > 4096 or any(
            ord(character) < 32 or 127 <= ord(character) <= 159 for character in value
        ):
            raise AdapterError("invalid_path", "remote path exceeds the safe lexical bound")
        path = PurePosixPath(value)
        if (
            path.is_absolute()
            or str(path) != value
            or any(part in {"", ".", ".."} for part in path.parts)
        ):
            raise AdapterError(
                "invalid_path",
                "remote path must be normalized and relative to the configured remote cwd",
            )
        return value

    def _decode_write_data(self, value: Any, encoding: Any) -> bytes:
        if not isinstance(value, str):
            raise AdapterError("invalid_data", "remote write data must be a string")
        if encoding == "utf8":
            payload = value.encode("utf-8")
        elif encoding == "base64":
            try:
                payload = base64.b64decode(value.encode("ascii"), validate=True)
            except (UnicodeEncodeError, ValueError) as error:
                raise AdapterError("invalid_data", "remote write base64 is invalid") from error
        else:
            raise AdapterError("invalid_encoding", "remote write encoding must be utf8 or base64")
        if len(payload) > self.config.limits.max_file_bytes:
            raise AdapterError(
                "file_too_large",
                f"remote write exceeds the {self.config.limits.max_file_bytes}-byte limit",
            )
        return payload

    def _validate_timeout(self, value: Optional[int]) -> int:
        if value is None:
            return self.config.limits.operation_timeout_ms
        if isinstance(value, bool) or not isinstance(value, int):
            raise AdapterError("invalid_timeout", "remote timeout must be an integer")
        if not 100 <= value <= self.config.limits.operation_timeout_ms:
            raise AdapterError(
                "invalid_timeout",
                f"remote timeout must be between 100 and {self.config.limits.operation_timeout_ms} ms",
            )
        return value

    def _enabled_target(self, target_id: str) -> Target:
        if not isinstance(target_id, str):
            raise AdapterError("unknown_target", "SSH target must be a configured identifier")
        target = self._targets.get(target_id)
        if target is None or not target.enabled:
            raise AdapterError("unknown_target", "SSH target is not in the enabled configured allowlist")
        return target

    def _target_alias(self, target_id: str) -> Optional[str]:
        target = self._targets.get(target_id)
        return target.alias if target is not None else None

    def _session_id_from_command_context(self, context: Mapping[str, Any]) -> str:
        session_id = self._optional_session_id(context)
        if session_id is None:
            raise AdapterError(
                "session_required",
                "SSH target selection requires an active Ygg session; no global default is chosen",
            )
        return session_id

    @staticmethod
    def _optional_session_id(context: Mapping[str, Any]) -> Optional[str]:
        resource_owner = context.get("resource_owner")
        if isinstance(resource_owner, Mapping) and isinstance(resource_owner.get("session_id"), str):
            return resource_owner["session_id"]
        host = context.get("host")
        if isinstance(host, Mapping) and isinstance(host.get("session_id"), str):
            return host["session_id"]
        return None

    def _owner_from_command_context(
        self, context: Mapping[str, Any], session_id: str
    ) -> Optional[OwnerFence]:
        if isinstance(context.get("resource_owner"), Mapping):
            return OwnerFence.from_context(context)
        with self._lock:
            return self._latest_owner.get(session_id)


def _remote_sh(script: str, arguments: Sequence[str]) -> str:
    words = ["sh", "-c", script, "ygg-ssh", *arguments]
    return " ".join(shlex.quote(word) for word in words)


def _encode_bytes(value: bytes) -> tuple[str, str]:
    try:
        text = value.decode("utf-8")
    except UnicodeDecodeError:
        return "base64", base64.b64encode(value).decode("ascii")
    if "\x1b" in text or any(
        ord(character) < 32 and character not in "\n\r\t" for character in text
    ) or any(127 <= ord(character) <= 159 for character in text):
        return "base64", base64.b64encode(value).decode("ascii")
    return "utf8", text
