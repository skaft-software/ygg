#!/usr/bin/env python3
"""Verify the pinned Pi 0.84.4 compatibility and TUI evidence ledgers."""

from __future__ import annotations

import argparse
import base64
from collections import Counter
from copy import deepcopy
import hashlib
import json
from pathlib import Path, PurePosixPath
import re
import shutil
import subprocess
import sys
from typing import Any


REPOSITORY_ROOT = Path(__file__).resolve().parents[1]
COMPAT_PROFILE_PATH = Path("extensions/ygg-pi-compat/profiles/0.84.4.json")
COMPAT_MATRIX_PATH = Path("extensions/ygg-pi-compat/COMPATIBILITY.md")
TUI_PROFILE_PATH = Path("crates/sexy-tui-rs/upstream/pi-tui-0.84.4.json")
TUI_CRATE_PATH = Path("crates/sexy-tui-rs")

PI_REPOSITORY = "https://github.com/earendil-works/pi.git"
PI_REVISION = "b79e4cc834970cca69daebffab7df1da7d1e52c4"
PI_TAG = "v0.84.4"
PI_VERSION = "0.84.4"
PI_NODE_MINIMUM = "22.19.0"
PI_NODE_ENGINE = ">=22.19.0"

CODING_AGENT_PACKAGE = {
    "name": "@earendil-works/pi-coding-agent",
    "version": PI_VERSION,
    "npm_integrity": (
        "sha512-jmOlrqUmvhh/siNWFRXjYLJzhKFIHNsAQaysRwzQPQFnPAaV/"
        "vhqHsLH/MBsIISA1Rjj7WTUFR3nJrpXoLx39w=="
    ),
}
TUI_PACKAGE = {
    "name": "@earendil-works/pi-tui",
    "version": PI_VERSION,
    "npm_integrity": (
        "sha512-nPUnwDkLtupPXnZQYrCwPFcuTydCDqTY6ZbFqhsL4S4kVq0AT418kPa/"
        "6uXwtaCD+MjBNBltb7ScTYX65yeE1w=="
    ),
}

# These fingerprints make a same-size substitution fail even when no upstream
# checkout was supplied. They hash each ordered, newline-terminated inventory.
SURFACE_PINS = {
    "events": (36, "a2ce8286e67dd1191696eaca3bf2cbfca219f3efdacdaca51de71cee0c8f9ddf"),
    "extension_api": (
        27,
        "c53fe0432607641a58bfc26fb11e1fae3a0e1109090a91d7742e1e3bdfa2fd82",
    ),
    "ui_context": (
        28,
        "50848a6be105c4d3857db48cb0ae2ec5a92f2042a69d7e9e3d9dc6d37e113eb8",
    ),
    "context": (27, "10dc39c8022aeeef252fd5247ecc071dbb57eeca57b6533ddf3fe3faa89831d7"),
}
EXAMPLE_COUNT = 78
EXAMPLE_INVENTORY_SHA256 = "dcbab05ab74b517116b9d4acf5c03bb774888072d794d59afb401101e3d3d5a1"
TUI_TEST_COUNT = 33
TUI_TEST_INVENTORY_SHA256 = "e01be22b1107bdd87c24a861715ae830f3a4c40b8e63d5d0da2cb45a21287992"

MATRIX_HEADINGS = {
    "## Extension events": "events",
    "## `ExtensionAPI`": "extension_api",
    "## `ExtensionUIContext`": "ui_context",
    "## Context surfaces": "context",
}
MATRIX_STATUSES = {
    "passing",
    "safe divergence",
    "approved safe divergence",
    "not implemented",
}
MATRIX_CLOSED_STATUSES = {"passing", "approved safe divergence"}
TUI_STATUSES = {
    "requires_0.84.4_audit",
    "requires_0.84.4_port",
    "passing",
    "approved_divergence",
}
TUI_CLOSED_STATUSES = {"passing", "approved_divergence"}
RELEASE_STATUSES = {"in_progress", "complete"}
GATE_STATUSES = {"open", "passing"}
GATE_COUNTS = {
    "all_official_examples": EXAMPLE_COUNT,
    "tui_test_files": TUI_TEST_COUNT,
    "silent_unsupported_calls": 0,
}
GATE_NAMES = (
    "plan_mode",
    "all_official_examples",
    "tui_test_files",
    "silent_unsupported_calls",
    "aggregate_process",
    "provider_and_oauth",
)


class Checker:
    def __init__(self) -> None:
        self.errors: list[str] = []

    def error(self, message: str) -> None:
        self.errors.append(message)

    def equal(self, actual: Any, expected: Any, label: str) -> None:
        if actual != expected:
            self.error(f"{label}: expected {expected!r}, got {actual!r}")


class DuplicateJsonKey(ValueError):
    pass


def reject_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise DuplicateJsonKey(f"duplicate JSON key {key!r}")
        result[key] = value
    return result


def reject_nonfinite_json(value: str) -> Any:
    raise ValueError(f"non-finite JSON number {value!r}")


