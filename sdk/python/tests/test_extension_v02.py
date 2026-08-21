import base64
import hashlib
import io
import json
import queue
import threading
import time
import unittest

from ygg_extension import (
    MAX_INPUT_PROMPT_BYTES,
    MAX_INPUT_VALUE_BYTES,
    MAX_SECRET_VALUE_BYTES,
    REQUEST_CANCELLED,
    CancelledError,
    Extension,
    RpcError,
    audio_content,
    current_cancellation,
    current_request_id,
    image_content,
    text_content,
    tool_result,
)


def rpc_request(request_id, method, params=None):
    return {
        "jsonrpc": "2.0",
        "id": request_id,
        "method": method,
        "params": {} if params is None else params,
    }


def initialize_v02(
    *,
    tools=None,
    commands=None,
    required=None,
    optional=None,
    max_concurrent_requests=4,
    **contributes,
):
    manifest = {
        "tools": [] if tools is None else tools,
        "commands": [] if commands is None else commands,
    }
    manifest.update(contributes)
    return rpc_request(
        1,
        "initialize",
        {
            "api_version": "0.2",
            "contributes": manifest,
            "protocol": {
                "version": "0.2",
                "required_features": (
                    ["request_cancellation", "content_parts"]
                    if required is None
                    else required
                ),
                "optional_features": (
                    [
                        "request_progress",
                        "artifacts",
                        "lifecycle_events",
                        "policy_intents",
                        "dynamic_tools",
                    ]
                    if optional is None
                    else optional
                ),
                "limits": {"max_concurrent_requests": max_concurrent_requests},
            },
        },
    )


class QueueReader:
    _EOF = object()

    def __init__(self):
        self._lines = queue.Queue()

    def feed(self, message):
        self._lines.put(json.dumps(message, separators=(",", ":")) + "\n")

    def close(self):
        self._lines.put(self._EOF)

    def readline(self):
        value = self._lines.get()
        return "" if value is self._EOF else value


class RecordingWriter:
    def __init__(self, reader, responder=None, write_delay=0.0):
        self.reader = reader
        self.responder = responder
        self.write_delay = write_delay
        self.messages = []
        self.writer_threads = []
        self.concurrent_write = False
        self._writing = False
        self._condition = threading.Condition()

    def write(self, value):
        with self._condition:
            if self._writing:
                self.concurrent_write = True
            self._writing = True
        try:
            if self.write_delay:
                time.sleep(self.write_delay)
            decoded = [json.loads(line) for line in value.splitlines() if line]
            with self._condition:
                self.messages.extend(decoded)
                self.writer_threads.extend(
                    [threading.current_thread().name for _ in decoded]
                )
                self._condition.notify_all()
            for message in decoded:
                if self.responder is not None:
                    response = self.responder(message)
                    if response is not None:
                        self.reader.feed(response)
            return len(value)
        finally:
            with self._condition:
                self._writing = False

    def flush(self):
        return None

    def wait_for(self, predicate, timeout=2.0):
        deadline = time.monotonic() + timeout
        with self._condition:
            while True:
                for message in self.messages:
                    if predicate(message):
                        return message
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise AssertionError(f"timed out waiting for message; got {self.messages!r}")
                self._condition.wait(remaining)

    def matching(self, predicate):
        with self._condition:
            return [message for message in self.messages if predicate(message)]


class RunningExtension:
    def __init__(self, extension, responder=None, write_delay=0.0):
        self.extension = extension
        self.reader = QueueReader()
        self.writer = RecordingWriter(
            self.reader,
            responder=responder,
            write_delay=write_delay,
        )
        self.thread = threading.Thread(
            target=extension.run,
            kwargs={"stdin": self.reader, "stdout": self.writer},
            daemon=True,
        )

    def start(self, initialize):
        self.thread.start()
        self.reader.feed(initialize)
        return self.writer.wait_for(lambda message: message.get("id") == 1)

    def shutdown(self, request_id=900):
        self.reader.feed(rpc_request(request_id, "shutdown"))
        response = self.writer.wait_for(lambda message: message.get("id") == request_id)
        self.reader.close()
        self.thread.join(timeout=2.0)
        if self.thread.is_alive():
            raise AssertionError("extension did not finish shutdown")
        return response


class NegotiationTests(unittest.TestCase):
    def test_api_0_1_initialize_wire_remains_literal_compatible(self):
        input_stream = io.StringIO(
            '{"jsonrpc":"2.0","id":1,"method":"initialize","params":'
            '{"api_version":"0.1","contributes":{"tools":[],"commands":[]}}}\n'
        )
        output = io.StringIO()
        extension = Extension(
            api_version="0.1",
            stdin=input_stream,
            stdout=output,
            stderr=io.StringIO(),
        )
        extension.run()
        self.assertEqual(
            output.getvalue(),
            '{"jsonrpc":"2.0","id":1,"result":'
            '{"api_version":"0.1","tools":[],"commands":[]}}\n',
        )

    def test_negotiates_supported_subset_caps_concurrency_and_subscriptions(self):
        extension = Extension(api_version="0.2", max_concurrent_requests=2)

        @extension.on_lifecycle("turn_settled")
        def settled(params):
            pass

        host = RunningExtension(extension)
        reply = host.start(
            initialize_v02(
                optional=[
                    "request_progress",
                    "artifacts",
                    "lifecycle_events",
                    "policy_intents",
                    "dynamic_tools",
                    "future_optional_feature",
                ],
                max_concurrent_requests=12,
            )
        )
        protocol = reply["result"]["protocol"]
        self.assertEqual(protocol["version"], "0.2")
        self.assertEqual(
            protocol["features"],
            [
                "request_cancellation",
                "content_parts",
                "request_progress",
                "artifacts",
                "lifecycle_events",
                "policy_intents",
                "dynamic_tools",
            ],
        )
        self.assertEqual(protocol["limits"], {"max_concurrent_requests": 2})
        self.assertEqual(protocol["lifecycle_events"], ["turn/settled"])
        self.assertEqual(extension.negotiated_concurrency, 2)
        host.shutdown()

    def test_missing_required_feature_rejects_candidate(self):
        extension = Extension(
            api_version="0.2",
            supported_features=["content_parts"],
        )
        host = RunningExtension(extension)
        reply = host.start(initialize_v02())
        self.assertEqual(reply["error"]["code"], -32000)
        self.assertEqual(
            reply["error"]["data"],
            {"unsupported_features": ["request_cancellation"]},
        )
        self.assertFalse(extension.initialized)
        host.reader.close()
        host.thread.join(timeout=2.0)

    def test_lifecycle_feature_is_not_advertised_without_a_subscription(self):
        extension = Extension(api_version="0.2")
        host = RunningExtension(extension)
        reply = host.start(initialize_v02())
        protocol = reply["result"]["protocol"]
        self.assertNotIn("lifecycle_events", protocol["features"])
        self.assertNotIn("lifecycle_events", protocol)
        host.shutdown()

    def test_invalid_protocol_limit_is_rejected(self):
        extension = Extension(api_version="0.2")
        host = RunningExtension(extension)
        reply = host.start(initialize_v02(max_concurrent_requests=True))
        self.assertEqual(reply["error"]["code"], -32602)
        self.assertFalse(extension.initialized)
        host.reader.close()
        host.thread.join(timeout=2.0)


