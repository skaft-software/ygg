# Ygg AI design

## Canonical model

`ygg-ai` exposes provider-independent `Request`, `Message`, `AssistantPart`, `Usage`, `Response`, and `StreamEvent` types. Protocol codecs translate these values to and from OpenAI Chat Completions, OpenAI Responses, and Anthropic Messages. Provider DTOs do not cross the crate boundary.

## Capability and reasoning model

`Capabilities` keeps transport and model facts explicit. `responses_lite` selects
a Responses wire contract, while `agent_delegation` records a collaboration
protocol the model advertises; it does not imply that `ygg-ai` owns or can run
an agent team. `ReasoningEffort::Ultra` is ordered above `Max`. OpenAI Responses
requests backed by V2 delegation map Ultra to the model effort `"max"`; the
coding product supplies the delegation half only through its observing
`ygg-subagents` extension. Defensive non-V2 routes that advertise Ultra retain
the provider effort value `"ultra"`.

`ReasoningMode::Pro` remains deserializable only for older callers and persisted
sessions. Protocol validation rejects it in strict mode (or reports
`ignored_reasoning_mode` in lossy mode), and no codec serializes a
`reasoning.mode` field. The product layer must migrate legacy Pro state only
after it has both model metadata and a live, trusted `ygg-subagents` observer.

## Responses Lite

A model with `responses_lite = true` uses the same capability-driven contract for
ordinary Responses, opaque replay, and `POST /responses/compact`, regardless of
endpoint identity or session-affinity format:

- add `x-openai-internal-codex-responses-lite: true`;
- carry function schemas in a developer `additional_tools` input item, wrapped
  by the `functions` namespace, rather than top-level `tools`;
- carry nonempty instructions as a developer message input item rather than
  top-level `instructions`;
- emit `parallel_tool_calls: false` explicitly even when model metadata advertises
  parallel support, as required by the internal Lite route;
- include `reasoning.context: "all_turns"` alongside any advertised effort; and
- remove only `detail` from `input_image` parts in messages and function/custom
  tool outputs while retaining every other opaque field.

Public/non-Lite compact routes retain their narrower schema. Lite is never
inferred from a model name, endpoint label, or authentication plan.

## Stream contract

A successful guarded stream has exactly one `Started`, balanced start/delta/end events for every indexed part, at most one usage event, and exactly one terminal `Finished`. Premature EOF, events after finish, and unbalanced parts are errors. Malformed tool arguments are also errors unless an authoritative max-token terminal proves the output was truncated; that case retains only the call envelope with empty arguments so the agent can pair a non-executing error result and continue safely.

The response builder enforces absolute limits before appending:

- 16 MiB per tool argument object;
- 64 MiB aggregate text, reasoning, tool identifiers/arguments, and media;
- 100,000 events;
- 1,024 indexed parts;
- protocol SSE event/body and timeout limits in the transport layer.

Transport timeouts are phase-specific rather than one short request timer:
connection establishment remains bounded separately, `Endpoint::timeout` covers
request send and response headers, and the client defaults to a fifteen-minute
first-body allowance, a five-minute inter-chunk idle allowance, and a one-hour
overall body deadline. Optional error snippets use tighter two-second idle and
five-second overall ceilings after the HTTP status is known. A preferred
WebSocket falls back to HTTP when connection establishment fails before a
generation frame could have been sent. A bounded body-disconnect retry is also
allowed only before any text, reasoning, media, or tool generation is observed;
post-send header timeouts and every failure after generation are terminal so
provider work is never replayed ambiguously. A provider error reporting WebSocket
connection-lifetime exhaustion retires and disables the poisoned socket before
the error is published, so an immediate safe pre-generation retry uses HTTP.
Once generation has been observed, every automatic retry path remains disabled.
The coding product uses a
fifteen-minute response-header default for built-in and custom routes; custom
providers can override that startup allowance for their own cold-start profile.
All of these are cancellable bounds, not a requirement to wait before cancelling
a stalled request.

Observed indices use a hash set and are sorted only during final assembly, keeping hostile many-part processing near-linear.

## Validation and compatibility

Strict mode rejects unsupported modalities, reasoning state, tools, malformed schemas, missing/orphan tool results, invalid sampling parameters, and model-limit violations before network I/O. Lossy conversion emits bounded diagnostics and visible placeholders rather than silently changing semantic data. Defensive token-budget and non-native protocol transforms map Ultra to the existing maximum budget; that fallback does not advertise complete Ultra orchestration semantics.

## Authentication and secrets

Endpoints resolve static, environment, or dynamic credentials immediately before requests. Secret values redact `Debug` and `Display`; authorization headers are marked sensitive; redirects are disabled. Transport errors and bounded response snippets are sanitized before crossing the API.

## Deterministic catalog

Normal builds generate display-name aliases and trusted provider pricing only
from the checked-in `models/models-dev-names.json` and
`models/models-dev-pricing.json` snapshots. They never contact the network. The
explicit maintainer script `scripts/refresh-models-dev-pricing.py` refreshes both
snapshots together and excludes provider-known dead aliases before writing them.
Pricing is provider-scoped, represented as integer microdollars per million
tokens, and is used as a fallback for discovered built-in routes; explicit
`CatalogConfig` pricing remains authoritative. Runtime discovery is a
coding-product concern and can be disabled with `--offline`/`YGG_OFFLINE=true`.

## Cost accounting

Usage buckets remain disjoint and pricing uses integer picodollar arithmetic. A response carries exact provider usage and optional cost; the agent decides when that completed operation becomes durable session accounting.
