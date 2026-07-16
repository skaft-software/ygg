# Native Multimodal Input Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Attach images/audio and large pastes natively from the TUI composer through the agent to the model, per `docs/superpowers/specs/2026-07-15-multimodal-input-design.md`.

**Architecture:** A composer attachment ledger in the TUI turns media-path pastes/drops, `@` file mentions, and large text pastes into placeholder chips inside the existing `String` editor. At submit the chips are resolved into ordered `InputPart`s (text + `ygg_ai::Media`). The ygg-agent boundary widens from `impl Into<String>` to `impl Into<UserInput>` on `prompt`/`complete`/`steer`/`follow_up`; ygg-ai is untouched (`UserPart::Media` already exists).

**Tech Stack:** Rust workspace. Crates touched: `ygg-agent`, `ygg-coding-agent`. New deps for `ygg-coding-agent`: `mime = "0.3"`, `bytes = "1"`, `ignore = "0.4"`. Tests: cargo unit tests + existing wiremock harness in `crates/ygg-agent/tests/agent_run.rs`.

## Global Constraints

- Spec: `docs/superpowers/specs/2026-07-15-multimodal-input-design.md` — reread before each task.
- Thresholds (exact values from spec): large paste = **>10 lines or >2,048 chars**; size caps = **5 MB images, 25 MB audio**.
- Extension map (spec): png/jpg/jpeg/gif/webp → image; wav/mp3/flac/opus/aac/m4a → audio (m4a → `AudioFormat::Aac`). Mime from extension, not content sniffing.
- Capability gate happens **at attach time** against the active model's `input_modalities`; refusal shows a notice and inserts the path as plain text.
- `ygg-ai` must not change. `keymap::translate` stays a pure text translator (the only addition is the Task 8 `CompleteMention` action).
- Workspace lints apply (`#![allow(missing_docs)]` headers exist in TUI files; ygg-agent is documented — new public items in ygg-agent need doc comments).
- Run tests per-crate: `cargo test -p ygg-agent`, `cargo test -p ygg-coding-agent`. Lint: `cargo clippy --workspace --all-targets`.
- Commit after every task (small, conventional-commit messages).

---

### Task 1: `UserInput` / `InputPart` types in ygg-agent

**Files:**
- Create: `crates/ygg-agent/src/input.rs`
- Modify: `crates/ygg-agent/src/lib.rs` (module + re-export)

**Interfaces:**
- Consumes: `ygg_ai::{Media, UserPart}`.
- Produces (used by Tasks 2, 4, 6):
  - `pub struct UserInput { pub parts: Vec<InputPart> }`
  - `pub enum InputPart { Text(String), Media(Media) }`
  - `impl From<String> for UserInput`, `impl From<&str> for UserInput`, `impl From<Vec<InputPart>> for UserInput`
  - `UserInput::text_summary(&self) -> String` — text parts joined with spaces; media parts rendered `[image]` / `[audio]`.
  - `UserInput::into_user_parts(self) -> Vec<UserPart>`

- [ ] **Step 1: Write the failing tests**

Create `crates/ygg-agent/src/input.rs` with the types stubbed only as much as needed for the tests to reference them — actually, for a single-module task write tests and implementation in one file but run tests before implementing bodies is awkward in Rust. Instead: write the full module with `todo!()` bodies plus tests:

```rust
//! Typed user input crossing the agent boundary: ordered text and media parts.

use ygg_ai::{Media, UserPart};

/// A user-authored input: ordered text and media parts.
///
/// This is the type accepted by [`Agent::prompt`](crate::Agent::prompt),
/// [`RunControl::steer`](crate::RunControl::steer) and
/// [`RunControl::follow_up`](crate::RunControl::follow_up). Plain strings
/// convert via `From`, so text-only callers pass `&str`/`String` unchanged.
#[derive(Clone, Debug)]
pub struct UserInput {
    /// Ordered content parts.
    pub parts: Vec<InputPart>,
}

/// One part of a [`UserInput`].
#[derive(Clone, Debug)]
pub enum InputPart {
    /// Plain text.
    Text(String),
    /// Image or audio payload.
    Media(Media),
}

impl From<String> for UserInput {
    fn from(text: String) -> Self {
        Self {
            parts: vec![InputPart::Text(text)],
        }
    }
}

impl From<&str> for UserInput {
    fn from(text: &str) -> Self {
        Self::from(text.to_owned())
    }
}

impl From<Vec<InputPart>> for UserInput {
    fn from(parts: Vec<InputPart>) -> Self {
        Self { parts }
    }
}

impl UserInput {
    /// Human-readable single-line summary: text parts joined, media parts as
    /// `[image]` / `[audio]`. Used for steering-delivery events and logs.
    pub fn text_summary(&self) -> String {
        todo!()
    }

    /// Converts the parts into session-persistable [`UserPart`]s, 1:1.
    pub fn into_user_parts(self) -> Vec<UserPart> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_media() -> Media {
        Media::image_bytes(
            bytes::Bytes::from_static(&[0x89, 0x50]),
            "image/png".parse().unwrap(),
        )
    }

    #[test]
    fn from_string_yields_one_text_part() {
        let input = UserInput::from("hello".to_owned());
        assert!(matches!(&input.parts[..], [InputPart::Text(t)] if t == "hello"));
    }

    #[test]
    fn text_summary_joins_text_and_labels_media() {
        let input = UserInput::from(vec![
            InputPart::Text("look at".into()),
            InputPart::Media(png_media()),
            InputPart::Text("please".into()),
        ]);
        assert_eq!(input.text_summary(), "look at [image] please");
    }

    #[test]
    fn into_user_parts_maps_one_to_one_preserving_order() {
        let input = UserInput::from(vec![
            InputPart::Text("a".into()),
            InputPart::Media(png_media()),
        ]);
        let parts = input.into_user_parts();
        assert_eq!(parts.len(), 2);
        assert!(matches!(&parts[0], UserPart::Text(t) if t == "a"));
        assert!(matches!(&parts[1], UserPart::Media(Media::Image(_))));
    }
}
```

Note: `bytes` is already a transitive dep of ygg-agent via ygg-ai; check `crates/ygg-agent/Cargo.toml` — if `bytes` is not a direct dependency, add `bytes = "1"` to its `[dev-dependencies]`.

Register the module in `crates/ygg-agent/src/lib.rs`:

```rust
pub mod input;
```
and extend the re-exports:
```rust
pub use input::{InputPart, UserInput};
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ygg-agent input::`
Expected: FAIL — panics at `todo!()` (`from_string_yields_one_text_part` passes; the two `todo!()`-hitting tests fail).

- [ ] **Step 3: Implement the bodies**

```rust
    pub fn text_summary(&self) -> String {
        let mut pieces = Vec::with_capacity(self.parts.len());
        for part in &self.parts {
            match part {
                InputPart::Text(text) => pieces.push(text.clone()),
                InputPart::Media(Media::Image(_)) => pieces.push("[image]".into()),
                InputPart::Media(Media::Audio(_)) => pieces.push("[audio]".into()),
            }
        }
        pieces.join(" ")
    }

    pub fn into_user_parts(self) -> Vec<UserPart> {
        self.parts
            .into_iter()
            .map(|part| match part {
                InputPart::Text(text) => UserPart::Text(text),
                InputPart::Media(media) => UserPart::Media(media),
            })
            .collect()
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ygg-agent input::`
Expected: PASS (3 tests)

- [ ] **Step 5: Commit**

```bash
git add crates/ygg-agent/src/input.rs crates/ygg-agent/src/lib.rs crates/ygg-agent/Cargo.toml
git commit -m "feat(ygg-agent): UserInput/InputPart typed user input"
```

---

### Task 2: Widen the agent boundary to `UserInput`

**Files:**
- Modify: `crates/ygg-agent/src/agent.rs` (`user_message` ~line 218, `prompt` ~298, `complete` ~692, `RunControl::steer` ~162, `RunControl::follow_up` ~172, steering queues ~332-343, delivery loop ~359-382, drain sites ~438, ~504, ~584)
- Modify: `crates/ygg-agent/src/events.rs` (`Control` enum ~line 117)
- Test: `crates/ygg-agent/tests/agent_run.rs`