def parse_json(text: str, label: str, checker: Checker) -> dict[str, Any] | None:
    try:
        value = json.loads(
            text,
            object_pairs_hook=reject_duplicate_keys,
            parse_constant=reject_nonfinite_json,
        )
    except (json.JSONDecodeError, ValueError) as error:
        checker.error(f"{label}: cannot parse strict JSON: {error}")
        return None
    if type(value) is not dict:
        checker.error(f"{label}: root must be an object")
        return None
    return value


def load_json(path: Path, label: str, checker: Checker) -> dict[str, Any] | None:
    try:
        text = path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        checker.error(f"{label}: cannot read strict JSON: {error}")
        return None
    return parse_json(text, label, checker)


def exact_object(
    value: Any, label: str, keys: set[str] | tuple[str, ...], checker: Checker
) -> dict[str, Any]:
    if type(value) is not dict:
        checker.error(f"{label}: expected an object")
        return {}
    expected = set(keys)
    actual = set(value)
    missing = sorted(expected - actual)
    unexpected = sorted(actual - expected)
    if missing:
        checker.error(f"{label}: missing keys {missing!r}")
    if unexpected:
        checker.error(f"{label}: unexpected keys {unexpected!r}")
    return value


def string_value(value: Any, label: str, checker: Checker) -> str | None:
    if type(value) is not str or not value or value != value.strip():
        checker.error(f"{label}: expected a non-empty string without surrounding whitespace")
        return None
    return value


def integer_value(value: Any, label: str, checker: Checker) -> int | None:
    if type(value) is not int:
        checker.error(f"{label}: expected an integer")
        return None
    return value


def list_value(value: Any, label: str, checker: Checker) -> list[Any]:
    if type(value) is not list:
        checker.error(f"{label}: expected an array")
        return []
    return value


def inventory_digest(values: list[str]) -> str:
    return hashlib.sha256(("\n".join(values) + "\n").encode("utf-8")).hexdigest()


def validate_inventory(
    value: Any,
    label: str,
    expected_count: int,
    expected_digest: str,
    checker: Checker,
    *,
    sorted_required: bool = False,
) -> list[str]:
    raw_values = list_value(value, label, checker)
    values: list[str] = []
    all_strings = True
    for index, raw in enumerate(raw_values):
        item = string_value(raw, f"{label}[{index}]", checker)
        if item is None:
            all_strings = False
        else:
            values.append(item)
    if len(raw_values) != expected_count:
        checker.error(f"{label}: expected {expected_count} entries, got {len(raw_values)}")
    if all_strings:
        duplicates = sorted(name for name, count in Counter(values).items() if count > 1)
        if duplicates:
            checker.error(f"{label}: duplicate entries {duplicates!r}")
        if sorted_required and values != sorted(values):
            checker.error(f"{label}: entries must be sorted byte-for-byte")
        actual_digest = inventory_digest(values)
        if actual_digest != expected_digest:
            checker.error(
                f"{label}: pinned inventory fingerprint mismatch "
                f"(expected {expected_digest}, got {actual_digest})"
            )
    return values


def validate_integrity(value: Any, expected: str, label: str, checker: Checker) -> None:
    integrity = string_value(value, label, checker)
    if integrity is None:
        return
    checker.equal(integrity, expected, label)
    if not integrity.startswith("sha512-"):
        checker.error(f"{label}: expected a sha512 SRI value")
        return
    try:
        decoded = base64.b64decode(integrity.removeprefix("sha512-"), validate=True)
    except ValueError as error:
        checker.error(f"{label}: invalid base64: {error}")
        return
    if len(decoded) != 64:
        checker.error(f"{label}: sha512 SRI payload must be 64 bytes")


def validate_package(
    value: Any, expected: dict[str, str], label: str, checker: Checker
) -> dict[str, Any]:
    package = exact_object(value, label, {"name", "version", "npm_integrity"}, checker)
    name = string_value(package.get("name"), f"{label}.name", checker)
    version = string_value(package.get("version"), f"{label}.version", checker)
    if name is not None:
        checker.equal(name, expected["name"], f"{label}.name")
    if version is not None:
        checker.equal(version, expected["version"], f"{label}.version")
    validate_integrity(
        package.get("npm_integrity"),
        expected["npm_integrity"],
        f"{label}.npm_integrity",
        checker,
    )
    return package


def validate_relative_file(
    value: Any, root: Path, label: str, checker: Checker
) -> str | None:
    path_text = string_value(value, label, checker)
    if path_text is None:
        return None
    pure = PurePosixPath(path_text)
    if (
        pure.is_absolute()
        or "\\" in path_text
        or any(part in {"", ".", ".."} for part in pure.parts)
        or str(pure) != path_text
    ):
        checker.error(f"{label}: expected a normalized repository-relative POSIX path")
        return path_text
    candidate = root.joinpath(*pure.parts)
    try:
        root_resolved = root.resolve(strict=True)
        resolved = candidate.resolve(strict=True)
        resolved.relative_to(root_resolved)
    except (OSError, ValueError):
        checker.error(f"{label}: path does not resolve to a file below {root}")
        return path_text
    if candidate.is_symlink() or not candidate.is_file():
        checker.error(f"{label}: path must name a regular, non-symlink file below {root}")
    return path_text


