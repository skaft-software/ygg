# Ygg SDK and native host

Ygg has two process boundaries for consumers:

- Rust applications embed the public `ygg-agent` and `ygg-ai` crates. The
  `ygg_sdk` library in `ygg-coding-agent` contains the product runtime shared by
  the `ygg` and `ygg-host` binaries.
- Other languages launch `ygg-host` and exchange one UTF-8 JSON object per line
  over standard input and output. Standard output is protocol-only; logs and
  diagnostics go to standard error.

The process boundary keeps provider and agent behavior in Rust without exposing
an unstable Rust FFI ABI.

## Install

From a checkout, install both binaries with:

```console
cargo install --locked --path crates/ygg-coding-agent --bins
```

A consumer may use a configured host path, but should always send `hello` and
validate the response before accepting work.

## Protocol invariants

- Protocol version: `1`.
- Encoding: UTF-8 NDJSON, exactly one object per line.
- Maximum request or event frame: 1 MiB, including the terminating newline.
- Requests are handled serially; `hello` reports `max_concurrent_runs: 1`.
- Every request has `protocol_version` and a caller-generated `request_id`.
- Every event echoes `request_id` and has a per-request `seq` beginning at `1`
  and increasing by exactly one.
- Run events also carry `run_id` and echo `session_id` when the request supplied
  one.
- A run terminates with one `final_result` or `protocol_error`. `hello`,
  `models`, and `shutdown` each terminate with the same-named event.
- Malformed, oversized, or unknown request fields produce a bounded
  `protocol_error`; the reader discards the rest of that line before accepting
  another request. Request objects are strict so misspelled authority or
  capability fields cannot silently fall back to defaults. Consumers should
  negotiate advertised request features through `hello` and tolerate additive
  fields in host events.
- An oversized outbound value is replaced by a terminal, bounded
  `protocol_error`; an oversized frame is never written.
- EOF exits cleanly. A successful `shutdown` response is flushed before exit.
- Consumers must drain standard error separately and bound retained diagnostic
  output. Never parse standard error as protocol data.

Request, run, and session IDs are at most 128 bytes and use only ASCII letters,
digits, `-`, `_`, `.`, and `:`. They are identifiers, not paths.

## Handshake

Request:

```json
{"protocol_version":1,"request_id":"probe-1","command":"hello"}
```

Response:

```json
{"protocol_version":1,"request_id":"probe-1","seq":1,"type":"hello","data":{"sdk_version":"0.5.0","protocol_version":1,"max_frame_bytes":1048576,"max_concurrent_runs":1,"commands":["hello","models","run","shutdown"],"features":{"streaming":true,"persistent_sessions":true,"seed_history":true,"typed_media_input":true,"typed_image_input":true,"typed_audio_input":true,"prompt_display_text":true,"inline_models":true,"tools":true,"skills":true,"extensions":true,"process_group_abort":true,"in_band_abort":false}}}
```

Consumers must reject a protocol mismatch, unknown request ID, run/session ID
mismatch, or sequence gap.

## Model inventory

The `models` command returns the resolved catalog:

```json
{"protocol_version":1,"request_id":"models-1","command":"models","offline":true}
```

`offline: true` suppresses live model discovery while constructing the catalog.
Each model reports `input_modalities` (including implied `text`) plus legacy
`vision` and additive `audio` booleans. These values are route-effective: audio
is advertised only when both the model and selected wire protocol support native
audio input. It is not an operating-system network sandbox and does not prevent
a later run from calling its selected provider.

## Run requests

Required run fields are `run_id`, `workspace`, `model`, and `prompt`:

```json
{"protocol_version":1,"request_id":"req-1","command":"run","run_id":"run-1","session_id":"customer-42","workspace":"/srv/workspace","session_dir":"/srv/state/ygg-sessions","model":"gpt-5.6","prompt":"Summarize MEMORY.md","tools":["read"],"allow_file_mutation":false}
```

Useful optional fields include:

| Field | Behavior |
| --- | --- |
| `working_dir` | Invocation directory; it must resolve inside `workspace`. |
| `session_id` | Stable ID used to create `<session_dir>/<id>.jsonl`. |
| `session_dir` | Application-owned session root; defaults to `<workspace>/.ygg/sessions`. |
| `resume_session` | Existing regular JSONL file confined to `session_dir`; final symlinks are rejected. |
| `system_prompt` | Application-owned system context, up to 512 KiB. |
| `prompt_display_text` | Exact caller-visible transcript text when `prompt` contains model-only composition. It may be empty, is capped at 256 KiB, never reaches the model, and must be sent only when `hello.features.prompt_display_text` is true. |
| `history` | Seed `user`/`assistant` messages for a new session only; at most 256 messages and 2 MiB. |
| `tools` | Explicit tool registration allowlist. `[]` disables tools; omission uses Ygg's default registered surface. Registration does not bypass effect admission. |
| `allow_file_mutation` | When false, edit, write, process, and shell authority is removed. True retains those capability gates but does not relax the Controlled effect policy. |
| `allow_external_paths` | Allows caller-supplied session/media paths outside the workspace. Model-controlled file tools remain workspace-only under the fixed Controlled policy. |
| `context_files` | Enables or disables normal trusted workspace context files. |
| `reasoning` | Ygg reasoning level accepted by the selected model. |
| `max_turns` | Run turn limit; omission defaults to 40. |
| `max_cost_microdollars` | Exact integer run-cost ceiling. |
| `media` | Ordered typed inputs: `{"type":"image","path":"…"}` or `{"type":"audio","path":"…"}`. At most 12 items: eight images and four audio clips. |
| `image_paths` | Legacy image-only input. It cannot be combined with `media`. |
| `prompt_paths`, `skill_paths` | Explicit resource roots. |
| `extension_paths`, `enabled_extensions`, `trusted_extensions` | Executable-extension discovery and trust configuration. Protocol v1 reports discovery diagnostics but never starts extension processes. |
| `offline` | Suppresses live model discovery during bootstrap, not provider traffic. |

