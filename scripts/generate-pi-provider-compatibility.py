#!/usr/bin/env python3
"""Render the Pi provider compatibility ledger from its checked-in fixtures."""

from __future__ import annotations

import argparse
import json
import pathlib
import sys
from typing import Any
from urllib.parse import urlsplit


ROOT = pathlib.Path(__file__).resolve().parents[1]
SOURCE = ROOT / "crates/ygg-coding-agent/fixtures/providers/pi-0.84.4.json"
DESTINATION = ROOT / "docs/pi-provider-compatibility.md"


def fail(message: str) -> None:
    raise ValueError(message)


def require_object(value: Any, context: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        fail(f"{context} must be an object")
    return value


def require_string(value: Any, context: str, *, allow_empty: bool = False) -> str:
    if not isinstance(value, str) or (not allow_empty and not value.strip()):
        fail(f"{context} must be a non-empty string")
    return value


def require_keys(value: dict[str, Any], expected: set[str], context: str) -> None:
    actual = set(value)
    if actual != expected:
        fail(
            f"{context} keys differ: expected {sorted(expected)}, got {sorted(actual)}"
        )


def require_keys_with_optional(
    value: dict[str, Any], required: set[str], optional: set[str], context: str
) -> None:
    require_keys(value, required | (set(value) & optional), context)


def markdown_cell(value: str) -> str:
    return value.replace("|", "\\|").replace("\n", " ")


def validate_fixture(value: Any, context: str) -> dict[str, str]:
    fixture = require_object(value, context)
    require_keys(
        fixture,
        {
            "registration",
            "model_id",
            "protocol",
            "endpoint_id",
            "auth_presentation",
            "base_url",
            "environment_variable",
        },
        context,
    )
    result = {
        key: require_string(
            raw,
            f"{context}.{key}",
            allow_empty=key == "environment_variable" and fixture["registration"] == "subscription",
        )
        for key, raw in fixture.items()
    }
    if result["registration"] not in {"discovered", "static", "subscription"}:
        fail(f"{context}.registration is unknown")
    if result["protocol"] not in {
        "anthropic_messages",
        "openai_chat",
        "openai_responses",
    }:
        fail(f"{context}.protocol is unknown")
    if result["auth_presentation"] not in {"api_key_header", "bearer", "dynamic"}:
        fail(f"{context}.auth_presentation is unknown")
    parsed = urlsplit(result["base_url"])
    if (
        parsed.scheme != "https"
        or not parsed.netloc
        or parsed.hostname is None
        or parsed.username is not None
        or parsed.password is not None
        or parsed.query
        or parsed.fragment
        or not parsed.path.endswith("/")
    ):
        fail(f"{context}.base_url must be a credential-free HTTPS base URL ending in /")
    if result["registration"] == "subscription":
        if result["auth_presentation"] != "dynamic" or result["environment_variable"]:
            fail(f"{context} subscription auth must be dynamic and credential-free")
    elif result["auth_presentation"] == "dynamic" or not result["environment_variable"]:
        fail(f"{context} environment auth needs a credential presentation and environment variable")
    return result


def validate_document(source: pathlib.Path) -> dict[str, Any]:
    raw = json.loads(source.read_text(encoding="utf-8"))
    root = require_object(raw, "inventory")
    require_keys(
        root,
        {
            "schema_version",
            "pi_package",
            "inventory_basis",
            "expected_provider_ids",
            "providers",
        },
        "inventory",
    )
    if root["schema_version"] != 1:
        fail("inventory.schema_version must be 1")
    require_string(root["pi_package"], "inventory.pi_package")
    require_string(root["inventory_basis"], "inventory.inventory_basis")
    expected_ids = root["expected_provider_ids"]
    if not isinstance(expected_ids, list) or not expected_ids:
        fail("inventory.expected_provider_ids must be a non-empty array")
    expected_ids = [
        require_string(value, "inventory.expected_provider_ids[]") for value in expected_ids
    ]
    if len(expected_ids) != len(set(expected_ids)):
        fail("inventory.expected_provider_ids contains duplicates")

    providers = root["providers"]
    if not isinstance(providers, list) or not providers:
        fail("inventory.providers must be a non-empty array")
    actual_ids: list[str] = []
    fixture_ids: set[str] = set()
    for index, raw_provider in enumerate(providers):
        context = f"inventory.providers[{index}]"
        provider = require_object(raw_provider, context)
        require_keys(provider, {"id", "label", "evidence", "decision"}, context)
        provider_id = require_string(provider["id"], f"{context}.id")
        actual_ids.append(provider_id)
        require_string(provider["label"], f"{context}.label")
        require_string(provider["evidence"], f"{context}.evidence")
        decision = require_object(provider["decision"], f"{context}.decision")
        kind = require_string(decision.get("kind"), f"{context}.decision.kind")
        fixture_id = require_string(
            decision.get("fixture_id"), f"{context}.decision.fixture_id"
        )
        if fixture_id in fixture_ids:
            fail(f"duplicate fixture id: {fixture_id}")
        fixture_ids.add(fixture_id)

        common = {"kind", "fixture_id"}
        if kind == "declared":
            require_keys_with_optional(
                decision,
                common | {"provider_id", "fixture"},
                {"note"},
                f"{context}.decision",
            )
            require_string(decision["provider_id"], f"{context}.decision.provider_id")
            validate_fixture(decision["fixture"], f"{context}.decision.fixture")
        elif kind == "declared_subset":
            require_keys_with_optional(
                decision,
                common
                | {
                    "provider_id",
                    "fixture",
                    "excluded_surfaces",
                    "missing_primitive",
                    "release_blocker",
                },
                {"note"},
                f"{context}.decision",
            )
            require_string(decision["provider_id"], f"{context}.decision.provider_id")
            validate_fixture(decision["fixture"], f"{context}.decision.fixture")
            excluded = decision["excluded_surfaces"]
            if not isinstance(excluded, list) or not excluded:
                fail(f"{context}.decision.excluded_surfaces must be a non-empty array")
            for model in excluded:
                require_string(model, f"{context}.decision.excluded_surfaces[]")
            require_string(decision["missing_primitive"], f"{context}.decision.missing_primitive")
            require_string(decision["release_blocker"], f"{context}.decision.release_blocker")
        elif kind == "unsupported":
            require_keys_with_optional(
                decision,
                common | {"missing_primitive", "release_blocker"},
                {"legacy_declaration", "note"},
                f"{context}.decision",
            )
            require_string(decision["missing_primitive"], f"{context}.decision.missing_primitive")
            require_string(decision["release_blocker"], f"{context}.decision.release_blocker")
            if "legacy_declaration" in decision:
                require_string(
                    decision["legacy_declaration"],
                    f"{context}.decision.legacy_declaration",
                )
        else:
            fail(f"{context}.decision.kind is unknown")
        if "note" in decision:
            require_string(decision["note"], f"{context}.decision.note")

    if len(actual_ids) != len(set(actual_ids)):
        fail("inventory.providers contains duplicate ids")
    if actual_ids != expected_ids:
        fail(
            "inventory.providers ids must exactly match inventory.expected_provider_ids"
        )
    return root


def render(inventory: dict[str, Any]) -> str:
    lines = [
        "<!-- Generated by scripts/generate-pi-provider-compatibility.py from crates/ygg-coding-agent/fixtures/providers/pi-0.84.4.json; do not edit. -->",
        "",
        "# Pi 0.84.4 provider compatibility ledger",
        "",
        f"Target: `{inventory['pi_package']}`.",
        "",
        inventory["inventory_basis"],
        "",
        "A **declared** row has a deterministic catalog/route fixture. A **declared subset** row names every excluded Pi surface and its release blocker. An **unsupported** row has no native declaration and names the primitive that must land before it can be exposed. Fixture IDs are exercised by `providers::contract::tests::pinned_pi_provider_inventory_has_tested_decisions` and `providers::contract::tests::pinned_pi_provider_fixtures_send_declared_routes_without_network_access`; this is not a load-only inventory.",
        "",
        "| Pi provider | Decision | Tested fixture | Route or missing primitive | Evidence |",
        "| --- | --- | --- | --- | --- |",
    ]
    for provider in inventory["providers"]:
        decision = provider["decision"]
        kind = decision["kind"]
        if kind == "unsupported":
            decision_text = "unsupported"
            compatibility = (
                f"Missing: {decision['missing_primitive']} "
                f"(release blocker {decision['release_blocker']})"
            )
            if "legacy_declaration" in decision:
                compatibility += f"; legacy declaration: `{decision['legacy_declaration']}`"
        else:
            decision_text = f"declared as `{decision['provider_id']}`"
            if kind == "declared_subset":
                decision_text = f"declared subset as `{decision['provider_id']}`"
            wire = decision["fixture"]
            compatibility = (
                f"`{wire['protocol']}` via `{wire['endpoint_id']}` at "
                f"`{wire['base_url']}` ({wire['auth_presentation']})"
            )
            if kind == "declared_subset":
                excluded = ", ".join(f"`{value}`" for value in decision["excluded_surfaces"])
                compatibility += (
                    f"; excludes {excluded}: {decision['missing_primitive']} "
                    f"(release blocker {decision['release_blocker']})"
                )
        if "note" in decision:
            compatibility += f"; {decision['note']}"
        lines.append(
            "| "
            + " | ".join(
                markdown_cell(value)
                for value in (
                    f"`{provider['id']}` — {provider['label']}",
                    decision_text,
                    f"`{decision['fixture_id']}`",
                    compatibility,
                    provider["evidence"],
                )
            )
            + " |"
        )
    lines.extend(
        [
            "",
            "## Closure guard",
            "",
            "Do not convert an unsupported or subset row to `declared` from a successful load alone. The change needs a deterministic request/stream/auth fixture for the named primitive and an update to this source fixture; the generator and test then keep the documentation synchronized.",
            "",
        ]
    )
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check",
        action="store_true",
        help="fail instead of writing when the checked-in ledger is stale",
    )
    args = parser.parse_args()
    try:
        rendered = render(validate_document(SOURCE))
    except (OSError, json.JSONDecodeError, ValueError) as error:
        print(f"provider compatibility generation failed: {error}", file=sys.stderr)
        return 1
    if args.check:
        try:
            current = DESTINATION.read_text(encoding="utf-8")
        except OSError as error:
            print(f"provider compatibility check failed: {error}", file=sys.stderr)
            return 1
        if current != rendered:
            print(
                "provider compatibility ledger is stale; run "
                "python3 scripts/generate-pi-provider-compatibility.py",
                file=sys.stderr,
            )
            return 1
        return 0
    DESTINATION.write_text(rendered, encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