**Interfaces:**
- Consumes: `UserInput`, `InputPart` from Task 1.
- Produces (used by Task 6):
  - `Agent::prompt(&mut self, input: impl Into<UserInput>) -> Result<Run<'_>, AgentError>`
  - `Agent::complete(&mut self, input: impl Into<UserInput>) -> Result<RunOutput, AgentError>`
  - `RunControl::steer(&self, input: impl Into<UserInput>)`, `RunControl::follow_up(&self, input: impl Into<UserInput>)`
  - `Control::Steer(UserInput)`, `Control::FollowUp(UserInput)`
  - `AgentEvent::SteeringDelivered { messages: Vec<String> }` unchanged in shape; each message is now `UserInput::text_summary()`. **Delivery order is FIFO** — consumers must match positionally, not by equality.

- [ ] **Step 1: Write the failing integration test**

Append to `crates/ygg-agent/tests/agent_run.rs` (reuse the existing `harness`, `text_turn`, `assert_single_run_finished`, `collect` helpers already in that file):

```rust
#[tokio::test]
async fn prompt_with_media_persists_media_user_part() {
    use ygg_agent::{InputPart, UserInput};
    use ygg_ai::{Media, Message, UserPart};

    let mut h = harness(vec![text_turn("seen")], 8).await;
    let input = UserInput::from(vec![
        InputPart::Text("what is in this image?".into()),
        InputPart::Media(Media::image_bytes(
            bytes::Bytes::from_static(&[0x89, 0x50, 0x4e, 0x47]),
            "image/png".parse().unwrap(),
        )),
    ]);
    let mut run = h.agent.prompt(input).await.unwrap();
    let events = collect(&mut run).await;
    assert!(matches!(
        assert_single_run_finished(&events),
        ygg_agent::FinishReason::Completed
    ));
    drop(run);

    // The first session entry is the user message with both parts.
    let session = h.agent.session();
    let mut entries = Vec::new();
    let mut cursor = session.head();
    while let Some(id) = cursor {
        let entry = session.entry(&id).unwrap();
        entries.push(entry);
        cursor = entry.parent.clone();
    }
    entries.reverse();
    let ygg_agent::EntryValue::Message(Message::User(user)) = &entries[0].value else {
        panic!("first entry is not a user message");
    };
    assert_eq!(user.content.len(), 2);
    assert!(matches!(&user.content[0], UserPart::Text(t) if t == "what is in this image?"));
    assert!(matches!(&user.content[1], UserPart::Media(Media::Image(_))));
}
```

