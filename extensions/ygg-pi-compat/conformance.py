#!/usr/bin/env python3
"""Evidence-oriented Pi 0.84.4 conformance gates.

Without --full this validates only checked-in fixtures and ledgers.  It never
labels that result as real-runtime coverage.  --full requires integrity-checked
local artifacts and uses a fresh HOME plus Linux network namespace before it
loads unchanged upstream sources.
"""
from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import select
import stat
import subprocess
import sys
import tarfile
import tempfile
import time

ROOT = Path(__file__).resolve().parent
REPO = ROOT.parents[1]
PROFILE_PATH = ROOT / "profiles/0.84.4.json"
LEDGER_PATH = ROOT / "profiles/0.84.4.ledger.json"
INTEGRITY_PATH = ROOT / "profiles/0.84.4.integrity.json"
FIXTURE_DIR = ROOT / "tests/fixtures/conformance"
TUI_PROFILE_PATH = REPO / "crates/sexy-tui-rs/upstream/pi-tui-0.84.4.json"
BRIDGE = ROOT / "bridge.mjs"
PROFILE = "pi-0.84.4"
REVISION = "b79e4cc834970cca69daebffab7df1da7d1e52c4"
VERSION = "0.84.4"
SKIP = {".git", ".pytest_cache", "__pycache__", "node_modules", "target"}
MAX_FILES, MAX_ENTRIES, MAX_DEPTH, MAX_BYTES = 4096, 8192, 64, 64 * 1024 * 1024
MAX_TARBALL_BYTES = 128 * 1024 * 1024


class GateFailure(RuntimeError):
    pass


def fail(message: str):
    raise GateFailure(message)


def document(path: Path):
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        fail(f"cannot read {path.relative_to(REPO)}: {error}")
    if not isinstance(value, dict):
        fail(f"{path.relative_to(REPO)} is not a JSON object")
    return value


def nonempty(value, label):
    if not isinstance(value, str) or not value:
        fail(f"{label} must be a non-empty string")
    return value


def fixture_index(path: Path):
    result = {}
    for row in document(path).get("fixtures", []):
        if not isinstance(row, dict):
            fail(f"fixture row in {path.name} is not an object")
        identifier = nonempty(row.get("id"), f"fixture id in {path.name}")
        if identifier in result:
            fail(f"duplicate fixture {identifier} in {path.name}")
        result[identifier] = row
    return result


def verify_profile_digest(profile):
    sidecar = document(INTEGRITY_PATH)
    actual = hashlib.sha256(PROFILE_PATH.read_bytes()).hexdigest()
    if (
        sidecar.get("schema_version") != 1
        or sidecar.get("profile") != profile.get("profile")
        or sidecar.get("algorithm") != "sha256"
        or sidecar.get("encoding") != "raw_utf8_bytes"
        or sidecar.get("digest") != actual
    ):
        fail("profile integrity sidecar does not authenticate the current raw profile bytes")