class CancellationAndConcurrencyTests(unittest.TestCase):
    def test_admission_queue_rejects_overflow_without_blocking_reader(self):
        extension = Extension(
            api_version="0.2",
            max_concurrent_requests=1,
            max_pending_requests=1,
        )
        started = threading.Event()
        release = threading.Event()
        overflow_called = threading.Event()

        @extension.tool(name="bounded", description="Bounded")
        def bounded(args):
            if args["kind"] == "first":
                started.set()
                release.wait(1.0)
                return "first"
            overflow_called.set()
            return "overflow"

        host = RunningExtension(extension)
        host.start(initialize_v02(tools=["bounded"], max_concurrent_requests=1))
        host.reader.feed(
            rpc_request(
                10,
                "tool/call",
                {"name": "bounded", "arguments": {"kind": "first"}, "context": {}},
            )
        )
        self.assertTrue(started.wait(1.0))
        host.reader.feed(
            rpc_request(
                11,
                "tool/call",
                {"name": "bounded", "arguments": {"kind": "overflow"}, "context": {}},
            )
        )
        overflow = host.writer.wait_for(lambda message: message.get("id") == 11)
        self.assertEqual(overflow["error"]["message"], "extension request queue is full")
        self.assertFalse(overflow_called.is_set())
        release.set()
        host.writer.wait_for(lambda message: message.get("id") == 10)
        host.shutdown()

    def test_running_handler_observes_cancel_control_frame(self):
        extension = Extension(api_version="0.2")
        started = threading.Event()
        saw_ambient = []

        @extension.tool(name="wait", description="Wait cooperatively")
        def wait(args):
            token = current_cancellation()
            saw_ambient.append((token is extension.cancellation, current_request_id()))
            started.set()
            self.assertTrue(token.wait(1.0))
            token.raise_if_cancelled()

        host = RunningExtension(extension)
        host.start(initialize_v02(tools=["wait"]))
        host.reader.feed(
            rpc_request(10, "tool/call", {"name": "wait", "arguments": {}, "context": {}})
        )
        self.assertTrue(started.wait(1.0))
        host.reader.feed(
            {
                "jsonrpc": "2.0",
                "method": "$/cancelRequest",
                "params": {"id": 10, "reason": "user"},
            }
        )
        reply = host.writer.wait_for(lambda message: message.get("id") == 10)
        self.assertEqual(reply["error"]["code"], REQUEST_CANCELLED)
        self.assertEqual(reply["error"]["data"], {"reason": "user"})
        self.assertEqual(saw_ambient, [(True, 10)])
        host.shutdown()

    def test_queued_cancellation_never_invokes_handler(self):
        extension = Extension(api_version="0.2", max_concurrent_requests=1)
        blocking_started = threading.Event()
        release = threading.Event()
        queued_called = threading.Event()

        @extension.tool(name="work", description="Bounded work")
        def work(args):
            if args["kind"] == "blocking":
                blocking_started.set()
                release.wait(1.0)
                return "first"
            queued_called.set()
            return "second"

        host = RunningExtension(extension)
        host.start(initialize_v02(tools=["work"], max_concurrent_requests=1))
        host.reader.feed(
            rpc_request(
                10,
                "tool/call",
                {"name": "work", "arguments": {"kind": "blocking"}, "context": {}},
            )
        )
        self.assertTrue(blocking_started.wait(1.0))
        host.reader.feed(
            rpc_request(
                11,
                "tool/call",
                {"name": "work", "arguments": {"kind": "queued"}, "context": {}},
            )
        )
        host.reader.feed(
            {
                "jsonrpc": "2.0",
                "method": "$/cancelRequest",
                "params": {"id": 11, "reason": "superseded"},
            }
        )
        release.set()
        first = host.writer.wait_for(lambda message: message.get("id") == 10)
        second = host.writer.wait_for(lambda message: message.get("id") == 11)
        self.assertEqual(first["result"]["content"], [text_content("first")])
        self.assertEqual(second["error"]["code"], REQUEST_CANCELLED)
        self.assertFalse(queued_called.is_set())
        host.shutdown()

    def test_bounded_handlers_overlap_but_only_writer_thread_touches_stdout(self):
        extension = Extension(api_version="0.2", max_concurrent_requests=2)
        barrier = threading.Barrier(2)

        @extension.tool(name="parallel", description="Overlap")
        def parallel(args):
            barrier.wait(timeout=1.0)
            return args["value"]

        host = RunningExtension(extension, write_delay=0.005)
        host.start(initialize_v02(tools=["parallel"], max_concurrent_requests=2))
        for request_id in (10, 11):
            host.reader.feed(
                rpc_request(
                    request_id,
                    "tool/call",
                    {
                        "name": "parallel",
                        "arguments": {"value": request_id},
                        "context": {},
                    },
                )
            )
        host.writer.wait_for(lambda message: message.get("id") == 10)
        host.writer.wait_for(lambda message: message.get("id") == 11)
        self.assertFalse(host.writer.concurrent_write)
        self.assertEqual(set(host.writer.writer_threads), {"ygg-extension-writer"})
        host.shutdown()

    def test_normal_result_wins_before_late_cancellation(self):
        extension = Extension(api_version="0.2")

        @extension.tool(name="fast", description="Finish")
        def fast(args):
            return "done"

        host = RunningExtension(extension)
        host.start(initialize_v02(tools=["fast"]))
        host.reader.feed(
            rpc_request(10, "tool/call", {"name": "fast", "arguments": {}, "context": {}})
        )
        host.writer.wait_for(lambda message: message.get("id") == 10)
        host.reader.feed(
            {
                "jsonrpc": "2.0",
                "method": "$/cancelRequest",
                "params": {"id": 10, "reason": "late"},
            }
        )
        time.sleep(0.05)
        self.assertEqual(
            len(host.writer.matching(lambda message: message.get("id") == 10)),
            1,
        )
        host.shutdown()

    def test_result_serialization_failure_is_not_reported_as_cancellation(self):
        extension = Extension(
            api_version="0.2",
            max_message_bytes=512,
            stderr=io.StringIO(),
        )

        @extension.tool(name="oversized", description="Return an oversized result")
        def oversized(args):
            return "x" * 600

        host = RunningExtension(extension)
        host.start(initialize_v02(tools=["oversized"]))
        host.reader.feed(
            rpc_request(10, "tool/call", {"name": "oversized", "arguments": {}, "context": {}})
        )
        reply = host.writer.wait_for(lambda message: message.get("id") == 10)
        self.assertEqual(reply["error"]["code"], -32603)
        self.assertIn("exceeds 512 bytes", reply["error"]["message"])
        self.assertNotEqual(reply["error"]["code"], REQUEST_CANCELLED)
        self.assertEqual(
            len(host.writer.matching(lambda message: message.get("id") == 10)),
            1,
        )
        host.shutdown()


