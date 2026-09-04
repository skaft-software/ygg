#!/usr/bin/env python3
"""Generate the bounded API 0.3 extension contract from one schema source.

The generator deliberately owns protocol tables, wire models, validators, fixtures,
reference documentation, and SDK packaging declarations.  Handwritten runtime code
may convert generated values to product types, but cannot define API 0.3 wire
semantics independently.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
from copy import deepcopy
from pathlib import Path
from typing import Any, Iterable


ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "protocol/extension-api-v0.3.schema.json"
FIXTURE_DIRECTORY = ROOT / "protocol/fixtures/extension-api-v0.3"
NEGATIVE_FIXTURE_DIRECTORY = FIXTURE_DIRECTORY / "negative"

IDENTIFIER = re.compile(r"^[a-z][a-z0-9_]*$")
FIXTURE_NAME = re.compile(r"^[a-z0-9][a-z0-9-]*$")


def canonical_json(value: Any, *, max_depth: int, max_integer: int) -> str:
    """Encode one source-selected canonical JSON value."""

    def validate_string(item: str) -> None:
        try:
            item.encode("utf-8")
        except UnicodeEncodeError as error:
            raise ValueError("canonical JSON strings must be valid UTF-8") from error

    def validate(item: Any, depth: int = 0) -> None:
        if depth > max_depth:
            raise ValueError("canonical JSON nesting exceeds max_json_depth")
        if item is None or isinstance(item, bool):
            return
        if isinstance(item, str):
            validate_string(item)
            return
        if isinstance(item, int):
            if abs(item) > max_integer:
                raise ValueError("canonical JSON integer exceeds portable range")
            return
        if isinstance(item, float):
            raise ValueError("canonical JSON does not permit floating-point values")
        if isinstance(item, list):
            for entry in item:
                validate(entry, depth + 1)
            return
        if isinstance(item, dict):
            for key, entry in item.items():
                if not isinstance(key, str):
                    raise ValueError("canonical JSON object keys must be strings")
                validate_string(key)
                validate(entry, depth + 1)
            return
        raise ValueError(f"canonical JSON value is unsupported: {type(item).__name__}")

    validate(value)
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"), allow_nan=False)


def constant_name(name: str) -> str:
    return re.sub(r"[^A-Za-z0-9]", "_", name).upper()


def snake(name: str) -> str:
    return re.sub(r"(?<!^)(?=[A-Z])", "_", name).lower()


def field_presence(field: dict[str, Any]) -> str:
    # The original candidate used `required`; API 0.3 now makes presence explicit.
    if "presence" in field:
        return field["presence"]
    return "required" if field.get("required", True) else "optional"


def is_optional(field: dict[str, Any]) -> bool:
    return field_presence(field) == "optional"


def is_presence_aware(field: dict[str, Any]) -> bool:
    return is_optional(field) and field.get("nullable", False)


def split_array(type_name: str) -> tuple[str, bool]:
    return (type_name[:-2], True) if type_name.endswith("[]") else (type_name, False)


def rust_type(type_name: str) -> str:
    base, array = split_array(type_name)
    primitives = {
        "string": "String",
        "integer": "usize",
        "signed_integer": "i64",
        "boolean": "bool",
        "json": "serde_json::Value",
        "disposition": "String",
        "rpc_id": "JsonRpcId",
    }
    value = primitives.get(base, base)
    return f"Vec<{value}>" if array else value


def python_type(type_name: str) -> str:
    base, array = split_array(type_name)
    primitives = {
        "string": "str",
        "integer": "int",
        "signed_integer": "int",
        "boolean": "bool",
        "json": "Any",
        "disposition": "str",
        "rpc_id": "JsonRpcId",
    }
    value = primitives.get(base, base)
    return f"list[{value}]" if array else value


def typescript_type(type_name: str) -> str:
    base, array = split_array(type_name)
    primitives = {
        "string": "string",
        "integer": "number",
        "signed_integer": "number",
        "boolean": "boolean",
        "json": "JsonValue",
        "disposition": "DispositionKind",
        "rpc_id": "JsonRpcId",
    }
    value = primitives.get(base, base)
    return f"{value}[]" if array else value


def model_spec_literal(schema: dict[str, Any]) -> str:
    models: dict[str, Any] = {}
    for model in schema["models"]:
        if model.get("kind") == "tagged_union":
            models[model["name"]] = {
                "kind": "tagged_union",
                "tag": model["tag"],
                "variants": model["variants"],
            }
        else:
            models[model["name"]] = {
                "kind": "record",
                "fields": [
                    {
                        "name": field["name"],
                        "type": field["type"],
                        "presence": field_presence(field),
                        "nullable": field.get("nullable", False),
                        "max_items": field.get("max_items"),
                        "max_bytes": field.get("max_bytes"),
                        "values": field.get("values", []),
                    }
                    for field in model["fields"]
                ],
            }
    return json.dumps(models, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def validate_schema(schema: dict[str, Any]) -> None:
    required = {
        "schema_format", "api_version", "schema_id", "canonical_encoding",
        "canonical_profile", "legacy_adapters", "version_policy", "bounds",
        "capabilities", "methods", "errors", "dispositions", "models",
        "envelopes", "fixtures", "negative_fixtures",
    }
    missing = sorted(required - set(schema))
    if missing:
        raise ValueError(f"schema is missing required keys: {', '.join(missing)}")
    unknown = sorted(set(schema) - required)
    if unknown:
        raise ValueError(f"schema has unsupported keys: {', '.join(unknown)}")
    if schema["schema_format"] != "ygg.extension.protocol.schema/v1":
        raise ValueError("unsupported schema_format")
    if schema["api_version"] != "0.3":
        raise ValueError("API 0.3 generator requires api_version = 0.3")
    if not isinstance(schema["schema_id"], str) or not schema["schema_id"]:
        raise ValueError("schema_id must be a non-empty string")
    if not isinstance(schema["canonical_encoding"], str) or not schema["canonical_encoding"]:
        raise ValueError("canonical_encoding must be a non-empty string")
    if schema["canonical_profile"] != {
        "utf8": "required", "object_key_order": "unicode_scalar",
        "numbers": "portable_integers_only", "whitespace": "none",
    }:
        raise ValueError("canonical_profile must select the API 0.3 canonical JSON rules")

    def names(entries: Any, label: str) -> list[str]:
        if not isinstance(entries, list):
            raise ValueError(f"{label} must be a list")
        values = [entry.get("name") if isinstance(entry, dict) else None for entry in entries]
        if not all(isinstance(value, str) and value for value in values):
            raise ValueError(f"{label} names must be non-empty strings")
        if len(values) != len(set(values)):
            raise ValueError(f"{label} names must be unique")
        return values

    bound_names = names(schema["bounds"], "bounds")
    bounds: dict[str, int] = {}
    for bound in schema["bounds"]:
        value = bound.get("value")
        if not isinstance(value, int) or isinstance(value, bool) or value <= 0:
            raise ValueError(f"bound {bound['name']} must be a positive integer")
        if not isinstance(bound.get("description"), str) or not bound["description"]:
            raise ValueError(f"bound {bound['name']} must have a description")
        if not isinstance(bound.get("negotiated"), bool):
            raise ValueError(f"bound {bound['name']} must declare negotiated")
        bounds[bound["name"]] = value
    fixed_bounds = {
        "max_frame_bytes", "max_concurrent_requests", "max_tools", "max_capabilities",
        "max_methods", "max_capability_name_bytes", "max_method_name_bytes",
        "max_reason_bytes", "max_json_depth", "max_portable_json_integer",
        "max_content_parts", "max_tool_name_bytes", "max_tool_description_bytes",
        "max_json_rpc_id_bytes",
    }
    if not fixed_bounds.issubset(bound_names):
        raise ValueError("API 0.3 bound names are fixed")
    if bounds.get("max_portable_json_integer") != 9_007_199_254_740_991:
        raise ValueError("max_portable_json_integer must be the portable JSON integer maximum")

    policy = schema["version_policy"]
    if not isinstance(policy, list) or len(policy) != 3:
        raise ValueError("version_policy must declare API 0.1, 0.2, and 0.3 exactly once")
    expected_policy = [
        ("0.1", "frozen", "legacy-json-rpc", "supported", "unavailable"),
        ("0.2", "supported", "legacy-json-rpc", "supported", "supported"),
        ("0.3", "current", "canonical-json-rpc", "supported", "supported"),
    ]
    actual_policy = [
        (entry.get("version"), entry.get("status"), entry.get("wire"), entry.get("runtime"), entry.get("bundles"))
        for entry in policy
    ]
    if actual_policy != expected_policy:
        raise ValueError("version_policy must explicitly preserve API 0.1/0.2 and installable API 0.2/0.3")
    adapters = schema["legacy_adapters"]
    if [(entry.get("version"), entry.get("status"), entry.get("wire")) for entry in adapters] != [
        ("0.1", "frozen", "legacy-json-rpc"), ("0.2", "supported", "legacy-json-rpc"),
    ]:
        raise ValueError("legacy_adapters must match the explicit API 0.1/0.2 policy")

    capability_names = names(schema["capabilities"], "capabilities")
    for capability in schema["capabilities"]:
        if capability.get("host_offer") not in {"required", "optional", "unavailable"}:
            raise ValueError(f"capability {capability['name']} has invalid host_offer")
        if capability.get("status") not in {"foundation", "deferred"}:
            raise ValueError(f"capability {capability['name']} has invalid status")
    if not {"core", "tool_call", "content_parts", "request_cancellation"}.issubset(capability_names):
        raise ValueError("API 0.3 foundation capabilities are required")

    model_names = names(schema["models"], "models")
    record_models = {model["name"] for model in schema["models"] if model.get("kind") != "tagged_union"}
    valid_base_types = {"string", "integer", "signed_integer", "boolean", "json", "disposition", "rpc_id", *model_names}

    def validate_fields(fields: Any, label: str, *, allow_empty: bool = False) -> None:
        if not isinstance(fields, list) or (not fields and not allow_empty):
            raise ValueError(f"{label} must contain fields")
        seen: set[str] = set()
        saw_optional = False
        for field in fields:
            if not isinstance(field, dict):
                raise ValueError(f"{label} fields must be objects")
            allowed = {"name", "type", "presence", "nullable", "max_items", "max_bytes", "values"}
            if set(field) - allowed:
                raise ValueError(f"{label}.{field.get('name')} has unsupported semantic fields")
            name = field.get("name")
            type_name = field.get("type")
            if not isinstance(name, str) or not IDENTIFIER.fullmatch(name) or name in seen:
                raise ValueError(f"{label} has invalid or duplicate field {name!r}")
            seen.add(name)
            base, _array = split_array(type_name) if isinstance(type_name, str) else (None, False)
            if base not in valid_base_types:
                raise ValueError(f"{label}.{name} has unsupported type {type_name!r}")
            presence = field_presence(field)
            if presence not in {"required", "optional"}:
                raise ValueError(f"{label}.{name} has invalid presence")
            if not isinstance(field.get("nullable", False), bool):
                raise ValueError(f"{label}.{name} has invalid nullable marker")
            if presence == "optional":
                saw_optional = True
            elif saw_optional:
                raise ValueError(f"{label} required field {name} follows optional field")
            for key in ("max_items", "max_bytes"):
                if key in field and field[key] not in bounds:
                    raise ValueError(f"{label}.{name} references unknown bound {field[key]!r}")
            if "max_items" in field and not _array:
                raise ValueError(f"{label}.{name} max_items requires an array type")
            if "max_bytes" in field and base != "string":
                raise ValueError(f"{label}.{name} max_bytes requires a string type")
            if "values" in field:
                if not isinstance(field["values"], list) or not field["values"] or not all(isinstance(v, str) for v in field["values"]):
                    raise ValueError(f"{label}.{name} values must be non-empty string list")
                if base != "string":
                    raise ValueError(f"{label}.{name} values requires string type")

    for model in schema["models"]:
        kind = model.get("kind", "record")
        allowed = {"name", "description", "fields", "kind", "tag", "variants"}
        if set(model) - allowed:
            raise ValueError(f"model {model['name']} has unsupported keys")
        if not isinstance(model.get("description"), str) or not model["description"]:
            raise ValueError(f"model {model['name']} needs a description")
        if kind == "record":
            validate_fields(model.get("fields"), f"model {model['name']}", allow_empty=True)
        elif kind == "tagged_union":
            if not isinstance(model.get("tag"), str) or not IDENTIFIER.fullmatch(model["tag"]):
                raise ValueError(f"tagged union {model['name']} needs an identifier tag")
            variants = model.get("variants")
            if not isinstance(variants, list) or not variants:
                raise ValueError(f"tagged union {model['name']} needs variants")
            wires: set[str] = set()
            variant_names: set[str] = set()
            for variant in variants:
                if not isinstance(variant, dict) or set(variant) - {"name", "wire", "status", "fields"}:
                    raise ValueError(f"tagged union {model['name']} has invalid variant")
                if not isinstance(variant.get("name"), str) or not variant["name"].isidentifier() or variant["name"] in variant_names:
                    raise ValueError(f"tagged union {model['name']} variant name is invalid")
                variant_names.add(variant["name"])
                if not isinstance(variant.get("wire"), str) or not variant["wire"] or variant["wire"] in wires:
                    raise ValueError(f"tagged union {model['name']} variant wire is invalid")
                wires.add(variant["wire"])
                if variant.get("status", "foundation") not in {"foundation", "deferred"}:
                    raise ValueError(f"tagged union {model['name']} variant status is invalid")
                validate_fields(variant.get("fields"), f"variant {model['name']}.{variant['name']}", allow_empty=True)
        else:
            raise ValueError(f"model {model['name']} has unsupported kind")

    method_names = names(schema["methods"], "methods")
    expected_methods = {"initialize", "tool/call", "shutdown", "$/cancelRequest"}
    for method in schema["methods"]:
        allowed = {"name", "direction", "capability", "host_offer", "status", "params", "result", "terminal", "notification"}
        if set(method) - allowed:
            raise ValueError(f"method {method['name']} has unsupported semantic fields")
        if method.get("direction") not in {"host_to_extension", "extension_to_host", "bidirectional"}:
            raise ValueError(f"method {method['name']} has invalid direction")
        if method.get("capability") not in capability_names:
            raise ValueError(f"method {method['name']} references an unknown capability")
        if method.get("host_offer") not in {"required", "optional", "unavailable"}:
            raise ValueError(f"method {method['name']} has invalid host_offer")
        if method.get("status") not in {"foundation", "deferred"}:
            raise ValueError(f"method {method['name']} has invalid status")
        if method.get("terminal") not in {"initialized", "result_or_error", "shutdown", "original_request_cancelled", "stream_event", "deferred"}:
            raise ValueError(f"method {method['name']} has invalid terminal semantics")
        if not isinstance(method.get("notification"), bool):
            raise ValueError(f"method {method['name']} must declare notification semantics")
        for key in ("params", "result"):
            if method.get(key) is not None and method[key] not in model_names:
                raise ValueError(f"method {method['name']} references unknown {key} model")
    required_method_models = {
        "initialize": ("InitializeRequest", "InitializeResponse", "initialized", False),
        "tool/call": ("ToolCallParams", "ToolCallResult", "result_or_error", False),
        "shutdown": ("ShutdownParams", "ShutdownResult", "shutdown", False),
        "$/cancelRequest": ("CancelRequestParams", "CancelRequestResult", "original_request_cancelled", True),
    }
    by_method = {method["name"]: method for method in schema["methods"]}
    if not expected_methods.issubset(method_names):
        raise ValueError("API 0.3 foundation methods are required")
    for name, semantics in required_method_models.items():
        method = by_method[name]
        if (method.get("params"), method.get("result"), method.get("terminal"), method.get("notification")) != semantics:
            raise ValueError(f"foundation method {name} semantics must remain explicit")

    error_names = names(schema["errors"], "errors")
    if set(error_names) != {
        "parse_error", "invalid_request", "unknown_method", "invalid_params", "internal_error",
        "version_mismatch", "capability_mismatch", "resource_exhausted", "request_cancelled",
    }:
        raise ValueError("API 0.3 error semantic names are fixed")
    codes = [entry.get("code") for entry in schema["errors"]]
    if not all(isinstance(code, int) and not isinstance(code, bool) for code in codes) or len(codes) != len(set(codes)):
        raise ValueError("error codes must be unique integers")
    if not all(isinstance(entry.get("message"), str) and entry["message"] for entry in schema["errors"]):
        raise ValueError("error messages must be non-empty strings")

    disposition_names = names(schema["dispositions"], "dispositions")
    if set(disposition_names) != {"continue", "deny", "defer"}:
        raise ValueError("API 0.3 disposition semantic names are fixed")
    for disposition in schema["dispositions"]:
        if set(disposition) != {"name", "requires_reason", "description"}:
            raise ValueError("disposition has unsupported semantic fields")
        if not isinstance(disposition["requires_reason"], bool):
            raise ValueError("disposition requires_reason must be boolean")
        if not isinstance(disposition["description"], str) or not disposition["description"]:
            raise ValueError("disposition description must be non-empty")

    envelopes = schema["envelopes"]
    if not isinstance(envelopes, list) or {entry.get("name") for entry in envelopes} != {"request", "notification", "success_response", "error_response"}:
        raise ValueError("envelopes must define request, notification, success_response, and error_response")
    expected_envelope_semantics = {
        "request": None,
        "notification": None,
        "success_response": None,
        "error_response": "error_object",
    }
    for envelope in envelopes:
        if set(envelope) - {"name", "model", "id", "method", "result", "error", "semantic_validator"}:
            raise ValueError("envelope has unsupported semantic fields")
        if envelope.get("semantic_validator") != expected_envelope_semantics[envelope["name"]]:
            raise ValueError("envelope semantic validators must remain explicit")
        if envelope["model"] not in record_models:
            raise ValueError("envelope references an unknown model")
        if any(envelope[key] not in {"required", "forbidden"} for key in ("id", "method", "result", "error")):
            raise ValueError("envelope field rules must be required or forbidden")

    fixture_names = names(schema["fixtures"], "fixtures")
    for fixture in schema["fixtures"]:
        if not FIXTURE_NAME.fullmatch(fixture["name"]) or set(fixture) != {"name", "value"}:
            raise ValueError("golden fixtures need a canonical name and value")
        canonical_json(fixture["value"], max_depth=bounds["max_json_depth"], max_integer=bounds["max_portable_json_integer"])
    negative_names = names(schema["negative_fixtures"], "negative fixtures")
    if set(fixture_names) & set(negative_names):
        raise ValueError("golden and negative fixture names must be disjoint")
    for fixture in schema["negative_fixtures"]:
        if not FIXTURE_NAME.fullmatch(fixture["name"]) or set(fixture) - {"name", "value", "raw"} or ("value" in fixture) == ("raw" in fixture):
            raise ValueError("negative fixtures need exactly one value or raw payload")
        if "value" in fixture:
            canonical_json(fixture["value"], max_depth=bounds["max_json_depth"], max_integer=bounds["max_portable_json_integer"])
        elif not isinstance(fixture["raw"], str):
            raise ValueError("negative raw fixture must be a string")


def rust_string(value: str) -> str:
    return json.dumps(value, ensure_ascii=False)


def rust_array(values: Iterable[str]) -> str:
    return ", ".join(rust_string(value) for value in values)


def rust_field_type(field: dict[str, Any]) -> str:
    value = rust_type(field["type"])
    if is_presence_aware(field):
        return f"Presence<{value}>"
    if is_optional(field) or field.get("nullable", False):
        return f"Option<{value}>"
    return value


def render_rust(schema: dict[str, Any], source_hash: str) -> str:
    bounds = {entry["name"]: entry["value"] for entry in schema["bounds"]}
    required_capabilities = [entry["name"] for entry in schema["capabilities"] if entry["host_offer"] == "required"]
    optional_capabilities = [entry["name"] for entry in schema["capabilities"] if entry["host_offer"] == "optional"]
    required_methods = [entry["name"] for entry in schema["methods"] if entry["host_offer"] == "required"]
    optional_methods = [entry["name"] for entry in schema["methods"] if entry["host_offer"] == "optional"]
    directions = {"host_to_extension": "HostToExtension", "extension_to_host": "ExtensionToHost", "bidirectional": "Bidirectional"}
    models = schema["models"]
    records = [model for model in models if model.get("kind") != "tagged_union"]
    tagged = [model for model in models if model.get("kind") == "tagged_union"]
    lines: list[str] = [
        "// @generated by scripts/generate-extension-api-v03.py; DO NOT EDIT.",
        f"// Source: protocol/extension-api-v0.3.schema.json (sha256: {source_hash})",
        "//! Generated canonical wire models and validation for extension API 0.3.",
        "#![allow(missing_docs)]",
        "#![allow(clippy::too_many_lines)]",
        "#![allow(clippy::possible_missing_else)]",  # Generated validators remain intentionally compact.
        "",
        "use std::collections::{BTreeMap, BTreeSet};",
        "use std::fmt;",
        "",
        "use serde::de::DeserializeOwned;",
        "use serde::{Deserialize, Deserializer, Serialize, Serializer};",
        "",
        f"pub const API_VERSION: &str = {rust_string(schema['api_version'])};",
        f"pub const SCHEMA_ID: &str = {rust_string(schema['schema_id'])};",
        f"pub const CANONICAL_ENCODING: &str = {rust_string(schema['canonical_encoding'])};",
        f"pub const SCHEMA_SHA256: &str = {rust_string(source_hash)};",
        f"pub const MAX_PORTABLE_JSON_INTEGER: i64 = {bounds['max_portable_json_integer']};",
    ]
    for bound in schema["bounds"]:
        if bound["name"] != "max_portable_json_integer":
            lines.append(f"pub const {constant_name(bound['name'])}: usize = {bound['value']};")
    lines.extend([
        "",
        "#[derive(Clone, Copy, Debug, PartialEq, Eq)]",
        "pub enum MethodDirection { HostToExtension, ExtensionToHost, Bidirectional }",
        "",
        "#[derive(Clone, Copy, Debug, PartialEq, Eq)]",
        "pub struct ApiVersionSpec { pub version: &'static str, pub status: &'static str, pub wire: &'static str, pub runtime_supported: bool, pub bundle_supported: bool }",
        "pub const API_VERSIONS: &[ApiVersionSpec] = &[",
    ])
    for policy in schema["version_policy"]:
        lines.append(
            "    ApiVersionSpec { "
            f"version: {rust_string(policy['version'])}, status: {rust_string(policy['status'])}, wire: {rust_string(policy['wire'])}, "
            f"runtime_supported: {str(policy['runtime'] == 'supported').lower()}, bundle_supported: {str(policy['bundles'] == 'supported').lower()} }},"
        )
    lines.extend([
        "];",
        "pub fn api_version_spec(version: &str) -> Option<&'static ApiVersionSpec> { API_VERSIONS.iter().find(|entry| entry.version == version) }",
        "pub fn runtime_supports_api_version(version: &str) -> bool { api_version_spec(version).is_some_and(|entry| entry.runtime_supported) }",
        "pub fn bundle_supports_api_version(version: &str) -> bool { api_version_spec(version).is_some_and(|entry| entry.bundle_supported) }",
        "",
        "#[derive(Clone, Copy, Debug, PartialEq, Eq)]",
        "pub struct CapabilitySpec { pub name: &'static str, pub required_by_default: bool, pub available: bool }",
        "#[derive(Clone, Copy, Debug, PartialEq, Eq)]",
        "pub struct MethodSpec { pub name: &'static str, pub direction: MethodDirection, pub capability: &'static str, pub required_by_default: bool, pub available: bool, pub params: Option<&'static str>, pub result: Option<&'static str>, pub terminal: &'static str, pub notification: bool }",
        "pub const CAPABILITIES: &[CapabilitySpec] = &[",
    ])
    for item in schema["capabilities"]:
        lines.append(f"    CapabilitySpec {{ name: {rust_string(item['name'])}, required_by_default: {str(item['host_offer'] == 'required').lower()}, available: {str(item['host_offer'] != 'unavailable').lower()} }},")
    lines.extend(["] ;", "pub const METHODS: &[MethodSpec] = &["])
    for item in schema["methods"]:
        params = "None" if item["params"] is None else f"Some({rust_string(item['params'])})"
        result = "None" if item["result"] is None else f"Some({rust_string(item['result'])})"
        lines.append(
            "    MethodSpec { "
            f"name: {rust_string(item['name'])}, direction: MethodDirection::{directions[item['direction']]}, capability: {rust_string(item['capability'])}, "
            f"required_by_default: {str(item['host_offer'] == 'required').lower()}, available: {str(item['host_offer'] != 'unavailable').lower()}, "
            f"params: {params}, result: {result}, terminal: {rust_string(item['terminal'])}, notification: {str(item['notification']).lower()} }},"
        )
    lines.extend([
        "];",
        "pub fn capability_spec(name: &str) -> Option<&'static CapabilitySpec> { CAPABILITIES.iter().find(|entry| entry.name == name) }",
        "pub fn method_spec(name: &str) -> Option<&'static MethodSpec> { METHODS.iter().find(|entry| entry.name == name) }",
        "",
        "#[derive(Clone, Copy, Debug, PartialEq, Eq)]",
        "pub struct ErrorSpec { pub name: &'static str, pub code: i64, pub message: &'static str }",
        "pub const ERRORS: &[ErrorSpec] = &[",
    ])
    for item in schema["errors"]:
        lines.append(f"    ErrorSpec {{ name: {rust_string(item['name'])}, code: {item['code']}, message: {rust_string(item['message'])} }},")
    lines.extend([
        "];",
        "pub fn error_spec(name: &str) -> Option<&'static ErrorSpec> { ERRORS.iter().find(|entry| entry.name == name) }",
        "",
        "#[derive(Clone, Copy, Debug, PartialEq, Eq)]",
        "pub struct DispositionSpec { pub name: &'static str, pub requires_reason: bool }",
        "pub const DISPOSITIONS: &[DispositionSpec] = &[",
    ])
    for item in schema["dispositions"]:
        lines.append(f"    DispositionSpec {{ name: {rust_string(item['name'])}, requires_reason: {str(item['requires_reason']).lower()} }},")
    lines.extend([
        "];",
        "pub fn disposition_spec(name: &str) -> Option<&'static DispositionSpec> { DISPOSITIONS.iter().find(|entry| entry.name == name) }",
        "",
        "#[derive(Clone, Copy, Debug, PartialEq, Eq)]",
        "pub struct LegacyAdapterSpec { pub version: &'static str, pub status: &'static str, pub wire: &'static str }",
        "pub const LEGACY_ADAPTERS: &[LegacyAdapterSpec] = &[",
    ])
    for item in schema["legacy_adapters"]:
        lines.append(f"    LegacyAdapterSpec {{ version: {rust_string(item['version'])}, status: {rust_string(item['status'])}, wire: {rust_string(item['wire'])} }},")
    lines.extend([
        "];",
        "pub fn legacy_adapter(version: &str) -> Option<&'static LegacyAdapterSpec> { LEGACY_ADAPTERS.iter().find(|entry| entry.version == version) }",
        "",
        "#[derive(Clone, Debug, PartialEq, Eq, Default)]",
        "pub enum Presence<T> { #[default] Absent, Null, Value(T) }",
        "impl<T> Presence<T> { pub fn is_absent(value: &Self) -> bool { matches!(value, Self::Absent) } pub fn into_option(self) -> Option<T> { match self { Self::Value(value) => Some(value), Self::Absent | Self::Null => None } } }",
        "impl<T: Serialize> Serialize for Presence<T> { fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error> where S: Serializer { match self { Self::Absent => serializer.serialize_unit(), Self::Null => serializer.serialize_none(), Self::Value(value) => value.serialize(serializer) } } }",
        "impl<'de, T: Deserialize<'de>> Deserialize<'de> for Presence<T> { fn deserialize<D>(deserializer: D) -> Result<Self, D::Error> where D: Deserializer<'de> { Ok(match Option::<T>::deserialize(deserializer)? { Some(value) => Self::Value(value), None => Self::Null }) } }",
        "",
        "#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]",
        "#[serde(untagged)]",
        "pub enum JsonRpcId { Number(u64), String(String) }",
        "",
    ])
    for model in records:
        lines.extend(["#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]", "#[serde(deny_unknown_fields)]", f"pub struct {model['name']} {{"])
        for field in model["fields"]:
            if is_presence_aware(field):
                lines.extend(["    #[serde(default, skip_serializing_if = \"Presence::is_absent\")]"])
            elif is_optional(field):
                lines.extend(["    #[serde(default, skip_serializing_if = \"Option::is_none\")]"])
            lines.append(f"    pub {field['name']}: {rust_field_type(field)},")
        lines.extend(["}", ""])
    for model in tagged:
        lines.extend(["#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]", f"#[serde(tag = {rust_string(model['tag'])}, deny_unknown_fields)]", f"pub enum {model['name']} {{"])
        for variant in model["variants"]:
            lines.append(f"    #[serde(rename = {rust_string(variant['wire'])})]")
            if variant["fields"]:
                lines.append(f"    {variant['name']} {{")
                for field in variant["fields"]:
                    if is_presence_aware(field):
                        lines.append("        #[serde(default, skip_serializing_if = \"Presence::is_absent\")]")
                    elif is_optional(field):
                        lines.append("        #[serde(default, skip_serializing_if = \"Option::is_none\")]")
                    lines.append(f"        {field['name']}: {rust_field_type(field)},")
                lines.append("    },")
            else:
                lines.append(f"    {variant['name']},")
        lines.extend(["}", ""])
    lines.extend([
        "#[derive(Clone, Debug, PartialEq)]",
        "pub enum JsonRpcEnvelope { Request(JsonRpcRequest), Notification(JsonRpcNotification), SuccessResponse(JsonRpcSuccessResponse), ErrorResponse(JsonRpcErrorResponse) }",
        "",
        "#[derive(Clone, Debug, PartialEq, Eq)]",
        "pub struct ContractError { pub code: i64, pub message: String }",
        "impl ContractError { fn named(name: &str, detail: impl Into<String>) -> Self { let spec = error_spec(name).expect(\"generated error semantic exists\"); let detail = detail.into(); Self { code: spec.code, message: if detail.is_empty() { spec.message.to_owned() } else { format!(\"{}: {detail}\", spec.message) } } } }",
        "impl fmt::Display for ContractError { fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result { write!(formatter, \"{}\", self.message) } }",
        "impl std::error::Error for ContractError {}",
        "",
        "#[derive(Clone, Debug, PartialEq)]",
        "pub struct NegotiatedContract { pub capabilities: BTreeSet<String>, pub methods: BTreeSet<String>, pub limits: ProtocolLimits }",
        "",
        "#[derive(Clone, Copy)]",
        "struct WireFieldSpec { name: &'static str, type_name: &'static str, required: bool, nullable: bool, max_items: Option<usize>, max_bytes: Option<usize>, values: &'static [&'static str] }",
    ])
    for model in records:
        lines.append(f"const {constant_name('fields_' + snake(model['name']))}: &[WireFieldSpec] = &[")
        for field in model["fields"]:
            max_items = "None" if "max_items" not in field else f"Some({constant_name(field['max_items'])})"
            max_bytes = "None" if "max_bytes" not in field else f"Some({constant_name(field['max_bytes'])})"
            values = f"&[{rust_array(field.get('values', []))}]"
            lines.append(
                "    WireFieldSpec { "
                f"name: {rust_string(field['name'])}, type_name: {rust_string(field['type'])}, required: {str(not is_optional(field)).lower()}, nullable: {str(field.get('nullable', False)).lower()}, max_items: {max_items}, max_bytes: {max_bytes}, values: {values} }},"
            )
        lines.append("];" )
    for model in tagged:
        for variant in model["variants"]:
            if variant.get("status", "foundation") != "foundation":
                continue
            fields = [{"name": model["tag"], "type": "string", "presence": "required", "nullable": False, "values": [variant["wire"]]}, *variant["fields"]]
            lines.append(f"const {constant_name('fields_' + snake(model['name']) + '_' + snake(variant['name']))}: &[WireFieldSpec] = &[")
            for field in fields:
                max_items = "None" if "max_items" not in field else f"Some({constant_name(field['max_items'])})"
                max_bytes = "None" if "max_bytes" not in field else f"Some({constant_name(field['max_bytes'])})"
                values = f"&[{rust_array(field.get('values', []))}]"
                lines.append(
                    "    WireFieldSpec { "
                    f"name: {rust_string(field['name'])}, type_name: {rust_string(field['type'])}, required: {str(not is_optional(field)).lower()}, nullable: {str(field.get('nullable', False)).lower()}, max_items: {max_items}, max_bytes: {max_bytes}, values: {values} }},"
                )
            lines.append("];" )
    lines.extend([
        "",
        "fn validate_version(value: &str) -> Result<(), ContractError> { if value == API_VERSION { Ok(()) } else { Err(ContractError::named(\"version_mismatch\", format!(\"expected {API_VERSION}, received {value}\"))) } }",
        "fn validate_named_list(values: &[String], limit: usize, byte_limit: usize, kind: &str, known: impl Fn(&str) -> bool) -> Result<BTreeSet<String>, ContractError> { if values.len() > limit { return Err(ContractError::named(\"resource_exhausted\", format!(\"{kind} count exceeds {limit}\"))); } let mut result = BTreeSet::new(); for value in values { if value.is_empty() || value.len() > byte_limit { return Err(ContractError::named(\"invalid_params\", format!(\"invalid {kind} name {value:?}\"))); } if !known(value) { return Err(ContractError::named(\"capability_mismatch\", format!(\"unknown {kind} {value:?}\"))); } if !result.insert(value.clone()) { return Err(ContractError::named(\"capability_mismatch\", format!(\"duplicate {kind} {value:?}\"))); } } Ok(result) }",
        "fn validate_limits(limits: &ProtocolLimits) -> Result<(), ContractError> { if limits.max_frame_bytes == 0 || limits.max_concurrent_requests == 0 || limits.max_tools == 0 { return Err(ContractError::named(\"invalid_params\", \"negotiated limits must be greater than zero\")); } if limits.max_frame_bytes > MAX_FRAME_BYTES || limits.max_concurrent_requests > MAX_CONCURRENT_REQUESTS || limits.max_tools > MAX_TOOLS { return Err(ContractError::named(\"resource_exhausted\", \"negotiated limit exceeds API 0.3 maximum\")); } Ok(()) }",
        "fn validate_disjoint(left: &BTreeSet<String>, right: &BTreeSet<String>, kind: &str) -> Result<(), ContractError> { if let Some(value) = left.intersection(right).next() { return Err(ContractError::named(\"capability_mismatch\", format!(\"{kind} {value:?} is both required and optional\"))); } Ok(()) }",
        "fn validate_exact_host_offer(values: &BTreeSet<String>, expected: &[&str], kind: &str) -> Result<(), ContractError> { if values.len() != expected.len() || expected.iter().any(|name| !values.contains(*name)) { return Err(ContractError::named(\"capability_mismatch\", format!(\"host offer {kind} set differs from generated API 0.3 contract\"))); } Ok(()) }",
        "fn validate_available_capabilities(values: &BTreeSet<String>) -> Result<(), ContractError> { if let Some(value) = values.iter().find(|value| !capability_spec(value).expect(\"validated capability exists\").available) { return Err(ContractError::named(\"capability_mismatch\", format!(\"capability {value:?} is unavailable\"))); } Ok(()) }",
        "fn validate_available_methods(values: &BTreeSet<String>) -> Result<(), ContractError> { if let Some(value) = values.iter().find(|value| !method_spec(value).expect(\"validated method exists\").available) { return Err(ContractError::named(\"capability_mismatch\", format!(\"method {value:?} is unavailable\"))); } Ok(()) }",
        "fn validate_method_capabilities(capabilities: &BTreeSet<String>, methods: &BTreeSet<String>) -> Result<(), ContractError> { for method in methods { let capability = method_spec(method).expect(\"validated method exists\").capability; if !capabilities.contains(capability) { return Err(ContractError::named(\"capability_mismatch\", format!(\"method {method:?} requires capability {capability:?}\"))); } } Ok(()) }",
        "",
        "pub fn validate_offer(offer: &ContractOffer) -> Result<(), ContractError> { if offer.schema != SCHEMA_ID { return Err(ContractError::named(\"version_mismatch\", format!(\"expected schema {SCHEMA_ID}, received {}\", offer.schema))); } if offer.encoding != CANONICAL_ENCODING { return Err(ContractError::named(\"invalid_params\", format!(\"expected encoding {CANONICAL_ENCODING}, received {}\", offer.encoding))); } let required_capabilities = validate_named_list(&offer.required_capabilities, MAX_CAPABILITIES, MAX_CAPABILITY_NAME_BYTES, \"capability\", |name| capability_spec(name).is_some())?; let optional_capabilities = validate_named_list(&offer.optional_capabilities, MAX_CAPABILITIES, MAX_CAPABILITY_NAME_BYTES, \"capability\", |name| capability_spec(name).is_some())?; validate_disjoint(&required_capabilities, &optional_capabilities, \"capability\")?;",
        f"    validate_exact_host_offer(&required_capabilities, &[{rust_array(required_capabilities)}], \"required capability\")?;",
        f"    validate_exact_host_offer(&optional_capabilities, &[{rust_array(optional_capabilities)}], \"optional capability\")?;",
        "    let capabilities = required_capabilities.union(&optional_capabilities).cloned().collect::<BTreeSet<_>>(); validate_available_capabilities(&capabilities)?; let required_methods = validate_named_list(&offer.required_methods, MAX_METHODS, MAX_METHOD_NAME_BYTES, \"method\", |name| method_spec(name).is_some())?; let optional_methods = validate_named_list(&offer.optional_methods, MAX_METHODS, MAX_METHOD_NAME_BYTES, \"method\", |name| method_spec(name).is_some())?; validate_disjoint(&required_methods, &optional_methods, \"method\")?;",
        f"    validate_exact_host_offer(&required_methods, &[{rust_array(required_methods)}], \"required method\")?;",
        f"    validate_exact_host_offer(&optional_methods, &[{rust_array(optional_methods)}], \"optional method\")?;",
        "    let methods = required_methods.union(&optional_methods).cloned().collect::<BTreeSet<_>>(); validate_available_methods(&methods)?; validate_method_capabilities(&capabilities, &methods)?; validate_limits(&offer.limits) }",
        "pub fn validate_selection(selection: &ContractSelection) -> Result<(), ContractError> { if selection.schema != SCHEMA_ID { return Err(ContractError::named(\"version_mismatch\", format!(\"expected schema {SCHEMA_ID}, received {}\", selection.schema))); } if selection.encoding != CANONICAL_ENCODING { return Err(ContractError::named(\"invalid_params\", format!(\"expected encoding {CANONICAL_ENCODING}, received {}\", selection.encoding))); } let capabilities = validate_named_list(&selection.capabilities, MAX_CAPABILITIES, MAX_CAPABILITY_NAME_BYTES, \"capability\", |name| capability_spec(name).is_some())?; let methods = validate_named_list(&selection.methods, MAX_METHODS, MAX_METHOD_NAME_BYTES, \"method\", |name| method_spec(name).is_some())?; validate_available_capabilities(&capabilities)?; validate_available_methods(&methods)?; validate_method_capabilities(&capabilities, &methods)?; validate_limits(&selection.limits) }",
        "pub fn host_offer(max_frame_bytes: usize, max_concurrent_requests: usize) -> Result<ContractOffer, ContractError> { if max_frame_bytes == 0 || max_concurrent_requests == 0 { return Err(ContractError::named(\"invalid_params\", \"host offer limits must be greater than zero\")); } let offer = ContractOffer { schema: SCHEMA_ID.to_owned(), encoding: CANONICAL_ENCODING.to_owned(),",
        f"        required_capabilities: vec![{rust_array(required_capabilities)}].into_iter().map(str::to_owned).collect(), optional_capabilities: vec![{rust_array(optional_capabilities)}].into_iter().map(str::to_owned).collect(), required_methods: vec![{rust_array(required_methods)}].into_iter().map(str::to_owned).collect(), optional_methods: vec![{rust_array(optional_methods)}].into_iter().map(str::to_owned).collect(),",
        "        limits: ProtocolLimits { max_frame_bytes: max_frame_bytes.min(MAX_FRAME_BYTES), max_concurrent_requests: max_concurrent_requests.min(MAX_CONCURRENT_REQUESTS), max_tools: MAX_TOOLS } }; validate_offer(&offer)?; Ok(offer) }",
        "pub fn select_required(offer: &ContractOffer) -> Result<ContractSelection, ContractError> { validate_offer(offer)?; Ok(ContractSelection { schema: offer.schema.clone(), encoding: offer.encoding.clone(), capabilities: offer.required_capabilities.clone(), methods: offer.required_methods.clone(), limits: offer.limits.clone() }) }",
        "pub fn negotiate(offer: &ContractOffer, selection: &ContractSelection) -> Result<NegotiatedContract, ContractError> { validate_offer(offer)?; validate_selection(selection)?; let offered_capabilities = offer.required_capabilities.iter().chain(&offer.optional_capabilities).cloned().collect::<BTreeSet<_>>(); let selected_capabilities = selection.capabilities.iter().cloned().collect::<BTreeSet<_>>(); if let Some(value) = selected_capabilities.iter().find(|value| !offered_capabilities.contains(value.as_str())) { return Err(ContractError::named(\"capability_mismatch\", format!(\"capability {value:?} was not offered\"))); } if let Some(value) = offer.required_capabilities.iter().find(|value| !selected_capabilities.contains(value.as_str())) { return Err(ContractError::named(\"capability_mismatch\", format!(\"required capability {value:?} was not selected\"))); } let offered_methods = offer.required_methods.iter().chain(&offer.optional_methods).cloned().collect::<BTreeSet<_>>(); let selected_methods = selection.methods.iter().cloned().collect::<BTreeSet<_>>(); if let Some(value) = selected_methods.iter().find(|value| !offered_methods.contains(value.as_str())) { return Err(ContractError::named(\"capability_mismatch\", format!(\"method {value:?} was not offered\"))); } if let Some(value) = offer.required_methods.iter().find(|value| !selected_methods.contains(value.as_str())) { return Err(ContractError::named(\"capability_mismatch\", format!(\"required method {value:?} was not selected\"))); } if selection.limits.max_frame_bytes > offer.limits.max_frame_bytes || selection.limits.max_concurrent_requests > offer.limits.max_concurrent_requests || selection.limits.max_tools > offer.limits.max_tools { return Err(ContractError::named(\"capability_mismatch\", \"selection increases a host offer limit\")); } Ok(NegotiatedContract { capabilities: selected_capabilities, methods: selected_methods, limits: selection.limits.clone() }) }",
        "pub fn method_is_available(contract: &NegotiatedContract, name: &str, direction: MethodDirection) -> bool { let Some(spec) = method_spec(name) else { return false; }; (spec.direction == MethodDirection::Bidirectional || spec.direction == direction) && spec.available && contract.methods.contains(name) && contract.capabilities.contains(spec.capability) }",
        "pub fn require_method(contract: &NegotiatedContract, name: &str, direction: MethodDirection) -> Result<(), ContractError> { if method_spec(name).is_none() { return Err(ContractError::named(\"unknown_method\", format!(\"unknown method {name:?}\"))); } if method_is_available(contract, name, direction) { Ok(()) } else { Err(ContractError::named(\"unknown_method\", format!(\"unnegotiated method {name:?}\"))) } }",
        "",
        "fn canonical_value(value: &serde_json::Value, depth: usize) -> Result<serde_json::Value, ContractError> { if depth > MAX_JSON_DEPTH { return Err(ContractError::named(\"invalid_params\", \"canonical JSON nesting exceeds max_json_depth\")); } match value { serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::String(_) => Ok(value.clone()), serde_json::Value::Number(number) => { let valid = number.as_i64().is_some_and(|number| (-MAX_PORTABLE_JSON_INTEGER..=MAX_PORTABLE_JSON_INTEGER).contains(&number)) || number.as_u64().is_some_and(|number| number <= MAX_PORTABLE_JSON_INTEGER as u64); if valid { Ok(value.clone()) } else { Err(ContractError::named(\"invalid_params\", \"canonical JSON permits only portable integers\")) } }, serde_json::Value::Array(values) => values.iter().map(|value| canonical_value(value, depth + 1)).collect::<Result<Vec<_>, _>>().map(serde_json::Value::Array), serde_json::Value::Object(values) => { let mut sorted = BTreeMap::new(); for (key, value) in values { sorted.insert(key.clone(), canonical_value(value, depth + 1)?); } Ok(serde_json::Value::Object(sorted.into_iter().collect())) } } }",
        "pub fn canonical_json(value: &serde_json::Value) -> Result<String, ContractError> { serde_json::to_string(&canonical_value(value, 0)?).map_err(|error| ContractError::named(\"internal_error\", error.to_string())) }",
        "pub fn canonical_frame(value: &serde_json::Value, max_frame_bytes: usize) -> Result<String, ContractError> { if max_frame_bytes == 0 { return Err(ContractError::named(\"invalid_params\", \"frame limit must be greater than zero\")); } if max_frame_bytes > MAX_FRAME_BYTES { return Err(ContractError::named(\"resource_exhausted\", \"frame limit exceeds API 0.3 maximum\")); } let frame = canonical_json(value)?; if frame.len() > max_frame_bytes { return Err(ContractError::named(\"resource_exhausted\", \"canonical frame exceeds negotiated max_frame_bytes\")); } Ok(frame) }",
        "",
        "fn validate_wire_type(type_name: &str, value: &serde_json::Value) -> Result<(), ContractError> { if let Some(element) = type_name.strip_suffix(\"[]\") { let values = value.as_array().ok_or_else(|| ContractError::named(\"invalid_params\", format!(\"expected {type_name}\")))?; for entry in values { validate_wire_type(element, entry)?; } return Ok(()); } match type_name { \"string\" => if value.is_string() { Ok(()) } else { Err(ContractError::named(\"invalid_params\", \"expected string\")) }, \"boolean\" => if value.is_boolean() { Ok(()) } else { Err(ContractError::named(\"invalid_params\", \"expected boolean\")) }, \"integer\" => if value.as_u64().is_some() { Ok(()) } else { Err(ContractError::named(\"invalid_params\", \"expected unsigned integer\")) }, \"signed_integer\" => if value.as_i64().is_some() { Ok(()) } else { Err(ContractError::named(\"invalid_params\", \"expected signed integer\")) }, \"json\" => canonical_value(value, 0).map(|_| ()), \"disposition\" => if value.is_string() { Ok(()) } else { Err(ContractError::named(\"invalid_params\", \"expected disposition string\")) }, \"rpc_id\" => match value { serde_json::Value::String(value) if !value.is_empty() && value.len() <= MAX_JSON_RPC_ID_BYTES => Ok(()), serde_json::Value::Number(number) if number.as_u64().is_some() => Ok(()), _ => Err(ContractError::named(\"invalid_request\", \"JSON-RPC id must be a bounded string or unsigned integer\")) }, model => validate_model_value(model, value) } }",
        "fn validate_record(value: &serde_json::Value, fields: &[WireFieldSpec]) -> Result<(), ContractError> { let object = value.as_object().ok_or_else(|| ContractError::named(\"invalid_params\", \"model must be an object\"))?; for key in object.keys() { if !fields.iter().any(|field| field.name == key) { return Err(ContractError::named(\"invalid_params\", format!(\"unknown model field {key:?}\"))); } } for field in fields { let Some(value) = object.get(field.name) else { if field.required { return Err(ContractError::named(\"invalid_params\", format!(\"{} is required\", field.name))); } continue; }; if value.is_null() { if field.nullable { continue; } return Err(ContractError::named(\"invalid_params\", format!(\"{} must not be null\", field.name))); } validate_wire_type(field.type_name, value)?; if let Some(max_items) = field.max_items { if value.as_array().is_some_and(|items| items.len() > max_items) { return Err(ContractError::named(\"resource_exhausted\", format!(\"{} exceeds item bound\", field.name))); } } if let Some(max_bytes) = field.max_bytes { let exceeds = value.as_str().is_some_and(|text| text.len() > max_bytes) || (field.type_name.strip_suffix(\"[]\") == Some(\"string\") && value.as_array().is_some_and(|items| items.iter().any(|item| item.as_str().is_some_and(|text| text.len() > max_bytes)))); if exceeds { return Err(ContractError::named(\"resource_exhausted\", format!(\"{} exceeds byte bound\", field.name))); } } if !field.values.is_empty() && !value.as_str().is_some_and(|text| field.values.contains(&text)) { return Err(ContractError::named(\"invalid_params\", format!(\"{} has unsupported value\", field.name))); } } Ok(()) }",
        "fn validate_model_value(name: &str, value: &serde_json::Value) -> Result<(), ContractError> { match name {",
    ])
    for model in records:
        lines.append(f"    {rust_string(model['name'])} => validate_record(value, {constant_name('fields_' + snake(model['name']))}),")
    for model in tagged:
        lines.extend([
            f"    {rust_string(model['name'])} => {{",
            f"        let object = value.as_object().ok_or_else(|| ContractError::named(\"invalid_params\", \"{model['name']} must be an object\"))?;",
            f"        match object.get({rust_string(model['tag'])}).and_then(serde_json::Value::as_str) {{",
        ])
        for variant in model["variants"]:
            fields = constant_name('fields_' + snake(model['name']) + '_' + snake(variant['name']))
            if variant.get("status", "foundation") == "foundation":
                lines.append(f"            Some({rust_string(variant['wire'])}) => validate_record(value, {fields}),")
            else:
                lines.append(f"            Some({rust_string(variant['wire'])}) => Err(ContractError::named(\"capability_mismatch\", \"{model['name']} variant {variant['wire']} is deferred\")),")
        lines.append(f"            _ => Err(ContractError::named(\"invalid_params\", \"unknown {model['name']} variant\")),")
        lines.append("        }")
        lines.append("    },")
    lines.extend([
        "    _ => Err(ContractError::named(\"invalid_params\", format!(\"unknown generated model {name}\"))),",
        "} }",
        "fn parse_model<T: DeserializeOwned>(name: &str, value: serde_json::Value) -> Result<T, ContractError> { canonical_value(&value, 0)?; validate_model_value(name, &value)?; serde_json::from_value(value).map_err(|error| ContractError::named(\"invalid_params\", format!(\"invalid {name}: {error}\"))) }",
    ])
    for model in models:
        func = snake(model["name"])
        lines.append(f"pub fn parse_{func}(value: serde_json::Value) -> Result<{model['name']}, ContractError> {{ parse_model({rust_string(model['name'])}, value) }}")
    lines.extend([
        "",
        "fn serialized_value<T: Serialize>(value: &T) -> Result<serde_json::Value, ContractError> { let value = serde_json::to_value(value).map_err(|error| ContractError::named(\"internal_error\", error.to_string()))?; canonical_value(&value, 0)?; Ok(value) }",
        "pub fn validate_disposition(disposition: &Disposition) -> Result<(), ContractError> { let value = serialized_value(disposition)?; validate_model_value(\"Disposition\", &value)?; let object = value.as_object().expect(\"validated record\"); let kind = object.get(\"kind\").and_then(serde_json::Value::as_str).expect(\"validated kind\"); let spec = disposition_spec(kind).ok_or_else(|| ContractError::named(\"invalid_params\", format!(\"unknown disposition {kind:?}\")))?; let reason = object.get(\"reason\").and_then(serde_json::Value::as_str); if reason.is_some_and(|reason| reason.is_empty() || reason.len() > MAX_REASON_BYTES) { return Err(ContractError::named(\"invalid_params\", \"disposition reason is empty or exceeds max_reason_bytes\")); } if spec.requires_reason && reason.is_none() { return Err(ContractError::named(\"invalid_params\", \"disposition requires a reason\")); } Ok(()) }",
        "pub fn validate_initialize_request(request: &InitializeRequest) -> Result<(), ContractError> { let value = serialized_value(request)?; validate_model_value(\"InitializeRequest\", &value)?; validate_version(&request.api_version)?; validate_offer(&request.contract) }",
        "pub fn validate_initialize_response(response: &InitializeResponse) -> Result<(), ContractError> { let value = serialized_value(response)?; validate_model_value(\"InitializeResponse\", &value)?; validate_version(&response.api_version)?; validate_selection(&response.contract)?; if response.tools.len() > response.contract.limits.max_tools { return Err(ContractError::named(\"resource_exhausted\", \"initialize tool catalog exceeds negotiated max_tools\")); } Ok(()) }",
        "pub fn validate_tool_call_params(params: &ToolCallParams) -> Result<(), ContractError> { let value = serialized_value(params)?; validate_model_value(\"ToolCallParams\", &value) }",
        "pub fn validate_tool_call_result(result: &ToolCallResult) -> Result<(), ContractError> { let value = serialized_value(result)?; validate_model_value(\"ToolCallResult\", &value) }",
        "pub fn validate_cancel_request_params(params: &CancelRequestParams) -> Result<(), ContractError> { let value = serialized_value(params)?; validate_model_value(\"CancelRequestParams\", &value) }",
        "pub fn validate_shutdown_params(params: &ShutdownParams) -> Result<(), ContractError> { let value = serialized_value(params)?; validate_model_value(\"ShutdownParams\", &value) }",
        "pub fn validate_shutdown_result(result: &ShutdownResult) -> Result<(), ContractError> { let value = serialized_value(result)?; validate_model_value(\"ShutdownResult\", &value) }",
        "pub fn validate_error_object(error: &ErrorObject) -> Result<(), ContractError> { let value = serialized_value(error)?; validate_model_value(\"ErrorObject\", &value)?; let Some(spec) = ERRORS.iter().find(|spec| spec.code == error.code) else { return Err(ContractError::named(\"invalid_params\", \"error code is absent from API 0.3 table\")); }; if spec.message != error.message { return Err(ContractError::named(\"invalid_params\", \"error message does not match its API 0.3 code\")); } Ok(()) }",
        "pub fn error_object(name: &str, data: Option<serde_json::Value>) -> Result<ErrorObject, ContractError> { let spec = error_spec(name).ok_or_else(|| ContractError::named(\"internal_error\", \"unknown generated error semantic\"))?; let error = ErrorObject { code: spec.code, message: spec.message.to_owned(), data: match data { Some(value) => Presence::Value(value), None => Presence::Absent } }; validate_error_object(&error)?; Ok(error) }",
        "",
        "#[derive(Clone, Copy)]",
        "struct EnvelopeSpec { model: &'static str, id: bool, method: bool, result: bool, error: bool, semantic_validator: Option<&'static str> }",
        "const ENVELOPES: &[EnvelopeSpec] = &[",
    ])
    for entry in schema["envelopes"]:
        semantic_validator = entry.get("semantic_validator")
        semantic = f"Some({rust_string(semantic_validator)})" if semantic_validator else "None"
        lines.append(f"    EnvelopeSpec {{ model: {rust_string(entry['model'])}, id: {str(entry['id'] == 'required').lower()}, method: {str(entry['method'] == 'required').lower()}, result: {str(entry['result'] == 'required').lower()}, error: {str(entry['error'] == 'required').lower()}, semantic_validator: {semantic} }},")
    lines.extend([
        "];",
        "pub fn parse_json_rpc_envelope(value: serde_json::Value) -> Result<JsonRpcEnvelope, ContractError> { let invalid = |error: ContractError| ContractError::named(\"invalid_request\", error.message); canonical_value(&value, 0).map_err(invalid)?; let object = value.as_object().ok_or_else(|| ContractError::named(\"invalid_request\", \"JSON-RPC envelope must be an object\"))?; let facts = (object.contains_key(\"id\"), object.contains_key(\"method\"), object.contains_key(\"result\"), object.contains_key(\"error\")); let spec = ENVELOPES.iter().find(|entry| (entry.id, entry.method, entry.result, entry.error) == facts).ok_or_else(|| ContractError::named(\"invalid_request\", \"JSON-RPC envelope has an invalid request/response shape\"))?; if let Some(method) = object.get(\"method\").and_then(serde_json::Value::as_str) { if let Some(method_spec) = method_spec(method) { if method_spec.notification != !facts.0 { return Err(ContractError::named(\"invalid_request\", \"JSON-RPC method id presence violates generated method semantics\")); } } } let parsed = match spec.model { \"JsonRpcRequest\" => parse_json_rpc_request(value).map(JsonRpcEnvelope::Request).map_err(invalid), \"JsonRpcNotification\" => parse_json_rpc_notification(value).map(JsonRpcEnvelope::Notification).map_err(invalid), \"JsonRpcSuccessResponse\" => parse_json_rpc_success_response(value).map(JsonRpcEnvelope::SuccessResponse).map_err(invalid), \"JsonRpcErrorResponse\" => parse_json_rpc_error_response(value).map(JsonRpcEnvelope::ErrorResponse).map_err(invalid), _ => Err(ContractError::named(\"internal_error\", \"unknown generated envelope\")) }?; match spec.semantic_validator { None => {}, Some(\"error_object\") => match &parsed { JsonRpcEnvelope::ErrorResponse(response) => validate_error_object(&response.error).map_err(invalid)?, _ => return Err(ContractError::named(\"internal_error\", \"error_object validator applied to a non-error envelope\")), }, Some(_) => return Err(ContractError::named(\"internal_error\", \"unknown generated envelope semantic validator\")), }; Ok(parsed) }",
        "",
    ])
    return "\n".join(lines).rstrip("\n") + "\n"


def render_python(schema: dict[str, Any], source_hash: str) -> str:
    """Render a dependency-free Python validator using the same model spec."""
    bounds = {entry["name"]: entry["value"] for entry in schema["bounds"]}
    required_capabilities = [entry["name"] for entry in schema["capabilities"] if entry["host_offer"] == "required"]
    optional_capabilities = [entry["name"] for entry in schema["capabilities"] if entry["host_offer"] == "optional"]
    required_methods = [entry["name"] for entry in schema["methods"] if entry["host_offer"] == "required"]
    optional_methods = [entry["name"] for entry in schema["methods"] if entry["host_offer"] == "optional"]
    records = [model for model in schema["models"] if model.get("kind") != "tagged_union"]
    tagged = [model for model in schema["models"] if model.get("kind") == "tagged_union"]
    model_specs = model_spec_literal(schema)
    errors = {entry["name"]: (entry["code"], entry["message"]) for entry in schema["errors"]}
    lines: list[str] = [
        '"""Generated canonical wire models and validation for Ygg extension API 0.3."""',
        "# @generated by scripts/generate-extension-api-v03.py; DO NOT EDIT.",
        f"# Source: protocol/extension-api-v0.3.schema.json (sha256: {source_hash})",
        "from __future__ import annotations",
        "",
        "import json",
        "from dataclasses import dataclass",
        "from typing import Any, Mapping, Optional, Union",
        "",
        f"API_VERSION = {schema['api_version']!r}",
        f"SCHEMA_ID = {schema['schema_id']!r}",
        f"CANONICAL_ENCODING = {schema['canonical_encoding']!r}",
        f"SCHEMA_SHA256 = {source_hash!r}",
        f"MAX_PORTABLE_JSON_INTEGER = {bounds['max_portable_json_integer']}",
    ]
    for bound in schema["bounds"]:
        if bound["name"] != "max_portable_json_integer":
            lines.append(f"{constant_name(bound['name'])} = {bound['value']}")
    lines.extend([
        f"API_VERSIONS = {tuple(schema['version_policy'])!r}",
        f"LEGACY_ADAPTERS = {tuple((entry['version'], entry['status'], entry['wire']) for entry in schema['legacy_adapters'])!r}",
        f"CAPABILITY_SPECS = {tuple((entry['name'], entry['host_offer'] == 'required', entry['host_offer'] != 'unavailable') for entry in schema['capabilities'])!r}",
        f"METHOD_SPECS = {tuple((entry['name'], entry['direction'], entry['capability'], entry['host_offer'] == 'required', entry['host_offer'] != 'unavailable', entry['params'], entry['result'], entry['terminal'], entry['notification']) for entry in schema['methods'])!r}",
        f"ERROR_SPECS = {errors!r}",
        f"DISPOSITION_SPECS = {tuple((entry['name'], entry['requires_reason']) for entry in schema['dispositions'])!r}",
        f"MODEL_SPECS = json.loads({model_specs!r})",
        f"ENVELOPE_SPECS = {tuple(schema['envelopes'])!r}",
        "",
        "class ContractError(ValueError):",
        "    def __init__(self, name: str, detail: str = '') -> None:",
        "        self.name = name",
        "        self.code, message = ERROR_SPECS[name]",
        "        self.message = f'{message}: {detail}' if detail else message",
        "        super().__init__(self.message)",
        "",
        "@dataclass(frozen=True)",
        "class Presence:",
        "    kind: str",
        "    value: Any = None",
        "    @classmethod",
        "    def absent(cls) -> 'Presence': return cls('absent')",
        "    @classmethod",
        "    def null(cls) -> 'Presence': return cls('null')",
        "    @classmethod",
        "    def present(cls, value: Any) -> 'Presence': return cls('value', value)",
        "    def is_absent(self) -> bool: return self.kind == 'absent'",
        "",
        "JsonRpcId = Union[int, str]",
        "",
        "def _utf8_bytes(value: str, label: str) -> bytes:",
        "    try: return value.encode('utf-8')",
        "    except UnicodeEncodeError as error: raise ContractError('invalid_params', f'{label} must be valid UTF-8') from error",
        "",
        "def _validate_canonical(value: Any, depth: int = 0) -> None:",
        "    if depth > MAX_JSON_DEPTH: raise ContractError('invalid_params', 'canonical JSON nesting exceeds max_json_depth')",
        "    if value is None or isinstance(value, bool): return",
        "    if isinstance(value, str): _utf8_bytes(value, 'canonical JSON string'); return",
        "    if isinstance(value, int):",
        "        if abs(value) > MAX_PORTABLE_JSON_INTEGER: raise ContractError('invalid_params', 'canonical JSON integer exceeds portable range')",
        "        return",
        "    if isinstance(value, float): raise ContractError('invalid_params', 'canonical JSON does not permit floating-point values')",
        "    if isinstance(value, list):",
        "        for item in value: _validate_canonical(item, depth + 1)",
        "        return",
        "    if isinstance(value, Mapping):",
        "        for key, item in value.items():",
        "            if not isinstance(key, str): raise ContractError('invalid_params', 'canonical JSON object keys must be strings')",
        "            _utf8_bytes(key, 'canonical JSON object key'); _validate_canonical(item, depth + 1)",
        "        return",
        "    raise ContractError('invalid_params', f'canonical JSON value is unsupported: {type(value).__name__}')",
        "",
        "def canonical_json(value: Any) -> str:",
        "    _validate_canonical(value)",
        "    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(',', ':'), allow_nan=False)",
        "",
        "def _object(value: Any, label: str) -> Mapping[str, Any]:",
        "    if not isinstance(value, Mapping): raise ContractError('invalid_params', f'{label} must be an object')",
        "    return value",
        "",
        "def _validate_type(type_name: str, value: Any) -> None:",
        "    if type_name.endswith('[]'):",
        "        if not isinstance(value, list): raise ContractError('invalid_params', f'expected {type_name}')",
        "        for item in value: _validate_type(type_name[:-2], item)",
        "        return",
        "    if type_name == 'string':",
        "        if not isinstance(value, str): raise ContractError('invalid_params', 'expected string')",
        "        _utf8_bytes(value, 'string'); return",
        "    if type_name == 'boolean':",
        "        if not isinstance(value, bool): raise ContractError('invalid_params', 'expected boolean')",
        "        return",
        "    if type_name == 'integer':",
        "        if not isinstance(value, int) or isinstance(value, bool) or value < 0: raise ContractError('invalid_params', 'expected unsigned integer')",
        "        return",
        "    if type_name == 'signed_integer':",
        "        if not isinstance(value, int) or isinstance(value, bool): raise ContractError('invalid_params', 'expected signed integer')",
        "        return",
        "    if type_name == 'json': _validate_canonical(value); return",
        "    if type_name == 'disposition':",
        "        if not isinstance(value, str): raise ContractError('invalid_params', 'expected disposition string')",
        "        return",
        "    if type_name == 'rpc_id':",
        "        if isinstance(value, str) and value and len(_utf8_bytes(value, 'JSON-RPC id')) <= MAX_JSON_RPC_ID_BYTES: return",
        "        if isinstance(value, int) and not isinstance(value, bool) and value >= 0: return",
        "        raise ContractError('invalid_request', 'JSON-RPC id must be a bounded string or unsigned integer')",
        "    _validate_model_wire(type_name, value)",
        "",
        "def _validate_record(name: str, spec: Mapping[str, Any], value: Any) -> None:",
        "    obj = _object(value, name); fields = spec['fields']; expected = {field['name'] for field in fields}",
        "    unknown = set(obj) - expected",
        "    if unknown: raise ContractError('invalid_params', f'{name} has unknown fields: {sorted(unknown)}')",
        "    for field in fields:",
        "        field_name = field['name']",
        "        if field_name not in obj:",
        "            if field['presence'] == 'required': raise ContractError('invalid_params', f'{name}.{field_name} is required')",
        "            continue",
        "        item = obj[field_name]",
        "        if item is None:",
        "            if not field['nullable']: raise ContractError('invalid_params', f'{name}.{field_name} must not be null')",
        "            continue",
        "        _validate_type(field['type'], item)",
        "        if field.get('max_items') is not None and len(item) > globals()[constant_name(field['max_items'])]: raise ContractError('resource_exhausted', f'{name}.{field_name} exceeds item bound')",
        "        if field.get('max_bytes') is not None:",
        "            values = item if field['type'] == 'string[]' else [item]",
        "            if any(len(_utf8_bytes(value, field_name)) > globals()[constant_name(field['max_bytes'])] for value in values): raise ContractError('resource_exhausted', f'{name}.{field_name} exceeds byte bound')",
        "        if field.get('values') and item not in field['values']: raise ContractError('invalid_params', f'{name}.{field_name} has unsupported value')",
        "",
        "def _validate_model_wire(name: str, value: Any) -> None:",
        "    _validate_canonical(value)",
        "    spec = MODEL_SPECS.get(name)",
        "    if spec is None: raise ContractError('invalid_params', f'unknown generated model {name}')",
        "    if spec['kind'] == 'record': _validate_record(name, spec, value); return",
        "    obj = _object(value, name); tag = spec['tag']; wire = obj.get(tag)",
        "    for variant in spec['variants']:",
        "        if variant['wire'] == wire:",
        "            if variant.get('status', 'foundation') != 'foundation': raise ContractError('capability_mismatch', f'{name} variant {wire} is deferred')",
        "            fields = [{'name': tag, 'type': 'string', 'presence': 'required', 'nullable': False, 'values': [wire]}, *variant['fields']]",
        "            _validate_record(name, {'fields': fields}, value); return",
        "    raise ContractError('invalid_params', f'unknown {name} variant')",
        "",
        "def _decode_field(type_name: str, value: Any) -> Any:",
        "    if type_name.endswith('[]'): return [_decode_field(type_name[:-2], item) for item in value]",
        "    if type_name in {'string', 'integer', 'signed_integer', 'boolean', 'json', 'disposition', 'rpc_id'}: return value",
        "    return globals()[f'parse_{snake(type_name)}'](value)",
        "",
        "def _decode_model(name: str, value: Any) -> dict[str, Any]:",
        "    _validate_model_wire(name, value); obj = _object(value, name); decoded: dict[str, Any] = {}",
        "    for field in MODEL_SPECS[name]['fields']:",
        "        field_name = field['name']",
        "        if field_name not in obj:",
        "            decoded[field_name] = Presence.absent() if field['presence'] == 'optional' and field['nullable'] else None",
        "        elif obj[field_name] is None:",
        "            decoded[field_name] = Presence.null() if field['presence'] == 'optional' and field['nullable'] else None",
        "        else:",
        "            item = _decode_field(field['type'], obj[field_name])",
        "            decoded[field_name] = Presence.present(item) if field['presence'] == 'optional' and field['nullable'] else item",
        "    return decoded",
        "",
        "def _encode_field(type_name: str, value: Any) -> Any:",
        "    if type_name.endswith('[]'): return [_encode_field(type_name[:-2], item) for item in value]",
        "    if type_name in {'string', 'integer', 'signed_integer', 'boolean', 'json', 'disposition', 'rpc_id'}: return value",
        "    return value.to_wire()",
        "",
        "def _encode_model(name: str, instance: Any) -> dict[str, Any]:",
        "    output: dict[str, Any] = {}",
        "    for field in MODEL_SPECS[name]['fields']:",
        "        value = getattr(instance, field['name'])",
        "        if field['presence'] == 'optional' and field['nullable']:",
        "            if value.is_absent(): continue",
        "            output[field['name']] = None if value.kind == 'null' else _encode_field(field['type'], value.value); continue",
        "        if field['presence'] == 'optional' and value is None: continue",
        "        output[field['name']] = _encode_field(field['type'], value)",
        "    return output",
        "",
    ])
    for model in records:
        lines.extend(["@dataclass(frozen=True)", f"class {model['name']}:"])
        if not model["fields"]:
            lines.append("    pass")
        for field in model["fields"]:
            annotation = python_type(field["type"])
            if is_presence_aware(field):
                annotation = "Presence"
                lines.append(f"    {field['name']}: {annotation} = Presence.absent()")
            elif is_optional(field):
                lines.append(f"    {field['name']}: Optional[{annotation}] = None")
            elif field.get("nullable", False):
                lines.append(f"    {field['name']}: Optional[{annotation}]")
            else:
                lines.append(f"    {field['name']}: {annotation}")
        lines.extend([
            "    @classmethod",
            f"    def from_wire(cls, value: Any) -> '{model['name']}': return cls(**_decode_model('{model['name']}', value))",
            f"    def to_wire(self) -> dict[str, Any]: value = _encode_model('{model['name']}', self); _validate_model_wire('{model['name']}', value); return value",
            "",
        ])
    for model in tagged:
        variants: list[str] = []
        for variant in model["variants"]:
            cls = f"{model['name']}{variant['name']}"
            variants.append(cls)
            lines.extend(["@dataclass(frozen=True)", f"class {cls}:"])
            for field in variant["fields"]:
                annotation = python_type(field["type"])
                if is_presence_aware(field):
                    lines.append(f"    {field['name']}: Presence = Presence.absent()")
                elif is_optional(field):
                    lines.append(f"    {field['name']}: Optional[{annotation}] = None")
                elif field.get("nullable", False):
                    lines.append(f"    {field['name']}: Optional[{annotation}]")
                else:
                    lines.append(f"    {field['name']}: {annotation}")
            lines.extend([
                f"    wire_type: str = {variant['wire']!r}",
                "    def to_wire(self) -> dict[str, Any]:",
                f"        output: dict[str, Any] = {{{model['tag']!r}: self.wire_type}}",
            ])
            for field in variant["fields"]:
                lines.append(f"        value = self.{field['name']}")
                if is_presence_aware(field):
                    lines.append("        if not value.is_absent(): output[" + repr(field["name"]) + "] = None if value.kind == 'null' else _encode_field(" + repr(field["type"]) + ", value.value)")
                elif is_optional(field):
                    lines.append("        if value is not None: output[" + repr(field["name"]) + "] = _encode_field(" + repr(field["type"]) + ", value)")
                else:
                    lines.append("        output[" + repr(field["name"]) + "] = _encode_field(" + repr(field["type"]) + ", value)")
            lines.extend(["        _validate_model_wire(" + repr(model["name"]) + ", output); return output", ""])
        lines.append(f"{model['name']} = Union[{', '.join(variants)}]")
        lines.append("")
        func = snake(model["name"])
        lines.extend([
            f"def parse_{func}(value: Any) -> {model['name']}:",
            f"    _validate_model_wire('{model['name']}', value); obj = _object(value, '{model['name']}'); wire = obj[{model['tag']!r}]",
        ])
        for variant in model["variants"]:
            cls = f"{model['name']}{variant['name']}"
            lines.append(f"    if wire == {variant['wire']!r}:")
            args = []
            for field in variant["fields"]:
                expr = f"_decode_field({field['type']!r}, obj[{field['name']!r}])"
                if is_presence_aware(field):
                    expr = f"Presence.null() if obj.get({field['name']!r}) is None else Presence.present({expr})"
                elif is_optional(field):
                    expr = f"None if {field['name']!r} not in obj else {expr}"
                args.append(f"{field['name']}={expr}")
            lines.append(f"        return {cls}({', '.join(args)})")
        lines.append("    raise ContractError('invalid_params', 'unknown generated tagged union')")
        lines.append("")
    for model in records:
        lines.append(f"def parse_{snake(model['name'])}(value: Any) -> {model['name']}: return {model['name']}.from_wire(value)")
    lines.extend([
        "",
        "@dataclass(frozen=True)",
        "class NegotiatedContract:",
        "    capabilities: frozenset[str]",
        "    methods: frozenset[str]",
        "    limits: ProtocolLimits",
        "",
        "def api_version_spec(version: str) -> Optional[dict[str, Any]]:",
        "    return next((entry for entry in API_VERSIONS if entry['version'] == version), None)",
        "def runtime_supports_api_version(version: str) -> bool:",
        "    entry = api_version_spec(version); return bool(entry and entry['runtime'] == 'supported')",
        "def bundle_supports_api_version(version: str) -> bool:",
        "    entry = api_version_spec(version); return bool(entry and entry['bundles'] == 'supported')",
        "",
        "def _named_list(values: list[str], limit: int, byte_limit: int, kind: str, known: set[str]) -> set[str]:",
        "    if not isinstance(values, list): raise ContractError('invalid_params', f'{kind} names must be an array')",
        "    if len(values) > limit: raise ContractError('resource_exhausted', f'{kind} count exceeds {limit}')",
        "    result: set[str] = set()",
        "    for value in values:",
        "        if not isinstance(value, str) or not value or len(_utf8_bytes(value, f'{kind} name')) > byte_limit: raise ContractError('invalid_params', f'invalid {kind} name {value!r}')",
        "        if value not in known: raise ContractError('capability_mismatch', f'unknown {kind} {value!r}')",
        "        if value in result: raise ContractError('capability_mismatch', f'duplicate {kind} {value!r}')",
        "        result.add(value)",
        "    return result",
        "",
        "def _validate_limits(limits: ProtocolLimits) -> None:",
        "    values = (limits.max_frame_bytes, limits.max_concurrent_requests, limits.max_tools)",
        "    if any(not isinstance(value, int) or isinstance(value, bool) or value <= 0 for value in values): raise ContractError('invalid_params', 'negotiated limits must be positive integers')",
        "    if limits.max_frame_bytes > MAX_FRAME_BYTES or limits.max_concurrent_requests > MAX_CONCURRENT_REQUESTS or limits.max_tools > MAX_TOOLS: raise ContractError('resource_exhausted', 'negotiated limit exceeds API 0.3 maximum')",
        "",
        "def _validate_available(capabilities: set[str], methods: set[str]) -> None:",
        "    available_capabilities = {name for name, _required, available in CAPABILITY_SPECS if available}",
        "    if not capabilities <= available_capabilities: raise ContractError('capability_mismatch', 'contract contains an unavailable capability')",
        "    specs = {name: (capability, available) for name, _direction, capability, _required, available, _params, _result, _terminal, _notification in METHOD_SPECS}",
        "    for method in methods:",
        "        capability, available = specs[method]",
        "        if not available or capability not in capabilities: raise ContractError('capability_mismatch', f'method {method!r} is unavailable or lacks its capability')",
        "",
        "def validate_offer(offer: ContractOffer) -> None:",
        "    offer.to_wire()",
        "    if offer.schema != SCHEMA_ID: raise ContractError('version_mismatch', f'expected schema {SCHEMA_ID}, received {offer.schema}')",
        "    if offer.encoding != CANONICAL_ENCODING: raise ContractError('invalid_params', f'expected encoding {CANONICAL_ENCODING}, received {offer.encoding}')",
        "    capability_names = {name for name, _required, _available in CAPABILITY_SPECS}",
        "    required = _named_list(offer.required_capabilities, MAX_CAPABILITIES, MAX_CAPABILITY_NAME_BYTES, 'capability', capability_names); optional = _named_list(offer.optional_capabilities, MAX_CAPABILITIES, MAX_CAPABILITY_NAME_BYTES, 'capability', capability_names)",
        "    if required & optional: raise ContractError('capability_mismatch', 'a capability is both required and optional')",
        f"    if required != set({required_capabilities!r}) or optional != set({optional_capabilities!r}): raise ContractError('capability_mismatch', 'host offer capability sets differ from generated API 0.3 contract')",
        "    method_names = {name for name, _direction, _capability, _required, _available, _params, _result, _terminal, _notification in METHOD_SPECS}",
        "    required_methods = _named_list(offer.required_methods, MAX_METHODS, MAX_METHOD_NAME_BYTES, 'method', method_names); optional_methods = _named_list(offer.optional_methods, MAX_METHODS, MAX_METHOD_NAME_BYTES, 'method', method_names)",
        "    if required_methods & optional_methods: raise ContractError('capability_mismatch', 'a method is both required and optional')",
        f"    if required_methods != set({required_methods!r}) or optional_methods != set({optional_methods!r}): raise ContractError('capability_mismatch', 'host offer method sets differ from generated API 0.3 contract')",
        "    _validate_available(required | optional, required_methods | optional_methods); _validate_limits(offer.limits)",
        "",
        "def validate_selection(selection: ContractSelection) -> None:",
        "    selection.to_wire()",
        "    if selection.schema != SCHEMA_ID: raise ContractError('version_mismatch', f'expected schema {SCHEMA_ID}, received {selection.schema}')",
        "    if selection.encoding != CANONICAL_ENCODING: raise ContractError('invalid_params', f'expected encoding {CANONICAL_ENCODING}, received {selection.encoding}')",
        "    caps = _named_list(selection.capabilities, MAX_CAPABILITIES, MAX_CAPABILITY_NAME_BYTES, 'capability', {name for name, _required, _available in CAPABILITY_SPECS})",
        "    methods = _named_list(selection.methods, MAX_METHODS, MAX_METHOD_NAME_BYTES, 'method', {name for name, _direction, _capability, _required, _available, _params, _result, _terminal, _notification in METHOD_SPECS})",
        "    _validate_available(caps, methods); _validate_limits(selection.limits)",
        "",
        "def host_offer(max_frame_bytes: int, max_concurrent_requests: int) -> ContractOffer:",
        "    if not isinstance(max_frame_bytes, int) or not isinstance(max_concurrent_requests, int) or max_frame_bytes <= 0 or max_concurrent_requests <= 0: raise ContractError('invalid_params', 'host limits must be positive integers')",
        f"    offer = ContractOffer(SCHEMA_ID, CANONICAL_ENCODING, {required_capabilities!r}, {optional_capabilities!r}, {required_methods!r}, {optional_methods!r}, ProtocolLimits(min(max_frame_bytes, MAX_FRAME_BYTES), min(max_concurrent_requests, MAX_CONCURRENT_REQUESTS), MAX_TOOLS))",
        "    validate_offer(offer); return offer",
        "def select_required(offer: ContractOffer) -> ContractSelection:",
        "    validate_offer(offer); return ContractSelection(offer.schema, offer.encoding, list(offer.required_capabilities), list(offer.required_methods), offer.limits)",
        "def negotiate(offer: ContractOffer, selection: ContractSelection) -> NegotiatedContract:",
        "    validate_offer(offer); validate_selection(selection)",
        "    offered_caps = set(offer.required_capabilities) | set(offer.optional_capabilities); selected_caps = set(selection.capabilities)",
        "    offered_methods = set(offer.required_methods) | set(offer.optional_methods); selected_methods = set(selection.methods)",
        "    if not selected_caps <= offered_caps or not set(offer.required_capabilities) <= selected_caps: raise ContractError('capability_mismatch', 'selection does not satisfy capability subset rules')",
        "    if not selected_methods <= offered_methods or not set(offer.required_methods) <= selected_methods: raise ContractError('capability_mismatch', 'selection does not satisfy method subset rules')",
        "    if selection.limits.max_frame_bytes > offer.limits.max_frame_bytes or selection.limits.max_concurrent_requests > offer.limits.max_concurrent_requests or selection.limits.max_tools > offer.limits.max_tools: raise ContractError('capability_mismatch', 'selection increases a host offer limit')",
        "    return NegotiatedContract(frozenset(selected_caps), frozenset(selected_methods), selection.limits)",
        "def method_is_available(contract: NegotiatedContract, name: str, direction: str) -> bool:",
        "    for method, method_direction, capability, _required, available, _params, _result, _terminal, _notification in METHOD_SPECS:",
        "        if method == name: return available and method_direction in {direction, 'bidirectional'} and name in contract.methods and capability in contract.capabilities",
        "    return False",
        "def require_method(contract: NegotiatedContract, name: str, direction: str) -> None:",
        "    if not method_is_available(contract, name, direction): raise ContractError('unknown_method', f'method {name!r} is unavailable for {direction}')",
        "",
        "def validate_disposition(disposition: Disposition) -> None:",
        "    wire = disposition.to_wire(); spec = dict(DISPOSITION_SPECS).get(wire['kind'])",
        "    if spec is None: raise ContractError('invalid_params', 'unknown disposition')",
        "    reason = wire.get('reason')",
        "    if reason is not None and (not reason or len(_utf8_bytes(reason, 'disposition reason')) > MAX_REASON_BYTES): raise ContractError('invalid_params', 'disposition reason is empty or exceeds max_reason_bytes')",
        "    if spec and reason is None: raise ContractError('invalid_params', 'disposition requires a reason')",
        "",
        "def validate_initialize_request(request: InitializeRequest) -> None:",
        "    request.to_wire();",
        "    if request.api_version != API_VERSION: raise ContractError('version_mismatch', f'expected API {API_VERSION}, received {request.api_version}')",
        "    validate_offer(request.contract)",
        "def validate_initialize_response(response: InitializeResponse) -> None:",
        "    response.to_wire();",
        "    if response.api_version != API_VERSION: raise ContractError('version_mismatch', f'expected API {API_VERSION}, received {response.api_version}')",
        "    validate_selection(response.contract)",
        "    if len(response.tools) > response.contract.limits.max_tools: raise ContractError('resource_exhausted', 'initialize tool catalog exceeds negotiated max_tools')",
        "def validate_tool_call_params(value: ToolCallParams) -> None: value.to_wire()",
        "def validate_tool_call_result(value: ToolCallResult) -> None: value.to_wire()",
        "def validate_cancel_request_params(value: CancelRequestParams) -> None: value.to_wire()",
        "def validate_shutdown_params(value: ShutdownParams) -> None: value.to_wire()",
        "def validate_shutdown_result(value: ShutdownResult) -> None: value.to_wire()",
        "def validate_error_object(error: ErrorObject) -> None:",
        "    error.to_wire(); expected = next(((code, message) for code, message in ERROR_SPECS.values() if code == error.code), None)",
        "    if expected is None or expected[1] != error.message: raise ContractError('invalid_params', 'error code/message is absent from API 0.3 table')",
        "def error_object(name: str, data: Presence = Presence.absent()) -> ErrorObject:",
        "    code, message = ERROR_SPECS[name]; error = ErrorObject(code, message, data); validate_error_object(error); return error",
        "def canonical_frame(value: Any, max_frame_bytes: int) -> bytes:",
        "    if not isinstance(max_frame_bytes, int) or isinstance(max_frame_bytes, bool) or max_frame_bytes <= 0: raise ContractError('invalid_params', 'max_frame_bytes must be a positive integer')",
        "    if max_frame_bytes > MAX_FRAME_BYTES: raise ContractError('resource_exhausted', 'max_frame_bytes exceeds API 0.3 maximum')",
        "    encoded = canonical_json(value).encode('utf-8')",
        "    if len(encoded) > max_frame_bytes: raise ContractError('resource_exhausted', 'canonical frame exceeds negotiated max_frame_bytes')",
        "    return encoded",
        "",
        "def parse_json_rpc_envelope(value: Any) -> Any:",
        "    try:",
        "        _validate_canonical(value); obj = _object(value, 'JSON-RPC envelope'); facts = {key: key in obj for key in ('id', 'method', 'result', 'error')}",
        "        matches = [entry for entry in ENVELOPE_SPECS if all(facts[key] == (entry[key] == 'required') for key in facts)]",
        "        if len(matches) != 1: raise ContractError('invalid_request', 'JSON-RPC envelope has an invalid request/response shape')",
        "        method = obj.get('method'); method_spec = next((entry for entry in METHOD_SPECS if entry[0] == method), None)",
        "        if method_spec is not None and method_spec[8] != ('id' not in obj): raise ContractError('invalid_request', 'JSON-RPC method id presence violates generated method semantics')",
        "        parsed = globals()[f\"parse_{snake(matches[0]['model'])}\"](value)",
        "        semantic_validator = matches[0].get('semantic_validator')",
        "        if semantic_validator == 'error_object': validate_error_object(parsed.error)",
        "        elif semantic_validator is not None: raise ContractError('internal_error', f'unknown generated envelope semantic validator {semantic_validator!r}')",
        "        return parsed",
        "    except ContractError as error:",
        "        if error.code == ERROR_SPECS['invalid_request'][0]: raise",
        "        raise ContractError('invalid_request', error.message) from error",
        "",
        "def constant_name(value: str) -> str: return value.upper()",
        "def snake(value: str) -> str: return re.sub(r'(?<!^)(?=[A-Z])', '_', value).lower()",
        "",
    ])
    # `re` is only needed by snake in generated module; keep import source-only concise.
    lines.insert(7, "import re")
    return "\n".join(lines).rstrip("\n") + "\n"


def render_typescript_types(schema: dict[str, Any], source_hash: str) -> str:
    bounds = {entry["name"]: entry["value"] for entry in schema["bounds"]}
    records = [model for model in schema["models"] if model.get("kind") != "tagged_union"]
    tagged = [model for model in schema["models"] if model.get("kind") == "tagged_union"]
    lines = [
        "// @generated by scripts/generate-extension-api-v03.py; DO NOT EDIT.",
        f"// Source: protocol/extension-api-v0.3.schema.json (sha256: {source_hash})",
        "export type JsonPrimitive = string | number | boolean | null;",
        "export type JsonValue = JsonPrimitive | JsonValue[] | { [key: string]: JsonValue };",
        "export type JsonRpcId = string | number;",
        "export type Presence<T> = { kind: 'absent' } | { kind: 'null' } | { kind: 'value'; value: T };",
        "export type DispositionKind = 'continue' | 'deny' | 'defer';",
        "",
    ]
    for name in ["API_VERSION", "SCHEMA_ID", "CANONICAL_ENCODING", "SCHEMA_SHA256"]:
        lines.append(f"export declare const {name}: string;")
    lines.append("export declare const MAX_PORTABLE_JSON_INTEGER: number;")
    for bound in schema["bounds"]:
        if bound["name"] != "max_portable_json_integer":
            lines.append(f"export declare const {constant_name(bound['name'])}: number;")
    lines.extend([
        "export declare const API_VERSIONS: readonly Record<string, unknown>[];",
        "export declare const LEGACY_ADAPTERS: readonly { version: string; status: string; wire: string }[];",
        "export declare const CAPABILITY_SPECS: readonly Record<string, unknown>[];",
        "export declare const METHOD_SPECS: readonly Record<string, unknown>[];",
        "export declare const ERROR_SPECS: Readonly<Record<string, { code: number; message: string }>>;",
        "export declare class ContractError extends Error { readonly code: number; readonly name: string; }",
        "export declare function absent<T = never>(): Presence<T>;",
        "export declare function nullPresence<T = never>(): Presence<T>;",
        "export declare function present<T>(value: T): Presence<T>;",
        "",
    ])
    for model in records:
        lines.append(f"export interface {model['name']} {{")
        for field in model["fields"]:
            ty = typescript_type(field["type"])
            optional = "?" if is_optional(field) else ""
            if is_presence_aware(field):
                ty = f"Presence<{ty}>"
            elif field.get("nullable", False):
                ty = f"{ty} | null"
            lines.append(f"  {field['name']}{optional}: {ty};")
        lines.append("}")
        lines.append("")
    for model in tagged:
        variants: list[str] = []
        for variant in model["variants"]:
            name = f"{model['name']}{variant['name']}"
            variants.append(name)
            lines.append(f"export interface {name} {{")
            lines.append(f"  {model['tag']}: {json.dumps(variant['wire'])};")
            for field in variant["fields"]:
                ty = typescript_type(field["type"])
                optional = "?" if is_optional(field) else ""
                if is_presence_aware(field): ty = f"Presence<{ty}>"
                elif field.get("nullable", False): ty = f"{ty} | null"
                lines.append(f"  {field['name']}{optional}: {ty};")
            lines.append("}")
            lines.append("")
        lines.append(f"export type {model['name']} = {' | '.join(variants)};")
        lines.append("")
    lines.extend([
        "export interface NegotiatedContract { capabilities: ReadonlySet<string>; methods: ReadonlySet<string>; limits: ProtocolLimits; }",
        "export type JsonRpcEnvelope = JsonRpcRequest | JsonRpcNotification | JsonRpcSuccessResponse | JsonRpcErrorResponse;",
        "",
    ])
    for model in [*records, *tagged]:
        name = model["name"]
        lines.append(f"export declare function parse{name}(value: unknown): {name};")
    lines.extend([
        "export declare function parseJsonRpcEnvelope(value: unknown): JsonRpcEnvelope;",
        "export declare function runtimeSupportsApiVersion(version: string): boolean;",
        "export declare function bundleSupportsApiVersion(version: string): boolean;",
        "export declare function validateOffer(value: ContractOffer): void;",
        "export declare function validateSelection(value: ContractSelection): void;",
        "export declare function hostOffer(maxFrameBytes: number, maxConcurrentRequests: number): ContractOffer;",
        "export declare function selectRequired(value: ContractOffer): ContractSelection;",
        "export declare function negotiate(offer: ContractOffer, selection: ContractSelection): NegotiatedContract;",
        "export declare function methodIsAvailable(contract: NegotiatedContract, name: string, direction: 'host_to_extension' | 'extension_to_host'): boolean;",
        "export declare function requireMethod(contract: NegotiatedContract, name: string, direction: 'host_to_extension' | 'extension_to_host'): void;",
        "export declare function validateDisposition(value: Disposition): void;",
        "export declare function validateInitializeRequest(value: InitializeRequest): void;",
        "export declare function validateInitializeResponse(value: InitializeResponse): void;",
        "export declare function validateToolCallParams(value: ToolCallParams): void;",
        "export declare function validateToolCallResult(value: ToolCallResult): void;",
        "export declare function validateCancelRequestParams(value: CancelRequestParams): void;",
        "export declare function validateShutdownParams(value: ShutdownParams): void;",
        "export declare function validateShutdownResult(value: ShutdownResult): void;",
        "export declare function validateErrorObject(value: ErrorObject): void;",
        "export declare function canonicalJson(value: JsonValue): string;",
        "export declare function canonicalFrame(value: JsonValue, maxFrameBytes: number): Uint8Array;",
        "export declare function errorObject(name: keyof typeof ERROR_SPECS, data?: Presence<JsonValue>): ErrorObject;",
        "",
    ])
    return "\n".join(lines)


def render_typescript_runtime(schema: dict[str, Any], source_hash: str) -> str:
    """Render ESM runtime; data tables and all shape checks come from schema."""
    bounds = {entry["name"]: entry["value"] for entry in schema["bounds"]}
    required_capabilities = [entry["name"] for entry in schema["capabilities"] if entry["host_offer"] == "required"]
    optional_capabilities = [entry["name"] for entry in schema["capabilities"] if entry["host_offer"] == "optional"]
    required_methods = [entry["name"] for entry in schema["methods"] if entry["host_offer"] == "required"]
    optional_methods = [entry["name"] for entry in schema["methods"] if entry["host_offer"] == "optional"]
    errors = {entry["name"]: {"code": entry["code"], "message": entry["message"]} for entry in schema["errors"]}
    model_specs = model_spec_literal(schema)
    lines = [
        "// @generated by scripts/generate-extension-api-v03.py; DO NOT EDIT.",
        f"// Source: protocol/extension-api-v0.3.schema.json (sha256: {source_hash})",
        f"const API_VERSION = {json.dumps(schema['api_version'])};",
        f"const SCHEMA_ID = {json.dumps(schema['schema_id'])};",
        f"const CANONICAL_ENCODING = {json.dumps(schema['canonical_encoding'])};",
        f"const SCHEMA_SHA256 = {json.dumps(source_hash)};",
        f"const MAX_PORTABLE_JSON_INTEGER = {bounds['max_portable_json_integer']};",
    ]
    for bound in schema["bounds"]:
        if bound["name"] != "max_portable_json_integer": lines.append(f"const {constant_name(bound['name'])} = {bound['value']};")
    lines.extend([
        f"const API_VERSIONS = {json.dumps(schema['version_policy'])};",
        f"const LEGACY_ADAPTERS = {json.dumps(schema['legacy_adapters'])};",
        f"const CAPABILITY_SPECS = {json.dumps([{'name': entry['name'], 'required': entry['host_offer'] == 'required', 'available': entry['host_offer'] != 'unavailable'} for entry in schema['capabilities']])};",
        f"const METHOD_SPECS = {json.dumps([{'name': entry['name'], 'direction': entry['direction'], 'capability': entry['capability'], 'required': entry['host_offer'] == 'required', 'available': entry['host_offer'] != 'unavailable', 'params': entry['params'], 'result': entry['result'], 'terminal': entry['terminal'], 'notification': entry['notification']} for entry in schema['methods']])};",
        f"const ERROR_SPECS = {json.dumps(errors, sort_keys=True)};",
        f"const DISPOSITION_SPECS = {json.dumps({entry['name']: entry['requires_reason'] for entry in schema['dispositions']})};",
        f"const MODEL_SPECS = {model_specs};",
        f"const ENVELOPE_SPECS = {json.dumps(schema['envelopes'])};",
        "",
        "class ContractError extends Error { constructor(semantic, detail = '') { const spec = ERROR_SPECS[semantic]; super(detail ? `${spec.message}: ${detail}` : spec.message); this.name = 'ContractError'; this.code = spec.code; } }",
        "const PRESENCE = Symbol('ygg.extension.api_v03.presence'); function _presence(kind, value) { const result = kind === 'value' ? { kind, value } : { kind }; Object.defineProperty(result, PRESENCE, { value: true }); return result; } function _isPresence(value) { return Boolean(value && typeof value === 'object' && value[PRESENCE] === true); }",
        "const absent = () => _presence('absent'); const nullPresence = () => _presence('null'); const present = (value) => _presence('value', value);",
        "function _utf8Bytes(value, label) { for (let index = 0; index < value.length; index += 1) { const unit = value.charCodeAt(index); if (unit >= 0xd800 && unit <= 0xdbff) { if (index + 1 >= value.length) throw new ContractError('invalid_params', `${label} must be valid UTF-8`); const next = value.charCodeAt(index + 1); if (next < 0xdc00 || next > 0xdfff) throw new ContractError('invalid_params', `${label} must be valid UTF-8`); index += 1; } else if (unit >= 0xdc00 && unit <= 0xdfff) throw new ContractError('invalid_params', `${label} must be valid UTF-8`); } return new TextEncoder().encode(value); }",
        "function _object(value, label) { if (value === null || typeof value !== 'object' || Array.isArray(value)) throw new ContractError('invalid_params', `${label} must be an object`); return value; }",
        "function _validateCanonical(value, depth = 0) { if (depth > MAX_JSON_DEPTH) throw new ContractError('invalid_params', 'canonical JSON nesting exceeds max_json_depth'); if (value === null || typeof value === 'boolean') return; if (typeof value === 'string') { _utf8Bytes(value, 'canonical JSON string'); return; } if (typeof value === 'number') { if (!Number.isSafeInteger(value) || Math.abs(value) > MAX_PORTABLE_JSON_INTEGER) throw new ContractError('invalid_params', 'canonical JSON permits only portable integers'); return; } if (Array.isArray(value)) { for (const item of value) _validateCanonical(item, depth + 1); return; } if (typeof value === 'object') { const prototype = Object.getPrototypeOf(value); if (prototype !== Object.prototype && prototype !== null) throw new ContractError('invalid_params', 'canonical JSON objects must be plain objects'); for (const [key, item] of Object.entries(value)) { _utf8Bytes(key, 'canonical JSON object key'); _validateCanonical(item, depth + 1); } return; } throw new ContractError('invalid_params', `canonical JSON value is unsupported: ${typeof value}`); }",
        "function _validateType(typeName, value) { if (typeName.endsWith('[]')) { if (!Array.isArray(value)) throw new ContractError('invalid_params', `expected ${typeName}`); for (const item of value) _validateType(typeName.slice(0, -2), item); return; } if (typeName === 'string') { if (typeof value !== 'string') throw new ContractError('invalid_params', 'expected string'); _utf8Bytes(value, 'string'); return; } if (typeName === 'boolean') { if (typeof value !== 'boolean') throw new ContractError('invalid_params', 'expected boolean'); return; } if (typeName === 'integer') { if (!Number.isSafeInteger(value) || value < 0) throw new ContractError('invalid_params', 'expected unsigned integer'); return; } if (typeName === 'signed_integer') { if (!Number.isSafeInteger(value)) throw new ContractError('invalid_params', 'expected signed integer'); return; } if (typeName === 'json') { _validateCanonical(value); return; } if (typeName === 'disposition') { if (typeof value !== 'string') throw new ContractError('invalid_params', 'expected disposition string'); return; } if (typeName === 'rpc_id') { if ((typeof value === 'string' && value && _utf8Bytes(value, 'JSON-RPC id').length <= MAX_JSON_RPC_ID_BYTES) || (typeof value === 'number' && Number.isSafeInteger(value) && value >= 0)) return; throw new ContractError('invalid_request', 'JSON-RPC id must be a bounded string or unsigned integer'); } _validateModel(typeName, value); }",
        "function _validateRecord(name, fields, value) { const obj = _object(value, name); const expected = new Set(fields.map((field) => field.name)); for (const key of Object.keys(obj)) if (!expected.has(key)) throw new ContractError('invalid_params', `${name} has unknown field ${key}`); for (const field of fields) { if (!(field.name in obj)) { if (field.presence === 'required') throw new ContractError('invalid_params', `${name}.${field.name} is required`); continue; } const item = obj[field.name]; if (item === null) { if (!field.nullable) throw new ContractError('invalid_params', `${name}.${field.name} must not be null`); continue; } _validateType(field.type, item); if (field.max_items && item.length > globalThis[field.max_items.toUpperCase()]) throw new ContractError('resource_exhausted', `${name}.${field.name} exceeds item bound`); if (field.max_bytes && (Array.isArray(item) ? item : [item]).some((entry) => _utf8Bytes(entry, field.name).length > globalThis[field.max_bytes.toUpperCase()])) throw new ContractError('resource_exhausted', `${name}.${field.name} exceeds byte bound`); if (field.values?.length && !field.values.includes(item)) throw new ContractError('invalid_params', `${name}.${field.name} has unsupported value`); } }",
        "function _bound(name) { return ({ " + ", ".join(f"{entry['name']}: {constant_name(entry['name'])}" for entry in schema["bounds"]) + " })[name]; }",
        "function _validateModel(name, value) { _validateCanonical(value); const spec = MODEL_SPECS[name]; if (!spec) throw new ContractError('invalid_params', `unknown generated model ${name}`); if (spec.kind === 'record') { _validateRecord(name, spec.fields, value); return; } const obj = _object(value, name); const wire = obj[spec.tag]; const variant = spec.variants.find((entry) => entry.wire === wire); if (!variant) throw new ContractError('invalid_params', `unknown ${name} variant`); if ((variant.status ?? 'foundation') !== 'foundation') throw new ContractError('capability_mismatch', `${name} variant ${wire} is deferred`); _validateRecord(name, [{ name: spec.tag, type: 'string', presence: 'required', nullable: false, values: [wire] }, ...variant.fields], value); }",
        "function _decodeField(typeName, value) { if (typeName.endsWith('[]')) return value.map((item) => _decodeField(typeName.slice(0, -2), item)); if (['string', 'integer', 'signed_integer', 'boolean', 'json', 'disposition', 'rpc_id'].includes(typeName)) return value; return parseModel(typeName, value); }",
        "function parseModel(name, value) { _validateModel(name, value); const spec = MODEL_SPECS[name]; const result = {}; let fields = spec.fields; if (spec.kind === 'tagged_union') { const variant = spec.variants.find((entry) => entry.wire === value[spec.tag]); result[spec.tag] = value[spec.tag]; fields = variant.fields; } for (const field of fields) { if (!(field.name in value)) { if (field.presence === 'optional' && field.nullable) result[field.name] = absent(); continue; } if (value[field.name] === null) { result[field.name] = field.presence === 'optional' && field.nullable ? nullPresence() : null; continue; } const item = _decodeField(field.type, value[field.name]); result[field.name] = field.presence === 'optional' && field.nullable ? present(item) : item; } return result; }",
    ])
    for model in schema["models"]:
        lines.append(f"function parse{model['name']}(value) {{ return parseModel({json.dumps(model['name'])}, value); }}")
    lines.extend([
        "const ABSENT_WIRE = Symbol('ygg.extension.api_v03.absent_wire'); function _wire(value) { if (_isPresence(value)) { if (value.kind === 'absent') return ABSENT_WIRE; if (value.kind === 'null') return null; return _wire(value.value); } if (Array.isArray(value)) return value.map((item) => { const wire = _wire(item); return wire === ABSENT_WIRE ? undefined : wire; }); if (value && typeof value === 'object') { const output = {}; for (const [key, item] of Object.entries(value)) { const wire = _wire(item); if (wire !== ABSENT_WIRE) output[key] = wire; } return output; } return value; }",
        "function _namedList(values, limit, bytes, kind, known) { if (!Array.isArray(values)) throw new ContractError('invalid_params', `${kind} names must be an array`); if (values.length > limit) throw new ContractError('resource_exhausted', `${kind} count exceeds ${limit}`); const result = new Set(); for (const value of values) { if (typeof value !== 'string' || !value || _utf8Bytes(value, `${kind} name`).length > bytes) throw new ContractError('invalid_params', `invalid ${kind} name`); if (!known.has(value) || result.has(value)) throw new ContractError('capability_mismatch', `unknown or duplicate ${kind}`); result.add(value); } return result; }",
        "function _validateLimits(limits) { if (!Number.isSafeInteger(limits.max_frame_bytes) || !Number.isSafeInteger(limits.max_concurrent_requests) || !Number.isSafeInteger(limits.max_tools) || limits.max_frame_bytes <= 0 || limits.max_concurrent_requests <= 0 || limits.max_tools <= 0) throw new ContractError('invalid_params', 'negotiated limits must be positive integers'); if (limits.max_frame_bytes > MAX_FRAME_BYTES || limits.max_concurrent_requests > MAX_CONCURRENT_REQUESTS || limits.max_tools > MAX_TOOLS) throw new ContractError('resource_exhausted', 'negotiated limit exceeds API 0.3 maximum'); }",
        "function _validateAvailable(caps, methods) { const availableCaps = new Set(CAPABILITY_SPECS.filter((entry) => entry.available).map((entry) => entry.name)); if (![...caps].every((value) => availableCaps.has(value))) throw new ContractError('capability_mismatch', 'contract contains unavailable capability'); for (const method of methods) { const spec = METHOD_SPECS.find((entry) => entry.name === method); if (!spec || !spec.available || !caps.has(spec.capability)) throw new ContractError('capability_mismatch', 'method is unavailable or lacks its capability'); } }",
        "function validateOffer(offer) { const value = parseContractOffer(_wire(offer)); if (value.schema !== SCHEMA_ID) throw new ContractError('version_mismatch', 'schema mismatch'); if (value.encoding !== CANONICAL_ENCODING) throw new ContractError('invalid_params', 'encoding mismatch'); const names = new Set(CAPABILITY_SPECS.map((entry) => entry.name)); const required = _namedList(value.required_capabilities, MAX_CAPABILITIES, MAX_CAPABILITY_NAME_BYTES, 'capability', names); const optional = _namedList(value.optional_capabilities, MAX_CAPABILITIES, MAX_CAPABILITY_NAME_BYTES, 'capability', names); if ([...required].some((name) => optional.has(name))) throw new ContractError('capability_mismatch', 'capability is both required and optional'); if (required.size !== " + str(len(required_capabilities)) + " || !" + json.dumps(required_capabilities) + ".every((name) => required.has(name)) || optional.size !== " + str(len(optional_capabilities)) + ") throw new ContractError('capability_mismatch', 'host offer capability sets differ from generated API 0.3 contract'); const methodNames = new Set(METHOD_SPECS.map((entry) => entry.name)); const requiredMethods = _namedList(value.required_methods, MAX_METHODS, MAX_METHOD_NAME_BYTES, 'method', methodNames); const optionalMethods = _namedList(value.optional_methods, MAX_METHODS, MAX_METHOD_NAME_BYTES, 'method', methodNames); if ([...requiredMethods].some((name) => optionalMethods.has(name))) throw new ContractError('capability_mismatch', 'method is both required and optional'); if (requiredMethods.size !== " + str(len(required_methods)) + " || !" + json.dumps(required_methods) + ".every((name) => requiredMethods.has(name)) || optionalMethods.size !== " + str(len(optional_methods)) + ") throw new ContractError('capability_mismatch', 'host offer method sets differ from generated API 0.3 contract'); _validateAvailable(new Set([...required, ...optional]), new Set([...requiredMethods, ...optionalMethods])); _validateLimits(value.limits); }",
        "function validateSelection(selection) { const value = parseContractSelection(_wire(selection)); if (value.schema !== SCHEMA_ID) throw new ContractError('version_mismatch', 'schema mismatch'); if (value.encoding !== CANONICAL_ENCODING) throw new ContractError('invalid_params', 'encoding mismatch'); const caps = _namedList(value.capabilities, MAX_CAPABILITIES, MAX_CAPABILITY_NAME_BYTES, 'capability', new Set(CAPABILITY_SPECS.map((entry) => entry.name))); const methods = _namedList(value.methods, MAX_METHODS, MAX_METHOD_NAME_BYTES, 'method', new Set(METHOD_SPECS.map((entry) => entry.name))); _validateAvailable(caps, methods); _validateLimits(value.limits); }",
        f"function hostOffer(maxFrameBytes, maxConcurrentRequests) {{ if (!Number.isSafeInteger(maxFrameBytes) || !Number.isSafeInteger(maxConcurrentRequests) || maxFrameBytes <= 0 || maxConcurrentRequests <= 0) throw new ContractError('invalid_params', 'host limits must be positive integers'); const offer = {{ schema: SCHEMA_ID, encoding: CANONICAL_ENCODING, required_capabilities: {json.dumps(required_capabilities)}, optional_capabilities: {json.dumps(optional_capabilities)}, required_methods: {json.dumps(required_methods)}, optional_methods: {json.dumps(optional_methods)}, limits: {{ max_frame_bytes: Math.min(maxFrameBytes, MAX_FRAME_BYTES), max_concurrent_requests: Math.min(maxConcurrentRequests, MAX_CONCURRENT_REQUESTS), max_tools: MAX_TOOLS }} }}; validateOffer(offer); return offer; }}",
        "function selectRequired(offer) { validateOffer(offer); return { schema: offer.schema, encoding: offer.encoding, capabilities: [...offer.required_capabilities], methods: [...offer.required_methods], limits: { ...offer.limits } }; }",
        "function negotiate(offer, selection) { validateOffer(offer); validateSelection(selection); const caps = new Set(selection.capabilities); const methods = new Set(selection.methods); const offeredCaps = new Set([...offer.required_capabilities, ...offer.optional_capabilities]); const offeredMethods = new Set([...offer.required_methods, ...offer.optional_methods]); if (![...caps].every((value) => offeredCaps.has(value)) || !offer.required_capabilities.every((value) => caps.has(value)) || ![...methods].every((value) => offeredMethods.has(value)) || !offer.required_methods.every((value) => methods.has(value))) throw new ContractError('capability_mismatch', 'selection violates subset rules'); if (selection.limits.max_frame_bytes > offer.limits.max_frame_bytes || selection.limits.max_concurrent_requests > offer.limits.max_concurrent_requests || selection.limits.max_tools > offer.limits.max_tools) throw new ContractError('capability_mismatch', 'selection increases a host offer limit'); return { capabilities: caps, methods, limits: { ...selection.limits } }; }",
        "function methodIsAvailable(contract, name, direction) { const spec = METHOD_SPECS.find((entry) => entry.name === name); return Boolean(spec && spec.available && (spec.direction === direction || spec.direction === 'bidirectional') && contract.methods.has(name) && contract.capabilities.has(spec.capability)); }",
        "function requireMethod(contract, name, direction) { if (!methodIsAvailable(contract, name, direction)) throw new ContractError('unknown_method', `method ${JSON.stringify(name)} is unavailable for ${direction}`); }",
        "function validateDisposition(value) { const parsed = parseDisposition(_wire(value)); const spec = DISPOSITION_SPECS[parsed.kind]; if (spec === undefined) throw new ContractError('invalid_params', 'unknown disposition'); if (parsed.reason !== undefined && (parsed.reason === null || !parsed.reason || _utf8Bytes(parsed.reason, 'disposition reason').length > MAX_REASON_BYTES)) throw new ContractError('invalid_params', 'disposition reason is empty or exceeds max_reason_bytes'); if (spec && parsed.reason === undefined) throw new ContractError('invalid_params', 'disposition requires a reason'); }",
        "function validateInitializeRequest(value) { const parsed = parseInitializeRequest(_wire(value)); if (parsed.api_version !== API_VERSION) throw new ContractError('version_mismatch', 'API version mismatch'); validateOffer(parsed.contract); }",
        "function validateInitializeResponse(value) { const parsed = parseInitializeResponse(_wire(value)); if (parsed.api_version !== API_VERSION) throw new ContractError('version_mismatch', 'API version mismatch'); validateSelection(parsed.contract); if (parsed.tools.length > parsed.contract.limits.max_tools) throw new ContractError('resource_exhausted', 'initialize tool catalog exceeds negotiated max_tools'); }",
        "function validateToolCallParams(value) { parseToolCallParams(_wire(value)); } function validateToolCallResult(value) { parseToolCallResult(_wire(value)); } function validateCancelRequestParams(value) { parseCancelRequestParams(_wire(value)); } function validateShutdownParams(value) { parseShutdownParams(_wire(value)); } function validateShutdownResult(value) { parseShutdownResult(_wire(value)); }",
        "function validateErrorObject(value) { const parsed = parseErrorObject(_wire(value)); const spec = Object.values(ERROR_SPECS).find((entry) => entry.code === parsed.code); if (!spec || spec.message !== parsed.message) throw new ContractError('invalid_params', 'error code/message is absent from API 0.3 table'); }",
        "function canonicalJson(value) { _validateCanonical(value); const encode = (item) => { if (item === null || typeof item === 'string' || typeof item === 'boolean' || typeof item === 'number') return JSON.stringify(item); if (Array.isArray(item)) return `[${item.map(encode).join(',')}]`; return `{${Object.keys(item).sort((left, right) => { const a = _utf8Bytes(left, 'canonical JSON object key'); const b = _utf8Bytes(right, 'canonical JSON object key'); for (let i = 0; i < Math.min(a.length, b.length); i += 1) if (a[i] !== b[i]) return a[i] - b[i]; return a.length - b.length; }).map((key) => `${JSON.stringify(key)}:${encode(item[key])}`).join(',')}}`; }; return encode(value); }",
        "function canonicalFrame(value, maxFrameBytes) { if (!Number.isSafeInteger(maxFrameBytes) || maxFrameBytes <= 0) throw new ContractError('invalid_params', 'maxFrameBytes must be positive integer'); if (maxFrameBytes > MAX_FRAME_BYTES) throw new ContractError('resource_exhausted', 'maxFrameBytes exceeds API 0.3 maximum'); const bytes = _utf8Bytes(canonicalJson(value), 'canonical frame'); if (bytes.length > maxFrameBytes) throw new ContractError('resource_exhausted', 'canonical frame exceeds negotiated max_frame_bytes'); return bytes; }",
        "function errorObject(name, data = absent()) { const spec = ERROR_SPECS[name]; const value = { code: spec.code, message: spec.message }; if (data.kind !== 'absent') value.data = data.kind === 'null' ? null : data.value; validateErrorObject(value); return parseErrorObject(value); }",
        "function parseJsonRpcEnvelope(value) { try { _validateCanonical(value); const obj = _object(value, 'JSON-RPC envelope'); const matches = ENVELOPE_SPECS.filter((entry) => ['id', 'method', 'result', 'error'].every((key) => (key in obj) === (entry[key] === 'required'))); if (matches.length !== 1) throw new ContractError('invalid_request', 'JSON-RPC envelope has an invalid request/response shape'); const methodSpec = METHOD_SPECS.find((entry) => entry.name === obj.method); if (methodSpec && methodSpec.notification !== !('id' in obj)) throw new ContractError('invalid_request', 'JSON-RPC method id presence violates generated method semantics'); const parsed = parseModel(matches[0].model, value); const semanticValidator = matches[0].semantic_validator; if (semanticValidator === 'error_object') validateErrorObject(parsed.error); else if (semanticValidator !== undefined) throw new ContractError('internal_error', `unknown generated envelope semantic validator ${semanticValidator}`); return parsed; } catch (error) { if (error instanceof ContractError && error.code !== ERROR_SPECS.invalid_request.code) throw new ContractError('invalid_request', error.message); throw error; } }",
        "function runtimeSupportsApiVersion(version) { return API_VERSIONS.some((entry) => entry.version === version && entry.runtime === 'supported'); } function bundleSupportsApiVersion(version) { return API_VERSIONS.some((entry) => entry.version === version && entry.bundles === 'supported'); }",
        "export { API_VERSION, SCHEMA_ID, CANONICAL_ENCODING, SCHEMA_SHA256, MAX_PORTABLE_JSON_INTEGER, " + ", ".join(constant_name(entry["name"]) for entry in schema["bounds"] if entry["name"] != "max_portable_json_integer") + ", API_VERSIONS, LEGACY_ADAPTERS, CAPABILITY_SPECS, METHOD_SPECS, ERROR_SPECS, ContractError, absent, nullPresence, present, " + ", ".join(f"parse{model['name']}" for model in schema["models"]) + ", parseJsonRpcEnvelope, runtimeSupportsApiVersion, bundleSupportsApiVersion, validateOffer, validateSelection, hostOffer, selectRequired, negotiate, methodIsAvailable, requireMethod, validateDisposition, validateInitializeRequest, validateInitializeResponse, validateToolCallParams, validateToolCallResult, validateCancelRequestParams, validateShutdownParams, validateShutdownResult, validateErrorObject, canonicalJson, canonicalFrame, errorObject };",
        "",
    ])
    # `_validateRecord` needs source-selected bound lookup, not global object lookup.
    output = "\n".join(lines).rstrip("\n") + "\n"
    output = output.replace("item.length > globalThis[field.max_items.toUpperCase()]", "item.length > _bound(field.max_items)")
    output = output.replace("globalThis[field.max_bytes.toUpperCase()]", "_bound(field.max_bytes)")
    return output


def render_docs(schema: dict[str, Any], source_hash: str) -> str:
    lines = [
        "<!-- @generated by scripts/generate-extension-api-v03.py; DO NOT EDIT. -->",
        f"<!-- Source: protocol/extension-api-v0.3.schema.json (sha256: {source_hash}) -->",
        "# Ygg Extension API 0.3 Reference",
        "",
        "API `0.3` is a schema-generated canonical JSON-RPC contract. The generated bindings validate all foundation shapes before runtime branching; product adapters only convert generated values to host types.",
        "",
        "## Version policy",
        "",
        "| API | Status | Wire | Runtime | Installable bundles |",
        "| --- | --- | --- | --- | --- |",
    ]
    for entry in schema["version_policy"]:
        lines.append(f"| `{entry['version']}` | {entry['status']} | `{entry['wire']}` | {entry['runtime']} | {entry['bundles']} |")
    lines.extend([
        "",
        "API `0.1` remains frozen at its legacy wire. API `0.2` remains runtime and bundle supported; API `0.3` is current. Selection is exact and never silently upgrades a legacy manifest.",
        "",
        "## Canonical framing and JSON-RPC envelopes",
        "",
        "Frames are UTF-8 canonical JSON followed by exactly one LF. `max_frame_bytes` excludes that delimiter. After initialization the selected bound replaces the offered bound atomically for both stdin writes and stdout reads. A frame exactly at the bound is accepted; one byte over terminates the protocol stream.",
        "",
        "JSON-RPC envelopes have no unknown fields. Requests require `id`, `method`, and `params`; notifications require `method` and `params` but forbid `id`; responses require a valid non-null ID and exactly one of `result` or `error`. Error-response code/message pairs must exactly match the generated API `0.3` error table. IDs are bounded strings or non-negative portable integers. Duplicate keys, noncanonical whitespace/escapes, malformed UTF-16 surrogate escapes, nonportable numbers, and depth violations are rejected before dispatch.",
        "",
        "## Bounds",
        "",
        "| Name | Maximum | Negotiated | Meaning |",
        "| --- | ---: | :---: | --- |",
    ])
    for bound in schema["bounds"]:
        lines.append(f"| `{bound['name']}` | {bound['value']} | {'yes' if bound['negotiated'] else 'no'} | {bound['description']} |")
    lines.extend(["", "## Capabilities", "", "| Capability | Default offer | Status | Meaning |", "| --- | --- | --- | --- |"])
    for entry in schema["capabilities"]:
        lines.append(f"| `{entry['name']}` | {entry['host_offer']} | {entry['status']} | {entry['description']} |")
    lines.extend(["", "## Methods and terminal semantics", "", "| Method | Direction | Params | Result | Terminal | Notification | Status |", "| --- | --- | --- | --- | --- | :---: | --- |"])
    for entry in schema["methods"]:
        lines.append(f"| `{entry['name']}` | `{entry['direction']}` | `{entry['params'] or '—'}` | `{entry['result'] or '—'}` | `{entry['terminal']}` | {'yes' if entry['notification'] else 'no'} | {entry['status']} |")
    lines.extend(["", "Deferred capabilities and methods are represented in this schema but remain unavailable; only the listed foundation methods and capabilities may be negotiated.", "", "## Errors", "", "| Name | Code | Message | Meaning |", "| --- | ---: | --- | --- |"])
    for entry in schema["errors"]:
        lines.append(f"| `{entry['name']}` | `{entry['code']}` | {entry['message']} | {entry['description']} |")
    lines.extend(["", "## Dispositions", "", "| Kind | Reason | Meaning |", "| --- | --- | --- |"])
    for entry in schema["dispositions"]:
        lines.append(f"| `{entry['name']}` | {'required' if entry['requires_reason'] else 'optional'} | {entry['description']} |")
    lines.extend(["", "## Generated wire models", ""])
    for model in schema["models"]:
        lines.extend([f"### `{model['name']}`", "", model["description"], ""])
        if model.get("kind") == "tagged_union":
            lines.extend(["| Variant | Wire tag | Status | Fields |", "| --- | --- | --- | --- |"])
            for variant in model["variants"]:
                fields = ", ".join(f"`{field['name']}: {field['type']}`" for field in variant["fields"]) or "—"
                lines.append(f"| `{variant['name']}` | `{variant['wire']}` | {variant.get('status', 'foundation')} | {fields} |")
        else:
            lines.extend(["| Field | Type | Presence | Nullable |", "| --- | --- | --- | :---: |"])
            for field in model["fields"]:
                lines.append(f"| `{field['name']}` | `{field['type']}` | {field_presence(field)} | {'yes' if field.get('nullable', False) else 'no'} |")
        lines.append("")
    lines.extend([
        "## Generated artifacts and conformance",
        "",
        "The schema generates Rust, Python, TypeScript ESM/runtime declarations, canonical golden fixtures, independent hostile negative fixtures, and this reference. Optional nullable fields preserve absent versus explicit `null`; optional non-null fields reject explicit `null` in all three SDKs.",
        "",
        "```console",
        "python3 scripts/generate-extension-api-v03.py --check",
        "```",
        "",
        "The TypeScript package exports ESM runtime at `@ygg/extension-api-v03` and declarations without registry dependencies. Do not hand-edit generated artifacts.",
        "",
    ])
    return "\n".join(lines)


def generated_files(schema: dict[str, Any], source_hash: str) -> dict[Path, bytes]:
    bounds = {entry["name"]: entry["value"] for entry in schema["bounds"]}
    kwargs = {"max_depth": bounds["max_json_depth"], "max_integer": bounds["max_portable_json_integer"]}
    types = render_typescript_types(schema, source_hash).encode("utf-8")
    files: dict[Path, bytes] = {
        ROOT / "crates/ygg-agent/src/extension_api_v03.rs": render_rust(schema, source_hash).encode("utf-8"),
        ROOT / "sdk/python/ygg_extension/api_v03.py": render_python(schema, source_hash).encode("utf-8"),
        ROOT / "sdk/typescript/src/api_v03.mjs": render_typescript_runtime(schema, source_hash).encode("utf-8"),
        ROOT / "sdk/typescript/src/api_v03.ts": types,
        ROOT / "sdk/typescript/src/api_v03.d.ts": types,
        ROOT / "docs/extensions/API-0.3-REFERENCE.md": render_docs(schema, source_hash).encode("utf-8"),
    }
    manifest: dict[str, Any] = {"api_version": schema["api_version"], "canonical_encoding": schema["canonical_encoding"], "schema_sha256": source_hash, "fixtures": []}
    for fixture in schema["fixtures"]:
        body = canonical_json(fixture["value"], **kwargs).encode("utf-8")
        files[FIXTURE_DIRECTORY / f"{fixture['name']}.json"] = body
        manifest["fixtures"].append({"name": fixture["name"], "sha256": hashlib.sha256(body).hexdigest()})
    files[FIXTURE_DIRECTORY / "manifest.json"] = canonical_json(manifest, **kwargs).encode("utf-8")
    negative_manifest: dict[str, Any] = {"api_version": schema["api_version"], "schema_sha256": source_hash, "fixtures": []}
    for fixture in schema["negative_fixtures"]:
        body = (canonical_json(fixture["value"], **kwargs) if "value" in fixture else fixture["raw"]).encode("utf-8")
        files[NEGATIVE_FIXTURE_DIRECTORY / f"{fixture['name']}.json"] = body
        negative_manifest["fixtures"].append({"name": fixture["name"], "sha256": hashlib.sha256(body).hexdigest()})
    files[NEGATIVE_FIXTURE_DIRECTORY / "manifest.json"] = canonical_json(negative_manifest, **kwargs).encode("utf-8")
    return files


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="fail if generated files are stale")
    args = parser.parse_args()
    source = SOURCE.read_bytes()
    try:
        schema = json.loads(source)
        validate_schema(schema)
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"invalid API 0.3 schema: {error}", file=sys.stderr)
        return 2
    source_hash = hashlib.sha256(source).hexdigest()
    files = generated_files(schema, source_hash)
    stale: list[Path] = []
    for path, expected in files.items():
        actual = path.read_bytes() if path.exists() else None
        if actual != expected:
            stale.append(path)
            if not args.check:
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(expected)
    # The fixture directory is generated-only. Keeping an old fixture would make
    # cross-language conformance silently test a superseded contract, so drift
    # includes unexpected JSON files as well as changed expected files.
    expected_fixture_paths = {path.resolve() for path in files if FIXTURE_DIRECTORY in path.parents}
    if FIXTURE_DIRECTORY.exists():
        for path in FIXTURE_DIRECTORY.rglob("*.json"):
            if path.resolve() not in expected_fixture_paths:
                stale.append(path)
                if not args.check:
                    path.unlink()
    if stale and args.check:
        print("API 0.3 generated artifacts are stale:", file=sys.stderr)
        for path in stale:
            print(f"  {path.relative_to(ROOT)}", file=sys.stderr)
        return 1
    if stale:
        for path in stale:
            print(f"generated {path.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