def validate_evidence_paths(
    value: Any, root: Path, label: str, checker: Checker
) -> list[str]:
    raw_paths = list_value(value, label, checker)
    paths: list[str] = []
    for index, raw in enumerate(raw_paths):
        path = validate_relative_file(raw, root, f"{label}[{index}]", checker)
        if path is not None:
            paths.append(path)
    duplicates = sorted(path for path, count in Counter(paths).items() if count > 1)
    if duplicates:
        checker.error(f"{label}: duplicate paths {duplicates!r}")
    return paths


def validate_source_pin(
    value: Any, label: str, checker: Checker, *, tui: bool
) -> dict[str, Any]:
    keys = {"repository", "revision", "tag"}
    if tui:
        keys.add("root")
    source = exact_object(value, label, keys, checker)
    repository = string_value(source.get("repository"), f"{label}.repository", checker)
    revision = string_value(source.get("revision"), f"{label}.revision", checker)
    tag = string_value(source.get("tag"), f"{label}.tag", checker)
    if repository is not None:
        checker.equal(repository, PI_REPOSITORY, f"{label}.repository")
    if revision is not None:
        checker.equal(revision, PI_REVISION, f"{label}.revision")
    if tag is not None:
        checker.equal(tag, PI_TAG, f"{label}.tag")
    if tui:
        root = string_value(source.get("root"), f"{label}.root", checker)
        if root is not None:
            checker.equal(root, "packages/tui", f"{label}.root")
    return source


def validate_release_gates(
    value: Any, repository_root: Path, checker: Checker
) -> tuple[list[str], dict[str, int]]:
    gates = exact_object(value, "compat.release_gates", set(GATE_NAMES), checker)
    open_gates: list[str] = []
    observed_counts: dict[str, int] = {}
    for name in GATE_NAMES:
        label = f"compat.release_gates.{name}"
        expected_keys = {"required", "status", "evidence"}
        if name in GATE_COUNTS:
            expected_keys.add("expected_count")
        gate = exact_object(gates.get(name), label, expected_keys, checker)
        required = gate.get("required")
        if type(required) is not bool:
            checker.error(f"{label}.required: expected a boolean")
        elif not required:
            checker.error(f"{label}.required: every Pi 0.84.4 completion gate is required")
        status = string_value(gate.get("status"), f"{label}.status", checker)
        if status is not None and status not in GATE_STATUSES:
            checker.error(f"{label}.status: invalid status {status!r}")
        if required is True and status != "passing":
            open_gates.append(name)
        evidence = validate_evidence_paths(
            gate.get("evidence"), repository_root, f"{label}.evidence", checker
        )
        if status == "passing" and not evidence:
            checker.error(f"{label}: passing requires at least one evidence path")
        if name in GATE_COUNTS:
            count = integer_value(
                gate.get("expected_count"), f"{label}.expected_count", checker
            )
            if count is not None:
                checker.equal(count, GATE_COUNTS[name], f"{label}.expected_count")
                observed_counts[name] = count
    return open_gates, observed_counts


def validate_compat_profile(
    profile: dict[str, Any], repository_root: Path, checker: Checker
) -> dict[str, Any]:
    exact_object(
        profile,
        "compat",
        {
            "schema_version",
            "profile",
            "release_status",
            "source",
            "packages",
            "node",
            "public_surface",
            "official_extension_examples",
            "release_gates",
        },
        checker,
    )
    schema_version = integer_value(profile.get("schema_version"), "compat.schema_version", checker)
    if schema_version is not None:
        checker.equal(schema_version, 1, "compat.schema_version")
    profile_name = string_value(profile.get("profile"), "compat.profile", checker)
    if profile_name is not None:
        checker.equal(profile_name, "pi-0.84.4", "compat.profile")
    release_status = string_value(
        profile.get("release_status"), "compat.release_status", checker
    )
    if release_status is not None and release_status not in RELEASE_STATUSES:
        checker.error(f"compat.release_status: invalid status {release_status!r}")

    validate_source_pin(profile.get("source"), "compat.source", checker, tui=False)
    packages = exact_object(
        profile.get("packages"), "compat.packages", {"coding_agent", "tui"}, checker
    )
    validate_package(
        packages.get("coding_agent"),
        CODING_AGENT_PACKAGE,
        "compat.packages.coding_agent",
        checker,
    )
    compat_tui_package = validate_package(
        packages.get("tui"), TUI_PACKAGE, "compat.packages.tui", checker
    )

    node = exact_object(profile.get("node"), "compat.node", {"minimum_version"}, checker)
    minimum_version = string_value(
        node.get("minimum_version"), "compat.node.minimum_version", checker
    )
    if minimum_version is not None:
        checker.equal(minimum_version, PI_NODE_MINIMUM, "compat.node.minimum_version")

    public_surface = exact_object(
        profile.get("public_surface"),
        "compat.public_surface",
        set(SURFACE_PINS),
        checker,
    )
    surfaces: dict[str, list[str]] = {}
    for name, (count, digest) in SURFACE_PINS.items():
        surfaces[name] = validate_inventory(
            public_surface.get(name),
            f"compat.public_surface.{name}",
            count,
            digest,
            checker,
        )

    examples = validate_inventory(
        profile.get("official_extension_examples"),
        "compat.official_extension_examples",
        EXAMPLE_COUNT,
        EXAMPLE_INVENTORY_SHA256,
        checker,
        sorted_required=True,
    )
    for index, name in enumerate(examples):
        if (
            name in {".", "..", "README.md"}
            or "/" in name
            or "\\" in name
            or name.startswith(".")
        ):
            checker.error(
                f"compat.official_extension_examples[{index}]: invalid top-level entry {name!r}"
            )

    open_gates, gate_counts = validate_release_gates(
        profile.get("release_gates"), repository_root, checker
    )
    return {
        "release_status": release_status,
        "surfaces": surfaces,
        "examples": examples,
        "open_gates": open_gates,
        "gate_counts": gate_counts,
        "tui_package": compat_tui_package,
    }


