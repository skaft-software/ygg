# Native Multimodal Input for ygg-coding-agent

**Date:** 2026-07-15
**Status:** Approved design, pre-implementation
**Scope:** `ygg-coding-agent` (composer/TUI), `ygg-agent` (input API). `ygg-ai` is unchanged.

## Problem

The input pipeline is text-only end-to-end: the editor is a `String`, `Event::Paste`
inserts verbatim (`keymap.rs`), and `Agent::prompt/steer/follow_up` take
`impl Into<String>`. Meanwhile `ygg-ai` is already fully multimodal
(`UserPart::Media` with `ImageMedia`/`AudioMedia`, inline bytes, per-model
`input_modalities`). The result: users cannot attach images or audio, large pastes
flood the composer, and the model resorts to workarounds (e.g. vision via AppKit)
instead of receiving media natively.

## Requirements (user-confirmed)

1. **Text paste**: small pastes insert verbatim; large pastes collapse to a
   placeholder chip and are spliced back at submit.
2. **Media channels**: path detection on paste/drag-drop, and `@` file mentions.
   No OS clipboard image readers in v1.
3. **Audio**: attaching audio *files* only. No microphone capture, no clipboard audio.
4. **`@` text files**: insert a plain path reference; the model reads them with its
   own tools. Only media files attach natively.
5. **Capability gating**: block at attach time against the active model's
   `input_modalities`, with an inline notice.
6. **API breadth**: `prompt`, `steer`, and `follow_up` all accept media parts.

## Approach

Chosen: **attachment ledger + typed input parts** (over inline sentinel markers,
and over a side-channel `attach()` API — both rejected: the former smuggles
composer syntax through the agent boundary; the latter is racy with steering and
loses part ordering).

```
keymap.rs         unchanged: Event::Paste → EditAction::Paste(String)
    ↓
InteractiveShell  view.rs + new tui/composer.rs
    classify paste · own AttachmentLedger · chips in String editor
    @ file-mention completion · drain → ComposedInput
    ↓
ygg-agent         prompt/steer/follow_up: impl Into<String> → impl Into<UserInput>
    UserInput { parts: Vec<InputPart> }, InputPart = Text(String) | Media(Media)
    ↓
ygg-ai            zero changes
```

## Composer (`tui/composer.rs` + `view.rs`)

### Attachment ledger

`AttachmentLedger` holds entries `{ id, chip_text, payload }` with
`payload = PastedText(String) | Media { media: ygg_ai::Media, file_name, byte_len }`.
Media **bytes are read at attach time**: validation (exists, readable, size, type)
is immediate and content is frozen even if the file changes before submit.

### Paste classification (on `EditAction::Paste`, in order)

1. Payload trims to a single existing file path — handling `file://` URLs,
   shell-escaped drag-drop paths, and `~` expansion:
   - media extension → attach: ledger entry + chip;
   - non-media file → insert the path as plain text.
2. Payload exceeds **10 lines or 2,048 chars** → pasted-text ledger entry + chip
   `[Pasted text #3: 87 lines]`; full text spliced back at submit.
3. Otherwise → insert verbatim (current behavior).

### `@` file mentions

Typing `@` opens an inline fuzzy file picker (reusing `pickers.rs` machinery),
filtered as the user types, git-ignore-aware. Selecting a media file attaches it
as a chip; any other file inserts `@relative/path` as plain text. Esc dismisses.

### Chips

Chips are plain text in the `String` editor (`[Image #1: screenshot.png]`,
`[Audio #2: memo.wav]`, `[Pasted text #3: 87 lines]`). Cursor movement, wrapping,
and backspace are unchanged. At submit, chips are matched back to ledger entries
by exact rendered string; a hand-mangled chip passes through as literal text and
its ledger entry is dropped. Ledger entries whose chips are gone are dropped.

### Extension → media map

| Extensions | Media |
|---|---|
| png, jpg, jpeg, gif, webp | `ImageMedia` (inline bytes, mime from extension) |
| wav, mp3, flac, opus, aac, m4a | `AudioMedia` (m4a → `AudioFormat::Aac`) |
| anything else | not media — plain text path |

### Capability gate and limits (at attach)

- Check active model `input_modalities`; unsupported media → transient notice
  ("<model> does not accept audio input") and the path is inserted as plain text.
- Size caps: **5 MB images, 25 MB audio**; refusal shows a notice.
- Unreadable/missing file → notice, no attach.

### Submit

`drain_editor()` returns `ComposedInput { parts }`: the editor text is scanned
for chips and split into ordered parts — text segments (pasted-text chips spliced
back in place) and `InputPart::Media` entries. Text–media–text ordering is
preserved. Submit, steer, and follow-up all use the same path.

## Agent boundary (`ygg-agent`)

```rust
pub struct UserInput { pub parts: Vec<InputPart> }
pub enum InputPart { Text(String), Media(ygg_ai::Media) }
impl From<String> for UserInput { /* one Text part */ }
impl From<&str>  for UserInput { /* one Text part */ }
impl UserInput { pub fn text_summary(&self) -> String /* text joined; media as [image]/[audio] */ }
```

`prompt`, `steer`, and `follow_up` widen to `impl Into<UserInput>`; every existing
call site compiles via `From<String>`. Internally parts map 1:1 to
`UserPart::Text`/`UserPart::Media`. The steering queue carries `UserInput`.
Logging/echo paths use `text_summary()`.

This is the second sanctioned change to the frozen ygg-agent v1 boundary,
user-approved: the boundary was text-only by omission, and ygg-ai beneath it was
always multimodal.

## Rendering, steering restore, persistence

- Transcript renders user media parts as themed chip lines
  (`🖼 screenshot.png · 1.2 MB`); text parts as today.
- `queue_steering`/`restore_queued_steering` operate on `ComposedInput`: an
  undelivered steering message restores both its editor text (chips included)
  and its ledger entries.
- Persistence is free: `UserPart::Media` already serializes in sessions. Sessions
  grow by attachment size — accepted for v1. Media parts are compacted like any
  other user content.

## Error handling summary

| Condition | Behavior |
|---|---|
| Missing/unreadable file at attach | notice, no attach |
| Oversized media | notice, no attach |
| Unsupported extension | plain text path |
| Model lacks modality | notice at attach, plain text path |
| Hand-mangled chip | literal text through; ledger entry dropped |
| Chip deleted, editor empty | normal empty-editor handling |

## Testing

- **Composer unit tests**: paste classification (verbatim / collapse / media path /
  non-media path / missing path), chip resolution and orphan dropping,
  capability-gate refusal, size-cap refusal, part ordering.
- **Agent tests**: `prompt(UserInput)` with media lands as `UserPart::Media` in the
  session; `From<String>` compat; steer/follow-up with parts.
- **Existing tests**: keymap paste tests unchanged; interactive-mode
  steer/restore tests updated for `ComposedInput`.

## Out of scope (v1)

- Microphone capture and clipboard image/audio readers.
- Inline image *display* in the terminal (Kitty/iTerm graphics protocols).
- Inlining text-file contents on `@` mention.
- Provider media uploads (`ProviderMediaRef` creation); inline bytes only.
