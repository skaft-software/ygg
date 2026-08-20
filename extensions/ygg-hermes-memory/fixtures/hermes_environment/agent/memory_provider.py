"""Test-only shape-compatible subset of Hermes Agent 0.20.1's contract.

This is a conformance fixture, not an implementation distributed at runtime.
"""

from abc import ABC, abstractmethod
from dataclasses import dataclass


@dataclass(frozen=True)
class RecallStatus:
    provider_label: str
    count: int
    glyph: str = "memory"


class MemoryProvider(ABC):
    @property
    @abstractmethod
    def name(self):
        raise NotImplementedError

    @abstractmethod
    def is_available(self):
        raise NotImplementedError

    @abstractmethod
    def initialize(self, session_id, **kwargs):
        raise NotImplementedError

    def unavailable_reason(self):
        return ""

    def system_prompt_block(self):
        return ""

    def prefetch(self, query, *, session_id=""):
        return ""

    def queue_prefetch(self, query, *, session_id=""):
        return None

    def recall_status(self):
        return None

    def sync_turn(self, user_content, assistant_content, *, session_id="", messages=None):
        return None

    @abstractmethod
    def get_tool_schemas(self):
        raise NotImplementedError

    def handle_tool_call(self, tool_name, args, **kwargs):
        raise NotImplementedError

    def shutdown(self):
        return None

    def on_turn_start(self, turn_number, message, **kwargs):
        return None

    def on_session_end(self, messages):
        return None

    def on_session_switch(self, new_session_id, *, parent_session_id="", reset=False, rewound=False, **kwargs):
        return None

    def on_pre_compress(self, messages):
        return ""

    def on_memory_write(self, action, target, content, metadata=None):
        return None

    def on_delegation(self, task, result, *, child_session_id="", **kwargs):
        return None

    def backup_paths(self):
        return []