class ProgressAndCorrelationTests(unittest.TestCase):
    def test_progress_sequences_are_monotonic_and_request_scoped(self):
        extension = Extension(api_version="0.2", max_concurrent_requests=2)
        barrier = threading.Barrier(2)

        @extension.tool(name="progress", description="Report progress")
        def report(args):
            barrier.wait(timeout=1.0)
            extension.progress(message=f"start {args['value']}", current=1, total=2)
            extension.progress(
                {
                    "type": "output",
                    "stream": "stdout",
                    "encoding": "utf8",
                    "data": str(args["value"]),
                }
            )
            return "done"

        host = RunningExtension(extension)
        host.start(initialize_v02(tools=["progress"], max_concurrent_requests=2))
        for request_id in (10, 11):
            host.reader.feed(
                rpc_request(
                    request_id,
                    "tool/call",
                    {
                        "name": "progress",
                        "arguments": {"value": request_id},
                        "context": {},
                    },
                )
            )
        host.writer.wait_for(lambda message: message.get("id") == 10)
        host.writer.wait_for(lambda message: message.get("id") == 11)
        progress = host.writer.matching(lambda message: message.get("method") == "$/progress")
        by_request = {}
        for message in progress:
            params = message["params"]
            by_request.setdefault(params["request_id"], []).append(params["sequence"])
        self.assertEqual(by_request, {10: [1, 2], 11: [1, 2]})
        host.shutdown()

    def test_input_is_parent_correlated_and_secret_stays_private(self):
        secret_value = "correct horse battery staple"
        diagnostics = io.StringIO()

        def responder(message):
            if message.get("method") == "input/request":
                return {
                    "jsonrpc": "2.0",
                    "id": message["id"],
                    "result": {"value": secret_value},
                }
            return None

        extension = Extension(api_version="0.2", stderr=diagnostics)
        observed = []

        @extension.tool(name="credential", description="Request ephemeral input")
        def credential(args):
            value = extension.request_input("Password:", secret=True)
            observed.append(value)
            return "input accepted"

        host = RunningExtension(extension, responder=responder)
        host.start(initialize_v02(tools=["credential"]))
        host.reader.feed(
            rpc_request(
                42,
                "tool/call",
                {"name": "credential", "arguments": {}, "context": {}},
            )
        )
        result = host.writer.wait_for(lambda message: message.get("id") == 42)["result"]
        request = host.writer.wait_for(
            lambda message: message.get("method") == "input/request"
        )
        self.assertTrue(request["id"].startswith("py:"))
        self.assertEqual(
            request["params"],
            {"prompt": "Password:", "secret": True, "parent_request_id": 42},
        )
        self.assertEqual(observed, [secret_value])
        self.assertEqual(result["content"], [{"type": "text", "text": "input accepted"}])
        self.assertNotIn(secret_value, json.dumps(host.writer.messages))
        self.assertNotIn(secret_value, diagnostics.getvalue())
        host.shutdown()

    def test_input_cancel_maps_to_none_and_malformed_response_fails_closed(self):
        def responder(message):
            if message.get("method") != "input/request":
                return None
            result = (
                {"value": None}
                if message["params"]["prompt"] == "Optional value:"
                else (
                    {"value": "x" * (MAX_INPUT_VALUE_BYTES + 1)}
                    if message["params"]["prompt"] == "Oversized value:"
                    else {"value": "answer", "unexpected": True}
                )
            )
            return {"jsonrpc": "2.0", "id": message["id"], "result": result}

        extension = Extension(api_version="0.2", stderr=io.StringIO())

        @extension.tool(name="input", description="Request input")
        def request_value(args):
            prompt = {
                "valid": "Optional value:",
                "oversized": "Oversized value:",
            }.get(args["kind"], "Malformed value:")
            return str(extension.request_input(prompt) is None).lower()

        host = RunningExtension(extension, responder=responder)
        host.start(initialize_v02(tools=["input"]))
        host.reader.feed(
            rpc_request(
                42,
                "tool/call",
                {"name": "input", "arguments": {"kind": "valid"}, "context": {}},
            )
        )
        cancelled = host.writer.wait_for(lambda message: message.get("id") == 42)
        self.assertEqual(cancelled["result"]["content"][0]["text"], "true")
        host.reader.feed(
            rpc_request(
                43,
                "tool/call",
                {"name": "input", "arguments": {"kind": "malformed"}, "context": {}},
            )
        )
        malformed = host.writer.wait_for(lambda message: message.get("id") == 43)
        self.assertEqual(malformed["error"]["code"], -32603)
        self.assertEqual(malformed["error"]["message"], "invalid input response")
        host.reader.feed(
            rpc_request(
                44,
                "tool/call",
                {"name": "input", "arguments": {"kind": "oversized"}, "context": {}},
            )
        )
        oversized = host.writer.wait_for(lambda message: message.get("id") == 44)
        self.assertEqual(oversized["error"]["code"], -32603)
        self.assertEqual(
            oversized["error"]["message"],
            "input response exceeds the 256 KiB bound",
        )
        host.shutdown()

    def test_parent_cancellation_cancels_pending_input_child(self):
        extension = Extension(api_version="0.2", stderr=io.StringIO())

        @extension.tool(name="input", description="Wait for input")
        def request_value(args):
            return extension.request_input("Value:")

        host = RunningExtension(extension)
        host.start(initialize_v02(tools=["input"]))
        host.reader.feed(
            rpc_request(
                42,
                "tool/call",
                {"name": "input", "arguments": {}, "context": {}},
            )
        )
        child = host.writer.wait_for(
            lambda message: message.get("method") == "input/request"
        )
        host.reader.feed(
            {
                "jsonrpc": "2.0",
                "method": "$/cancelRequest",
                "params": {"id": 42, "reason": "user"},
            }
        )
        child_cancel = host.writer.wait_for(
            lambda message: message.get("method") == "$/cancelRequest"
            and message.get("params", {}).get("id") == child["id"]
        )
        parent = host.writer.wait_for(lambda message: message.get("id") == 42)
        self.assertEqual(child_cancel["params"]["reason"], "user")
        self.assertEqual(parent["error"]["code"], REQUEST_CANCELLED)
        host.shutdown()

    def test_confirmation_artifact_and_result_preserve_parent_and_fidelity(self):
        def responder(message):
            if message.get("method") == "confirmation/request":
                return {"jsonrpc": "2.0", "id": message["id"], "result": {"confirmed": True}}
            if message.get("method") == "artifact/publish":
                return {
                    "jsonrpc": "2.0",
                    "id": message["id"],
                    "result": {"artifact_id": "artifact_01"},
                }
            if message.get("method") == "policy/evaluate":
                return {
                    "jsonrpc": "2.0",
                    "id": message["id"],
                    "result": {"decision": "ask", "approval_token": "a" * 64},
                }
            return None

        extension = Extension(api_version="0.2")

        output_schema = {
            "type": "object",
            "properties": {"sources": {"type": "array"}},
        }

        @extension.tool(
            name="capture",
            description="Capture",
            parameters={"type": "object"},
            output_schema=output_schema,
        )
        def capture(args):
            self.assertTrue(extension.confirm("Publish?"))
            policy = extension.evaluate_policy(
                {
                    "kind": "external_side_effect",
                    "operation": "browser.submit_form",
                    "target": {"origin": "https://example.com"},
                    "data_classes": ["user_text"],
                    "adapter_hints": {"read_only": False, "destructive": False},
                }
            )
            self.assertEqual(policy["approval_token"], "a" * 64)
            artifact_id = extension.publish_artifact(mime_type="image/png", data=b"png")
            return tool_result(
                text_content("captured"),
                image_content(artifact_id, "image/png", alt="after submit"),
                audio_content("artifact_audio", "audio/wav", transcript="spoken result"),
                structured_content={"sources": [{"title": "Example"}]},
                metadata={"cache": "miss"},
            )

        host = RunningExtension(extension, responder=responder)
        initialized = host.start(
            initialize_v02(
                tools=["capture"],
                confirmations=True,
                optional=[
                    "request_progress",
                    "artifacts",
                    "lifecycle_events",
                    "policy_intents",
                    "dynamic_tools",
                    "approvals",
                ],
            )
        )
        self.assertEqual(initialized["result"]["tools"][0]["output_schema"], output_schema)
        host.reader.feed(
            rpc_request(42, "tool/call", {"name": "capture", "arguments": {}, "context": {}})
        )
        result = host.writer.wait_for(lambda message: message.get("id") == 42)["result"]
        confirmation = host.writer.wait_for(
            lambda message: message.get("method") == "confirmation/request"
        )
        publication = host.writer.wait_for(
            lambda message: message.get("method") == "artifact/publish"
        )
        policy = host.writer.wait_for(
            lambda message: message.get("method") == "policy/evaluate"
        )
        self.assertTrue(confirmation["id"].startswith("py:"))
        self.assertEqual(len({confirmation["id"], policy["id"], publication["id"]}), 3)
        self.assertEqual(confirmation["params"]["parent_request_id"], 42)
        self.assertEqual(policy["params"]["parent_request_id"], 42)
        self.assertEqual(policy["params"]["intent"]["operation"], "browser.submit_form")
        self.assertEqual(publication["params"]["parent_request_id"], 42)
        self.assertEqual(publication["params"]["size"], 3)
        self.assertEqual(publication["params"]["sha256"], hashlib.sha256(b"png").hexdigest())
        self.assertEqual(base64.b64decode(publication["params"]["data"]["data"]), b"png")
        self.assertEqual(result["content"][1]["artifact_id"], "artifact_01")
        self.assertEqual(result["content"][2]["transcript"], "spoken result")
        self.assertEqual(result["structured_content"]["sources"][0]["title"], "Example")
        self.assertEqual(result["metadata"], {"cache": "miss"})
        host.shutdown()

    def test_approval_retry_and_secret_lookup_are_parent_scoped(self):
        token = "b" * 64
        observed = []

        def responder(message):
            method = message.get("method")
            if method == "policy/evaluate":
                observed.append((method, dict(message["params"])))
                if message["params"].get("approval_token") == token:
                    result = {"decision": "allow"}
                else:
                    result = {"decision": "ask", "approval_token": token}
                return {"jsonrpc": "2.0", "id": message["id"], "result": result}
            if method == "secret/get":
                observed.append((method, dict(message["params"])))
                if message["params"]["name"] == "browser.oversized":
                    value = "s" * (MAX_SECRET_VALUE_BYTES + 1)
                else:
                    value = "brokered-value"
                return {
                    "jsonrpc": "2.0",
                    "id": message["id"],
                    "result": {"value": value},
                }
            return None

        extension = Extension(api_version="0.2")
        intent = {
            "kind": "external_side_effect",
            "operation": "browser.submit_form",
            "target": {"origin": "https://example.com"},
        }

        @extension.tool(name="brokered", description="Use host services")
        def brokered(_args):
            first = extension.evaluate_policy(intent)
            self.assertEqual(first, {"decision": "ask", "approval_token": token})
            second = extension.evaluate_policy(intent, approval_token=token)
            self.assertEqual(second, {"decision": "allow"})
            self.assertEqual(extension.get_secret("browser.api_token"), "brokered-value")
            with self.assertRaisesRegex(RpcError, "invalid secret lookup response"):
                extension.get_secret("browser.oversized")
            return "secret consumed"

        host = RunningExtension(extension, responder=responder)
        host.start(
            initialize_v02(
                tools=["brokered"],
                optional=["policy_intents", "approvals", "secrets"],
            )
        )
        host.reader.feed(
            rpc_request(42, "tool/call", {"name": "brokered", "arguments": {}, "context": {}})
        )
        result = host.writer.wait_for(lambda message: message.get("id") == 42)["result"]
        self.assertEqual(result["content"], [{"type": "text", "text": "secret consumed"}])
        self.assertEqual(
            [method for method, _ in observed],
            ["policy/evaluate", "policy/evaluate", "secret/get", "secret/get"],
        )
        self.assertTrue(all(params["parent_request_id"] == 42 for _, params in observed))
        self.assertNotIn("approval_token", observed[0][1])
        self.assertEqual(observed[1][1]["approval_token"], token)
        self.assertEqual(observed[2][1]["name"], "browser.api_token")
        self.assertEqual(observed[3][1]["name"], "browser.oversized")
        host.shutdown()


