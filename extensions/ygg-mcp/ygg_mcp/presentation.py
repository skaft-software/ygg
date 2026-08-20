"""Frontend-neutral, bounded presentation state for TUI, Serve, and headless use."""

from __future__ import annotations

from collections import deque
from dataclasses import dataclass, replace
import json
import threading
import time
from typing import Any, Callable, Iterable, Mapping, Optional
import uuid


PRESENTATION_SCHEMA_VERSION = 1
MAX_PRESENTATION_SERVERS = 32
MAX_PRESENTATION_TOOLS = 256
MAX_PRESENTATION_ACTIVITIES = 64
MAX_SAFE_SUMMARY_BYTES = 512
_ALLOWED_STATES = {
    "configured",
    "connecting",
    "ready",
    "refreshing",
    "degraded",
    "backoff",
    "parked",
    "stopped",
}


@dataclass(frozen=True)
class Activity:
    id: str
    server_id: str
    tool: str
    state: str
    started_at_ms: int
    finished_at_ms: Optional[int] = None
    outcome: Optional[str] = None


class PresentationProducer:
    """Own semantic sequence/activity state without rendering either frontend."""

    def __init__(
        self,
        *,
        generation: Optional[str] = None,
        clock: Optional[Callable[[], int]] = None,
    ) -> None:
        self.generation = generation or f"mcp-{uuid.uuid4().hex}"
        self._clock = clock or (lambda: int(time.time() * 1000))
        self._sequence = 0
        self._activity_sequence = 0
        self._activities: deque[Activity] = deque(maxlen=MAX_PRESENTATION_ACTIVITIES)
        self._lock = threading.RLock()
        self._updated_at_ms = self._clock()

    @property
    def sequence(self) -> int:
        with self._lock:
            return self._sequence

    def touch(self) -> int:
        with self._lock:
            self._sequence += 1
            self._updated_at_ms = self._clock()
            return self._sequence

    def start_activity(self, server_id: str, tool: str) -> str:
        with self._lock:
            self._activity_sequence += 1
            identifier = f"activity-{self._activity_sequence}"
            self._activities.append(
                Activity(
                    id=identifier,
                    server_id=_safe_identifier(server_id),
                    tool=_safe_identifier(tool),
                    state="running",
                    started_at_ms=self._clock(),
                )
            )
            self._sequence += 1
            self._updated_at_ms = self._clock()
            return identifier

    def finish_activity(self, identifier: str, outcome: str) -> None:
        allowed = {"succeeded", "failed", "cancelled", "timedOut", "ambiguous"}
        safe_outcome = outcome if outcome in allowed else "failed"
        with self._lock:
            replacement: deque[Activity] = deque(maxlen=MAX_PRESENTATION_ACTIVITIES)
            for activity in self._activities:
                if activity.id == identifier and activity.state == "running":
                    activity = replace(
                        activity,
                        state="settled",
                        finished_at_ms=self._clock(),
                        outcome=safe_outcome,
                    )
                replacement.append(activity)
            self._activities = replacement
            self._sequence += 1
            self._updated_at_ms = self._clock()

    def snapshot(
        self,
        servers: Iterable[Mapping[str, Any]],
        *,
        host_catalog_revision: int,
        config_error: Optional[Mapping[str, Any]] = None,
    ) -> dict[str, Any]:
        """Build a side-effect-free current-state snapshot.

        Calling this method never launches a server, refreshes a catalog, repeats
        a tool call, or changes the producer sequence.
        """

        normalized_servers = [_normalize_server(server) for server in servers]
        normalized_servers.sort(key=lambda item: item["id"])
        if len(normalized_servers) > MAX_PRESENTATION_SERVERS:
            normalized_servers = normalized_servers[:MAX_PRESENTATION_SERVERS]
        tool_budget = MAX_PRESENTATION_TOOLS
        for server in normalized_servers:
            server["tools"] = server["tools"][:tool_budget]
            server["toolCount"] = len(server["tools"])
            tool_budget -= len(server["tools"])
        with self._lock:
            activities = [
                {
                    "id": activity.id,
                    "serverId": activity.server_id,
                    "tool": activity.tool,
                    "state": activity.state,
                    "startedAtMs": activity.started_at_ms,
                    "finishedAtMs": activity.finished_at_ms,
                    "outcome": activity.outcome,
                }
                for activity in self._activities
            ]
            sequence = self._sequence
            observed_at = self._updated_at_ms
        connected = sum(1 for server in normalized_servers if bool(server["connected"]))
        published = sum(server["toolCount"] for server in normalized_servers)
        refreshing = any(server["state"] == "refreshing" for server in normalized_servers)
        degraded = config_error is not None or any(
            server["state"] in {"degraded", "backoff", "parked"}
            for server in normalized_servers
        )
        snapshot: dict[str, Any] = {
            "schemaVersion": PRESENTATION_SCHEMA_VERSION,
            "producer": {"id": "ygg-mcp", "generation": self.generation},
            "sequence": sequence,
            "observedAtMs": observed_at,
            "summary": {
                "configuredServers": len(normalized_servers),
                "connectedServers": connected,
                "publishedTools": published,
                "hostCatalogRevision": max(0, int(host_catalog_revision)),
                "degraded": degraded,
                "refreshing": refreshing,
            },
            "servers": normalized_servers,
            "activities": activities,
        }
        if config_error is not None:
            snapshot["configurationError"] = {
                "code": _safe_identifier(config_error.get("code", "invalid_config")),
                "summary": _safe_summary(
                    config_error.get("summary", "MCP configuration is invalid")
                ),
            }
        return snapshot


