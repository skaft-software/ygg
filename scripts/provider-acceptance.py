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