class AgentSessionTests(unittest.TestCase):
    def test_agent_session_helpers_are_feature_gated_and_parent_scoped(self):
        responses = {
            "agent/spawn": {
                "agent_id": "agent-1",
                "agent_path": "/root/ext-task",
                "task_name": "research",
                "status": "pending",
            },
            "agent/message": {"delivered": True},
            "agent/follow_up": {"accepted": True},
            "agent/list": {"agents": [], "principal": "fixture"},
            "agent/wait": {"timed_out": False, "snapshot": {"agents": []}},
            "agent/interrupt": {"interrupt_requested": True},
        }

        def responder(message):
            method = message.get("method")
            if method not in responses:
                return None
            return {
                "jsonrpc": "2.0",
                "id": message["id"],
                "result": responses[method],
            }

        extension = Extension(api_version="0.2", stderr=io.StringIO())

        @extension.tool(name="orchestrate", description="Use child sessions")
        def orchestrate(_args):
            child = extension.spawn_agent(
                task_name="research",
                profile="review",
                fingerprint="f" * 64,
                message="Find the answer",
                idempotency_key="request-42-research",
                tools=["read", "search"],
                max_depth=1,
                max_concurrent_children=2,
                max_turns=8,
                max_tokens=None,
                max_cost_microdollars=200_000,
                max_output_bytes=8_192,
                timeout_ms=300_000,
            )
            extension.send_agent_message(child["agent_id"], "More context")
            extension.follow_up_agent(child["agent_id"], "Check the result")
            extension.list_agents()
            extension.wait_agents(timeout_ms=250)
            extension.interrupt_agent(child["agent_id"])
            return "done"

        host = RunningExtension(extension, responder=responder)
        initialized = host.start(
            initialize_v02(
                tools=["orchestrate"],
                optional=["agent_sessions"],
            )
        )
        self.assertIn("agent_sessions", initialized["result"]["protocol"]["features"])
        host.reader.feed(
            rpc_request(42, "tool/call", {"name": "orchestrate", "arguments": {}, "context": {}})
        )
        result = host.writer.wait_for(lambda message: message.get("id") == 42)
        self.assertEqual(result["result"]["content"], [text_content("done")])
        calls = host.writer.matching(
            lambda message: isinstance(message.get("method"), str)
            and message["method"].startswith("agent/")
        )
        self.assertEqual(
            [message["method"] for message in calls],
            [
                "agent/spawn",
                "agent/message",
                "agent/follow_up",
                "agent/list",
                "agent/wait",
                "agent/interrupt",
            ],
        )
        self.assertTrue(all(message["params"]["parent_request_id"] == 42 for message in calls))
        self.assertEqual(calls[0]["params"]["profile"], "review")
        self.assertEqual(calls[0]["params"]["fingerprint"], "f" * 64)
        self.assertEqual(
            calls[0]["params"]["policy"],
            {
                "tools": ["read", "search"],
                "max_depth": 1,
                "max_concurrent_children": 2,
                "max_turns": 8,
                "max_tokens": None,
                "max_cost_microdollars": 200_000,
                "max_output_bytes": 8_192,
                "timeout_ms": 300_000,
            },
        )
        host.shutdown()

    def test_agent_session_helpers_require_negotiation(self):
        extension = Extension(api_version="0.2", stderr=io.StringIO())
        host = RunningExtension(extension)
        host.start(initialize_v02(optional=[]))
        with self.assertRaisesRegex(RpcError, "agent_sessions"):
            extension.list_agents(parent_request_id=1)
        host.shutdown()


