# ygg-ai

Provider-independent inference for Ygg's agent loop.

`ygg-ai` provides one canonical conversation model and one event stream across:

- OpenAI Chat Completions
- OpenAI Responses
- Anthropic Messages

The crate supports tools, reasoning continuation state, images, Chat conversational audio, structured output, strict/lossy cross-protocol conversion, dynamic authentication, integer usage pricing, custom endpoints, cancellation by stream drop, and an embedded offline model catalog.

See the [AI design](https://github.com/skaft-software/ygg/blob/main/docs/design/ygg-ai.md) and the crate-level Rust documentation for the public API.
