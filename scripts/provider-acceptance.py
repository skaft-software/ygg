#!/usr/bin/env python3
"""Run secret-safe live provider smoke checks for an immutable release candidate."""

from __future__ import annotations

import hashlib
import json
import os
import pathlib
import re
import signal
import stat
import subprocess
import sys
import tempfile
import uuid
from dataclasses import dataclass
from urllib.parse import urlsplit

PROTOCOL_VERSION = 1
MAX_FRAME_BYTES = 1024 * 1024
MAX_OUTPUT_BYTES = 16 * 1024 * 1024
MAX_HOST_BYTES = 256 * 1024 * 1024
PROCESS_TIMEOUT_SECONDS = 180
SAFE_ID = re.compile(r"^[A-Za-z0-9_.-]{1,128}$")
SAFE_MODEL_ID = re.compile(r"^[A-Za-z0-9_.:/@+-]{1,256}$")
SAFE_TOOL_CALL_ID = re.compile(r"^[A-Za-z0-9_.:/@+|=-]+$")
SAFE_TOOL_NAME = re.compile(r"^[a-z0-9_.-]{1,128}$")
MAX_TOOL_CALL_ID_BYTES = 512
POLICY_EFFECTS = {
    "pure",
    "workspace_read",
    "host_read",
    "workspace_mutation",
    "host_mutation",
    "host_process",
    "network",
    "delegation",
    "extension",
    "unknown",
}
POLICY_AUTHORIZATIONS = {"policy", "human_grant"}
POLICY_DENIAL_CODES = {
    "workspace_confinement",
    "edit_disabled",
    "write_disabled",
    "process_disabled",
    "shell_disabled",
    "remote_read_disabled",
    "effect_unknown",
    "effect_host_read_denied",
    "effect_host_mutation_denied",
    "effect_native_process_denied",
    "effect_network_denied",
    "effect_delegation_denied",
    "effect_extension_denied",
    "approval_unavailable",
    "approval_denied",
    "invalid_effect_intent",
    "effect_intent_too_large",
    "effect_broker_unavailable",
    "secondary_hook_denied",
    "effect_reservation_commit_denied",
    "invalid_tool_arguments",
}
POLICY_VALUE_SOURCES = {"default", "config", "environment", "cli", "host_request"}
EFFECTIVE_TOOL_POLICY_FIELDS = {
    "effect_policy",
    "workspace_confinement",
    "allow_edit",
    "allow_write",
    "allow_process",
    "allow_shell",
    "shell_path",
    "bash_timeout_ms",
    "max_output_bytes",
    "allow_remote_read",
}
BOOLEAN_POLICY_FIELDS = {
    "workspace_confinement",
    "allow_edit",
    "allow_write",
    "allow_process",
    "allow_shell",
    "allow_remote_read",
}
SHELL_SELECTIONS = {
    "configured",
    "system_bash",
    "path_bash",
    "sh_fallback",
    "unavailable",
}
HOST_REQUEST_SHELL_SELECTIONS = SHELL_SELECTIONS - {"configured"}
MAX_BASH_TIMEOUT_MS = 3_600_000
MAX_TOOL_OUTPUT_BYTES = 1024 * 1024
FIXED_HOST_REQUEST_BOOLEAN_POLICY = {
    "workspace_confinement": True,
    "allow_edit": False,
    "allow_write": False,
    "allow_process": False,
    "allow_shell": False,
    "allow_remote_read": False,
}
FIXED_HOST_REQUEST_BASH_TIMEOUT_MS = 120_000
FIXED_HOST_REQUEST_MAX_OUTPUT_BYTES = 50 * 1024
FIXED_NON_AUDIO_REGISTERED_TOOLS = frozenset({"read"})
EFFECT_BOUND_DENIAL_CODES = {
    "effect_unknown": "unknown",
    "effect_host_read_denied": "host_read",
    "effect_host_mutation_denied": "host_mutation",
    "effect_native_process_denied": "host_process",
    "effect_network_denied": "network",
    "effect_delegation_denied": "delegation",
    "effect_extension_denied": "extension",
}
EFFECTLESS_DENIAL_CODES = {
    "workspace_confinement",
    "edit_disabled",
    "write_disabled",
    "process_disabled",
    "shell_disabled",
    "remote_read_disabled",
    "invalid_tool_arguments",
}
CLASSIFIED_DENIAL_CODES = {
    "invalid_effect_intent",
    "effect_intent_too_large",
    "effect_broker_unavailable",
    "approval_unavailable",
    "approval_denied",
    "secondary_hook_denied",
    "effect_reservation_commit_denied",
}
TOOL_LIFECYCLE_EVENT_TYPES = {
    "tool_start",
    "tool_progress",
    "tool_policy",
    "tool_finish",
}
RUN_EVENT_TYPES = {
    "accepted",
    "started",
    "extension_notification",
    "model_delta",
    "output_media",
    "provider_retry",
    "steering_delivered",
    "follow_up_delivered",
    "compaction_start",
    "compaction_finish",
    "tool_start",
    "tool_policy",
    "tool_progress",
    "tool_finish",
    "candidate_rejected",
    "model_step",
    "settled",
    "final_result",
    "protocol_error",
}
TERMINAL_EVENT_TYPES = {"hello", "models", "final_result", "protocol_error", "shutdown"}
AUDIO_FIXTURE_SHA256 = (
    "0847c6aac1d2530ef9a090bca8f824253aafc50df4da1b649eb5b9246846b5b1"
)
AUDIO_EXPECTED_TRANSCRIPT = "cobalt seven marigold"


