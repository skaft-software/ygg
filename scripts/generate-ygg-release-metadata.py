#!/usr/bin/env python3
"""Render the signed, immutable metadata handoff for a Ygg binary release.

The caller supplies every identity field explicitly.  This command never reads
Cargo.toml or a release API; it validates the already-created checksum asset and
its local release files before atomically writing the metadata document.
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
from typing import Any, Mapping, Sequence

REPOSITORY = "skaft-software/ygg"
TARGETS = (
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-gnu",
)
CHECKSUM_PATTERN = re.compile(r"^([0-9a-f]{64})  \.\/([A-Za-z0-9_.-]+)$")
COMMIT_PATTERN = re.compile(r"[0-9a-f]{40}")
VERSION_PATTERN = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+")


class MetadataError(Exception):
    pass


def fail(message: str) -> None:
    raise MetadataError(message)


def regular_file(path: pathlib.Path, label: str) -> None:
    try:
        metadata = path.lstat()
    except OSError as error:
        fail(f"{label} is not readable: {path}: {error}")
    if not stat.S_ISREG(metadata.st_mode):
        fail(f"{label} must be a regular file: {path}")


def sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def parse_checksums(path: pathlib.Path, version: str) -> Mapping[str, str]:
    regular_file(path, "checksum manifest")
    try:
        lines = path.read_text(encoding="ascii").splitlines()
    except (OSError, UnicodeDecodeError) as error:
        fail(f"checksum manifest is not ASCII text: {path}: {error}")
    expected = {
        "install-ygg.sh",
        *(f"ygg-{version}-{target}.tar.gz" for target in TARGETS),
    }
    entries: dict[str, str] = {}
    for line in lines:
        match = CHECKSUM_PATTERN.fullmatch(line)
        if match is None:
            fail("checksum manifest contains a malformed line")
        digest, name = match.groups()
        if name in entries:
            fail(f"checksum manifest repeats {name}")
        entries[name] = digest
    if set(entries) != expected:
        fail(f"checksum manifest must contain exactly {sorted(expected)}")
    return entries


def validate_identity(
    version: str,
    tag: str,
    source_commit: str,
    workflow_commit: str,
    workflow_ref: str,
    repository: str,
) -> None:
    if VERSION_PATTERN.fullmatch(version) is None:
        fail(f"version is not a stable release version: {version}")
    if tag != f"v{version}":
        fail(f"tag does not match version: {tag}")
    for label, value in (("source", source_commit), ("workflow", workflow_commit)):
        if COMMIT_PATTERN.fullmatch(value) is None:
            fail(f"{label} commit is malformed")
    if repository != REPOSITORY:
        fail(f"repository is not the canonical Ygg repository: {repository}")
    expected_workflow_ref = (
        f"{repository}/.github/workflows/release-ygg.yml@refs/tags/ygg-binaries-{tag}"
    )
    if workflow_ref != expected_workflow_ref:
        fail(f"workflow ref is not the immutable binary release workflow tag: {workflow_ref}")


def write_atomic(path: pathlib.Path, payload: bytes) -> None:
    if path.is_symlink():
        fail(f"metadata output must not be a symlink: {path}")
    parent = path.parent
    if not parent.is_dir() or parent.is_symlink():
        fail(f"metadata output parent must be a real directory: {parent}")
    temporary = parent / f".{path.name}.tmp-{os.getpid()}"
    if temporary.exists() or temporary.is_symlink():
        fail(f"temporary metadata output already exists: {temporary}")
    try:
        temporary.write_bytes(payload)
        temporary.chmod(0o644)
        os.replace(temporary, path)
    finally:
        if temporary.exists() or temporary.is_symlink():
            temporary.unlink()


def build_metadata(
    version: str,
    tag: str,
    source_commit: str,
    workflow_commit: str,
    workflow_ref: str,
    repository: str,
    checksums_path: pathlib.Path,
) -> dict[str, Any]:
    validate_identity(version, tag, source_commit, workflow_commit, workflow_ref, repository)
    entries = parse_checksums(checksums_path, version)
    assets: list[dict[str, str]] = []
    for name, kind, target in [
        ("install-ygg.sh", "installer", None),
        *[
            (
                f"ygg-{version}-{target}.tar.gz",
                "binary",
                target,
            )
            for target in TARGETS
        ],
    ]:
        asset_path = checksums_path.parent / name
        regular_file(asset_path, f"release asset {name}")
        digest = sha256(asset_path)
        if digest != entries[name]:
            fail(f"release asset digest disagrees with checksum manifest: {name}")
        asset: dict[str, str] = {
            "name": name,
            "kind": kind,
            "sha256": digest,
            "url": f"https://github.com/{repository}/releases/download/{tag}/{name}",
        }
        if target is not None:
            asset["target"] = target
        assets.append(asset)
    return {
        "schema": "ygg.release.metadata.v1",
        "repository": repository,
        "tag": tag,
        "version": version,
        "source_commit": source_commit,
        "workflow_commit": workflow_commit,
        "workflow_ref": workflow_ref,
        "checksum_manifest": {
            "name": checksums_path.name,
            "sha256": sha256(checksums_path),
        },
        "assets": assets,
    }


def main(argv: Sequence[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("version")
    parser.add_argument("tag")
    parser.add_argument("source_commit")
    parser.add_argument("workflow_commit")
    parser.add_argument("workflow_ref")
    parser.add_argument("repository")
    parser.add_argument("checksums", type=pathlib.Path)
    parser.add_argument("output", type=pathlib.Path)
    args = parser.parse_args(argv)
    metadata = build_metadata(
        args.version,
        args.tag,
        args.source_commit,
        args.workflow_commit,
        args.workflow_ref,
        args.repository,
        args.checksums,
    )
    payload = (json.dumps(metadata, sort_keys=True, indent=2) + "\n").encode("utf-8")
    write_atomic(args.output, payload)
    print(f"wrote immutable release metadata to {args.output}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main(sys.argv[1:]))
    except MetadataError as error:
        print(f"release metadata generation failed: {error}", file=sys.stderr)
        raise SystemExit(1)
