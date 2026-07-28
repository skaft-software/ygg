import io
import json
import threading
import unittest

from ygg_extension import Extension, Logger, RpcError


API_VERSION = "0.1"


def initialize(*, tools=None, commands=None, **contributes):
    manifest = {
        "tools": [] if tools is None else tools,
        "commands": [] if commands is None else commands,
    }
    manifest.update(contributes)
    return {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {"api_version": API_VERSION, "contributes": manifest},
    }


def request(request_id, method, params=None):
    return {
        "jsonrpc": "2.0",
        "id": request_id,
        "method": method,
        "params": {} if params is None else params,
    }


def decode_lines(stream):
    return [json.loads(line) for line in stream.getvalue().splitlines()]


class DuplexInput:
    """A tiny in-memory host for testing extension-originated requests."""

    def __init__(self, messages):
        self._lines = [json.dumps(message) + "\n" for message in messages]
        self._condition = threading.Condition()
        self.writes = []

    def readline(self):
        with self._condition:
            while not self._lines:
                self._condition.wait(timeout=1)
                if not self._lines:
                    return ""
            return self._lines.pop(0)

    def append(self, message):
        with self._condition:
            self._lines.append(json.dumps(message) + "\n")
            self._condition.notify_all()

    def prepend(self, message):
        with self._condition:
            self._lines.insert(0, json.dumps(message) + "\n")
            self._condition.notify_all()


class HostWriter(io.StringIO):
    def __init__(self, reader):
        super().__init__()
        self.reader = reader
        self.writes = []

    def write(self, value):
        result = super().write(value)
        for line in value.splitlines():
            message = json.loads(line)
            self.writes.append(message)
            if message.get("method") == "confirmation/request":
                self.reader.prepend(
                    {
                        "jsonrpc": "2.0",
                        "id": message["id"],
                        "result": {"confirmed": True},
                    }
                )
        return result


