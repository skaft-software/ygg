#!/usr/bin/env python3
"""Verify npm registry metadata and its signed provenance payload."""

from __future__ import annotations

import argparse
import base64
import binascii
import json
import re
import sys
from pathlib import Path
from typing import Any, Mapping, Sequence
from urllib.parse import unquote, urlsplit


REPOSITORY_HOST = "registry.npmjs.org"
SLSA_PROVENANCE_V1 = "https://slsa.dev/provenance/v1"
PACKAGE_ARTIFACTS = {
    "@skaft-software/ygg": "ygg-{version}.tgz",
    "@skaft-software/ygg-darwin-arm64": "ygg-darwin-arm64-{version}.tgz",
    "@skaft-software/ygg-darwin-x64": "ygg-darwin-x64-{version}.tgz",
    "@skaft-software/ygg-linux-x64-gnu": "ygg-linux-x64-gnu-{version}.tgz",
}


class VerificationError(Exception):
    pass


def fail(message: str) -> None:
    raise VerificationError(message)


def parse_unique_json(raw: str, path: Path) -> Any:
    try:
        def no_duplicates(pairs: Sequence[tuple[str, Any]]) -> dict[str, Any]:
            result: dict[str, Any] = {}
            for key, value in pairs:
                if key in result:
                    raise ValueError(f"duplicate key {key}")
                result[key] = value
            return result

        return json.loads(raw, object_pairs_hook=no_duplicates)
    except (UnicodeDecodeError, json.JSONDecodeError, ValueError) as error:
        fail(f"{path} is not valid unique-key JSON: {error}")


def load_json(path: Path) -> Mapping[str, Any]:
    try:
        raw = path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError) as error:
        fail(f"cannot read JSON {path}: {error}")
    value = parse_unique_json(raw, path)
    if not isinstance(value, dict):
        fail(f"{path} must be a JSON object")
    return value


def sha512_hex(integrity: str) -> str:
    prefix, separator, encoded = integrity.partition("-")
    if prefix != "sha512" or not separator or not encoded:
        fail("expected package integrity must be a sha512 SRI value")
    try:
        decoded = base64.b64decode(encoded, validate=True)
    except (ValueError, binascii.Error) as error:
        fail(f"expected package integrity is not valid base64: {error}")
    if len(decoded) != 64:
        fail("expected package integrity must decode to a SHA-512 digest")
    return decoded.hex()


def normalized_repository(value: str) -> str:
    parsed = urlsplit(value)
    if parsed.scheme and parsed.netloc:
        if parsed.scheme not in {"http", "https"} or parsed.query or parsed.fragment:
            return ""
        result = f"{parsed.netloc}{parsed.path}"
    else:
        result = value
    return result.removesuffix("/").removesuffix(".git").lower()


def validate_attestation_url(value: Any, package: str, version: str) -> None:
    if not isinstance(value, str):
        fail(f"registry provenance URL is missing for {package}@{version}")
    try:
        parsed = urlsplit(value)
        port = parsed.port
    except ValueError as error:
        fail(f"registry provenance URL is malformed: {error}")
    if (
        parsed.scheme != "https"
        or parsed.hostname != REPOSITORY_HOST
        or parsed.username is not None
        or parsed.password is not None
        or port is not None
        or parsed.query
        or parsed.fragment
        or unquote(parsed.path) != f"/-/npm/v1/attestations/{package}@{version}"
    ):
        fail(f"registry provenance URL is not the canonical npm endpoint for {package}@{version}")


