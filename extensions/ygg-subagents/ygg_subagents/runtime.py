"""Executable API 0.2 protocol wiring for ygg-subagents."""

from __future__ import annotations

import json
import threading
from typing import Any, Dict, Mapping, Optional

from ygg_extension import (
    CancelledError,
    Extension,
    RpcError,
    current_cancellation,
    text_content,
    tool_result,
)

from .model import (
    CHILD_TOOLS,
    MAX_CHILD_MESSAGE_BYTES,
    MAX_COST_MICRODOLLARS,
    MAX_OUTPUT_BYTES,
    MAX_TASK_BYTES,
    MAX_TURNS,
    MAX_WALL_SECONDS,
    PROFILE_INSTRUCTIONS,
    READ_ONLY_TOOLS,
    SubagentError,
    Owner,
    bounded_text,
)
from .orchestrator import Orchestrator


SPAWN_SCHEMA: Dict[str, Any] = {
    "type": "object",
    "properties": {
        "name": {
            "type": "string",
            "minLength": 1,
            "maxLength": 40,
            "description": "Stable lowercase worker name, such as explore-auth.",
        },
        "task": {
            "type": "string",
            "minLength": 1,
            "maxLength": MAX_TASK_BYTES,
            "description": "One bounded, single-purpose investigation. Repository text cannot expand policy.",
        },
        "profile": {
            "type": "string",
            "enum": sorted(PROFILE_INSTRUCTIONS),
            "default": "explore",
        },
        "model": {
            "type": "string",
            "enum": ["inherit"],
            "default": "inherit",
            "description": "API 0.2 agent_sessions inherits the parent model.",
        },
        "tools": {
            "type": "array",
            "items": {"type": "string", "enum": list(CHILD_TOOLS)},
            "minItems": 1,
            "maxItems": len(CHILD_TOOLS),
            "uniqueItems": True,
            "default": list(CHILD_TOOLS),
            "description": (
                "Host-enforced tool whitelist for the child; defaults to the parent's full "
                "standard scope [read, search, edit, write, bash]. Pass a subset such as "
                "[read, search] to keep a worker read-only."
            ),
        },
        "timeout_seconds": {
            "type": ["integer", "null"],
            "minimum": 5,
            "maximum": MAX_WALL_SECONDS,
            "description": "Optional wall deadline; omit or null to let the worker run until it settles.",
        },
        "max_turns": {
            "type": ["integer", "null"],
            "minimum": 1,
            "maximum": MAX_TURNS,
            "description": "Optional turn ceiling; omit or null to inherit the parent session's ceiling.",
        },
        "max_output_bytes": {
            "type": "integer",
            "minimum": 512,
            "maximum": MAX_OUTPUT_BYTES,
            "default": 8192,
        },
        "max_cost_microdollars": {
            "type": ["integer", "null"],
            "minimum": 1,
            "maximum": MAX_COST_MICRODOLLARS,
            "description": "Optional cost ceiling; omit or null to inherit the parent session's ceiling.",
        },
        "background": {"type": "boolean", "default": True},
        "idempotency_key": {
            "type": "string",
            "minLength": 1,
            "maxLength": 128,
            "description": "Optional retry key; reuse with different input is rejected.",
        },
    },
    "required": ["name", "task"],
    "additionalProperties": False,
}
STATUS_SCHEMA: Dict[str, Any] = {
    "type": "object",
    "properties": {
        "target": {
            "type": "string",
            "minLength": 1,
            "maxLength": 512,
            "description": "Optional worker name, stable agent ID, or host agent path.",
        }
    },
    "additionalProperties": False,
}
WAIT_SCHEMA: Dict[str, Any] = {
    "type": "object",
    "properties": {
        "target": {"type": "string", "minLength": 1, "maxLength": 512},
        "timeout_seconds": {
            "type": "integer",
            "minimum": 1,
            "maximum": 60,
            "default": 30,
        },
    },
    "additionalProperties": False,
}
STOP_SCHEMA: Dict[str, Any] = {
    "type": "object",
    "properties": {
        "target": {"type": "string", "minLength": 1, "maxLength": 512},
        "all": {"type": "boolean", "default": False},
    },
    "additionalProperties": False,
}
CONTINUE_SCHEMA: Dict[str, Any] = {
    "type": "object",
    "properties": {
        "target": {
            "type": "string",
            "minLength": 1,
            "maxLength": 512,
            "description": "Worker name, stable agent ID, or host agent path.",
        },
        "message": {
            "type": "string",
            "minLength": 1,
            "maxLength": MAX_CHILD_MESSAGE_BYTES,
            "description": (
                "Steer an active worker, or queue a follow-up task for a settled worker; "
                "the worker's existing task, profile, tools, and ceilings are preserved."
            ),
        },
    },
    "required": ["target", "message"],
    "additionalProperties": False,
}


