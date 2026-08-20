"""Realistic network-free provider fixture with a tiny provider-owned JSON store."""

import json
from pathlib import Path
import threading

from agent.memory_provider import MemoryProvider, RecallStatus


class OfflineRecallProvider(MemoryProvider):
    def __init__(self):
        self._path = None
        self._items = []
        self._lock = threading.Lock()
        self._last_count = 0
        self._closed = False

    @property
    def name(self):
        return "offline-recall"

    def is_available(self):
        return True

    def initialize(self, session_id, **kwargs):
        del session_id
        home = Path(kwargs["hermes_home"])
        self._path = home / "offline-recall-fixture.json"
        if self._path.is_file():
            value = json.loads(self._path.read_text(encoding="utf-8"))
            if isinstance(value, list):
                self._items = [str(item)[:4096] for item in value[:100]]

    def system_prompt_block(self):
        return "An explicitly selected offline lexical memory provider is available."

    def prefetch(self, query, *, session_id=""):
        del session_id
        words = {word.lower() for word in query.split() if len(word) > 2}
        with self._lock:
            matches = [
                item for item in self._items if words.intersection(item.lower().split())
            ][:5]
        self._last_count = len(matches)
        return "\n".join(f"Offline memory: {item}" for item in matches)

    def recall_status(self):
        return RecallStatus("Offline recall", self._last_count)

    def sync_turn(self, user_content, assistant_content, *, session_id="", messages=None):
        del session_id, messages
        if user_content and assistant_content:
            self._append(f"Turn: {user_content[:1000]} | {assistant_content[:1000]}")

    def get_tool_schemas(self):
        return [
            {
                "name": "recall_offline",
                "description": "Search the provider-owned offline lexical store",
                "parameters": {
                    "type": "object",
                    "properties": {"query": {"type": "string", "maxLength": 4096}},
                    "required": ["query"],
                    "additionalProperties": False,
                },
            },
            {
                "name": "remember_offline",
                "description": "Commit one item to the provider-owned offline store",
                "parameters": {
                    "type": "object",
                    "properties": {"content": {"type": "string", "maxLength": 4096}},
                    "required": ["content"],
                    "additionalProperties": False,
                },
            },
        ]

    def handle_tool_call(self, tool_name, args, **kwargs):
        del kwargs
        if tool_name == "remember_offline":
            content = str(args.get("content", ""))[:4096]
            self._append(content)
            return json.dumps({"committed": True, "items": 1, "bytes": len(content.encode("utf-8"))})
        query = str(args.get("query", ""))
        result = self.prefetch(query)
        return json.dumps({"items": result.splitlines() if result else [], "committed": False})

    def on_session_end(self, messages):
        if messages:
            self._append(f"Session ended after {len(messages)} bounded messages")

    def shutdown(self):
        self._closed = True

    def _append(self, value):
        if self._path is None:
            raise RuntimeError("provider not initialized")
        with self._lock:
            self._items.append(value)
            self._items = self._items[-100:]
            self._path.parent.mkdir(parents=True, exist_ok=True)
            temporary = self._path.with_suffix(".tmp")
            temporary.write_text(json.dumps(self._items, ensure_ascii=False), encoding="utf-8")
            temporary.replace(self._path)


def register(ctx):
    ctx.register_memory_provider(OfflineRecallProvider())