def validate_tui_profile(
    profile: dict[str, Any], repository_root: Path, checker: Checker
) -> dict[str, Any]:
    exact_object(
        profile,
        "tui",
        {"schema_version", "profile", "release_status", "source", "package", "test_files"},
        checker,
    )
    schema_version = integer_value(profile.get("schema_version"), "tui.schema_version", checker)
    if schema_version is not None:
        checker.equal(schema_version, 1, "tui.schema_version")
    profile_name = string_value(profile.get("profile"), "tui.profile", checker)
    if profile_name is not None:
        checker.equal(profile_name, "pi-tui-0.84.4", "tui.profile")
    release_status = string_value(profile.get("release_status"), "tui.release_status", checker)
    if release_status is not None and release_status not in RELEASE_STATUSES:
        checker.error(f"tui.release_status: invalid status {release_status!r}")

    validate_source_pin(profile.get("source"), "tui.source", checker, tui=True)
    package = validate_package(profile.get("package"), TUI_PACKAGE, "tui.package", checker)

    raw_rows = list_value(profile.get("test_files"), "tui.test_files", checker)
    if len(raw_rows) != TUI_TEST_COUNT:
        checker.error(f"tui.test_files: expected {TUI_TEST_COUNT} rows, got {len(raw_rows)}")
    upstream_names: list[str] = []
    open_rows: list[str] = []
    for index, raw_row in enumerate(raw_rows):
        label = f"tui.test_files[{index}]"
        row = exact_object(
            raw_row,
            label,
            {"upstream", "area", "required", "status", "rust_equivalents"},
            checker,
        )
        upstream = string_value(row.get("upstream"), f"{label}.upstream", checker)
        if upstream is not None:
            upstream_names.append(upstream)
            if not re.fullmatch(r"test/[A-Za-z0-9][A-Za-z0-9._-]*\.test\.ts", upstream):
                checker.error(f"{label}.upstream: invalid pinned TUI test path {upstream!r}")
        area = string_value(row.get("area"), f"{label}.area", checker)
        if area is not None and not re.fullmatch(r"[a-z0-9]+(?:-[a-z0-9]+)*", area):
            checker.error(f"{label}.area: invalid area {area!r}")
        required = row.get("required")
        if type(required) is not bool:
            checker.error(f"{label}.required: expected a boolean")
        elif not required:
            checker.error(f"{label}.required: every pinned upstream test is required")
        status = string_value(row.get("status"), f"{label}.status", checker)
        if status is not None and status not in TUI_STATUSES:
            checker.error(f"{label}.status: invalid status {status!r}")
        if required is True and status not in TUI_CLOSED_STATUSES:
            open_rows.append(upstream or f"row {index}")
        equivalents = validate_evidence_paths(
            row.get("rust_equivalents"),
            repository_root / TUI_CRATE_PATH,
            f"{label}.rust_equivalents",
            checker,
        )
        if status in TUI_CLOSED_STATUSES and not equivalents:
            checker.error(f"{label}: {status} requires at least one Rust equivalent path")

    duplicates = sorted(
        name for name, count in Counter(upstream_names).items() if count > 1
    )
    if duplicates:
        checker.error(f"tui.test_files: duplicate upstream paths {duplicates!r}")
    if upstream_names != sorted(upstream_names):
        checker.error("tui.test_files: upstream paths must be sorted byte-for-byte")
    if len(upstream_names) == len(raw_rows):
        digest = inventory_digest(upstream_names)
        if digest != TUI_TEST_INVENTORY_SHA256:
            checker.error(
                "tui.test_files: pinned inventory fingerprint mismatch "
                f"(expected {TUI_TEST_INVENTORY_SHA256}, got {digest})"
            )

    return {
        "release_status": release_status,
        "package": package,
        "upstream_names": upstream_names,
        "open_rows": open_rows,
    }


