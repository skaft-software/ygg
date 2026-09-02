#!/usr/bin/env python3
"""Create the signed evidence manifest for an immutable Ygg npm candidate."""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import pathlib
import re
import stat
import sys
from typing import Any, Mapping, Sequence

REPOSITORY = "skaft-software/ygg"
COMMIT_PATTERN = re.compile(r"[0-9a-f]{40}")
VERSION_PATTERN = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+")
PACKAGES = (
    ("ygg-{version}.tgz", "@skaft-software/ygg", "launcher"),
    (
        "ygg-darwin-arm64-{version}.tgz",
        "@skaft-software/ygg-darwin-arm64",
        "aarch64-apple-darwin",
    ),
    (
        "ygg-darwin-x64-{version}.tgz",
        "@skaft-software/ygg-darwin-x64",
        "x86_64-apple-darwin",
    ),
    (
        "ygg-linux-x64-gnu-{version}.tgz",
        "@skaft-software/ygg-linux-x64-gnu",
        "x86_64-unknown-linux-gnu",
    ),
)


class ManifestError(Exception):
    pass


def fail(message: str) -> None:
    raise ManifestError(message)


def regular_file(path: pathlib.Path, label: str) -> None:
    try:
        metadata = path.lstat()
    except OSError as error:
        fail(f"{label} is not readable: {path}: {error}")
    if not stat.S_ISREG(metadata.st_mode):
        fail(f"{label} must be a regular file: {path}")


def digest(path: pathlib.Path, algorithm: str) -> str:
    hasher = hashlib.new(algorithm)
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            hasher.update(chunk)
    return hasher.hexdigest()


def read_release_metadata(
    path: pathlib.Path,
    version: str,
    tag: str,
    source_commit: str,
    workflow_commit: str,
) -> Mapping[str, Any]:
    regular_file(path, "release metadata")
    try:
        def no_duplicates(pairs: Sequence[tuple[str, Any]]) -> dict[str, Any]:
            result: dict[str, Any] = {}
            for key, value in pairs:
                if key in result:
                    raise ValueError(f"duplicate key {key}")
                result[key] = value
            return result

        value = json.loads(
            path.read_text(encoding="utf-8"), object_pairs_hook=no_duplicates
        )
    except (OSError, UnicodeDecodeError, json.JSONDecodeError, ValueError) as error:
        fail(f"release metadata is not valid JSON: {error}")
    if not isinstance(value, dict):
        fail("release metadata must be a JSON object")
    expected = {
        "schema": "ygg.release.metadata.v1",
        "repository": REPOSITORY,
        "tag": tag,
        "version": version,
        "source_commit": source_commit,
        "workflow_commit": workflow_commit,
    }
    if set(value) != {
        *expected,
        "workflow_ref",
        "checksum_manifest",
        "assets",
    }:
        fail("release metadata has unexpected fields")
    for key, expected_value in expected.items():
        if value.get(key) != expected_value:
            fail(f"release metadata field {key} does not match the candidate")
    workflow_ref = value.get("workflow_ref")
    if not isinstance(workflow_ref, str) or not workflow_ref.startswith(
        f"{REPOSITORY}/.github/workflows/release-ygg.yml@"
    ):
        fail("release metadata workflow ref is not canonical")
    checksum_manifest = value.get("checksum_manifest")
    if not isinstance(checksum_manifest, dict) or set(checksum_manifest) != {"name", "sha256"}:
        fail("release metadata checksum manifest is malformed")
    if checksum_manifest.get("name") != "YGG_SHA256SUMS" or not isinstance(
        checksum_manifest.get("sha256"), str
    ) or re.fullmatch(r"[0-9a-f]{64}", checksum_manifest["sha256"]) is None:
        fail("release metadata checksum manifest identity is malformed")
    expected_assets = {
        "install-ygg.sh": ("installer", None),
        f"ygg-{version}-aarch64-apple-darwin.tar.gz": (
            "binary",
            "aarch64-apple-darwin",
        ),
        f"ygg-{version}-x86_64-apple-darwin.tar.gz": (
            "binary",
            "x86_64-apple-darwin",
        ),
        f"ygg-{version}-x86_64-unknown-linux-gnu.tar.gz": (
            "binary",
            "x86_64-unknown-linux-gnu",
        ),
    }
    assets = value.get("assets")
    if not isinstance(assets, list) or len(assets) != len(expected_assets):
        fail("release metadata has an incomplete asset list")
    seen_assets: set[str] = set()
    for asset in assets:
        if not isinstance(asset, dict):
            fail("release metadata asset is not an object")
        name = asset.get("name")
        if not isinstance(name, str) or name in seen_assets or name not in expected_assets:
            fail("release metadata has an unexpected or repeated asset")
        kind, target = expected_assets[name]
        expected_fields = {"name", "kind", "sha256", "url"}
        if target is not None:
            expected_fields.add("target")
        if set(asset) != expected_fields or asset.get("kind") != kind:
            fail(f"release metadata asset fields are malformed: {name}")
        if target is not None and asset.get("target") != target:
            fail(f"release metadata asset target is malformed: {name}")
        if not isinstance(asset.get("sha256"), str) or re.fullmatch(
            r"[0-9a-f]{64}", asset["sha256"]
        ) is None:
            fail(f"release metadata asset digest is malformed: {name}")
        if asset.get("url") != f"https://github.com/{REPOSITORY}/releases/download/{tag}/{name}":
            fail(f"release metadata asset URL is malformed: {name}")
        seen_assets.add(name)
    if seen_assets != set(expected_assets):
        fail("release metadata asset set is incomplete")
    return value