def check_static():
    profile, ledger, tui = document(PROFILE_PATH), document(LEDGER_PATH), document(TUI_PROFILE_PATH)
    public = fixture_index(FIXTURE_DIR / "public-surfaces.json")
    examples_fixture = fixture_index(FIXTURE_DIR / "official-examples.json")
    tui_fixture = fixture_index(FIXTURE_DIR / "tui-audit.json")
    plan = document(FIXTURE_DIR / "plan-mode-journey.json")

    if profile.get("schema_version") != 1 or profile.get("profile") != PROFILE:
        fail("wrong Pi profile schema or name")
    if profile.get("release_status") != "dogfood_conformance":
        fail("profile must identify dogfood conformance status")
    if profile.get("source", {}).get("revision") != REVISION:
        fail("profile source revision is not pinned Pi 0.84.4")
    if profile.get("node", {}).get("minimum_version") != "22.19.0":
        fail("profile Node minimum is not 22.19.0")
    packages = profile.get("packages", {})
    if packages.get("coding_agent", {}).get("version") != VERSION or packages.get("tui", {}).get("version") != VERSION:
        fail("profile package versions are not exactly 0.84.4")
    for package_name in ("coding_agent", "tui"):
        sri_bytes(packages.get(package_name, {}).get("npm_integrity"), f"{package_name} npm integrity")
    verify_profile_digest(profile)
    conformance = profile.get("conformance")
    expected_files = {
        "tests/fixtures/conformance/public-surfaces.json",
        "tests/fixtures/conformance/official-examples.json",
        "tests/fixtures/conformance/plan-mode-journey.json",
        "tests/fixtures/conformance/tui-audit.json",
    }
    if not isinstance(conformance, dict) or conformance.get("ledger") != "0.84.4.ledger.json" or set(conformance.get("fixtures", [])) != expected_files:
        fail("profile conformance cross-links are incomplete")

    if ledger.get("schema_version") != 1 or ledger.get("profile") != PROFILE or ledger.get("release_status") != "dogfood_conformance":
        fail("ledger identity/status is incorrect")
    if "not" + " implemented" in json.dumps(ledger, sort_keys=True).lower():
        fail("ledger contains an unclassified implementation status")
    decisions = {
        entry.get("id") for entry in ledger.get("safe_divergence_decisions", [])
        if isinstance(entry, dict) and isinstance(entry.get("id"), str)
    }
    decision = "pi-0.84.4-dogfood-explicit-safe-divergence"
    if decision not in decisions:
        fail("ledger lacks its named safe-divergence decision")

    expected_surfaces = [
        (area, name) for area, names in profile.get("public_surface", {}).items() for name in names
    ]
    if len(expected_surfaces) != 118:
        fail(f"profile has {len(expected_surfaces)} surfaces, expected 118")
    rows = ledger.get("public_surfaces")
    if not isinstance(rows, list) or len(rows) != 118:
        fail("ledger must contain exactly 118 public-surface rows")
    indexed = {}
    for row in rows:
        if not isinstance(row, dict):
            fail("public-surface ledger row is not an object")
        area, name = nonempty(row.get("area"), "surface area"), nonempty(row.get("surface"), "surface name")
        key = (area, name)
        if key in indexed:
            fail(f"duplicate public-surface row {area}.{name}")
        indexed[key] = row
        if row.get("status") not in {"passing", "safe_divergence", "known_dogfood_bug"}:
            fail(f"invalid status for {area}.{name}")
        nonempty(row.get("behavior"), f"behavior for {area}.{name}")
        fixture = nonempty(row.get("behavioral_fixture"), f"fixture for {area}.{name}")
        if fixture not in public or public[fixture].get("surface") != f"{area}.{name}":
            fail(f"broken fixture link for {area}.{name}")
        if row.get("status") != "passing":
            divergence = row.get("safe_divergence")
            if not isinstance(divergence, dict) or divergence.get("decision") not in decisions:
                fail(f"{area}.{name} has no named safe divergence")
            nonempty(divergence.get("reason"), f"safe-divergence reason for {area}.{name}")
    if set(indexed) != set(expected_surfaces):
        fail("ledger public-surface inventory differs from profile")
    if set(public) != {row["behavioral_fixture"] for row in rows}:
        fail("public fixture catalog is not an exact ledger mirror")

    examples = profile.get("official_extension_examples")
    ledger_examples = ledger.get("official_examples")
    if not isinstance(examples, list) or len(examples) != 78 or len(set(examples)) != 78:
        fail("profile must retain 78 unique official examples")
    if not isinstance(ledger_examples, list) or [row.get("upstream") for row in ledger_examples] != examples:
        fail("ledger example inventory/order differs from profile")
    directories = 0
    for row in ledger_examples:
        if not isinstance(row, dict):
            fail("example ledger row is not an object")
        upstream = nonempty(row.get("upstream"), "example upstream")
        kind = "file" if upstream.endswith(".ts") else "directory"
        if row.get("kind") != kind:
            fail(f"wrong kind for {upstream}")
        directories += kind == "directory"
        for key in ("load_fixture", "behavioral_fixture"):
            value = nonempty(row.get(key), f"{key} for {upstream}")
            if value not in examples_fixture and value != "plan-mode:full-journey":
                fail(f"missing example fixture {value} for {upstream}")
        for surface in row.get("exercised_surfaces", []):
            if not isinstance(surface, str):
                fail(f"non-string example surface in {upstream}")
            area, separator, name = surface.partition(".")
            if not separator or (area, name) not in indexed:
                fail(f"unknown example surface {surface} in {upstream}")
    if directories != 9:
        fail(f"inventory has {directories} directory examples, expected 9")

    tui_rows, upstream_tui = ledger.get("tui_audit"), tui.get("test_files")
    if not isinstance(tui_rows, list) or not isinstance(upstream_tui, list) or len(tui_rows) != 33:
        fail("ledger must retain 33 TUI audit rows")
    if [row.get("upstream") for row in tui_rows] != [row.get("upstream") for row in upstream_tui]:
        fail("TUI audit inventory differs from pinned upstream list")
    tui_ids = set()
    for row in tui_rows:
        fixture = nonempty(row.get("behavioral_fixture"), "TUI fixture")
        tui_ids.add(fixture)
        if fixture not in tui_fixture or row.get("status") not in {"passing", "safe_divergence", "known_dogfood_bug"}:
            fail(f"invalid TUI audit row {row.get('upstream')!r}")
        if row.get("status") != "passing" and not isinstance(row.get("safe_divergence"), dict):
            fail(f"TUI divergence lacks decision for {row.get('upstream')!r}")
    if set(tui_fixture) != tui_ids:
        fail("TUI fixture catalog is not an exact audit mirror")

    expected_journeys = {
        "plan-toggle-and-policy", "plan-interception", "plan-persistence-resume",
        "plan-dialogs-and-widgets", "plan-messaging", "plan-commands-flags-shortcuts",
    }
    deferred_host_seams = [
        "tool_policy", "session_state", "messages", "widgets", "editor", "shortcuts", "flags",
    ]
    journeys = plan.get("journeys")
    if plan.get("profile") != PROFILE or plan.get("source") != "examples/extensions/plan-mode/index.ts" or not isinstance(journeys, list):
        fail("plan fixture identity is incorrect")
    if plan.get("deferred_host_seams") != deferred_host_seams or "requires_opt_in_host_seam" in plan:
        fail("plan fixture must record deferred bridge host-control seams")
    if {row.get("id") for row in journeys if isinstance(row, dict)} != expected_journeys:
        fail("plan fixture is missing a required journey")
    if not all(isinstance(row, dict) and str(row.get("assertion", "")).startswith(("Deferred:", "Supported:")) for row in journeys):
        fail("plan fixture must distinguish deferred and currently supported behavior")
    plan_ids = {nonempty(row.get("fixture"), "plan fixture") for row in journeys}
    static_gates = {
        "profile-integrity", "official-example-inventory", "bridge-cancellation", "bridge-bounds",
        "host-supervisor-restart", "host-extension-trust", "bridge-source-fingerprint",
        "host-sanitized-environment", "legacy-api-regression", "generated-api-0.3", "tui-audit",
        "plan-mode:full-journey",
    }
    known = set(public) | set(examples_fixture) | set(tui_fixture) | plan_ids | static_gates
    for gate in ledger.get("gates", []):
        if not isinstance(gate, dict) or nonempty(gate.get("fixture"), "gate fixture") not in known:
            fail(f"ledger gate has an unknown fixture: {gate!r}")
    return {
        "profile": PROFILE, "public_surface_rows": 118, "official_examples": 78,
        "directory_examples": directories, "tui_audit_rows": 33, "plan_journeys": len(journeys),
        "real_runtime": "not_supplied",
    }