def split_markdown_row(line: str) -> list[str] | None:
    stripped = line.strip()
    if not stripped.startswith("|") or not stripped.endswith("|"):
        return None
    return [cell.strip() for cell in stripped[1:-1].split("|")]


def strip_inline_code(value: str) -> str:
    if len(value) >= 2 and value.startswith("`") and value.endswith("`"):
        return value[1:-1]
    return value


def parse_compatibility_matrix(text: str, checker: Checker) -> dict[str, list[dict[str, str]]]:
    lines = text.splitlines()
    result: dict[str, list[dict[str, str]]] = {}
    for heading, surface_name in MATRIX_HEADINGS.items():
        positions = [index for index, line in enumerate(lines) if line.strip() == heading]
        if len(positions) != 1:
            checker.error(
                f"compatibility matrix: expected one {heading!r} heading, found {len(positions)}"
            )
            result[surface_name] = []
            continue
        index = positions[0] + 1
        while index < len(lines) and not lines[index].strip():
            index += 1
        if index + 1 >= len(lines):
            checker.error(f"compatibility matrix {heading!r}: missing table")
            result[surface_name] = []
            continue
        header = split_markdown_row(lines[index])
        separator = split_markdown_row(lines[index + 1])
        if header is None or len(header) != 3 or header[1:] != [
            "Status",
            "Current behavior / blocker",
        ]:
            checker.error(f"compatibility matrix {heading!r}: invalid three-column header")
        if separator is None or len(separator) != 3 or not all(
            re.fullmatch(r":?-{3,}:?", cell) for cell in separator
        ):
            checker.error(f"compatibility matrix {heading!r}: invalid separator row")
        index += 2
        rows: list[dict[str, str]] = []
        while index < len(lines):
            cells = split_markdown_row(lines[index])
            if cells is None:
                break
            if len(cells) != 3:
                checker.error(
                    f"compatibility matrix {heading!r} line {index + 1}: expected three cells"
                )
                index += 1
                continue
            name = strip_inline_code(cells[0])
            status = cells[1]
            behavior = cells[2]
            if not name:
                checker.error(f"compatibility matrix {heading!r} line {index + 1}: empty name")
            if status not in MATRIX_STATUSES:
                checker.error(
                    f"compatibility matrix {heading!r} row {name!r}: invalid status {status!r}"
                )
            if not behavior:
                checker.error(
                    f"compatibility matrix {heading!r} row {name!r}: empty behavior/blocker"
                )
            rows.append({"name": name, "status": status, "behavior": behavior})
            index += 1
        duplicates = sorted(
            name
            for name, count in Counter(row["name"] for row in rows).items()
            if count > 1
        )
        if duplicates:
            checker.error(f"compatibility matrix {heading!r}: duplicate rows {duplicates!r}")
        result[surface_name] = rows
    return result


def summarize_names(names: list[str], limit: int = 5) -> str:
    if len(names) <= limit:
        return ", ".join(names)
    return f"{', '.join(names[:limit])}, ... ({len(names)} total)"


def validate_ledgers(
    compat_profile: dict[str, Any],
    tui_profile: dict[str, Any],
    compatibility_markdown: str,
    repository_root: Path,
    checker: Checker,
) -> dict[str, Any]:
    compat = validate_compat_profile(compat_profile, repository_root, checker)
    tui = validate_tui_profile(tui_profile, repository_root, checker)
    matrix = parse_compatibility_matrix(compatibility_markdown, checker)

    checker.equal(
        compat.get("tui_package"), tui.get("package"), "shared pinned TUI package metadata"
    )
    gate_counts = compat.get("gate_counts", {})
    checker.equal(
        gate_counts.get("all_official_examples"),
        len(compat.get("examples", [])),
        "official example count pin",
    )
    checker.equal(
        gate_counts.get("tui_test_files"),
        len(tui.get("upstream_names", [])),
        "TUI test count pin",
    )

    matrix_open_rows: list[str] = []
    for surface_name in SURFACE_PINS:
        expected_names = compat.get("surfaces", {}).get(surface_name, [])
        rows = matrix.get(surface_name, [])
        actual_names = [row["name"] for row in rows]
        if actual_names != expected_names:
            missing = sorted(set(expected_names) - set(actual_names))
            unexpected = sorted(set(actual_names) - set(expected_names))
            checker.error(
                f"compatibility matrix {surface_name}: rows do not match the machine inventory; "
                f"missing={missing!r}, unexpected={unexpected!r}"
            )
        matrix_open_rows.extend(
            f"{surface_name}.{row['name']}"
            for row in rows
            if row["status"] not in MATRIX_CLOSED_STATUSES
        )

    tui_open_rows = tui.get("open_rows", [])
    if tui.get("release_status") == "complete" and tui_open_rows:
        checker.error(
            "tui.release_status=complete is forbidden while required TUI rows are open: "
            + summarize_names(tui_open_rows)
        )
    if compat.get("release_status") == "complete":
        open_gates = compat.get("open_gates", [])
        if open_gates:
            checker.error(
                "compat.release_status=complete is forbidden while required release "
                "gates are open: "
                + summarize_names(open_gates)
            )
        if matrix_open_rows:
            checker.error(
                "compat.release_status=complete is forbidden while compatibility rows are "
                "not implemented or have unapproved divergence: "
                + summarize_names(matrix_open_rows)
            )
        if tui_open_rows:
            checker.error(
                "compat.release_status=complete is forbidden while required TUI rows are open: "
                + summarize_names(tui_open_rows)
            )
        if tui.get("release_status") != "complete":
            checker.error(
                "compat.release_status=complete requires tui.release_status=complete"
            )

    return {
        "compat": compat,
        "tui": tui,
        "matrix": matrix,
        "matrix_open_rows": matrix_open_rows,
    }


