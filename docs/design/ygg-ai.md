# Ygg AI design

## Canonical model

`ygg-ai` exposes provider-independent `Request`, `Message`, `AssistantPart`, `Usage`, `Response`, and `StreamEvent` types. Protocol codecs translate these values to and from OpenAI Chat Completions, OpenAI Responses, and Anthropic Messages. Provider DTOs do not cross the crate boundary.

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

Strict mode rejects unsupported modalities, reasoning state, tools, malformed schemas, missing/orphan tool results, invalid sampling parameters, and model-limit violations before network I/O. Lossy conversion emits bounded diagnostics and visible placeholders rather than silently changing semantic data.

## Authentication and secrets

Endpoints resolve static, environment, or dynamic credentials immediately before requests. Secret values redact `Debug` and `Display`; authorization headers are marked sensitive; redirects are disabled. Transport errors and bounded response snippets are sanitized before crossing the API.

## Deterministic catalog

Normal builds generate display-name aliases only from the checked-in `models/models-dev-names.json` snapshot and never contact the network. Runtime discovery is a coding-product concern and can be disabled with `--offline`/`YGG_OFFLINE=true`.

## Cost accounting

Usage buckets remain disjoint and pricing uses integer picodollar arithmetic. A response carries exact provider usage and optional cost; the agent decides when that completed operation becomes durable session accounting.