def sri_bytes(value, label):
    if not isinstance(value, str) or not value.startswith("sha512-"):
        fail(f"{label} has no sha512 npm SRI")
    try:
        decoded = base64.b64decode(value[7:], validate=True)
    except ValueError as error:
        fail(f"invalid {label} SRI: {error}")
    if len(decoded) != hashlib.sha512().digest_size:
        fail(f"invalid {label} SRI digest length")
    return decoded


def digest_regular_file(path: Path, label: str, algorithm, limit=MAX_BYTES):
    try:
        before = path.lstat()
        if not stat.S_ISREG(before.st_mode) or before.st_size > limit:
            fail(f"{label} is not a bounded regular file")
        digest = algorithm()
        with path.open("rb") as handle:
            for chunk in iter(lambda: handle.read(65536), b""):
                digest.update(chunk)
        after = path.lstat()
    except OSError as error:
        fail(f"cannot read {label}: {error}")
    if (before.st_size, before.st_mtime_ns, before.st_ino, before.st_dev) != (
        after.st_size,
        after.st_mtime_ns,
        after.st_ino,
        after.st_dev,
    ):
        fail(f"{label} changed while hashing")
    return digest.digest()


def read_regular_file(path: Path, label: str, limit=MAX_BYTES):
    try:
        before = path.lstat()
        if not stat.S_ISREG(before.st_mode) or before.st_size > limit:
            fail(f"{label} is not a bounded regular file")
        value = path.read_bytes()
        after = path.lstat()
    except OSError as error:
        fail(f"cannot read {label}: {error}")
    if (before.st_size, before.st_mtime_ns, before.st_ino, before.st_dev) != (
        after.st_size,
        after.st_mtime_ns,
        after.st_ino,
        after.st_dev,
    ):
        fail(f"{label} changed while reading")
    return value


