#![allow(missing_docs)]

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use sexy_tui_rs::{ImageId, ImageLimits, TerminalImage};
use ygg_agent::{Entry, EntryMetadata, EntryValue, Session};
use ygg_ai::{AssistantPart, ImageSource, Media, Message, ToolCallId, ToolResultPart, UserPart};

use crate::tui::theme::ModelLab;

/// Maximum owned image bytes accepted from one tool-result image. This sits
/// below the terminal foundation's hard cap so the product projection remains
/// bounded even when a session contains larger provider payloads.
pub(crate) const MAX_TOOL_RESULT_IMAGE_BYTES: usize = 2 * 1024 * 1024;
/// Maximum inline image payload bytes retained for one tool result.
pub(crate) const MAX_TOOL_RESULT_IMAGE_BYTES_PER_ITEM: usize = 4 * 1024 * 1024;
/// Maximum validated images retained for one tool result.
pub(crate) const MAX_TOOL_RESULT_IMAGES_PER_ITEM: usize = 4;
/// Maximum validated tool-result images retained in one interactive session.
pub(crate) const MAX_SESSION_TOOL_RESULT_IMAGES: usize = 32;
/// Maximum inline image payload bytes retained in one interactive session.
pub(crate) const MAX_SESSION_TOOL_RESULT_IMAGE_BYTES: usize = 16 * 1024 * 1024;

/// A bounded, opaque image projection for one tool result.
///
/// Raw bytes remain private inside [`TerminalImage`]. The only public
/// projection methods expose display-safe format, dimensions, and byte-count
/// summaries, never base64, source locations, or payload slices.
#[derive(Clone)]
pub(crate) enum ToolResultImage {
    Ready {
        image: Arc<TerminalImage>,
        id: Option<ImageId>,
    },
    Placeholder(ToolImagePlaceholder),
}

impl fmt::Debug for ToolResultImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ready { image, id } => f
                .debug_struct("ToolResultImage")
                .field("format", &image.format())
                .field("dimensions", &image.dimensions())
                .field("byte_len", &image.byte_len())
                .field("id", id)
                .finish(),
            Self::Placeholder(reason) => f
                .debug_tuple("ToolResultImage::Placeholder")
                .field(reason)
                .finish(),
        }
    }
}

impl PartialEq for ToolResultImage {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Ready { image: left, .. }, Self::Ready { image: right, .. }) => {
                left.format() == right.format()
                    && left.dimensions() == right.dimensions()
                    && left.byte_len() == right.byte_len()
            }
            (Self::Placeholder(left), Self::Placeholder(right)) => left == right,
            _ => false,
        }
    }
}

impl ToolResultImage {
    fn ready(image: TerminalImage) -> Self {
        Self::Ready {
            image: Arc::new(image),
            id: None,
        }
    }

    /// Clone the validated opaque image for the terminal-owned store.
    pub(crate) fn terminal_image(&self) -> Option<Arc<TerminalImage>> {
        match self {
            Self::Ready { image, .. } => Some(image.clone()),
            Self::Placeholder(_) => None,
        }
    }

    /// Stable logical image ID assigned by the interactive shell.
    pub(crate) fn id(&self) -> Option<ImageId> {
        match self {
            Self::Ready { id, .. } => *id,
            Self::Placeholder(_) => None,
        }
    }

    /// Assign a newly allocated stable terminal image ID.
    pub(crate) fn set_id(&mut self, id: ImageId) {
        if let Self::Ready { id: current, .. } = self {
            *current = Some(id);
        }
    }

    /// Clear a previous terminal placement before rebuilding a transcript.
    pub(crate) fn clear_id(&mut self) {
        if let Self::Ready { id, .. } = self {
            *id = None;
        }
    }

    /// Replace a ready payload with a bounded non-payload fallback.
    pub(crate) fn replace_with_placeholder(&mut self, reason: ToolImagePlaceholder) {
        *self = Self::Placeholder(reason);
    }

    /// Owned payload bytes, used only for bounded internal accounting.
    pub(crate) fn byte_len(&self) -> Option<usize> {
        match self {
            Self::Ready { image, .. } => Some(image.byte_len()),
            Self::Placeholder(_) => None,
        }
    }

    /// Deterministic text fallback that never includes payload data.
    pub(crate) fn fallback_text(&self, hidden: bool) -> String {
        match self {
            Self::Ready { image, .. } => {
                let dimensions = image.dimensions();
                let state = if hidden {
                    "hidden; enable terminal images"
                } else {
                    "unavailable"
                };
                format!(
                    "[image: {} {}x{} ({state})]",
                    image.format().name(),
                    dimensions.width(),
                    dimensions.height(),
                )
            }
            Self::Placeholder(reason) => reason.text().to_owned(),
        }
    }
}

