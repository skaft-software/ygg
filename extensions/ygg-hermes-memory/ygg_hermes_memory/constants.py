"""Pinned Hermes contract and package-wide safety ceilings."""

from __future__ import annotations

HERMES_CONTRACT_NAME = "hermes-agent.MemoryProvider"
HERMES_CONTRACT_VERSION = "0.20.1"
HERMES_CONTRACT_COMMIT = "7095e23eb2066fe9a2f93b99cdbfe0e2b5ece397"
HERMES_CONTRACT_ID = (
    f"{HERMES_CONTRACT_NAME}@{HERMES_CONTRACT_VERSION}+{HERMES_CONTRACT_COMMIT}"
)
HERMES_ENTRY_POINT_GROUP = "hermes_agent.memory_providers"

EXTENSION_VERSION = "0.1.0"

MAX_CONFIG_BYTES = 256 * 1024
MAX_PROVIDER_METADATA_BYTES = 64 * 1024
MAX_PROVIDER_CODE_FILES = 256
MAX_PROVIDER_CODE_BYTES = 16 * 1024 * 1024
MAX_DISCOVERED_PROVIDERS = 64
MAX_PROVIDER_TOOLS = 64
MAX_SCHEMA_BYTES = 128 * 1024
MAX_SCHEMA_DEPTH = 24
MAX_SCHEMA_NODES = 4096
MAX_QUERY_BYTES = 32 * 1024
MAX_CONTEXT_BYTES = 48 * 1024
MAX_SYSTEM_CONTEXT_BYTES = 16 * 1024
MAX_TOOL_ARGUMENT_BYTES = 64 * 1024
MAX_TOOL_RESULT_BYTES = 64 * 1024
MAX_TURN_TEXT_BYTES = 48 * 1024
MAX_SESSION_MESSAGES = 32
MAX_SESSION_MESSAGE_BYTES = 16 * 1024
MAX_ACTIVITIES = 64
MAX_PRESENTATION_NODES = 128
MAX_SAFE_LABEL_BYTES = 256
MAX_SAFE_DETAIL_BYTES = 16 * 1024
MAX_RETAINED_ERROR_BYTES = 512

SUPPORTED_SCHEMA_KEYWORDS = frozenset(
    {
        "$schema",
        "title",
        "description",
        "default",
        "examples",
        "type",
        "properties",
        "required",
        "additionalProperties",
        "items",
        "enum",
        "const",
        "allOf",
        "anyOf",
        "oneOf",
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "minLength",
        "maxLength",
        "minItems",
        "maxItems",
        "uniqueItems",
        "minProperties",
        "maxProperties",
    }
)

GENERIC_PRESENTATION_STATES = frozenset(
    {
        "empty",
        "loading",
        "pending",
        "active",
        "running",
        "succeeded",
        "failed",
        "cancelled",
        "degraded",
        "stopped",
        "unavailable",
    }
)