def verify_tarball(path: Path, sri: str, label: str):
    if digest_regular_file(path, f"{label} tarball", hashlib.sha512, MAX_TARBALL_BYTES) != sri_bytes(sri, label):
        fail(f"{label} tarball does not match pinned npm integrity")


def tar_package_files(archive, label):
    files, total = {}, 0
    for member in archive.getmembers():
        name = member.name
        path = PurePosixPath(name)
        if name.startswith("/") or any(part in {"", ".", ".."} for part in path.parts) or not path.parts or path.parts[0] != "package":
            fail(f"{label} tarball has unsafe package member {name!r}")
        parts = path.parts[1:]
        if not parts:
            if not member.isdir():
                fail(f"{label} tarball has non-directory package root")
            continue
        relative = "/".join(parts)
        if member.isdir():
            continue
        if not member.isfile():
            fail(f"{label} tarball has unsupported package member {name!r}")
        if relative in files:
            fail(f"{label} tarball has duplicate package member {relative!r}")
        if len(files) >= MAX_FILES or member.size > MAX_BYTES - total:
            fail(f"{label} tarball package content exceeds conformance bounds")
        files[relative] = member
        total += member.size
    if not files:
        fail(f"{label} tarball has no package files")
    return files


def package_disk_files(root: Path, label: str):
    try:
        metadata = root.lstat()
    except OSError as error:
        fail(f"cannot inspect {label} root: {error}")
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
        fail(f"{label} root is not a real directory")
    files, directories, entries = {}, [root], 0
    while directories:
        directory = directories.pop()
        try:
            children = list(directory.iterdir())
        except OSError as error:
            fail(f"cannot inspect {label} root: {error}")
        for child in children:
            try:
                data = child.lstat()
            except OSError as error:
                fail(f"cannot inspect {label} root: {error}")
            relative = child.relative_to(root).as_posix()
            entries += 1
            if entries > MAX_ENTRIES or len(relative.encode()) > 4096 or len(relative.split("/")) > MAX_DEPTH:
                fail(f"{label} root exceeds conformance path bounds")
            # Dependencies are outside the npm package payload. They are checked
            # separately when the Node resolver selects the pinned Pi TUI.
            if child.name == "node_modules" and child.parent == root:
                if stat.S_ISLNK(data.st_mode) or not stat.S_ISDIR(data.st_mode):
                    fail(f"{label} root has unsafe node_modules")
                continue
            if stat.S_ISLNK(data.st_mode):
                fail(f"{label} root has symlink {relative}")
            if stat.S_ISDIR(data.st_mode):
                directories.append(child)
            elif stat.S_ISREG(data.st_mode):
                if len(files) >= MAX_FILES:
                    fail(f"{label} root file limit exceeded")
                files[relative] = child
            else:
                fail(f"{label} root has special file {relative}")
    return files