def run_git_bytes(
    source: Path, arguments: list[str], checker: Checker, label: str
) -> bytes | None:
    try:
        process = subprocess.run(
            ["git", "--no-replace-objects", "-C", str(source), *arguments],
            check=False,
            capture_output=True,
            timeout=15,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        checker.error(f"{label}: cannot run git: {error}")
        return None
    if process.returncode != 0:
        raw_detail = process.stderr.strip() or process.stdout.strip()
        detail = raw_detail.decode("utf-8", errors="replace") or f"exit {process.returncode}"
        checker.error(f"{label}: git failed: {detail}")
        return None
    return process.stdout


def run_git(source: Path, arguments: list[str], checker: Checker, label: str) -> str | None:
    output = run_git_bytes(source, arguments, checker, label)
    if output is None:
        return None
    try:
        return output.decode("utf-8").strip()
    except UnicodeError as error:
        checker.error(f"{label}: git output is not UTF-8: {error}")
        return None


def load_git_json(
    source: Path, path: str, label: str, checker: Checker
) -> dict[str, Any] | None:
    output = run_git_bytes(
        source,
        ["cat-file", "blob", f"{PI_REVISION}:{path}"],
        checker,
        f"{label} blob",
    )
    if output is None:
        return None
    try:
        text = output.decode("utf-8")
    except UnicodeError as error:
        checker.error(f"{label}: blob is not UTF-8: {error}")
        return None
    return parse_json(text, label, checker)


def read_git_tree(
    source: Path,
    treeish: str,
    label: str,
    checker: Checker,
    *,
    recursive: bool = False,
    pathspec: str | None = None,
) -> list[dict[str, str]]:
    arguments = ["ls-tree"]
    if recursive:
        arguments.append("-r")
    arguments.extend(["-z", treeish])
    if pathspec is not None:
        arguments.extend(["--", pathspec])
    output = run_git_bytes(source, arguments, checker, label)
    if output is None:
        return []

    entries: list[dict[str, str]] = []
    for index, record in enumerate(output.split(b"\0")):
        if not record:
            continue
        metadata, separator, raw_path = record.partition(b"\t")
        fields = metadata.split(b" ")
        if not separator or len(fields) != 3 or not raw_path:
            checker.error(f"{label}: malformed ls-tree record {index}")
            continue
        try:
            mode, object_type, object_id = (field.decode("ascii") for field in fields)
            path = raw_path.decode("utf-8")
        except UnicodeError as error:
            checker.error(f"{label}: non-UTF-8 ls-tree record {index}: {error}")
            continue
        entries.append(
            {
                "mode": mode,
                "type": object_type,
                "object": object_id,
                "path": path,
            }
        )
    return entries


def is_regular_blob(entry: dict[str, str] | None) -> bool:
    return entry is not None and entry.get("type") == "blob" and entry.get(
        "mode"
    ) in {"100644", "100755"}


def validate_manifest_node_floor(
    manifest: dict[str, Any], label: str, checker: Checker
) -> None:
    engines = manifest.get("engines")
    if type(engines) is not dict:
        checker.error(f"{label}.engines: expected an object")
        return
    node_engine = string_value(engines.get("node"), f"{label}.engines.node", checker)
    if node_engine is not None:
        checker.equal(node_engine, PI_NODE_ENGINE, f"{label}.engines.node")


def compare_inventory(
    actual: list[str], expected: list[str], label: str, checker: Checker
) -> None:
    if actual == expected:
        return
    missing = sorted(set(expected) - set(actual))
    unexpected = sorted(set(actual) - set(expected))
    checker.error(
        f"{label}: inventory mismatch; missing={missing!r}, unexpected={unexpected!r}"
    )


def validate_pi_source(
    source: Path, ledger_state: dict[str, Any], checker: Checker
) -> None:
    if shutil.which("git") is None:
        checker.error("--pi-source requires git on PATH")
        return
    try:
        source = source.resolve(strict=True)
    except OSError as error:
        checker.error(f"--pi-source: cannot resolve source directory: {error}")
        return
    if not source.is_dir():
        checker.error(f"--pi-source: expected a directory, got {source}")
        return

    top_level = run_git(source, ["rev-parse", "--show-toplevel"], checker, "Pi worktree")
    if top_level is not None:
        try:
            top_level_path = Path(top_level).resolve(strict=True)
        except OSError as error:
            checker.error(f"Pi worktree: cannot resolve git top level: {error}")
        else:
            if top_level_path != source:
                checker.error(
                    "Pi worktree: --pi-source must be the checkout root "
                    f"{top_level_path}, got {source}"
                )

    object_type = run_git(source, ["cat-file", "-t", PI_REVISION], checker, "Pi commit object")
    if object_type is not None:
        checker.equal(object_type, "commit", "Pi commit object type")
    tag_ref = f"refs/tags/{PI_TAG}"
    tag_object = run_git(
        source, ["show-ref", "--verify", "--hash", tag_ref], checker, "Pi tag ref"
    )
    if tag_object is not None:
        checker.equal(tag_object, PI_REVISION, "Pi lightweight tag object")
    tag_type = run_git(source, ["cat-file", "-t", tag_ref], checker, "Pi tag object")
    if tag_type is not None:
        checker.equal(tag_type, "commit", "Pi tag object type")
    peeled_tag = run_git(
        source, ["rev-parse", "--verify", f"{tag_ref}^{{commit}}"], checker, "Pi tag target"
    )
    if peeled_tag is not None:
        checker.equal(peeled_tag, PI_REVISION, "Pi tag target")
    if object_type != "commit":
        return

    root_manifest = load_git_json(source, "package.json", "Pi root package.json", checker)
    coding_manifest = load_git_json(
        source,
        "packages/coding-agent/package.json",
        "Pi coding-agent package.json",
        checker,
    )
    tui_manifest = load_git_json(
        source, "packages/tui/package.json", "Pi TUI package.json", checker
    )
    if root_manifest is not None:
        validate_manifest_node_floor(root_manifest, "Pi root package.json", checker)
    if coding_manifest is not None:
        checker.equal(
            coding_manifest.get("name"),
            CODING_AGENT_PACKAGE["name"],
            "Pi coding-agent package name",
        )
        checker.equal(
            coding_manifest.get("version"),
            CODING_AGENT_PACKAGE["version"],
            "Pi coding-agent package version",
        )
        validate_manifest_node_floor(
            coding_manifest, "Pi coding-agent package.json", checker
        )
    if tui_manifest is not None:
        checker.equal(tui_manifest.get("name"), TUI_PACKAGE["name"], "Pi TUI package name")
        checker.equal(
            tui_manifest.get("version"), TUI_PACKAGE["version"], "Pi TUI package version"
        )
        validate_manifest_node_floor(tui_manifest, "Pi TUI package.json", checker)

    tui_entries = read_git_tree(
        source,
        f"{PI_REVISION}:packages/tui",
        "Pi TUI test tree",
        checker,
        recursive=True,
        pathspec="test",
    )
    test_entries = [entry for entry in tui_entries if entry["path"].endswith(".test.ts")]
    test_paths = sorted(entry["path"] for entry in test_entries)
    compare_inventory(
        test_paths,
        ledger_state.get("tui", {}).get("upstream_names", []),
        "Pi TUI tests",
        checker,
    )
    if len(test_paths) != TUI_TEST_COUNT:
        checker.error(f"Pi TUI tests: expected {TUI_TEST_COUNT} files, got {len(test_paths)}")
    for entry in test_entries:
        if not is_regular_blob(entry):
            checker.error(
                f"Pi TUI tests: expected a regular blob at {entry['path']!r}, "
                f"got {entry['mode']} {entry['type']}"
            )

    examples_treeish = f"{PI_REVISION}:packages/coding-agent/examples/extensions"
    direct_examples = read_git_tree(
        source, examples_treeish, "Pi extension examples tree", checker
    )
    recursive_examples = read_git_tree(
        source,
        examples_treeish,
        "Pi extension examples recursive tree",
        checker,
        recursive=True,
    )
    examples_by_name = {entry["path"]: entry for entry in direct_examples}
    recursive_by_path = {entry["path"]: entry for entry in recursive_examples}
    entries = sorted(name for name in examples_by_name if name != "README.md")
    expected_examples = ledger_state.get("compat", {}).get("examples", [])
    compare_inventory(entries, expected_examples, "Pi extension examples", checker)
    if len(entries) != EXAMPLE_COUNT:
        checker.error(
            f"Pi extension examples: expected {EXAMPLE_COUNT} entries, got {len(entries)}"
        )
    for name in expected_examples:
        entry = examples_by_name.get(name)
        if name.endswith(".ts"):
            if not is_regular_blob(entry):
                checker.error(
                    f"Pi extension example {name!r}: expected a regular TypeScript blob"
                )
            continue
        if entry is None or entry.get("type") != "tree" or entry.get("mode") != "040000":
            checker.error(f"Pi extension example {name!r}: expected a tree")
            continue
        if not is_regular_blob(recursive_by_path.get(f"{name}/index.ts")):
            checker.error(
                f"Pi extension example {name!r}: tree is missing a regular index.ts blob"
            )


def verify_repository(
    repository_root: Path, pi_source: Path | None = None
) -> tuple[list[str], dict[str, Any] | None]:
    checker = Checker()
    compat_profile = load_json(
        repository_root / COMPAT_PROFILE_PATH, "compatibility profile", checker
    )
    tui_profile = load_json(repository_root / TUI_PROFILE_PATH, "TUI profile", checker)
    try:
        compatibility_markdown = (repository_root / COMPAT_MATRIX_PATH).read_text(
            encoding="utf-8"
        )
    except (OSError, UnicodeError) as error:
        checker.error(f"compatibility matrix: cannot read: {error}")
        compatibility_markdown = ""
    state: dict[str, Any] | None = None
    if compat_profile is not None and tui_profile is not None:
        state = validate_ledgers(
            compat_profile,
            tui_profile,
            compatibility_markdown,
            repository_root,
            checker,
        )
        if pi_source is not None:
            validate_pi_source(pi_source, state, checker)
    return checker.errors, state


def run_self_tests(repository_root: Path) -> list[str]:
    load_checker = Checker()
    compat = load_json(repository_root / COMPAT_PROFILE_PATH, "self-test compat", load_checker)
    tui = load_json(repository_root / TUI_PROFILE_PATH, "self-test TUI", load_checker)
    try:
        matrix = (repository_root / COMPAT_MATRIX_PATH).read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        load_checker.error(f"self-test compatibility matrix: cannot read: {error}")
        matrix = ""
    if load_checker.errors or compat is None or tui is None:
        return load_checker.errors

    baseline = Checker()
    validate_ledgers(compat, tui, matrix, repository_root, baseline)
    if baseline.errors:
        return ["self-test baseline did not validate", *baseline.errors]

    failures: list[str] = []

    duplicate_compat = deepcopy(compat)
    examples = duplicate_compat["official_extension_examples"]
    examples[-1] = examples[0]
    duplicate_check = Checker()
    validate_ledgers(duplicate_compat, tui, matrix, repository_root, duplicate_check)
    if not any("duplicate entries" in error for error in duplicate_check.errors):
        failures.append("self-test failed to reject a duplicate example inventory")

    invalid_tui = deepcopy(tui)
    invalid_tui["test_files"][0]["rust_equivalents"] = ["../escape.rs"]
    path_check = Checker()
    validate_ledgers(compat, invalid_tui, matrix, repository_root, path_check)
    if not any("normalized repository-relative" in error for error in path_check.errors):
        failures.append("self-test failed to reject an escaping Rust equivalent path")

    complete_compat = deepcopy(compat)
    complete_tui = deepcopy(tui)
    complete_compat["release_status"] = "complete"
    complete_tui["release_status"] = "complete"
    complete_tui["test_files"][0]["required"] = True
    complete_tui["test_files"][0]["status"] = "requires_0.84.4_audit"
    release_check = Checker()
    validate_ledgers(
        complete_compat, complete_tui, matrix, repository_root, release_check
    )
    if not any("release_status=complete is forbidden" in error for error in release_check.errors):
        failures.append("self-test failed to reject a complete claim with open required rows")

    return failures


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--pi-source",
        type=Path,
        metavar="DIR",
        help=(
            "also verify the pinned commit object and lightweight tag in a Pi "
            "worktree; blobs and trees are read without switching its checkout"
        ),
    )
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="exercise duplicate, path, and premature-complete failure fixtures",
    )
    return parser


