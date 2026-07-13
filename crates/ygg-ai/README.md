# ygg-ai

Provider-independent inference for Ygg's agent loop.

`ygg-ai` provides one canonical conversation model and one event stream across:

- OpenAI Chat Completions
- OpenAI Responses
- Anthropic Messages

The crate supports tools, reasoning continuation state, images, Chat conversational audio, structured output, strict/lossy cross-protocol conversion, dynamic authentication, integer usage pricing, custom endpoints, cancellation by stream drop, and an embedded offline model catalog.

See [`../../docs/design/ygg-ai.md`](../../docs/design/ygg-ai.md) for the normative design and the crate-level Rust documentation for the public API.
