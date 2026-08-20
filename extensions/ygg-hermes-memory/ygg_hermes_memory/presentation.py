"""Generic API 0.2 memory presentation and bounded headless fallbacks."""

from __future__ import annotations

import json
from typing import Any, List, Mapping, Optional

from .constants import MAX_ACTIVITIES, MAX_PRESENTATION_NODES
from .safety import safe_detail, safe_identifier, safe_label


def build_presentation(
    *,
    revision: int,
    discovery: Mapping[str, Any],
    owner: Mapping[str, Any],
) -> dict:
    """Build one complete frontend-neutral provider picker/activity snapshot."""

    selected_id = owner.get("selectedId")
    inspected_id = owner.get("inspectedId") or selected_id or "off"
    owner_state = owner.get("state", "off")
    providers = [item for item in discovery.get("providers", []) if isinstance(item, Mapping)]
    providers = providers[: max(0, MAX_PRESENTATION_NODES - 1)]

    if selected_id is None and discovery.get("environmentState") == "unavailable":
        status_state = "unavailable"
        status_label = "memory unavailable"
    elif selected_id is None:
        status_state = "empty"
        status_label = "memory off"
    elif owner_state in {"loading", "selecting"}:
        status_state = "loading"
        status_label = "memory loading"
    elif owner_state == "syncing":
        status_state = "running"
        status_label = f"memory {safe_label(owner.get('providerLabel', 'provider'))} · syncing"
    elif owner_state == "active":
        status_state = "active"
        status_label = f"memory {safe_label(owner.get('providerLabel', 'provider'))}"
    elif owner_state == "unavailable":
        status_state = "unavailable"
        status_label = f"memory {safe_label(owner.get('providerLabel', 'provider'))} · unavailable"
    elif owner_state in {"degraded", "stopping"}:
        status_state = "degraded"
        status_label = f"memory {safe_label(owner.get('providerLabel', 'provider'))} · degraded"
    else:
        status_state = "stopped"
        status_label = "memory off"

    queue_depth = _integer(owner.get("queueDepth"))
    last_prefetch = owner.get("lastPrefetch") if isinstance(owner.get("lastPrefetch"), Mapping) else {}
    last_sync = owner.get("lastSync") if isinstance(owner.get("lastSync"), Mapping) else {}
    status_detail = (
        f"queue {queue_depth}; prefetch {safe_identifier(last_prefetch.get('outcome', 'none'))}; "
        f"sync {safe_identifier(last_sync.get('outcome', 'none'))}; "
        f"contract {safe_label(discovery.get('contractVersion', 'unknown'), maximum=80)}"
    )

    nodes: List[dict] = []
    actions: List[dict] = []
    off_actions = []
    if selected_id is not None:
        off_actions.append("disable")
        actions.append(
            {
                "id": "disable",
                "label": "Turn memory off",
                "command": "memory",
                "arguments": ["off"],
                "destructive": False,
            }
        )
    nodes.append(
        {
            "id": "off",
            "state": "active" if selected_id is None else "stopped",
            "label": "Off",
            "secondary": "No provider context, tools, or writes",
            "action_ids": off_actions,
            "references": [],
        }
    )

    detail_provider = None
    for provider in providers:
        candidate_id = safe_identifier(provider.get("id"), fallback="provider", maximum=96)
        is_selected = candidate_id == selected_id
        trusted = provider.get("trusted") is True
        availability = provider.get("availability")
        if is_selected:
            node_state = status_state
        elif availability == "unavailable":
            node_state = "unavailable"
        elif not trusted:
            node_state = "pending"
        else:
            node_state = "stopped"
        action_ids = []
        if is_selected:
            action_id = f"reload:{candidate_id}"
            action_ids.append(action_id)
            actions.append(
                {
                    "id": action_id,
                    "label": "Reload provider",
                    "command": "memory",
                    "arguments": ["reload"],
                    "destructive": False,
                }
            )
            if owner_state in {"degraded", "unavailable"}:
                retry_id = f"retry:{candidate_id}"
                action_ids.append(retry_id)
                actions.append(
                    {
                        "id": retry_id,
                        "label": "Retry provider",
                        "command": "memory",
                        "arguments": ["retry"],
                        "destructive": False,
                    }
                )
        elif availability != "unavailable" and trusted:
            action_id = f"select:{candidate_id}"
            action_ids.append(action_id)
            actions.append(
                {
                    "id": action_id,
                    "label": "Select provider",
                    "command": "memory",
                    "arguments": ["select", candidate_id],
                    "destructive": False,
                }
            )
        elif availability != "unavailable" and provider.get("fingerprint"):
            action_id = f"trust:{candidate_id}"
            action_ids.append(action_id)
            actions.append(
                {
                    "id": action_id,
                    "label": "Trust provider for this run",
                    "command": "memory",
                    "arguments": ["trust", candidate_id, str(provider["fingerprint"])],
                    "destructive": True,
                }
            )
        secondary = (
            f"{safe_label(provider.get('version', 'unknown'), maximum=64)} · "
            f"{safe_identifier(provider.get('source', 'provider'))} · "
            f"network {safe_identifier(provider.get('network', 'unknown'))} · "
            f"storage {safe_identifier(provider.get('storage', 'unknown'))} · "
            f"{'trusted' if trusted else 'not trusted'}"
        )
        node = {
            "id": candidate_id,
            "state": node_state,
            "label": safe_label(provider.get("label", provider.get("name", "provider"))),
            "secondary": secondary,
            "action_ids": action_ids[:4],
            "references": [],
        }
        nodes.append(node)
        if candidate_id == inspected_id:
            detail_provider = provider

    selected_node = inspected_id if any(node["id"] == inspected_id for node in nodes) else "off"
    detail = _provider_detail(detail_provider, owner, discovery) if detail_provider else _off_detail(discovery)
    detail["node_id"] = selected_node

    activities = []
    for activity in owner.get("activities", [])[-MAX_ACTIVITIES:]:
        if not isinstance(activity, Mapping):
            continue
        state = activity.get("state")
        if state not in {
            "pending",
            "running",
            "succeeded",
            "failed",
            "cancelled",
            "degraded",
            "unavailable",
            "stopped",
            "active",
        }:
            state = "degraded"
        item = {
            "id": safe_identifier(activity.get("id"), fallback="memory-activity", maximum=96),
            "kind": safe_identifier(activity.get("kind"), fallback="memory_activity", maximum=64),
            "state": state,
            "summary": safe_label(activity.get("summary", "Memory activity"), maximum=512),
            "provenance": safe_label(activity.get("provenance", "provider"), maximum=1024),
            "started_at_ms": _integer(activity.get("startedAtMs")),
            "references": [],
        }
        completed = activity.get("completedAtMs")
        if completed is not None:
            item["completed_at_ms"] = _integer(completed)
        owner_reference = activity.get("ownerReference")
        if owner_reference:
            item["references"] = [
                {
                    "kind": "resource",
                    "id": safe_identifier(owner_reference, fallback="memory-owner", maximum=128),
                    "label": "Ygg memory owner",
                }
            ]
        activities.append(item)

    retained_actions = actions[:64]
    retained_action_ids = {action["id"] for action in retained_actions}
    for node in nodes:
        node["action_ids"] = [
            action_id for action_id in node.get("action_ids", []) if action_id in retained_action_ids
        ]
    return {
        "revision": max(0, int(revision)),
        "status": {
            "state": status_state,
            "label": safe_label(status_label, maximum=256),
            "detail": safe_label(status_detail, maximum=1024),
        },
        "activities": activities,
        "collection": {
            "kind": "list",
            "title": "Memory providers",
            "nodes": nodes,
            "selected_node_id": selected_node,
            "detail": detail,
        },
        "actions": actions[:64],
    }