class AcceptanceError(Exception):
    """A sanitized acceptance failure."""


@dataclass(frozen=True)
class Route:
    label: str
    provider: str
    model: str
    base_url: str
    api_key: str
    provider_mode: str
    audio: bool = False


@dataclass(frozen=True)
class PolicyDecision:
    effect: str | None
    allowed: bool
    authorization: str | None
    denial_code: str | None


@dataclass
class ActiveTool:
    name: str
    registered: bool
    decision: PolicyDecision | None = None


def required_environment(name: str) -> str:
    value = os.environ.get(name, "")
    if not value or any(character.isspace() for character in value):
        raise AcceptanceError(f"required acceptance setting is missing or malformed: {name}")
    return value


def validate_route(route: Route) -> None:
    if SAFE_ID.fullmatch(route.provider) is None:
        raise AcceptanceError(f"provider identifier is malformed for {route.label}")
    if SAFE_MODEL_ID.fullmatch(route.model) is None:
        raise AcceptanceError(f"model identifier is malformed for {route.label}")
    endpoint = urlsplit(route.base_url)
    if (
        endpoint.scheme != "https"
        or not endpoint.hostname
        or endpoint.username is not None
        or endpoint.password is not None
        or endpoint.query
        or endpoint.fragment
    ):
        raise AcceptanceError(f"endpoint configuration is malformed for {route.label}")


def strict_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for key, value in pairs:
        if key in result:
            raise AcceptanceError("host emitted duplicate JSON fields")
        result[key] = value
    return result


def reject_constant(_value: str) -> object:
    raise AcceptanceError("host emitted a non-standard JSON constant")


def parse_events(payload: bytes) -> list[dict[str, object]]:
    if len(payload) > MAX_OUTPUT_BYTES:
        raise AcceptanceError("host acceptance output exceeded its aggregate limit")
    if payload and not payload.endswith(b"\n"):
        raise AcceptanceError("host emitted an incomplete protocol frame")
    events: list[dict[str, object]] = []
    for raw_line in payload.splitlines(keepends=True):
        if len(raw_line) > MAX_FRAME_BYTES:
            raise AcceptanceError("host emitted an oversized protocol frame")
        try:
            line = raw_line[:-1].decode("utf-8")
            event = json.loads(
                line,
                object_pairs_hook=strict_object,
                parse_constant=reject_constant,
            )
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise AcceptanceError("host emitted malformed protocol JSON") from error
        if not isinstance(event, dict):
            raise AcceptanceError("host emitted a non-object protocol frame")
        protocol_version = event.get("protocol_version")
        if (
            isinstance(protocol_version, bool)
            or not isinstance(protocol_version, int)
            or protocol_version != PROTOCOL_VERSION
        ):
            raise AcceptanceError("host protocol version mismatch")
        events.append(event)
    if not events:
        raise AcceptanceError("host emitted no acceptance events")
    return events


def exact_policy_object(
    value: object, expected_fields: set[str], error: str
) -> dict[str, object]:
    if not isinstance(value, dict) or set(value) != expected_fields:
        raise AcceptanceError(error)
    return value


def tool_call_id(value: object) -> str:
    if (
        not isinstance(value, str)
        or SAFE_TOOL_CALL_ID.fullmatch(value) is None
        or len(value) > MAX_TOOL_CALL_ID_BYTES
    ):
        raise AcceptanceError("host tool-policy identity was malformed")
    return value


def tool_name(value: object) -> str:
    if not isinstance(value, str) or SAFE_TOOL_NAME.fullmatch(value) is None:
        raise AcceptanceError("host tool-policy identity was malformed")
    return value