Adjust the harness field/name usage to whatever the existing `Harness` struct exposes (it has an `agent` field per `build_agent`; follow the surrounding tests' idiom exactly). If `bytes` is not a dev-dependency of ygg-agent yet, it was added in Task 1.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p ygg-agent --test agent_run prompt_with_media_persists_media_user_part`
Expected: FAIL to compile — `prompt` takes `impl Into<String>`; `UserInput` does not convert to `String`.

- [ ] **Step 3: Widen the API**

In `crates/ygg-agent/src/events.rs`, change the `Control` enum (keep doc comments, adjust wording from "text" to "input"):

```rust
pub enum Control {
    /// Inject input into the conversation at the next model-turn boundary of
    /// the active run.
    Steer(crate::input::UserInput),
    /// Queue input for after the current run settles (the model completes a
    /// turn without tool calls). The run then continues with this input
    /// instead of finishing.
    FollowUp(crate::input::UserInput),
    /// Abort the run at the next safe boundary.
    Abort,
}
```
(Preserve the existing `Abort` variant and its doc comment as-is; also update the `SteeringDelivered.messages` doc comment to say the strings are single-line summaries of the delivered inputs, delivered in FIFO order.)

In `crates/ygg-agent/src/agent.rs`:

1. Import `crate::input::UserInput`.
2. `user_message`:
```rust
fn user_message(input: UserInput) -> EntryValue {
    EntryValue::Message(Message::User(UserMessage {
        content: input.into_user_parts(),
    }))
}
```
3. `RunControl::steer` / `follow_up`:
```rust
    pub async fn steer(&self, input: impl Into<UserInput>) -> Result<(), AgentError> {
        self.tx
            .send(Control::Steer(input.into()))
            .await
            .map_err(|_| AgentError::RunEnded)
    }

    pub async fn follow_up(&self, input: impl Into<UserInput>) -> Result<(), AgentError> {
        self.tx
            .send(Control::FollowUp(input.into()))
            .await
            .map_err(|_| AgentError::RunEnded)
    }
```
4. `prompt` signature and first line:
```rust
    pub async fn prompt(&mut self, input: impl Into<UserInput>) -> Result<Run<'_>, AgentError> {
        let first_entry = self.session.append(user_message(input.into()))?;
```
5. `complete` signature: `pub async fn complete(&mut self, input: impl Into<UserInput>) -> Result<RunOutput, AgentError>` (body unchanged — it forwards to `prompt`).
6. Queues: `let mut pending_steer: Vec<UserInput> = Vec::new();` and `let mut followups: VecDeque<UserInput> = VecDeque::new();`. All five drain sites (`Ok(Control::Steer(text)) => pending_steer.push(text)` etc.) keep the same shape — rename the binding from `text` to `input`.
7. Steering delivery loop (~line 359): summaries are computed before the append consumes the input:
```rust
                if !pending_steer.is_empty() {
                    let queued = std::mem::take(&mut pending_steer);
                    let mut delivered = Vec::with_capacity(queued.len());
                    for input in queued {
                        let summary = input.text_summary();
                        if let Err(e) = session.append(user_message(input)) {
                            if !delivered.is_empty() {
                                let ev = AgentEvent::SteeringDelivered {
                                    messages: delivered,
                                };
                                notify_observers(&observers, &ev);
                                yield ev;
                            }
                            break 'run FinishReason::Failed(e.into());
                        }
                        delivered.push(summary);
                    }
                    if !delivered.is_empty() {
                        let ev = AgentEvent::SteeringDelivered {
                            messages: delivered,
                        };
                        notify_observers(&observers, &ev);
                        yield ev;
                    }
                }
```
8. Follow-up consumption (~line 527): `if let Some(input) = followups.pop_front() { if let Err(e) = session.append(user_message(input)) { ... } }`.

- [ ] **Step 4: Run the ygg-agent test suite**

Run: `cargo test -p ygg-agent`
Expected: PASS — new test and all existing tests (existing call sites pass `&str`/`String` and compile via `From`).

- [ ] **Step 5: Verify dependent crates still compile**

Run: `cargo test -p ygg-coding-agent`
Expected: PASS — all coding-agent call sites (`prompt(prompt.clone())`, `control.steer(text)`, …) pass `String` and convert via `From<String>`.

- [ ] **Step 6: Commit**

```bash
git add crates/ygg-agent/src/agent.rs crates/ygg-agent/src/events.rs crates/ygg-agent/tests/agent_run.rs
git commit -m "feat(ygg-agent): accept UserInput parts in prompt/steer/follow_up"
```

---

### Task 3: Composer paste classification (pure functions)

**Files:**
- Create: `crates/ygg-coding-agent/src/tui/composer.rs`
- Modify: `crates/ygg-coding-agent/src/tui/mod.rs` (add `pub mod composer;`)
- Modify: `crates/ygg-coding-agent/Cargo.toml` (add `mime = "0.3"`, `bytes = "1"`)

**Interfaces:**
- Consumes: `ygg_ai::{AudioFormat, Media}` (Task 4 uses `Media`; this task only needs `AudioFormat`).
- Produces (used by Tasks 4, 5, 8):
  - `pub const LARGE_PASTE_LINES: usize = 10;`
  - `pub const LARGE_PASTE_CHARS: usize = 2048;`
  - `pub const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;`
  - `pub const MAX_AUDIO_BYTES: u64 = 25 * 1024 * 1024;`
  - `pub enum MediaKind { Image(mime::Mime), Audio(ygg_ai::AudioFormat) }`
  - `pub fn media_kind_for_path(path: &Path) -> Option<MediaKind>`
  - `pub fn parse_dropped_path(text: &str) -> Option<PathBuf>` — single-line payload → existing file path; handles `file://` URLs, `\ `-escaped spaces, surrounding quotes, and `~/` (via `dirs::home_dir()`).
  - `pub enum PasteKind { Verbatim, LargeText, MediaFile(PathBuf), NonMediaFile(PathBuf) }`
  - `pub fn classify_paste(text: &str) -> PasteKind`

- [ ] **Step 1: Add dependencies**

In `crates/ygg-coding-agent/Cargo.toml` under `[dependencies]` add:

```toml
mime = "0.3"
bytes = "1"
```

- [ ] **Step 2: Write the failing tests**

Create `crates/ygg-coding-agent/src/tui/composer.rs`:

```rust
#![allow(missing_docs)]

//! Composer attachment machinery: paste classification, the attachment
//! ledger with placeholder chips, and submit-time composition into parts.

use std::path::{Path, PathBuf};

use ygg_ai::AudioFormat;

/// A paste larger than either bound collapses to a placeholder chip.
pub const LARGE_PASTE_LINES: usize = 10;
pub const LARGE_PASTE_CHARS: usize = 2048;
/// Attach-time size caps, aligned with common provider limits.
pub const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
pub const MAX_AUDIO_BYTES: u64 = 25 * 1024 * 1024;

/// Media classification of a file path, by extension (no content sniffing).
#[derive(Clone, Debug, PartialEq)]
pub enum MediaKind {
    Image(mime::Mime),
    Audio(AudioFormat),
}

pub fn media_kind_for_path(path: &Path) -> Option<MediaKind> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "png" => Some(MediaKind::Image(mime::IMAGE_PNG)),
        "jpg" | "jpeg" => Some(MediaKind::Image(mime::IMAGE_JPEG)),
        "gif" => Some(MediaKind::Image(mime::IMAGE_GIF)),
        "webp" => Some(MediaKind::Image("image/webp".parse().expect("static mime"))),
        "wav" => Some(MediaKind::Audio(AudioFormat::Wav)),
        "mp3" => Some(MediaKind::Audio(AudioFormat::Mp3)),
        "flac" => Some(MediaKind::Audio(AudioFormat::Flac)),
        "opus" => Some(MediaKind::Audio(AudioFormat::Opus)),
        "aac" | "m4a" => Some(MediaKind::Audio(AudioFormat::Aac)),
        _ => None,
    }
}

/// Interpret a paste payload as a dropped/pasted file path, if it is one.
///
/// Terminals deliver drag-drops as the path text, variously shell-escaped
/// (`My\ File.png`), quoted (`'My File.png'`), or as a `file://` URL.
/// Returns the path only when the file exists.
pub fn parse_dropped_path(text: &str) -> Option<PathBuf> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.contains('\n') {
        return None;
    }
    let unquoted = trimmed
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .or_else(|| trimmed.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
        .unwrap_or(trimmed);
    let unescaped = unquoted.replace("\\ ", " ");
    let expanded = if let Some(rest) = unescaped.strip_prefix("file://") {
        // file://localhost/... and percent-encoding are out of scope; plain
        // file:///path is the shape macOS terminals produce.
        rest.trim_start_matches("localhost").to_owned()
    } else if let Some(rest) = unescaped.strip_prefix("~/") {
        let home = dirs::home_dir()?;
        return existing_file(home.join(rest));
    } else {
        unescaped
    };
    existing_file(PathBuf::from(expanded))
}

fn existing_file(path: PathBuf) -> Option<PathBuf> {
    path.is_file().then_some(path)
}

/// How a paste payload should enter the composer.
#[derive(Clone, Debug, PartialEq)]
pub enum PasteKind {
    Verbatim,
    LargeText,
    MediaFile(PathBuf),
    NonMediaFile(PathBuf),
}

pub fn classify_paste(text: &str) -> PasteKind {
    if let Some(path) = parse_dropped_path(text) {
        return if media_kind_for_path(&path).is_some() {
            PasteKind::MediaFile(path)
        } else {
            PasteKind::NonMediaFile(path)
        };
    }
    if text.lines().count() > LARGE_PASTE_LINES || text.chars().count() > LARGE_PASTE_CHARS {
        return PasteKind::LargeText;
    }
    PasteKind::Verbatim
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn extension_map_matches_the_spec() {
        assert_eq!(
            media_kind_for_path(Path::new("a.PNG")),
            Some(MediaKind::Image(mime::IMAGE_PNG))
        );
        assert_eq!(
            media_kind_for_path(Path::new("b.m4a")),
            Some(MediaKind::Audio(AudioFormat::Aac))
        );
        assert_eq!(media_kind_for_path(Path::new("c.rs")), None);
        assert_eq!(media_kind_for_path(Path::new("noext")), None);
    }

    #[test]
    fn dropped_paths_are_unescaped_unquoted_and_must_exist() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("shot.png");
        let spaced = dir.path().join("my shot.png");
        fs::write(&plain, b"x").unwrap();
        fs::write(&spaced, b"x").unwrap();

        assert_eq!(
            parse_dropped_path(&plain.display().to_string()),
            Some(plain.clone())
        );
        let escaped = spaced.display().to_string().replace(' ', "\\ ");
        assert_eq!(parse_dropped_path(&escaped), Some(spaced.clone()));
        assert_eq!(
            parse_dropped_path(&format!("'{}'", spaced.display())),
            Some(spaced.clone())
        );
        assert_eq!(
            parse_dropped_path(&format!("file://{}", plain.display())),
            Some(plain.clone())
        );
        assert_eq!(
            parse_dropped_path(&dir.path().join("missing.png").display().to_string()),
            None
        );
        assert_eq!(parse_dropped_path("just some words"), None);
    }

    #[test]
    fn paste_classification_follows_spec_order() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("shot.png");
        let source = dir.path().join("main.rs");
        fs::write(&image, b"x").unwrap();
        fs::write(&source, b"x").unwrap();

        assert_eq!(
            classify_paste(&image.display().to_string()),
            PasteKind::MediaFile(image)
        );
        assert_eq!(
            classify_paste(&source.display().to_string()),
            PasteKind::NonMediaFile(source)
        );
        assert_eq!(classify_paste("short text"), PasteKind::Verbatim);
        assert_eq!(classify_paste(&"line\n".repeat(11)), PasteKind::LargeText);
        assert_eq!(classify_paste(&"x".repeat(2049)), PasteKind::LargeText);
        // Exactly at the bounds stays verbatim.
        assert_eq!(classify_paste(&"x".repeat(2048)), PasteKind::Verbatim);
    }
}
```

Add to `crates/ygg-coding-agent/src/tui/mod.rs`: `pub mod composer;`

`tempfile` is already a dev-dependency of ygg-coding-agent.

- [ ] **Step 3: Run tests**

Run: `cargo test -p ygg-coding-agent composer::`
Expected: PASS (this task's functions are written directly with their tests; the compile itself is the failure gate — if any test fails, fix before proceeding).

- [ ] **Step 4: Commit**

```bash
git add crates/ygg-coding-agent/src/tui/composer.rs crates/ygg-coding-agent/src/tui/mod.rs crates/ygg-coding-agent/Cargo.toml Cargo.lock
git commit -m "feat(coding-agent): composer paste classification"
```

---

### Task 4: Attachment ledger, chips, and `compose()`

**Files:**
- Modify: `crates/ygg-coding-agent/src/tui/composer.rs`

**Interfaces:**
- Consumes: Task 3 items; `ygg_agent::{InputPart, UserInput}`; `ygg_ai::{Media, Modality, ModalitySet}`.
- Produces (used by Tasks 5, 6, 8):
  - `pub enum AttachmentPayload { PastedText(String), Media { media: Media, byte_len: u64 } }`
  - `pub struct Attachment { pub id: u64, pub chip: String, pub payload: AttachmentPayload }`
  - `#[derive(Default)] pub struct AttachmentLedger { .. }` with:
    - `pub fn attach_pasted_text(&mut self, text: String) -> String` (returns the chip)
    - `pub fn attach_media(&mut self, path: &Path, modalities: ModalitySet) -> Result<String, AttachError>` (gate → size cap → read bytes → ledger entry → chip)
    - `pub fn restore(&mut self, entries: Vec<Attachment>)`
    - `pub fn is_empty(&self) -> bool`
  - `pub enum AttachError { Unreadable(String), TooLarge { limit_bytes: u64 }, UnsupportedModality { modality: &'static str } }` implementing `std::fmt::Display`
  - `pub struct ComposedInput { pub display_text: String, pub parts: Vec<InputPart>, pub attachments: Vec<Attachment> }` with:
    - `pub fn from_text(text: String) -> Self`
    - `pub fn is_empty(&self) -> bool` (no media parts and all text is whitespace)
    - `pub fn text_for_estimate(&self) -> String` (text parts joined with `\n`)
    - `pub fn into_user_input(self) -> UserInput`
  - `pub fn compose(display_text: String, ledger: &mut AttachmentLedger) -> ComposedInput` — **drains the entire ledger**; entries whose chip appears in the text become parts (pasted text spliced in place); entries whose chip is absent are dropped; chip text without an entry passes through literally.

