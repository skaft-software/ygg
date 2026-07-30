#![allow(missing_docs)]

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use serde::de::{IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use ygg_agent::{EntryId, EntryValue, Session};
use ygg_ai::{EndpointId, Message, ModelId, Protocol, UserPart};

static NEXT_SESSION_SUFFIX: AtomicU64 = AtomicU64::new(1);

// Keep the picker scanner under the same documented bounds as Session::open.
// Unlike a semantic replay, this path retains only IDs, parents, entry kinds,
// and one clipped user title per entry.
pub(crate) const MAX_SESSION_FILE_BYTES: usize = 256 * 1024 * 1024;
const MAX_SESSION_RECORDS: usize = 1_000_000;
const MAX_SESSION_METADATA_BYTES: usize = 64 * 1024;
const MAX_SESSION_NAME_CHARS: usize = 120;
const MAX_SESSION_TAGS: usize = 32;
const MAX_SESSION_TAG_CHARS: usize = 48;

/// Filesystem-backed sessions scoped to one canonical workspace.
#[derive(Clone, Debug)]
pub struct SessionStore {
    dir: PathBuf,
}

/// Metadata used by startup and session pickers.
#[derive(Clone, Debug)]
pub struct SessionMeta {
    pub id: String,
    pub path: PathBuf,
    pub title: String,
    pub name: Option<String>,
    pub tags: Vec<String>,
    #[cfg_attr(not(feature = "serve"), allow(dead_code))]
    pub pinned: bool,
    #[cfg_attr(not(feature = "serve"), allow(dead_code))]
    pub archived: bool,
    #[cfg_attr(not(feature = "serve"), allow(dead_code))]
    pub trashed_at_ms: Option<u64>,
    #[cfg_attr(not(feature = "serve"), allow(dead_code))]
    pub purge_after_ms: Option<u64>,
    #[cfg_attr(not(feature = "serve"), allow(dead_code))]
    pub forked_from_session_id: Option<String>,
    #[cfg_attr(not(feature = "serve"), allow(dead_code))]
    pub forked_from_entry_id: Option<String>,
    pub modified: SystemTime,
}

/// Small user-owned metadata kept next to, but separate from, append-only
/// session records. Sidecars let older Ygg binaries continue to open JSONL
/// sessions while catalog metadata remains easy to export and recover.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionUserMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub archived: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trashed_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purge_after_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from_entry_id: Option<String>,
}

#[cfg_attr(not(feature = "serve"), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionStorageLifecycle {
    Active,
    Archived,
    Trash,
}

#[cfg_attr(not(feature = "serve"), allow(dead_code))]
pub const SESSION_TRASH_RETENTION_MS: u64 = 30 * 24 * 60 * 60 * 1_000;

#[derive(Debug)]
struct SessionCandidate {
    path: PathBuf,
    modified: SystemTime,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SummaryEntryKind {
    User,
    Assistant,
    Other,
}

#[derive(Debug)]
struct SummaryEntry {
    parent: Option<EntryId>,
    kind: SummaryEntryKind,
    title: Option<String>,
    position: u32,
    assistant_model: Option<ModelId>,
    assistant_protocol: Option<Protocol>,
}

fn summary_ancestry_intervals(entries: &HashMap<EntryId, SummaryEntry>) -> (Vec<u32>, Vec<u32>) {
    const NONE: u32 = u32::MAX;

    let mut first_child = vec![NONE; entries.len()];
    let mut next_sibling = vec![NONE; entries.len()];
    for entry in entries.values() {
        let Some(parent) = entry.parent.as_ref() else {
            continue;
        };
        let parent = entries
            .get(parent)
            .expect("summary replay validates every parent before ancestry")
            .position;
        next_sibling[entry.position as usize] = first_child[parent as usize];
        first_child[parent as usize] = entry.position;
    }

    let mut entered = vec![0u32; entries.len()];
    let mut exited = vec![0u32; entries.len()];
    let mut clock = 0u32;
    let mut stack = Vec::<(u32, bool)>::new();
    for entry in entries.values().filter(|entry| entry.parent.is_none()) {
        stack.push((entry.position, false));
        while let Some((node, leaving)) = stack.pop() {
            let position = node as usize;
            if leaving {
                exited[position] = clock;
                continue;
            }
            entered[position] = clock;
            clock = clock.saturating_add(1);
            stack.push((node, true));
            let mut child = first_child[position];
            while child != NONE {
                stack.push((child, false));
                child = next_sibling[child as usize];
            }
        }
    }
    (entered, exited)
}

/// A title-only JSON string. serde_json can lend ordinary strings directly to
/// this visitor, so the common path never allocates the complete prompt merely
/// to retain its first 60 normalized characters.
struct TitleText(String);

impl<'de> Deserialize<'de> for TitleText {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct TitleVisitor;

        impl Visitor<'_> for TitleVisitor {
            type Value = TitleText;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a session-title string")
            }

            fn visit_borrowed_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(TitleText(trim_title(value)))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(TitleText(trim_title(value)))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(TitleText(trim_title(&value)))
            }
        }

        deserializer.deserialize_string(TitleVisitor)
    }
}

#[derive(Deserialize)]
enum SummaryUserPart {
    Text(TitleText),
    Media(IgnoredAny),
    ToolResult(IgnoredAny),
}

#[derive(Deserialize)]
struct SummaryUserMessage {
    content: Vec<SummaryUserPart>,
}

#[derive(Deserialize)]
enum SummaryMessage {
    User(SummaryUserMessage),
    Assistant(SummaryAssistantMessage),
}

#[derive(Deserialize)]
struct SummaryAssistantMessage {
    model: ModelId,
    protocol: Protocol,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SummaryResponsesField {
    Type,
    EncryptedContent,
    Other,
}

impl<'de> Deserialize<'de> for SummaryResponsesField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FieldVisitor;

        impl Visitor<'_> for FieldVisitor {
            type Value = SummaryResponsesField;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a Responses item field")
            }

            fn visit_borrowed_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                self.visit_str(value)
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(match value {
                    "type" => SummaryResponsesField::Type,
                    "encrypted_content" => SummaryResponsesField::EncryptedContent,
                    _ => SummaryResponsesField::Other,
                })
            }
        }

        deserializer.deserialize_identifier(FieldVisitor)
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct IsCompaction(bool);

#[derive(Clone, Copy, Debug, Default)]
struct IsNonEmptyString(bool);

macro_rules! impl_summary_string_probe {
    ($name:ident, $predicate:expr, $expected:literal) => {
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                struct ProbeVisitor;

                impl<'de> Visitor<'de> for ProbeVisitor {
                    type Value = $name;

                    fn expecting(
                        &self,
                        formatter: &mut std::fmt::Formatter<'_>,
                    ) -> std::fmt::Result {
                        formatter.write_str($expected)
                    }

                    fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E>
                    where
                        E: serde::de::Error,
                    {
                        Ok($name(($predicate)(value)))
                    }

                    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
                    where
                        E: serde::de::Error,
                    {
                        Ok($name(($predicate)(value)))
                    }

                    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
                        Ok($name(false))
                    }

                    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
                        Ok($name(false))
                    }

                    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
                        Ok($name(false))
                    }

                    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
                        Ok($name(false))
                    }

                    fn visit_none<E>(self) -> Result<Self::Value, E> {
                        Ok($name(false))
                    }

                    fn visit_unit<E>(self) -> Result<Self::Value, E> {
                        Ok($name(false))
                    }

                    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
                    where
                        D: Deserializer<'de>,
                    {
                        deserializer.deserialize_any(self)
                    }

                    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
                    where
                        A: SeqAccess<'de>,
                    {
                        while sequence.next_element::<IgnoredAny>()?.is_some() {}
                        Ok($name(false))
                    }

                    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
                    where
                        A: MapAccess<'de>,
                    {
                        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                        Ok($name(false))
                    }
                }

                deserializer.deserialize_any(ProbeVisitor)
            }
        }
    };
}

