"""Test doubles for the dependency-free Ygg Browse suite."""

from __future__ import annotations

import json
import queue
import threading
from typing import Any, Dict, List, Mapping, Optional


OWNER_CONTEXT = {
    "resource_owner": {
        "session_id": "session-owner",
        "extension_instance_id": "instance-owner",
        "process_generation": 1,
    },
    "workspace": "/tmp/workspace",
    "host": {},
}


class FakeLocator:
    def __init__(self, elements: Optional[List[Any]] = None, *, body_text: Optional[str] = None):
        self.elements = list(elements or [])
        self.body_text = body_text

    def count(self) -> int:
        return len(self.elements)

    def nth(self, index: int) -> Any:
        return self.elements[index]

    def inner_text(self, timeout: Optional[int] = None) -> str:
        _ = timeout
        if self.body_text is not None:
            return self.body_text
        if len(self.elements) == 1:
            return self.elements[0].inner_text()
        return ""


class FakeElement:
    def __init__(
        self,
        text: str = "",
        *,
        attrs: Optional[Mapping[str, str]] = None,
        visible: bool = True,
        enabled: bool = True,
        form: Optional["FakeElement"] = None,
        parent_text: str = "",
    ) -> None:
        self.text = text
        self.attrs = dict(attrs or {})
        self.visible = visible
        self.enabled = enabled
        self.form = form
        self.parent_text = parent_text
        self.filled: List[str] = []
        self.clicked = 0
        self.pressed: List[str] = []
        self.disposed = False

    def get_attribute(self, name: str) -> Optional[str]:
        return self.attrs.get(name)

    def inner_text(self) -> str:
        return self.text

    def is_visible(self) -> bool:
        return self.visible

    def is_enabled(self) -> bool:
        return self.enabled

    def element_handle(self) -> "FakeElement":
        return self

    def dispose(self) -> None:
        self.disposed = True

    def locator(self, selector: str) -> FakeLocator:
        if selector == "xpath=ancestor::form[1]":
            return FakeLocator([self.form] if self.form is not None else [])
        if selector == "xpath=.." and self.parent_text:
            return FakeLocator([FakeElement(self.parent_text)])
        return FakeLocator([])

    def fill(self, value: str, timeout: Optional[int] = None) -> None:
        _ = timeout
        self.filled.append(value)

    def click(self, timeout: Optional[int] = None) -> None:
        _ = timeout
        self.clicked += 1

    def press(self, key: str, timeout: Optional[int] = None) -> None:
        _ = timeout
        self.pressed.append(key)


class FakePage:
    def __init__(self, *, body: str = "", title: str = "Fixture", url: str = "https://example.test/"):
        self.body = body
        self._title = title
        self.url = url
        self.selector_elements: Dict[str, List[FakeElement]] = {}
        self.role_elements: Dict[str, List[FakeElement]] = {}
        self.text_elements: Dict[str, List[FakeElement]] = {}
        self._closed = False

    def locator(self, selector: str) -> FakeLocator:
        if selector == "body":
            return FakeLocator(body_text=self.body)
        return FakeLocator(self.selector_elements.get(selector, []))

    def get_by_role(self, role: str, name: Optional[str] = None, exact: bool = False) -> FakeLocator:
        _ = exact
        elements = list(self.role_elements.get(role, []))
        if name is not None:
            elements = [
                item
                for item in elements
                if (item.attrs.get("aria-label") or item.text or item.attrs.get("title")) == name
            ]
        return FakeLocator(elements)

    def get_by_text(self, text: str, exact: bool = False) -> FakeLocator:
        _ = exact
        return FakeLocator(self.text_elements.get(text, []))

    def title(self) -> str:
        return self._title

    def is_closed(self) -> bool:
        return self._closed

    def close(self) -> None:
        self._closed = True

    def on(self, _event: str, _handler: Any) -> None:
        pass


class MemoryProtocol:
    """Line-oriented helper that runs an Extension against queue-backed streams."""

    class Reader:
        def __init__(self) -> None:
            self.queue: "queue.Queue[Optional[str]]" = queue.Queue()

        def readline(self) -> str:
            item = self.queue.get(timeout=5)
            return "" if item is None else item

        def send(self, value: Mapping[str, Any]) -> None:
            self.queue.put(json.dumps(dict(value), separators=(",", ":")) + "\n")

        def close(self) -> None:
            self.queue.put(None)

    class Writer:
        def __init__(self) -> None:
            self.queue: "queue.Queue[Mapping[str, Any]]" = queue.Queue()
            self.buffer = ""
            self.lock = threading.Lock()

        def write(self, value: str) -> None:
            with self.lock:
                self.buffer += value
                while "\n" in self.buffer:
                    line, self.buffer = self.buffer.split("\n", 1)
                    if line:
                        self.queue.put(json.loads(line))

        def flush(self) -> None:
            pass

        def receive(self, timeout: float = 5.0) -> Mapping[str, Any]:
            return self.queue.get(timeout=timeout)

    def __init__(self, extension: Any) -> None:
        self.extension = extension
        self.reader = self.Reader()
        self.writer = self.Writer()
        self.thread = threading.Thread(
            target=extension.run,
            kwargs={"stdin": self.reader, "stdout": self.writer},
            daemon=True,
        )
        self.thread.start()

    def request(self, request_id: int, method: str, params: Mapping[str, Any]) -> Mapping[str, Any]:
        self.reader.send(
            {"jsonrpc": "2.0", "id": request_id, "method": method, "params": dict(params)}
        )
        while True:
            message = self.writer.receive()
            if message.get("id") == request_id:
                return message
            # Tests that need child requests should use receive directly.

    def close(self) -> None:
        self.reader.close()
        self.thread.join(timeout=3)