class DynamicToolTests(unittest.TestCase):
    def test_revision_snapshots_cover_publication_before_ack_replace_and_unregister(self):
        extension = Extension(api_version="0.2", stderr=io.StringIO())

        @extension.tool(name="versioned", description="Versioned tool")
        def old_handler(args):
            return f"old:{args['old']}"

        host = RunningExtension(extension)
        host.start(initialize_v02(tools=["versioned"]))
        outcomes = {}

        def replace():
            try:
                outcomes["replace"] = extension.register_tool(
                    name="versioned",
                    description="Replacement tool",
                    handler=lambda args: f"new:{args['new']}",
                )
            except BaseException as error:
                outcomes["replace_error"] = error

        replacement = threading.Thread(target=replace, daemon=True)
        replacement.start()
        registration = host.writer.wait_for(
            lambda message: message.get("method") == "tools/register"
        )

        # Host publication precedes the mutation acknowledgement. The SDK has
        # already staged epoch 1, so this call must not hit the epoch-0 handler.
        host.reader.feed(
            rpc_request(
                40,
                "tool/call",
                {
                    "name": "versioned",
                    "arguments": {"new": "before-ack"},
                    "catalog_revision": 1,
                    "context": {},
                },
            )
        )
        staged = host.writer.wait_for(lambda message: message.get("id") == 40)
        self.assertEqual(staged["result"]["content"], [text_content("new:before-ack")])

        host.reader.feed(
            {
                "jsonrpc": "2.0",
                "id": registration["id"],
                "result": {"revision": 1, "tools": ["versioned"]},
            }
        )
        replacement.join(timeout=1.0)
        self.assertFalse(replacement.is_alive())
        self.assertNotIn("replace_error", outcomes)

        host.reader.feed(
            rpc_request(
                41,
                "tool/call",
                {
                    "name": "versioned",
                    "arguments": {"old": "snapshot"},
                    "catalog_revision": 0,
                    "context": {},
                },
            )
        )
        old = host.writer.wait_for(lambda message: message.get("id") == 41)
        self.assertEqual(old["result"]["content"], [text_content("old:snapshot")])

        def unregister():
            try:
                outcomes["unregister"] = extension.unregister_tool("versioned")
            except BaseException as error:
                outcomes["unregister_error"] = error

        removal = threading.Thread(target=unregister, daemon=True)
        removal.start()
        unregistration = host.writer.wait_for(
            lambda message: message.get("method") == "tools/unregister"
        )
        host.reader.feed(
            {
                "jsonrpc": "2.0",
                "id": unregistration["id"],
                "result": {"revision": 2, "tools": []},
            }
        )
        removal.join(timeout=1.0)
        self.assertFalse(removal.is_alive())
        self.assertNotIn("unregister_error", outcomes)

        host.reader.feed(
            rpc_request(
                42,
                "tool/call",
                {
                    "name": "versioned",
                    "arguments": {"new": "stale"},
                    "catalog_revision": 1,
                    "context": {},
                },
            )
        )
        stale = host.writer.wait_for(lambda message: message.get("id") == 42)
        self.assertEqual(stale["result"]["content"], [text_content("new:stale")])

        host.reader.feed(
            rpc_request(
                43,
                "tool/call",
                {
                    "name": "versioned",
                    "arguments": {},
                    "catalog_revision": 2,
                    "context": {},
                },
            )
        )
        removed = host.writer.wait_for(lambda message: message.get("id") == 43)
        self.assertEqual(removed["error"]["code"], -32601)
        host.shutdown()

    def test_revision_history_is_bounded_and_retired_revisions_are_rejected(self):
        state = {"revision": 0}

        def responder(message):
            if message.get("method") != "tools/register":
                return None
            state["revision"] += 1
            return {
                "jsonrpc": "2.0",
                "id": message["id"],
                "result": {"revision": state["revision"], "tools": ["versioned"]},
            }

        extension = Extension(api_version="0.2", stderr=io.StringIO())

        @extension.tool(name="versioned", description="Versioned tool")
        def initial(args):
            return "revision:0"

        host = RunningExtension(extension, responder=responder)
        host.start(initialize_v02(tools=["versioned"]))
        for revision in range(1, 9):
            def replacement(args, *, revision=revision):
                return f"revision:{revision}"

            extension.register_tool(
                name="versioned",
                description=f"Version {revision}",
                handler=replacement,
            )

        host.reader.feed(
            rpc_request(
                50,
                "tool/call",
                {
                    "name": "versioned",
                    "arguments": {},
                    "catalog_revision": 0,
                    "context": {},
                },
            )
        )
        retired = host.writer.wait_for(lambda message: message.get("id") == 50)
        self.assertEqual(retired["error"]["code"], -32602)
        self.assertIn("unknown or retired", retired["error"]["message"])

        host.reader.feed(
            rpc_request(
                51,
                "tool/call",
                {
                    "name": "versioned",
                    "arguments": {},
                    "catalog_revision": 1,
                    "context": {},
                },
            )
        )
        retained = host.writer.wait_for(lambda message: message.get("id") == 51)
        self.assertEqual(retained["result"]["content"], [text_content("revision:1")])
        host.shutdown()

    def test_handler_originated_catalog_update_is_global_not_parent_correlated(self):
        requests = []

        def responder(message):
            if message.get("method") != "tools/register":
                return None
            requests.append(message)
            return {
                "jsonrpc": "2.0",
                "id": message["id"],
                "result": {"revision": 1, "tools": ["manager", "live"]},
            }

        extension = Extension(api_version="0.2")

        @extension.tool(name="manager", description="Manage the live catalog")
        def manager(args):
            update = extension.register_tool(
                name="live",
                description="Live tool",
                handler=lambda dynamic_args: dynamic_args["value"],
            )
            return f"revision:{update['revision']}"

        host = RunningExtension(extension, responder=responder)
        host.start(initialize_v02(tools=["manager"]))
        host.reader.feed(
            rpc_request(40, "tool/call", {"name": "manager", "arguments": {}, "context": {}})
        )
        reply = host.writer.wait_for(lambda message: message.get("id") == 40)
        self.assertEqual(reply["result"]["content"], [text_content("revision:1")])
        self.assertEqual(len(requests), 1)
        self.assertNotIn("parent_request_id", requests[0]["params"])
        host.shutdown()

    def test_register_and_unregister_update_handlers_without_parent_correlation(self):
        state = {"revision": 0, "tools": ["base"]}
        requests = []

        def responder(message):
            method = message.get("method")
            if method == "tools/register":
                requests.append(message)
                for definition in message["params"]["tools"]:
                    if definition["name"] not in state["tools"]:
                        state["tools"].append(definition["name"])
                state["revision"] += 1
                return {
                    "jsonrpc": "2.0",
                    "id": message["id"],
                    "result": {
                        "revision": state["revision"],
                        "tools": list(state["tools"]),
                    },
                }
            if method == "tools/unregister":
                requests.append(message)
                removed = set(message["params"]["names"])
                state["tools"] = [name for name in state["tools"] if name not in removed]
                state["revision"] += 1
                return {
                    "jsonrpc": "2.0",
                    "id": message["id"],
                    "result": {
                        "revision": state["revision"],
                        "tools": list(state["tools"]),
                    },
                }
            return None

        extension = Extension(api_version="0.2")

        @extension.tool(name="base", description="Initial tool")
        def base(args):
            return "base"

        host = RunningExtension(extension, responder=responder)
        host.start(initialize_v02(tools=["base"]))

        registered = extension.register_tool(
            name="live",
            description="Live tool",
            parameters={"type": "object"},
            handler=lambda args: f"live:{args['value']}",
        )
        self.assertEqual(registered, {"revision": 1, "tools": ["base", "live"]})
        self.assertEqual(extension.tool_catalog_revision, 1)
        self.assertEqual(requests[0]["params"]["tools"][0]["name"], "live")
        self.assertNotIn("parent_request_id", requests[0]["params"])

        host.reader.feed(
            rpc_request(
                42,
                "tool/call",
                {"name": "live", "arguments": {"value": "ok"}, "context": {}},
            )
        )
        live = host.writer.wait_for(lambda message: message.get("id") == 42)
        self.assertEqual(live["result"]["content"], [text_content("live:ok")])

        unregistered = extension.unregister_tool("live")
        self.assertEqual(unregistered, {"revision": 2, "tools": ["base"]})
        self.assertEqual(extension.tool_catalog_revision, 2)
        self.assertEqual(requests[1]["params"], {"names": ["live"]})

        host.reader.feed(
            rpc_request(43, "tool/call", {"name": "live", "arguments": {}, "context": {}})
        )
        missing = host.writer.wait_for(lambda message: message.get("id") == 43)
        self.assertEqual(missing["error"]["code"], -32601)
        host.shutdown()

    def test_dynamic_mutations_roll_back_local_handlers_on_host_rejection(self):
        def responder(message):
            if message.get("method") in {"tools/register", "tools/unregister"}:
                return {
                    "jsonrpc": "2.0",
                    "id": message["id"],
                    "error": {"code": -32602, "message": "catalog rejected"},
                }
            return None

        extension = Extension(api_version="0.2")

        @extension.tool(name="base", description="Initial tool")
        def base(args):
            return "old"

        host = RunningExtension(extension, responder=responder)
        host.start(initialize_v02(tools=["base"]))

        with self.assertRaises(RpcError) as registration:
            extension.register_tool(
                name="base",
                description="Replacement",
                handler=lambda args: "new",
            )
        self.assertEqual(registration.exception.code, -32602)
        with self.assertRaises(RpcError) as unregistration:
            extension.unregister_tool("base")
        self.assertEqual(unregistration.exception.code, -32602)
        self.assertEqual(extension.tool_catalog_revision, 0)

        host.reader.feed(
            rpc_request(
                42,
                "tool/call",
                {
                    "name": "base",
                    "arguments": {},
                    "catalog_revision": 0,
                    "context": {},
                },
            )
        )
        reply = host.writer.wait_for(lambda message: message.get("id") == 42)
        self.assertEqual(reply["result"]["content"], [text_content("old")])

        host.reader.feed(
            rpc_request(
                43,
                "tool/call",
                {
                    "name": "base",
                    "arguments": {},
                    "catalog_revision": 1,
                    "context": {},
                },
            )
        )
        discarded = host.writer.wait_for(lambda message: message.get("id") == 43)
        self.assertEqual(discarded["error"]["code"], -32602)
        self.assertIn("unknown or retired", discarded["error"]["message"])
        host.shutdown()

    def test_dynamic_tools_require_negotiation(self):
        extension = Extension(api_version="0.2")
        host = RunningExtension(extension)
        host.start(initialize_v02(optional=[]))
        with self.assertRaises(RpcError) as error:
            extension.register_tool(
                name="live",
                description="Live tool",
                handler=lambda args: "live",
            )
        self.assertEqual(error.exception.code, -32601)
        self.assertFalse(
            host.writer.matching(lambda message: message.get("method") == "tools/register")
        )
        host.reader.feed(
            rpc_request(
                42,
                "tool/call",
                {
                    "name": "live",
                    "arguments": {},
                    "catalog_revision": 0,
                    "context": {},
                },
            )
        )
        call = host.writer.wait_for(lambda message: message.get("id") == 42)
        self.assertEqual(call["error"]["code"], -32602)
        self.assertIn("requires negotiated dynamic_tools", call["error"]["message"])
        host.shutdown()