Chip formats (exact): `[Image #{id}: {file_name}]`, `[Audio #{id}: {file_name}]`, `[Pasted text #{id}: {n} lines]`. One monotonically increasing `id` counter across all kinds, starting at 1, never reset while entries remain.

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module in `composer.rs`:

```rust
    use ygg_ai::{Modality, ModalitySet};

    fn all_modalities() -> ModalitySet {
        ModalitySet::none().with(Modality::Image).with(Modality::Audio)
    }

    #[test]
    fn attach_media_reads_bytes_and_returns_a_chip() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("shot.png");
        fs::write(&image, b"pngbytes").unwrap();

        let mut ledger = AttachmentLedger::default();
        let chip = ledger.attach_media(&image, all_modalities()).unwrap();
        assert_eq!(chip, "[Image #1: shot.png]");
        assert!(!ledger.is_empty());
    }

    #[test]
    fn attach_media_gates_on_modality_and_size() {
        let dir = tempfile::tempdir().unwrap();
        let audio = dir.path().join("memo.wav");
        fs::write(&audio, b"wav").unwrap();

        let image_only = ModalitySet::none().with(Modality::Image);
        let mut ledger = AttachmentLedger::default();
        assert!(matches!(
            ledger.attach_media(&audio, image_only),
            Err(AttachError::UnsupportedModality { modality: "audio" })
        ));

        let big = dir.path().join("big.png");
        fs::write(&big, vec![0u8; (MAX_IMAGE_BYTES + 1) as usize]).unwrap();
        assert!(matches!(
            ledger.attach_media(&big, all_modalities()),
            Err(AttachError::TooLarge { .. })
        ));
        assert!(ledger.is_empty());
    }

    #[test]
    fn compose_splits_text_and_media_preserving_order() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("shot.png");
        fs::write(&image, b"pngbytes").unwrap();

        let mut ledger = AttachmentLedger::default();
        let chip = ledger.attach_media(&image, all_modalities()).unwrap();
        let composed = compose(format!("before {chip} after"), &mut ledger);

        assert_eq!(composed.display_text, format!("before {chip} after"));
        assert_eq!(composed.parts.len(), 3);
        assert!(
            matches!(&composed.parts[0], ygg_agent::InputPart::Text(t) if t.trim() == "before")
        );
        assert!(matches!(
            &composed.parts[1],
            ygg_agent::InputPart::Media(ygg_ai::Media::Image(_))
        ));
        assert!(
            matches!(&composed.parts[2], ygg_agent::InputPart::Text(t) if t.trim() == "after")
        );
        assert!(ledger.is_empty());
    }

    #[test]
    fn compose_splices_pasted_text_in_place() {
        let mut ledger = AttachmentLedger::default();
        let pasted = "l1\nl2\nl3".to_owned();
        let chip = ledger.attach_pasted_text(pasted.clone());
        assert_eq!(chip, "[Pasted text #1: 3 lines]");

        let composed = compose(format!("context: {chip}"), &mut ledger);
        assert_eq!(composed.parts.len(), 1);
        assert!(
            matches!(&composed.parts[0], ygg_agent::InputPart::Text(t) if t == &format!("context: {pasted}"))
        );
    }

    #[test]
    fn compose_drops_orphans_and_keeps_mangled_chips_literal() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("shot.png");
        fs::write(&image, b"pngbytes").unwrap();

        let mut ledger = AttachmentLedger::default();
        let _chip = ledger.attach_media(&image, all_modalities()).unwrap();
        // The user deleted part of the chip: entry is orphaned, text is literal.
        let composed = compose("[Image #1: shot.pn".to_owned(), &mut ledger);
        assert_eq!(composed.parts.len(), 1);
        assert!(
            matches!(&composed.parts[0], ygg_agent::InputPart::Text(t) if t == "[Image #1: shot.pn")
        );
        assert!(ledger.is_empty());
    }

    #[test]
    fn composed_input_emptiness_and_estimate_text() {
        assert!(ComposedInput::from_text("   \n".to_owned()).is_empty());
        assert!(!ComposedInput::from_text("hi".to_owned()).is_empty());

        let mut ledger = AttachmentLedger::default();
        let chip = ledger.attach_pasted_text("body".to_owned());
        let composed = compose(chip, &mut ledger);
        assert_eq!(composed.text_for_estimate(), "body");
        assert!(!composed.is_empty());
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ygg-coding-agent composer::`
Expected: FAIL to compile — `AttachmentLedger`, `compose`, `ComposedInput` undefined.

- [ ] **Step 3: Implement the ledger and compose**

Add to `composer.rs` (below the Task 3 code):

```rust
use std::fs;

use ygg_agent::{InputPart, UserInput};
use ygg_ai::{Media, Modality, ModalitySet};

/// What a chip stands for.
#[derive(Clone, Debug)]
pub enum AttachmentPayload {
    PastedText(String),
    Media { media: Media, byte_len: u64 },
}

/// One chip-backed attachment awaiting submit.
#[derive(Clone, Debug)]
pub struct Attachment {
    pub id: u64,
    pub chip: String,
    pub payload: AttachmentPayload,
}

/// Why an attach was refused. Rendered as a composer notice.
#[derive(Debug)]
pub enum AttachError {
    Unreadable(String),
    TooLarge { limit_bytes: u64 },
    UnsupportedModality { modality: &'static str },
}

impl std::fmt::Display for AttachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable(reason) => write!(f, "cannot read file: {reason}"),
            Self::TooLarge { limit_bytes } => {
                write!(f, "file exceeds the {} MB limit", limit_bytes / (1024 * 1024))
            }
            Self::UnsupportedModality { modality } => {
                write!(f, "the active model does not accept {modality} input")
            }
        }
    }
}

/// Chip-keyed attachments owned by the composer.
#[derive(Clone, Debug, Default)]
pub struct AttachmentLedger {
    next_id: u64,
    entries: Vec<Attachment>,
}

impl AttachmentLedger {
    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Collapse a large paste into a chip; the text returns at compose time.
    pub fn attach_pasted_text(&mut self, text: String) -> String {
        let id = self.next_id();
        let lines = text.lines().count();
        let chip = format!("[Pasted text #{id}: {lines} lines]");
        self.entries.push(Attachment {
            id,
            chip: chip.clone(),
            payload: AttachmentPayload::PastedText(text),
        });
        chip
    }

    /// Gate, cap, read, and record a media file. Returns the chip on success.
    pub fn attach_media(
        &mut self,
        path: &Path,
        modalities: ModalitySet,
    ) -> Result<String, AttachError> {
        let kind = media_kind_for_path(path).ok_or_else(|| {
            AttachError::Unreadable("unsupported media extension".into())
        })?;
        let (label, modality, limit) = match &kind {
            MediaKind::Image(_) => ("Image", Modality::Image, MAX_IMAGE_BYTES),
            MediaKind::Audio(_) => ("Audio", Modality::Audio, MAX_AUDIO_BYTES),
        };
        if !modalities.contains(modality) {
            return Err(AttachError::UnsupportedModality {
                modality: match modality {
                    Modality::Image => "image",
                    Modality::Audio => "audio",
                },
            });
        }
        let metadata =
            fs::metadata(path).map_err(|e| AttachError::Unreadable(e.to_string()))?;
        if metadata.len() > limit {
            return Err(AttachError::TooLarge { limit_bytes: limit });
        }
        let data = fs::read(path).map_err(|e| AttachError::Unreadable(e.to_string()))?;
        let byte_len = data.len() as u64;
        let media = match kind {
            MediaKind::Image(mime) => Media::image_bytes(bytes::Bytes::from(data), mime),
            MediaKind::Audio(format) => Media::audio_bytes(bytes::Bytes::from(data), format),
        };
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let id = self.next_id();
        let chip = format!("[{label} #{id}: {file_name}]");
        self.entries.push(Attachment {
            id,
            chip: chip.clone(),
            payload: AttachmentPayload::Media { media, byte_len },
        });
        Ok(chip)
    }

    /// Put restored steering attachments back (chips re-enter the editor
    /// alongside them). IDs continue from the highest ever issued.
    pub fn restore(&mut self, entries: Vec<Attachment>) {
        self.entries.extend(entries);
    }

    fn take_all(&mut self) -> Vec<Attachment> {
        std::mem::take(&mut self.entries)
    }
}

/// The drained composer content: display text, ordered parts, and the
/// attachments the parts were built from (kept for steering restore).
#[derive(Clone, Debug)]
pub struct ComposedInput {
    pub display_text: String,
    pub parts: Vec<InputPart>,
    pub attachments: Vec<Attachment>,
}

impl ComposedInput {
    pub fn from_text(text: String) -> Self {
        Self {
            parts: vec![InputPart::Text(text.clone())],
            display_text: text,
            attachments: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.parts.iter().all(|part| match part {
            InputPart::Text(text) => text.trim().is_empty(),
            InputPart::Media(_) => false,
        })
    }

    pub fn text_for_estimate(&self) -> String {
        self.parts
            .iter()
            .filter_map(|part| match part {
                InputPart::Text(text) => Some(text.as_str()),
                InputPart::Media(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn into_user_input(self) -> UserInput {
        UserInput::from(self.parts)
    }
}

/// Resolve chips against the ledger, draining it entirely.
pub fn compose(display_text: String, ledger: &mut AttachmentLedger) -> ComposedInput {
    let entries = ledger.take_all();
    // Locate the first occurrence of each entry's chip; unmatched entries drop.
    let mut found: Vec<(usize, &Attachment)> = entries
        .iter()
        .filter_map(|entry| display_text.find(&entry.chip).map(|at| (at, entry)))
        .collect();
    found.sort_by_key(|(at, _)| *at);

    let mut parts: Vec<InputPart> = Vec::new();
    let mut text_run = String::new();
    let mut cursor = 0usize;
    for (at, entry) in &found {
        // Overlapping matches cannot happen: chips contain a unique "#id".
        text_run.push_str(&display_text[cursor..*at]);
        match &entry.payload {
            AttachmentPayload::PastedText(pasted) => text_run.push_str(pasted),
            AttachmentPayload::Media { media, .. } => {
                if !text_run.is_empty() {
                    parts.push(InputPart::Text(std::mem::take(&mut text_run)));
                }
                parts.push(InputPart::Media(media.clone()));
            }
        }
        cursor = at + entry.chip.len();
    }
    text_run.push_str(&display_text[cursor..]);
    if !text_run.is_empty() || parts.is_empty() {
        parts.push(InputPart::Text(text_run));
    }

    let matched: Vec<Attachment> = found.into_iter().map(|(_, e)| e.clone()).collect();
    ComposedInput {
        display_text,
        parts,
        attachments: matched,
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ygg-coding-agent composer::`
Expected: PASS (all Task 3 + Task 4 tests)

