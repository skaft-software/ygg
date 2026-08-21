#!/usr/bin/env python3
"""Refresh Ygg's checked-in models.dev pricing snapshot.

This is an explicit maintainer operation. Normal builds never run this script
or contact the network.
"""

from __future__ import annotations

import argparse
import json
from decimal import Decimal, ROUND_HALF_UP
from pathlib import Path
from urllib.request import urlopen

API_URL = "https://models.dev/api.json"
DEFAULT_OUTPUT = Path("crates/ygg-ai/models/models-dev-pricing.json")

# Ygg's endpoint ids do not all use models.dev's provider ids. Keep this small
# route-identity mapping here; model names and rates come entirely from the
# downloaded catalog.
PROVIDER_SOURCES = {
    "anthropic": "anthropic",
    "cerebras": "cerebras",
    "deepseek": "deepseek",
    "fireworks": "fireworks-ai",
    "groq": "groq",
    "huggingface": "huggingface",
    "minimax": "minimax",
    "moonshotai": "moonshotai",
    "nvidia": "nvidia",
    "openai": "openai",
    "openrouter": "openrouter",
    "opencode": "opencode",
    "together": "togetherai",
    "xai": "xai",
    "xiaomi": "xiaomi",
}


def microdollars(value: object | None) -> int:
    if value is None:
        return 0
    amount = Decimal(str(value))
    if amount < 0:
        raise ValueError(f"negative models.dev price: {value!r}")
    return int((amount * Decimal(1_000_000)).to_integral_value(rounding=ROUND_HALF_UP))


def snapshot(catalog: dict[str, object]) -> dict[str, dict[str, int | None]]:
    output: dict[str, dict[str, int | None]] = {}
    for ygg_provider, source_provider in sorted(PROVIDER_SOURCES.items()):
        provider = catalog.get(source_provider)
        if not isinstance(provider, dict):
            continue
        models = provider.get("models")
        if not isinstance(models, dict):
            continue
        for model_id, model in sorted(models.items()):
            if not isinstance(model_id, str) or not isinstance(model, dict):
                continue
            cost = model.get("cost")
            if not isinstance(cost, dict) or "input" not in cost or "output" not in cost:
                continue
            output[f"{ygg_provider}/{model_id}".lower()] = {
                "cache_read": microdollars(cost.get("cache_read")),
                "cache_write_5m": microdollars(cost.get("cache_write")),
                "input": microdollars(cost["input"]),
                "output": microdollars(cost["output"]),
                "reasoning": (
                    None
                    if cost.get("reasoning") is None
                    else microdollars(cost["reasoning"])
                ),
            }
    return output


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--source",
        type=Path,
        help="local api.json fixture; downloads models.dev when omitted",
    )
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    args = parser.parse_args()

    if args.source:
        catalog = json.loads(args.source.read_text())
    else:
        with urlopen(API_URL, timeout=30) as response:  # noqa: S310 - explicit maintainer tool
            catalog = json.load(response)
    if not isinstance(catalog, dict):
        raise SystemExit("models.dev api.json must contain an object")

    result = snapshot(catalog)
    if not result:
        raise SystemExit("models.dev api.json contained no Ygg provider pricing")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    print(f"wrote {len(result)} priced routes to {args.output}")


if __name__ == "__main__":
    main()
