"""Generic API 0.2 presentation projection for SSH state."""

from __future__ import annotations

from typing import Any, Mapping, Sequence

from .config import Target


MAX_SAFE_LABEL_BYTES = 512
MAX_SAFE_DETAIL_BYTES = 16 * 1024


def build_presentation(
    *,
    revision: int,
    targets: Sequence[Target],
    connections: Sequence[Mapping[str, Any]],
    activities: Sequence[Mapping[str, Any]],
    config_source: str,
    configuration_error: Any = None,
) -> dict[str, Any]:
    """Build one complete, side-effect-free generic presentation snapshot."""

    enabled = sorted((target for target in targets if target.enabled), key=lambda item: item.id)
    ordered_connections = sorted(
        connections,
        key=lambda item: (
            _safe_id(item.get("targetId", "target")),
            _nonnegative_int(item.get("generation")),
            _safe_id(item.get("owner", "owner")),
        ),
    )[:128]
    ready = [item for item in ordered_connections if item.get("state") == "ready"]
    degraded = [item for item in ordered_connections if item.get("state") == "degraded"]
    connecting = [item for item in ordered_connections if item.get("state") == "connecting"]

    if configuration_error:
        status_state = "degraded"
        status_label = "ssh configuration degraded"
        status_detail = _safe_label(configuration_error)
    elif not enabled:
        status_state = "empty"
        status_label = "ssh not configured"
        status_detail = "No explicit OpenSSH aliases are configured; the package is inert."
    elif connecting:
        status_state = "loading"
        status_label = "ssh connecting · remote authority"
        status_detail = "Replay-safe OpenSSH connection setup is in progress."
    elif degraded:
        status_state = "degraded"
        status_label = f"ssh {len(ready)} ready · {len(degraded)} degraded · remote authority"
        status_detail = "A degraded connection requires an explicit retry; mutations are never replayed."
    elif len(ready) == 1:
        item = ready[0]
        status_state = "active"
        status_label = _safe_label(
            f"ssh {item.get('alias', 'configured')} · {item.get('authority', 'read-only')} · "
            f"{item.get('remoteCwd', '/')} · gen {_nonnegative_int(item.get('generation'))} · ready"
        )
        status_detail = "Authenticated existing OpenSSH session; all remote output remains untrusted."
    elif ready:
        status_state = "active"
        status_label = f"ssh {len(ready)} ready · remote authority"
        status_detail = "Multiple owner-fenced authenticated OpenSSH sessions are ready."
    else:
        status_state = "stopped"
        status_label = f"ssh disconnected · {len(enabled)} configured"
        status_detail = "Choose an explicit configured target through /ssh connect."

    nodes: list[dict[str, Any]] = []
    actions: list[dict[str, Any]] = []
    connection_by_target: dict[str, list[Mapping[str, Any]]] = {}
    for item in ordered_connections:
        connection_by_target.setdefault(str(item.get("targetId")), []).append(item)

    for target in enabled[:32]:
        related = connection_by_target.get(target.id, [])
        target_state = _target_state(related)
        action_ids: list[str] = []
        degraded_target = any(item.get("state") == "degraded" for item in related)
        active_target = any(item.get("state") in {"ready", "connecting"} for item in related)
        if degraded_target:
            for action_name, label in (("retry", "Retry connection"), ("disconnect", "Disconnect")):
                action_ids.append(f"{action_name}:{target.id}")
                actions.append(
                    {
                        "id": f"{action_name}:{target.id}",
                        "label": label,
                        "command": "ssh",
                        "arguments": [action_name, target.id],
                        "destructive": False,
                    }
                )
        elif active_target:
            action_ids.append(f"disconnect:{target.id}")
            actions.append(
                {
                    "id": f"disconnect:{target.id}",
                    "label": "Disconnect",
                    "command": "ssh",
                    "arguments": ["disconnect", target.id],
                    "destructive": False,
                }
            )
        else:
            action_ids.append(f"connect:{target.id}")
            actions.append(
                {
                    "id": f"connect:{target.id}",
                    "label": "Connect",
                    "command": "ssh",
                    "arguments": ["connect", target.id],
                    "destructive": False,
                }
            )
        nodes.append(
            {
                "id": f"target:{target.id}",
                "state": target_state,
                "label": _safe_label(target.label),
                "secondary": _safe_label(
                    f"{target.alias} · {target.authority} · {target.remote_cwd}"
                ),
                "action_ids": action_ids,
                "references": [],
            }
        )
        for item in related[:8]:
            owner = _safe_id(item.get("owner", "owner"))
            state = _generic_connection_state(item.get("state"))
            secondary = (
                f"owner {owner} · gen {_nonnegative_int(item.get('generation'))} · "
                f"{'ambiguous' if item.get('ambiguous') else state}"
            )
            nodes.append(
                {
                    "id": f"connection:{target.id}:{owner}",
                    "parent_id": f"target:{target.id}",
                    "state": "degraded" if item.get("ambiguous") else state,
                    "label": f"session {owner}",
                    "secondary": _safe_label(secondary),
                    "action_ids": [],
                    "references": [],
                }
            )

    selected_node_id = nodes[0]["id"] if nodes else None
    detail = None
    if enabled and selected_node_id is not None:
        target = enabled[0]
        related = connection_by_target.get(target.id, [])
        current = max(
            related,
            key=lambda item: _nonnegative_int(item.get("generation")),
            default=None,
        )
        body = [
            f"Configured alias: {target.alias}",
            f"Authority: {target.authority}",
            f"Remote cwd: {target.remote_cwd}",
            f"Configuration scope: {target.scope}",
            f"Configuration state: {config_source}",
        ]
        if current is None:
            body.extend(["State: disconnected", "Connection generation: none"])
        else:
            body.extend(
                [
                    f"Owner session: {_safe_id(current.get('owner', 'owner'))}",
                    f"Owner fence: {_safe_id(current.get('ownerFence', 'fence'))}",
                    f"State: {_safe_state(current.get('state'))}",
                    f"Connection generation: {_nonnegative_int(current.get('generation'))}",
                    f"Ambiguous mutation: {'yes' if current.get('ambiguous') else 'no'}",
                    f"Last error: {_safe_detail(current.get('lastError') or 'none')}",
                ]
            )
        body.extend(
            [
                "Authentication: existing OpenSSH config/agent/key selection (no credential prompts)",
                "Safety: remote paths and output are bounded untrusted data; no filesystem sandbox is claimed.",
            ]
        )
        detail = {
            "node_id": selected_node_id,
            "title": _safe_label(f"SSH target · {target.label}"),
            "body": _safe_detail("\n".join(body)),
            "references": [],
        }

    generic_activities = []
    for activity in activities[-128:]:
        state = _activity_state(activity)
        command_class = (
            activity.get("commandClass")
            if activity.get("commandClass") in {"read", "mutation", "connection_setup"}
            else "read"
        )
        alias = _safe_label(activity.get("alias", "configured"))
        generation = _nonnegative_int(activity.get("connectionGeneration"))
        summary = "Remote read" if command_class == "read" else "Remote mutation"
        item: dict[str, Any] = {
            "id": _safe_id(activity.get("id", "ssh-activity")),
            "kind": "ssh_remote_operation",
            "state": state,
            "summary": summary,
            "provenance": _safe_label(
                f"remote · {alias} · {command_class} · generation {generation}"
            ),
            "started_at_ms": _nonnegative_int(activity.get("startedAtMs")),
            "references": [],
        }
        if activity.get("completedAtMs") is not None:
            item["completed_at_ms"] = _nonnegative_int(activity.get("completedAtMs"))
        generic_activities.append(item)

    return {
        "revision": max(0, int(revision)),
        "status": {
            "state": status_state,
            "label": _safe_label(status_label),
            "detail": _safe_label(status_detail),
        },
        "activities": generic_activities,
        "collection": {
            "kind": "tree",
            "title": "Configured SSH targets",
            "nodes": nodes[:256],
            **({"selected_node_id": selected_node_id} if selected_node_id else {}),
            **({"detail": detail} if detail else {}),
        },
        "actions": actions[:64],
    }


