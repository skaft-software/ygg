#!/usr/bin/env python3
"""Record Docker Hub image availability/digests without pulling task images."""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
BENCHMARK_ROOT = SCRIPT_DIR.parent
sys.path.insert(0, str(SCRIPT_DIR))
from common import (  # noqa: E402
    DATASET_REVISION,
    DOCKER_PLATFORM,
    IMAGE_ARCH,
    IMAGE_NAMESPACE,
    IMAGE_TAG,
    ensure_dataset,
    image_reference,
    load_rows,
    write_json,
)


def request_json(url: str, headers: dict[str, str] | None = None) -> tuple[int, dict | None, str | None]:
    try:
        request = urllib.request.Request(url, headers=headers or {"User-Agent": "ygg-swebench-lite-runner/1"})
        with urllib.request.urlopen(request, timeout=20) as response:
            body = response.read(1024 * 1024)
            return response.status, json.loads(body), None
    except urllib.error.HTTPError as error:
        return error.code, None, str(error)
    except Exception as error:
        return 0, None, f"{type(error).__name__}: {error}"


def inspect_image(instance_id: str, arch: str, tag: str) -> dict:
    repository = image_reference(instance_id, arch, tag).rsplit(":", 1)[0]
    token_url = "https://auth.docker.io/token?" + urllib.parse.urlencode(
        {"service": "registry.docker.io", "scope": f"repository:{repository}:pull"}
    )
    token_status, token_value, token_error = request_json(token_url)
    token = token_value.get("token") if isinstance(token_value, dict) else None
    if not token:
        return {"instance_id": instance_id, "reference": f"{repository}:{tag}", "status": "token_error", "http_status": token_status, "error": token_error}
    manifest_url = f"https://registry-1.docker.io/v2/{repository}/manifests/{tag}"
    status, _value, error = request_json(
        manifest_url,
        {
            "User-Agent": "ygg-swebench-lite-runner/1",
            "Authorization": f"Bearer {token}",
            "Accept": "application/vnd.docker.distribution.manifest.v2+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.list.v2+json",
        },
    )
    if status == 200:
        # A HEAD would expose the digest but some registries reject HEAD.  A
        # second GET is avoided here; Docker itself records the digest after a
        # pull and the run manifest retains it per instance.
        return {"instance_id": instance_id, "reference": f"{repository}:{tag}", "status": "available", "http_status": status}
    if status == 429:
        return {"instance_id": instance_id, "reference": f"{repository}:{tag}", "status": "rate_limited_unknown", "http_status": status, "error": error}
    return {"instance_id": instance_id, "reference": f"{repository}:{tag}", "status": "unavailable_or_private", "http_status": status, "error": error}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--parquet", type=Path, default=BENCHMARK_ROOT / "data/lite.parquet")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--arch", default=IMAGE_ARCH, choices=["x86_64", "arm64"])
    parser.add_argument("--tag", default=IMAGE_TAG)
    parser.add_argument("--workers", type=int, default=4)
    args = parser.parse_args()
    if args.workers < 1:
        parser.error("--workers must be positive")
    rows = load_rows(ensure_dataset(args.parquet.resolve()))
    ids = [row["instance_id"] for row in rows]
    started = time.time()
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.workers) as executor:
        records = list(executor.map(lambda instance_id: inspect_image(instance_id, args.arch, args.tag), ids))
    counts: dict[str, int] = {}
    for record in records:
        counts[record["status"]] = counts.get(record["status"], 0) + 1
    write_json(
        args.output,
        {
            "schema_version": "swebench-live-image-preflight-v1",
            "dataset_revision": DATASET_REVISION,
            "namespace": IMAGE_NAMESPACE,
            "arch": args.arch,
            "platform": DOCKER_PLATFORM,
            "tag": args.tag,
            "started_unix": started,
            "finished_unix": time.time(),
            "counts": counts,
            "records": records,
            "interpretation": "rate_limited_unknown is not an invalid-instance decision; official gold validation is authoritative",
        },
    )
    print(json.dumps(counts, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