class ExtensionTests(unittest.TestCase):
    def test_tool_handshake_dispatch_and_shutdown(self):
        input_stream = io.StringIO(
            "\n".join(
                [
                    json.dumps(initialize(tools=["hello_world"])),
                    json.dumps(
                        request(
                            2,
                            "tool/call",
                            {
                                "name": "hello_world",
                                "arguments": {"name": "Ada"},
                                "context": {},
                            },
                        )
                    ),
                    json.dumps(request(3, "shutdown")),
                ]
            )
            + "\n"
        )
        output = io.StringIO()
        diagnostics = io.StringIO()
        extension = Extension(stdin=input_stream, stdout=output, stderr=diagnostics)

        @extension.tool(name="hello_world", description="Greet someone")
        def hello(args):
            return {"content": f"Hello, {args['name']}!"}

        extension.run()
        messages = decode_lines(output)
        self.assertEqual([message["id"] for message in messages], [1, 2, 3])
        self.assertEqual(messages[0]["result"]["api_version"], API_VERSION)
        self.assertEqual(messages[1]["result"]["content"], "Hello, Ada!")
        self.assertFalse(extension.running)

    def test_manifest_mismatch_is_rejected_during_initialize(self):
        output = io.StringIO()
        extension = Extension(
            stdin=io.StringIO(json.dumps(initialize()) + "\n"),
            stdout=output,
            stderr=io.StringIO(),
        )

        @extension.tool(name="not_declared", description="Unexpected")
        def unexpected(args):
            return {"content": "never called"}

        extension.run()
        messages = decode_lines(output)
        self.assertEqual(messages[0]["id"], 1)
        self.assertEqual(messages[0]["error"]["code"], -32602)
        self.assertFalse(extension.initialized)

    def test_unknown_method_and_malformed_input_use_rpc_errors(self):
        output = io.StringIO()
        lines = [
            "not json",
            json.dumps(initialize()),
            json.dumps(request(2, "not/a/method")),
        ]
        extension = Extension(
            stdin=io.StringIO("\n".join(lines) + "\n"),
            stdout=output,
            stderr=io.StringIO(),
        )
        extension.run()
        messages = decode_lines(output)
        self.assertEqual(messages[0]["error"]["code"], -32700)
        self.assertIsNone(messages[0]["id"])
        self.assertEqual(messages[1]["result"]["api_version"], API_VERSION)
        self.assertEqual(messages[2]["error"]["code"], -32601)

    def test_notification_stays_on_protocol_stream_and_logging_stays_on_stderr(self):
        output = io.StringIO()
        diagnostics = io.StringIO()
        extension = Extension(
            stdin=io.StringIO(
                "\n".join(
                    [
                        json.dumps(
                            initialize(tools=["announce"], notifications=True)
                        ),
                        json.dumps(
                            request(
                                2,
                                "tool/call",
                                {"name": "announce", "arguments": {}, "context": {}},
                            )
                        ),
                    ]
                )
                + "\n"
            ),
            stdout=output,
            stderr=diagnostics,
        )

        @extension.tool(name="announce", description="Announce")
        def announce(args):
            extension.notify("ready", level="success")
            return {"content": "done"}

        extension.run()
        messages = decode_lines(output)
        self.assertEqual(messages[0]["result"]["api_version"], API_VERSION)
        self.assertEqual(messages[1]["method"], "notification")
        self.assertEqual(messages[2]["result"]["content"], "done")
        self.assertTrue(all(line.startswith("{") for line in diagnostics.getvalue().splitlines()))
        for line in diagnostics.getvalue().splitlines():
            self.assertEqual(json.loads(line)["level"], "info")

    def test_extension_request_correlation_allows_confirmation_inside_tool(self):
        reader = DuplexInput([initialize(tools=["confirm_me"], confirmations=True)])
        writer = HostWriter(reader)
        extension = Extension(stdin=reader, stdout=writer, stderr=io.StringIO())

        @extension.tool(name="confirm_me", description="Ask before continuing")
        def confirm_me(args, context):
            self.assertEqual(context, {})
            return {
                "content": "approved" if extension.confirm("Continue?") else "denied",
            }

        def enqueue_tool_call():
            reader.append(
                request(
                    2,
                    "tool/call",
                    {"name": "confirm_me", "arguments": {}, "context": {}},
                )
            )
            # The tool result is followed by a graceful host shutdown.
            reader.append(request(3, "shutdown"))

        enqueue_tool_call()
        extension.run()
        messages = writer.writes
        self.assertEqual(messages[1]["method"], "confirmation/request")
        self.assertEqual(messages[2]["result"]["content"], "approved")
        self.assertEqual(messages[3]["result"], {})

    def test_eof_is_a_clean_shutdown(self):
        output = io.StringIO()
        diagnostics = io.StringIO()
        extension = Extension(
            stdin=io.StringIO(json.dumps(initialize()) + "\n"),
            stdout=output,
            stderr=diagnostics,
        )
        extension.run()
        self.assertFalse(extension.running)
        self.assertTrue(any("stdin closed" in line for line in diagnostics.getvalue().splitlines()))

    def test_contribution_defaults_and_handlers(self):
        output = io.StringIO()
        messages = [
            initialize(
                hooks=["before_prompt"],
                ui=["status"],
                context=True,
                tool_renderers=["inspect"],
            ),
            request(2, "hook/run", {"hook": "before_prompt", "payload": {}, "context": {}}),
            request(3, "context/collect", {"prompt": "hi", "context": {}}),
            request(4, "status/collect", {"surface": "status", "context": {}}),
            request(
                5,
                "tool/render",
                {"name": "inspect", "arguments": {}, "output": "ok", "context": {}},
            ),
        ]
        extension = Extension(
            stdin=io.StringIO("\n".join(json.dumps(message) for message in messages) + "\n"),
            stdout=output,
            stderr=io.StringIO(),
        )

        @extension.hook("before_prompt")
        def hook(payload, context):
            return {"context": [{"label": "hook", "content": "active"}]}

        @extension.context
        def collect(params):
            return [{"label": "context", "content": params["prompt"]}]

        @extension.status("status")
        def status(params):
            return {"surface": params["surface"], "text": "ready"}

        @extension.renderer("inspect")
        def render(params):
            return {"segments": [{"text": params["output"], "style_role": None}]}

        extension.run()
        replies = decode_lines(output)
        self.assertEqual(replies[1]["result"]["context"][0]["label"], "hook")
        self.assertEqual(replies[2]["result"][0]["content"], "hi")
        self.assertEqual(replies[3]["result"]["text"], "ready")
        self.assertEqual(replies[4]["result"]["segments"][0]["text"], "ok")


class LoggerTests(unittest.TestCase):
    def test_logger_is_json_lines(self):
        stream = io.StringIO()
        Logger(stream).warning("diagnostic", request_id=4)
        entry = json.loads(stream.getvalue())
        self.assertEqual(entry, {"level": "warning", "message": "diagnostic", "request_id": 4})


class RpcErrorTests(unittest.TestCase):
    def test_error_response_preserves_code_and_data(self):
        error = RpcError.from_response(
            {"error": {"code": -32001, "message": "failed", "data": {"reason": "x"}}}
        )
        self.assertEqual(error.code, -32001)
        self.assertEqual(error.data, {"reason": "x"})


if __name__ == "__main__":
    unittest.main()