def host_presentation(snapshot: Mapping[str, Any]) -> dict[str, Any]:
    """Project the package snapshot onto Ygg's generic presentation contract."""

    summary = snapshot.get("summary", {})
    configured = _nonnegative_int(summary.get("configuredServers"))
    connected = _nonnegative_int(summary.get("connectedServers"))
    published = _nonnegative_int(summary.get("publishedTools"))
    server_states = {
        server.get("state")
        for server in snapshot.get("servers", [])
        if isinstance(server, Mapping)
    }
    if configured == 0:
        generic_state = "empty"
    elif "connecting" in server_states:
        generic_state = "loading"
    elif bool(summary.get("refreshing")):
        generic_state = "running"
    elif bool(summary.get("degraded")):
        generic_state = "degraded"
    else:
        generic_state = "active"
    status_text = compact_status(snapshot)["text"]

    nodes: list[dict[str, Any]] = []
    actions: list[dict[str, Any]] = []
    state_map = {
        "configured": "empty",
        "connecting": "loading",
        "ready": "active",
        "refreshing": "running",
        "degraded": "degraded",
        "backoff": "degraded",
        "parked": "unavailable",
        "stopped": "stopped",
    }
    servers = snapshot.get("servers", [])[:MAX_PRESENTATION_SERVERS]
    for server in servers:
        if len(nodes) >= MAX_PRESENTATION_TOOLS:
            break
        server_id = _safe_identifier(server.get("id", "server"))
        node_id = f"server:{server_id}"
        action_ids: list[str] = []
        for action in server.get("actions", []):
            if not isinstance(action, Mapping) or action.get("enabled") is not True:
                continue
            action_name = action.get("id")
            if action_name not in {"refresh", "restart", "stop"} or len(actions) >= 64:
                continue
            action_id = f"{action_name}:{server_id}"
            action_ids.append(action_id)
            actions.append(
                {
                    "id": action_id,
                    "label": action_name.capitalize(),
                    "command": "mcp",
                    "arguments": [action_name, server_id],
                    "destructive": False,
                }
            )
        nodes.append(
            {
                "id": node_id,
                "state": state_map.get(server.get("state"), "degraded"),
                "label": _safe_label(server.get("label", server_id)),
                "secondary": (
                    f"stdio · catalog {_nonnegative_int(server.get('catalogRevision'))} · "
                    f"{_nonnegative_int(server.get('toolCount'))} tools"
                ),
                "action_ids": action_ids,
                "references": [],
            }
        )
        for tool in server.get("tools", []):
            if len(nodes) >= MAX_PRESENTATION_TOOLS:
                break
            tool_name = _safe_identifier(tool.get("name", "mcp_tool"))
            schema = tool.get("schemaSummary", {})
            nodes.append(
                {
                    "id": f"tool:{tool_name}",
                    "parent_id": node_id,
                    "state": "active",
                    "label": tool_name,
                    "secondary": (
                        f"{_nonnegative_int(schema.get('propertyCount'))} parameters · "
                        f"{tool.get('approval', 'unknown')}"
                    ),
                    "action_ids": [],
                    "references": [],
                }
            )

    activities = []
    activity_state = {
        ("running", None): "running",
        ("settled", "succeeded"): "succeeded",
        ("settled", "cancelled"): "cancelled",
        ("settled", "failed"): "failed",
        ("settled", "timedOut"): "failed",
        ("settled", "ambiguous"): "degraded",
    }
    for activity in snapshot.get("activities", [])[:MAX_PRESENTATION_ACTIVITIES]:
        outcome = activity.get("outcome")
        activities.append(
            {
                "id": _safe_identifier(activity.get("id", "activity")),
                "kind": "mcp_tool_call",
                "state": activity_state.get(
                    (activity.get("state"), outcome), "degraded"
                ),
                "summary": "MCP tool call",
                "provenance": (
                    f"{_safe_identifier(activity.get('serverId', 'server'))} · "
                    f"{_safe_identifier(activity.get('tool', 'mcp_tool'))}"
                ),
                "started_at_ms": _nonnegative_int(activity.get("startedAtMs")),
                **(
                    {"completed_at_ms": _nonnegative_int(activity.get("finishedAtMs"))}
                    if activity.get("finishedAtMs") is not None
                    else {}
                ),
                "references": [],
            }
        )

    selected = nodes[0]["id"] if nodes else None
    detail = None
    if servers and selected is not None:
        first = servers[0]
        restart = first.get("restart", {})
        body = (
            f"Lifecycle: {first.get('state', 'degraded')}\n"
            "Transport: stdio\n"
            f"Catalog revision: {_nonnegative_int(first.get('catalogRevision'))}\n"
            f"Host catalog revision: {_nonnegative_int(first.get('hostCatalogRevision'))}\n"
            f"Published tools: {_nonnegative_int(first.get('toolCount'))}\n"
            f"Restart/backoff: {_nonnegative_int(restart.get('attempt'))}/"
            f"{_nonnegative_int(restart.get('maxAttempts'))}"
        )
        last_error = first.get("lastError")
        if isinstance(last_error, Mapping):
            body += (
                "\nLast error: "
                f"{_safe_identifier(last_error.get('code', 'error'))} · "
                f"{_safe_summary(last_error.get('summary', 'MCP server degraded'))}"
            )
        detail = {
            "node_id": selected,
            "title": _safe_label(first.get("label", first.get("id", "server"))),
            "body": body,
            "references": [],
        }

    return {
        "revision": _nonnegative_int(snapshot.get("sequence")),
        "status": {
            "state": generic_state,
            "label": status_text,
            "detail": (
                f"{connected} of {configured} configured MCP servers connected; "
                f"{published} tools published"
            ),
        },
        "activities": activities,
        "collection": {
            "kind": "tree",
            "title": "MCP servers",
            "nodes": nodes,
            **({"selected_node_id": selected} if selected is not None else {}),
            **({"detail": detail} if detail is not None else {}),
        },
        "actions": actions[:64],
    }