def validate_effective_tool_policy(value: object) -> dict[str, object]:
    policy = exact_policy_object(
        value,
        EFFECTIVE_TOOL_POLICY_FIELDS,
        "host tool-policy effective policy was not secret-safe",
    )
    values: dict[str, dict[str, object]] = {}
    for field in EFFECTIVE_TOOL_POLICY_FIELDS:
        values[field] = exact_policy_object(
            policy[field],
            {"value", "source"},
            "host tool-policy provenance was not secret-safe",
        )
        source = values[field]["source"]
        if not isinstance(source, str) or source not in POLICY_VALUE_SOURCES:
            raise AcceptanceError("host tool-policy provenance was malformed")

    effect_policy = values["effect_policy"]["value"]
    if not isinstance(effect_policy, str) or effect_policy not in {
        "controlled",
        "controlled_bash_approval",
        "unsafe_host",
    }:
        raise AcceptanceError("host tool-policy effective policy was malformed")
    for field in BOOLEAN_POLICY_FIELDS:
        if type(values[field]["value"]) is not bool:
            raise AcceptanceError("host tool-policy effective policy was malformed")

    shell = exact_policy_object(
        values["shell_path"]["value"],
        {"selection"},
        "host tool-policy effective policy was not secret-safe",
    )
    selection = shell["selection"]
    if not isinstance(selection, str) or selection not in SHELL_SELECTIONS:
        raise AcceptanceError("host tool-policy effective policy was malformed")

    bash_timeout_ms = values["bash_timeout_ms"]["value"]
    if (
        type(bash_timeout_ms) is not int
        or not 1_000 <= bash_timeout_ms <= MAX_BASH_TIMEOUT_MS
    ):
        raise AcceptanceError("host tool-policy effective policy was malformed")
    max_output_bytes = values["max_output_bytes"]["value"]
    if (
        type(max_output_bytes) is not int
        or not 1_024 <= max_output_bytes <= MAX_TOOL_OUTPUT_BYTES
    ):
        raise AcceptanceError("host tool-policy effective policy was malformed")
    return policy


def validate_fixed_host_request_policy(policy: dict[str, object]) -> None:
    for field in EFFECTIVE_TOOL_POLICY_FIELDS:
        if policy[field]["source"] != "host_request":
            raise AcceptanceError("host tool-policy provenance did not match the request")
    if policy["effect_policy"]["value"] != "controlled":
        raise AcceptanceError("host tool-policy mode did not match the request")
    for field, expected in FIXED_HOST_REQUEST_BOOLEAN_POLICY.items():
        if policy[field]["value"] is not expected:
            raise AcceptanceError("host tool-policy capabilities did not match the request")
    if policy["bash_timeout_ms"]["value"] != FIXED_HOST_REQUEST_BASH_TIMEOUT_MS:
        raise AcceptanceError("host tool-policy limits did not match the request")
    if policy["max_output_bytes"]["value"] != FIXED_HOST_REQUEST_MAX_OUTPUT_BYTES:
        raise AcceptanceError("host tool-policy limits did not match the request")
    if policy["shell_path"]["value"]["selection"] not in HOST_REQUEST_SHELL_SELECTIONS:
        raise AcceptanceError("host tool-policy shell selection did not match the request")


def validate_registered_tools(value: object) -> frozenset[str]:
    if not isinstance(value, list):
        raise AcceptanceError("host registered-tool set was malformed")
    names = [tool_name(name) for name in value]
    if len(names) != len(set(names)):
        raise AcceptanceError("host registered-tool set was malformed")
    registered = frozenset(names)
    if registered != FIXED_NON_AUDIO_REGISTERED_TOOLS:
        raise AcceptanceError("host registered-tool set did not match the request")
    return registered


def validate_denial_correlation(effect: str | None, denial_code: str) -> None:
    if effect is None:
        if denial_code not in EFFECTLESS_DENIAL_CODES:
            raise AcceptanceError("host tool-policy denial did not match its effect")
        return
    expected_effect = EFFECT_BOUND_DENIAL_CODES.get(denial_code)
    if expected_effect is not None:
        if effect != expected_effect:
            raise AcceptanceError("host tool-policy denial did not match its effect")
        return
    if denial_code == "workspace_confinement":
        if effect not in {"workspace_read", "workspace_mutation"}:
            raise AcceptanceError("host tool-policy denial did not match its effect")
        return
    if denial_code in CLASSIFIED_DENIAL_CODES and effect != "unknown":
        return
    raise AcceptanceError("host tool-policy denial did not match its effect")