class ToolResultValidationTests(unittest.TestCase):
    def test_api_0_2_rejects_host_invalid_result_envelopes(self):
        extension = Extension(api_version="0.2", stderr=io.StringIO())

        @extension.tool(name="invalid", description="Return an invalid envelope")
        def invalid(args):
            kind = args["kind"]
            if kind == "none":
                return None
            if kind == "empty":
                return {"content": []}
            if kind == "too_many":
                return {"content": [text_content("x") for _ in range(257)]}
            if kind == "media_only":
                return {"content": [image_content("artifact_1", "image/png")]}
            if kind == "unknown_part":
                return {"content": [{"type": "text", "text": "x", "extra": True}]}
            if kind == "unknown_result":
                return {"content": [text_content("x")], "extra": True}
            if kind == "structured_without_schema":
                return {"content": [text_content("x")], "structured_content": {}}
            if kind == "invalid_is_error":
                return {"content": [text_content("x")], "is_error": "yes"}
            raise AssertionError(f"unexpected test case: {kind}")

        host = RunningExtension(extension)
        host.start(initialize_v02(tools=["invalid"]))
        cases = {
            "none": "must not be empty",
            "empty": "must not be empty",
            "too_many": "exceeds 256 parts",
            "media_only": "requires an explicit text part",
            "unknown_part": "unknown text content fields",
            "unknown_result": "unknown API 0.2 tool result fields",
            "structured_without_schema": "requires a declared output_schema",
            "invalid_is_error": "must be a boolean",
        }
        for request_id, (kind, message) in enumerate(cases.items(), start=40):
            with self.subTest(kind=kind):
                host.reader.feed(
                    rpc_request(
                        request_id,
                        "tool/call",
                        {"name": "invalid", "arguments": {"kind": kind}, "context": {}},
                    )
                )
                reply = host.writer.wait_for(
                    lambda response, request_id=request_id: response.get("id") == request_id
                )
                self.assertEqual(reply["error"]["code"], -32603)
                self.assertIn(message, reply["error"]["message"])
        host.shutdown()

    def test_output_schema_allows_error_without_structured_content(self):
        extension = Extension(api_version="0.2", stderr=io.StringIO())

        @extension.tool(
            name="structured",
            description="Return structured content",
            output_schema={"type": "object"},
        )
        def structured(args):
            if args["kind"] == "error":
                return tool_result(text_content("failed"), is_error=True)
            if args["kind"] == "exception":
                raise RuntimeError("boom")
            if args["kind"] == "valid":
                return tool_result(text_content("ok"), structured_content={"ok": True})
            return {"content": [text_content("missing")]}

        host = RunningExtension(extension)
        host.start(initialize_v02(tools=["structured"]))
        for request_id, kind in enumerate(("error", "exception", "valid"), start=60):
            host.reader.feed(
                rpc_request(
                    request_id,
                    "tool/call",
                    {"name": "structured", "arguments": {"kind": kind}, "context": {}},
                )
            )
            reply = host.writer.wait_for(
                lambda response, request_id=request_id: response.get("id") == request_id
            )
            self.assertIn("result", reply)
            if kind != "valid":
                self.assertTrue(reply["result"]["is_error"])
                self.assertNotIn("structured_content", reply["result"])

        host.reader.feed(
            rpc_request(
                63,
                "tool/call",
                {"name": "structured", "arguments": {"kind": "missing"}, "context": {}},
            )
        )
        missing = host.writer.wait_for(lambda message: message.get("id") == 63)
        self.assertEqual(missing["error"]["code"], -32603)
        self.assertIn("omitted structured_content", missing["error"]["message"])
        host.shutdown()

    def test_media_results_require_artifact_negotiation(self):
        extension = Extension(api_version="0.2", stderr=io.StringIO())

        @extension.tool(name="media", description="Return media")
        def media(args):
            return tool_result(
                text_content("image"),
                image_content("artifact_1", "image/png"),
            )

        host = RunningExtension(extension)
        host.start(initialize_v02(tools=["media"], optional=[]))
        host.reader.feed(
            rpc_request(42, "tool/call", {"name": "media", "arguments": {}, "context": {}})
        )
        reply = host.writer.wait_for(lambda message: message.get("id") == 42)
        self.assertEqual(reply["error"]["code"], -32603)
        self.assertIn("requires artifacts negotiation", reply["error"]["message"])
        host.shutdown()


