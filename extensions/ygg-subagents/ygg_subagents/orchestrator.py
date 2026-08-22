"""Bounded orchestration over Ygg API 0.2's host-owned agent_sessions service."""

from __future__ import annotations

from collections import OrderedDict
from dataclasses import dataclass, field
import re
import threading
import time
from typing import Any, Callable, Dict, List, Mapping, Optional, Protocol, Sequence, Tuple

from .model import (
    CHILD_TOOLS,
    MAX_ACTIVE_CHILDREN,
    MAX_CHILD_MESSAGE_BYTES,
    MAX_DEPTH,
    MAX_ERROR_BYTES,
    MAX_OWNER_CACHES,
    MAX_WORKERS_PER_OWNER,
    PROFILE_INSTRUCTIONS,
    SpawnRequest,
    SubagentError,
    Owner,
    Worker,
    aggregate_usage,
    bounded_int,
    depth_from_record,
    host_state,
    parse_artifacts,
    parse_recent_tools,
    parse_target,
    safe_label,
    sanitize_document,
    validate_plain_text,
)
from .presentation import build_snapshot, detail_body, narrow_list


_AGENT_ID_RE = re.compile(r"^[A-Za-z0-9_.:-]{1,512}$")
_AGENT_PATH_RE = re.compile(r"^/[A-Za-z0-9_./:-]{1,1023}$")
_IDEMPOTENCY_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$")
_FINGERPRINT_RE = re.compile(r"^[0-9a-f]{64}$")


def _host_policy(
    record: Mapping[str, Any],
) -> Tuple[
    Tuple[str, ...], Optional[int], Optional[int], Optional[int], int, Optional[int], Optional[int]
]:
    policy = record.get("policy")
    deadline_at_ms = record.get("deadline_at_ms")
    if not isinstance(policy, Mapping):
        raise SubagentError(
            "agent_sessions omitted the host-enforced child policy",
            code="host_state_invalid",
        )
    raw_tools = policy.get("tools")
    if (
        not isinstance(raw_tools, list)
        or not raw_tools
        or len(raw_tools) > len(CHILD_TOOLS)
        or any(tool not in CHILD_TOOLS for tool in raw_tools)
        or len(set(raw_tools)) != len(raw_tools)
    ):
        raise SubagentError(
            "agent_sessions returned an invalid effective child tool policy",
            code="host_state_invalid",
        )
    max_tokens = policy.get("max_tokens")
    # Omitted (null) child-specific ceilings are valid: they inherit the
    # parent session's limits, which may themselves be unlimited.
    values = (
        policy.get("max_turns"),
        policy.get("max_cost_microdollars"),
        policy.get("timeout_ms"),
        deadline_at_ms,
    )
    for value in values:
        if value is not None and (
            not isinstance(value, int) or isinstance(value, bool) or value < 0
        ):
            raise SubagentError(
                "agent_sessions returned invalid host-enforced child limits",
                code="host_state_invalid",
            )
    max_output = policy.get("max_output_bytes")
    if (
        not isinstance(max_output, int)
        or isinstance(max_output, bool)
        or max_output < 1
    ):
        raise SubagentError(
            "agent_sessions returned invalid host-enforced child limits",
            code="host_state_invalid",
        )
    if max_tokens is not None and (
        not isinstance(max_tokens, int)
        or isinstance(max_tokens, bool)
        or max_tokens < 0
    ):
        raise SubagentError(
            "agent_sessions returned invalid host-enforced child limits",
            code="host_state_invalid",
        )
    max_turns, max_cost, timeout_ms, deadline = values
    if (
        (max_turns is not None and max_turns < 1)
        or (timeout_ms is not None and timeout_ms < 1)
        or (deadline is not None and deadline < 1)
    ):
        raise SubagentError(
            "agent_sessions returned non-positive host-enforced child limits",
            code="host_state_invalid",
        )
    return (
        tuple(raw_tools),
        max_turns,
        max_tokens,
        max_cost,
        max_output,
        timeout_ms,
        deadline,
    )


def _host_timestamps(
    record: Mapping[str, Any], state_name: str, deadline_at_ms: Optional[int]
) -> Tuple[int, int, Optional[int]]:
    created = record.get("created_at_ms")
    started = record.get("started_at_ms")
    completed = record.get("completed_at_ms")
    if (
        not isinstance(created, int)
        or isinstance(created, bool)
        or created < 0
        or (deadline_at_ms is not None and created > deadline_at_ms)
    ):
        raise SubagentError(
            "agent_sessions omitted a valid host-created worker timestamp",
            code="host_state_invalid",
        )
    if started is not None and (
        not isinstance(started, int)
        or isinstance(started, bool)
        or started < created
    ):
        raise SubagentError(
            "agent_sessions returned an invalid worker start timestamp",
            code="host_state_invalid",
        )
    terminal = state_name in {
        "completed",
        "done",
        "failed",
        "interrupted",
        "cancelled",
        "shutdown",
        "orphaned",
        "timed_out",
        "stopped",
    }
    if state_name in {"running", "waiting"} and started is None:
        raise SubagentError(
            "agent_sessions omitted the worker start timestamp",
            code="host_state_invalid",
        )
    if completed is not None and (
        not isinstance(completed, int)
        or isinstance(completed, bool)
        or completed < (started if isinstance(started, int) else created)
    ):
        raise SubagentError(
            "agent_sessions returned an invalid worker completion timestamp",
            code="host_state_invalid",
        )
    if not terminal and completed is not None:
        # A resumed worker can still expose the previous run's completion
        # timestamp from an older host; an active record is not completed.
        completed = None
    if terminal and completed is None:
        raise SubagentError(
            "agent_sessions omitted the worker completion timestamp",
            code="host_state_invalid",
        )
    return created, started if isinstance(started, int) else created, completed