impl_summary_string_probe!(
    IsCompaction,
    |value: &str| value == "compaction",
    "the Responses item type"
);
impl_summary_string_probe!(
    IsNonEmptyString,
    |value: &str| !value.is_empty(),
    "opaque encrypted Responses content"
);

#[derive(Clone, Copy, Debug, Default)]
struct SummaryResponsesItem {
    is_compaction: bool,
    has_encrypted_content: bool,
}

impl<'de> Deserialize<'de> for SummaryResponsesItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ItemVisitor;

        impl<'de> Visitor<'de> for ItemVisitor {
            type Value = SummaryResponsesItem;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a Responses item object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut item = SummaryResponsesItem::default();
                while let Some(field) = map.next_key::<SummaryResponsesField>()? {
                    match field {
                        SummaryResponsesField::Type => {
                            item.is_compaction = map.next_value::<IsCompaction>()?.0;
                        }
                        SummaryResponsesField::EncryptedContent => {
                            item.has_encrypted_content = map.next_value::<IsNonEmptyString>()?.0;
                        }
                        SummaryResponsesField::Other => {
                            let _ = map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(item)
            }
        }

        deserializer.deserialize_map(ItemVisitor)
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct SummaryResponsesOutput {
    is_empty: bool,
    has_valid_compaction: bool,
}

impl SummaryResponsesOutput {
    fn is_empty(&self) -> bool {
        self.is_empty
    }

    fn has_valid_compaction(&self) -> bool {
        self.has_valid_compaction
    }
}

impl<'de> Deserialize<'de> for SummaryResponsesOutput {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct OutputVisitor;

        impl<'de> Visitor<'de> for OutputVisitor {
            type Value = SummaryResponsesOutput;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a Responses output array")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut item_count = 0usize;
                let mut compaction_count = 0usize;
                let mut valid_compaction_count = 0usize;
                while let Some(item) = sequence.next_element::<SummaryResponsesItem>()? {
                    item_count = item_count.saturating_add(1);
                    if item.is_compaction {
                        compaction_count = compaction_count.saturating_add(1);
                        if item.has_encrypted_content {
                            valid_compaction_count = valid_compaction_count.saturating_add(1);
                        }
                    }
                }
                Ok(SummaryResponsesOutput {
                    is_empty: item_count == 0,
                    has_valid_compaction: compaction_count == 1 && valid_compaction_count == 1,
                })
            }
        }

        deserializer.deserialize_seq(OutputVisitor)
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SummaryEntryValue {
    Message(SummaryMessage),
    Compaction {
        first_kept: EntryId,
    },
    ResponsesTurn {
        assistant: EntryId,
        #[serde(rename = "endpoint")]
        _endpoint: EndpointId,
        model: ModelId,
        output: SummaryResponsesOutput,
    },
    ResponsesCompaction {
        covered_through: EntryId,
        #[serde(rename = "endpoint")]
        _endpoint: EndpointId,
        #[serde(rename = "model")]
        _model: ModelId,
        output: SummaryResponsesOutput,
    },
    Config {},
    PromptTemplateSelected {},
    SkillActivated {},
    SkillResourceRead {},
    SkillDeactivated {},
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SummaryUsageKind {
    AssistantTurn {
        assistant: EntryId,
    },
    Compaction,
    RejectedResponsesTurn,
    TerminalGate {
        #[serde(rename = "returned")]
        _returned: Option<bool>,
    },
}

#[derive(Deserialize)]
struct SummaryUsageRecord {
    kind: SummaryUsageKind,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SummaryRecord {
    Entry {
        id: EntryId,
        parent: Option<EntryId>,
        value: SummaryEntryValue,
    },
    Head {
        id: EntryId,
    },
    RootHead {},
    Checkpoint {
        prompt: EntryId,
        head: EntryId,
    },
    Usage {
        record: SummaryUsageRecord,
    },
}

/// Derive a compact title from the oldest user text on the active branch.
pub fn active_branch_title(session: &Session) -> String {
    let mut oldest: Option<&str> = None;
    let mut cursor = session.head_ref();
    while let Some(id) = cursor {
        let Some(entry) = session.entry(id) else {
            break;
        };
        if let EntryValue::Message(Message::User(user)) = &entry.value {
            if let Some(display) = entry
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.display_text.as_deref())
            {
                oldest = Some(display);
            } else if let Some(UserPart::Text(text)) = user
                .content
                .iter()
                .find(|part| matches!(part, UserPart::Text(_)))
            {
                oldest = Some(text);
            }
        }
        cursor = entry.parent.as_ref();
    }
    oldest.map_or_else(|| "(empty session)".to_owned(), trim_title)
}

fn trim_title(title: &str) -> String {
    const LIMIT: usize = 60;
    let mut normalized = String::with_capacity(LIMIT + 3);
    let mut length = 0usize;
    for word in title.split_whitespace() {
        if !normalized.is_empty() {
            if length == LIMIT {
                normalized.push('…');
                return normalized;
            }
            normalized.push(' ');
            length += 1;
        }
        for character in word.chars() {
            if length == LIMIT {
                normalized.push('…');
                return normalized;
            }
            normalized.push(character);
            length += 1;
        }
    }
    normalized
}

fn workspace_key(workspace: &Path) -> String {
    // FNV-1a is small, deterministic, and avoids another hashing dependency.
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in workspace.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    format!("{hash:012x}")
}

fn session_id_is_valid(id: &str) -> bool {
    if id.is_empty() || id.chars().any(char::is_control) {
        return false;
    }
    let mut components = Path::new(id).components();
    matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(component)), None) if component == id
    )
}

fn sanitize_session_name(name: &str) -> anyhow::Result<Option<String>> {
    let name = name.trim();
    if name.is_empty() {
        return Ok(None);
    }
    if name.chars().count() > MAX_SESSION_NAME_CHARS || name.chars().any(char::is_control) {
        anyhow::bail!(
            "session name must be at most {MAX_SESSION_NAME_CHARS} characters and contain no control characters"
        );
    }
    Ok(Some(name.to_owned()))
}

fn sanitize_session_tags(tags: &[String]) -> anyhow::Result<Vec<String>> {
    if tags.len() > MAX_SESSION_TAGS {
        anyhow::bail!("a session may have at most {MAX_SESSION_TAGS} tags");
    }
    let mut sanitized = Vec::with_capacity(tags.len());
    for tag in tags {
        let tag = tag.trim();
        if tag.is_empty()
            || tag.chars().count() > MAX_SESSION_TAG_CHARS
            || !tag.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | '/')
            })
        {
            anyhow::bail!(
                "session tags must be 1-{MAX_SESSION_TAG_CHARS} ASCII letters/digits or '-', '_', '.', '/'"
            );
        }
        if !sanitized.iter().any(|existing| existing == tag) {
            sanitized.push(tag.to_owned());
        }
    }
    Ok(sanitized)
}