def decode_payload(attestation: Mapping[str, Any], index: int) -> Mapping[str, Any]:
    bundle = attestation.get("bundle")
    if not isinstance(bundle, Mapping):
        fail(f"npm provenance attestation {index} has no bundle")
    if not isinstance(bundle.get("verificationMaterial"), Mapping):
        fail(f"npm provenance attestation {index} has no verification material")
    envelope = bundle.get("dsseEnvelope")
    if not isinstance(envelope, Mapping) or not isinstance(envelope.get("signatures"), list):
        fail(f"npm provenance attestation {index} has no DSSE signatures")
    signatures = envelope["signatures"]
    if not any(
        isinstance(signature, Mapping)
        and isinstance(signature.get("sig"), str)
        and signature["sig"]
        for signature in signatures
    ):
        fail(f"npm provenance attestation {index} has no DSSE signatures")
    encoded = envelope.get("payload")
    if not isinstance(encoded, str) or not encoded:
        fail(f"npm provenance attestation {index} has no DSSE payload")
    try:
        raw = base64.b64decode(encoded, validate=True).decode("utf-8")
    except (UnicodeDecodeError, ValueError, binascii.Error) as error:
        fail(f"npm provenance attestation {index} has an invalid DSSE payload: {error}")
    value = parse_unique_json(raw, Path(f"attestation-{index}.payload"))
    if not isinstance(value, dict):
        fail(f"npm provenance attestation {index} payload is not an object")
    return value


def load_attestation_payloads(document: Mapping[str, Any]) -> list[Mapping[str, Any]]:
    attestations = document.get("attestations")
    if not isinstance(attestations, list) or not attestations:
        fail("npm attestation response contains no attestations")
    payloads = []
    for index, attestation in enumerate(attestations):
        if not isinstance(attestation, Mapping):
            fail(f"npm provenance attestation {index} is not an object")
        if attestation.get("predicateType") != SLSA_PROVENANCE_V1:
            continue
        payload = decode_payload(attestation, index)
        if payload.get("predicateType") == SLSA_PROVENANCE_V1:
            payloads.append(payload)
    if not payloads:
        fail("npm attestations contain no SLSA provenance v1 payload")
    return payloads


def subject_matches(payload: Mapping[str, Any], package: str, version: str, digest_hex: str) -> bool:
    subjects = payload.get("subject")
    if not isinstance(subjects, list):
        return False
    expected_name = f"pkg:npm/{package}@{version}"
    for subject in subjects:
        if not isinstance(subject, Mapping):
            continue
        digest = subject.get("digest")
        if not isinstance(digest, Mapping) or digest.get("sha512") != digest_hex:
            continue
        name = subject.get("name")
        if isinstance(name, str) and unquote(name) == expected_name:
            return True
    return False


def workflow_matches(
    payload: Mapping[str, Any],
    repository: str,
    workflow_path: str,
    workflow_commit: str,
) -> bool:
    predicate = payload.get("predicate")
    if not isinstance(predicate, Mapping):
        return False
    build_definition = predicate.get("buildDefinition")
    if not isinstance(build_definition, Mapping):
        return False
    external_parameters = build_definition.get("externalParameters")
    if not isinstance(external_parameters, Mapping):
        return False
    workflow = external_parameters.get("workflow")
    if not isinstance(workflow, Mapping):
        return False
    if normalized_repository(str(workflow.get("repository", ""))) != normalized_repository(repository):
        return False
    if workflow.get("path") != workflow_path:
        return False
    dependencies = build_definition.get("resolvedDependencies")
    if not isinstance(dependencies, list):
        return False
    return any(
        isinstance(dependency, Mapping)
        and isinstance(dependency.get("uri"), str)
        and isinstance(dependency.get("digest"), Mapping)
        and dependency["digest"].get("gitCommit") == workflow_commit
        for dependency in dependencies
    )