- [ ] **Step 5: Commit**

```bash
git add crates/ygg-coding-agent/src/tui/composer.rs
git commit -m "feat(coding-agent): attachment ledger, chips, and compose()"
```

---

### Task 5: Wire the composer into the shell (view.rs)

**Files:**
- Modify: `crates/ygg-coding-agent/src/tui/view.rs`
  - `ShellState` (~line 162): new fields
  - `apply_edit` `EditAction::Paste` arm (~line 1380)
  - `queue_steering` (~1343), `restore_queued_steering` (~1354), `on_agent_event` `SteeringDelivered` arm (~1244), `render_pending_steering` (~928)
  - new `drain_composed`, `set_input_modalities`
- Test: `view.rs` `#[cfg(test)]` module (uses the existing `InteractiveShell::test_shell()` + `debug_snapshot()` pattern)

**Interfaces:**
- Consumes: `composer::{classify_paste, compose, Attachment, AttachmentLedger, ComposedInput, PasteKind}`; `ygg_ai::ModalitySet`.
- Produces (used by Task 6):
  - `InteractiveShell::set_input_modalities(&mut self, modalities: ModalitySet)`
  - `InteractiveShell::drain_composed(&mut self) -> ComposedInput` (drains editor **and** ledger; `drain_editor` remains for slash commands)
  - `InteractiveShell::queue_steering(&mut self, composed: &ComposedInput)` (signature change from `&str`)
  - `SteeringDelivered` handling becomes positional (FIFO `remove(0)`), displaying the queued entry's `display_text`.

State additions to `ShellState`:

```rust
    /// Chip-backed attachments awaiting submit.
    ledger: composer::AttachmentLedger,
    /// Input modalities of the active model; gates attach attempts.
    input_modalities: ModalitySet,
```
and `steering_queue` changes from `Vec<String>` to `Vec<QueuedSteering>`:
```rust
#[derive(Clone, Debug)]
struct QueuedSteering {
    display: String,
    attachments: Vec<composer::Attachment>,
}
```

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module in `view.rs` (follow the existing test style, e.g. the `SteeringDelivered` test at ~line 1860):

```rust
    #[test]
    fn media_path_paste_attaches_a_chip_and_composes_media_parts() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("shot.png");
        std::fs::write(&image, b"png").unwrap();

        let mut shell = InteractiveShell::test_shell();
        shell.set_input_modalities(
            ygg_ai::ModalitySet::none()
                .with(ygg_ai::Modality::Image)
                .with(ygg_ai::Modality::Audio),
        );
        for character in "see ".chars() {
            shell.apply_edit(EditAction::Char(character));
        }
        shell.apply_edit(EditAction::Paste(image.display().to_string()));

        let composed = shell.drain_composed();
        assert_eq!(composed.display_text, "see [Image #1: shot.png]");
        assert!(composed
            .parts
            .iter()
            .any(|p| matches!(p, ygg_agent::InputPart::Media(_))));
    }

    #[test]
    fn media_paste_without_capability_inserts_plain_path_and_notice() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("shot.png");
        std::fs::write(&image, b"png").unwrap();

        let mut shell = InteractiveShell::test_shell();
        shell.set_input_modalities(ygg_ai::ModalitySet::none());
        shell.apply_edit(EditAction::Paste(image.display().to_string()));

        let composed = shell.drain_composed();
        assert_eq!(composed.display_text, image.display().to_string());
        assert!(composed
            .parts
            .iter()
            .all(|p| matches!(p, ygg_agent::InputPart::Text(_))));
        assert!(shell.debug_snapshot().contains("does not accept image input"));
    }

    #[test]
    fn large_paste_collapses_to_chip_and_splices_back_on_drain() {
        let mut shell = InteractiveShell::test_shell();
        let large = "line\n".repeat(20);
        shell.apply_edit(EditAction::Paste(large.clone()));

        let state_text = shell.pending();
        assert!(state_text.starts_with("[Pasted text #1: 20 lines]"));

        let composed = shell.drain_composed();
        let text = composed.text_for_estimate();
        assert_eq!(text.matches("line").count(), 20);
    }

    #[test]
    fn small_paste_still_inserts_verbatim() {
        let mut shell = InteractiveShell::test_shell();
        shell.apply_edit(EditAction::Paste("first\nsecond".into()));
        assert_eq!(shell.pending(), "first\nsecond");
    }

    #[test]
    fn steering_restore_returns_chips_and_attachments() {
        let mut shell = InteractiveShell::test_shell();
        let large = "line\n".repeat(20);
        shell.apply_edit(EditAction::Paste(large));
        let composed = shell.drain_composed();
        shell.queue_steering(&composed);

        shell.restore_queued_steering();
        assert!(shell.pending().contains("[Pasted text #1: 20 lines]"));
        // The ledger got its entry back: draining resolves the chip again.
        let recomposed = shell.drain_composed();
        assert_eq!(recomposed.text_for_estimate().matches("line").count(), 20);
    }

    #[test]
    fn steering_delivery_is_positional_fifo() {
        let mut shell = InteractiveShell::test_shell();
        shell.apply_edit(EditAction::Paste("go left".into()));
        let first = shell.drain_composed();
        shell.queue_steering(&first);
        shell.apply_edit(EditAction::Paste("go right".into()));
        let second = shell.drain_composed();
        shell.queue_steering(&second);

        shell.on_agent_event(&AgentEvent::SteeringDelivered {
            messages: vec!["go left".into()],
        });
        let snapshot = shell.debug_snapshot();
        assert!(snapshot.contains("go left"));
        // Second message still pending.
        assert!(render_shell(&shell.state.borrow(), 120)
            .iter()
            .any(|line| line.contains("go right")));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ygg-coding-agent --lib view`
Expected: FAIL to compile — `set_input_modalities`, `drain_composed` undefined; `queue_steering` takes `&str`.

- [ ] **Step 3: Implement the shell wiring**