fn validate_session_metadata(metadata: &SessionUserMetadata) -> anyhow::Result<()> {
    if metadata.trashed_at_ms.is_some() != metadata.purge_after_ms.is_some()
        || metadata
            .trashed_at_ms
            .zip(metadata.purge_after_ms)
            .is_some_and(|(trashed, purge)| trashed == 0 || purge <= trashed)
        || metadata.trashed_at_ms.is_some() && !metadata.archived
    {
        anyhow::bail!("invalid session trash retention metadata");
    }
    match (
        metadata.forked_from_session_id.as_deref(),
        metadata.forked_from_entry_id.as_deref(),
    ) {
        (None, None) => Ok(()),
        (Some(session), Some(entry))
            if session_id_is_valid(session)
                && !entry.is_empty()
                && entry.len() <= 256
                && !entry.chars().any(char::is_control) =>
        {
            Ok(())
        }
        _ => anyhow::bail!("invalid session fork provenance metadata"),
    }
}

pub(crate) fn absolute_read_path(path: &Path) -> anyhow::Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("session path has no parent: {}", path.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("session path has no filename: {}", path.display()))?;
    Ok(parent.canonicalize()?.join(name))
}

fn corrupt_summary(line: usize, message: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("corrupt session record at line {line}: {message}")
}

fn summary_text_with_torn_tail(bytes: &[u8]) -> anyhow::Result<&str> {
    let completed_end = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |position| position + 1);
    if let Err(error) = std::str::from_utf8(&bytes[..completed_end]) {
        let line = bytes[..error.valid_up_to()]
            .iter()
            .filter(|byte| **byte == b'\n')
            .count()
            + 1;
        return Err(corrupt_summary(line, format!("invalid UTF-8: {error}")));
    }

    match std::str::from_utf8(bytes) {
        Ok(content) => Ok(content),
        Err(_) => Ok(std::str::from_utf8(&bytes[..completed_end])
            .expect("the completed summary prefix was validated above")),
    }
}

/// Replay only the graph metadata needed by the session picker. Large model,
/// tool, media, skill, and compaction bodies are consumed by serde without
/// being retained. This deliberately mirrors Session::open_read_only's graph
/// checks and torn-final-record handling so the fast path cannot bless a file
/// that normal resume would reject.
fn summarize_session(path: &Path) -> anyhow::Result<Option<String>> {
    let path = absolute_read_path(path)?;
    let bytes = ygg_agent::secure_fs::read_regular_file_bounded(&path, MAX_SESSION_FILE_BYTES)?;
    let content = summary_text_with_torn_tail(&bytes)?;
    let mut entries = HashMap::<EntryId, SummaryEntry>::new();
    let mut head = None;
    let mut checkpoints = Vec::<(EntryId, EntryId, usize)>::new();
    let mut segments = content.split_inclusive('\n').peekable();
    let mut line_no = 0usize;

    while let Some(segment) = segments.next() {
        line_no += 1;
        if line_no > MAX_SESSION_RECORDS {
            anyhow::bail!("session has more than {MAX_SESSION_RECORDS} records");
        }
        let is_last = segments.peek().is_none();
        let line = segment.strip_suffix('\n').unwrap_or(segment);
        let record: SummaryRecord = match serde_json::from_str(line) {
            Ok(record) => record,
            // An interrupted append cannot have written its terminating
            // newline before the preceding JSON bytes. Only an unterminated
            // final segment is therefore eligible for torn-tail recovery.
            Err(_) if is_last && !segment.ends_with('\n') => break,
            Err(error) => return Err(corrupt_summary(line_no, error)),
        };

        match record {
            SummaryRecord::Entry { id, parent, value } => {
                if entries.contains_key(&id) {
                    return Err(corrupt_summary(
                        line_no,
                        format!("duplicate entry id {:?}", id.0),
                    ));
                }
                if let Some(parent) = &parent {
                    if !entries.contains_key(parent) {
                        return Err(corrupt_summary(
                            line_no,
                            format!("entry {:?} references unknown parent {:?}", id.0, parent.0),
                        ));
                    }
                }

                let (kind, title, assistant_route) = match value {
                    SummaryEntryValue::Message(SummaryMessage::User(message)) => {
                        let title = message.content.into_iter().find_map(|part| match part {
                            SummaryUserPart::Text(TitleText(title)) => Some(title),
                            SummaryUserPart::Media(_) | SummaryUserPart::ToolResult(_) => None,
                        });
                        (SummaryEntryKind::User, title, None)
                    }
                    SummaryEntryValue::Message(SummaryMessage::Assistant(message)) => (
                        SummaryEntryKind::Assistant,
                        None,
                        Some((message.model, message.protocol)),
                    ),
                    SummaryEntryValue::Compaction { first_kept } => {
                        if !entries.contains_key(&first_kept) {
                            return Err(corrupt_summary(
                                line_no,
                                format!(
                                    "compaction {:?} references unknown first_kept {:?}",
                                    id.0, first_kept.0
                                ),
                            ));
                        }
                        (SummaryEntryKind::Other, None, None)
                    }
                    SummaryEntryValue::ResponsesTurn {
                        assistant,
                        model,
                        output,
                        ..
                    } => {
                        let valid_assistant = entries.get(&assistant).is_some_and(|candidate| {
                            candidate.kind == SummaryEntryKind::Assistant
                                && candidate.assistant_protocol == Some(Protocol::OpenAiResponses)
                                && candidate.assistant_model.as_ref() == Some(&model)
                        });
                        if !valid_assistant
                            || parent.as_ref() != Some(&assistant)
                            || output.is_empty()
                        {
                            return Err(corrupt_summary(
                                line_no,
                                format!(
                                    "Responses turn {:?} is not a direct sidecar of assistant {:?}",
                                    id.0, assistant.0
                                ),
                            ));
                        }
                        (SummaryEntryKind::Other, None, None)
                    }
                    SummaryEntryValue::ResponsesCompaction {
                        covered_through,
                        output,
                        ..
                    } => {
                        if !entries.contains_key(&covered_through)
                            || parent.as_ref() != Some(&covered_through)
                            || !output.has_valid_compaction()
                        {
                            return Err(corrupt_summary(
                                line_no,
                                format!(
                                    "Responses compaction {:?} is not a direct checkpoint of {:?}",
                                    id.0, covered_through.0
                                ),
                            ));
                        }
                        (SummaryEntryKind::Other, None, None)
                    }
                    SummaryEntryValue::Config {}
                    | SummaryEntryValue::PromptTemplateSelected {}
                    | SummaryEntryValue::SkillActivated {}
                    | SummaryEntryValue::SkillResourceRead {}
                    | SummaryEntryValue::SkillDeactivated {} => {
                        (SummaryEntryKind::Other, None, None)
                    }
                };
                let (assistant_model, assistant_protocol) = assistant_route
                    .map_or((None, None), |(model, protocol)| {
                        (Some(model), Some(protocol))
                    });
                let position = u32::try_from(entries.len()).expect("session record limit fits u32");
                entries.insert(
                    id,
                    SummaryEntry {
                        parent,
                        kind,
                        title,
                        position,
                        assistant_model,
                        assistant_protocol,
                    },
                );
            }
            SummaryRecord::Head { id } => {
                if !entries.contains_key(&id) {
                    return Err(corrupt_summary(
                        line_no,
                        format!("head references unknown entry {:?}", id.0),
                    ));
                }
                head = Some(id);
            }
            SummaryRecord::RootHead {} => {
                head = None;
            }
            SummaryRecord::Checkpoint {
                prompt,
                head: checkpoint_head,
            } => {
                let prompt_is_user = entries
                    .get(&prompt)
                    .is_some_and(|entry| entry.kind == SummaryEntryKind::User);
                if !prompt_is_user || !entries.contains_key(&checkpoint_head) {
                    return Err(corrupt_summary(
                        line_no,
                        "checkpoint references unknown or non-user entries",
                    ));
                }
                checkpoints.push((prompt, checkpoint_head, line_no));
            }
            SummaryRecord::Usage { record } => {
                if let SummaryUsageKind::AssistantTurn { assistant } = record.kind {
                    let valid_assistant = entries
                        .get(&assistant)
                        .is_some_and(|entry| entry.kind == SummaryEntryKind::Assistant);
                    if !valid_assistant {
                        return Err(corrupt_summary(
                            line_no,
                            "usage record references an unknown or non-assistant entry",
                        ));
                    }
                }
            }
        }
    }

    if !checkpoints.is_empty() {
        let (entered, exited) = summary_ancestry_intervals(&entries);
        for (prompt, checkpoint_head, checkpoint_line) in checkpoints {
            let prompt = entries[&prompt].position as usize;
            let checkpoint_head = entries[&checkpoint_head].position as usize;
            let prompt_is_ancestor = entered[prompt] <= entered[checkpoint_head]
                && exited[checkpoint_head] <= exited[prompt];
            if !prompt_is_ancestor {
                return Err(corrupt_summary(
                    checkpoint_line,
                    "checkpoint prompt is not an ancestor of its head",
                ));
            }
        }
    }

    let mut oldest_title = None;
    let mut cursor = head.as_ref();
    while let Some(id) = cursor {
        let Some(entry) = entries.get(id) else {
            break;
        };
        if entry.kind == SummaryEntryKind::User {
            if let Some(title) = &entry.title {
                oldest_title = Some(title.clone());
            }
        }
        cursor = entry.parent.as_ref();
    }
    Ok(oldest_title)
}