def verify_manifest(
    manifest: Mapping[str, Any],
    package: str,
    version: str,
    expected_integrity: str,
    source_commit: str,
    workflow_commit: str,
) -> None:
    expected_identity = {
        "schema": "ygg.npm.release.v1",
        "repository": "skaft-software/ygg",
        "tag": f"v{version}",
        "version": version,
        "source_commit": source_commit,
        "workflow_commit": workflow_commit,
    }
    if any(manifest.get(key) != value for key, value in expected_identity.items()):
        fail("npm manifest identity does not match the release candidate")
    packages = manifest.get("packages")
    if not isinstance(packages, list) or len(packages) != len(PACKAGE_ARTIFACTS):
        fail("npm manifest package list is malformed")
    package_names = {
        entry.get("name") for entry in packages if isinstance(entry, Mapping)
    }
    if package_names != set(PACKAGE_ARTIFACTS):
        fail("npm manifest package list is not the expected four-package release")
    expected_artifact_template = PACKAGE_ARTIFACTS.get(package)
    if expected_artifact_template is None:
        fail(f"unsupported npm release package: {package}")
    matching = [entry for entry in packages if isinstance(entry, Mapping) and entry.get("name") == package]
    expected_artifact = expected_artifact_template.format(version=version)
    if (
        len(matching) != 1
        or matching[0].get("artifact") != expected_artifact
        or matching[0].get("sha512_integrity") != expected_integrity
    ):
        fail(f"npm manifest does not bind {package}@{version} to the expected artifact")


def verify(
    metadata: Mapping[str, Any],
    attestation_document: Mapping[str, Any],
    manifest: Mapping[str, Any],
    package: str,
    version: str,
    expected_integrity: str,
    repository: str,
    workflow_path: str,
    source_commit: str,
    workflow_commit: str,
) -> dict[str, str]:
    if metadata.get("name") != package or metadata.get("version") != version:
        fail(f"registry metadata identity does not match {package}@{version}")
    dist = metadata.get("dist")
    if not isinstance(dist, Mapping) or dist.get("integrity") != expected_integrity:
        fail(f"registry integrity does not match {package}@{version}")
    registry_attestations = dist.get("attestations")
    if not isinstance(registry_attestations, Mapping):
        fail(f"registry attestations are missing for {package}@{version}")
    provenance = registry_attestations.get("provenance")
    if not isinstance(provenance, Mapping):
        fail(f"registry provenance is missing for {package}@{version}")
    if provenance.get("predicateType") != SLSA_PROVENANCE_V1:
        fail(f"registry provenance is not SLSA v1 for {package}@{version}")
    validate_attestation_url(registry_attestations.get("url"), package, version)
    verify_manifest(manifest, package, version, expected_integrity, source_commit, workflow_commit)

    digest_hex = sha512_hex(expected_integrity)
    for payload in load_attestation_payloads(attestation_document):
        if subject_matches(payload, package, version, digest_hex) and workflow_matches(
            payload, repository, workflow_path, workflow_commit
        ):
            return {
                "package": package,
                "version": version,
                "integrity": expected_integrity,
                "provenance": "bound",
            }
    fail(f"npm provenance is not bound to the artifact, workflow, or release commit for {package}@{version}")


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("metadata", type=Path)
    parser.add_argument("attestations", type=Path)
    parser.add_argument("manifest", type=Path)
    parser.add_argument("package")
    parser.add_argument("version")
    parser.add_argument("expected_integrity")
    parser.add_argument("repository")
    parser.add_argument("workflow_path")
    parser.add_argument("source_commit")
    parser.add_argument("workflow_commit")
    args = parser.parse_args(argv)
    for label, value in (("source", args.source_commit), ("workflow", args.workflow_commit)):
        if re.fullmatch(r"[0-9a-f]{40}", value) is None:
            parser.error(f"{label} commit is not a full lowercase SHA")
    try:
        result = verify(
            load_json(args.metadata),
            load_json(args.attestations),
            load_json(args.manifest),
            args.package,
            args.version,
            args.expected_integrity,
            args.repository,
            args.workflow_path,
            args.source_commit,
            args.workflow_commit,
        )
    except VerificationError as error:
        print(f"npm provenance verification failed: {error}", file=sys.stderr)
        return 1
    print(json.dumps(result, sort_keys=True, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