class SdkAgentSessions:
    """No scheduler or session store: delegate every operation to the SDK."""

    def __init__(self, extension: Extension) -> None:
        self.extension = extension

    def spawn_agent(
        self,
        *,
        task_name: str,
        profile: str,
        fingerprint: str,
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
        return self.extension.spawn_agent(
            task_name=task_name,
            profile=profile,
            fingerprint=fingerprint,
            message=message,
            idempotency_key=idempotency_key,
            tools=tools,
            max_depth=max_depth,
            max_concurrent_children=max_concurrent_children,
            max_turns=max_turns,
            max_tokens=max_tokens,
            max_cost_microdollars=max_cost_microdollars,
            max_output_bytes=max_output_bytes,
            timeout_ms=timeout_ms,
        )

    def list_agents(self) -> Mapping[str, Any]:
        return self.extension.list_agents()

    def wait_agents(self, *, timeout_ms: int) -> Mapping[str, Any]:
        return self.extension.wait_agents(timeout_ms=timeout_ms)

    def interrupt_agent(self, target: str) -> Mapping[str, Any]:
        return self.extension.interrupt_agent(target)

    def send_agent_message(self, target: str, message: str) -> Mapping[str, Any]:
        return self.extension.send_agent_message(target, message)

    def follow_up_agent(self, target: str, message: str) -> Mapping[str, Any]:
        return self.extension.follow_up_agent(target, message)


class PresentationPublisher:
    """Assign one monotonic process-generation revision to complete snapshots."""

    def __init__(self, extension: Extension) -> None:
        self.extension = extension
        self._lock = threading.Lock()
        self._next_revision = 0
        self._closed = False

    def __call__(self, snapshot: Mapping[str, Any]) -> None:
        if not self.extension.initialized:
            return
        with self._lock:
            if self._closed:
                return
            value = dict(snapshot)
            owner = value.pop("_resource_owner", None)
            value["revision"] = self._next_revision
            if isinstance(owner, Mapping):
                self.extension.publish_presentation(value, resource_owner=owner)
            else:
                self.extension.publish_presentation(value)
            self._next_revision += 1

    def close(self) -> None:
        with self._lock:
            self._closed = True


def _require_agent_sessions(extension: Extension) -> None:
    required = {"agent_sessions", "delegation_telemetry_v1"}
    missing = sorted(required.difference(extension.negotiated_features))
    if not missing:
        return
    if "agent_sessions" in missing:
        message = (
            "the trusted ygg-subagents extension does not have an active owner-bound API 0.2 "
            "agent_sessions service; missing: %s"
        ) % ", ".join(missing)
    else:
        message = (
            "the trusted ygg-subagents extension does not have the required delegation telemetry "
            "contract `%s`; restart Ygg after reinstalling a matching current host and "
            "ygg-subagents bundle"
        ) % "delegation_telemetry_v1"
    raise SubagentError(message, code="agent_sessions_unavailable")


def _compact_metadata(result: Mapping[str, Any]) -> Dict[str, Any]:
    metadata: Dict[str, Any] = {
        "schema": "ygg.subagents.result.v1",
        "operation": result.get("operation", "unknown"),
    }
    if isinstance(result.get("counts"), Mapping):
        metadata["counts"] = dict(result["counts"])
    worker = result.get("worker")
    if isinstance(worker, Mapping):
        keep = {
            "id",
            "path",
            "name",
            "state",
            "profile",
            "model",
            "tools",
            "elapsed_ms",
            "turn_count",
            "turn_limit",
            "tokens_used",
            "token_budget",
            "cost_microdollars",
            "cost_budget_microdollars",
            "session",
            "artifacts",
            "recovered_after_restart",
            "delivery",
        }
        metadata["worker"] = {key: value for key, value in worker.items() if key in keep}
    workers = result.get("workers")
    if isinstance(workers, list):
        metadata["worker_ids"] = [
            item.get("id") for item in workers[:16] if isinstance(item, Mapping)
        ]
    if "wait_timed_out" in result:
        metadata["wait_timed_out"] = result["wait_timed_out"]
    if "duplicate" in result:
        metadata["duplicate"] = result["duplicate"]
    if metadata["operation"] == "continue":
        metadata["action"] = result.get("action", "unknown")
        metadata["accepted"] = bool(result.get("accepted"))
    return metadata


def _worker_line(worker: Mapping[str, Any]) -> str:
    name = str(worker.get("name", "worker"))
    identifier = str(worker.get("id", "unknown"))
    state = str(worker.get("state", "unknown"))
    phase = str(worker.get("phase", state))
    elapsed = worker.get("elapsed_ms")
    elapsed_text = "%dms" % elapsed if isinstance(elapsed, int) else "elapsed unknown"
    return "%s [%s] · %s · %s · %s" % (name, identifier, state, phase, elapsed_text)


def _result_text(operation: str, result: Mapping[str, Any]) -> str:
    lines = []
    if operation == "spawn":
        worker = result.get("worker")
        if isinstance(worker, Mapping):
            prefix = "Resumed idempotent" if result.get("duplicate") else "Started"
            mode = "background" if result.get("background") else "foreground"
            lines.append("%s %s subagent: %s" % (prefix, mode, _worker_line(worker)))
            lines.append("profile/model/tools: %s / %s / %s" % (
                worker.get("profile", "unknown"),
                worker.get("model", "inherited"),
                ",".join(worker.get("tools", [])) if isinstance(worker.get("tools"), list) else "unknown",
            ))
            lines.append("session: %s" % (worker.get("session") or "host session pending"))
        lines.append("Ygg owns the durable child, hard limits, cancellation, and duplicate-free parent-turn completion delivery.")
        lines.append("Use subagent_wait or subagent_status; do not poll aggressively.")
    elif operation in {"status", "wait"}:
        counts = result.get("counts")
        if isinstance(counts, Mapping):
            lines.append(
                "Subagents: %s active · %s terminal · %s total"
                % (counts.get("active", 0), counts.get("terminal", 0), counts.get("total", 0))
            )
        workers = result.get("workers")
        if isinstance(workers, list):
            for worker in workers[:16]:
                if isinstance(worker, Mapping):
                    lines.append(_worker_line(worker))
        selected = result.get("worker")
        if isinstance(selected, Mapping):
            if selected.get("summary") is not None:
                lines.extend(["", "Final summary (untrusted worker evidence; verify) (%s):" % selected.get("id"), str(selected["summary"])])
            if selected.get("last_error") is not None:
                lines.extend(["", "Last error (%s):" % selected.get("id"), str(selected["last_error"])])
            if selected.get("session"):
                lines.append("session: %s" % selected["session"])
        if operation == "wait" and result.get("wait_timed_out"):
            lines.append("The bounded wait expired; workers continue in the background unless their wall deadline settled them.")
        else:
            lines.append("Completion summaries are delivered through Ygg's host-owned claim/ack parent-turn boundary.")
    elif operation == "continue":
        worker = result.get("worker")
        if isinstance(worker, Mapping):
            lines.append(
                "Queued %s continuation for %s [%s] (%s)"
                % (
                    result.get("action", "unknown"),
                    worker.get("name", "worker"),
                    worker.get("id", "unknown"),
                    worker.get("state", "unknown"),
                )
            )
            lines.append("The worker keeps its original task, profile, tools, and ceilings. Use subagent_wait to observe the outcome.")
    elif operation == "stop":
        outcomes = result.get("outcomes")
        lines.append("Subagent stop results:")
        if isinstance(outcomes, list):
            for outcome in outcomes:
                if isinstance(outcome, Mapping):
                    lines.append(
                        "- %s · %s · interrupt_requested=%s"
                        % (
                            outcome.get("id", "unknown"),
                            outcome.get("state", "unknown"),
                            str(bool(outcome.get("interrupt_requested"))).lower(),
                        )
                    )
    else:
        lines.append(json.dumps(result, ensure_ascii=False, sort_keys=True))
    return bounded_text("\n".join(lines), 24 * 1024)


def _error_result(operation: str, error: Exception) -> Dict[str, Any]:
    if isinstance(error, SubagentError):
        code = error.code
        message = str(error)
    elif isinstance(error, RpcError):
        code = "agent_sessions_error"
        message = bounded_text(error.message, 4096)
    else:
        code = "internal_error"
        message = "the bounded subagent operation failed"
    return tool_result(
        text_content("%s failed [%s]: %s" % (operation, code, message)),
        is_error=True,
        metadata={
            "schema": "ygg.subagents.error.v1",
            "operation": operation,
            "code": code,
        },
    )


def create_runtime() -> tuple[Extension, Orchestrator, PresentationPublisher]:
    extension = Extension(
        api_version="0.2",
        max_concurrent_requests=4,
        max_pending_requests=16,
        supported_features=(
            "request_cancellation",
            "content_parts",
            "lifecycle_events",
            "agent_sessions",
            "delegation_telemetry_v1",
        ),
    )
    publisher = PresentationPublisher(extension)
    orchestrator = Orchestrator(publish=publisher)
    sessions = SdkAgentSessions(extension)

    def invoke(operation: str, arguments: Mapping[str, Any], context: Mapping[str, Any]):
        try:
            _require_agent_sessions(extension)
            owner = Owner.from_context(context)
            token = current_cancellation()
            if operation == "spawn":
                result = orchestrator.spawn(sessions, owner, arguments, token)
            elif operation == "status":
                result = orchestrator.status(sessions, owner, arguments, token)
            elif operation == "wait":
                result = orchestrator.wait(sessions, owner, arguments, token)
            elif operation == "stop":
                result = orchestrator.stop(sessions, owner, arguments, token)
            elif operation == "continue":
                result = orchestrator.continue_worker(sessions, owner, arguments, token)
            else:  # pragma: no cover - closed registration set
                raise SubagentError("unknown operation")
            return tool_result(
                text_content(_result_text(operation, result)),
                metadata=_compact_metadata(result),
            )
        except CancelledError:
            raise
        except (SubagentError, RpcError) as error:
            return _error_result(operation, error)
        except Exception as error:
            extension.log.error(
                "subagent operation failed",
                operation=operation,
                error_type=type(error).__name__,
            )
            return _error_result(operation, error)

    @extension.tool(
        name="subagent_spawn",
        description=(
            "Launch one named, depth-one Ygg worker with a bounded profile, inherited model, and an optional (host-enforced) tool whitelist, wall deadline, turn ceiling, output size, and cost ceiling. "
            "Ceilings default to inherited/unlimited, so a minimal spawn is just name and task. Defaults to background and is retry-safe through an idempotency key. "
            "At most 8 active children per parent (32 total); no writers beyond the granted tools, no graph, swarm, team chat, or recursive spawn."
        ),
        parameters=SPAWN_SCHEMA,
    )
    def subagent_spawn(arguments: Mapping[str, Any], context: Mapping[str, Any]):
        return invoke("spawn", arguments, context)

    @extension.tool(
        name="subagent_status",
        description=(
            "Read the authoritative host-present worker tree plus bounded terminal evidence retained after owning-run cleanup, and return states, budgets, session/artifact references, and an optional terminal summary. "
            "Prompts, tool arguments/results, and running child prose are excluded."
        ),
        parameters=STATUS_SCHEMA,
    )
    def subagent_status(arguments: Mapping[str, Any], context: Mapping[str, Any]):
        return invoke("status", arguments, context)

    @extension.tool(
        name="subagent_wait",
        description=(
            "Wait at most 60 seconds for one or all owned workers, while preserving background execution on wait cancellation/expiry and settling an observed worker wall deadline through Ygg cancellation."
        ),
        parameters=WAIT_SCHEMA,
    )
    def subagent_wait(arguments: Mapping[str, Any], context: Mapping[str, Any]):
        return invoke("wait", arguments, context)

    @extension.tool(
        name="subagent_stop",
        description=(
            "Idempotently request host-owned interruption of one owned worker tree or all active owned workers. Never accepts a model-supplied owner or cross-session target."
        ),
        parameters=STOP_SCHEMA,
    )
    def subagent_stop(arguments: Mapping[str, Any], context: Mapping[str, Any]):
        return invoke("stop", arguments, context)

    @extension.tool(
        name="subagent_continue",
        description=(
            "Continue one owned worker with a new instruction: it is steered while active, or resumed through its durable host session after settlement. "
            "The worker's original task, profile, tools, and ceilings are preserved. The call returns once the host accepts the instruction; use subagent_wait to observe the outcome."
        ),
        parameters=CONTINUE_SCHEMA,
    )
    def subagent_continue(arguments: Mapping[str, Any], context: Mapping[str, Any]):
        return invoke("continue", arguments, context)

    @extension.command(
        name="subagents",
        description="Browse workers and inspect read-only delegated transcripts",
        usage="/subagents [list|inspect <name-or-id>|stop <name-or-id|all>]",
    )
    def subagents_command(arguments: list[str], context: Mapping[str, Any]):
        try:
            live_list = not arguments or (
                len(arguments) == 1 and arguments[0] in {"list", "status"}
            )
            if live_list and isinstance(context.get("resource_owner"), Mapping):
                # Interactive frontends may keep the explicit worker browser
                # open while children finish. Refresh through the same
                # owner-bound host service used by the tool; cached state alone
                # cannot observe delegated lifecycle changes.
                _require_agent_sessions(extension)
                authenticated_owner = Owner.from_context(context)
                result = orchestrator.status(
                    sessions,
                    authenticated_owner,
                    {},
                    current_cancellation(),
                )
                return {
                    "text": _result_text("status", result),
                    "notifications": [],
                }
            if (
                len(arguments) == 2
                and arguments[0] == "stop"
                and isinstance(context.get("resource_owner"), Mapping)
            ):
                # Generic TUI/Serve action routing may attach an authenticated
                # operation owner. Use it only when the host also binds that
                # command request as an agent_sessions parent; otherwise the
                # reverse service rejects it and the fallback remains fail-closed.
                _require_agent_sessions(extension)
                authenticated_owner = Owner.from_context(context)
                stop_arguments: Dict[str, Any]
                if arguments[1] == "all":
                    stop_arguments = {"all": True}
                else:
                    stop_arguments = {"target": arguments[1]}
                result = orchestrator.stop(
                    sessions,
                    authenticated_owner,
                    stop_arguments,
                    current_cancellation(),
                )
                return {
                    "text": _result_text("stop", result),
                    "notifications": [],
                }
            return orchestrator.command(arguments, context)
        except (SubagentError, RpcError) as error:
            extension.log.warning(
                "subagent command action denied",
                error_type=type(error).__name__,
            )
            return {
                "text": "subagents command failed closed: %s" % bounded_text(str(error), 4096),
                "notifications": [
                    {
                        "level": "warning",
                        "title": "Subagents",
                        "message": "The command did not alter host-owned worker state; use subagent_stop from an active owner-scoped turn.",
                    }
                ],
            }

    @extension.status("status")
    def subagents_status(_params: Mapping[str, Any], context: Mapping[str, Any]):
        return orchestrator.status_contribution(context)

    @extension.on_lifecycle("session/settled")
    def parent_session_settled(event: Mapping[str, Any]):
        orchestrator.session_settled(event)

    @extension.on_shutdown
    def shutdown(_params: Mapping[str, Any]):
        # The host's API 0.2 shutdown path interrupts every principal-owned child
        # tree. No stale parent request is reused from this process callback.
        publisher.close()
        orchestrator.shutdown_local()

    return extension, orchestrator, publisher


EXTENSION, ORCHESTRATOR, PRESENTATION = create_runtime()


def main() -> None:
    EXTENSION.run()


if __name__ == "__main__":
    main()
