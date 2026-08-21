"""Deterministic fake of Ygg's host-owned API 0.2 agent_sessions service.

This is package test data, not a second runtime. It models owner/principal scoping,
idempotent spawn, host persistence, mailbox claim/ack, and descendant shutdown so
the extension can be tested without a model provider.
"""

from __future__ import annotations

from dataclasses import dataclass, field
import hashlib
import threading
from typing import Any, Dict, List, Mapping, Optional, Tuple


class FakeAgentSessionsError(RuntimeError):
    pass


def fake_session_reference(agent_id: str) -> str:
    digest = hashlib.sha256(("fake-team/" + agent_id).encode("ascii")).hexdigest()
    return "agent-session:" + digest


def _bounded_utf8(text: str, limit: int) -> str:
    encoded = text.encode("utf-8")
    if len(encoded) <= limit:
        return text
    marker = "\n...[truncated]".encode("utf-8")
    prefix = encoded[: max(0, limit - len(marker))]
    while prefix:
        try:
            return prefix.decode("utf-8") + marker.decode("utf-8")
        except UnicodeDecodeError:
            prefix = prefix[:-1]
    return ""


class ManualClock:
    def __init__(self, start_ms: int = 1_700_000_000_000) -> None:
        self.value = start_ms
        self._lock = threading.Lock()

    def __call__(self) -> int:
        with self._lock:
            return self.value

    def advance(self, milliseconds: int) -> int:
        with self._lock:
            self.value += milliseconds
            return self.value


@dataclass
class FakeDelivery:
    delivery_id: str
    agent_id: str
    summary: str
    session: str
    artifacts: List[Dict[str, Any]]
    claimed: bool = False


@dataclass
class FakeAgent:
    agent_id: str
    agent_path: str
    parent_id: str
    task_name: str
    profile: Optional[str]
    idempotency_key: str
    fingerprint: Optional[str]
    depth: int
    session: str
    policy: Dict[str, Any]
    created_at_ms: int
    deadline_at_ms: Optional[int]
    started_at_ms: Optional[int] = None
    completed_at_ms: Optional[int] = None
    status: Dict[str, Any] = field(default_factory=lambda: {"state": "pending"})
    phase: str = "queued"
    usage: Dict[str, int] = field(default_factory=dict)
    tool_call_count: int = 0
    artifacts: List[Dict[str, Any]] = field(default_factory=list)
    export_reference: Optional[str] = None
    delivery_enqueued: bool = False
    delivery_state: str = "host_managed"

    def record(self) -> Dict[str, Any]:
        return {
            "agent_id": self.agent_id,
            "agent_path": self.agent_path,
            "parent_id": self.parent_id,
            "task_name": self.task_name,
            "profile": self.profile,
            "idempotency_key": self.idempotency_key,
            "fingerprint": self.fingerprint,
            "depth": self.depth,
            "session": self.session,
            "status": dict(self.status),
            "phase": self.phase,
            "policy": dict(self.policy),
            "created_at_ms": self.created_at_ms,
            "started_at_ms": self.started_at_ms,
            "completed_at_ms": self.completed_at_ms,
            "deadline_at_ms": self.deadline_at_ms,
            "turn_count": self.usage.get("turns", 0),
            "tool_call_count": self.tool_call_count,
            "usage": {
                "input_tokens": self.usage.get("input_tokens", 0),
                "cache_read_tokens": 0,
                "cache_write_tokens": 0,
                "cache_write_1h_tokens": 0,
                "output_tokens": self.usage.get("output_tokens", 0),
                "reasoning_tokens": 0,
                "total_tokens": self.usage.get("total_tokens", 0),
            },
            "cost_microdollars": self.usage.get("cost_microdollars"),
            "provenance": {"kind": "extension_agent_session"},
            "delivery_state": self.delivery_state,
        }


@dataclass
class _Spawn:
    task_name: str
    profile: Optional[str]
    fingerprint: Optional[str]
    message: str
    policy: Dict[str, Any]
    result: Dict[str, Any]