def compact_status(snapshot: Mapping[str, Any]) -> dict[str, Any]:
    """Return safe semantic status contribution fields for the Ygg SDK."""

    summary = snapshot.get("summary", {})
    configured = _nonnegative_int(summary.get("configuredServers"))
    connected = _nonnegative_int(summary.get("connectedServers"))
    tools = _nonnegative_int(summary.get("publishedTools"))
    suffix = ""
    role = "extension.ygg_mcp.ready"
    if bool(summary.get("refreshing")):
        suffix = " · refreshing"
        role = "extension.ygg_mcp.refreshing"
    elif bool(summary.get("degraded")):
        suffix = " · degraded"
        role = "extension.ygg_mcp.degraded"
    elif configured == 0:
        role = "extension.ygg_mcp.empty"
    return {
        "surface": "status",
        "text": f"mcp {connected}/{configured} · {tools} tools{suffix}",
        "style_role": role,
        "priority": 15,
    }


def format_status(snapshot: Mapping[str, Any]) -> str:
    """Bounded narrow/headless text fallback; never includes launch data."""

    compact = compact_status(snapshot)["text"]
    lines = [compact]
    for server in snapshot.get("servers", [])[:MAX_PRESENTATION_SERVERS]:
        state = server.get("state", "degraded")
        revision = _nonnegative_int(server.get("catalogRevision"))
        tools = _nonnegative_int(server.get("toolCount"))
        lines.append(
            f"- {_safe_identifier(server.get('id', 'server'))}: {state} · "
            f"{tools} tools · catalog {revision}"
        )
    return "\n".join(lines)


def format_server_detail(snapshot: Mapping[str, Any], server_id: str) -> str:
    for server in snapshot.get("servers", [])[:MAX_PRESENTATION_SERVERS]:
        if server.get("id") != server_id:
            continue
        restart = server.get("restart", {})
        lines = [
            f"{server['id']} · {server['state']} · {server['transport']}",
            f"catalog {server['catalogRevision']} · host catalog "
            f"{server['hostCatalogRevision']} · {server['toolCount']} tools",
            f"restart {restart.get('attempt', 0)}/{restart.get('maxAttempts', 0)}",
        ]
        last_error = server.get("lastError")
        if isinstance(last_error, Mapping):
            lines.append(
                f"last error: {_safe_identifier(last_error.get('code', 'error'))} · "
                f"{_safe_summary(last_error.get('summary', 'MCP server degraded'))}"
            )
        actions = [
            action["id"]
            for action in server.get("actions", [])
            if isinstance(action, Mapping) and action.get("enabled") is True
        ]
        lines.append("actions: " + (", ".join(actions) if actions else "none"))
        for tool in server.get("tools", [])[:MAX_PRESENTATION_TOOLS]:
            summary = tool.get("schemaSummary", {})
            lines.append(
                f"  - {tool.get('name', 'mcp_tool')} · {tool.get('approval', 'unknown')} · "
                f"{_nonnegative_int(summary.get('propertyCount'))} parameters"
            )
        return "\n".join(lines)
    return f"Unknown MCP server: {_safe_identifier(server_id)}"


