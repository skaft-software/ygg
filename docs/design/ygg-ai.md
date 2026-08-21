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
- emit `parallel_tool_calls: false` explicitly, including when there are no
  tools;
- include `reasoning.context: "all_turns"` alongside any advertised effort; and
- remove only `detail` from `input_image` parts in messages and function/custom
  tool outputs while retaining every other opaque field.

Public/non-Lite compact routes retain their narrower schema. Lite is never
inferred from a model name, endpoint label, or authentication plan.

## Stream contract

A successful guarded stream has exactly one `Started`, balanced start/delta/end events for every indexed part, at most one usage event, and exactly one terminal `Finished`. Premature EOF, events after finish, malformed tool arguments, and unbalanced parts are errors.

The response builder enforces absolute limits before appending:

- 16 MiB per tool argument object;
- 64 MiB aggregate text, reasoning, tool identifiers/arguments, and media;
- 100,000 events;
- 1,024 indexed parts;
- protocol SSE event/body and timeout limits in the transport layer.

Observed indices use a hash set and are sorted only during final assembly, keeping hostile many-part processing near-linear.

## Validation and compatibility

Strict mode rejects unsupported modalities, reasoning state, tools, malformed schemas, missing/orphan tool results, invalid sampling parameters, and model-limit violations before network I/O. Lossy conversion emits bounded diagnostics and visible placeholders rather than silently changing semantic data. Defensive token-budget and non-native protocol transforms map Ultra to the existing maximum budget; that fallback does not advertise complete Ultra orchestration semantics.

## Authentication and secrets

Endpoints resolve static, environment, or dynamic credentials immediately before requests. Secret values redact `Debug` and `Display`; authorization headers are marked sensitive; redirects are disabled. Transport errors and bounded response snippets are sanitized before crossing the API.

## Deterministic catalog

Normal builds generate display-name aliases only from the checked-in `models/models-dev-names.json` snapshot and never contact the network. Runtime discovery is a coding-product concern and can be disabled with `--offline`/`YGG_OFFLINE=true`.

## Cost accounting

Usage buckets remain disjoint and pricing uses integer picodollar arithmetic. A response carries exact provider usage and optional cost; the agent decides when that completed operation becomes durable session accounting.