class FakeHostState:
    def __init__(self, clock: Optional[ManualClock] = None) -> None:
        self.clock = clock or ManualClock()
        self.next_agent = 1
        self.next_delivery = 1
        self.agents: Dict[str, FakeAgent] = {}
        self.owners: Dict[Tuple[str, str], List[str]] = {}
        self.spawns: Dict[Tuple[str, str, str], _Spawn] = {}
        self.deliveries: Dict[Tuple[str, str], List[FakeDelivery]] = {}
        self.spawn_messages: List[str] = []
        self.calls: List[Tuple[str, str, str]] = []
        self.steers: List[Tuple[str, str]] = []
        self.follow_ups: List[Tuple[str, str]] = []
        self.persistence_error: Optional[str] = None
        self._lock = threading.RLock()

    def client(
        self,
        *,
        owner: str = "owner-a",
        principal: str = "ygg-subagents@test",
        owner_path: str = "/root",
        max_active: int = 8,
    ) -> "FakeAgentSessions":
        return FakeAgentSessions(
            self,
            owner=owner,
            principal=principal,
            owner_path=owner_path,
            max_active=max_active,
        )

    def start(self, agent_id: str, *, phase: str = "searching", tool_name: Optional[str] = None) -> None:
        with self._lock:
            agent = self._agent(agent_id)
            agent.status = {"state": "running"}
            if agent.started_at_ms is None:
                agent.started_at_ms = self.clock()
            agent.phase = phase
            if tool_name:
                agent.tool_call_count += 1
                agent.status["tool_name"] = tool_name

    def complete(
        self,
        agent_id: str,
        output: str,
        *,
        turns: int = 2,
        input_tokens: int = 800,
        output_tokens: int = 200,
        cost_microdollars: int = 1200,
        artifacts: Optional[List[Mapping[str, Any]]] = None,
    ) -> None:
        with self._lock:
            agent = self._agent(agent_id)
            output = _bounded_utf8(output, int(agent.policy["max_output_bytes"]))
            if agent.started_at_ms is None:
                agent.started_at_ms = agent.created_at_ms
            agent.completed_at_ms = self.clock()
            agent.status = {"state": "completed", "output": output}
            agent.phase = "completed"
            agent.usage = {
                "turns": turns,
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "total_tokens": input_tokens + output_tokens,
                "cost_microdollars": cost_microdollars,
            }
            agent.artifacts = [dict(value) for value in (artifacts or [])]
            agent.export_reference = "export:%s" % agent.agent_id
            self._enqueue_delivery(agent, output)

    def fail(self, agent_id: str, error: str) -> None:
        with self._lock:
            agent = self._agent(agent_id)
            if agent.started_at_ms is None:
                agent.started_at_ms = agent.created_at_ms
            agent.completed_at_ms = self.clock()
            agent.status = {"state": "failed", "error": error}
            agent.phase = "failed"
            self._enqueue_delivery(agent, "Worker failed: %s" % error)

    def claim_completion(self, *, owner: str, principal: str) -> Optional[Dict[str, Any]]:
        key = (principal, owner)
        with self._lock:
            for delivery in self.deliveries.get(key, []):
                if delivery.claimed:
                    continue
                delivery.claimed = True
                agent = self.agents[delivery.agent_id]
                agent.delivery_state = "claimed"
                return {
                    "delivery_id": delivery.delivery_id,
                    "kind": "subagent_completion",
                    "agent_id": delivery.agent_id,
                    "summary": delivery.summary,
                    "session": delivery.session,
                    "artifacts": [dict(value) for value in delivery.artifacts],
                    "legal_new_parent_turn": True,
                    "state": agent.status["state"],
                }
        return None

    def acknowledge_completion(
        self, delivery_id: str, *, owner: str, principal: str, committed: bool
    ) -> bool:
        key = (principal, owner)
        with self._lock:
            values = self.deliveries.get(key, [])
            for index, delivery in enumerate(values):
                if delivery.delivery_id != delivery_id:
                    continue
                agent = self.agents[delivery.agent_id]
                if committed:
                    agent.delivery_state = "acked"
                    del values[index]
                else:
                    agent.delivery_state = "pending"
                    delivery.claimed = False
                return True
        return False

    def parent_turn_delivery(
        self, *, owner: str, principal: str, commit: bool = True
    ) -> Optional[Dict[str, Any]]:
        event = self.claim_completion(owner=owner, principal=principal)
        if event is None:
            return None
        self.acknowledge_completion(
            event["delivery_id"],
            owner=owner,
            principal=principal,
            committed=commit,
        )
        return event

    def export_session(
        self, target: str, *, owner: str, principal: str
    ) -> Dict[str, Any]:
        with self._lock:
            owned = self._owned_ids(principal, owner)
            agent = self._resolve(target)
            if agent.agent_id not in owned:
                raise FakeAgentSessionsError("cross-owner session export denied")
            return {
                "session": agent.session,
                "export_reference": agent.export_reference or "export:%s" % agent.agent_id,
                "messages": [
                    {"role": "user", "content": "<bounded task omitted from UI fixture>"},
                    {
                        "role": "assistant",
                        "content": agent.status.get("output", "<worker still running>"),
                    },
                ],
                "read_only": True,
            }

    def shutdown_principal(self, principal: str) -> None:
        with self._lock:
            roots = []
            for (candidate, _owner), ids in self.owners.items():
                if candidate == principal:
                    roots.extend(ids)
            root_paths = [self.agents[agent_id].agent_path for agent_id in roots]
            for agent in self.agents.values():
                if any(
                    agent.agent_path == path or agent.agent_path.startswith(path + "/")
                    for path in root_paths
                ):
                    agent.completed_at_ms = self.clock()
                    agent.status = {"state": "shutdown"}
                    agent.phase = "host shutdown"

    def shut_down(self) -> None:
        """Model a host shutdown: every owned descendant becomes orphaned."""
        for principal in [key[0] for key in self.owners]:
            self.shutdown_principal(principal)

    def _expire_deadlines(self) -> None:
        now = self.clock()
        for agent in self.agents.values():
            if (
                agent.deadline_at_ms is not None
                and agent.status.get("state") in {"pending", "running"}
                and now >= agent.deadline_at_ms
            ):
                agent.completed_at_ms = now
                agent.status = {"state": "timed_out"}
                agent.phase = "host wall deadline reached"

    def _enqueue_delivery(self, agent: FakeAgent, summary: str) -> None:
        if agent.delivery_enqueued:
            return
        key = next(
            (
                owner_key
                for owner_key, ids in self.owners.items()
                if agent.agent_id in ids
            ),
            None,
        )
        if key is None:
            return
        delivery = FakeDelivery(
            delivery_id="delivery-%d" % self.next_delivery,
            agent_id=agent.agent_id,
            summary=summary,
            session=agent.session,
            artifacts=[dict(value) for value in agent.artifacts],
        )
        self.next_delivery += 1
        self.deliveries.setdefault(key, []).append(delivery)
        agent.delivery_enqueued = True
        agent.delivery_state = "pending"

    def _owned_ids(self, principal: str, owner: str) -> List[str]:
        roots = self.owners.get((principal, owner), [])
        paths = [self.agents[agent_id].agent_path for agent_id in roots]
        return [
            agent.agent_id
            for agent in self.agents.values()
            if any(
                agent.agent_path == path or agent.agent_path.startswith(path + "/")
                for path in paths
            )
        ]

    def _resolve(self, target: str) -> FakeAgent:
        if target in self.agents:
            return self.agents[target]
        matches = [agent for agent in self.agents.values() if agent.agent_path == target]
        if len(matches) != 1:
            raise FakeAgentSessionsError("unknown fake agent target")
        return matches[0]

    def _agent(self, agent_id: str) -> FakeAgent:
        try:
            return self.agents[agent_id]
        except KeyError as error:
            raise FakeAgentSessionsError("unknown fake agent") from error