1. Imports at the top of `view.rs`: `use crate::tui::composer::{self, ComposedInput};` and `use ygg_ai::ModalitySet;`.
2. Add the two `ShellState` fields and the `QueuedSteering` struct; change `steering_queue: Vec<QueuedSteering>`.
3. Replace the `EditAction::Paste` arm in `apply_edit`:

```rust
            EditAction::Paste(text) => {
                let pasted = normalize_paste(&text);
                match composer::classify_paste(&pasted) {
                    composer::PasteKind::Verbatim => {
                        let cursor = state.editor_cursor;
                        state.editor.insert_str(cursor, &pasted);
                        state.editor_cursor = cursor + pasted.len();
                    }
                    composer::PasteKind::LargeText => {
                        let chip = state.ledger.attach_pasted_text(pasted);
                        let cursor = state.editor_cursor;
                        state.editor.insert_str(cursor, &chip);
                        state.editor_cursor = cursor + chip.len();
                    }
                    composer::PasteKind::MediaFile(path) => {
                        let modalities = state.input_modalities;
                        match state.ledger.attach_media(&path, modalities) {
                            Ok(chip) => {
                                let cursor = state.editor_cursor;
                                state.editor.insert_str(cursor, &chip);
                                state.editor_cursor = cursor + chip.len();
                            }
                            Err(error) => {
                                state.push_block(TranscriptBlock::Notice(error.to_string()));
                                let cursor = state.editor_cursor;
                                state.editor.insert_str(cursor, &pasted);
                                state.editor_cursor = cursor + pasted.len();
                            }
                        }
                    }
                    composer::PasteKind::NonMediaFile(_) => {
                        let cursor = state.editor_cursor;
                        state.editor.insert_str(cursor, &pasted);
                        state.editor_cursor = cursor + pasted.len();
                    }
                }
            }
```
(The `MediaFile` error arm covers the capability gate, size cap, and unreadable file uniformly: notice + plain path text, per the spec.)

4. New methods on `InteractiveShell`:

```rust
    pub fn set_input_modalities(&mut self, modalities: ModalitySet) {
        self.state.borrow_mut().input_modalities = modalities;
    }

    /// Drain the editor and resolve chips into ordered parts.
    pub fn drain_composed(&mut self) -> ComposedInput {
        let mut state = self.state.borrow_mut();
        state.editor_cursor = 0;
        let text = std::mem::take(&mut state.editor);
        composer::compose(text, &mut state.ledger)
    }
```

5. `queue_steering` and `restore_queued_steering`:

```rust
    pub fn queue_steering(&mut self, composed: &ComposedInput) {
        if composed.is_empty() {
            return;
        }
        let mut state = self.state.borrow_mut();
        state.steering_queue.push(QueuedSteering {
            display: composed.display_text.clone(),
            attachments: composed.attachments.clone(),
        });
        state.scroll_from_bottom = 0;
    }

    pub fn restore_queued_steering(&mut self) {
        let mut state = self.state.borrow_mut();
        if state.steering_queue.is_empty() {
            return;
        }
        let queued = std::mem::take(&mut state.steering_queue);
        let mut attachments = Vec::new();
        let mut displays = Vec::with_capacity(queued.len());
        for entry in queued {
            displays.push(entry.display);
            attachments.extend(entry.attachments);
        }
        state.ledger.restore(attachments);
        let restored = displays.join("\n\n");
        let current = std::mem::take(&mut state.editor);
        state.editor = if current.trim().is_empty() {
            restored
        } else if restored.is_empty() {
            current
        } else {
            format!("{restored}\n\n{current}")
        };
        state.editor_cursor = state.editor.len();
    }
```
(Note: this also fixes the pre-existing bug at line 1366 where a literal `\\n\\n` was inserted instead of newlines.)

6. `SteeringDelivered` arm becomes positional:

```rust
            AgentEvent::SteeringDelivered { messages } => {
                state.close_streaming_blocks();
                for message in messages {
                    let display = if state.steering_queue.is_empty() {
                        message.clone()
                    } else {
                        state.steering_queue.remove(0).display
                    };
                    state.push_block(TranscriptBlock::User(display));
                }
                state.scroll_from_bottom = 0;
            }
```

7. `render_pending_steering` (~line 928): iterate `state.steering_queue` reading `.display` instead of the string itself.
8. Fix the existing `steering delivery` test (~line 1860) — it queues via the old `&str` signature; update it to build a `ComposedInput::from_text` and use the new signature, with FIFO expectations.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ygg-coding-agent --lib`
Expected: PASS — new tests plus all existing view tests (some updated).

- [ ] **Step 5: Commit**

```bash
git add crates/ygg-coding-agent/src/tui/view.rs
git commit -m "feat(coding-agent): composer-backed paste, chips, and steering in the shell"
```

---

### Task 6: Plumb `ComposedInput` through interactive mode

**Files:**
- Modify: `crates/ygg-coding-agent/src/modes/interactive.rs`
  - `Idle` enum (~line 87), `wait_for_prompt` (~156), `ControlIntent` (~34), steer/follow-up arms (~441-456), submit path (~759-780), `update_status` (~536), startup prompt (~745)

**Interfaces:**
- Consumes: `ComposedInput` (Task 4), shell methods (Task 5), `UserInput` agent API (Task 2).
- Produces: no new public items; behavior only.

- [ ] **Step 1: Make the changes** (this task is glue; the test gate is the existing interactive test suite plus one new steering test)

1. `use crate::tui::composer::ComposedInput;`
2. `enum Idle { Submit(ComposedInput), Command(String), Quit }`
3. `wait_for_prompt`: `InputAction::Submit(_) => return Ok(Idle::Submit(shell.drain_composed()))` (Command keeps `drain_editor()`).
4. `ControlIntent`:
```rust
enum ControlIntent {
    Steer(ygg_agent::UserInput),
    FollowUp(ygg_agent::UserInput),
}
```
(`Debug` derive stays; `UserInput` derives Debug.)
5. Steer/follow-up arms in `drive_active_run`:
```rust
                    InputAction::Steer(_) => {
                        let composed = shell.drain_composed();
                        if !composed.is_empty() {
                            shell.queue_steering(&composed);
                            intents.push_back(ControlIntent::Steer(composed.into_user_input()));
                        }
                        shell.render();
                    }
                    InputAction::FollowUp(_) => {
                        let composed = shell.drain_composed();
                        if !composed.is_empty() {
                            shell.on_prompt_submitted(&composed.display_text);
                            intents.push_back(ControlIntent::FollowUp(composed.into_user_input()));
                        }
                        shell.render();
                    }
```
6. Submit path in `run_interactive`:
```rust
            Idle::Submit(composed) => {
                prepare_prompt(&mut shell);
                let estimate_text = composed.text_for_estimate();
                match ensure_capacity_before_prompt(&mut app, &estimate_text).await? {
                    CapacityDecision::Proceed(outcome) => {
                        report_compaction(&mut shell, &outcome);
                        shell.set_context_estimate(
                            estimate_next_request_tokens(&app, &estimate_text),
                            hard_input_budget(&app.model),
                        );
                    }
                    CapacityDecision::Exceeded { estimate, budget } => {
                        // (existing error message and continue, unchanged)
                    }
                }
                let display = composed.display_text.clone();
                let mut run = app.agent.prompt(composed.into_user_input()).await?;
                shell.on_prompt_submitted(&display);
```
(Note: media bytes are not counted by the token estimate in v1 — text parts only; this matches the spec's accepted limitations.)
7. Startup prompt (~line 745): `Some(prompt) if !prompt.is_empty() => Idle::Submit(ComposedInput::from_text(prompt))`.
8. `update_status`: add
```rust
    shell.set_input_modalities(app.model.spec.capabilities.input_modalities);
```
(`update_status` runs at startup and after every `transition`, so model switches re-gate attachments.)
9. Update the existing tests at the bottom of `interactive.rs` that exercise steer/queue (e.g. the test around line 964 that types "steer first") for the new signatures.

- [ ] **Step 2: Add a steering round-trip test**

In the `tests` module of `interactive.rs`, extend or mirror the existing steer test (~line 964) to assert that a steer composed with an attachment reaches the session with a media part. If the existing test harness there drives a real `Agent` (see `agent.prompt("initial")` at ~line 998), add:

```rust
    // In the existing steer-delivery test flow, after the run completes:
    // build a steer with media via the shell, deliver it, then assert the
    // session's user entries contain a UserPart::Media.
```
Concretely: write the image file with `tempfile`, `shell.apply_edit(EditAction::Paste(image_path_string))` (after `shell.set_input_modalities(...)` with image enabled), `let composed = shell.drain_composed()`, send `control.steer(composed.into_user_input())`, and after the run finishes walk `agent.session()` entries asserting `matches!(part, UserPart::Media(_))` on the steering user message. Follow the surrounding test's structure for driving the run to completion.

- [ ] **Step 3: Run the full crate test suite**

Run: `cargo test -p ygg-coding-agent`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add crates/ygg-coding-agent/src/modes/interactive.rs
git commit -m "feat(coding-agent): submit, steer, and follow up with composed multimodal input"
```

---

### Task 7: Render media parts in the hydrated transcript

**Files:**
- Modify: `crates/ygg-coding-agent/src/hydrate.rs` (`push_message` ~line 51-69)
- Test: same file, `tests` module

**Interfaces:**
- Consumes: `ygg_ai::{ImageSource, AudioPayload, Media}`.
- Produces: hydrated `TranscriptItem::User` strings include `[image image/png · 1.2 MB]` / `[audio wav · 3.4 MB]` markers where media parts occur.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn user_media_parts_render_as_markers() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("session.jsonl")).unwrap();
        session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![
                    UserPart::Text("look: ".into()),
                    UserPart::Media(ygg_ai::Media::image_bytes(
                        bytes::Bytes::from(vec![0u8; 1024]),
                        "image/png".parse().unwrap(),
                    )),
                ],
            })))
            .unwrap();

        let items = hydrate_transcript(&session).unwrap();
        assert!(
            matches!(&items[0], TranscriptItem::User(text) if text == "look: [image image/png · 1.0 KB]")
        );
    }