class AgentSessions(Protocol):
    """The exact SDK helper surface used by this extension."""

    def spawn_agent(
        self,
        *,
        task_name: str,
        profile: Optional[str],
        fingerprint: Optional[str],
        message: str,
        idempotency_key: str,
        tools: Sequence[str],
        max_depth: int,
        max_concurrent_children: int,
        max_turns: Optional[int],
        max_tokens: Optional[int],
        max_cost_microdollars: Optional[int],
        max_output_bytes: int,
        timeout_ms: Optional[int],
    ) -> Mapping[str, Any]: ...

    def list_agents(self) -> Mapping[str, Any]: ...

    def wait_agents(self, *, timeout_ms: int) -> Mapping[str, Any]: ...

    def interrupt_agent(self, target: str) -> Mapping[str, Any]: ...

    def send_agent_message(self, target: str, message: str) -> Mapping[str, Any]:
        """Steer a running child, or queue the message for an idle one."""
        ...

    def follow_up_agent(self, target: str, message: str) -> Mapping[str, Any]:
        """Queue a follow-up task; resumes a settled worker's durable session."""
        ...


class Cancellation(Protocol):
    def raise_if_cancelled(self) -> None: ...


@dataclass
class OwnerState:
    owner: Owner
    workers: "OrderedDict[str, Worker]" = field(default_factory=OrderedDict)
    idempotency: Dict[str, Tuple[str, str]] = field(default_factory=dict)
    pending_spawns: Dict[str, SpawnRequest] = field(default_factory=dict)
    selected_agent_id: Optional[str] = None
    last_used_ms: int = 0