/// Why an image has a text placeholder instead of an owned payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolImagePlaceholder {
    NotInline,
    Invalid,
    ImageByteLimit,
    PerItemByteLimit,
    PerItemImageLimit,
    SessionImageLimit,
    SessionByteLimit,
    TerminalRegistryLimit,
}

impl ToolImagePlaceholder {
    fn text(self) -> &'static str {
        match self {
            Self::NotInline => "[image unavailable: inline payload required]",
            Self::Invalid => "[image unavailable: invalid inline payload]",
            Self::ImageByteLimit => "[image unavailable: image byte limit]",
            Self::PerItemByteLimit => "[image unavailable: tool-result byte limit]",
            Self::PerItemImageLimit => "[image unavailable: tool-result image limit]",
            Self::SessionImageLimit => "[image unavailable: session image limit]",
            Self::SessionByteLimit => "[image unavailable: session byte limit]",
            Self::TerminalRegistryLimit => "[image unavailable: terminal image limit]",
        }
    }
}

/// Accounting for validated images retained by one interactive session.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ToolImageBudget {
    image_count: usize,
    image_bytes: usize,
}

impl ToolImageBudget {
    fn can_retain(&self, bytes: usize) -> Result<(), ToolImagePlaceholder> {
        if self.image_count >= MAX_SESSION_TOOL_RESULT_IMAGES {
            return Err(ToolImagePlaceholder::SessionImageLimit);
        }
        if self.image_bytes.saturating_add(bytes) > MAX_SESSION_TOOL_RESULT_IMAGE_BYTES {
            return Err(ToolImagePlaceholder::SessionByteLimit);
        }
        Ok(())
    }

    fn retain(&mut self, bytes: usize) {
        self.image_count = self.image_count.saturating_add(1);
        self.image_bytes = self.image_bytes.saturating_add(bytes);
    }

    /// Re-account one already validated payload while rebuilding the visible
    /// transcript, preserving the precise bounded fallback when it no longer
    /// fits the session budget.
    pub(crate) fn retain_existing(&mut self, bytes: usize) -> Result<(), ToolImagePlaceholder> {
        self.can_retain(bytes)?;
        self.retain(bytes);
        Ok(())
    }
}

/// Product-level limits used for every parse and placement revalidation.
pub(crate) fn tool_image_limits() -> ImageLimits {
    ImageLimits::default()
        .with_max_payload_bytes(MAX_TOOL_RESULT_IMAGE_BYTES)
        .expect("fixed tool image payload limit is valid")
}

/// Project ordered tool-result image media into bounded opaque storage.
///
/// Non-inline image sources are never resolved. Invalid and over-budget image
/// parts become small deterministic placeholders, preserving result ordering
/// without retaining a path, URL, provider reference, or raw payload.
pub(crate) fn project_tool_images<'a>(
    media: impl IntoIterator<Item = &'a Media>,
    session_budget: &mut ToolImageBudget,
) -> Vec<ToolResultImage> {
    let mut images = Vec::new();
    let mut item_count = 0usize;
    let mut item_bytes = 0usize;
    for media in media {
        let Media::Image(image) = media else {
            continue;
        };
        if item_count >= MAX_TOOL_RESULT_IMAGES_PER_ITEM {
            images.push(ToolResultImage::Placeholder(
                ToolImagePlaceholder::PerItemImageLimit,
            ));
            break;
        }
        item_count = item_count.saturating_add(1);
        let ImageSource::Inline(data) = &image.source else {
            images.push(ToolResultImage::Placeholder(
                ToolImagePlaceholder::NotInline,
            ));
            continue;
        };
        let bytes = data.len();
        let placeholder = if bytes > MAX_TOOL_RESULT_IMAGE_BYTES {
            Some(ToolImagePlaceholder::ImageByteLimit)
        } else if item_bytes.saturating_add(bytes) > MAX_TOOL_RESULT_IMAGE_BYTES_PER_ITEM {
            Some(ToolImagePlaceholder::PerItemByteLimit)
        } else {
            session_budget.can_retain(bytes).err()
        };
        if let Some(placeholder) = placeholder {
            images.push(ToolResultImage::Placeholder(placeholder));
            continue;
        }
        let Ok(image) =
            TerminalImage::from_slice_with_metadata(data, Default::default(), &tool_image_limits())
        else {
            images.push(ToolResultImage::Placeholder(ToolImagePlaceholder::Invalid));
            continue;
        };
        item_bytes = item_bytes.saturating_add(bytes);
        session_budget.retain(bytes);
        images.push(ToolResultImage::ready(image));
    }
    images
}

