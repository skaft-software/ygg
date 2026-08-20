"""Installed-entry-point style MemoryProvider conformance fixture."""

import json

from agent.memory_provider import MemoryProvider


class EntryPointMemoryProvider(MemoryProvider):
    @property
    def name(self):
        return "entrypoint-memory"

    def is_available(self):
        return True

    def initialize(self, session_id, **kwargs):
        self.session_id = session_id
        self.environment = kwargs.get("agent_identity")

    def system_prompt_block(self):
        return "Entry-point provider selected."

    def prefetch(self, query, *, session_id=""):
        return f"Entry-point recall for {query}" if query else ""

    def get_tool_schemas(self):
        return [
            {
                "name": "entrypoint_recall",
                "description": "Return one offline entry-point fixture result",
                "parameters": {"type": "object", "properties": {}},
            }
        ]

    def handle_tool_call(self, tool_name, args, **kwargs):
        del tool_name, args, kwargs
        return json.dumps({"items": ["offline entry-point result"]})


def register(ctx):
    ctx.register_memory_provider(EntryPointMemoryProvider())