def compare_tar_member(path: Path, archive, member, label):
    try:
        before = path.lstat()
        if not stat.S_ISREG(before.st_mode) or before.st_size != member.size:
            fail(f"{label} differs from integrity-verified tarball")
        source = archive.extractfile(member)
        if source is None:
            fail(f"cannot read {member.name!r} from {label} tarball")
        with path.open("rb") as actual:
            while True:
                expected_chunk, actual_chunk = source.read(65536), actual.read(65536)
                if expected_chunk != actual_chunk:
                    fail(f"{label} differs from integrity-verified tarball at {path.name!r}")
                if not expected_chunk:
                    break
        after = path.lstat()
    except OSError as error:
        fail(f"cannot compare {label} root: {error}")
    if (before.st_size, before.st_mtime_ns, before.st_ino, before.st_dev) != (
        after.st_size,
        after.st_mtime_ns,
        after.st_ino,
        after.st_dev,
    ):
        fail(f"{label} root changed while comparing {path.name!r}")


def verify_package_root(tarball: Path, root: Path, package_name: str, entrypoint=None):
    root = root.absolute()
    label = package_name
    try:
        with tarfile.open(tarball, "r:*") as archive:
            expected, actual = tar_package_files(archive, label), package_disk_files(root, label)
            if set(actual) != set(expected):
                fail(f"{label} root file inventory differs from integrity-verified tarball")
            for relative, member in expected.items():
                compare_tar_member(actual[relative], archive, member, label)
    except (tarfile.TarError, OSError) as error:
        fail(f"cannot inspect {label} tarball: {error}")
    if set(package_disk_files(root, label)) != set(expected):
        fail(f"{label} root changed while comparing")
    manifest = read_regular_file(root / "package.json", f"{label} package manifest", 256 * 1024)
    try:
        parsed = json.loads(manifest)
    except json.JSONDecodeError as error:
        fail(f"{label} package manifest is invalid: {error}")
    if parsed.get("name") != package_name or parsed.get("version") != VERSION:
        fail(f"selected package is not {package_name}@{VERSION}")
    if entrypoint is not None and entrypoint not in expected:
        fail(f"{label} tarball lacks required {entrypoint}")
    return root


def node_resolved_package(root: Path, package_name: str):
    current, package_path = root / "dist", Path(*package_name.split("/"))
    while True:
        candidate = current / "node_modules" / package_path
        if candidate.exists() or candidate.is_symlink():
            return candidate
        if current.parent == current:
            break
        current = current.parent
    fail(f"Node cannot resolve {package_name} from {root / 'dist/index.js'}")


def git(arguments, cwd: Path):
    try:
        return subprocess.run(["git", *arguments], cwd=cwd, check=True, capture_output=True, text=True, timeout=10).stdout.strip()
    except (OSError, subprocess.SubprocessError) as error:
        fail(f"cannot inspect source checkout: {error}")


def source_entries(root: Path):
    entries, directories, file_count = [], [root], 0
    while directories:
        directory = directories.pop()
        for child in directory.iterdir():
            data = child.lstat()
            if stat.S_ISLNK(data.st_mode):
                fail(f"source fingerprint rejects symlink {child}")
            relative = child.relative_to(root).as_posix()
            if len(relative.encode()) > 4096 or len(relative.split("/")) > MAX_DEPTH:
                fail(f"source fingerprint rejects path {child}")
            if len(entries) >= MAX_ENTRIES:
                fail("source fingerprint entry limit exceeded")
            if stat.S_ISDIR(data.st_mode):
                if child.name not in SKIP:
                    entries.append(("d", relative, child)); directories.append(child)
            elif stat.S_ISREG(data.st_mode):
                file_count += 1
                if file_count > MAX_FILES:
                    fail("source fingerprint file limit exceeded")
                entries.append(("f", relative, child))
            else:
                fail(f"source fingerprint rejects special file {child}")
    return sorted(entries, key=lambda item: (item[1].encode(), item[0]))


