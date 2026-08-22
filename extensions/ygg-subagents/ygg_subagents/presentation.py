"""Frontend-neutral worker tree, activity, detail, and headless formatting."""

from __future__ import annotations

import hashlib
import re
from typing import Any, Dict, List, Optional, Sequence, Tuple

from .model import Worker, bounded_text, safe_label


_ID_RE = re.compile(r"^[A-Za-z0-9_.:/-]+$")
GENERIC_STATE = {
    "queued": "pending",
    "running": "running",
    "waiting": "running",
    "stopping": "cancelled",
    "done": "succeeded",
    "failed": "failed",
    "stopped": "stopped",
    "timed_out": "failed",
    "cancelled": "cancelled",
    "orphaned": "unavailable",
    "restarted": "degraded",
}
STATE_LABEL = {
    "queued": "queued",
    "running": "running",
    "waiting": "waiting",
    "stopping": "stopping",
    "done": "done",
    "failed": "failed",
    "stopped": "stopped",
    "timed_out": "timed out",
    "cancelled": "cancelled",
    "orphaned": "orphaned",
    "restarted": "restarted",
}


def semantic_id(prefix: str, value: str) -> str:
    if value and len(value.encode("utf-8")) <= 900 and _ID_RE.fullmatch(value):
        return "%s:%s" % (prefix, value)
    digest = hashlib.sha256(value.encode("utf-8", errors="replace")).hexdigest()[:24]
    return "%s:%s" % (prefix, digest)