/// One displayable item reconstructed from a session's active branch.
#[derive(Clone, Debug, PartialEq)]
pub enum TranscriptItem {
    User {
        text: String,
        /// Model that was active when this prompt was submitted, so the
        /// prompt card can be rendered in that model's accent colour.
        model_lab: Option<ModelLab>,
        /// Exact immutable sRGB gutter colour recorded with the prompt.
        prompt_color: Option<String>,
    },
    Assistant(String),
    Reasoning(String),
    ToolCall {
        id: ToolCallId,
        name: String,
        args: serde_json::Value,
    },
    ToolResult {
        id: ToolCallId,
        text: String,
        is_error: bool,
        /// Milliseconds between the tool's effect admission and the
        /// finalization of its result. `None` for legacy entries, for
        /// interrupted calls, and for results whose timing window was not
        /// recorded.
        duration_ms: Option<u64>,
        /// Opaque bounded image projection. Text/copy/plain surfaces ignore
        /// this field and retain their existing payload-free semantics.
        images: Vec<ToolResultImage>,
    },
    CompactionMarker {
        summary: String,
    },
    NativeCompactionMarker,
}

fn tool_result_text(parts: &[ToolResultPart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            ToolResultPart::Text(text) => Some(text.as_str()),
            ToolResultPart::Media(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

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
                .map(ToString::to_string)
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

/// Wall-time window, in milliseconds, recorded in the tool call's
/// durable metadata.
fn tool_result_duration_ms(metadata: Option<&EntryMetadata>) -> Option<u64> {
    metadata.and_then(|metadata| {
        match (
            metadata.tool_started_unix_ms,
            metadata.tool_finished_unix_ms,
        ) {
            (Some(started), Some(finished)) => Some(finished.saturating_sub(started)),
            _ => None,
        }
    })
}

fn push_message(
    items: &mut Vec<TranscriptItem>,
    image_budget: &mut ToolImageBudget,
    message: &Message,
    model_lab: Option<ModelLab>,
    prompt_color: Option<String>,
    display_text: Option<&str>,
    metadata: Option<&EntryMetadata>,
) {
    match message {
        Message::User(user) => {
            if let Some(text) = display_text.filter(|_| {
                !user
                    .content
                    .iter()
                    .any(|part| matches!(part, UserPart::ToolResult(_)))
            }) {
                items.push(TranscriptItem::User {
                    text: text.to_owned(),
                    model_lab,
                    prompt_color,
                });
                return;
            }
            let contains_tool_result = user
                .content
                .iter()
                .any(|part| matches!(part, UserPart::ToolResult(_)));
            let prompt_color = (!contains_tool_result).then_some(prompt_color).flatten();
            let mut text = String::new();
            for part in &user.content {
                match part {
                    UserPart::Text(value) => text.push_str(value),
                    UserPart::ToolResult(result) => {
                        // Flush the accumulated text run first so interleaved
                        // text and tool results keep their original order.
                        if !text.is_empty() {
                            items.push(TranscriptItem::User {
                                text: std::mem::take(&mut text),
                                model_lab,
                                prompt_color: prompt_color.clone(),
                            });
                        }
                        items.push(TranscriptItem::ToolResult {
                            id: result.tool_call_id.clone(),
                            text: tool_result_text(&result.content),
                            is_error: result.is_error,
                            duration_ms: tool_result_duration_ms(metadata),
                            images: project_tool_images(
                                result.content.iter().filter_map(|part| match part {
                                    ToolResultPart::Media(media) => Some(media),
                                    ToolResultPart::Text(_) => None,
                                }),
                                image_budget,
                            ),
                        })
                    }
                    UserPart::Media(media) => text.push_str(&media_marker(media)),
                }
            }
            if !text.is_empty() {
                items.push(TranscriptItem::User {
                    text,
                    model_lab,
                    prompt_color,
                });
            }
        }
        Message::Assistant(assistant) => {
            for part in &assistant.content {
                match part {
                    AssistantPart::Text(text) => {
                        items.push(TranscriptItem::Assistant(text.clone()))
                    }
                    AssistantPart::Reasoning(reasoning) => {
                        if let Some(text) = &reasoning.text {
                            items.push(TranscriptItem::Reasoning(text.clone()));
                        }
                    }
                    AssistantPart::ToolCall(call) => {
                        let args = call.arguments_value().unwrap_or(serde_json::Value::Null);
                        items.push(TranscriptItem::ToolCall {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            args,
                        });
                    }
                    AssistantPart::Media(_) | AssistantPart::ProviderMetadata(_) => {}
                }
            }
        }
    }
}

fn active_branch_tail_from<'a>(
    session: &'a Session,
    head: Option<&'a ygg_agent::EntryId>,
    max_entries: Option<usize>,
) -> anyhow::Result<(Vec<&'a Entry>, bool)> {
    let mut entries = Vec::new();
    let mut displayable_entries = 0usize;
    let mut cursor = head;
    let mut truncated = false;
    while let Some(id) = cursor {
        if max_entries.is_some_and(|limit| displayable_entries >= limit) {
            truncated = true;
            break;
        }
        let entry = session
            .entry(id)
            .ok_or_else(|| anyhow::anyhow!("dangling session entry {}", id.0))?;
        if matches!(
            &entry.value,
            EntryValue::Message(_)
                | EntryValue::Compaction { .. }
                | EntryValue::ResponsesCompaction { .. }
        ) {
            displayable_entries = displayable_entries.saturating_add(1);
        }
        entries.push(entry);
        cursor = entry.parent.as_ref();
    }
    // A user tool-result entry normally follows the assistant tool call it
    // completes. ResponsesTurn and other display-invisible records can sit
    // between that pair, so walk through them and include the next displayable
    // parent whenever the bounded cut would otherwise show an orphan result.
    if truncated
        && entries.last().is_some_and(|entry| {
            matches!(
                &entry.value,
                EntryValue::Message(Message::User(user))
                    if user
                        .content
                        .iter()
                        .any(|part| matches!(part, UserPart::ToolResult(_)))
            )
        })
    {
        while let Some(id) = cursor {
            let entry = session
                .entry(id)
                .ok_or_else(|| anyhow::anyhow!("dangling session entry {}", id.0))?;
            entries.push(entry);
            cursor = entry.parent.as_ref();
            if matches!(
                &entry.value,
                EntryValue::Message(_)
                    | EntryValue::Compaction { .. }
                    | EntryValue::ResponsesCompaction { .. }
            ) {
                break;
            }
        }
    }
    entries.reverse();
    Ok((entries, truncated))
}

fn hydrate_entries_with_image_budget(
    entries: Vec<&Entry>,
) -> (Vec<TranscriptItem>, ToolImageBudget) {
    // Pair results only with the assistant turn they follow. Provider call IDs
    // are not guaranteed globally unique across a long session, so one later
    // turn reusing an ID must not make an older interrupted call look durable.
    // This reverse pass is linear in entries and message parts.
    let mut following_results = HashMap::<ToolCallId, usize>::new();
    let mut interrupted_calls = HashSet::new();
    for entry in entries.iter().rev() {
        match &entry.value {
            EntryValue::Message(Message::User(user)) => {
                for result in user.content.iter().filter_map(|part| match part {
                    UserPart::ToolResult(result) => Some(result),
                    _ => None,
                }) {
                    *following_results
                        .entry(result.tool_call_id.clone())
                        .or_default() += 1;
                }
            }
            EntryValue::Message(Message::Assistant(assistant)) => {
                for (part_index, call) in
                    assistant
                        .content
                        .iter()
                        .enumerate()
                        .filter_map(|(index, part)| match part {
                            AssistantPart::ToolCall(call) => Some((index, call)),
                            _ => None,
                        })
                {
                    let paired = following_results.get_mut(&call.id).is_some_and(|count| {
                        if *count == 0 {
                            false
                        } else {
                            *count -= 1;
                            true
                        }
                    });
                    if !paired {
                        interrupted_calls.insert((entry.id.clone(), part_index));
                    }
                }
                following_results.clear();
            }
            _ => {}
        }
    }
    // Track the active model through the session so user prompts can be
    // rendered in the accent of the model they were sent to.
    let mut active_lab: Option<ModelLab> = None;
    let mut active_model: Option<String> = None;
    let mut items = Vec::new();
    let mut image_budget = ToolImageBudget::default();
    for entry in entries {
        match &entry.value {
            EntryValue::Message(message) => {
                // New sessions attach the exact prompt model/source to the
                // message entry. Legacy sessions fall back only to historical
                // config markers on this branch; the currently selected model
                // is never consulted during replay.
                let prompt_lab = entry
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.prompt_model_source.as_deref())
                    .and_then(ModelLab::from_key)
                    .or_else(|| {
                        entry
                            .metadata
                            .as_ref()
                            .and_then(|metadata| metadata.prompt_model.as_ref())
                            .and_then(|model| crate::tui::theme::classify_model_text(&model.0))
                    })
                    .or(active_lab);
                // The stored colour is authoritative forever. Older sessions
                // derive a deterministic fallback only from durable identity
                // on this branch, never from the model selected at resume.
                let prompt_color = entry
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.prompt_color.clone())
                    .or_else(|| {
                        entry
                            .metadata
                            .as_ref()
                            .and_then(|metadata| metadata.prompt_model.as_ref())
                            .map(|model| crate::tui::theme::prompt_color_for_model_id(&model.0))
                    })
                    .or_else(|| {
                        active_model
                            .as_deref()
                            .map(crate::tui::theme::prompt_color_for_model_id)
                    });
                let display_text = entry
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.display_text.as_deref());
                push_message(
                    &mut items,
                    &mut image_budget,
                    message,
                    prompt_lab,
                    prompt_color,
                    display_text,
                    entry.metadata.as_ref(),
                );
                // No process from a previous UI instance can still be running.
                // Close an unpaired persisted call visibly instead of reviving
                // it as an active spinner. Agent recovery will durably pair the
                // call before the next provider prompt (safe calls may replay;
                // mutating calls receive an indeterminate error).
                if let Message::Assistant(assistant) = message {
                    for (part_index, call) in assistant.content.iter().enumerate().filter_map(
                        |(index, part)| match part {
                            AssistantPart::ToolCall(call) => Some((index, call)),
                            _ => None,
                        },
                    ) {
                        if interrupted_calls.contains(&(entry.id.clone(), part_index)) {
                            items.push(TranscriptItem::ToolResult {
                                id: call.id.clone(),
                                text: "interrupted before a durable tool result was recorded; this call is not running and will be reconciled before the next prompt".into(),
                                is_error: true,
                                duration_ms: None,
                                images: Vec::new(),
                            });
                        }
                    }
                }
            }
            EntryValue::Config { model, .. } => {
                // Update the active model lab whenever a config entry
                // records a model change.
                if let Some(model_name) = model {
                    let lab =
                        crate::tui::theme::classify_model_text(&model_name.to_ascii_lowercase());
                    active_lab = lab;
                    active_model = Some(model_name.clone());
                }
            }
            EntryValue::Compaction { summary, .. } => {
                items.push(TranscriptItem::CompactionMarker {
                    summary: summary.clone(),
                });
            }
            EntryValue::ResponsesCompaction { .. } => {
                items.push(TranscriptItem::NativeCompactionMarker);
            }
            EntryValue::ResponsesTurn { .. }
            | EntryValue::SkillActivated { .. }
            | EntryValue::PromptTemplateSelected { .. }
            | EntryValue::SkillResourceRead { .. }
            | EntryValue::SkillDeactivated { .. } => {}
        }
    }
    (items, image_budget)
}

