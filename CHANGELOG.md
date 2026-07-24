# Changelog

All notable changes to Ygg are documented here. This project follows Semantic Versioning while pre-1.0 APIs may evolve rapidly.

## 0.1.1-alpha — 2026-07-24

### Added

- Restored the animated, model-tinted braille-tree startup identity. The startup
  card reports the package version, selected model, reasoning configuration, and
  workspace without taking over the terminal background.
- Added entitlement-gated GPT-5.6 Pro reasoning mode for ChatGPT OAuth Pro
  routes, with independent CLI, configuration, session persistence, picker, and
  OpenAI Responses wire support.
- Added `shell_path`, `--shell-path`, and `YGG_SHELL_PATH` for explicit
  Bash-compatible shell selection.
- Added syntax-aware inline Bash command rendering, including distinct command
  names, strings, operators, flags, and arguments.

### Changed

- Renamed the model command tool from `exec` to `bash`. Every command is now
  passed intact to one Bash-compatible shell with `-c`, matching Pi's Unix
  semantics: explicit `shell_path`, `/bin/bash`, `bash` on `PATH`, then `sh`.
  Ygg does not consult `$SHELL`.
- Renamed the primary execution limit to `bash_timeout_secs`,
  `--bash-timeout-secs`, and `YGG_BASH_TIMEOUT_SECS`. The prior configuration,
  CLI, and environment spellings remain compatibility aliases.
- Reworked the transcript hierarchy around one live reasoning indicator,
  in-place activity, bold neutral tool names, restrained metadata, quieter
  collapsed-output hints, consistent spacing, and model-provenance user prompts.
- Tool lifecycle dots now blink in lockstep while work is active, settle dimly,
  use green only for successful Bash commands, and reserve red for failures.
- Completed reasoning disappears by default and remains available through the
  global verbose disclosure mode.
- Ported Pi-compatible terminal input, selection, paste, key-repeat, and overlay
  behavior while preserving native terminal selection and scrollback.
- Long-session rendering now hydrates a bounded tail, caches stable transcript
  rows, and avoids replaying or repainting committed native scrollback.
- Simplified tool output presentation: Bash output remains neutral, file tools
  expose diffs when relevant, and completed tool evidence stays collapsed unless
  explicitly expanded.

### Compatibility and reliability

- Existing sessions containing historical `exec` calls continue to render as
  Bash events; new provider schemas advertise only `bash`.
- Command cancellation and timeouts retain process-group cleanup, bounded
  stdout/stderr capture, live progress, and detached-descendant supervision.
- Added regression coverage for shell selection and Bash expansion, Pro-mode
  entitlement and persistence, synchronized event-dot animation, startup version
  display, reasoning cleanup, command syntax styling, and transcript lifecycle.
- Reduced development-profile codegen units to limit incremental artifact
  accumulation without disabling incremental compilation.

## 0.1.0-alpha — 2026-07-22

### Added

- Interactive TUI, chronological plain mode, and response-only print mode.
- OpenAI Chat, OpenAI Responses, and Anthropic Messages protocol support.
- Local OpenAI-compatible endpoint configuration and cloud/provider discovery.
- Branchable append-only sessions, usage/cost records, checkpoints, resume, and compaction.
- Bounded `read`, `search`, `edit`, `write`, and `exec` tools plus skill discovery/activation tools.
- Complete CLI tool allowlist/deny controls, offline startup, context-file disable switch, workspace trust gate, and `--version`.
- Deterministic checked-in model metadata and Unix containment profile.

### Security and reliability

- Project configuration/resources are ignored unless the workspace is explicitly trusted; project settings cannot relax global authority floors.
- Disabled tools are absent from provider schemas and execution dispatch.
- `--no-edit` disables both mutation tools.
- Descriptor-relative no-follow file operations close parent-symlink replacement races and compare target state immediately before rename.
- File/context/config/credential/session/discovery/provider-stream inputs have hard byte/count limits; special files are rejected.
- Arbitrary process and shell execution use one truthful authority gate.
- Unresolved mutating calls are never replayed after a crash.
- Session appends use interprocess locking, stale-generation detection, private permissions, and synced writes; listing is read-only.
- Cancellation propagates through autonomous compaction and prevents post-cancel summary/usage commits.
- TTY print output neutralizes terminal control sequences.

### Performance and usability

- Session resume hydrates and paints only a bounded tail instead of cloning, parsing, and rendering the entire transcript; older history materializes on demand for PageUp/PageDown, wheel navigation, selection, and semantic copy.
- Session discovery uses bounded lightweight metadata scans, and direct resume-by-id avoids parsing unrelated session bodies.
- TUI redraws emit exact changed rows, clear stale Kitty images, coalesce composer border colour runs, anchor scrolled readers while output arrives, and repeat only editing/navigation keys (never submit, close, or toggle actions).
- Provider model inventories use private, scoped cache-first startup. Built-in inventories refresh in the background; stale custom inventories refresh before catalog construction so the current launch sees server changes while retaining last-known-good models on failure.
- Connection setup and response headers have separate bounds. Custom endpoints have a configurable cold-start header allowance, while non-timeout network loss retries visibly and cancellably up to five times; a full transport timeout is not multiplied automatically.
- Ordinary final answers no longer trigger a hidden second completion-confirmation inference.
- Request sizing and transformation avoid temporary whole-history buffers and redundant context reconstruction during resume and send.
- Codex Responses requests use zstd compression, low text verbosity, and capability-gated parallel tool-call declarations without changing generic OpenAI-compatible routes.
- Streaming parsers use bounded linear scans and aggregate response budgets, including adversarial one-byte compatibility streams, pre-ID tool arguments, and Anthropic signatures.
- Interactive shell commands drain stdout and stderr concurrently under a fixed output budget, enforce the execution timeout, and terminate the complete process group on cancellation.
- Native terminal selection and scrollback are the default again; stable-prefix frame updates avoid redrawing committed history, while application-owned semantic mouse behavior remains available through `--mouse app`.
- Semantic transcript blocks use one consistent breathing row between actions without separating a tool header from its result or diff.
- Custom hlid/llama.cpp discovery reads the active nested `meta.n_ctx` context window instead of falling back to training limits or a generic default.
- Custom endpoint reasoning controls are authoritative: off-only, binary, and level-based metadata produce exactly the corresponding picker choices and wire values.
- Reasoning is collapsed by default into a stable two-line, model-colored status that settles to an elapsed-time label and expands with `Ctrl+O`.
- Every bundled theme retains its authored palette, while the compiled default follows the selected model lab and resets cleanly after theme switches.
- Batched tool results retain independent bounded output allowances so a large early result cannot starve later calls in the same turn.

### Release engineering

- Added root installation/security documentation, MIT and third-party notices, checked-in architecture docs, reproducible release gates, dependency policy, a fuzz target, and complete package metadata.
- Release builds enable ThinLTO, one codegen unit, symbol stripping, and abort-on-panic to reduce startup work and binary/RSS footprint.
- The alpha release target is macOS and Linux; command execution is explicitly Unix-only.