def validate_tool_policy_event(
    value: object, effective_policy: dict[str, object]
) -> tuple[str, str, PolicyDecision]:
    data = exact_policy_object(
        value,
        {"toolCallId", "toolName", "decision"},
        "host tool-policy event was not secret-safe",
    )
    call_id = tool_call_id(data["toolCallId"])
    name = tool_name(data["toolName"])
    raw_decision = data["decision"]
    if not isinstance(raw_decision, dict) or type(raw_decision.get("allowed")) is not bool:
        raise AcceptanceError("host tool-policy decision was malformed")
    allowed = raw_decision["allowed"]
    effect: str | None
    authorization: str | None
    denial_code: str | None
    if allowed:
        decision = exact_policy_object(
            raw_decision,
            {"effect", "allowed", "authorization", "policy"},
            "host tool-policy event was not secret-safe",
        )
        effect = decision["effect"]
        authorization = decision["authorization"]
        denial_code = None
        if (
            not isinstance(effect, str)
            or effect not in POLICY_EFFECTS - {"unknown"}
            or not isinstance(authorization, str)
            or authorization not in POLICY_AUTHORIZATIONS
        ):
            raise AcceptanceError("host tool-policy decision was malformed")
    else:
        decision = raw_decision
        decision_fields = set(decision)
        if decision_fields not in (
            {"allowed", "denial_code", "policy"},
            {"effect", "allowed", "denial_code", "policy"},
        ):
            raise AcceptanceError("host tool-policy event was not secret-safe")
        denial_code = decision.get("denial_code")
        if not isinstance(denial_code, str) or denial_code not in POLICY_DENIAL_CODES:
            raise AcceptanceError("host tool-policy decision was malformed")
        effect = None
        if "effect" in decision:
            effect = decision["effect"]
            if not isinstance(effect, str) or effect not in POLICY_EFFECTS:
                raise AcceptanceError("host tool-policy decision was malformed")
        authorization = None
        validate_denial_correlation(effect, denial_code)

    policy = validate_effective_tool_policy(decision["policy"])
    if policy != effective_policy:
        raise AcceptanceError("host tool-policy did not match the effective run policy")
    return call_id, name, PolicyDecision(effect, allowed, authorization, denial_code)


def validate_read_decision(decision: PolicyDecision) -> None:
    if decision.allowed:
        if decision.effect != "workspace_read" or decision.authorization != "policy":
            raise AcceptanceError("host read tool-policy decision did not match the request")
        return
    if decision.effect is None:
        if decision.denial_code not in {
            "invalid_tool_arguments",
            "workspace_confinement",
            "remote_read_disabled",
        }:
            raise AcceptanceError("host read tool-policy denial did not match the request")
        return
    if decision.effect != "workspace_read" or decision.denial_code not in {
        "workspace_confinement",
        "invalid_effect_intent",
        "effect_intent_too_large",
        "effect_broker_unavailable",
        "secondary_hook_denied",
        "effect_reservation_commit_denied",
    }:
        raise AcceptanceError("host read tool-policy denial did not match the request")


def validate_tool_policy_lifecycle(run_events: list[dict[str, object]]) -> None:
    active_tools: dict[str, ActiveTool] = {}
    completed_tools: set[str] = set()
    effective_policy: dict[str, object] | None = None
    registered_tools: frozenset[str] | None = None
    accepted = False
    started = False
    settled = False
    successful_read_calls = 0
    for event in run_events:
        event_type = event["type"]
        if event_type == "accepted":
            if accepted or started:
                raise AcceptanceError("host tool-policy acceptance lifecycle was malformed")
            data = event.get("data")
            if not isinstance(data, dict):
                raise AcceptanceError("host tool-policy acceptance lifecycle was malformed")
            effective_policy = validate_effective_tool_policy(
                data.get("effective_tool_policy")
            )
            validate_fixed_host_request_policy(effective_policy)
            registered_tools = validate_registered_tools(data.get("registered_tools"))
            accepted = True
            continue
        if event_type == "started":
            if not accepted or started or settled:
                raise AcceptanceError("host tool-policy acceptance lifecycle was malformed")
            started = True
            continue
        if event_type == "settled":
            if not accepted or not started or settled:
                raise AcceptanceError("host tool-policy acceptance lifecycle was malformed")
            if active_tools:
                raise AcceptanceError("host tool-policy call did not finish")
            settled = True
            continue
        if event_type == "tool_start":
            if not accepted or not started or settled or registered_tools is None:
                raise AcceptanceError("host tool-policy appeared outside the run lifecycle")
            data = event.get("data")
            if not isinstance(data, dict):
                raise AcceptanceError("host tool-policy tool lifecycle was malformed")
            call_id = tool_call_id(data.get("toolCallId"))
            name = tool_name(data.get("toolName"))
            if call_id in active_tools or call_id in completed_tools:
                raise AcceptanceError("host tool-policy tool identity was reused")
            active_tools[call_id] = ActiveTool(name, name in registered_tools)
            continue
        if event_type == "tool_progress":
            if not accepted or not started or settled:
                raise AcceptanceError("host tool-policy appeared outside the run lifecycle")
            data = event.get("data")
            if not isinstance(data, dict):
                raise AcceptanceError("host tool-policy tool lifecycle was malformed")
            call_id = tool_call_id(data.get("toolCallId"))
            tool = active_tools.get(call_id)
            if (
                tool is None
                or not tool.registered
                or tool.decision is not None and not tool.decision.allowed
            ):
                raise AcceptanceError("host tool-policy tool lifecycle was misordered")
            continue
        if event_type == "tool_policy":
            if not accepted or not started or settled or effective_policy is None:
                raise AcceptanceError("host tool-policy appeared outside the run lifecycle")
            call_id, name, decision = validate_tool_policy_event(
                event.get("data"), effective_policy
            )
            tool = active_tools.get(call_id)
            if (
                tool is None
                or not tool.registered
                or tool.name != name
                or tool.decision is not None
            ):
                raise AcceptanceError("host tool-policy tool lifecycle was misordered")
            validate_read_decision(decision)
            tool.decision = decision
            continue
        if event_type == "tool_finish":
            if not accepted or not started or settled:
                raise AcceptanceError("host tool-policy appeared outside the run lifecycle")
            data = event.get("data")
            if not isinstance(data, dict):
                raise AcceptanceError("host tool-policy tool lifecycle was malformed")
            call_id = tool_call_id(data.get("toolCallId"))
            ok = data.get("ok")
            if type(ok) is not bool:
                raise AcceptanceError("host tool-policy tool lifecycle was malformed")
            tool = active_tools.pop(call_id, None)
            if tool is None:
                raise AcceptanceError("host tool-policy tool lifecycle was misordered")
            if not tool.registered:
                if ok:
                    raise AcceptanceError("host unregistered tool call unexpectedly succeeded")
                completed_tools.add(call_id)
                continue
            decision = tool.decision
            if decision is None:
                raise AcceptanceError("host registered tool call lacked a policy decision")
            if not decision.allowed and ok:
                raise AcceptanceError("host denied tool-policy call did not fail")
            if tool.name == "read" and decision.allowed and ok:
                successful_read_calls += 1
            completed_tools.add(call_id)

    if not accepted or not started or not settled:
        raise AcceptanceError("host tool-policy acceptance lifecycle was malformed")
    if successful_read_calls == 0:
        raise AcceptanceError("host did not prove a successful read tool-policy decision")