/// Rebuild the display transcript by walking head-to-root on the active branch
/// and reversing it into chronological order. Abandoned branches never appear.
#[cfg(test)]
pub fn hydrate_transcript(session: &Session) -> anyhow::Result<Vec<TranscriptItem>> {
    let (entries, _) = active_branch_tail_from(session, session.head_ref(), None)?;
    Ok(hydrate_entries_with_image_budget(entries).0)
}

/// Rebuild an immutable session head and return the exact image budget consumed
/// by the retained opaque projection.
pub(crate) fn hydrate_transcript_at_with_image_budget(
    session: &Session,
    head: &ygg_agent::EntryId,
) -> anyhow::Result<(Vec<TranscriptItem>, ToolImageBudget)> {
    let (entries, _) = active_branch_tail_from(session, Some(head), None)?;
    Ok(hydrate_entries_with_image_budget(entries))
}

/// Rebuild only the newest active-branch entries for latency-sensitive first
/// paint. The caller can materialize the full history on the first attempt to
/// scroll past this window.
pub fn hydrate_transcript_tail(
    session: &Session,
    max_entries: usize,
) -> anyhow::Result<(Vec<TranscriptItem>, bool)> {
    let (items, truncated, _) = hydrate_transcript_tail_with_image_budget(session, max_entries)?;
    Ok((items, truncated))
}