def fingerprint(source: Path):
    source = source.resolve(); metadata = source.lstat()
    if stat.S_ISLNK(metadata.st_mode):
        fail(f"source fingerprint rejects symlink {source}")
    if stat.S_ISREG(metadata.st_mode):
        tag, entries = b"f", [("f", ".", source)]
    elif stat.S_ISDIR(metadata.st_mode):
        tag, entries = b"d", source_entries(source)
    else:
        fail(f"source fingerprint rejects {source}")
    digest, total = hashlib.sha256(), 0
    digest.update(b"ygg-pi-source-fingerprint\0"); digest.update((1).to_bytes(4, "big")); digest.update(tag)
    for kind, relative, path in entries:
        encoded = relative.encode(); digest.update(kind.encode()); digest.update(len(encoded).to_bytes(8, "big")); digest.update(encoded)
        if kind == "f":
            before = path.lstat()
            if before.st_size > MAX_BYTES - total:
                fail(f"source fingerprint byte limit exceeded at {path}")
            digest.update(before.st_size.to_bytes(8, "big"))
            with path.open("rb") as handle:
                for chunk in iter(lambda: handle.read(65536), b""):
                    digest.update(chunk); total += len(chunk)
            after = path.lstat()
            if (before.st_size, before.st_mtime_ns, before.st_ino, before.st_dev) != (after.st_size, after.st_mtime_ns, after.st_ino, after.st_dev):
                fail(f"source changed while fingerprinting {path}")
    if tag == b"d" and [(a, b) for a, b, _ in entries] != [(a, b) for a, b, _ in source_entries(source)]:
        fail("source tree changed while fingerprinting")
    return digest.hexdigest()


def load_source(node: str, package: Path, checkout: Path, extension: Path, digest: str, env):
    unshare = shutil.which("unshare")
    if not sys.platform.startswith("linux") or not unshare:
        fail("--full requires Linux unshare --net")
    command = [unshare, "--net", "--", node, str(BRIDGE), "--pi-package", str(package), "--extension", str(extension), "--source-fingerprint", digest]
    initialize = {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"workspace": str(checkout), "host": {}, "protocol": {"optional_features": ["runtime_commands"]}}}
    shutdown = {"jsonrpc": "2.0", "id": 2, "method": "shutdown", "params": {}}
    process = None
    try:
        process = subprocess.Popen(
            command, cwd=checkout, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, text=True, encoding="utf-8", bufsize=1,
        )
        assert process.stdin is not None and process.stdout is not None
        process.stdin.write(json.dumps(initialize) + "\n")
        process.stdin.flush()
        response = None
        deadline = time.monotonic() + 20
        while response is None:
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not select.select([process.stdout], [], [], remaining)[0]:
                fail(f"{extension} timed out while loading")
            line = process.stdout.readline()
            if not line:
                stderr = process.stderr.read() if process.stderr is not None else ""
                fail(f"{extension} exited before initialize response; stderr={stderr[-3000:]!r}")
            try:
                message = json.loads(line)
            except json.JSONDecodeError:
                fail(f"{extension} wrote non-JSON protocol stdout")
            if isinstance(message, dict) and message.get("id") == 1 and "method" not in message:
                response = message
            elif isinstance(message, dict) and message.get("method"):
                fail(f"{extension} requested host method {message['method']!r} during hermetic initialization")
        if "error" in response:
            fail(f"{extension} failed loader initialization: {response['error']!r}")
        process.stdin.write(json.dumps(shutdown) + "\n")
        process.stdin.flush()
        process.stdin.close()
        process.stdin = None
        stdout, stderr = process.communicate(timeout=20)
        for line in stdout.splitlines():
            try:
                json.loads(line)
            except json.JSONDecodeError:
                fail(f"{extension} wrote non-JSON protocol stdout")
        if process.returncode:
            fail(f"{extension} exited after initialize with {process.returncode}; stderr={stderr[-3000:]!r}")
    except subprocess.TimeoutExpired:
        fail(f"{extension} timed out while loading")
    except OSError as error:
        fail(f"could not launch {extension}: {error}")
    finally:
        if process is not None and process.poll() is None:
            process.kill()
            process.wait()