def validate_audio_toolless_lifecycle(run_events: list[dict[str, object]]) -> None:
    if any(event["type"] in TOOL_LIFECYCLE_EVENT_TYPES for event in run_events):
        raise AcceptanceError("host audio route unexpectedly emitted a tool lifecycle")


def validate_exchange(
    events: list[dict[str, object]],
    hello_request_id: str,
    request_id: str,
    run_id: str,
    *,
    require_audio: bool,
) -> list[dict[str, object]]:
    expected_sequence = {hello_request_id: 1, request_id: 1}
    terminal = {hello_request_id: False, request_id: False}
    hello_events: list[dict[str, object]] = []
    run_events: list[dict[str, object]] = []
    seen_run = False
    for event in events:
        scope = event.get("request_id")
        if not isinstance(scope, str) or scope not in expected_sequence:
            raise AcceptanceError("host protocol request scope mismatch")
        if terminal[scope]:
            raise AcceptanceError("host emitted output after a terminal event")
        sequence = event.get("seq")
        if (
            isinstance(sequence, bool)
            or not isinstance(sequence, int)
            or sequence != expected_sequence[scope]
        ):
            raise AcceptanceError("host protocol sequence mismatch")
        expected_sequence[scope] += 1
        event_type = event.get("type")
        if not isinstance(event_type, str):
            raise AcceptanceError("host emitted an invalid event type")
        if event_type in TERMINAL_EVENT_TYPES:
            terminal[scope] = True
        if scope == hello_request_id:
            if seen_run or event.get("run_id") is not None or event_type != "hello":
                raise AcceptanceError("host hello negotiation was malformed")
            hello_events.append(event)
        else:
            seen_run = True
            if event.get("run_id") != run_id or event.get("session_id") is not None:
                raise AcceptanceError("host protocol run/session scope mismatch")
            if event_type not in RUN_EVENT_TYPES:
                raise AcceptanceError("host emitted an event not permitted for run")
            run_events.append(event)
    if len(hello_events) != 1 or not terminal[hello_request_id]:
        raise AcceptanceError("host hello negotiation was incomplete")
    hello_data = hello_events[0].get("data")
    if not isinstance(hello_data, dict):
        raise AcceptanceError("host hello capabilities were malformed")
    max_frame_bytes = hello_data.get("max_frame_bytes")
    if (
        isinstance(max_frame_bytes, bool)
        or not isinstance(max_frame_bytes, int)
        or max_frame_bytes != MAX_FRAME_BYTES
    ):
        raise AcceptanceError("host frame limit negotiation failed")
    commands = hello_data.get("commands")
    features = hello_data.get("features")
    required_features = {"streaming", "inline_models", "typed_media_input"}
    if require_audio:
        required_features.add("typed_audio_input")
    if (
        not isinstance(commands, list)
        or "run" not in commands
        or not isinstance(features, dict)
        or any(features.get(feature) is not True for feature in required_features)
    ):
        raise AcceptanceError("host lacks required provider-acceptance capabilities")
    if not run_events or not terminal[request_id]:
        raise AcceptanceError("host run protocol was incomplete")
    if require_audio:
        validate_audio_toolless_lifecycle(run_events)
    else:
        validate_tool_policy_lifecycle(run_events)
    return run_events