/// Tail hydration plus the retained image budget used by the interactive
/// shell's subsequent live tool-result projection.
pub(crate) fn hydrate_transcript_tail_with_image_budget(
    session: &Session,
    max_entries: usize,
) -> anyhow::Result<(Vec<TranscriptItem>, bool, ToolImageBudget)> {
    let (entries, truncated) =
        active_branch_tail_from(session, session.head_ref(), Some(max_entries.max(1)))?;
    let (items, budget) = hydrate_entries_with_image_budget(entries);
    Ok((items, truncated, budget))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ygg_agent::{EntryMetadata, EntryValue, Session};
    use ygg_ai::{
        AssistantMessage, AssistantPart, ModelId, Protocol, ToolCall, ToolResult, UserMessage,
    };

    fn user(text: &str) -> EntryValue {
        EntryValue::Message(Message::User(UserMessage {
            content: vec![UserPart::Text(text.into())],
        }))
    }

    fn assistant(text: &str) -> EntryValue {
        EntryValue::Message(Message::Assistant(AssistantMessage {
            content: vec![AssistantPart::Text(text.into())],
            model: ModelId("test".into()),
            protocol: Protocol::OpenAiChat,
        }))
    }

    #[test]
    fn tool_image_budget_rejects_the_next_session_image() {
        let mut budget = ToolImageBudget::default();
        for _ in 0..MAX_SESSION_TOOL_RESULT_IMAGES {
            assert!(budget.retain_existing(1).is_ok());
        }
        assert_eq!(
            budget.retain_existing(1),
            Err(ToolImagePlaceholder::SessionImageLimit)
        );
    }

    #[test]
    fn walks_the_active_branch_chronologically_and_excludes_abandoned_branch() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("session.jsonl")).unwrap();
        let root = session.append(user("active question")).unwrap();
        let abandoned = session.append(assistant("abandoned answer")).unwrap();
        session.checkout(root).unwrap();
        session.append(assistant("active answer")).unwrap();
        assert_ne!(session.head(), Some(abandoned));

        assert_eq!(
            hydrate_transcript(&session).unwrap(),
            vec![
                TranscriptItem::User {
                    text: "active question".into(),
                    model_lab: None,
                    prompt_color: None,
                },
                TranscriptItem::Assistant("active answer".into()),
            ]
        );
    }

    #[test]
    fn tail_hydration_bounds_resume_work_and_reports_deferred_history() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("session.jsonl")).unwrap();
        for index in 0..100 {
            session.append(user(&format!("prompt {index}"))).unwrap();
        }

        let (items, truncated) = hydrate_transcript_tail(&session, 4).unwrap();
        assert!(truncated);
        assert_eq!(items.len(), 4);
        assert_eq!(
            items,
            (96..100)
                .map(|index| TranscriptItem::User {
                    text: format!("prompt {index}"),
                    model_lab: None,
                    prompt_color: None,
                })
                .collect::<Vec<_>>()
        );

        let (items, truncated) = hydrate_transcript_tail(&session, 100).unwrap();
        assert!(!truncated);
        assert_eq!(items.len(), 100);
    }

    #[test]
    fn resumed_compaction_retains_the_complete_expandable_summary() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        let mut session = Session::create(&path).unwrap();
        let first_kept = session.append(user("kept prompt")).unwrap();
        let summary = "# Earlier work\n\n- retained detail\n- final sentinel";
        session
            .append(EntryValue::Compaction {
                summary: summary.into(),
                first_kept,
                active_skills: Vec::new(),
                skill_resources: Vec::new(),
                details: Default::default(),
            })
            .unwrap();
        drop(session);

        let resumed = Session::open(path).unwrap();
        assert!(matches!(
            hydrate_transcript(&resumed).unwrap().last(),
            Some(TranscriptItem::CompactionMarker { summary: hydrated }) if hydrated == summary
        ));
    }

    #[test]
    fn resumed_native_compaction_renders_only_a_payload_free_marker() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        let mut session = Session::create(&path).unwrap();
        let covered_through = session.append(user("kept prompt")).unwrap();
        session
            .append(EntryValue::ResponsesCompaction {
                endpoint: ygg_ai::EndpointId("responses".into()),
                model: ModelId("gpt-test".into()),
                covered_through,
                output: ygg_ai::ResponsesOutput::new(vec![ygg_ai::ResponsesItem::new(
                    serde_json::json!({
                        "type": "compaction",
                        "encrypted_content": "must-never-render"
                    }),
                )
                .unwrap()]),
            })
            .unwrap();
        drop(session);

        let resumed = Session::open(path).unwrap();
        let items = hydrate_transcript(&resumed).unwrap();
        assert!(matches!(
            items.last(),
            Some(TranscriptItem::NativeCompactionMarker)
        ));
        assert!(!format!("{items:?}").contains("must-never-render"));
    }

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
            matches!(&items[0], TranscriptItem::User { text, .. } if text == "look: [image image/png · 1.0 KB]")
        );
    }

    #[test]
    fn mixed_text_and_tool_result_parts_keep_their_order() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("session.jsonl")).unwrap();
        session
            .append_with_metadata(
                EntryValue::Message(Message::User(UserMessage {
                    content: vec![
                        UserPart::Text("before".into()),
                        UserPart::ToolResult(ToolResult {
                            tool_call_id: ToolCallId("call-1".into()),
                            content: vec![ToolResultPart::Text("ok".into())],
                            is_error: false,
                            added_tool_names: None,
                        }),
                        UserPart::Text("after".into()),
                    ],
                })),
                Some(EntryMetadata {
                    prompt_color: Some("#123456".into()),
                    ..EntryMetadata::default()
                }),
            )
            .unwrap();

        assert_eq!(
            hydrate_transcript(&session).unwrap(),
            vec![
                TranscriptItem::User {
                    text: "before".into(),
                    model_lab: None,
                    prompt_color: None,
                },
                TranscriptItem::ToolResult {
                    id: ToolCallId("call-1".into()),
                    text: "ok".into(),
                    is_error: false,
                    duration_ms: None,
                    images: Vec::new(),
                },
                TranscriptItem::User {
                    text: "after".into(),
                    model_lab: None,
                    prompt_color: None,
                },
            ]
        );
    }

    #[test]
    fn exact_prompt_colors_survive_model_switch_save_and_resume() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        let mut session = Session::create(&path).unwrap();
        session
            .append_with_metadata(
                user("prompt for model A"),
                Some(EntryMetadata {
                    prompt_model: Some(ModelId("local-alias-a".into())),
                    prompt_model_source: Some("deepseek".into()),
                    prompt_color: Some("#123456".into()),
                    display_text: None,
                    run_outcome: None,
                    local_synthetic_assistant: false,
                    tool_output: None,
                    tool_started_unix_ms: None,
                    tool_finished_unix_ms: None,
                    extension_metadata: Default::default(),
                }),
            )
            .unwrap();
        session
            .append(EntryValue::Config {
                model: Some("local-alias-b".into()),
                reasoning: Some("high".into()),
                reasoning_mode: None,
            })
            .unwrap();
        session
            .append_with_metadata(
                user("prompt for model B"),
                Some(EntryMetadata {
                    prompt_model: Some(ModelId("local-alias-b".into())),
                    prompt_model_source: Some("anthropic".into()),
                    prompt_color: Some("#abcdef".into()),
                    display_text: None,
                    run_outcome: None,
                    local_synthetic_assistant: false,
                    tool_output: None,
                    tool_started_unix_ms: None,
                    tool_finished_unix_ms: None,
                    extension_metadata: Default::default(),
                }),
            )
            .unwrap();
        drop(session);

        let resumed = Session::open(path).unwrap();
        let prompts = hydrate_transcript(&resumed)
            .unwrap()
            .into_iter()
            .filter_map(|item| match item {
                TranscriptItem::User {
                    text,
                    model_lab,
                    prompt_color,
                } => Some((text, model_lab, prompt_color)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            prompts,
            vec![
                (
                    "prompt for model A".into(),
                    Some(ModelLab::DeepSeek),
                    Some("#123456".into()),
                ),
                (
                    "prompt for model B".into(),
                    Some(ModelLab::Anthropic),
                    Some("#abcdef".into()),
                ),
            ]
        );
    }

    #[test]
    fn legacy_prompts_use_only_historical_config_markers() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("session.jsonl")).unwrap();
        session
            .append(EntryValue::Config {
                model: Some("gpt-5.6".into()),
                reasoning: None,
                reasoning_mode: None,
            })
            .unwrap();
        session.append(user("old A")).unwrap();
        session
            .append(EntryValue::Config {
                model: Some("deepseek-v4".into()),
                reasoning: None,
                reasoning_mode: None,
            })
            .unwrap();
        session.append(user("old B")).unwrap();

        let prompts = hydrate_transcript(&session)
            .unwrap()
            .into_iter()
            .filter_map(|item| match item {
                TranscriptItem::User {
                    model_lab,
                    prompt_color,
                    ..
                } => Some((model_lab, prompt_color)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            prompts,
            vec![
                (
                    Some(ModelLab::OpenAi),
                    Some(crate::tui::theme::prompt_color_for_model_id("gpt-5.6")),
                ),
                (
                    Some(ModelLab::DeepSeek),
                    Some(crate::tui::theme::prompt_color_for_model_id("deepseek-v4")),
                ),
            ]
        );
    }

    #[test]
    fn maps_tool_call_and_result_parts() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("session.jsonl")).unwrap();
        session.append(user("read it")).unwrap();
        session
            .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::ToolCall(ToolCall {
                    id: ToolCallId("call-1".into()),
                    name: "read".into(),
                    arguments_json: r#"{"path":"x"}"#.into(),
                    argument_error: None,
                })],
                model: ModelId("test".into()),
                protocol: Protocol::OpenAiChat,
            })))
            .unwrap();
        session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::ToolResult(ToolResult {
                    tool_call_id: ToolCallId("call-1".into()),
                    content: vec![ToolResultPart::Text("contents".into())],
                    is_error: false,
                    added_tool_names: None,
                })],
            })))
            .unwrap();

        let path = session.path().to_owned();
        drop(session);
        let session = Session::open(path).unwrap();

        let items = hydrate_transcript(&session).unwrap();
        assert_eq!(
            items.len(),
            3,
            "a durable result must not gain a synthetic interruption"
        );
        assert!(matches!(items[1], TranscriptItem::ToolCall { ref name, .. } if name == "read"));
        assert!(
            matches!(items[2], TranscriptItem::ToolResult { ref text, .. } if text == "contents")
        );

        let (tail, truncated) = hydrate_transcript_tail(&session, 1).unwrap();
        assert!(truncated);
        assert!(matches!(tail[0], TranscriptItem::ToolCall { .. }));
        assert!(matches!(tail[1], TranscriptItem::ToolResult { .. }));
    }

    #[test]
    fn responses_sidecar_is_transparent_to_tool_pair_tail_hydration() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("session.jsonl")).unwrap();
        session.append(user("read it")).unwrap();
        let assistant = session
            .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::ToolCall(ToolCall {
                    id: ToolCallId("call-responses".into()),
                    name: "read".into(),
                    arguments_json: r#"{"path":"x"}"#.into(),
                    argument_error: None,
                })],
                model: ModelId("responses-model".into()),
                protocol: Protocol::OpenAiResponses,
            })))
            .unwrap();
        session
            .append_responses_turn(
                assistant,
                ygg_ai::EndpointId("responses-endpoint".into()),
                ModelId("responses-model".into()),
                ygg_ai::ResponsesOutput::new(vec![ygg_ai::ResponsesItem::new(serde_json::json!({
                    "type": "function_call",
                    "id": "fc_responses",
                    "call_id": "call-responses",
                    "name": "read",
                    "arguments": "{\"path\":\"x\"}"
                }))
                .unwrap()]),
            )
            .unwrap();
        session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::ToolResult(ToolResult {
                    tool_call_id: ToolCallId("call-responses".into()),
                    content: vec![ToolResultPart::Text("contents".into())],
                    is_error: false,
                    added_tool_names: None,
                })],
            })))
            .unwrap();

        for limit in [1, 2] {
            let (tail, truncated) = hydrate_transcript_tail(&session, limit).unwrap();
            assert!(truncated);
            assert_eq!(tail.len(), 2, "limit {limit}: {tail:?}");
            assert!(matches!(
                &tail[0],
                TranscriptItem::ToolCall { id, .. } if id.0 == "call-responses"
            ));
            assert!(matches!(
                &tail[1],
                TranscriptItem::ToolResult { id, text, .. }
                    if id.0 == "call-responses" && text == "contents"
            ));
        }
    }

    #[test]
    fn restart_closes_an_unpaired_tool_call_instead_of_reviving_a_spinner() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        let mut session = Session::create(&path).unwrap();
        session.append(user("read it")).unwrap();
        session
            .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::ToolCall(ToolCall {
                    id: ToolCallId("interrupted-call".into()),
                    name: "read".into(),
                    arguments_json: r#"{"path":"x"}"#.into(),
                    argument_error: None,
                })],
                model: ModelId("test".into()),
                protocol: Protocol::OpenAiChat,
            })))
            .unwrap();
        drop(session);

        let resumed = Session::open(path).unwrap();
        let (tail, truncated) = hydrate_transcript_tail(&resumed, 1).unwrap();
        assert!(truncated);
        assert!(matches!(tail[0], TranscriptItem::ToolCall { .. }));
        assert!(matches!(
            &tail[1],
            TranscriptItem::ToolResult {
                id,
                text,
                is_error: true,
                ..
            }
                if id.0 == "interrupted-call"
                    && text.contains("not running")
                    && text.contains("reconciled")
        ));
    }

    #[test]
    fn reused_call_ids_are_paired_with_only_their_own_assistant_turn() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("session.jsonl")).unwrap();
        for name in ["interrupted", "completed"] {
            session
                .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                    content: vec![AssistantPart::ToolCall(ToolCall {
                        id: ToolCallId("reused".into()),
                        name: name.into(),
                        arguments_json: "{}".into(),
                        argument_error: None,
                    })],
                    model: ModelId("test".into()),
                    protocol: Protocol::OpenAiChat,
                })))
                .unwrap();
        }
        session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::ToolResult(ToolResult {
                    tool_call_id: ToolCallId("reused".into()),
                    content: vec![ToolResultPart::Text("durable".into())],
                    is_error: false,
                    added_tool_names: None,
                })],
            })))
            .unwrap();

        let items = hydrate_transcript(&session).unwrap();
        assert!(matches!(
            &items[1],
            TranscriptItem::ToolResult { is_error: true, text, .. }
                if text.contains("not running")
        ));
        assert!(matches!(
            &items[3],
            TranscriptItem::ToolResult { is_error: false, text, .. } if text == "durable"
        ));
    }

    #[test]
    fn duplicate_call_ids_in_one_turn_consume_results_only_once() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("session.jsonl")).unwrap();
        session
            .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                content: vec![
                    AssistantPart::ToolCall(ToolCall {
                        id: ToolCallId("duplicate".into()),
                        name: "first".into(),
                        arguments_json: "{}".into(),
                        argument_error: None,
                    }),
                    AssistantPart::ToolCall(ToolCall {
                        id: ToolCallId("duplicate".into()),
                        name: "second".into(),
                        arguments_json: "{}".into(),
                        argument_error: None,
                    }),
                ],
                model: ModelId("test".into()),
                protocol: Protocol::OpenAiChat,
            })))
            .unwrap();
        session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::ToolResult(ToolResult {
                    tool_call_id: ToolCallId("duplicate".into()),
                    content: vec![ToolResultPart::Text("one durable result".into())],
                    is_error: false,
                    added_tool_names: None,
                })],
            })))
            .unwrap();

        let items = hydrate_transcript(&session).unwrap();
        assert_eq!(
            items
                .iter()
                .filter(|item| matches!(item, TranscriptItem::ToolCall { .. }))
                .count(),
            2
        );
        assert_eq!(
            items
                .iter()
                .filter(|item| matches!(item, TranscriptItem::ToolResult { is_error: true, .. }))
                .count(),
            1,
            "one durable result must not complete two duplicate call occurrences"
        );
        assert_eq!(
            items
                .iter()
                .filter(|item| matches!(
                    item,
                    TranscriptItem::ToolResult {
                        is_error: false,
                        text,
                        ..
                    } if text == "one durable result"
                ))
                .count(),
            1
        );
    }
}