class FakeAgentSessions:
    def __init__(
        self,
        host: FakeHostState,
        *,
        owner: str,
        principal: str,
        owner_path: str,
        max_active: int,
    ) -> None:
        self.host = host
        self.owner = owner
        self.principal = principal
        self.owner_path = owner_path.rstrip("/") or "/root"
        self.max_active = max_active
        self.cancel_next: Optional[str] = None

    def spawn_agent(
        self,
        *,
        task_name: str,
        profile: Optional[str],
        fingerprint: Optional[str],
        message: str,
        idempotency_key: str,
        tools,
        max_depth: int,
        max_concurrent_children: int,
        max_turns: Optional[int],
        max_tokens: Optional[int],
        max_cost_microdollars: Optional[int],
        max_output_bytes: int,
        timeout_ms: Optional[int],
    ) -> Mapping[str, Any]:
        self._maybe_cancel("spawn")
        policy = {
            "tools": list(tools),
            "max_depth": max_depth,
            "max_concurrent_children": max_concurrent_children,
            "max_turns": max_turns,
            "max_tokens": max_tokens,
            "max_cost_microdollars": max_cost_microdollars,
            "max_output_bytes": max_output_bytes,
            "timeout_ms": timeout_ms,
        }
        key = (self.principal, self.owner, idempotency_key)
        with self.host._lock:
            self.host.calls.append((self.principal, self.owner, "spawn"))
            existing = self.host.spawns.get(key)
            if existing is not None:
                if (
                    existing.task_name != task_name
                    or existing.profile != profile
                    or existing.fingerprint != fingerprint
                    or existing.message != message
                    or existing.policy != policy
                ):
                    raise FakeAgentSessionsError(
                        "spawn idempotency_key was reused with different input"
                    )
                return dict(existing.result)
            if (
                not policy["tools"]
                or len(set(policy["tools"])) != len(policy["tools"])
                or any(
                    tool not in {"read", "search", "edit", "write", "bash"}
                    for tool in policy["tools"]
                )
            ):
                raise FakeAgentSessionsError("host rejected child tools outside the whitelist")
            parent_depth = max(
                0, len([part for part in self.owner_path.split("/") if part]) - 1
            )
            if parent_depth + 1 > max_depth:
                raise FakeAgentSessionsError("host extension child depth limit reached")
            active = [
                self.host.agents[agent_id]
                for agent_id in self.host._owned_ids(self.principal, self.owner)
                if self.host.agents[agent_id].status["state"] in {"pending", "running"}
            ]
            if len(active) >= min(self.max_active, max_concurrent_children):
                raise FakeAgentSessionsError("host delegation concurrency limit reached")
            if len(self.host.owners.get((self.principal, self.owner), [])) >= 32:
                raise FakeAgentSessionsError("host extension child total limit reached")
            number = self.host.next_agent
            self.host.next_agent += 1
            agent_id = "agent-%d" % number
            path = "%s/%s" % (self.owner_path, task_name)
            if any(agent.agent_path == path for agent in self.host.agents.values()):
                raise FakeAgentSessionsError("task name already exists under owner")
            depth = max(0, len([part for part in path.split("/") if part]) - 1)
            parent_id = "root" if self.owner_path == "/root" else self.owner_path.rsplit("/", 1)[-1]
            now = self.host.clock()
            agent = FakeAgent(
                agent_id=agent_id,
                agent_path=path,
                parent_id=parent_id,
                task_name=task_name,
                profile=profile,
                idempotency_key=idempotency_key,
                fingerprint=fingerprint,
                depth=depth,
                session=fake_session_reference(agent_id),
                policy=dict(policy),
                created_at_ms=now,
                deadline_at_ms=(now + timeout_ms) if timeout_ms is not None else None,
            )
            self.host.agents[agent_id] = agent
            self.host.owners.setdefault((self.principal, self.owner), []).append(agent_id)
            result = {
                "agent_id": agent_id,
                "agent_path": path,
                "task_name": task_name,
                "profile": profile,
                "idempotency_key": idempotency_key,
                "fingerprint": fingerprint,
                "status": "pending",
                "depth": depth,
                "principal": self.principal,
                "resource_owner": self.owner,
                "policy": dict(policy),
                "created_at_ms": agent.created_at_ms,
                "started_at_ms": agent.started_at_ms,
                "completed_at_ms": agent.completed_at_ms,
                "deadline_at_ms": agent.deadline_at_ms,
            }
            self.host.spawns[key] = _Spawn(
                task_name, profile, fingerprint, message, dict(policy), dict(result)
            )
            self.host.spawn_messages.append(message)
            return result

    def list_agents(self) -> Mapping[str, Any]:
        self._maybe_cancel("list")
        with self.host._lock:
            self.host.calls.append((self.principal, self.owner, "list"))
            self.host._expire_deadlines()
            ids = self.host._owned_ids(self.principal, self.owner)
            return {
                "principal": self.principal,
                "resource_owner": self.owner,
                "persistence_error": self.host.persistence_error,
                "agents": [self.host.agents[agent_id].record() for agent_id in ids],
            }

    def wait_agents(self, *, timeout_ms: int) -> Mapping[str, Any]:
        self._maybe_cancel("wait")
        if not 1 <= timeout_ms <= 60_000:
            raise FakeAgentSessionsError("fake wait bound exceeded")
        self.host.clock.advance(timeout_ms)
        snapshot = self.list_agents()
        running = any(
            record["status"]["state"] in {"pending", "running"}
            for record in snapshot["agents"]
        )
        return {"timed_out": running, "snapshot": snapshot}

    def interrupt_agent(self, target: str) -> Mapping[str, Any]:
        self._maybe_cancel("interrupt")
        with self.host._lock:
            self.host.calls.append((self.principal, self.owner, "interrupt"))
            agent = self.host._resolve(target)
            owned = self.host._owned_ids(self.principal, self.owner)
            if agent.agent_id not in owned:
                raise FakeAgentSessionsError("extension principal may access only owned trees")
            previous = agent.status["state"]
            requested = previous in {"pending", "running"}
            if requested:
                root_path = agent.agent_path
                for candidate in self.host.agents.values():
                    if candidate.agent_path == root_path or candidate.agent_path.startswith(root_path + "/"):
                        candidate.completed_at_ms = self.host.clock()
                        candidate.status = {"state": "interrupted"}
                        candidate.phase = "interrupted"
            return {
                "agent_id": agent.agent_id,
                "agent_path": agent.agent_path,
                "previous_status": previous,
                "interrupt_requested": requested,
            }

    def send_agent_message(self, target: str, message: str) -> Mapping[str, Any]:
        self._maybe_cancel("message")
        if not isinstance(message, str) or not message:
            raise FakeAgentSessionsError("agent message must be a non-empty string")
        with self.host._lock:
            self.host.calls.append((self.principal, self.owner, "message"))
            agent = self.host._resolve(target)
            owned = self.host._owned_ids(self.principal, self.owner)
            if agent.agent_id not in owned:
                raise FakeAgentSessionsError("extension principal may access only owned trees")
            if agent.status["state"] == "shutdown":
                raise FakeAgentSessionsError("target is shut down")
            self.host.steers.append((agent.agent_id, message))
            return {"agent_id": agent.agent_id, "queued": True}

    def follow_up_agent(self, target: str, message: str) -> Mapping[str, Any]:
        self._maybe_cancel("follow_up")
        if not isinstance(message, str) or not message:
            raise FakeAgentSessionsError("agent message must be a non-empty string")
        with self.host._lock:
            self.host.calls.append((self.principal, self.owner, "follow_up"))
            agent = self.host._resolve(target)
            owned = self.host._owned_ids(self.principal, self.owner)
            if agent.agent_id not in owned:
                raise FakeAgentSessionsError("extension principal may access only owned trees")
            if agent.status["state"] == "shutdown":
                raise FakeAgentSessionsError("target is shut down")
            previous = agent.status["state"]
            if previous in {"pending", "running"}:
                # A live session absorbs the message; no new run starts.
                self.host.follow_ups.append((agent.agent_id, message))
                return {
                    "agent_id": agent.agent_id,
                    "agent_path": agent.agent_path,
                    "previous_status": previous,
                    "delivery": "follow_up",
                }
            # A settled session is resumed: the host marks it pending and
            # drops the stale completion timestamp (see the delegation host).
            agent.status = {"state": "pending"}
            agent.phase = "follow-up queued"
            agent.completed_at_ms = None
            self.host.follow_ups.append((agent.agent_id, message))
            return {
                "agent_id": agent.agent_id,
                "agent_path": agent.agent_path,
                "previous_status": previous,
                "delivery": "new_run",
            }

    def _maybe_cancel(self, operation: str) -> None:
        if self.cancel_next == operation:
            self.cancel_next = None
            raise FakeAgentSessionsError("fake %s request cancelled" % operation)