def run_full(arguments, report):
    if not arguments.network_isolated:
        fail("--full refuses extension loading without --network-isolated")
    required = [arguments.coding_agent_tarball, arguments.tui_tarball, arguments.pi_package, arguments.source_root]
    if any(value is None for value in required):
        fail("--full requires --coding-agent-tarball, --tui-tarball, --pi-package, and --source-root")
    profile = document(PROFILE_PATH)
    packages = profile["packages"]
    verify_tarball(arguments.coding_agent_tarball, packages["coding_agent"]["npm_integrity"], "coding-agent")
    verify_tarball(arguments.tui_tarball, packages["tui"]["npm_integrity"], "TUI")
    package = verify_package_root(
        arguments.coding_agent_tarball,
        arguments.pi_package,
        packages["coding_agent"]["name"],
        "dist/index.js",
    )
    tui_root = node_resolved_package(package, packages["tui"]["name"])
    verify_package_root(arguments.tui_tarball, tui_root, packages["tui"]["name"])
    checkout = arguments.source_root.resolve()
    if git(["rev-parse", "HEAD"], checkout) != REVISION or git(["status", "--porcelain"], checkout):
        fail("source checkout must be clean and exactly at the pinned revision")
    examples_root, examples = checkout / "examples/extensions", profile["official_extension_examples"]
    if not examples_root.is_dir() or any(not (examples_root / example).exists() for example in examples):
        fail("source checkout lacks the exact official example inventory")
    node = shutil.which("node")
    if node is None:
        fail("--full requires node on PATH")
    failures = []
    with tempfile.TemporaryDirectory(prefix="ygg-pi-conformance-") as directory:
        home = Path(directory); (home / "tmp").mkdir()
        env = {"HOME": str(home), "TMPDIR": str(home / "tmp"), "PATH": os.environ.get("PATH", ""), "LANG": "C.UTF-8", "LC_ALL": "C.UTF-8", "NO_PROXY": "*", "no_proxy": "*", "HTTP_PROXY": "http://127.0.0.1:9", "HTTPS_PROXY": "http://127.0.0.1:9", "http_proxy": "http://127.0.0.1:9", "https_proxy": "http://127.0.0.1:9"}
        for example in examples:
            source = examples_root / example
            try:
                load_source(node, package, checkout, source, fingerprint(source), env)
            except GateFailure as error:
                failures.append(str(error))
    if failures:
        suffix = "" if len(failures) <= 8 else f"\n... and {len(failures) - 8} more"
        fail(f"{len(failures)} of 78 exact-source examples failed:\n" + "\n".join(failures[:8]) + suffix)
    report.update({"real_runtime": "integrity_verified_local_full_run", "real_examples_loaded": 78, "network_isolation": "linux_unshare_net"})


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="run checked-in gates (default)")
    parser.add_argument("--full", action="store_true", help="load all exact-source examples with verified local artifacts")
    parser.add_argument("--network-isolated", action="store_true", help="allow --full to use Linux unshare --net")
    parser.add_argument("--coding-agent-tarball", type=Path)
    parser.add_argument("--tui-tarball", type=Path)
    parser.add_argument("--pi-package", type=Path)
    parser.add_argument("--source-root", type=Path)
    parser.add_argument("--json", action="store_true")
    arguments = parser.parse_args(argv)
    try:
        report = check_static()
        runtime_inputs = [arguments.coding_agent_tarball, arguments.tui_tarball, arguments.pi_package, arguments.source_root]
        if arguments.full:
            run_full(arguments, report)
        elif any(value is not None for value in runtime_inputs):
            fail("runtime inputs require --full; partial local material cannot prove a pinned runtime")
    except GateFailure as error:
        if arguments.json:
            print(json.dumps({"ok": False, "error": str(error)}, sort_keys=True))
        else:
            print(f"pi-conformance: FAIL: {error}", file=sys.stderr)
        return 1
    if arguments.json:
        print(json.dumps({"ok": True, **report}, sort_keys=True))
    else:
        print(f"pi-conformance: PASS ({report['public_surface_rows']} surfaces, {report['official_examples']} examples, {report['tui_audit_rows']} TUI rows; real runtime: {report['real_runtime']})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