Prompts are capped at 512 KiB. Caller-visible display text is capped at 256
KiB and rejects control characters other than newline and tab; it is durable
presentation metadata only, while `prompt` remains the exact replayable model
input. PNG/JPEG/GIF/WebP images are capped at 5 MiB each and 20 MiB total.
WAV/MP3/FLAC/Opus/AAC/PCM16 inputs are recognized, with a 20 MiB per-clip and
40 MiB total cap; the selected route must natively support the exact format
(currently OpenAI Chat accepts WAV and MP3). Media is opened through
descriptor-bound, symlink-resistant reads, retained as typed session input in
request order, and sent from the original bytes. Media bytes never cross the
NDJSON frame.

Example with two visual references around one audio reference:

```json
{"protocol_version":1,"request_id":"req-media","command":"run","run_id":"run-media","workspace":"/srv/workspace","model":"gpt-audio-1.5","prompt":"Compare the references and describe the music.","media":[{"type":"image","path":"/srv/workspace/moodboard/one.png"},{"type":"audio","path":"/srv/workspace/music/theme.wav"},{"type":"image","path":"/srv/workspace/moodboard/two.jpg"}]}
```

### Inline providers

An application can define one route without modifying global Ygg configuration:

```json
{"protocol_version":1,"request_id":"req-local","command":"run","run_id":"run-local","workspace":"/srv/workspace","model":"local-model","provider":"local","base_url":"http://127.0.0.1:1234/v1","api_key":"application-owned-secret","custom_headers":{"x-tenant":"example"},"provider_mode":"openai-compatible","context_window_tokens":32768,"max_output_tokens":4096,"input_modalities":["image"],"supports_reasoning":false,"prompt":"Reply with OK","tools":[]}
```

Supported `provider_mode` values are:

- `openai-compatible` (OpenAI Chat Completions),
- `openai-responses`, and
- `anthropic-messages`.

`input_modalities` is an explicit capability assertion for an inline route; it
may contain `image` and `audio`. Do not advertise `audio` merely because a local
or OpenAI-compatible endpoint is configured. Add it only after the exact
provider/model route is known to accept native audio through OpenAI Chat. The
legacy `vision: true` flag remains equivalent to declaring `image`.

Inline base URLs must be absolute HTTP(S) URLs with a host and no userinfo,
query, or fragment. Route/model IDs are SHA-256-derived and isolated from the
built-in catalog. Custom headers are capped at 64 entries and 64 KiB total;
hop-by-hop and routing headers are rejected. Anthropic keys use `x-api-key`;
other inline modes use Bearer authentication.

Without `base_url`, `model` is resolved through Ygg's normal catalog and
credential resolvers. If Ygg has no Codex credential on first use, it can copy a
valid existing Codex CLI credential from `~/.codex/auth.json`, falling back to
the former Hamr store at `~/.hamr/agent/auth.json`. The imported credential is
written to `~/.ygg/credentials/codex.json` with owner-only permissions while
holding Ygg's cross-process refresh lock. Source credentials are never modified
or deleted.

## Event lifecycle

A successful run normally emits:

1. `accepted` with the resolved model, native session path, and registered tools;
2. `started`;
3. zero or more streaming events;
4. `settled`; and
5. exactly one `final_result`.

Streaming events currently include:

- `model_delta` and `output_media`;
- `provider_retry` and `candidate_rejected`;
- `tool_start`, `tool_progress`, and `tool_finish`;
- `model_step` usage/cost accounting;
- `steering_delivered` and `follow_up_delivered`;
- `compaction_start` and `compaction_finish`; and
- `extension_notification`.

`final_result.data` contains `status`, `output`, `error`, `filesChanged`,
`toolCalls`, `steps`, and `sessionFile`. Status is `completed`, `blocked`, or
`error`. Failures before or during a valid run request are represented as an
error `final_result`; malformed protocol requests use `protocol_error`.

## Headless safety and cancellation

`ygg-host` never waits for interactive input. It always uses the Controlled
effect policy: pure and workspace-read calls may run, while workspace mutation
requires an approval that the headless host denies and ambient host/process,
network, delegation, extension, and unknown effects fail closed. Core-tool
confirmation requests are denied and typed input requests are cancelled.
Controlled also prevents executable-extension process startup itself; protocol
v1 exposes no unsafe-host effect opt-in.

Protocol v1 has no in-band abort command. Launch each host in its own process
group and terminate the entire group on timeout or caller cancellation. The
`hello` flags `process_group_abort: true` and `in_band_abort: false` make that
contract explicit. On Unix, `ygg-host` coordinates `HUP`, `INT`, `QUIT`, and
`TERM`: it aborts the active run, gives registered shell process groups a
bounded cleanup window, force-kills survivors, and exits with the conventional
`128 + signal` status. This registration also reaches shell children that
created process groups outside the host's own group.

## Resource and session ownership

The application chooses `workspace` and `session_dir`. Ygg loads its normal
deterministic resource layers (`~/.ygg`, trusted workspace `.ygg`, then explicit
roots) and persists native append-only JSONL sessions. Keep application-domain
memory in the application's own store and inject retrieved context through
`system_prompt`; use Ygg sessions for model/tool continuity.