class LifecycleAndDrainTests(unittest.TestCase):
    def test_lifecycle_notification_runs_off_reader_and_shutdown_drains_it(self):
        extension = Extension(api_version="0.2")
        lifecycle_started = threading.Event()
        lifecycle_release = threading.Event()
        lifecycle_finished = threading.Event()

        @extension.on_lifecycle("turn_settled")
        def turn_settled(params):
            self.assertEqual(params["outcome"], "failed")
            lifecycle_started.set()
            lifecycle_release.wait(1.0)
            lifecycle_finished.set()

        host = RunningExtension(extension)
        host.start(initialize_v02())
        host.reader.feed(
            {
                "jsonrpc": "2.0",
                "method": "turn/settled",
                "params": {"turn_id": "turn_1", "outcome": "failed"},
            }
        )
        self.assertTrue(lifecycle_started.wait(1.0))
        host.reader.feed(rpc_request(90, "shutdown"))
        time.sleep(0.05)
        self.assertFalse(host.writer.matching(lambda message: message.get("id") == 90))
        lifecycle_release.set()
        host.writer.wait_for(lambda message: message.get("id") == 90)
        self.assertTrue(lifecycle_finished.is_set())
        host.reader.close()
        host.thread.join(timeout=2.0)

    def test_shutdown_deadline_cancels_cooperative_request_before_ack(self):
        extension = Extension(
            api_version="0.2",
            shutdown_timeout=1.0,
            cancellation_grace=1.0,
        )
        started = threading.Event()

        @extension.tool(name="wait", description="Wait")
        def wait(args):
            started.set()
            extension.cancellation.wait(1.0)
            extension.cancellation.raise_if_cancelled()

        host = RunningExtension(extension)
        host.start(initialize_v02(tools=["wait"]))
        host.reader.feed(
            rpc_request(10, "tool/call", {"name": "wait", "arguments": {}, "context": {}})
        )
        self.assertTrue(started.wait(1.0))
        host.reader.feed(rpc_request(90, "shutdown", {"drain_timeout_ms": 0}))
        cancelled = host.writer.wait_for(lambda message: message.get("id") == 10)
        shutdown = host.writer.wait_for(lambda message: message.get("id") == 90)
        self.assertEqual(cancelled["error"]["code"], REQUEST_CANCELLED)
        messages = host.writer.messages
        self.assertLess(messages.index(cancelled), messages.index(shutdown))
        host.reader.close()
        host.thread.join(timeout=2.0)


