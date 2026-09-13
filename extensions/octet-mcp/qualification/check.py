"""Offline integrity checks for the source/evidence ledger, not a test runner."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path, PurePosixPath
import re
import subprocess

MAX_FILE_BYTES = 131072
COLLECTION_BOUNDS = {"sources": 64, "matrix": 48, "journeys": 24,
                     "release_gates": 12, "retained_http_defects": 9, "candidate_evidence": 64}
HEX40 = re.compile(r"[0-9a-f]{40}\Z")
HEX64 = re.compile(r"[0-9a-f]{64}\Z")
STATUSES = {"implemented", "partial", "missing", "needs-live-evidence"}
RESULTS = {"not-run", "passed", "failed", "blocked", "partial"}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def _object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, "duplicate JSON object key")
        result[key] = value
    return result


def load(path):
    with Path(path).open("rb") as stream:
        raw = stream.read(MAX_FILE_BYTES + 1)
    require(len(raw) <= MAX_FILE_BYTES, "ledger exceeds file budget")
    return json.loads(raw, object_pairs_hook=_object,
                      parse_constant=lambda _: require(False, "non-finite JSON number"))


def validate(ledger):
    require(isinstance(ledger, dict) and ledger.get("schema") == "octet.mcp.qualification.v1",
            "unknown qualification ledger")
    pending = [(ledger, 0)]
    while pending:
        value, depth = pending.pop()
        require(depth <= 20, "ledger exceeds nesting budget")
        if isinstance(value, str):
            require(len(value.encode("utf-8")) <= 4096, "ledger string exceeds budget")
        elif isinstance(value, dict):
            pending.extend((item, depth + 1) for pair in value.items() for item in pair)
        elif isinstance(value, list):
            pending.extend((item, depth + 1) for item in value)
    ids = {}
    for collection, maximum in COLLECTION_BOUNDS.items():
        rows = ledger.get(collection)
        require(isinstance(rows, list) and len(rows) <= maximum, "invalid ledger collection")
        identifiers = [row.get("id") for row in rows if isinstance(row, dict)]
        require(len(identifiers) == len(rows) and all(isinstance(i, str) and i for i in identifiers),
                "invalid ledger row ID")
        require(len(set(identifiers)) == len(identifiers), "duplicate ledger row ID")
        ids[collection] = set(identifiers)
    require(len(set.union(*ids.values())) == sum(map(len, ids.values())), "cross-collection duplicate ID")
    require({f"MCP-{number:03}" for number in range(1, 33)} <= ids["matrix"],
            "pinned baseline feature was removed")
    require(ids["retained_http_defects"] == {f"D-HTTP-{number:02}" for number in range(1, 10)},
            "retained transport defect ledger was changed")

    baselines = ledger["baselines"]
    for name in ("CODEX", "MCP", "OCTET"):
        require(bool(HEX40.fullmatch(baselines[name]["commit"])), "baseline is not commit-pinned")
    for source in ledger["sources"]:
        baseline = baselines[source["baseline"]]
        path = PurePosixPath(source["path"])
        require(not path.is_absolute() and ".." not in path.parts, "invalid source path")
        require(bool(HEX64.fullmatch(source["sha256"])), "invalid source content digest")
        if source["baseline"] == "RMCP":
            require(source["archive_sha256"] == baseline["archive_sha256"]
                    and bool(HEX64.fullmatch(source["archive_sha256"])), "invalid archive pin")
            expected = f"https://docs.rs/crate/rmcp/{baseline['version']}/source/{path}"
        else:
            require(bool(HEX40.fullmatch(source["git_blob_sha1"])), "invalid source blob pin")
            expected = f"https://github.com/{baseline['repository']}/blob/{baseline['commit']}/{path}"
        require(source["url"] == expected, "source URL is not pinned to its baseline")
        require(bool(source["line_ranges"]), "source has no reviewed range")
        for span in source["line_ranges"]:
            require(isinstance(span, list) and len(span) == 2
                    and all(type(n) is int for n in span) and 1 <= span[0] <= span[1],
                    "invalid reviewed source range")

    def references(values, collection):
        require(isinstance(values, list) and all(v in ids[collection] for v in values),
                "unresolved ledger reference")

    for row in ledger["matrix"]:
        require(row["current_octet"]["status"] in STATUSES and row["qualification"] in RESULTS,
                "invalid feature assessment")
        references(row["current_octet"]["source_evidence"], "sources")
        references(row["journeys"], "journeys")
        references(row["release_gates"], "release_gates")
    for row in ledger["journeys"]:
        require(row["result"] in RESULTS, "invalid journey result")
        if row["result"] == "passed":
            require(bool(row.get("evidence")), "passed journey lacks candidate evidence")
            references(row["evidence"], "candidate_evidence")
    for row in ledger["retained_http_defects"]:
        references([row["source"]], "sources")
        references([row["gate"]], "release_gates")
        references(row["journeys"], "journeys")
        references(row["remediation_evidence"], "candidate_evidence")
    for row in ledger["release_gates"]:
        require(row["status"] in {"open", "closed"}, "invalid release gate status")
        references(row["evidence"], "candidate_evidence")
        require(row["status"] != "closed" or bool(row["evidence"]), "closed gate lacks evidence")
    for row in ledger["candidate_evidence"]:
        require(bool(HEX40.fullmatch(row["candidate_commit"]))
                and bool(HEX64.fullmatch(row["evidence_sha256"])), "candidate evidence is not pinned")
        require(row["result"] in RESULTS, "invalid candidate outcome")
    result = ledger["qualification_result"]
    references(result["executed_journeys"], "journeys")
    if result["status"] == "qualified" or result["production_http"] or result["full_codex_parity"]:
        require(all(gate["status"] == "closed" for gate in ledger["release_gates"]),
                "qualification claim has open release gates")


def verify_local_sources(ledger):
    root = Path(__file__).resolve().parents[3]
    commit = ledger["baselines"]["OCTET"]["commit"]
    for source in ledger["sources"]:
        if source["baseline"] != "OCTET":
            continue
        raw = subprocess.run(["git", "cat-file", "blob", f"{commit}:{source['path']}"],
                             cwd=root, capture_output=True, check=True, timeout=10).stdout
        require(hashlib.sha256(raw).hexdigest() == source["sha256"], "local source digest mismatch")
        blob = hashlib.sha1(f"blob {len(raw)}\0".encode() + raw).hexdigest()
        require(blob == source["git_blob_sha1"], "local source blob mismatch")
        require(all(end <= len(raw.splitlines()) for _, end in source["line_ranges"]),
                "reviewed range exceeds local source")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--verify-local-snapshot", action="store_true")
    parser.add_argument("--require-qualified", action="store_true")
    args = parser.parse_args(argv)
    try:
        ledger = load(Path(__file__).with_name("baseline.json"))
        validate(ledger)
        if args.verify_local_snapshot:
            verify_local_sources(ledger)
        if args.require_qualified:
            require(ledger["qualification_result"]["status"] == "qualified",
                    "candidate is not qualified; inspect the open gates and unrun journeys")
    except (ValueError, KeyError, TypeError, OSError, RecursionError, subprocess.SubprocessError) as error:
        print(f"Qualification check failed: {error}")
        return 1
    print("Ledger integrity valid; this is not runtime or live-provider qualification.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