impl SessionStore {
    /// Create a store rooted at `<session_dir>/<workspace-key>`.
    pub fn new(session_dir: &Path, workspace: &Path) -> Self {
        Self {
            dir: session_dir.join(workspace_key(workspace)),
        }
    }

    /// The workspace-scoped session directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Allocate a new JSONL path. The caller supplies a timestamp for testability.
    pub fn new_path(&self, stamp: &str) -> PathBuf {
        let suffix = NEXT_SESSION_SUFFIX.fetch_add(1, Ordering::Relaxed);
        self.dir.join(format!("{stamp}-{suffix:04x}.jsonl"))
    }

    fn candidates(&self) -> Vec<SessionCandidate> {
        let mut candidates = std::fs::read_dir(&self.dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                let entry = entry.ok()?;
                if !entry.file_type().ok()?.is_file() {
                    return None;
                }
                let path = entry.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                    return None;
                }
                let modified = entry.metadata().ok()?.modified().ok()?;
                Some(SessionCandidate { path, modified })
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.modified));
        candidates
    }

    fn summarize(&self, candidate: SessionCandidate) -> Option<SessionMeta> {
        let fallback_title = match summarize_session(&candidate.path) {
            Ok(Some(title)) => title,
            Ok(None) => return None,
            Err(_) => "(unreadable session)".to_owned(),
        };
        let id = candidate
            .path
            .file_stem()
            .and_then(|value| value.to_str())?
            .to_owned();
        let metadata = self.load_metadata(&id).unwrap_or_default();
        let title = metadata
            .name
            .clone()
            .unwrap_or_else(|| fallback_title.clone());
        let modified = self
            .metadata_path(&id)
            .ok()
            .and_then(|path| path.symlink_metadata().ok())
            .filter(|metadata| metadata.file_type().is_file())
            .and_then(|metadata| metadata.modified().ok())
            .map_or(candidate.modified, |metadata_modified| {
                std::cmp::max(candidate.modified, metadata_modified)
            });
        Some(SessionMeta {
            id,
            path: candidate.path,
            title,
            name: metadata.name,
            tags: metadata.tags,
            pinned: metadata.pinned,
            archived: metadata.archived,
            trashed_at_ms: metadata.trashed_at_ms,
            purge_after_ms: metadata.purge_after_ms,
            forked_from_session_id: metadata.forked_from_session_id,
            forked_from_entry_id: metadata.forked_from_entry_id,
            modified,
        })
    }

    /// List sessions newest-first by filesystem modification time.
    pub fn list(&self) -> Vec<SessionMeta> {
        self.candidates()
            .into_iter()
            .filter_map(|candidate| self.summarize(candidate))
            .collect()
    }

    /// Return the newest session or an actionable error when none exists.
    pub fn latest(&self) -> anyhow::Result<SessionMeta> {
        self.candidates()
            .into_iter()
            .find_map(|candidate| self.summarize(candidate))
            .ok_or_else(|| anyhow::anyhow!("no sessions for this workspace yet"))
    }

    /// Resolve a filename stem without enumerating or parsing unrelated sessions.
    pub fn path_by_id(&self, id: &str) -> anyhow::Result<PathBuf> {
        if !session_id_is_valid(id) {
            anyhow::bail!("invalid session id {id:?}");
        }
        let path = self.dir.join(format!("{id}.jsonl"));
        let metadata = path
            .symlink_metadata()
            .map_err(|error| anyhow::anyhow!("session {id:?} was not found: {error}"))?;
        if !metadata.file_type().is_file() {
            anyhow::bail!("session {id:?} is not a regular file");
        }
        Ok(path)
    }

    fn metadata_dir(&self) -> PathBuf {
        self.dir.join(".metadata")
    }

    fn metadata_path(&self, id: &str) -> anyhow::Result<PathBuf> {
        if !session_id_is_valid(id) {
            anyhow::bail!("invalid session id {id:?}");
        }
        Ok(self.metadata_dir().join(format!("{id}.json")))
    }

    /// Read optional user-owned session catalog metadata.
    pub fn load_metadata(&self, id: &str) -> anyhow::Result<SessionUserMetadata> {
        let path = self.metadata_path(id)?;
        let bytes = match crate::auth::read_bounded_regular(&path, MAX_SESSION_METADATA_BYTES) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return Ok(SessionUserMetadata::default()),
            Err(error) => anyhow::bail!("cannot read session metadata {}: {error}", path.display()),
        };
        let parsed: SessionUserMetadata = serde_json::from_slice(&bytes).map_err(|error| {
            anyhow::anyhow!("invalid session metadata {}: {error}", path.display())
        })?;
        let metadata = SessionUserMetadata {
            name: parsed
                .name
                .as_deref()
                .map(sanitize_session_name)
                .transpose()?
                .flatten(),
            tags: sanitize_session_tags(&parsed.tags)?,
            pinned: parsed.pinned,
            archived: parsed.archived,
            trashed_at_ms: parsed.trashed_at_ms,
            purge_after_ms: parsed.purge_after_ms,
            forked_from_session_id: parsed.forked_from_session_id,
            forked_from_entry_id: parsed.forked_from_entry_id,
        };
        validate_session_metadata(&metadata)?;
        Ok(metadata)
    }

    /// Atomically replace user-owned catalog metadata. The target session must exist.
    pub fn save_metadata(&self, id: &str, metadata: &SessionUserMetadata) -> anyhow::Result<()> {
        self.path_by_id(id)?;
        let metadata = SessionUserMetadata {
            name: metadata
                .name
                .as_deref()
                .map(sanitize_session_name)
                .transpose()?
                .flatten(),
            tags: sanitize_session_tags(&metadata.tags)?,
            pinned: metadata.pinned,
            archived: metadata.archived,
            trashed_at_ms: metadata.trashed_at_ms,
            purge_after_ms: metadata.purge_after_ms,
            forked_from_session_id: metadata.forked_from_session_id.clone(),
            forked_from_entry_id: metadata.forked_from_entry_id.clone(),
        };
        validate_session_metadata(&metadata)?;
        let bytes = serde_json::to_vec_pretty(&metadata)?;
        if bytes.len() > MAX_SESSION_METADATA_BYTES {
            anyhow::bail!("session metadata exceeds {MAX_SESSION_METADATA_BYTES} bytes");
        }
        crate::auth::write_private_atomic(&self.metadata_path(id)?, &bytes, ".session-metadata-")
    }

    pub fn rename(&self, id: &str, name: &str) -> anyhow::Result<SessionUserMetadata> {
        let mut metadata = self.load_metadata(id)?;
        metadata.name = sanitize_session_name(name)?;
        self.save_metadata(id, &metadata)?;
        Ok(metadata)
    }

    pub fn set_tags(&self, id: &str, tags: Vec<String>) -> anyhow::Result<SessionUserMetadata> {
        let mut metadata = self.load_metadata(id)?;
        metadata.tags = sanitize_session_tags(&tags)?;
        self.save_metadata(id, &metadata)?;
        Ok(metadata)
    }

    #[cfg_attr(not(feature = "serve"), allow(dead_code))]
    pub fn set_pinned(&self, id: &str, pinned: bool) -> anyhow::Result<SessionUserMetadata> {
        let mut metadata = self.load_metadata(id)?;
        metadata.pinned = pinned;
        self.save_metadata(id, &metadata)?;
        Ok(metadata)
    }

    #[cfg_attr(not(feature = "serve"), allow(dead_code))]
    pub fn set_archived(&self, id: &str, archived: bool) -> anyhow::Result<SessionUserMetadata> {
        let mut metadata = self.load_metadata(id)?;
        metadata.archived = archived;
        metadata.trashed_at_ms = None;
        metadata.purge_after_ms = None;
        self.save_metadata(id, &metadata)?;
        Ok(metadata)
    }

    #[cfg_attr(not(feature = "serve"), allow(dead_code))]
    pub fn set_lifecycle(
        &self,
        id: &str,
        lifecycle: SessionStorageLifecycle,
        changed_at_ms: u64,
    ) -> anyhow::Result<SessionUserMetadata> {
        if changed_at_ms == 0 {
            anyhow::bail!("session lifecycle timestamp must be positive");
        }
        let mut metadata = self.load_metadata(id)?;
        match lifecycle {
            SessionStorageLifecycle::Active => {
                metadata.archived = false;
                metadata.trashed_at_ms = None;
                metadata.purge_after_ms = None;
            }
            SessionStorageLifecycle::Archived => {
                metadata.archived = true;
                metadata.trashed_at_ms = None;
                metadata.purge_after_ms = None;
            }
            SessionStorageLifecycle::Trash => {
                metadata.archived = true;
                metadata.pinned = false;
                if metadata.trashed_at_ms.is_none() {
                    metadata.trashed_at_ms = Some(changed_at_ms);
                    metadata.purge_after_ms = changed_at_ms.checked_add(SESSION_TRASH_RETENTION_MS);
                }
                if metadata.purge_after_ms.is_none() {
                    anyhow::bail!("session trash retention timestamp overflow");
                }
            }
        }
        self.save_metadata(id, &metadata)?;
        Ok(metadata)
    }

    #[cfg_attr(not(feature = "serve"), allow(dead_code))]
    pub fn set_fork_provenance(
        &self,
        id: &str,
        source_session_id: &str,
        source_entry_id: &str,
    ) -> anyhow::Result<SessionUserMetadata> {
        if !session_id_is_valid(source_session_id)
            || source_entry_id.is_empty()
            || source_entry_id.len() > 256
            || source_entry_id.chars().any(char::is_control)
        {
            anyhow::bail!("invalid session fork provenance");
        }
        let mut metadata = self.load_metadata(id)?;
        metadata.forked_from_session_id = Some(source_session_id.to_owned());
        metadata.forked_from_entry_id = Some(source_entry_id.to_owned());
        self.save_metadata(id, &metadata)?;
        Ok(metadata)
    }

    #[cfg_attr(not(feature = "serve"), allow(dead_code))]
    pub fn delete_permanently(&self, id: &str, expected_trashed_at_ms: u64) -> anyhow::Result<()> {
        let metadata = self.load_metadata(id)?;
        if metadata.trashed_at_ms != Some(expected_trashed_at_ms) {
            anyhow::bail!("session trash confirmation is stale");
        }
        let session_path = self.path_by_id(id)?;
        let metadata_path = self.metadata_path(id)?;
        let suffix = NEXT_SESSION_SUFFIX.fetch_add(1, Ordering::Relaxed);
        let staged_session = self.dir.join(format!(".delete-{id}-{suffix:016x}"));
        let staged_metadata = self
            .metadata_dir()
            .join(format!(".delete-{id}-{suffix:016x}"));

        std::fs::rename(&session_path, &staged_session)?;
        let metadata_present = metadata_path
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.file_type().is_file());
        if metadata_present {
            if let Err(error) = std::fs::rename(&metadata_path, &staged_metadata) {
                let _ = std::fs::rename(&staged_session, &session_path);
                return Err(error.into());
            }
        }
        if let Err(error) = std::fs::remove_file(&staged_session) {
            if metadata_present {
                let _ = std::fs::rename(&staged_metadata, &metadata_path);
            }
            let _ = std::fs::rename(&staged_session, &session_path);
            return Err(error.into());
        }
        if metadata_present {
            std::fs::remove_file(&staged_metadata)?;
        }
        Ok(())
    }

    /// Removes a just-created session and sidecar during a higher-level
    /// transaction rollback. This is intentionally not a user-facing delete
    /// path and must only be used before the new session is acknowledged.
    #[cfg_attr(not(feature = "serve"), allow(dead_code))]
    pub fn discard_unacknowledged(&self, id: &str) -> anyhow::Result<()> {
        let session_path = self.path_by_id(id)?;
        std::fs::remove_file(session_path)?;
        let metadata_path = self.metadata_path(id)?;
        match metadata_path.symlink_metadata() {
            Ok(metadata) if metadata.file_type().is_file() => {
                std::fs::remove_file(metadata_path)?;
            }
            Ok(_) => anyhow::bail!("session metadata path is not a regular file"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_workspace_dirs_are_stable_and_distinct() {
        let root = tempfile::tempdir().unwrap();
        let workspace_a = tempfile::tempdir().unwrap();
        let workspace_b = tempfile::tempdir().unwrap();
        let first = SessionStore::new(root.path(), workspace_a.path());
        let second = SessionStore::new(root.path(), workspace_a.path());
        let other = SessionStore::new(root.path(), workspace_b.path());
        assert_eq!(first.dir(), second.dir());
        assert_ne!(first.dir(), other.dir());
        assert!(first.dir().starts_with(root.path()));
    }

    #[test]
    fn new_path_is_inside_dir_with_jsonl_extension_and_prefix() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path(), workspace.path());
        let path = store.new_path("2026-07-12T14-30-05Z");
        assert!(path.starts_with(store.dir()));
        assert_eq!(path.extension().and_then(|ext| ext.to_str()), Some("jsonl"));
        assert!(path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("2026-07-12T14-30-05Z-")));
    }

    #[test]
    fn catalog_metadata_round_trips_without_rewriting_the_session() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path(), workspace.path());
        std::fs::create_dir_all(store.dir()).unwrap();
        let path = store.dir().join("metadata.jsonl");
        let mut session = Session::create(&path).unwrap();
        session
            .append(EntryValue::Message(Message::User(ygg_ai::UserMessage {
                content: vec![UserPart::Text("original title".into())],
            })))
            .unwrap();
        drop(session);
        let session_bytes = std::fs::read(&path).unwrap();

        store
            .set_tags("metadata", vec!["work".into(), "active".into()])
            .unwrap();
        store.rename("metadata", "  Renamed session  ").unwrap();
        store.set_pinned("metadata", true).unwrap();
        store.set_archived("metadata", true).unwrap();

        let reopened = SessionStore::new(root.path(), workspace.path());
        let metadata = reopened.load_metadata("metadata").unwrap();
        assert_eq!(metadata.name.as_deref(), Some("Renamed session"));
        assert_eq!(metadata.tags, ["work", "active"]);
        assert!(metadata.pinned);
        assert!(metadata.archived);
        let listed = reopened.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].title, "Renamed session");
        assert!(listed[0].pinned);
        assert!(listed[0].archived);
        assert_eq!(std::fs::read(path).unwrap(), session_bytes);
    }

    #[test]
    fn trash_lifecycle_is_recoverable_and_preserves_its_retention_deadline() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path(), workspace.path());
        std::fs::create_dir_all(store.dir()).unwrap();
        let path = store.dir().join("lifecycle.jsonl");
        let mut session = Session::create(&path).unwrap();
        session
            .append(EntryValue::Message(Message::User(ygg_ai::UserMessage {
                content: vec![UserPart::Text("recover me".into())],
            })))
            .unwrap();
        drop(session);
        store.set_pinned("lifecycle", true).unwrap();

        let trashed = store
            .set_lifecycle("lifecycle", SessionStorageLifecycle::Trash, 1_000)
            .unwrap();
        assert!(trashed.archived);
        assert!(!trashed.pinned);
        assert_eq!(trashed.trashed_at_ms, Some(1_000));
        assert_eq!(
            trashed.purge_after_ms,
            Some(1_000 + SESSION_TRASH_RETENTION_MS)
        );

        let repeated = store
            .set_lifecycle("lifecycle", SessionStorageLifecycle::Trash, 9_000)
            .unwrap();
        assert_eq!(repeated.trashed_at_ms, trashed.trashed_at_ms);
        assert_eq!(repeated.purge_after_ms, trashed.purge_after_ms);
        let listed = store.list();
        assert_eq!(listed[0].trashed_at_ms, Some(1_000));
        assert_eq!(
            listed[0].purge_after_ms,
            Some(1_000 + SESSION_TRASH_RETENTION_MS)
        );

        let restored = store
            .set_lifecycle("lifecycle", SessionStorageLifecycle::Active, 10_000)
            .unwrap();
        assert!(!restored.archived);
        assert_eq!(restored.trashed_at_ms, None);
        assert_eq!(restored.purge_after_ms, None);

        let archived = store
            .set_lifecycle("lifecycle", SessionStorageLifecycle::Archived, 11_000)
            .unwrap();
        assert!(archived.archived);
        assert_eq!(archived.trashed_at_ms, None);
        assert!(path.is_file());
    }

    #[test]
    fn permanent_delete_requires_the_current_trash_confirmation() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path(), workspace.path());
        std::fs::create_dir_all(store.dir()).unwrap();
        let path = store.dir().join("delete-me.jsonl");
        let mut session = Session::create(&path).unwrap();
        session
            .append(EntryValue::Message(Message::User(ygg_ai::UserMessage {
                content: vec![UserPart::Text("delete me".into())],
            })))
            .unwrap();
        drop(session);
        store
            .set_lifecycle("delete-me", SessionStorageLifecycle::Trash, 2_000)
            .unwrap();
        let metadata_path = store.metadata_path("delete-me").unwrap();

        let error = store.delete_permanently("delete-me", 1_999).unwrap_err();
        assert!(error.to_string().contains("confirmation is stale"));
        assert!(path.is_file());
        assert!(metadata_path.is_file());

        store.delete_permanently("delete-me", 2_000).unwrap();
        assert!(!path.exists());
        assert!(!metadata_path.exists());
        assert!(store.path_by_id("delete-me").is_err());
    }

    #[test]
    fn fork_provenance_round_trips_as_an_atomic_pair() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path(), workspace.path());
        std::fs::create_dir_all(store.dir()).unwrap();
        let path = store.dir().join("fork.jsonl");
        let mut session = Session::create(path).unwrap();
        session
            .append(EntryValue::Message(Message::User(ygg_ai::UserMessage {
                content: vec![UserPart::Text("fork".into())],
            })))
            .unwrap();
        drop(session);

        let metadata = store
            .set_fork_provenance("fork", "source-session", "0042")
            .unwrap();
        assert_eq!(
            metadata.forked_from_session_id.as_deref(),
            Some("source-session")
        );
        assert_eq!(metadata.forked_from_entry_id.as_deref(), Some("0042"));
        let listed = store.list();
        assert_eq!(
            listed[0].forked_from_session_id.as_deref(),
            Some("source-session")
        );
        assert_eq!(listed[0].forked_from_entry_id.as_deref(), Some("0042"));

        let invalid = SessionUserMetadata {
            forked_from_session_id: Some("source-session".into()),
            ..SessionUserMetadata::default()
        };
        assert!(store.save_metadata("fork", &invalid).is_err());
    }

    #[test]
    fn latest_returns_newest_by_mtime() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path(), workspace.path());
        std::fs::create_dir_all(store.dir()).unwrap();
        let older_path = store.dir().join("2026-01-01T00-00-00Z-aaaa.jsonl");
        let mut older = Session::create(&older_path).unwrap();
        older
            .append(EntryValue::Message(Message::User(ygg_ai::UserMessage {
                content: vec![UserPart::Text("older".into())],
            })))
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(15));
        let newer_path = store.dir().join("2026-02-02T00-00-00Z-bbbb.jsonl");
        let mut newer = Session::create(&newer_path).unwrap();
        newer
            .append(EntryValue::Message(Message::User(ygg_ai::UserMessage {
                content: vec![UserPart::Text("newer".into())],
            })))
            .unwrap();
        assert_eq!(store.latest().unwrap().path, newer_path);
    }

    #[test]
    fn latest_skips_a_newer_config_only_session() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path(), workspace.path());
        std::fs::create_dir_all(store.dir()).unwrap();
        let older_path = store.dir().join("conversation.jsonl");
        let mut older = Session::create(&older_path).unwrap();
        older
            .append(EntryValue::Message(Message::User(ygg_ai::UserMessage {
                content: vec![UserPart::Text("resumable".into())],
            })))
            .unwrap();
        drop(older);
        std::thread::sleep(std::time::Duration::from_millis(15));
        let newer_path = store.dir().join("config-only.jsonl");
        let mut newer = Session::create(&newer_path).unwrap();
        newer
            .append(EntryValue::Config {
                model: Some("model".into()),
                reasoning: None,
                reasoning_mode: None,
            })
            .unwrap();

        let latest = store.latest().unwrap();
        assert_eq!(latest.path, older_path);
        assert_eq!(latest.title, "resumable");
    }

    #[test]
    fn active_branch_title_uses_oldest_active_user_text() {
        use ygg_agent::{EntryValue, Session};
        use ygg_ai::{
            AssistantMessage, AssistantPart, Message, ModelId, Protocol, UserMessage, UserPart,
        };

        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("session.jsonl")).unwrap();
        let root = session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Text("active title".into())],
            })))
            .unwrap();
        session
            .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::Text("abandoned".into())],
                model: ModelId("m".into()),
                protocol: Protocol::OpenAiChat,
            })))
            .unwrap();
        session.checkout(root).unwrap();
        session
            .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::Text("active".into())],
                model: ModelId("m".into()),
                protocol: Protocol::OpenAiChat,
            })))
            .unwrap();
        assert_eq!(active_branch_title(&session), "active title");
    }

    #[test]
    fn title_normalization_is_bounded_and_unicode_aware() {
        assert_eq!(trim_title("  one\n\ttwo  "), "one two");
        assert_eq!(
            trim_title(&format!("{}   ", "é".repeat(60))),
            "é".repeat(60)
        );
        assert_eq!(
            trim_title(&format!("{} next", "é".repeat(60))),
            format!("{}…", "é".repeat(60))
        );
        assert_eq!(trim_title(&"a".repeat(61)), format!("{}…", "a".repeat(60)));
    }

    #[test]
    fn listing_is_byte_for_byte_read_only_even_for_a_torn_tail() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path(), workspace.path());
        std::fs::create_dir_all(store.dir()).unwrap();
        let path = store.dir().join("torn.jsonl");
        let mut session = Session::create(&path).unwrap();
        session
            .append(EntryValue::Message(Message::User(ygg_ai::UserMessage {
                content: vec![UserPart::Text("durable title".into())],
            })))
            .unwrap();
        drop(session);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(b"{\"type\":\"entry\"");
        std::fs::write(&path, &bytes).unwrap();

        assert_eq!(store.list().len(), 1);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn listing_accepts_invalid_utf8_only_in_the_unterminated_tail() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path(), workspace.path());
        std::fs::create_dir_all(store.dir()).unwrap();
        let path = store.dir().join("utf8-tail.jsonl");
        let mut session = Session::create(&path).unwrap();
        session
            .append(EntryValue::Message(Message::User(ygg_ai::UserMessage {
                content: vec![UserPart::Text("durable title".into())],
            })))
            .unwrap();
        drop(session);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(b"{\"text\":\"");
        bytes.extend_from_slice(&[0xf0, 0x9f]);
        std::fs::write(&path, &bytes).unwrap();

        let listed = store.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].title, "durable title");
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn lightweight_summary_rejects_invalid_utf8_in_a_completed_record() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path(), workspace.path());
        std::fs::create_dir_all(store.dir()).unwrap();
        let path = store.dir().join("utf8-corrupt.jsonl");
        let mut session = Session::create(&path).unwrap();
        session
            .append(EntryValue::Message(Message::User(ygg_ai::UserMessage {
                content: vec![UserPart::Text("durable title".into())],
            })))
            .unwrap();
        drop(session);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(&[0xff, b'\n']);
        std::fs::write(&path, &bytes).unwrap();

        let error = summarize_session(&path).unwrap_err();
        assert!(error.to_string().contains("line 3"), "{error:#}");
        assert!(error.to_string().contains("invalid UTF-8"), "{error:#}");
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn lightweight_summary_rejects_a_malformed_completed_final_record() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path(), workspace.path());
        std::fs::create_dir_all(store.dir()).unwrap();
        let path = store.dir().join("corrupt.jsonl");
        let mut session = Session::create(&path).unwrap();
        session
            .append(EntryValue::Message(Message::User(ygg_ai::UserMessage {
                content: vec![UserPart::Text("durable title".into())],
            })))
            .unwrap();
        drop(session);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(b"{\"type\":\"entry\"\n");
        std::fs::write(&path, &bytes).unwrap();

        let error = summarize_session(&path).unwrap_err();
        assert!(error.to_string().contains("line 3"), "{error:#}");
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn lightweight_summary_rejects_a_cross_branch_checkpoint() {
        use std::io::Write as _;

        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path(), workspace.path());
        std::fs::create_dir_all(store.dir()).unwrap();
        let path = store.dir().join("cross-branch.jsonl");
        let mut session = Session::create(&path).unwrap();
        let root_entry = session
            .append(EntryValue::Message(Message::User(ygg_ai::UserMessage {
                content: vec![UserPart::Text("root".into())],
            })))
            .unwrap();
        let abandoned_prompt = session
            .append(EntryValue::Message(Message::User(ygg_ai::UserMessage {
                content: vec![UserPart::Text("abandoned".into())],
            })))
            .unwrap();
        session
            .append(EntryValue::Message(Message::Assistant(
                ygg_ai::AssistantMessage {
                    content: vec![ygg_ai::AssistantPart::Text("old answer".into())],
                    model: ygg_ai::ModelId("model".into()),
                    protocol: ygg_ai::Protocol::OpenAiChat,
                },
            )))
            .unwrap();
        session.checkout(root_entry).unwrap();
        session
            .append(EntryValue::Message(Message::User(ygg_ai::UserMessage {
                content: vec![UserPart::Text("active".into())],
            })))
            .unwrap();
        let active_head = session
            .append(EntryValue::Message(Message::Assistant(
                ygg_ai::AssistantMessage {
                    content: vec![ygg_ai::AssistantPart::Text("new answer".into())],
                    model: ygg_ai::ModelId("model".into()),
                    protocol: ygg_ai::Protocol::OpenAiChat,
                },
            )))
            .unwrap();
        drop(session);

        let record = ygg_agent::SessionRecord::Checkpoint {
            prompt: abandoned_prompt,
            head: active_head,
            usage: None,
            run_cost_microdollars: None,
        };
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        serde_json::to_writer(&mut file, &record).unwrap();
        file.write_all(b"\n").unwrap();
        drop(file);

        let error = summarize_session(&path).unwrap_err();
        assert!(error.to_string().contains("line 12"), "{error:#}");
        assert!(error.to_string().contains("not an ancestor"), "{error:#}");
    }

    #[test]
    fn lightweight_summary_validates_responses_sidecar_structure() {
        use std::io::Write as _;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("bad-responses-turn.jsonl");
        let mut session = Session::create(&path).unwrap();
        session
            .append(EntryValue::Message(Message::User(ygg_ai::UserMessage {
                content: vec![UserPart::Text("title".into())],
            })))
            .unwrap();
        let assistant = session
            .append(EntryValue::Message(Message::Assistant(
                ygg_ai::AssistantMessage {
                    content: vec![ygg_ai::AssistantPart::Text("answer".into())],
                    model: ModelId("model-a".into()),
                    protocol: Protocol::OpenAiResponses,
                },
            )))
            .unwrap();
        drop(session);

        let malformed = serde_json::json!({
            "type": "entry",
            "id": "999",
            "parent": assistant,
            "value": {
                "type": "responses_turn",
                "assistant": assistant,
                "endpoint": "responses",
                "model": "model-b",
                "output": [{"type": "message"}]
            }
        });
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        serde_json::to_writer(&mut file, &malformed).unwrap();
        file.write_all(b"\n").unwrap();
        drop(file);

        let error = summarize_session(&path).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("is not a direct sidecar of assistant"),
            "{error:#}"
        );

        let compact_path = directory.path().join("bad-responses-compact.jsonl");
        let mut session = Session::create(&compact_path).unwrap();
        let first = session
            .append(EntryValue::Message(Message::User(ygg_ai::UserMessage {
                content: vec![UserPart::Text("compact title".into())],
            })))
            .unwrap();
        let second = session
            .append(EntryValue::Config {
                model: None,
                reasoning: None,
                reasoning_mode: None,
            })
            .unwrap();
        drop(session);
        let malformed = serde_json::json!({
            "type": "entry",
            "id": "999",
            "parent": second,
            "value": {
                "type": "responses_compaction",
                "endpoint": "responses",
                "model": "model-a",
                "covered_through": first,
                "output": [{"type": "compaction"}]
            }
        });
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&compact_path)
            .unwrap();
        serde_json::to_writer(&mut file, &malformed).unwrap();
        file.write_all(b"\n").unwrap();
        drop(file);
        let error = summarize_session(&compact_path).unwrap_err();
        assert!(
            error.to_string().contains("is not a direct checkpoint"),
            "{error:#}"
        );
    }

    #[test]
    fn lightweight_responses_output_matches_full_compaction_validation() {
        let cases = [
            serde_json::json!([]),
            serde_json::json!([{"type": "message", "content": "ignored"}]),
            serde_json::json!([{"type": "compaction"}]),
            serde_json::json!([{"type": "compaction", "encrypted_content": ""}]),
            serde_json::json!([{"type": "compaction", "encrypted_content": 42}]),
            serde_json::json!([{"type": "compaction", "encrypted_content": "opaque"}]),
            serde_json::json!([
                {"type": "message", "future": {"large": [1, 2, 3]}},
                {"type": "compaction", "encrypted_content": "opaque"}
            ]),
            serde_json::json!([
                {"type": "compaction", "encrypted_content": "one"},
                {"type": "compaction", "encrypted_content": "two"}
            ]),
        ];

        for value in cases {
            let summary: SummaryResponsesOutput = serde_json::from_value(value.clone()).unwrap();
            let full: ygg_ai::ResponsesOutput = serde_json::from_value(value).unwrap();
            assert_eq!(summary.is_empty(), full.is_empty());
            assert_eq!(summary.has_valid_compaction(), full.has_valid_compaction());
        }
    }

    #[test]
    fn lightweight_summary_accepts_non_assistant_usage_records() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("usage-kinds.jsonl");
        let mut session = Session::create(&path).unwrap();
        session
            .append(EntryValue::Message(Message::User(ygg_ai::UserMessage {
                content: vec![UserPart::Text("usage title".into())],
            })))
            .unwrap();
        session
            .record_rejected_responses_turn_usage(
                ygg_ai::EndpointId("responses".into()),
                ModelId("model".into()),
                ygg_ai::Usage::default(),
                None,
            )
            .unwrap();
        session
            .record_terminal_gate_usage(
                ygg_ai::EndpointId("responses".into()),
                ModelId("model".into()),
                ygg_ai::Usage::default(),
                None,
                None,
            )
            .unwrap();
        drop(session);

        assert_eq!(
            summarize_session(&path).unwrap().as_deref(),
            Some("usage title")
        );
    }

    #[test]
    fn list_omits_empty_and_config_only_sessions() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path(), workspace.path());
        std::fs::create_dir_all(store.dir()).unwrap();
        let _empty = Session::create(store.dir().join("empty.jsonl")).unwrap();
        let mut config_only = Session::create(store.dir().join("config.jsonl")).unwrap();
        config_only
            .append(EntryValue::Config {
                model: Some("model".into()),
                reasoning: Some("high".into()),
                reasoning_mode: None,
            })
            .unwrap();

        assert!(store.list().is_empty());
    }

    #[test]
    fn lightweight_listing_matches_the_active_branch_and_ignores_large_bodies() {
        use ygg_ai::{
            AssistantMessage, AssistantPart, ModelId, Protocol, ToolCallId, ToolResult,
            ToolResultPart,
        };

        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path(), workspace.path());
        std::fs::create_dir_all(store.dir()).unwrap();
        let path = store.dir().join("large.jsonl");
        let mut session = Session::create(&path).unwrap();
        session
            .append(EntryValue::Message(Message::User(ygg_ai::UserMessage {
                content: vec![UserPart::Text(
                    "  title   with whitespace that the picker normalizes  ".into(),
                )],
            })))
            .unwrap();
        session
            .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::Text("x".repeat(2 * 1024 * 1024))],
                model: ModelId("model".into()),
                protocol: Protocol::OpenAiChat,
            })))
            .unwrap();
        session
            .append(EntryValue::Message(Message::User(ygg_ai::UserMessage {
                content: vec![UserPart::ToolResult(ToolResult {
                    tool_call_id: ToolCallId("call-1".into()),
                    content: vec![ToolResultPart::Text("y".repeat(2 * 1024 * 1024))],
                    is_error: false,
                })],
            })))
            .unwrap();
        let expected = active_branch_title(&session);
        drop(session);

        let listed = store.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].title, expected);
        assert_eq!(
            listed[0].title,
            "title with whitespace that the picker normalizes"
        );
    }

    #[test]
    fn listing_scales_across_many_session_files() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path(), workspace.path());
        std::fs::create_dir_all(store.dir()).unwrap();
        let template_path = store.dir().join("session-0000.jsonl");
        let mut template = Session::create(&template_path).unwrap();
        template
            .append(EntryValue::Message(Message::User(ygg_ai::UserMessage {
                content: vec![UserPart::Text("scale fixture".into())],
            })))
            .unwrap();
        drop(template);
        let bytes = std::fs::read(&template_path).unwrap();
        for index in 1..512 {
            std::fs::write(
                store.dir().join(format!("session-{index:04}.jsonl")),
                &bytes,
            )
            .unwrap();
        }

        let listed = store.list();
        assert_eq!(listed.len(), 512);
        assert!(listed
            .iter()
            .all(|session| session.title == "scale fixture"));
    }

    #[test]
    fn path_by_id_resolves_only_a_valid_direct_regular_file() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path(), workspace.path());
        std::fs::create_dir_all(store.dir()).unwrap();
        let one_path = store.dir().join("one.jsonl");
        let mut session = Session::create(&one_path).unwrap();
        session
            .append(EntryValue::Message(Message::User(ygg_ai::UserMessage {
                content: vec![UserPart::Text("one".into())],
            })))
            .unwrap();
        std::fs::write(store.dir().join("one.txt"), b"").unwrap();
        std::fs::write(
            store.dir().join("unrelated.jsonl"),
            b"not-json\nstill-not-json\n",
        )
        .unwrap();
        std::fs::create_dir(store.dir().join("directory.jsonl")).unwrap();

        assert_eq!(store.path_by_id("one").unwrap(), one_path);
        for invalid in ["", ".", "..", "../one", "one/two", "one\n"] {
            assert!(store.path_by_id(invalid).is_err(), "accepted {invalid:?}");
        }
        assert!(store.path_by_id("directory").is_err());

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&one_path, store.dir().join("linked.jsonl")).unwrap();
            assert!(store.path_by_id("linked").is_err());
            assert!(!store.list().iter().any(|session| {
                session.path.file_stem().and_then(|stem| stem.to_str()) == Some("linked")
            }));
        }
    }
}