def format_picker(discovery: Mapping[str, Any], owner: Mapping[str, Any]) -> str:
    """Narrow `/memory` fallback with no provider contents or backend paths."""

    selected = owner.get("selectedId")
    status = compact_status(owner)
    if selected is None and discovery.get("environmentState") == "unavailable":
        status = "memory unavailable"
    lines = [status, "Providers:", f"- off: {'selected' if selected is None else 'available'}"]
    for provider in discovery.get("providers", [])[:MAX_PRESENTATION_NODES - 1]:
        if not isinstance(provider, Mapping):
            continue
        candidate_id = safe_identifier(provider.get("id"), fallback="provider", maximum=96)
        trust = "trusted" if provider.get("trusted") is True else "not trusted"
        marker = "selected" if candidate_id == selected else provider.get("availability", "discoverable")
        lines.append(
            f"- {candidate_id}: {safe_label(provider.get('label', candidate_id))} · "
            f"{safe_label(provider.get('version', 'unknown'), maximum=64)} · {marker} · {trust} · "
            f"network {safe_identifier(provider.get('network', 'unknown'))} · "
            f"storage {safe_identifier(provider.get('storage', 'unknown'))}"
        )
    lines.append("Use /memory show ID, /memory trust ID FINGERPRINT, /memory select ID, or /memory off.")
    return "\n".join(lines)


def format_detail(provider: Optional[Mapping[str, Any]], owner: Mapping[str, Any], discovery: Mapping[str, Any]) -> str:
    detail = _provider_detail(provider, owner, discovery) if provider else _off_detail(discovery)
    return f"{detail['title']}\n{detail['body']}"


def compact_status(owner: Mapping[str, Any]) -> str:
    selected = owner.get("selectedId")
    if selected is None:
        return "memory off"
    label = safe_label(owner.get("providerLabel", selected), maximum=128)
    state = safe_identifier(owner.get("state", "degraded"))
    queue = _integer(owner.get("queueDepth"))
    suffix = f" · queue {queue}" if queue else ""
    return f"memory {label} · {state}{suffix}"