class PresentationTests(unittest.TestCase):
    def test_publishes_bounded_monotonic_semantic_snapshot(self):
        extension = Extension(api_version="0.2")

        @extension.command(name="workers", description="Manage workers")
        def workers(arguments):
            return {"text": "ok"}

        host = RunningExtension(extension)
        host.start(initialize_v02(commands=["workers"], presentation=True))
        snapshot = {
            "revision": 0,
            "status": {"state": "active", "label": "1 worker"},
            "activities": [
                {
                    "id": "worker:1",
                    "kind": "delegation",
                    "state": "running",
                    "summary": "Reviewing tests",
                }
            ],
            "collection": {
                "kind": "tree",
                "title": "Workers",
                "nodes": [
                    {
                        "id": "worker:1",
                        "state": "running",
                        "label": "test-review",
                        "action_ids": ["stop"],
                    }
                ],
                "selected_node_id": "worker:1",
                "detail": {
                    "node_id": "worker:1",
                    "title": "test-review",
                    "body": "Running in a bounded child session.",
                },
            },
            "actions": [
                {
                    "id": "stop",
                    "label": "Stop worker",
                    "command": "workers",
                    "arguments": ["stop", "worker:1"],
                    "destructive": True,
                }
            ],
        }
        extension.publish_presentation(snapshot)
        update = host.writer.wait_for(
            lambda message: message.get("method") == "presentation/update"
        )
        self.assertEqual(update["params"], {"snapshot": snapshot})
        with self.assertRaises(ValueError):
            extension.publish_presentation(snapshot)
        snapshot["revision"] = 2**53
        with self.assertRaisesRegex(ValueError, "portable"):
            extension.publish_presentation(snapshot)
        snapshot["revision"] = 1
        extension.presentation(snapshot)
        host.shutdown()

    def test_handler_presentation_is_correlated_to_its_host_parent(self):
        extension = Extension(api_version="0.2")

        @extension.tool(name="publish", description="Publish owner state")
        def publish(_arguments):
            extension.publish_presentation({"revision": 0})
            return "ok"

        host = RunningExtension(extension)
        host.start(initialize_v02(tools=["publish"], presentation=True))
        host.reader.feed(
            rpc_request(
                42,
                "tool/call",
                {"name": "publish", "arguments": {}, "context": {}},
            )
        )
        update = host.writer.wait_for(
            lambda message: message.get("method") == "presentation/update"
        )
        self.assertEqual(
            update["params"],
            {"snapshot": {"revision": 0}, "parent_request_id": 42},
        )
        host.writer.wait_for(lambda message: message.get("id") == 42)
        host.shutdown()

    def test_background_presentation_carries_the_exact_host_owner_triple(self):
        extension = Extension(api_version="0.2")
        host = RunningExtension(extension)
        host.start(initialize_v02(presentation=True))
        owner = {
            "session_id": "session-owner",
            "extension_instance_id": "instance-owner",
            "process_generation": 2,
        }
        extension.publish_presentation({"revision": 0}, resource_owner=owner)
        update = host.writer.wait_for(
            lambda message: message.get("method") == "presentation/update"
        )
        self.assertEqual(
            update["params"],
            {"snapshot": {"revision": 0}, "resource_owner": owner},
        )
        with self.assertRaisesRegex(ValueError, "exact owner triple"):
            extension.publish_presentation(
                {"revision": 1}, resource_owner={"session_id": "session-owner"}
            )
        host.shutdown()

    def test_requires_api_and_manifest_declaration(self):
        extension = Extension(api_version="0.2")
        host = RunningExtension(extension)
        host.start(initialize_v02())
        with self.assertRaises(RpcError):
            extension.publish_presentation({"revision": 0})
        host.shutdown()

        extension = Extension(api_version="0.1")
        with self.assertRaises(RpcError):
            extension.publish_presentation({"revision": 0})


class HelperValidationTests(unittest.TestCase):
    def test_artifact_path_is_bounded_before_host_request(self):
        extension = Extension(api_version="0.2")
        with self.assertRaises(ValueError):
            image_content("", "image/png")
        with self.assertRaises(ValueError):
            extension.publish_artifact(
                mime_type="image/png",
                path="../escape.png",
                size=1,
                sha256="0" * 64,
                parent_request_id=1,
            )

    def test_cancelled_error_uses_lsp_code(self):
        error = CancelledError("user")
        self.assertEqual(error.code, -32800)
        self.assertEqual(error.error_object()["data"], {"reason": "user"})

    def test_api_0_1_rejects_output_schema(self):
        extension = Extension(api_version="0.1")
        with self.assertRaises(ValueError):
            extension.tool(
                name="structured",
                description="Structured",
                output_schema={"type": "object"},
            )

    def test_input_helper_validates_prompt_secret_and_message_bound(self):
        extension = Extension(api_version="0.2", max_message_bytes=16)
        with self.assertRaises(TypeError):
            extension.request_input(123, parent_request_id=1)
        with self.assertRaises(ValueError):
            extension.request_input("", parent_request_id=1)
        with self.assertRaises(ValueError):
            extension.request_input("  \t", parent_request_id=1)
        with self.assertRaises(TypeError):
            extension.request_input("prompt", secret="yes", parent_request_id=1)
        with self.assertRaises(ValueError):
            extension.request_input("é" * 9, parent_request_id=1)
        default_extension = Extension(api_version="0.2")
        with self.assertRaises(ValueError):
            default_extension.request_input(
                "x" * (MAX_INPUT_PROMPT_BYTES + 1),
                parent_request_id=1,
            )
        self.assertEqual(MAX_INPUT_VALUE_BYTES, 256 * 1024)

    def test_input_request_is_not_added_to_api_0_1(self):
        extension = Extension(api_version="0.1", stderr=io.StringIO())

        @extension.tool(name="legacy", description="Legacy tool")
        def legacy(args):
            return extension.request_input("Value:")

        host = RunningExtension(extension)
        host.start(
            rpc_request(
                1,
                "initialize",
                {
                    "api_version": "0.1",
                    "contributes": {"tools": ["legacy"], "commands": []},
                },
            )
        )
        host.reader.feed(
            rpc_request(
                42,
                "tool/call",
                {"name": "legacy", "arguments": {}, "context": {}},
            )
        )
        response = host.writer.wait_for(lambda message: message.get("id") == 42)
        self.assertEqual(response["error"]["code"], -32601)
        self.assertFalse(
            host.writer.matching(lambda message: message.get("method") == "input/request")
        )
        host.shutdown()


if __name__ == "__main__":
    unittest.main()
