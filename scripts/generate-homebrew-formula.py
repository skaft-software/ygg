#!/usr/bin/env python3
"""Render a Homebrew formula from a verified immutable Ygg release record.

The metadata file is produced by ``generate-ygg-release-metadata.py`` and is
signed by the protected binary-release workflow before this command is called.
This command is intentionally offline: it never reads the checkout for release
identity, calls a release API, or discovers a version from the checkout.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import pathlib
import re
import stat
import sys
from typing import Any, Iterable, Mapping, Sequence

REPOSITORY = "skaft-software/ygg"
REPOSITORY_URL = f"https://github.com/{REPOSITORY}"
SCHEMA = "ygg.release.metadata.v1"
COMMIT_PATTERN = re.compile(r"[0-9a-f]{40}")
SHA256_PATTERN = re.compile(r"[0-9a-f]{64}")
VERSION_PATTERN = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+")
MAX_METADATA_BYTES = 1024 * 1024
MAX_ASSET_BYTES = 512 * 1024 * 1024
CHECKSUM_LINE = re.compile(r"^([0-9a-f]{64})  \.\/([A-Za-z0-9_.-]+)$")


class FormulaError(Exception):
    pass


def fail(message: str) -> None:
    raise FormulaError(message)


def regular_file(path: pathlib.Path, label: str, maximum: int | None = None) -> os.stat_result:
    try:
        metadata = path.lstat()
    except FileNotFoundError:
        fail(f"{label} is missing: {path}")
    except OSError as error:
        fail(f"could not inspect {label} {path}: {error}")
    if not stat.S_ISREG(metadata.st_mode) or stat.S_ISLNK(metadata.st_mode):
        fail(f"{label} must be a regular file: {path}")
    if maximum is not None and metadata.st_size > maximum:
        fail(f"{label} exceeds its size limit: {path}")
    return metadata


def real_directory(path: pathlib.Path, label: str) -> None:
    try:
        metadata = path.lstat()
    except FileNotFoundError:
        fail(f"{label} is missing: {path}")
    except OSError as error:
        fail(f"could not inspect {label} {path}: {error}")
    if not stat.S_ISDIR(metadata.st_mode) or stat.S_ISLNK(metadata.st_mode):
        fail(f"{label} must be a real directory: {path}")


def sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    try:
        with path.open("rb") as stream:
            for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                digest.update(chunk)
    except OSError as error:
        fail(f"could not read {path}: {error}")
    return digest.hexdigest()


def parse_unique_json(path: pathlib.Path) -> Mapping[str, Any]:
    regular_file(path, "release metadata", MAX_METADATA_BYTES)

    def unique_pairs(pairs: Iterable[tuple[str, Any]]) -> dict[str, Any]:
        result: dict[str, Any] = {}
        for key, value in pairs:
            if key in result:
                raise ValueError(f"duplicate key {key}")
            result[key] = value
        return result

    try:
        value = json.loads(
            path.read_text(encoding="utf-8"), object_pairs_hook=unique_pairs
        )
    except (OSError, UnicodeDecodeError, json.JSONDecodeError, ValueError) as error:
        fail(f"release metadata is not valid unique-key UTF-8 JSON: {error}")
    if not isinstance(value, dict):
        fail("release metadata must be a JSON object")
    return value


def require_string(mapping: Mapping[str, Any], key: str, label: str) -> str:
    value = mapping.get(key)
    if not isinstance(value, str):
        fail(f"release metadata {label} must be a string")
    return value


def require_digest(value: Any, label: str) -> str:
    if not isinstance(value, str) or SHA256_PATTERN.fullmatch(value) is None:
        fail(f"{label} must be a lowercase SHA-256 digest")
    return value


def expected_asset_names(version: str) -> dict[str, tuple[str, str | None]]:
    return {
        "install-ygg.sh": ("installer", None),
        **{
            f"ygg-{version}-{target}.tar.gz": ("binary", target)
            for target, _ in (
                ("aarch64-apple-darwin", "arm64"),
                ("x86_64-apple-darwin", "x86_64"),
                ("x86_64-unknown-linux-gnu", "linux-x86_64"),
            )
        },
    }


def parse_metadata(value: Mapping[str, Any]) -> tuple[str, str, dict[str, str], str]:
    expected_top_level = {
        "schema",
        "repository",
        "tag",
        "version",
        "source_commit",
        "workflow_commit",
        "workflow_ref",
        "checksum_manifest",
        "assets",
    }
    if set(value) != expected_top_level:
        fail("release metadata has unexpected or missing top-level fields")
    if value.get("schema") != SCHEMA or value.get("repository") != REPOSITORY:
        fail("release metadata is not for the canonical Ygg repository")

    version = require_string(value, "version", "version")
    tag = require_string(value, "tag", "tag")
    if VERSION_PATTERN.fullmatch(version) is None or tag != f"v{version}":
        fail("release metadata must identify a stable vX.Y.Z release")
    for key, label in (("source_commit", "source commit"), ("workflow_commit", "workflow commit")):
        commit = require_string(value, key, label)
        if COMMIT_PATTERN.fullmatch(commit) is None:
            fail(f"release metadata {label} is malformed")
    workflow_ref = require_string(value, "workflow_ref", "workflow ref")
    expected_workflow_ref = (
        f"{REPOSITORY}/.github/workflows/release-ygg.yml@refs/tags/ygg-binaries-v{version}"
    )
    if workflow_ref != expected_workflow_ref:
        fail("release metadata workflow ref is not the immutable binary release tag")

    checksum_manifest = value["checksum_manifest"]
    if not isinstance(checksum_manifest, dict) or set(checksum_manifest) != {"name", "sha256"}:
        fail("release metadata checksum manifest is malformed")
    if checksum_manifest.get("name") != "YGG_SHA256SUMS":
        fail("release metadata checksum manifest has the wrong name")
    checksum_digest = require_digest(checksum_manifest.get("sha256"), "checksum manifest")

    expected = expected_asset_names(version)
    assets = value["assets"]
    if not isinstance(assets, list) or len(assets) != len(expected):
        fail("release metadata asset list is incomplete")
    parsed: dict[str, str] = {}
    for index, asset in enumerate(assets):
        if not isinstance(asset, dict):
            fail(f"release metadata asset {index} is not an object")
        name = asset.get("name")
        if not isinstance(name, str) or name not in expected or name in parsed:
            fail(f"release metadata asset {index} has an unexpected or repeated name")
        kind, target = expected[name]
        fields = {"name", "kind", "sha256", "url"}
        if target is not None:
            fields.add("target")
        if set(asset) != fields or asset.get("kind") != kind:
            fail(f"release metadata asset fields are malformed: {name}")
        if target is not None and asset.get("target") != target:
            fail(f"release metadata asset target is malformed: {name}")
        if asset.get("url") != f"{REPOSITORY_URL}/releases/download/{tag}/{name}":
            fail(f"release metadata asset URL is not immutable: {name}")
        parsed[name] = require_digest(asset.get("sha256"), f"asset {name}")
    if set(parsed) != set(expected):
        fail("release metadata asset set is incomplete")
    return tag, version, parsed, checksum_digest


def parse_checksum_manifest(path: pathlib.Path, version: str) -> dict[str, str]:
    regular_file(path, "local checksum manifest", MAX_METADATA_BYTES)
    expected = set(expected_asset_names(version))
    entries: dict[str, str] = {}
    try:
        lines = path.read_text(encoding="ascii").splitlines()
    except (OSError, UnicodeDecodeError) as error:
        fail(f"local checksum manifest is not ASCII text: {error}")
    for line in lines:
        match = CHECKSUM_LINE.fullmatch(line)
        if match is None or match.group(2) in entries:
            fail("local checksum manifest has a malformed or repeated entry")
        entries[match.group(2)] = match.group(1)
    if set(entries) != expected:
        fail("local checksum manifest does not contain exactly the release assets")
    return entries


def verify_local_assets(
    assets_directory: pathlib.Path,
    version: str,
    metadata_assets: Mapping[str, str],
    checksum_manifest_digest: str,
) -> None:
    real_directory(assets_directory, "local release asset directory")
    manifest = assets_directory / "YGG_SHA256SUMS"
    if sha256(manifest) != checksum_manifest_digest:
        fail("local checksum manifest does not match release metadata")
    manifest_assets = parse_checksum_manifest(manifest, version)
    if dict(manifest_assets) != dict(metadata_assets):
        fail("local checksum manifest entries do not match release metadata")
    for name, expected_digest in metadata_assets.items():
        path = assets_directory / name
        regular_file(path, f"local release asset {name}", MAX_ASSET_BYTES)
        if sha256(path) != expected_digest:
            fail(f"local release asset checksum mismatch: {name}")


def render_formula(
    tag: str,
    version: str,
    source_commit: str,
    workflow_commit: str,
    workflow_ref: str,
    checksum_manifest_digest: str,
    assets: Mapping[str, str],
) -> str:
    arm_name = f"ygg-{version}-aarch64-apple-darwin.tar.gz"
    intel_name = f"ygg-{version}-x86_64-apple-darwin.tar.gz"
    return f'''# Generated from verified immutable Ygg release metadata.
# Release tag: {tag}
# Release source commit: {source_commit}
# Release workflow commit: {workflow_commit}
# Release workflow ref: {workflow_ref}
# YGG_SHA256SUMS SHA-256: {checksum_manifest_digest}
class Ygg < Formula
  desc "High-performance coding agent"
  homepage "{REPOSITORY_URL}"
  version "{version}"
  depends_on :macos
  depends_on "ripgrep"

  on_arm do
    url "{REPOSITORY_URL}/releases/download/{tag}/{arm_name}"
    sha256 "{assets[arm_name]}"
  end

  on_intel do
    url "{REPOSITORY_URL}/releases/download/{tag}/{intel_name}"
    sha256 "{assets[intel_name]}"
  end

  def install
    root = Dir["ygg-*/"].find {{ |candidate| File.executable?(File.join(candidate, "ygg")) }}
    odie "Ygg release archive has no executable ygg binary" unless root
    bin.install File.join(root, "ygg")
    bin.install File.join(root, "ygg-host")
  end

  test do
    assert_match "ygg #{{version}}", shell_output("#{{bin}}/ygg --version")
  end
end
'''


def write_output(path: pathlib.Path, contents: str) -> None:
    if path.exists() and path.is_symlink():
        fail(f"formula output must not be a symlink: {path}")
    if not path.parent.is_dir() or path.parent.is_symlink():
        fail(f"formula output parent must be a real directory: {path.parent}")
    temporary = path.parent / f".{path.name}.tmp-{os.getpid()}"
    if temporary.exists() or temporary.is_symlink():
        fail(f"formula temporary output already exists: {temporary}")
    try:
        temporary.write_text(contents, encoding="utf-8", newline="\n")
        temporary.chmod(0o644)
        os.replace(temporary, path)
    except OSError as error:
        fail(f"could not write formula output {path}: {error}")
    finally:
        if temporary.exists() or temporary.is_symlink():
            temporary.unlink()


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Render a Homebrew formula from immutable release metadata."
    )
    parser.add_argument("metadata_path", nargs="?", type=pathlib.Path)
    parser.add_argument("--metadata", "--metadata-file", dest="metadata_option", type=pathlib.Path)
    parser.add_argument("--assets-dir", type=pathlib.Path)
    parser.add_argument("-o", "--output", type=pathlib.Path)
    return parser


def main(argv: Sequence[str]) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    if args.metadata_path is not None and args.metadata_option is not None:
        parser.error("metadata path cannot be supplied twice")
    metadata_path = args.metadata_path or args.metadata_option
    if metadata_path is None:
        parser.error("an immutable release metadata path is required")
    value = parse_unique_json(metadata_path)
    tag, version, assets, checksum_manifest_digest = parse_metadata(value)
    if args.assets_dir is not None:
        verify_local_assets(args.assets_dir, version, assets, checksum_manifest_digest)
    formula = render_formula(
        tag,
        version,
        require_string(value, "source_commit", "source commit"),
        require_string(value, "workflow_commit", "workflow commit"),
        require_string(value, "workflow_ref", "workflow ref"),
        checksum_manifest_digest,
        assets,
    )
    if args.output is None:
        sys.stdout.write(formula)
    else:
        write_output(args.output, formula)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main(sys.argv[1:]))
    except FormulaError as error:
        print(f"homebrew formula generation failed: {error}", file=sys.stderr)
        raise SystemExit(1)