class Orchestrator:
    """Policy layer only; Ygg remains the session, persistence, and limit owner."""

    def __init__(
        self,
        *,
        publish: Optional[Callable[[Mapping[str, Any]], None]] = None,
        now_ms: Optional[Callable[[], int]] = None,
    ) -> None:
        self._publish_callback = publish
        self._now_ms = now_ms or (lambda: int(time.time() * 1000))
        self._owners: "OrderedDict[Tuple[str, str], OwnerState]" = OrderedDict()
        self._lock = threading.RLock()
        self._shutting_down = False

    def set_publisher(
        self, publish: Optional[Callable[[Mapping[str, Any]], None]]
    ) -> None:
        with self._lock:
            self._publish_callback = publish

    def spawn(
        self,
        client: AgentSessions,
        owner: Owner,
        arguments: Mapping[str, Any],
        cancellation: Optional[Cancellation] = None,
    ) -> Dict[str, Any]:
        request = SpawnRequest.parse(arguments)
        self._check_cancelled(cancellation)
        state = self._owner_state(owner)
        self._refresh(client, state, cancellation)

        with self._lock:
            existing = self._existing_idempotent_locked(state, request)
            if existing is not None:
                state.selected_agent_id = existing.agent_id
                result = self._spawn_result(existing, request, duplicate=True)
                publish = self._snapshot_locked(state)
            else:
                publish = None
                self._reserve_spawn_locked(state, request)
        if publish is not None:
            self._publish(publish)
            return result

        worker: Optional[Worker] = None
        try:
            self._check_cancelled(cancellation)
            response = client.spawn_agent(
                task_name=request.name,
                profile=request.profile,
                fingerprint=request.fingerprint,
                message=request.child_message(owner),
                idempotency_key=request.idempotency_key,
                tools=request.tools,
                max_depth=MAX_DEPTH,
                max_concurrent_children=MAX_ACTIVE_CHILDREN,
                max_turns=request.max_turns,
                max_tokens=None,
                max_cost_microdollars=request.max_cost_microdollars,
                max_output_bytes=request.max_output_bytes,
                timeout_ms=(
                    None
                    if request.timeout_seconds is None
                    else request.timeout_seconds * 1000
                ),
            )
            worker = self._worker_from_spawn(owner, request, response)
            if worker.depth > MAX_DEPTH:
                # The extension is available inside inherited child sessions too.
                # Post-admission path/depth validation closes that recursive route
                # immediately without trusting model-generated owner data.
                try:
                    client.interrupt_agent(worker.agent_id)
                finally:
                    raise SubagentError(
                        "subagent depth one is the V1 limit; the rejected descendant was interrupted",
                        code="depth_limit",
                    )
            with self._lock:
                state.workers[worker.agent_id] = worker
                state.workers.move_to_end(worker.agent_id)
                state.idempotency[request.idempotency_key] = (
                    request.fingerprint,
                    worker.agent_id,
                )
                state.selected_agent_id = worker.agent_id
            # agent/spawn does not expose the durable session path. Reconcile once
            # through the only authoritative observation API rather than guessing.
            self._refresh(client, state, cancellation)
        except BaseException:
            with self._lock:
                state.pending_spawns.pop(request.idempotency_key, None)
            raise
        else:
            with self._lock:
                state.pending_spawns.pop(request.idempotency_key, None)
                current = state.workers.get(worker.agent_id) if worker else None
                if current is None:
                    raise SubagentError(
                        "the spawned worker disappeared during authoritative resync",
                        code="host_state_invalid",
                    )
                result = self._spawn_result(current, request, duplicate=False)
                publish = self._snapshot_locked(state)
            self._publish(publish)

        if not request.background:
            waited = self.wait(
                client,
                owner,
                {
                    "target": result["worker"]["id"],
                    "timeout_seconds": 60
                    if request.timeout_seconds is None
                    else min(60, request.timeout_seconds),
                },
                cancellation,
            )
            result["foreground_wait"] = waited
            result["worker"] = waited.get("worker", result["worker"])
        return result

    def status(
        self,
        client: AgentSessions,
        owner: Owner,
        arguments: Mapping[str, Any],
        cancellation: Optional[Cancellation] = None,
    ) -> Dict[str, Any]:
        if not isinstance(arguments, Mapping):
            raise SubagentError("subagent_status arguments must be an object")
        unknown = set(arguments) - {"target"}
        if unknown:
            raise SubagentError("unknown subagent_status fields: %s" % ", ".join(sorted(unknown)))
        target = parse_target(arguments, optional=True)
        state = self._owner_state(owner)
        self._refresh(client, state, cancellation)
        with self._lock:
            worker = self._resolve_locked(state, target) if target is not None else None
            if worker is not None:
                state.selected_agent_id = worker.agent_id
            result = self._status_result_locked(state, worker)
            publish = self._snapshot_locked(state)
        self._publish(publish)
        return result

    def wait(
        self,
        client: AgentSessions,
        owner: Owner,
        arguments: Mapping[str, Any],
        cancellation: Optional[Cancellation] = None,
    ) -> Dict[str, Any]:
        if not isinstance(arguments, Mapping):
            raise SubagentError("subagent_wait arguments must be an object")
        unknown = set(arguments) - {"target", "timeout_seconds"}
        if unknown:
            raise SubagentError("unknown subagent_wait fields: %s" % ", ".join(sorted(unknown)))
        target = parse_target(arguments, optional=True)
        timeout_seconds = bounded_int(
            arguments.get("timeout_seconds", 30), "timeout_seconds", 1, 60
        )
        state = self._owner_state(owner)
        self._refresh(client, state, cancellation)
        caller_deadline = self._now_ms() + timeout_seconds * 1000

        with self._lock:
            selected = self._resolve_locked(state, target) if target is not None else None
            if selected is not None:
                state.selected_agent_id = selected.agent_id
            waiting_ids = [
                worker.agent_id
                for worker in state.workers.values()
                if worker.active and (selected is None or worker.agent_id == selected.agent_id)
            ]
            for agent_id in waiting_ids:
                worker = state.workers[agent_id]
                worker.state = "waiting"
                worker.phase = "waiting for host completion"
            publish = self._snapshot_locked(state)
        self._publish(publish)

        wait_timed_out = False
        iterations = 0
        try:
            while True:
                iterations += 1
                self._check_cancelled(cancellation)
                now = self._now_ms()
                with self._lock:
                    current = (
                        self._resolve_locked(state, target)
                        if target is not None
                        else None
                    )
                    active = [
                        worker
                        for worker in state.workers.values()
                        if worker.active and (current is None or worker.agent_id == current.agent_id)
                    ]
                if not active:
                    break
                if now >= caller_deadline or iterations > 128:
                    wait_timed_out = True
                    break
                slice_ms = max(1, min(1_000, caller_deadline - now))
                response = client.wait_agents(timeout_ms=slice_ms)
                self._check_cancelled(cancellation)
                if not isinstance(response, Mapping) or not isinstance(
                    response.get("timed_out"), bool
                ):
                    raise SubagentError(
                        "agent_sessions returned an invalid wait response",
                        code="host_state_invalid",
                    )
                snapshot = response.get("snapshot")
                if not isinstance(snapshot, Mapping):
                    raise SubagentError(
                        "agent_sessions wait omitted its authoritative snapshot",
                        code="host_state_invalid",
                    )
                self._reconcile_snapshot(state, snapshot)
                self._enforce_policy_descendants(client, state, cancellation)
        finally:
            with self._lock:
                for agent_id in waiting_ids:
                    worker = state.workers.get(agent_id)
                    if worker is not None and worker.state == "waiting":
                        worker.state = "running"
                        worker.phase = "running in host session"
                publish = self._snapshot_locked(state)
            self._publish(publish)

        with self._lock:
            selected = self._resolve_locked(state, target) if target is not None else None
            result = self._status_result_locked(state, selected)
            result["operation"] = "wait"
            result["wait_timed_out"] = wait_timed_out
            result["completion_delivery"] = (
                "host_owned_parent_turn" if not wait_timed_out else "workers_continue_in_background"
            )
        return result

    def stop(
        self,
        client: AgentSessions,
        owner: Owner,
        arguments: Mapping[str, Any],
        cancellation: Optional[Cancellation] = None,
    ) -> Dict[str, Any]:
        if not isinstance(arguments, Mapping):
            raise SubagentError("subagent_stop arguments must be an object")
        unknown = set(arguments) - {"target", "all"}
        if unknown:
            raise SubagentError("unknown subagent_stop fields: %s" % ", ".join(sorted(unknown)))
        stop_all = arguments.get("all", False)
        if not isinstance(stop_all, bool):
            raise SubagentError("all must be a boolean")
        target = parse_target(arguments, optional=True)
        if stop_all == (target is not None):
            raise SubagentError("provide exactly one of target or all=true")

        state = self._owner_state(owner)
        self._refresh(client, state, cancellation)
        with self._lock:
            if stop_all:
                targets = [worker for worker in state.workers.values() if worker.active]
            else:
                targets = [self._resolve_locked(state, target)]
            target_ids = [worker.agent_id for worker in targets]
            for worker in targets:
                if worker.active:
                    worker.stop_requested = True
                    worker.state = "stopping"
                    worker.phase = "host interrupt requested"
            if targets:
                state.selected_agent_id = targets[-1].agent_id
            publish = self._snapshot_locked(state)
        self._publish(publish)

        outcomes: List[Dict[str, Any]] = []
        for agent_id in target_ids:
            self._check_cancelled(cancellation)
            with self._lock:
                worker = state.workers.get(agent_id)
                if worker is None:
                    continue
                if worker.terminal:
                    outcomes.append(
                        {"id": agent_id, "state": worker.state, "interrupt_requested": False}
                    )
                    continue
            response = client.interrupt_agent(agent_id)
            if not isinstance(response, Mapping):
                raise SubagentError(
                    "agent_sessions returned an invalid interrupt response",
                    code="host_state_invalid",
                )
            requested = response.get("interrupt_requested")
            if not isinstance(requested, bool):
                raise SubagentError(
                    "agent_sessions interrupt omitted interrupt_requested",
                    code="host_state_invalid",
                )
            with self._lock:
                worker = state.workers.get(agent_id)
                outcomes.append(
                    {
                        "id": agent_id,
                        "state": worker.state if worker else "orphaned",
                        "interrupt_requested": requested,
                    }
                )
        with self._lock:
            publish = self._snapshot_locked(state)
            public_workers = [
                state.workers[agent_id].public(self._now_ms())
                for agent_id in target_ids
                if agent_id in state.workers
            ]
        self._publish(publish)
        return {"operation": "stop", "outcomes": outcomes, "workers": public_workers}

    def continue_worker(
        self,
        client: AgentSessions,
        owner: Owner,
        arguments: Mapping[str, Any],
        cancellation: Optional[Cancellation] = None,
    ) -> Dict[str, Any]:
        """Continue one owned worker: steer it if active, resume it if settled."""
        if not isinstance(arguments, Mapping):
            raise SubagentError("subagent_continue arguments must be an object")
        unknown = set(arguments) - {"target", "message"}
        if unknown:
            raise SubagentError(
                "unknown subagent_continue fields: %s" % ", ".join(sorted(unknown))
            )
        target = parse_target(arguments)
        message = arguments.get("message")
        if not isinstance(message, str) or not message.strip():
            raise SubagentError("message must be a non-empty string")
        if len(message.encode("utf-8")) > MAX_CHILD_MESSAGE_BYTES:
            raise SubagentError("message exceeds the 64 KiB bound")
        validate_plain_text(message, "message", allow_newline=True)

        state = self._owner_state(owner)
        self._refresh(client, state, cancellation)
        with self._lock:
            worker = self._resolve_locked(state, target)
            state.selected_agent_id = worker.agent_id
            display = worker.name
            if worker.state == "orphaned":
                raise SubagentError(
                    "worker %s was orphaned by a host shutdown and cannot be resumed"
                    % display,
                    code="orphaned",
                )
            if worker.state == "stopping":
                raise SubagentError(
                    "worker %s is stopping; wait for it to settle before continuing"
                    % display,
                    code="worker_stopping",
                )
            action = "resumed" if worker.terminal else "steered"
            if worker.terminal:
                client.follow_up_agent(worker.agent_id, message)
                # The host accepted the resume, so the previous run is closed.
                # Clear the sticky flags or the next refresh would re-map the
                # fresh run back to "stopping"/"timed out" and this worker
                # could never be continued again.
                worker.stop_requested = False
                worker.timeout_requested = False
            else:
                client.send_agent_message(worker.agent_id, message)
        self._check_cancelled(cancellation)
        self._refresh(client, state, cancellation)
        with self._lock:
            worker = self._resolve_locked(state, target)
            result: Dict[str, Any] = {
                "operation": "continue",
                "accepted": True,
                "action": action,
                "worker": worker.public(self._now_ms()),
            }
            publish = self._snapshot_locked(state)
        self._publish(publish)
        return result

    def command(
        self, arguments: Sequence[Any], context: Mapping[str, Any]
    ) -> Dict[str, Any]:
        """Read cached state; owner-bound live stop is handled by the runtime."""

        if not isinstance(arguments, list) or any(not isinstance(item, str) for item in arguments):
            raise SubagentError("/subagents arguments must be strings")
        if len(arguments) > 3 or any(len(item.encode("utf-8")) > 512 for item in arguments):
            raise SubagentError("/subagents arguments exceed the command bound")
        state = self._owner_for_command(context)
        if state is None:
            return {
                "text": "Subagents\nNo cached worker state is available for this parent session. Use subagent_status from an active model turn to resync host-owned sessions.",
                "notifications": [],
            }
        now = self._now_ms()
        if not arguments or arguments[0] in {"list", "status"}:
            with self._lock:
                return {"text": narrow_list(list(state.workers.values()), now), "notifications": []}
        verb = arguments[0]
        if verb == "inspect" and len(arguments) == 2:
            with self._lock:
                worker = self._resolve_locked(state, arguments[1])
                state.selected_agent_id = worker.agent_id
                text = detail_body(worker, now)
                publish = self._snapshot_locked(state)
            self._publish(publish)
            return {"text": text, "notifications": []}
        if verb == "stop" and len(arguments) == 2:
            # This cached fallback has no live service client. Never smuggle a
            # stale request ID through it; the runtime handles owner-bound stop.
            with self._lock:
                if arguments[1] != "all":
                    worker = self._resolve_locked(state, arguments[1])
                    state.selected_agent_id = worker.agent_id
                    publish = self._snapshot_locked(state)
                else:
                    publish = self._snapshot_locked(state)
            self._publish(publish)
            return {
                "text": (
                    "Stop was not issued by the cached fallback. "
                    "Use the owner-bound /subagents stop action or subagent_stop tool (target=%s)."
                    % arguments[1]
                ),
                "notifications": [
                    {
                        "level": "warning",
                        "title": "Subagent stop requires owner authority",
                        "message": "No stale command request was reused; the worker remains authoritative in Ygg.",
                    }
                ],
            }
        return {
            "text": "Usage: /subagents [list|inspect <name-or-id>|stop <name-or-id|all>]\nThe fallback is cached/read-only; authoritative wait and stop use subagent_wait and subagent_stop.",
            "notifications": [],
        }

    def status_contribution(self, context: Mapping[str, Any]) -> Optional[Dict[str, Any]]:
        state = self._owner_for_command(context)
        if state is None:
            return None
        with self._lock:
            workers = list(state.workers.values())
            running = sum(worker.active for worker in workers)
            failed = sum(worker.state in {"failed", "timed_out"} for worker in workers)
        if failed:
            style = "extension.subagents.degraded"
        elif running:
            style = "extension.subagents.running"
        else:
            style = "extension.subagents.ready"
        return {
            "surface": "status",
            "text": "subagents · %d running · %d total" % (running, len(workers)),
            "style_role": style,
            "priority": 10,
        }

    def session_settled(self, event: Mapping[str, Any]) -> None:
        session_id = event.get("session_id") if isinstance(event, Mapping) else None
        outcome = event.get("outcome") if isinstance(event, Mapping) else None
        now = self._now_ms()
        snapshots: List[Mapping[str, Any]] = []
        with self._lock:
            for state in self._owners.values():
                if isinstance(session_id, str) and state.owner.host_session_id not in {None, session_id}:
                    continue
                for worker in state.workers.values():
                    if not worker.active:
                        continue
                    if outcome in {"cancelled", "interrupted"}:
                        worker.state = "cancelled"
                    elif outcome == "limit_reached":
                        worker.state = "timed_out"
                    else:
                        worker.state = "orphaned"
                    worker.phase = "parent session settled"
                    worker.completed_at_ms = now
                snapshots.append(self._snapshot_locked(state))
        for snapshot in snapshots:
            self._publish(snapshot)

    def shutdown_local(self) -> None:
        """Settle local views; host shutdown owns descendant interruption."""

        now = self._now_ms()
        snapshots: List[Mapping[str, Any]] = []
        with self._lock:
            self._shutting_down = True
            for state in self._owners.values():
                state.pending_spawns.clear()
                for worker in state.workers.values():
                    if worker.active:
                        worker.state = "orphaned"
                        worker.phase = "extension shutdown; host cleanup requested"
                        worker.completed_at_ms = now
                snapshots.append(self._snapshot_locked(state))
        for snapshot in snapshots:
            self._publish(snapshot)

    def cached_owner_count(self) -> int:
        with self._lock:
            return len(self._owners)

    def _owner_state(self, owner: Owner) -> OwnerState:
        key = owner.stable_key
        now = self._now_ms()
        with self._lock:
            if self._shutting_down:
                raise SubagentError("the subagent orchestrator is shutting down", code="shutdown")
            state = self._owners.get(key)
            if state is None:
                self._evict_owner_locked()
                state = OwnerState(owner=owner, last_used_ms=now)
                self._owners[key] = state
            else:
                if state.owner.process_generation != owner.process_generation:
                    for worker in state.workers.values():
                        worker.recovered = True
                        worker.restart_count += 1
                        worker.generation = owner.process_generation
                        if worker.active:
                            worker.phase = "resyncing after extension restart"
                    state.pending_spawns.clear()
                state.owner = owner
                state.last_used_ms = now
                self._owners.move_to_end(key)
            return state

    def _evict_owner_locked(self) -> None:
        if len(self._owners) < MAX_OWNER_CACHES:
            return
        for key, state in list(self._owners.items()):
            if not any(worker.active for worker in state.workers.values()):
                del self._owners[key]
                return
        raise SubagentError(
            "the bounded owner cache is full of active parent sessions",
            code="owner_limit",
        )

    def _owner_for_command(self, context: Mapping[str, Any]) -> Optional[OwnerState]:
        host_session_id = None
        if isinstance(context, Mapping):
            host = context.get("host")
            if isinstance(host, Mapping) and isinstance(host.get("session_id"), str):
                host_session_id = host["session_id"]
        with self._lock:
            if host_session_id is not None:
                matches = [
                    state
                    for state in self._owners.values()
                    if state.owner.host_session_id == host_session_id
                ]
                if len(matches) == 1:
                    return matches[0]
                return None
            if len(self._owners) == 1:
                return next(iter(self._owners.values()))
            return None

    def _refresh(
        self,
        client: AgentSessions,
        state: OwnerState,
        cancellation: Optional[Cancellation],
    ) -> None:
        self._check_cancelled(cancellation)
        snapshot = client.list_agents()
        self._check_cancelled(cancellation)
        self._reconcile_snapshot(state, snapshot)
        self._enforce_policy_descendants(client, state, cancellation)
        with self._lock:
            publish = self._snapshot_locked(state)
        self._publish(publish)

    def _reconcile_snapshot(
        self, state: OwnerState, snapshot: Mapping[str, Any]
    ) -> None:
        if not isinstance(snapshot, Mapping) or not isinstance(snapshot.get("agents"), list):
            raise SubagentError(
                "agent_sessions returned an invalid owned-tree snapshot",
                code="host_state_invalid",
            )
        records = snapshot["agents"]
        if len(records) > 64:
            raise SubagentError(
                "agent_sessions exceeded the extension's 64-record observation bound",
                code="host_state_invalid",
            )
        observed = set()
        with self._lock:
            for raw in records:
                if not isinstance(raw, Mapping):
                    raise SubagentError(
                        "agent_sessions returned a non-object agent record",
                        code="host_state_invalid",
                    )
                agent_id = raw.get("agent_id")
                path = raw.get("agent_path")
                if (
                    not isinstance(agent_id, str)
                    or _AGENT_ID_RE.fullmatch(agent_id) is None
                    or not isinstance(path, str)
                    or _AGENT_PATH_RE.fullmatch(path) is None
                ):
                    raise SubagentError(
                        "agent_sessions returned an invalid agent identity",
                        code="host_state_invalid",
                    )
                observed.add(agent_id)
                worker = state.workers.get(agent_id)
                if worker is None:
                    worker = self._recover_worker(state.owner, raw)
                    state.workers[agent_id] = worker
                    if worker.idempotency_key and worker.fingerprint:
                        existing = state.idempotency.get(worker.idempotency_key)
                        recovered = (worker.fingerprint, worker.agent_id)
                        if existing is not None and existing != recovered:
                            raise SubagentError(
                                "agent_sessions returned conflicting durable idempotency metadata",
                                code="host_state_invalid",
                            )
                        state.idempotency[worker.idempotency_key] = recovered
                self._update_worker_from_record(worker, raw)
                state.workers.move_to_end(agent_id)
            stale_ids = [
                worker.agent_id
                for worker in state.workers.values()
                if worker.agent_id not in observed
            ]
            for agent_id in stale_ids:
                state.workers.pop(agent_id, None)
            if stale_ids:
                stale = set(stale_ids)
                state.idempotency = {
                    key: value
                    for key, value in state.idempotency.items()
                    if value[1] not in stale
                }
            self._trim_workers_locked(state)
            persistence_error = snapshot.get("persistence_error")
            if isinstance(persistence_error, str) and persistence_error.strip():
                for worker in state.workers.values():
                    if worker.active:
                        worker.last_error = sanitize_document(
                            "Host session persistence is degraded: %s" % persistence_error,
                            MAX_ERROR_BYTES,
                        )

    def _recover_worker(self, owner: Owner, record: Mapping[str, Any]) -> Worker:
        agent_id = str(record["agent_id"])
        task_name = record.get("display_name", record.get("task_name"))
        if (
            not isinstance(task_name, str)
            or not task_name
            or task_name.startswith("ext-")
            or len(task_name.encode("utf-8")) > 128
        ):
            task_name = "recovered-%s" % safe_label(agent_id, "worker")
        (
            effective_tools,
            max_turns,
            max_tokens,
            max_cost,
            max_output,
            timeout_ms,
            deadline_at_ms,
        ) = _host_policy(record)
        profile = record.get("profile")
        if profile not in PROFILE_INSTRUCTIONS:
            raise SubagentError(
                "agent_sessions omitted the durable worker profile",
                code="host_state_invalid",
            )
        idempotency_key = record.get("idempotency_key")
        fingerprint = record.get("fingerprint")
        if (
            not isinstance(idempotency_key, str)
            or _IDEMPOTENCY_RE.fullmatch(idempotency_key) is None
            or not isinstance(fingerprint, str)
            or _FINGERPRINT_RE.fullmatch(fingerprint) is None
        ):
            raise SubagentError(
                "agent_sessions omitted durable idempotency metadata",
                code="host_state_invalid",
            )
        state_name, _ = host_state(record)
        created_at_ms, started_at_ms, completed_at_ms = _host_timestamps(
            record, state_name, deadline_at_ms
        )
        return Worker(
            agent_id=agent_id,
            agent_path=str(record["agent_path"]),
            parent_id=(
                record.get("parent_id")
                if isinstance(record.get("parent_id"), str)
                and _AGENT_ID_RE.fullmatch(record["parent_id"]) is not None
                else None
            ),
            depth=depth_from_record(record),
            name=safe_label(task_name),
            profile=profile,
            requested_model="inherit",
            effective_model=owner.inherited_model or "inherited",
            tools=effective_tools,
            state="restarted",
            phase="recovered from host ancestry",
            created_at_ms=created_at_ms,
            started_at_ms=started_at_ms,
            deadline_at_ms=deadline_at_ms,
            timeout_seconds=(
                None if timeout_ms is None else max(1, timeout_ms // 1000)
            ),
            max_turns=max_turns,
            max_output_bytes=max_output,
            max_tokens=max_tokens,
            max_cost_microdollars=max_cost,
            idempotency_key=idempotency_key,
            fingerprint=fingerprint,
            completed_at_ms=completed_at_ms,
            recovered=True,
            restart_count=1,
            generation=owner.process_generation,
        )

    def _update_worker_from_record(
        self, worker: Worker, record: Mapping[str, Any]
    ) -> None:
        state_name, status = host_state(record)
        mapped = {
            "pending": "queued",
            "queued": "queued",
            "running": "running",
            "waiting": "waiting",
            "completed": "done",
            "done": "done",
            "failed": "failed",
            "interrupted": "cancelled",
            "cancelled": "cancelled",
            "shutdown": "orphaned",
            "orphaned": "orphaned",
            "timed_out": "timed_out",
            "stopped": "stopped",
            "restarted": "restarted",
        }.get(state_name, "orphaned")
        if state_name == "interrupted":
            if worker.timeout_requested:
                mapped = "timed_out"
            elif worker.stop_requested:
                mapped = "stopped"
        if worker.timeout_requested and mapped in {"running", "queued", "waiting"}:
            mapped = "timed_out"
        elif worker.stop_requested and mapped in {"running", "queued", "waiting"}:
            mapped = "stopping"
        elif worker.state == "waiting" and mapped == "running":
            mapped = "waiting"
        worker.state = mapped
        worker.agent_path = str(record.get("agent_path", worker.agent_path))
        parent_id = record.get("parent_id")
        if isinstance(parent_id, str) and _AGENT_ID_RE.fullmatch(parent_id) is not None:
            worker.parent_id = parent_id
        worker.depth = depth_from_record(record)
        (
            effective_tools,
            max_turns,
            max_tokens,
            max_cost,
            max_output,
            timeout_ms,
            deadline_at_ms,
        ) = _host_policy(record)
        worker.tools = effective_tools
        worker.max_turns = max_turns
        worker.max_tokens = max_tokens
        worker.max_cost_microdollars = max_cost
        worker.max_output_bytes = max_output
        worker.timeout_seconds = (
            None if timeout_ms is None else max(1, timeout_ms // 1000)
        )
        worker.deadline_at_ms = deadline_at_ms
        profile = record.get("profile")
        idempotency_key = record.get("idempotency_key")
        fingerprint = record.get("fingerprint")
        if profile not in PROFILE_INSTRUCTIONS:
            raise SubagentError(
                "agent_sessions omitted the durable worker profile",
                code="host_state_invalid",
            )
        if (
            not isinstance(idempotency_key, str)
            or _IDEMPOTENCY_RE.fullmatch(idempotency_key) is None
            or not isinstance(fingerprint, str)
            or _FINGERPRINT_RE.fullmatch(fingerprint) is None
        ):
            raise SubagentError(
                "agent_sessions omitted durable idempotency metadata",
                code="host_state_invalid",
            )
        created_at_ms, started_at_ms, completed_at_ms = _host_timestamps(
            record, state_name, deadline_at_ms
        )
        worker.profile = profile
        worker.idempotency_key = idempotency_key
        worker.fingerprint = fingerprint
        worker.created_at_ms = created_at_ms
        worker.started_at_ms = started_at_ms
        worker.completed_at_ms = completed_at_ms
        session = record.get("session")
        if (
            isinstance(session, str)
            and session
            and len(session.encode("utf-8")) <= 1024
            and all(32 <= ord(character) < 127 or ord(character) >= 160 for character in session)
        ):
            worker.session = session
        export_ref = record.get("export_reference")
        if isinstance(export_ref, str) and export_ref:
            worker.export_reference = sanitize_document(export_ref, 1024)
        phase = record.get("phase", status.get("phase"))
        tool_name = record.get("tool_name", status.get("tool_name"))
        if isinstance(tool_name, str) and tool_name.strip():
            worker.current_tool = safe_label(tool_name)
            worker.phase = "using %s" % worker.current_tool
        elif isinstance(phase, str) and phase.strip():
            worker.current_tool = None
            worker.phase = safe_label(phase)
        else:
            worker.current_tool = None
            worker.phase = {
                "queued": "queued by host",
                "running": "running in host session",
                "waiting": "waiting for host completion",
                "done": "completed",
                "failed": "failed",
                "cancelled": "cancelled",
                "stopped": "stopped",
                "timed_out": "timed out",
                "orphaned": "host session unavailable",
                "restarted": "recovered after restart",
                "stopping": "host interrupt requested",
            }.get(worker.state, "unknown")
        (
            turns,
            tool_calls,
            input_tokens,
            cache_read_tokens,
            cache_write_tokens,
            output_tokens,
            reasoning_tokens,
            tokens,
            cost,
        ) = aggregate_usage(record, status)
        if turns is not None:
            worker.turn_count = turns
        if tool_calls is not None:
            worker.tool_call_count = tool_calls
        if input_tokens is not None:
            worker.input_tokens = input_tokens
        if cache_read_tokens is not None:
            worker.cache_read_tokens = cache_read_tokens
        if cache_write_tokens is not None:
            worker.cache_write_tokens = cache_write_tokens
        if output_tokens is not None:
            worker.output_tokens = output_tokens
        if reasoning_tokens is not None:
            worker.reasoning_tokens = reasoning_tokens
        if tokens is not None:
            worker.tokens_used = tokens
        if cost is not None:
            worker.cost_microdollars = cost
        worker.artifacts = parse_artifacts(record, status)
        worker.recent_tools = parse_recent_tools(record)
        delivery = record.get("delivery_state", status.get("delivery_state"))
        if delivery in {"pending", "claimed", "acked", "host_managed"}:
            worker.delivery_state = str(delivery)
        if worker.state == "done":
            output = status.get("output")
            if isinstance(output, str):
                worker.summary = sanitize_document(output, worker.max_output_bytes)
        if worker.state == "failed":
            error = status.get("error")
            if isinstance(error, str):
                worker.last_error = sanitize_document(error, MAX_ERROR_BYTES)

    def _enforce_policy_descendants(
        self,
        client: AgentSessions,
        state: OwnerState,
        cancellation: Optional[Cancellation],
    ) -> None:
        with self._lock:
            forbidden = [
                worker.agent_id
                for worker in state.workers.values()
                if worker.depth > MAX_DEPTH and worker.active
            ]
        for agent_id in forbidden:
            self._check_cancelled(cancellation)
            try:
                client.interrupt_agent(agent_id)
                error = "Recursive child interrupt requested: V1 permits depth one only."
            except Exception as exception:
                error = "Recursive child violated depth one; host interrupt failed: %s" % exception
            with self._lock:
                worker = state.workers.get(agent_id)
                if worker is not None:
                    worker.stop_requested = True
                    worker.state = "stopping"
                    worker.phase = "interrupting rejected recursive descendant"
                    worker.last_error = sanitize_document(error, MAX_ERROR_BYTES)

    def _reserve_spawn_locked(self, state: OwnerState, request: SpawnRequest) -> None:
        if request.idempotency_key in state.pending_spawns:
            previous = state.pending_spawns[request.idempotency_key]
            if previous.fingerprint != request.fingerprint:
                raise SubagentError(
                    "idempotency_key is already in flight with different input",
                    code="idempotency_conflict",
                )
            raise SubagentError(
                "the identical idempotent spawn is already in flight; retry the same key",
                code="spawn_in_flight",
            )
        name_matches = [worker for worker in state.workers.values() if worker.name == request.name]
        if name_matches:
            raise SubagentError(
                "worker name already exists for this parent; inspect it or choose a new bounded name",
                code="duplicate_name",
            )
        active = sum(worker.active for worker in state.workers.values()) + len(state.pending_spawns)
        if active >= MAX_ACTIVE_CHILDREN:
            raise SubagentError(
                "subagent concurrency limit reached (%d active children)"
                % MAX_ACTIVE_CHILDREN,
                code="concurrency_limit",
            )
        if len(state.workers) + len(state.pending_spawns) >= MAX_WORKERS_PER_OWNER:
            raise SubagentError(
                "subagent total worker limit reached for this parent (%d)"
                % MAX_WORKERS_PER_OWNER,
                code="worker_limit",
            )
        state.pending_spawns[request.idempotency_key] = request

    def _existing_idempotent_locked(
        self, state: OwnerState, request: SpawnRequest
    ) -> Optional[Worker]:
        existing = state.idempotency.get(request.idempotency_key)
        if existing is None:
            return None
        fingerprint, agent_id = existing
        if fingerprint != request.fingerprint:
            raise SubagentError(
                "idempotency_key was reused with different input",
                code="idempotency_conflict",
            )
        worker = state.workers.get(agent_id)
        if worker is None:
            state.idempotency.pop(request.idempotency_key, None)
            return None
        return worker

    def _worker_from_spawn(
        self,
        owner: Owner,
        request: SpawnRequest,
        response: Mapping[str, Any],
    ) -> Worker:
        if not isinstance(response, Mapping):
            raise SubagentError(
                "agent_sessions returned an invalid spawn response",
                code="host_state_invalid",
            )
        agent_id = response.get("agent_id")
        path = response.get("agent_path")
        if (
            not isinstance(agent_id, str)
            or _AGENT_ID_RE.fullmatch(agent_id) is None
            or not isinstance(path, str)
            or _AGENT_PATH_RE.fullmatch(path) is None
        ):
            raise SubagentError(
                "agent_sessions spawn omitted a bounded child identity",
                code="host_state_invalid",
            )
        depth = depth_from_record(response)
        (
            effective_tools,
            max_turns,
            max_tokens,
            max_cost,
            max_output,
            timeout_ms,
            deadline_at_ms,
        ) = _host_policy(response)
        if (
            response.get("profile") != request.profile
            or response.get("idempotency_key") != request.idempotency_key
            or response.get("fingerprint") != request.fingerprint
        ):
            raise SubagentError(
                "agent_sessions did not retain the requested recovery metadata",
                code="host_state_invalid",
            )
        state_name, _ = host_state(response)
        created_at_ms, started_at_ms, completed_at_ms = _host_timestamps(
            response, state_name, deadline_at_ms
        )
        return Worker(
            agent_id=agent_id,
            agent_path=path,
            parent_id=(
                response.get("parent_id")
                if isinstance(response.get("parent_id"), str)
                and _AGENT_ID_RE.fullmatch(response["parent_id"]) is not None
                else None
            ),
            depth=depth,
            name=request.name,
            profile=request.profile,
            requested_model=request.model,
            effective_model=owner.inherited_model or "inherited",
            tools=effective_tools,
            state="queued",
            phase="queued by host",
            created_at_ms=created_at_ms,
            started_at_ms=started_at_ms,
            deadline_at_ms=deadline_at_ms,
            timeout_seconds=(
                None if timeout_ms is None else max(1, timeout_ms // 1000)
            ),
            max_turns=max_turns,
            max_output_bytes=max_output,
            max_tokens=max_tokens,
            max_cost_microdollars=max_cost,
            completed_at_ms=completed_at_ms,
            idempotency_key=request.idempotency_key,
            fingerprint=request.fingerprint,
            recovered=False,
            generation=owner.process_generation,
        )

    def _resolve_locked(self, state: OwnerState, target: Optional[str]) -> Worker:
        if target is None:
            raise SubagentError("target is required")
        matches = [
            worker
            for worker in state.workers.values()
            if target in {worker.agent_id, worker.agent_path, worker.name}
        ]
        if not matches:
            raise SubagentError(
                "unknown worker for this host-derived owner",
                code="unknown_worker",
            )
        if len(matches) > 1:
            raise SubagentError(
                "worker name is ambiguous; use the stable agent ID",
                code="ambiguous_worker",
            )
        return matches[0]

    def _trim_workers_locked(self, state: OwnerState) -> None:
        while len(state.workers) > MAX_WORKERS_PER_OWNER:
            removable = next(
                (
                    agent_id
                    for agent_id, worker in state.workers.items()
                    if worker.terminal and agent_id != state.selected_agent_id
                ),
                None,
            )
            if removable is None:
                raise SubagentError(
                    "authoritative worker tree exceeds the local retention bound",
                    code="worker_limit",
                )
            del state.workers[removable]
            for key, (_, agent_id) in list(state.idempotency.items()):
                if agent_id == removable:
                    del state.idempotency[key]

    def _status_result_locked(
        self, state: OwnerState, selected: Optional[Worker]
    ) -> Dict[str, Any]:
        now = self._now_ms()
        workers = list(state.workers.values())
        result: Dict[str, Any] = {
            "operation": "status",
            "counts": {
                "active": sum(worker.active for worker in workers),
                "terminal": sum(worker.terminal for worker in workers),
                "total": len(workers),
            },
            "workers": [worker.public(now) for worker in workers],
            "persistence": "host_owned",
            "completion_delivery": "host_owned_claim_ack_parent_turn",
        }
        if selected is not None:
            result["worker"] = selected.public(now)
        return result

    def _spawn_result(
        self, worker: Worker, request: SpawnRequest, *, duplicate: bool
    ) -> Dict[str, Any]:
        return {
            "operation": "spawn",
            "accepted": True,
            "duplicate": duplicate,
            "background": request.background,
            "worker": worker.public(self._now_ms()),
            "completion_delivery": "host_owned_claim_ack_parent_turn",
            "next": "Use subagent_wait or subagent_status; do not poll aggressively.",
        }

    def _snapshot_locked(self, state: OwnerState) -> Mapping[str, Any]:
        snapshot = dict(
            build_snapshot(
                list(state.workers.values()),
                selected_agent_id=state.selected_agent_id,
                now_ms=self._now_ms(),
            )
        )
        snapshot["_resource_owner"] = {
            "session_id": state.owner.session_id,
            "extension_instance_id": state.owner.extension_instance_id,
            "process_generation": state.owner.process_generation,
        }
        return snapshot

    def _publish(self, snapshot: Mapping[str, Any]) -> None:
        callback = None
        with self._lock:
            callback = self._publish_callback
        if callback is None:
            return
        try:
            callback(snapshot)
        except Exception:
            # Presentation is an inert projection and cannot change authoritative
            # orchestration results or host cleanup.
            return

    @staticmethod
    def _check_cancelled(cancellation: Optional[Cancellation]) -> None:
        if cancellation is not None:
            cancellation.raise_if_cancelled()