def snapshot_json(snapshot: Mapping[str, Any]) -> str:
    """Stable structured fallback for `/mcp snapshot` and host adapters."""

    return json.dumps(snapshot, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def _normalize_server(value: Mapping[str, Any]) -> dict[str, Any]:
    state = value.get("state")
    if state not in _ALLOWED_STATES:
        state = "degraded"
    tools = []
    for tool in value.get("tools", [])[:MAX_PRESENTATION_TOOLS]:
        if not isinstance(tool, Mapping):
            continue
        summary = tool.get("schemaSummary", {})
        tools.append(
            {
                "id": _safe_identifier(tool.get("id", tool.get("name", "mcp_tool"))),
                "name": _safe_identifier(tool.get("name", "mcp_tool")),
                "schemaSummary": {
                    "rootType": _safe_identifier(summary.get("rootType", "object")),
                    "propertyCount": _nonnegative_int(summary.get("propertyCount")),
                    "requiredCount": _nonnegative_int(summary.get("requiredCount")),
                    "additionalProperties": bool(summary.get("additionalProperties", True)),
                },
                "approval": (
                    tool.get("approval")
                    if tool.get("approval") in {"readOnly", "unknown", "destructive"}
                    else "unknown"
                ),
            }
        )
    actions = []
    for action_id in ("refresh", "restart", "stop"):
        enabled = any(
            isinstance(action, Mapping)
            and action.get("id") == action_id
            and action.get("enabled") is True
            for action in value.get("actions", [])
        )
        actions.append({"id": action_id, "enabled": enabled})
    restart = value.get("restart", {})
    result: dict[str, Any] = {
        "id": _safe_identifier(value.get("id", "server")),
        "label": _safe_label(value.get("label", value.get("id", "server"))),
        "state": state,
        "transport": "stdio",
        "connected": bool(value.get("connected", False)),
        "required": bool(value.get("required", False)),
        "scope": value.get("scope") if value.get("scope") in {"user", "project"} else "user",
        "catalogRevision": _nonnegative_int(value.get("catalogRevision")),
        "hostCatalogRevision": _nonnegative_int(value.get("hostCatalogRevision")),
        "toolCount": len(tools),
        "restart": {
            "attempt": _nonnegative_int(restart.get("attempt")),
            "maxAttempts": _nonnegative_int(restart.get("maxAttempts")),
            "nextRetryAtMs": (
                _nonnegative_int(restart.get("nextRetryAtMs"))
                if restart.get("nextRetryAtMs") is not None
                else None
            ),
        },
        "actions": actions,
        "tools": sorted(tools, key=lambda item: item["name"]),
    }
    last_error = value.get("lastError")
    if isinstance(last_error, Mapping):
        result["lastError"] = {
            "code": _safe_identifier(last_error.get("code", "error")),
            "summary": _safe_summary(last_error.get("summary", "MCP server degraded")),
            "atMs": _nonnegative_int(last_error.get("atMs")),
        }
    return result


def _safe_identifier(value: Any) -> str:
    text = str(value)
    cleaned = "".join(
        character if character.isascii() and (character.isalnum() or character in "._-") else "_"
        for character in text
    )
    cleaned = cleaned.strip(".")[:128]
    return cleaned or "unknown"


def _safe_label(value: Any) -> str:
    text = str(value)
    text = "".join(
        character if character in "\t" or ord(character) >= 32 else "�" for character in text
    )
    encoded = text.encode("utf-8")
    if len(encoded) <= 128:
        return text
    encoded = encoded[:128]
    while encoded:
        try:
            return encoded.decode("utf-8")
        except UnicodeDecodeError:
            encoded = encoded[:-1]
    return "server"


def _safe_summary(value: Any) -> str:
    text = str(value)
    text = "".join(
        character if character in "\n\t" or ord(character) >= 32 else "�"
        for character in text
    )
    encoded = text.encode("utf-8")
    if len(encoded) <= MAX_SAFE_SUMMARY_BYTES:
        return text
    encoded = encoded[:MAX_SAFE_SUMMARY_BYTES]
    while encoded:
        try:
            return encoded.decode("utf-8")
        except UnicodeDecodeError:
            encoded = encoded[:-1]
    return "MCP server degraded"


def _nonnegative_int(value: Any) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        return 0
    return max(0, value)