def terminate_group(process: subprocess.Popen[bytes]) -> None:
    if process.poll() is not None:
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
        process.wait(timeout=4)
    except (ProcessLookupError, subprocess.TimeoutExpired):
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait(timeout=4)


def host_environment(home: pathlib.Path) -> dict[str, str]:
    environment = {
        "HOME": str(home),
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "RUST_BACKTRACE": "0",
        "RUST_LOG": "off",
    }
    for name in ("SSL_CERT_FILE", "SSL_CERT_DIR", "TMPDIR", "TMP", "TEMP"):
        value = os.environ.get(name)
        if value:
            environment[name] = value
    return environment


def exchange(
    host: pathlib.Path,
    home: pathlib.Path,
    request: dict[str, object],
    *,
    require_audio: bool,
) -> list[dict[str, object]]:
    hello_request_id = f"hello-{uuid.uuid4().hex}"
    hello = {
        "protocol_version": PROTOCOL_VERSION,
        "request_id": hello_request_id,
        "command": "hello",
    }
    frames = [
        json.dumps(frame, separators=(",", ":"), ensure_ascii=False).encode("utf-8") + b"\n"
        for frame in (hello, request)
    ]
    if any(len(frame) > MAX_FRAME_BYTES for frame in frames):
        raise AcceptanceError("acceptance request exceeded the host frame limit")
    with tempfile.TemporaryFile() as protocol_output:
        process = subprocess.Popen(
            [str(host)],
            stdin=subprocess.PIPE,
            stdout=protocol_output,
            stderr=subprocess.DEVNULL,
            env=host_environment(home),
            start_new_session=True,
        )
        try:
            process.communicate(b"".join(frames), timeout=PROCESS_TIMEOUT_SECONDS)
        except subprocess.TimeoutExpired as error:
            terminate_group(process)
            raise AcceptanceError("host acceptance request timed out") from error
        finally:
            terminate_group(process)
        if process.returncode != 0:
            raise AcceptanceError("host exited unsuccessfully during acceptance")
        output_size = os.fstat(protocol_output.fileno()).st_size
        if output_size > MAX_OUTPUT_BYTES:
            raise AcceptanceError("host acceptance output exceeded its aggregate limit")
        protocol_output.seek(0)
        output = protocol_output.read(MAX_OUTPUT_BYTES + 1)
    return validate_exchange(
        parse_events(output),
        hello_request_id,
        str(request["request_id"]),
        str(request["run_id"]),
        require_audio=require_audio,
    )


def require_success(
    events: list[dict[str, object]],
    expected: str,
    *,
    require_tool: bool,
    normalize_transcript: bool = False,
) -> None:
    event_types = [event.get("type") for event in events]
    if event_types.count("final_result") != 1 or event_types[-1] != "final_result":
        raise AcceptanceError("host did not emit exactly one terminal result")
    if (
        event_types.count("accepted") != 1
        or event_types.count("started") != 1
        or event_types.count("settled") != 1
        or not (
            event_types.index("accepted")
            < event_types.index("started")
            < event_types.index("settled")
            < event_types.index("final_result")
        )
    ):
        raise AcceptanceError("host acceptance lifecycle was incomplete")
    streamed_text = any(
        event.get("type") == "model_delta"
        and isinstance(event.get("data"), dict)
        and event["data"].get("channel") == "text"
        and isinstance(event["data"].get("text"), str)
        and bool(event["data"]["text"])
        for event in events
    )
    if not streamed_text:
        raise AcceptanceError("provider route did not stream model text")
    if require_tool:
        started_tools = {
            event["data"].get("toolCallId")
            for event in events
            if event.get("type") == "tool_start"
            and isinstance(event.get("data"), dict)
            and event["data"].get("toolName") == "read"
        }
        successful_tools = {
            event["data"].get("toolCallId")
            for event in events
            if event.get("type") == "tool_finish"
            and isinstance(event.get("data"), dict)
            and event["data"].get("ok") is True
        }
        if not any(
            isinstance(tool_id, str) and tool_id in successful_tools for tool_id in started_tools
        ):
            raise AcceptanceError("provider route did not complete the read tool")
    final_data = events[-1].get("data")
    if not isinstance(final_data, dict) or final_data.get("status") != "completed":
        raise AcceptanceError("provider route did not complete successfully")
    output = final_data.get("output")
    if not isinstance(output, str):
        raise AcceptanceError("provider route did not return its acceptance canary")
    if normalize_transcript:
        output = " ".join(re.findall(r"[a-z0-9]+", output.casefold()))
    if expected not in output:
        raise AcceptanceError("provider route did not return its acceptance canary")