def snapshot_json(snapshot: Mapping[str, Any]) -> str:
    return json.dumps(snapshot, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def _provider_detail(
    provider: Mapping[str, Any],
    owner: Mapping[str, Any],
    discovery: Mapping[str, Any],
) -> dict:
    candidate_id = safe_identifier(provider.get("id"), fallback="provider", maximum=96)
    selected = candidate_id == owner.get("selectedId")
    lines = [
        f"Provider id: {candidate_id}",
        f"Provider version: {safe_label(provider.get('version', 'unknown'), maximum=64)}",
        f"Hermes contract: {safe_label(provider.get('contract', discovery.get('contractVersion', 'unknown')), maximum=160)}",
        f"Environment: {safe_label(provider.get('environment', discovery.get('environment', 'unknown')), maximum=128)}",
        f"Source: {safe_identifier(provider.get('source', 'provider'))}",
        f"Availability: {safe_identifier(provider.get('availability', 'unknown'))}",
        f"Trust: {'trusted' if provider.get('trusted') is True else 'not trusted'}",
        f"Fingerprint: {safe_identifier(provider.get('fingerprint', 'unavailable'), fallback='unavailable', maximum=96)}",
        f"Setup: {safe_identifier(provider.get('setup', 'unknown'))}",
        f"Network behavior: {safe_identifier(provider.get('network', 'unknown'))}",
        f"Storage behavior: {safe_identifier(provider.get('storage', 'unknown'))}",
        (
            "Context labels: "
            f"hermes-memory.{safe_identifier(provider.get('name', 'provider'))}.system, "
            f"hermes-memory.{safe_identifier(provider.get('name', 'provider'))}.prefetch"
        ),
        (
            "Declared read tools: "
            + (", ".join(safe_identifier(item, fallback="tool") for item in provider.get("readTools", [])[:32]) or "none")
        ),
        (
            "Declared write tools: "
            + (", ".join(safe_identifier(item, fallback="tool") for item in provider.get("writeTools", [])[:32]) or "none")
        ),
    ]
    if selected:
        lines.extend(
            [
                f"Runtime state: {safe_identifier(owner.get('state', 'degraded'))}",
                f"Queue depth: {_integer(owner.get('queueDepth'))}",
                f"Tool count: {_integer(owner.get('toolCount'))}",
                f"Context limit: {_integer(owner.get('contextByteLimit'))} bytes",
                f"Cache: prompt-epoch frozen; last {safe_identifier((owner.get('lastPrefetch') or {}).get('cache', 'none'))}",
                f"Last prefetch: {safe_identifier((owner.get('lastPrefetch') or {}).get('outcome', 'none'))}",
                f"Last sync: {safe_identifier((owner.get('lastSync') or {}).get('outcome', 'none'))}",
            ]
        )
        error_code = owner.get("lastErrorCode")
        if error_code:
            lines.append(f"Last error: {safe_identifier(error_code, fallback='provider_error')}")
        setup_hint = owner.get("setupHint")
        if setup_hint:
            lines.append(
                f"Setup hint (untrusted provider text): {safe_detail(setup_hint, maximum=512)}"
            )
        hooks = owner.get("optionalHooks", [])
        unsupported = owner.get("unsupportedHooks", [])
        lines.append(
            "Mapped optional hooks: "
            + (", ".join(safe_identifier(item, fallback="hook") for item in hooks[:16]) or "none")
        )
        lines.append(
            "Unsupported/no equivalent: "
            + (", ".join(safe_identifier(item, fallback="hook") for item in unsupported[:16]) or "none")
        )
        tools = owner.get("tools", [])
        if tools:
            lines.append("Provider tools: " + ", ".join(safe_identifier(item, fallback="tool") for item in tools[:32]))
        measurements = owner.get("measurements") if isinstance(owner.get("measurements"), Mapping) else {}
        lines.append(
            f"Process measurements: CPU {_number(measurements.get('cpuSeconds'))}s; "
            f"RSS {_integer(measurements.get('rssKiB'))} KiB"
        )
    return {
        "title": safe_label(provider.get("label", provider.get("name", "Memory provider"))),
        "body": safe_detail("\n".join(lines)),
        "references": [],
    }


def _off_detail(discovery: Mapping[str, Any]) -> dict:
    body = (
        "Memory is off for this Ygg resource owner.\n"
        f"Environment: {safe_label(discovery.get('environment', 'not-configured'), maximum=128)}\n"
        f"Environment state: {safe_identifier(discovery.get('environmentState', 'off'))}\n"
        f"Hermes contract: {safe_label(discovery.get('contractVersion', 'unknown'), maximum=160)}\n"
        "Discovery is metadata-only. Selecting a trusted provider is the first import/initialization boundary."
    )
    return {"title": "Memory off", "body": safe_detail(body), "references": []}


def _integer(value: Any) -> int:
    return value if isinstance(value, int) and not isinstance(value, bool) and value >= 0 else 0


def _number(value: Any) -> str:
    if isinstance(value, (int, float)) and not isinstance(value, bool) and value >= 0:
        return f"{value:.3f}"
    return "0.000"