```
Add `bytes = "1"` to ygg-coding-agent `[dev-dependencies]` only if the Task 3 dependency addition was placed under `[dependencies]` (then it's already available).

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p ygg-coding-agent hydrate`
Expected: FAIL — media parts are skipped (`UserPart::Media(_) => {}`), text is `"look: "` so the match fails.

- [ ] **Step 3: Implement**

In `hydrate.rs`:

```rust
fn human_bytes(len: u64) -> String {
    if len >= 1024 * 1024 {
        format!("{:.1} MB", len as f64 / (1024.0 * 1024.0))
    } else if len >= 1024 {
        format!("{:.1} KB", len as f64 / 1024.0)
    } else {
        format!("{len} B")
    }
}

fn media_marker(media: &ygg_ai::Media) -> String {
    match media {
        ygg_ai::Media::Image(image) => {
            let mime = image
                .media_type
                .as_ref()
                .map(|m| m.to_string())
                .unwrap_or_else(|| "image".into());
            match &image.source {
                ygg_ai::ImageSource::Inline(data) => {
                    format!("[image {mime} · {}]", human_bytes(data.len() as u64))
                }
                _ => format!("[image {mime}]"),
            }
        }
        ygg_ai::Media::Audio(audio) => {
            let format = format!("{:?}", audio.format).to_lowercase();
            match &audio.payload {
                ygg_ai::AudioPayload::Inline(data) => {
                    format!("[audio {format} · {}]", human_bytes(data.len() as u64))
                }
                _ => format!("[audio {format}]"),
            }
        }
    }
}
```
and replace `UserPart::Media(_) => {}` with `UserPart::Media(media) => text.push_str(&media_marker(media)),`.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p ygg-coding-agent hydrate`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/ygg-coding-agent/src/hydrate.rs crates/ygg-coding-agent/Cargo.toml
git commit -m "feat(coding-agent): render user media parts in hydrated transcripts"
```

---

### Task 8: `@` file mentions with fuzzy completion

**Files:**
- Modify: `crates/ygg-coding-agent/Cargo.toml` (add `ignore = "0.4"`)
- Modify: `crates/ygg-coding-agent/src/tui/composer.rs` (file index + mention matching)
- Modify: `crates/ygg-coding-agent/src/tui/keymap.rs` (`CompleteMention` action)
- Modify: `crates/ygg-coding-agent/src/tui/view.rs` (mention state, suggestions rendering, `complete_mention`)
- Modify: `crates/ygg-coding-agent/src/modes/interactive.rs` (wire `CompleteMention` in both loops; `shell.set_workspace(...)` in `update_status`)

**Interfaces:**
- Consumes: Tasks 3-5 items.
- Produces:
  - composer: `pub fn workspace_files(root: &Path, cap: usize) -> Vec<String>` (relative paths, gitignore-aware via `ignore::WalkBuilder`, files only, sorted, capped), `pub fn active_mention(text: &str) -> Option<&str>` (query after `@` when the text's final whitespace-delimited token starts with `@` and is not the whole-command `/`-prefixed text), `pub fn mention_matches<'a>(files: &'a [String], query: &str, limit: usize) -> Vec<&'a str>` (case-insensitive substring on the relative path; rank by match position, then path length).
  - keymap: `InputAction::CompleteMention` — emitted for Tab (no modifiers) when `active_mention(editor_text).is_some()` and the text does not start with `/`.
  - view: `InteractiveShell::set_workspace(&mut self, root: PathBuf)`, `InteractiveShell::complete_mention(&mut self)` — top match: media extension → attach (gate/cap/notice, chip replaces the `@token`); otherwise replace the `@token` with `@{relative/path} `; mention suggestions rendered under the prompt box while a mention is active (same pattern as `render_slash_suggestions` ~line 891).

- [ ] **Step 1: Write failing composer tests**

Append to `composer.rs` tests:

```rust
    #[test]
    fn workspace_files_lists_relative_paths_and_respects_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/main.rs"), b"x").unwrap();
        fs::write(dir.path().join("shot.png"), b"x").unwrap();
        fs::write(dir.path().join(".gitignore"), b"ignored.txt\n").unwrap();
        fs::write(dir.path().join("ignored.txt"), b"x").unwrap();

        let files = workspace_files(dir.path(), 100);
        assert!(files.contains(&"src/main.rs".to_owned()));
        assert!(files.contains(&"shot.png".to_owned()));
        assert!(!files.iter().any(|f| f.contains("ignored")));
    }

    #[test]
    fn active_mention_is_the_trailing_at_token() {
        assert_eq!(active_mention("look at @sr"), Some("sr"));
        assert_eq!(active_mention("@"), Some(""));
        assert_eq!(active_mention("email a@b.com"), None); // '@' not at token start
        assert_eq!(active_mention("no mention"), None);
        assert_eq!(active_mention("ends with space @x "), None);
    }

    #[test]
    fn mention_matches_rank_by_position_then_length() {
        let files = vec![
            "src/main.rs".to_owned(),
            "docs/main-notes.md".to_owned(),
            "main.rs".to_owned(),
        ];
        let matches = mention_matches(&files, "main", 10);
        assert_eq!(matches[0], "main.rs"); // position 0, shortest
        assert!(matches.contains(&"src/main.rs"));
        assert!(mention_matches(&files, "zzz", 10).is_empty());
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p ygg-coding-agent composer::`
Expected: FAIL to compile — the three functions are undefined.

- [ ] **Step 3: Implement the composer functions**

Add `ignore = "0.4"` to `[dependencies]`. Then in `composer.rs`:

```rust
/// List workspace files (relative, sorted, gitignore-aware), capped.
pub fn workspace_files(root: &Path, cap: usize) -> Vec<String> {
    let mut files = Vec::new();
    // require_git(false): honor .gitignore files even when the workspace is
    // not (yet) a git repository — also what the unit test's tempdir needs.
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .require_git(false)
        .build();
    for entry in walker.flatten() {
        if files.len() >= cap {
            break;
        }
        if entry.file_type().is_some_and(|t| t.is_file()) {
            if let Ok(relative) = entry.path().strip_prefix(root) {
                files.push(relative.to_string_lossy().into_owned());
            }
        }
    }
    files.sort();
    files
}

/// The mention query when the text ends in an `@`-prefixed token.
pub fn active_mention(text: &str) -> Option<&str> {
    if text.starts_with('/') || text.ends_with(char::is_whitespace) {
        return None;
    }
    let token = text.split_whitespace().next_back()?;
    token.strip_prefix('@')
}

/// Case-insensitive substring match on relative paths; earlier and shorter
/// matches rank first.
pub fn mention_matches<'a>(files: &'a [String], query: &str, limit: usize) -> Vec<&'a str> {
    let needle = query.to_lowercase();
    let mut scored: Vec<(usize, usize, &str)> = files
        .iter()
        .filter_map(|file| {
            file.to_lowercase()
                .find(&needle)
                .map(|at| (at, file.len(), file.as_str()))
        })
        .collect();
    scored.sort();
    scored.into_iter().take(limit).map(|(_, _, f)| f).collect()
}
```
(Empty query matches every file at position 0 — that is intended: bare `@` lists files.)

Run: `cargo test -p ygg-coding-agent composer::` — PASS.

- [ ] **Step 4: Keymap `CompleteMention` (test first)**

Add to `keymap.rs` tests:

```rust
    #[test]
    fn tab_on_trailing_at_token_requests_mention_completion() {
        assert_eq!(
            translate(Some(key(KeyCode::Tab, KeyModifiers::NONE)), false, "see @sr"),
            InputAction::CompleteMention
        );
        assert_eq!(
            translate(Some(key(KeyCode::Tab, KeyModifiers::NONE)), false, "plain"),
            InputAction::Ignore
        );
        assert_eq!(
            translate(Some(key(KeyCode::Tab, KeyModifiers::NONE)), false, "/mod"),
            InputAction::CompleteSlashCommand
        );
    }