def main() -> int:
    parser = build_parser()
    arguments = parser.parse_args()
    if arguments.self_test and arguments.pi_source is not None:
        parser.error("--self-test and --pi-source cannot be combined")
    if arguments.self_test:
        failures = run_self_tests(REPOSITORY_ROOT)
        if failures:
            print("Pi parity verifier self-tests failed:", file=sys.stderr)
            for failure in failures:
                print(f"  - {failure}", file=sys.stderr)
            return 1
        print("Pi parity verifier self-tests passed (3 invalid fixtures rejected).")
        return 0

    errors, state = verify_repository(REPOSITORY_ROOT, arguments.pi_source)
    if errors:
        print("Pi parity profile verification failed:", file=sys.stderr)
        for error in errors:
            print(f"  - {error}", file=sys.stderr)
        return 1
    assert state is not None
    compat = state["compat"]
    tui = state["tui"]
    matrix_count = sum(len(rows) for rows in state["matrix"].values())
    print(
        "Verified Pi 0.84.4 ledgers: "
        f"{matrix_count} public-surface rows, {len(compat['examples'])} examples, "
        f"{len(tui['upstream_names'])} TUI test files; "
        f"release_status={compat['release_status']}/{tui['release_status']}."
    )
    if arguments.pi_source is not None:
        print(
            f"Verified pinned Pi commit object in {arguments.pi_source.resolve()} "
            f"({PI_TAG} -> {PI_REVISION}); checkout was not switched."
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