def write_atomic(path: pathlib.Path, payload: bytes) -> None:
    if path.is_symlink():
        fail(f"manifest output must not be a symlink: {path}")
    parent = path.parent
    if not parent.is_dir() or parent.is_symlink():
        fail(f"manifest output parent must be a real directory: {parent}")
    temporary = parent / f".{path.name}.tmp-{os.getpid()}"
    if temporary.exists() or temporary.is_symlink():
        fail(f"temporary manifest output already exists: {temporary}")
    try:
        temporary.write_bytes(payload)
        temporary.chmod(0o644)
        os.replace(temporary, path)
    finally:
        if temporary.exists() or temporary.is_symlink():
            temporary.unlink()


def build_manifest(
    version: str,
    tag: str,
    source_commit: str,
    workflow_commit: str,
    release_metadata_path: pathlib.Path,
    package_directory: pathlib.Path,
) -> dict[str, Any]:
    if VERSION_PATTERN.fullmatch(version) is None or tag != f"v{version}":
        fail("version and tag do not identify a stable release")
    for label, value in (("source", source_commit), ("workflow", workflow_commit)):
        if COMMIT_PATTERN.fullmatch(value) is None:
            fail(f"{label} commit is malformed")
    if not package_directory.is_dir() or package_directory.is_symlink():
        fail(f"npm package directory must be a real directory: {package_directory}")
    release_metadata = read_release_metadata(
        release_metadata_path, version, tag, source_commit, workflow_commit
    )
    expected = [name.format(version=version) for name, _, _ in PACKAGES]
    actual = sorted(path.name for path in package_directory.iterdir() if path.name.endswith(".tgz"))
    if actual != sorted(expected):
        fail(f"npm package set does not match the candidate: expected {sorted(expected)}, found {actual}")
    packages: list[dict[str, Any]] = []
    for pattern, name, target in PACKAGES:
        artifact = pattern.format(version=version)
        path = package_directory / artifact
        regular_file(path, f"npm package {artifact}")
        packages.append(
            {
                "artifact": artifact,
                "name": name,
                "target": target,
                "bytes": path.stat().st_size,
                "sha256": digest(path, "sha256"),
                "sha512_integrity": "sha512-"
                + base64.b64encode(bytes.fromhex(digest(path, "sha512"))).decode("ascii"),
            }
        )
    return {
        "schema": "ygg.npm.release.v1",
        "repository": REPOSITORY,
        "tag": tag,
        "version": version,
        "source_commit": source_commit,
        "workflow_commit": workflow_commit,
        "release_metadata_sha256": digest(release_metadata_path, "sha256"),
        "packages": packages,
    }


def write_checksums(path: pathlib.Path, package_directory: pathlib.Path, manifest_path: pathlib.Path) -> None:
    if path.is_symlink():
        fail(f"npm checksum output must not be a symlink: {path}")
    parent = path.parent
    if not parent.is_dir() or parent.is_symlink():
        fail(f"npm checksum output parent must be a real directory: {parent}")
    files = sorted([manifest_path, *package_directory.glob("*.tgz")], key=lambda item: item.name)
    for item in files:
        regular_file(item, f"npm package evidence {item.name}")
    lines = [f"{digest(item, 'sha256')}  ./{item.name}" for item in files]
    write_atomic(path, ("\n".join(lines) + "\n").encode("ascii"))


def main(argv: Sequence[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("version")
    parser.add_argument("tag")
    parser.add_argument("source_commit")
    parser.add_argument("workflow_commit")
    parser.add_argument("release_metadata", type=pathlib.Path)
    parser.add_argument("package_directory", type=pathlib.Path)
    parser.add_argument("output_manifest", type=pathlib.Path)
    parser.add_argument("output_checksums", type=pathlib.Path)
    args = parser.parse_args(argv)
    metadata = build_manifest(
        args.version,
        args.tag,
        args.source_commit,
        args.workflow_commit,
        args.release_metadata,
        args.package_directory,
    )
    payload = (json.dumps(metadata, sort_keys=True, indent=2) + "\n").encode("utf-8")
    write_atomic(args.output_manifest, payload)
    write_checksums(args.output_checksums, args.package_directory, args.output_manifest)
    print(f"wrote npm candidate metadata to {args.output_manifest}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main(sys.argv[1:]))
    except ManifestError as error:
        print(f"npm manifest generation failed: {error}", file=sys.stderr)
        raise SystemExit(1)