```

Run `cargo test -p ygg-coding-agent keymap` — FAIL (no variant). Implement: add `CompleteMention` to `InputAction`; in `translate`, immediately after the slash-Tab check (~line 103):

```rust
            if key.code == KeyCode::Tab
                && key.modifiers.is_empty()
                && crate::tui::composer::active_mention(editor_text).is_some()
            {
                return InputAction::CompleteMention;
            }
```
Run again — PASS.

- [ ] **Step 5: Shell mention state + completion (test first)**

Add `view.rs` tests:

```rust
    #[test]
    fn mention_completion_inserts_path_reference_for_text_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), b"x").unwrap();

        let mut shell = InteractiveShell::test_shell();
        shell.set_workspace(dir.path().to_path_buf());
        for character in "see @main".chars() {
            shell.apply_edit(EditAction::Char(character));
        }
        shell.complete_mention();
        assert_eq!(shell.pending(), "see @src/main.rs ");
    }

    #[test]
    fn mention_completion_attaches_media_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("shot.png"), b"png").unwrap();

        let mut shell = InteractiveShell::test_shell();
        shell.set_workspace(dir.path().to_path_buf());
        shell.set_input_modalities(ygg_ai::ModalitySet::none().with(ygg_ai::Modality::Image));
        for character in "@shot".chars() {
            shell.apply_edit(EditAction::Char(character));
        }
        shell.complete_mention();
        assert_eq!(shell.pending(), "[Image #1: shot.png]");
        let composed = shell.drain_composed();
        assert!(composed
            .parts
            .iter()
            .any(|p| matches!(p, ygg_agent::InputPart::Media(_))));
    }
```

Run — FAIL (`set_workspace`, `complete_mention` undefined). Implement in `view.rs`:

1. `ShellState` fields: `workspace: Option<PathBuf>`, `file_index: Option<Vec<String>>`.
2. Methods:

```rust
    pub fn set_workspace(&mut self, root: PathBuf) {
        let mut state = self.state.borrow_mut();
        state.workspace = Some(root);
        state.file_index = None; // rebuild lazily
    }

    /// Complete the trailing `@token`: media files attach, others insert a
    /// plain `@relative/path` reference.
    pub fn complete_mention(&mut self) {
        let mut state = self.state.borrow_mut();
        if state.editor_cursor != state.editor.len() {
            return;
        }
        let Some(query) = composer::active_mention(&state.editor).map(str::to_owned) else {
            return;
        };
        let Some(root) = state.workspace.clone() else {
            return;
        };
        if state.file_index.is_none() {
            state.file_index = Some(composer::workspace_files(&root, 10_000));
        }
        let files = state.file_index.as_ref().expect("just built");
        let Some(top) = composer::mention_matches(files, &query, 1).first().copied() else {
            return;
        };
        let top = top.to_owned();
        let token_start = state.editor.len() - (query.len() + 1); // includes '@'
        let absolute = root.join(&top);
        if composer::media_kind_for_path(&absolute).is_some() {
            let modalities = state.input_modalities;
            match state.ledger.attach_media(&absolute, modalities) {
                Ok(chip) => {
                    state.editor.replace_range(token_start.., &chip);
                }
                Err(error) => {
                    state.push_block(TranscriptBlock::Notice(error.to_string()));
                    state.editor.replace_range(token_start.., &format!("@{top} "));
                }
            }
        } else {
            state.editor.replace_range(token_start.., &format!("@{top} "));
        }
        state.editor_cursor = state.editor.len();
    }
```
3. Suggestions panel: add `render_mention_suggestions(state, width, max_rows)` modeled exactly on `render_slash_suggestions` (~line 891): when `composer::active_mention(&state.editor)` is `Some(query)` and `state.file_index` is built, render up to 5 `mention_matches`; call it where `render_slash_suggestions` is called in `render_shell`/`render_prompt_box` composition (same insertion point, mention panel only when the slash panel is absent). Build the index lazily at render time only if already built — do **not** walk the filesystem during render; if the index is not built yet, render nothing (the first Tab builds it). To make suggestions appear as-you-type, also build the index in `apply_edit` when a mention becomes active and `file_index.is_none()`.

- [ ] **Step 6: Wire into interactive mode**

In `interactive.rs`, both `wait_for_prompt` and `drive_active_run` match arms: add

```rust
                    InputAction::CompleteMention => {
                        shell.complete_mention();
                        shell.render();
                    }
```
In `update_status`, add `shell.set_workspace(app.config.workspace.clone());` next to `set_input_modalities`.

- [ ] **Step 7: Run the crate suite**

Run: `cargo test -p ygg-coding-agent`
Expected: PASS

- [ ] **Step 8: Commit**

```bash
git add crates/ygg-coding-agent/src/tui/composer.rs crates/ygg-coding-agent/src/tui/keymap.rs crates/ygg-coding-agent/src/tui/view.rs crates/ygg-coding-agent/src/modes/interactive.rs crates/ygg-coding-agent/Cargo.toml Cargo.lock
git commit -m "feat(coding-agent): @ file mentions with gitignore-aware fuzzy completion"
```

---

### Task 9: Help text, docs, and full-workspace verification

**Files:**
- Modify: `crates/ygg-coding-agent/src/commands.rs` (`help_text()` — add lines documenting attachments: paste/drop a media file path to attach it; large pastes collapse to `[Pasted text #N]` chips; `@` + Tab completes project files and attaches media)
- Modify: `docs/design/ygg-agent.md` (update the `prompt`/`steer`/`follow_up` signatures to `impl Into<UserInput>` and document `UserInput`/`InputPart` in the API section)

**Steps:**

- [ ] **Step 1: Update `help_text()`** with the three new input behaviors (match its existing formatting; check its unit tests if any assert the exact text).

- [ ] **Step 2: Update `docs/design/ygg-agent.md`** — find the agent API section describing `prompt(impl Into<String>)` and rewrite for `UserInput`, noting `From<String>` compatibility and that this was a sanctioned boundary widening per the 2026-07-15 spec.

- [ ] **Step 3: Full verification**

Run, expecting all to pass cleanly:
```bash
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all -- --check
```

- [ ] **Step 4: Commit**

```bash
git add crates/ygg-coding-agent/src/commands.rs docs/design/ygg-agent.md
git commit -m "docs(coding-agent): document multimodal input in help and design docs"
```

---

## Spec coverage checklist (self-review)

| Spec requirement | Task |
|---|---|
| Large pastes collapse to chips, spliced at submit | 3, 4, 5 |
| Path detection on paste/drag-drop (file://, escapes, ~) | 3, 5 |
| `@` mentions: media attach, text files as `@path` reference | 8 |
| Audio files attach natively (wav/mp3/flac/opus/aac/m4a) | 3, 4 |
| Capability gate at attach + notice + plain-path fallback | 4, 5, 8 |
| Size caps 5 MB / 25 MB | 3, 4 |
| `prompt`/`steer`/`follow_up` accept parts; `From<String>` compat | 1, 2 |
| Steering queue carries parts; restore returns chips + ledger | 2, 5 |
| `SteeringDelivered` summaries, FIFO consumption | 2, 5 |
| Transcript renders media markers (live + hydrated) | 5, 7 |
| Orphaned chips dropped; mangled chips literal | 4 |
| ygg-ai unchanged | all |