def stage_audio_fixture(path: pathlib.Path) -> None:
    if not hasattr(os, "O_NOFOLLOW"):
        raise AcceptanceError("audio fixture staging is unsupported on this platform")
    source = pathlib.Path(__file__).parent / "fixtures" / "provider-acceptance.wav"
    source_fd = os.open(source, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW)
    try:
        before = os.fstat(source_fd)
        if (
            not stat.S_ISREG(before.st_mode)
            or before.st_nlink != 1
            or before.st_uid != os.geteuid()
            or before.st_mode & 0o022 != 0
            or before.st_size <= 44
            or before.st_size > 1024 * 1024
        ):
            raise AcceptanceError("native-audio fixture is unsafe or unavailable")
        payload = bytearray()
        while len(payload) <= before.st_size:
            chunk = os.read(source_fd, min(64 * 1024, before.st_size + 1 - len(payload)))
            if not chunk:
                break
            payload.extend(chunk)
        after = os.fstat(source_fd)
        identity_before = (
            before.st_dev,
            before.st_ino,
            before.st_mode,
            before.st_nlink,
            before.st_uid,
            before.st_size,
            before.st_mtime_ns,
            before.st_ctime_ns,
        )
        identity_after = (
            after.st_dev,
            after.st_ino,
            after.st_mode,
            after.st_nlink,
            after.st_uid,
            after.st_size,
            after.st_mtime_ns,
            after.st_ctime_ns,
        )
        if (
            identity_before != identity_after
            or len(payload) != before.st_size
            or hashlib.sha256(payload).hexdigest() != AUDIO_FIXTURE_SHA256
        ):
            raise AcceptanceError("native-audio fixture failed integrity validation")
    finally:
        os.close(source_fd)
    destination_fd = os.open(
        path,
        os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC,
        0o400,
    )
    try:
        view = memoryview(payload)
        while view:
            written = os.write(destination_fd, view)
            if written <= 0:
                raise AcceptanceError("native-audio fixture staging failed")
            view = view[written:]
        os.fchmod(destination_fd, 0o400)
        os.fsync(destination_fd)
    finally:
        os.close(destination_fd)


def run_route(host: pathlib.Path, route: Route) -> None:
    validate_route(route)
    with tempfile.TemporaryDirectory(prefix="ygg-provider-acceptance-") as temporary:
        root = pathlib.Path(temporary)
        home = root / "home"
        workspace = root / "workspace"
        sessions = workspace / "sessions"
        home.mkdir(mode=0o700)
        workspace.mkdir(mode=0o700)
        sessions.mkdir(mode=0o700)
        canary = f"YGG_ACCEPTANCE_{uuid.uuid4().hex.upper()}"
        request_id = f"accept-{uuid.uuid4().hex}"
        run_id = f"run-{uuid.uuid4().hex}"
        request: dict[str, object] = {
            "protocol_version": PROTOCOL_VERSION,
            "request_id": request_id,
            "command": "run",
            "run_id": run_id,
            "workspace": str(workspace),
            "session_dir": str(sessions),
            "model": route.model,
            "provider": route.provider,
            "base_url": route.base_url,
            "api_key": route.api_key,
            "provider_mode": route.provider_mode,
            "context_window_tokens": 32_768,
            "max_output_tokens": 1_024,
            "supports_reasoning": False,
            "allow_file_mutation": False,
            "context_files": False,
            "offline": True,
            "max_turns": 6,
        }
        if route.audio:
            audio = workspace / "acceptance.wav"
            stage_audio_fixture(audio)
            request.update(
                {
                    "prompt": (
                        "Transcribe the three-word code spoken in the attached audio. "
                        "Reply with only those three lowercase words separated by spaces."
                    ),
                    "tools": [],
                    "input_modalities": ["audio"],
                    "media": [{"type": "audio", "path": str(audio)}],
                }
            )
            expected = AUDIO_EXPECTED_TRANSCRIPT
            require_tool = False
        else:
            canary_file = workspace / "acceptance-canary.txt"
            canary_file.write_text(canary + "\n", encoding="utf-8")
            canary_file.chmod(0o400)
            request.update(
                {
                    "prompt": (
                        "Use the read tool to read acceptance-canary.txt. "
                        "Then finish your response with the exact token from that file."
                    ),
                    "tools": ["read"],
                }
            )
            expected = canary
            require_tool = True
        events = exchange(host, home, request, require_audio=route.audio)
        require_success(
            events,
            expected,
            require_tool=require_tool,
            normalize_transcript=route.audio,
        )