def duration_label(elapsed_ms: int) -> str:
    total_seconds = max(0, elapsed_ms // 1000)
    hours, remainder = divmod(total_seconds, 3600)
    minutes, seconds = divmod(remainder, 60)
    if hours:
        return "%d:%02d:%02d" % (hours, minutes, seconds)
    return "%02d:%02d" % (minutes, seconds)


def cost_label(microdollars: Optional[int]) -> str:
    if microdollars is None:
        return "?"
    return "$%.4f" % (microdollars / 1_000_000.0)


def counts(workers: Sequence[Worker]) -> Dict[str, int]:
    values = {"queued": 0, "running": 0, "done": 0, "failed": 0, "stopped": 0}
    for worker in workers:
        if worker.state == "queued":
            values["queued"] += 1
        elif worker.state in {"running", "waiting", "stopping"}:
            values["running"] += 1
        elif worker.state == "done":
            values["done"] += 1
        elif worker.state == "failed" or worker.state == "timed_out":
            values["failed"] += 1
        else:
            values["stopped"] += 1
    return values


def compact_status(workers: Sequence[Worker]) -> Tuple[str, str, Optional[str]]:
    value = counts(workers)
    pieces = []
    for key in ("running", "queued", "done", "failed", "stopped"):
        if value[key]:
            pieces.append("%d %s" % (value[key], key))
    label = "Subagents" if not pieces else "Subagents · " + " · ".join(pieces)
    if value["failed"]:
        state = "degraded"
        detail = "One or more bounded workers failed or timed out."
    elif value["running"] or value["queued"]:
        state = "active"
        detail = None
    elif workers:
        state = "active"
        detail = None
    else:
        state = "empty"
        detail = "No workers have been observed for this parent session."
    return state, bounded_text(label, 1024), detail


def safe_reference(kind: str, identifier: Optional[str], label: str) -> Optional[Dict[str, Any]]:
    if not isinstance(identifier, str) or not identifier.strip():
        return None
    if len(identifier.encode("utf-8")) > 1024 or "\x1b" in identifier:
        return None
    if any((ord(character) < 32 or 127 <= ord(character) <= 159) for character in identifier):
        return None
    return {"kind": kind, "id": identifier, "label": safe_label(label)}


def worker_references(worker: Worker) -> List[Dict[str, Any]]:
    references: List[Dict[str, Any]] = []
    session = safe_reference("session", worker.session, "Open worker transcript")
    if session is not None:
        references.append(session)
    for artifact in worker.artifacts:
        reference = safe_reference(
            "artifact", artifact.identifier, artifact.label or "Worker artifact"
        )
        if reference is not None:
            references.append(reference)
        if len(references) >= 8:
            break
    return references


def latest_action(worker: Worker) -> Optional[str]:
    """Bounded single-line description of the worker's most recent tool call."""
    if not worker.recent_tools:
        return None
    entry = worker.recent_tools[-1]
    args = entry.get("args") or ""
    action = "%s %s" % (entry["name"], args) if args else str(entry["name"])
    if entry.get("finished_at_ms") is None:
        return "* %s" % action
    if entry.get("error"):
        return "! %s" % action
    return action


def worker_secondary(worker: Worker, now_ms: int) -> str:
    state = STATE_LABEL.get(worker.state, safe_label(worker.state))
    if worker.max_turns is None:
        turns = "%s turns, no ceiling" % (
            worker.turn_count if worker.turn_count is not None else "?"
        )
    else:
        turns = "?/%d turns" % worker.max_turns
        if worker.turn_count is not None:
            turns = "%d/%d turns" % (worker.turn_count, worker.max_turns)
    if worker.max_tokens is None:
        tokens = "%s tok · inherited no ceiling" % (
            worker.tokens_used if worker.tokens_used is not None else "?"
        )
    else:
        tokens = "?/%d tok" % worker.max_tokens
        if worker.tokens_used is not None:
            tokens = "%d/%d tok" % (worker.tokens_used, worker.max_tokens)
    if worker.max_cost_microdollars is None:
        cost = "%s / no ceiling" % cost_label(worker.cost_microdollars)
    else:
        cost = "%s/%s" % (
            cost_label(worker.cost_microdollars),
            cost_label(worker.max_cost_microdollars),
        )
    restart = " · restarted" if worker.recovered else ""
    tool_calls = "%d tool call%s" % (
        worker.tool_call_count,
        "" if worker.tool_call_count == 1 else "s",
    )
    # The latest host-observed tool call (with its bounded argument summary)
    # replaces the bare phase token so the picker row answers "what is it
    # doing" without opening the transcript.
    action = latest_action(worker)
    focus = action if action is not None else safe_label(
        worker.current_tool or worker.phase or state
    )
    return bounded_text(
        "%s · %s · %s · %s/%s · %s · %s · %s · %s%s"
        % (
            state,
            duration_label(worker.elapsed_ms(now_ms)),
            focus,
            worker.profile,
            worker.effective_model,
            tool_calls,
            turns,
            tokens,
            cost,
            restart,
        ),
        1024,
    )


def detail_body(worker: Worker, now_ms: int) -> str:
    tools = ", ".join(worker.tools)
    token_use = str(worker.tokens_used) if worker.tokens_used is not None else "not exposed"
    token_limit = (
        str(worker.max_tokens)
        if worker.max_tokens is not None
        else "inherited parent setting (no session ceiling)"
    )
    lines = [
        "State: %s" % STATE_LABEL.get(worker.state, worker.state),
        "Worker: %s (%s)" % (worker.name, worker.agent_id),
        "Parentage: parent > %s; depth %d (maximum 1)" % (worker.name, worker.depth),
        "Elapsed: %s" % duration_label(worker.elapsed_ms(now_ms)),
        "Model/profile: %s (inherited) / %s" % (worker.effective_model, worker.profile),
        "Current phase/tool: %s" % safe_label(worker.current_tool or worker.phase),
        "Requested tool policy: %s"
        % (
            "read-only [%s]" % tools
            if worker.read_only
            else "granted mutation scope [%s]" % tools
        ),
        "Turn use: %s / %s"
        % (
            worker.turn_count if worker.turn_count is not None else "not exposed",
            "unlimited" if worker.max_turns is None else worker.max_turns,
        ),
        "Tool calls: %d" % worker.tool_call_count,
        "Token use: %s / %s" % (token_use, token_limit),
        "Token buckets: input %s + cache read %s + cache write %s; output %s (reasoning %s)."
        % (
            worker.input_tokens if worker.input_tokens is not None else "not exposed",
            worker.cache_read_tokens if worker.cache_read_tokens is not None else "not exposed",
            worker.cache_write_tokens if worker.cache_write_tokens is not None else "not exposed",
            worker.output_tokens if worker.output_tokens is not None else "not exposed",
            worker.reasoning_tokens if worker.reasoning_tokens is not None else "not exposed",
        ),
        "Cost use: %s / %s microdollars"
        % (
            worker.cost_microdollars
            if worker.cost_microdollars is not None
            else "not exposed",
            "unlimited"
            if worker.max_cost_microdollars is None
            else worker.max_cost_microdollars,
        ),
        "Wall deadline: %s"
        % (
            "not set (the host enforces no wall deadline)"
            if worker.deadline_at_ms is None
            else "%d ms Unix time%s"
            % (
                worker.deadline_at_ms,
                ""
                if worker.timeout_seconds is None
                else " (%d second request)" % worker.timeout_seconds,
            )
        ),
        "Cwd/workspace: inherited from the parent Ygg session.",
        "Sandbox/approval/environment/extensions: inherited and host-enforced; API 0.2 agent_sessions does not expose exact values to this view.",
        "Isolation: the cwd/filesystem may be shared and is not an isolation boundary.",
        "Delivery: %s; Ygg's durable parent mailbox owns completion claim/ack." % worker.delivery_state,
        "Session: %s" % (worker.session or "not yet exposed by agent_sessions"),
    ]
    if worker.export_reference:
        lines.append("Export: %s" % safe_label(worker.export_reference))
    if worker.recovered:
        lines.append(
            "Restart: recovered from host-owned ancestry after %d process generation change(s)."
            % max(1, worker.restart_count)
        )
    if worker.artifacts:
        lines.append("Artifacts:")
        for artifact in worker.artifacts:
            lines.append("- %s" % safe_label(artifact.label or artifact.identifier))
    if worker.recent_tools:
        lines.append("Recent tool activity (host-observed, latest last):")
        for entry in worker.recent_tools:
            args = entry.get("args") or ""
            action = "%s %s" % (entry["name"], args) if args else str(entry["name"])
            if entry.get("finished_at_ms") is None:
                marker = "running"
            elif entry.get("error"):
                marker = "error"
            else:
                marker = "ok"
            lines.append("- [%s] %s" % (marker, action))
    if worker.summary is not None:
        lines.extend(["", "Host-observed final summary (unsafe controls escaped):", worker.summary])
    if worker.last_error is not None:
        lines.extend(["", "Last bounded error:", worker.last_error])
    return bounded_text("\n".join(lines), 64 * 1024)


def build_snapshot(
    workers: Sequence[Worker],
    *,
    selected_agent_id: Optional[str],
    now_ms: int,
) -> Dict[str, Any]:
    ordered = sorted(workers, key=lambda worker: (worker.created_at_ms, worker.agent_id))
    status_state, status_label, status_detail = compact_status(ordered)
    status: Dict[str, Any] = {"state": status_state, "label": status_label}
    if status_detail:
        status["detail"] = status_detail

    node_ids = {worker.agent_id: semantic_id("worker", worker.agent_id) for worker in ordered}
    nodes: List[Dict[str, Any]] = []
    actions: List[Dict[str, Any]] = []
    activities: List[Dict[str, Any]] = []
    for worker in ordered:
        node_id = node_ids[worker.agent_id]
        inspect_id = semantic_id("inspect", worker.agent_id)
        stop_id = semantic_id("stop", worker.agent_id)
        action_ids = [inspect_id]
        actions.append(
            {
                "id": inspect_id,
                "label": "Inspect worker",
                "command": "subagents",
                "arguments": ["inspect", worker.agent_id],
                "destructive": False,
            }
        )
        if worker.active:
            action_ids.append(stop_id)
            actions.append(
                {
                    "id": stop_id,
                    "label": "Stop worker",
                    "command": "subagents",
                    "arguments": ["stop", worker.agent_id],
                    "destructive": True,
                }
            )
        node: Dict[str, Any] = {
            "id": node_id,
            "state": GENERIC_STATE.get(worker.state, "degraded"),
            "label": safe_label(worker.name),
            "secondary": worker_secondary(worker, now_ms),
            "action_ids": action_ids,
            "references": worker_references(worker),
        }
        parent_node_id = node_ids.get(worker.parent_id or "")
        if parent_node_id is not None:
            node["parent_id"] = parent_node_id
        nodes.append(node)

        phase = latest_action(worker) or safe_label(
            worker.current_tool or worker.phase or worker.state
        )
        metrics: Dict[str, Any] = {
            "tool_calls": worker.tool_call_count,
            "input_tokens": worker.input_tokens or 0,
            "cache_read_tokens": worker.cache_read_tokens or 0,
            "cache_write_tokens": worker.cache_write_tokens or 0,
            "output_tokens": worker.output_tokens or 0,
            "reasoning_tokens": worker.reasoning_tokens or 0,
        }
        if worker.cost_microdollars is not None:
            metrics["cost_microdollars"] = worker.cost_microdollars
        activity: Dict[str, Any] = {
            "id": semantic_id("activity", worker.agent_id),
            "kind": "subagent",
            "state": GENERIC_STATE.get(worker.state, "degraded"),
            # Content-free: no prompt, arguments, results, or child prose.
            "summary": bounded_text("%s · %s" % (worker.name, phase), 1024),
            "provenance": "Ygg agent_sessions · read-only",
            "started_at_ms": worker.started_at_ms,
            "metrics": metrics,
            "references": worker_references(worker),
        }
        if worker.completed_at_ms is not None:
            activity["completed_at_ms"] = worker.completed_at_ms
        activities.append(activity)

    selected: Optional[Worker] = None
    if selected_agent_id is not None:
        selected = next(
            (worker for worker in ordered if worker.agent_id == selected_agent_id), None
        )
    if selected is None and ordered:
        selected = ordered[-1]
    collection: Dict[str, Any] = {
        "kind": "tree",
        "title": status_label,
        "nodes": nodes,
    }
    if selected is not None:
        selected_node = node_ids[selected.agent_id]
        references = worker_references(selected)
        collection["selected_node_id"] = selected_node
        collection["detail"] = {
            "node_id": selected_node,
            "title": bounded_text("parent > %s" % selected.name, 1024),
            "body": detail_body(selected, now_ms),
            "references": references,
        }
    if any(worker.active for worker in ordered):
        actions.append(
            {
                "id": "stop-all",
                "label": "Stop all workers",
                "command": "subagents",
                "arguments": ["stop", "all"],
                "destructive": True,
            }
        )
    return {
        "status": status,
        "activities": activities[-128:],
        "collection": collection,
        "actions": actions,
    }


def narrow_list(workers: Sequence[Worker], now_ms: int) -> str:
    ordered = sorted(workers, key=lambda worker: (worker.created_at_ms, worker.agent_id))
    _, title, _ = compact_status(ordered)
    lines = [title]
    if not ordered:
        lines.append("No cached workers for this parent session.")
        return "\n".join(lines)
    for index, worker in enumerate(ordered):
        branch = "└─" if index == len(ordered) - 1 else "├─"
        action = latest_action(worker) or ""
        lines.append(
            "%s %-20s %-10s %s  %s  %s"
            % (
                branch,
                bounded_text(worker.name, 20),
                STATE_LABEL.get(worker.state, worker.state),
                duration_label(worker.elapsed_ms(now_ms)),
                bounded_text(action, 80) if action else "-",
                worker.agent_id,
            )
        )
    lines.append("Use /subagents inspect <name-or-id> for cached detail.")
    lines.append("Use the model-callable subagent_stop tool for authoritative cancellation.")
    return bounded_text("\n".join(lines), 16 * 1024)