def _target_state(connections: Sequence[Mapping[str, Any]]) -> str:
    if any(item.get("state") == "degraded" for item in connections):
        return "degraded"
    if any(item.get("state") == "connecting" for item in connections):
        return "loading"
    if any(item.get("state") == "ready" for item in connections):
        return "active"
    if any(item.get("state") == "stopped" for item in connections):
        return "stopped"
    return "empty"


def _generic_connection_state(value: Any) -> str:
    return {
        "connecting": "loading",
        "ready": "active",
        "degraded": "degraded",
        "stopped": "stopped",
    }.get(value, "unavailable")


def _activity_state(value: Mapping[str, Any]) -> str:
    if value.get("state") == "running":
        return "running"
    return {
        "succeeded": "succeeded",
        "failed": "failed",
        "cancelled": "cancelled",
        "ambiguous": "degraded",
    }.get(value.get("outcome"), "degraded")


def _safe_id(value: Any) -> str:
    text = str(value)
    cleaned = "".join(
        character
        if character.isascii() and (character.isalnum() or character in "._-:/")
        else "_"
        for character in text
    )
    return cleaned[:128] or "unknown"


def _safe_state(value: Any) -> str:
    return value if value in {"connecting", "ready", "degraded", "stopped"} else "unknown"


def _safe_label(value: Any) -> str:
    return _truncate_clean(str(value), MAX_SAFE_LABEL_BYTES, allow_newline=False)


def _safe_detail(value: Any) -> str:
    return _truncate_clean(str(value), MAX_SAFE_DETAIL_BYTES, allow_newline=True)


def _truncate_clean(value: str, limit: int, *, allow_newline: bool) -> str:
    allowed_controls = "\n\r\t" if allow_newline else ""
    value = "".join(
        character
        if character in allowed_controls
        or (
            ord(character) >= 32
            and not 127 <= ord(character) <= 159
            and character != "\x1b"
        )
        else "�"
        for character in value
    )
    encoded = value.encode("utf-8")
    if len(encoded) <= limit:
        return value or "unknown"
    encoded = encoded[:limit]
    while encoded:
        try:
            return encoded.decode("utf-8")
        except UnicodeDecodeError:
            encoded = encoded[:-1]
    return "unknown"


def _nonnegative_int(value: Any) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        return 0
    return max(0, value)