def routes_from_environment() -> list[Route]:
    return [
        Route(
            label="OpenAI Responses",
            provider="acceptance-openai-responses",
            model=required_environment("YGG_ACCEPTANCE_OPENAI_RESPONSES_MODEL"),
            base_url="https://api.openai.com/v1",
            api_key=required_environment("YGG_ACCEPTANCE_OPENAI_API_KEY"),
            provider_mode="openai-responses",
        ),
        Route(
            label="Anthropic Messages",
            provider="acceptance-anthropic-messages",
            model=required_environment("YGG_ACCEPTANCE_ANTHROPIC_MODEL"),
            base_url="https://api.anthropic.com/v1",
            api_key=required_environment("YGG_ACCEPTANCE_ANTHROPIC_API_KEY"),
            provider_mode="anthropic-messages",
        ),
        Route(
            label="OpenAI Chat",
            provider=required_environment("YGG_ACCEPTANCE_OPENAI_CHAT_PROVIDER"),
            model=required_environment("YGG_ACCEPTANCE_OPENAI_CHAT_MODEL"),
            base_url=required_environment("YGG_ACCEPTANCE_OPENAI_CHAT_BASE_URL"),
            api_key=required_environment("YGG_ACCEPTANCE_OPENAI_CHAT_API_KEY"),
            provider_mode="openai-compatible",
        ),
        Route(
            label="native audio",
            provider=required_environment("YGG_ACCEPTANCE_AUDIO_PROVIDER"),
            model=required_environment("YGG_ACCEPTANCE_AUDIO_MODEL"),
            base_url=required_environment("YGG_ACCEPTANCE_AUDIO_BASE_URL"),
            api_key=required_environment("YGG_ACCEPTANCE_AUDIO_API_KEY"),
            provider_mode="openai-compatible",
            audio=True,
        ),
    ]


def stage_host(source: pathlib.Path, destination: pathlib.Path) -> pathlib.Path:
    required_flags = ("O_CLOEXEC", "O_NOFOLLOW")
    if any(not hasattr(os, name) for name in required_flags):
        raise AcceptanceError("candidate host staging is unsupported on this platform")
    source_fd = os.open(source, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW)
    try:
        before = os.fstat(source_fd)
        if (
            not stat.S_ISREG(before.st_mode)
            or before.st_nlink != 1
            or before.st_uid != os.geteuid()
            or before.st_mode & 0o111 == 0
            or before.st_mode & 0o022 != 0
            or before.st_size <= 0
            or before.st_size > MAX_HOST_BYTES
        ):
            raise AcceptanceError("candidate host binary is unsafe or unavailable")
        destination_fd = os.open(
            destination,
            os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC,
            0o700,
        )
        try:
            remaining = before.st_size
            while remaining:
                chunk = os.read(source_fd, min(1024 * 1024, remaining))
                if not chunk:
                    raise AcceptanceError("candidate host changed while it was staged")
                view = memoryview(chunk)
                while view:
                    written = os.write(destination_fd, view)
                    if written <= 0:
                        raise AcceptanceError("candidate host staging failed")
                    view = view[written:]
                remaining -= len(chunk)
            if os.read(source_fd, 1):
                raise AcceptanceError("candidate host changed while it was staged")
            os.fchmod(destination_fd, 0o700)
            os.fsync(destination_fd)
        finally:
            os.close(destination_fd)
        after = os.fstat(source_fd)
        identity_before = (
            before.st_dev,
            before.st_ino,
            before.st_mode,
            before.st_nlink,
            before.st_uid,
            before.st_size,
            before.st_mtime_ns,
            before.st_ctime_ns,
        )
        identity_after = (
            after.st_dev,
            after.st_ino,
            after.st_mode,
            after.st_nlink,
            after.st_uid,
            after.st_size,
            after.st_mtime_ns,
            after.st_ctime_ns,
        )
        if identity_before != identity_after:
            destination.unlink(missing_ok=True)
            raise AcceptanceError("candidate host changed while it was staged")
    finally:
        os.close(source_fd)
    return destination


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: provider-acceptance.py PATH_TO_YGG_HOST", file=sys.stderr)
        return 2
    source_host = pathlib.Path(sys.argv[1])
    try:
        with tempfile.TemporaryDirectory(prefix="ygg-acceptance-host-") as staging:
            host = stage_host(source_host, pathlib.Path(staging) / "ygg-host")
            routes = routes_from_environment()
            for route in routes:
                try:
                    run_route(host, route)
                except AcceptanceError as error:
                    print(f"{route.label} acceptance failed: {error}", file=sys.stderr)
                    return 1
                print(f"{route.label} acceptance passed for {route.provider}:{route.model}")
    except AcceptanceError as error:
        print(str(error), file=sys.stderr)
        return 1
    except OSError:
        print("provider acceptance encountered a local operating-system failure", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
