//! One concrete append-only JSONL session: parent-linked entries, durable
//! head records, branching via checkout, and context reconstruction.
//!
//! The session *is* the conversation history model — there is no separate
//! store trait or conversation manager. Only semantic boundaries are
//! persisted (complete user messages, complete assistant messages, individual
//! tool results, config markers, compaction records); streaming deltas never
//! touch disk.
//!
//! # Crash semantics
//!
//! Every append writes the entry record and a head record to the append-only
//! file before returning. This makes records **process-crash safe**: once
//! `append` returns, the bytes are in the kernel and survive the process
//! dying. Every semantic record is followed by `sync_data` before success is
//! returned, so completed session commits survive ordinary OS crashes and
//! power loss subject to the filesystem's durability guarantees.
//!
//! After an unclean exit the file may end in a torn final line;
//! [`Session::open`] drops (and truncates away) an unparseable *final* line
//! and resumes from the last recorded head, while corruption in any earlier
//! (completed) record is rejected. Because there is an unavoidable window
//! between a tool's external side effect and the write of its result entry,
//! unresolved mutating calls are reported as **indeterminate** after an
//! unclean crash and are not automatically replayed. Read-only tools may opt
//! into safe replay. This avoids claiming exactly-once execution while also
//! avoiding silent at-least-once mutations.

use std::cell::{Ref, RefCell};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::path::PathBuf;

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use ygg_ai::{
    Cost, EndpointId, Message, ModelId, Usage, UserMessage, UserPart, PICODOLLARS_PER_MICRODOLLAR,
};

/// Identifier of a session entry. Unique within one session file.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct EntryId(pub String);

/// A durable restore point written after one submitted prompt completes.
///
/// `prompt` identifies the user entry that began the completed interaction;
/// `head` is the exact session entry restored by [`Session::restore_checkpoint`].
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Checkpoint {
    /// User-message entry that began this completed interaction.
    pub prompt: EntryId,
    /// Session head after the interaction completed.
    pub head: EntryId,
    /// Provider-reported cumulative usage for this completed interaction.
    #[serde(default)]
    pub usage: Option<Usage>,
    /// Cost accrued by this completed interaction, including explicit zero.
    #[serde(default)]
    pub run_cost_microdollars: Option<u64>,
}

/// The operation to which a durable usage record belongs.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UsageRecordKind {
    /// One completed provider turn persisted as an assistant message.
    AssistantTurn {
        /// The assistant entry that received the provider response.
        assistant: EntryId,
    },
    /// A tool-free call used to produce a context-compaction summary.
    Compaction,
    /// A bounded one-token decision about whether a candidate response may
    /// return control to the user. `None` records a billable malformed answer.
    TerminalGate {
        /// `Some(true)` returns, `Some(false)` continues, and `None` is invalid.
        returned: Option<bool>,
    },
}

/// Provider usage and cost recorded for one durable operation.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct UsageRecord {
    /// The operation that produced this usage.
    pub kind: UsageRecordKind,
    /// Provider-reported, disjoint token buckets.
    pub usage: Usage,
    /// Endpoint/provider route used for this operation.
    #[serde(default)]
    pub endpoint: Option<EndpointId>,
    /// Canonical selected model used for this operation.
    #[serde(default)]
    pub model: Option<ModelId>,
    /// Completion wall-clock time in milliseconds since the Unix epoch.
    #[serde(default)]
    pub completed_at_unix_ms: Option<u64>,
    /// Per-category request cost when pricing was available.
    #[serde(default)]
    pub cost: Option<Cost>,
    /// Request total retained explicitly for lightweight readers and backwards
    /// compatibility with the first usage-record format.
    #[serde(default)]
    pub cost_microdollars: Option<u64>,
    /// Cumulative whole-microdollar session cost after this operation. Keeping
    /// it on the same JSONL record makes usage and accounting one durable
    /// update rather than two crash-separable writes.
    #[serde(default)]
    pub session_cost_microdollars: Option<u64>,
    /// Picodollar remainder paired with `session_cost_microdollars`.
    #[serde(default)]
    pub session_cost_picodollars_remainder: Option<u32>,
}

/// Stable presentation metadata attached to a durable session entry.
///
/// Values are inert data, never terminal escape sequences. In addition to the
/// semantic model identity, a user prompt may retain the exact sRGB highlight
/// assigned when it was appended. Persisting that value keeps old prompts
/// visually immutable across model and theme changes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryMetadata {
    /// Canonical model that received a user prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_model: Option<ModelId>,
    /// Stable model creator/source key (for example `openai` or `deepseek`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_model_source: Option<String>,
    /// Exact normalized sRGB highlight assigned to this prompt (`#rrggbb`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_color: Option<String>,
    /// User-visible transcript text when model-only prompt composition added
    /// context around the submitted draft. The message body remains the exact
    /// replayable model input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_text: Option<String>,
}

impl EntryMetadata {
    fn sanitized(mut self) -> Option<Self> {
        self.prompt_model = self.prompt_model.filter(|model| {
            !model.0.is_empty() && !model.0.chars().any(|character| character.is_control())
        });
        self.prompt_model_source = self.prompt_model_source.and_then(|source| {
            let source = source.trim();
            (!source.is_empty()
                && source.chars().all(|character| {
                    character.is_ascii_alphanumeric()
                        || matches!(character, '-' | '_' | '.' | ':' | '/')
                }))
            .then(|| source.to_owned())
        });
        self.prompt_color = self.prompt_color.and_then(|color| {
            let color = color.trim();
            let digits = color.strip_prefix('#')?;
            (digits.len() == 6 && digits.bytes().all(|byte| byte.is_ascii_hexdigit()))
                .then(|| format!("#{}", digits.to_ascii_lowercase()))
        });
        self.display_text = self.display_text.and_then(|text| {
            (text.len() <= 256 * 1024
                && !text
                    .chars()
                    .any(|character| character.is_control() && !matches!(character, '\n' | '\t')))
            .then_some(text)
        });
        (self.prompt_model.is_some()
            || self.prompt_model_source.is_some()
            || self.prompt_color.is_some()
            || self.display_text.is_some())
        .then_some(self)
    }
}

/// A parent-linked session entry. `parent: None` marks a root entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    /// This entry's ID.
    pub id: EntryId,
    /// The entry this one follows; `None` for a conversation root.
    pub parent: Option<EntryId>,
    /// Stable presentation metadata. Legacy sessions omit this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<EntryMetadata>,
    /// The payload.
    pub value: EntryValue,
}

/// Payload of a session entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EntryValue {
    /// A complete conversation message (user, assistant, or tool results
    /// carried as a user message).
    Message(Message),
    /// A manual compaction record: everything on the parent chain older than
    /// `first_kept` is replaced by `summary` during context reconstruction.
    Compaction {
        /// Caller-provided summary of the replaced history.
        summary: String,
        /// The oldest entry still kept in full-fidelity context.
        first_kept: EntryId,
        /// Snapshots of active skills at the compaction boundary.
        #[serde(default)]
        active_skills: Vec<SkillActivatedSnapshot>,
        /// Snapshots of lazy resource reads active at the compaction boundary.
        #[serde(default)]
        skill_resources: Vec<SkillResourceSnapshot>,
        /// Pi-compatible cumulative read/modified file lists for handoff.
        #[serde(default)]
        details: crate::compaction::CompactionDetails,
    },
    /// A configuration marker (not part of model-visible context).
    Config {
        /// Model selection recorded at this point, if any.
        model: Option<String>,
        /// Reasoning setting recorded at this point, if any.
        reasoning: Option<String>,
        /// Reasoning execution mode recorded at this point, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_mode: Option<String>,
    },
    /// A named prompt template was expanded before a user prompt. Keeping this
    /// as a non-model-visible append-only marker makes template provenance
    /// inspectable without changing the submitted text or replay semantics.
    PromptTemplateSelected {
        /// Stable template name used by the command or CLI option.
        name: String,
        /// SHA-256 of the complete template file, including frontmatter.
        content_hash: String,
    },
    /// A skill was explicitly activated.
    SkillActivated {
        /// The skill metadata descriptor.
        descriptor: crate::skills::SkillDescriptor,
        /// Deterministic content hash of SKILL.md.
        instructions_hash: crate::skills::ContentHash,
        /// Raw core instructions content.
        instructions: String,
    },
    /// A resource associated with an active skill was loaded.
    SkillResourceRead {
        /// The unique ID of the activation that read this resource.
        activation_id: crate::skills::SkillActivationId,
        /// The unique ID of the skill.
        skill_id: crate::skills::SkillId,
        /// Relative path of the resource (e.g. "references/semver.md").
        resource_path: String,
        /// The optional start line.
        start_line: Option<u32>,
        /// The optional line count.
        line_count: Option<u32>,
        /// Content hash of the retrieved text.
        content_hash: crate::skills::ContentHash,
        /// Text content of the resource range.
        content: String,
    },
    /// A skill was explicitly deactivated.
    SkillDeactivated {
        /// The activation identifier that is being deactivated.
        activation_id: crate::skills::SkillActivationId,
        /// The unique ID of the skill.
        skill_id: crate::skills::SkillId,
    },
}

/// Snapshot of an activated skill, used in compaction records.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SkillActivatedSnapshot {
    /// The activation ID (which corresponds to the EntryId of the activation event).
    pub activation_id: crate::skills::SkillActivationId,
    /// The skill metadata descriptor.
    pub descriptor: crate::skills::SkillDescriptor,
    /// Deterministic content hash of the skill instructions.
    pub instructions_hash: crate::skills::ContentHash,
    /// Raw core instructions content.
    pub instructions: String,
}

/// Snapshot of a loaded skill resource, used in compaction records.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SkillResourceSnapshot {
    /// The activation ID.
    pub activation_id: crate::skills::SkillActivationId,
    /// The skill ID.
    pub skill_id: crate::skills::SkillId,
    /// The resource path.
    pub resource_path: String,
    /// Optional start line.
    pub start_line: Option<u32>,
    /// Optional line count.
    pub line_count: Option<u32>,
    /// Content hash of the resource text.
    pub content_hash: crate::skills::ContentHash,
    /// Raw text content.
    pub content: String,
}

/// One line of the session JSONL file.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionRecord {
    /// An appended entry.
    Entry(Box<Entry>),
    /// A durable head update: the current head entry ID and cumulative cost.
    Head {
        /// The entry the head now points at.
        id: EntryId,
        /// Cumulative whole-microdollar session cost.
        #[serde(default)]
        total_cost_microdollars: u64,
        /// Picodollar remainder paired with `total_cost_microdollars`.
        #[serde(default)]
        total_cost_picodollars_remainder: u32,
    },
    /// A completed prompt's durable restore point. Checkpoints do not alter
    /// the active head or model-visible context.
    Checkpoint {
        /// User-message entry that began the completed interaction.
        prompt: EntryId,
        /// Exact completed head to restore.
        head: EntryId,
        /// Provider-reported cumulative usage for this interaction.
        #[serde(default)]
        usage: Option<Usage>,
        /// Cost accrued by this interaction; `None` for legacy records.
        #[serde(default)]
        run_cost_microdollars: Option<u64>,
    },
    /// Usage for one assistant turn or compaction operation. This does not
    /// alter the active head or model-visible context.
    Usage {
        /// The durable usage record.
        record: UsageRecord,
    },
}

/// Borrowed serialization view used by append/checkout. Keeping the entry
/// borrowed avoids cloning large message payloads solely to write JSONL.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SessionRecordRef<'a> {
    Entry(&'a Entry),
    Head {
        id: &'a EntryId,
        total_cost_microdollars: &'a u64,
        total_cost_picodollars_remainder: &'a u32,
    },
    Checkpoint {
        prompt: &'a EntryId,
        head: &'a EntryId,
        usage: &'a Option<Usage>,
        run_cost_microdollars: &'a Option<u64>,
    },
    Usage {
        record: &'a UsageRecord,
    },
}

fn write_json_line<T: Serialize>(buf: &mut Vec<u8>, record: &T) -> Result<(), SessionError> {
    serde_json::to_writer(&mut *buf, record).map_err(|e| SessionError::Serde(e.to_string()))?;
    buf.push(b'\n');
    Ok(())
}

const MAX_SESSION_FILE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_SESSION_RECORDS: usize = 1_000_000;

/// Decode every completed JSONL record strictly while allowing an interrupted
/// append to end in arbitrary bytes. A crash can tear a write in the middle of
/// a multibyte UTF-8 scalar, so decoding the complete file before locating its
/// final unterminated segment would incorrectly make that recoverable tail
/// fatal.
fn session_text_with_torn_tail(bytes: &[u8]) -> Result<&str, SessionError> {
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
        return Err(SessionError::Corrupt {
            line,
            message: format!("invalid UTF-8: {error}"),
        });
    }

    match std::str::from_utf8(bytes) {
        Ok(content) => Ok(content),
        // The completed prefix was validated above, so any remaining UTF-8
        // error belongs exclusively to the final unterminated write.
        Err(_) => Ok(std::str::from_utf8(&bytes[..completed_end])
            .expect("the completed session prefix was validated above")),
    }
}

/// Build DFS entry/exit times for the parent-linked entry forest in linear
/// time. Parent records are guaranteed to precede children during replay, but
/// branches may be interleaved in insertion order, so numeric entry positions
/// alone cannot answer ancestry queries.
fn entry_ancestry_intervals(
    entries: &[Entry],
    index: &HashMap<EntryId, usize>,
) -> (Vec<u32>, Vec<u32>) {
    const NONE: u32 = u32::MAX;

    let mut first_child = vec![NONE; entries.len()];
    let mut next_sibling = vec![NONE; entries.len()];
    for (child, entry) in entries.iter().enumerate() {
        let Some(parent) = entry.parent.as_ref() else {
            continue;
        };
        let parent = *index
            .get(parent)
            .expect("replay validates every parent before building ancestry");
        let child = u32::try_from(child).expect("session record limit fits u32");
        next_sibling[child as usize] = first_child[parent];
        first_child[parent] = child;
    }

    let mut entered = vec![0u32; entries.len()];
    let mut exited = vec![0u32; entries.len()];
    let mut clock = 0u32;
    let mut stack = Vec::<(u32, bool)>::new();
    for (root, entry) in entries.iter().enumerate() {
        if entry.parent.is_some() {
            continue;
        }
        stack.push((
            u32::try_from(root).expect("session record limit fits u32"),
            false,
        ));
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

fn now_unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

/// Session errors.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// Filesystem failure.
    #[error("session io error: {0}")]
    Io(#[from] std::io::Error),
    /// A record failed to serialize.
    #[error("session serialization error: {0}")]
    Serde(String),
    /// A completed (non-final) record failed to parse or violated an invariant.
    #[error("corrupt session record at line {line}: {message}")]
    Corrupt {
        /// 1-based line number of the offending record.
        line: usize,
        /// What was wrong with it.
        message: String,
    },
    /// A configured session parsing/resource bound was exceeded.
    #[error("session limit exceeded: {0}")]
    Limit(String),
    /// Another session handle changed the file after this one was opened.
    #[error("session was modified by another process; reopen it before writing")]
    ConcurrentModification,
    /// An operation referenced an entry ID that does not exist.
    #[error("unknown session entry: {0:?}")]
    UnknownEntry(EntryId),
    /// A compaction/checkpoint entry is not an ancestor of the current head.
    #[error("entry {0:?} is not an ancestor of the current head")]
    NotAncestor(EntryId),
    /// An operation requires a non-empty session.
    #[error("the session has no current head")]
    EmptySession,
    /// No durable checkpoint exists for the supplied prompt entry.
    #[error("no checkpoint exists for prompt entry {0:?}")]
    UnknownCheckpoint(EntryId),
}

/// An append-only JSONL session file.
///
/// Entries form a tree via parent links; the durable head selects the active
/// branch. [`Session::checkout`] moves the head to any existing entry, and
/// subsequent appends fork a new branch from there — earlier branches are
/// preserved verbatim in the file.
pub struct Session {
    path: PathBuf,
    file: File,
    /// The file length observed after the last successful replay/write. Every
    /// mutation verifies this under an exclusive advisory lock so two stale
    /// `Session` handles cannot append colliding entry IDs.
    persisted_len: u64,
    /// Number of complete JSONL records durably present. This includes head,
    /// checkpoint, and usage records, not only conversation entries.
    persisted_records: usize,
    entries: Vec<Entry>,
    index: HashMap<EntryId, usize>,
    head: Option<EntryId>,
    /// Monotonically-increasing counter for the next entry ID. Derived from
    /// the maximum ID replayed during [`Session::open`] (or 0 for a new
    /// session) and incremented on every [`Session::append`]. Using an
    /// explicit counter instead of `entries.len()` avoids collisions when the
    /// in-memory entry vector diverges from the on-disk state (e.g. after an
    /// unclean reopen).
    next_id: u64,
    /// Cached model-visible context. Message appends update it in place;
    /// checkout and compaction invalidate it because they can change the
    /// active branch or summary boundary.
    context_cache: RefCell<Option<Vec<Message>>>,
    /// Cumulative whole-microdollar session cost.
    /// Persisted in Head/Usage records and restored on open.
    total_cost_microdollars: u64,
    /// Picodollar remainder carried across provider operations.
    total_cost_picodollars_remainder: u32,
    /// Durable completed-prompt restore points in append order.
    checkpoints: Vec<Checkpoint>,
    /// Usage records for every completed provider operation, in append order.
    usage_records: Vec<UsageRecord>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("path", &self.path)
            .field("entries", &self.entries.len())
            .field("head", &self.head)
            .finish()
    }
}

impl Session {
    /// Creates a new empty session file on disk. Fails if the file exists.
    pub fn create(path: impl Into<PathBuf>) -> Result<Self, SessionError> {
        let path = path.into();
        let mut options = OpenOptions::new();
        options.create_new(true).read(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path)?;
        Ok(Self {
            path,
            file,
            persisted_len: 0,
            persisted_records: 0,
            entries: Vec::new(),
            index: HashMap::new(),
            head: None,
            next_id: 1,
            context_cache: RefCell::new(None),
            total_cost_microdollars: 0,
            total_cost_picodollars_remainder: 0,
            checkpoints: Vec::new(),
            usage_records: Vec::new(),
        })
    }

    /// Opens an existing session, replaying all records and restoring the
    /// head from the last recorded head.
    ///
    /// A torn *final* line (an interrupted write during an unclean exit) is
    /// dropped — and physically truncated from the file, so subsequent
    /// appends start on a fresh line instead of merging into the torn bytes.
    /// A *valid* final record that merely lost its trailing newline is kept,
    /// and the missing newline is written to complete it. Any malformed
    /// record *before* the final line is corruption and is rejected, as are
    /// duplicate entry IDs and references to unknown entries.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, SessionError> {
        Self::open_impl(path.into(), true)
    }

    /// Inspect an existing session without repairing, truncating, appending,
    /// or otherwise changing its bytes. The returned snapshot is intended for
    /// listing and reporting only; mutation methods fail because its file
    /// descriptor is read-only.
    pub fn open_read_only(path: impl Into<PathBuf>) -> Result<Self, SessionError> {
        Self::open_impl(path.into(), false)
    }

    fn open_impl(path: PathBuf, recover_tail: bool) -> Result<Self, SessionError> {
        Self::open_impl_with_limits(
            path,
            recover_tail,
            MAX_SESSION_FILE_BYTES,
            MAX_SESSION_RECORDS,
        )
    }

    fn open_impl_with_limits(
        path: PathBuf,
        recover_tail: bool,
        max_file_bytes: u64,
        max_records: usize,
    ) -> Result<Self, SessionError> {
        // Replay and tail handling must observe one stable snapshot. Without
        // this lock, a writer could append after the read but before the
        // observed length is captured, pairing stale IDs with a newer length.
        let mut options = OpenOptions::new();
        options.read(true);
        if recover_tail {
            options.append(true);
        }
        let mut file = options.open(&path)?;
        if recover_tail {
            file.lock_exclusive()?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = file.metadata()?.permissions().mode() & 0o777;
                if mode != 0o600 {
                    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
                }
            }
        } else {
            FileExt::lock_shared(&file)?;
        }
        let file_len = file.metadata()?.len();
        if file_len > max_file_bytes {
            return Err(SessionError::Limit(format!(
                "{} is {file_len} bytes (limit {max_file_bytes})",
                path.display()
            )));
        }
        let mut reader = file.try_clone()?;
        reader.seek(std::io::SeekFrom::Start(0))?;
        let mut content_bytes = Vec::with_capacity(file_len as usize);
        reader.read_to_end(&mut content_bytes)?;
        let content = session_text_with_torn_tail(&content_bytes)?;

        let mut entries: Vec<Entry> = Vec::new();
        let mut index: HashMap<EntryId, usize> = HashMap::new();
        let mut head: Option<EntryId> = None;
        let mut max_id: u64 = 0;
        let mut total_cost_microdollars: u64 = 0;
        let mut total_cost_picodollars_remainder: u32 = 0;
        let mut checkpoints: Vec<Checkpoint> = Vec::new();
        let mut checkpoint_lines: Vec<usize> = Vec::new();
        let mut usage_records: Vec<UsageRecord> = Vec::new();

        // Byte offset of the end of the last accepted record, so a torn tail
        // can be truncated away below.
        let mut valid_end: usize = 0;
        let mut segments = content.split_inclusive('\n').peekable();
        let mut line_no = 0usize;
        let mut persisted_records = 0usize;
        while let Some(segment) = segments.next() {
            line_no += 1;
            if line_no > max_records {
                return Err(SessionError::Limit(format!(
                    "session has more than {max_records} records"
                )));
            }
            let is_last = segments.peek().is_none();
            let line = segment.strip_suffix('\n').unwrap_or(segment);
            let record: SessionRecord = match serde_json::from_str(line) {
                Ok(r) => r,
                // Only the final line may be torn by an interrupted write;
                // anything earlier was completed by a successful append. A
                // trailing newline proves the writer completed that record,
                // so a malformed newline-terminated final line is corruption
                // rather than a recoverable torn tail.
                Err(_) if is_last && !segment.ends_with('\n') => break,
                Err(e) => {
                    return Err(SessionError::Corrupt {
                        line: line_no,
                        message: e.to_string(),
                    })
                }
            };
            valid_end += segment.len();
            persisted_records += 1;
            match record {
                SessionRecord::Entry(entry) => {
                    if index.contains_key(&entry.id) {
                        return Err(SessionError::Corrupt {
                            line: line_no,
                            message: format!("duplicate entry id {:?}", entry.id.0),
                        });
                    }
                    // Track the maximum numeric ID so we can safely resume
                    // appending even if the in-memory vector diverges from
                    // disk state.
                    if let Ok(n) = entry.id.0.parse::<u64>() {
                        max_id = max_id.max(n);
                    }
                    if let Some(parent) = &entry.parent {
                        if !index.contains_key(parent) {
                            return Err(SessionError::Corrupt {
                                line: line_no,
                                message: format!(
                                    "entry {:?} references unknown parent {:?}",
                                    entry.id.0, parent.0
                                ),
                            });
                        }
                    }
                    if let EntryValue::Compaction { first_kept, .. } = &entry.value {
                        if !index.contains_key(first_kept) {
                            return Err(SessionError::Corrupt {
                                line: line_no,
                                message: format!(
                                    "compaction {:?} references unknown first_kept {:?}",
                                    entry.id.0, first_kept.0
                                ),
                            });
                        }
                    }
                    index.insert(entry.id.clone(), entries.len());
                    entries.push(*entry);
                }
                SessionRecord::Head {
                    id,
                    total_cost_microdollars: cost,
                    total_cost_picodollars_remainder: remainder,
                } => {
                    if !index.contains_key(&id) {
                        return Err(SessionError::Corrupt {
                            line: line_no,
                            message: format!("head references unknown entry {:?}", id.0),
                        });
                    }
                    head = Some(id);
                    total_cost_microdollars = cost;
                    total_cost_picodollars_remainder = remainder;
                }
                SessionRecord::Checkpoint {
                    prompt,
                    head: checkpoint_head,
                    usage,
                    run_cost_microdollars,
                } => {
                    let prompt_is_user = index
                        .get(&prompt)
                        .and_then(|position| entries.get(*position))
                        .is_some_and(|entry| {
                            matches!(&entry.value, EntryValue::Message(Message::User(_)))
                        });
                    if !prompt_is_user || !index.contains_key(&checkpoint_head) {
                        return Err(SessionError::Corrupt {
                            line: line_no,
                            message: "checkpoint references unknown or non-user entries"
                                .to_string(),
                        });
                    }
                    checkpoint_lines.push(line_no);
                    checkpoints.push(Checkpoint {
                        prompt,
                        head: checkpoint_head,
                        usage,
                        run_cost_microdollars,
                    });
                }
                SessionRecord::Usage { record } => {
                    if let UsageRecordKind::AssistantTurn { assistant } = &record.kind {
                        let valid_assistant = index
                            .get(assistant)
                            .and_then(|position| entries.get(*position))
                            .is_some_and(|entry| {
                                matches!(&entry.value, EntryValue::Message(Message::Assistant(_)))
                            });
                        if !valid_assistant {
                            return Err(SessionError::Corrupt {
                                line: line_no,
                                message:
                                    "usage record references an unknown or non-assistant entry"
                                        .to_string(),
                            });
                        }
                    }
                    if let Some(cost) = record.session_cost_microdollars {
                        total_cost_microdollars = cost;
                        total_cost_picodollars_remainder = record
                            .session_cost_picodollars_remainder
                            .unwrap_or_default();
                    } else {
                        // Usage records written before cumulative session
                        // accounting was introduced have only their request
                        // total. Rebuild that legacy tally while replaying so
                        // reports and limits work for resumed sessions too.
                        let request_cost = record
                            .cost_microdollars
                            .or_else(|| record.cost.map(|cost| cost.total))
                            .unwrap_or_default();
                        total_cost_microdollars =
                            total_cost_microdollars.saturating_add(request_cost);
                    }
                    usage_records.push(record);
                }
            }
        }

        if !checkpoints.is_empty() {
            let (entered, exited) = entry_ancestry_intervals(&entries, &index);
            for (checkpoint, checkpoint_line) in checkpoints.iter().zip(checkpoint_lines) {
                let prompt = index[&checkpoint.prompt];
                let checkpoint_head = index[&checkpoint.head];
                let prompt_is_ancestor = entered[prompt] <= entered[checkpoint_head]
                    && exited[checkpoint_head] <= exited[prompt];
                if !prompt_is_ancestor {
                    return Err(SessionError::Corrupt {
                        line: checkpoint_line,
                        message: "checkpoint prompt is not an ancestor of its head".to_string(),
                    });
                }
            }
        }

        // Validate the ID counter before repairing any tail bytes. A
        // syntactically valid record can still be semantically corrupt, and
        // opening such a file must not normalize or otherwise mutate it before
        // returning the corruption error.
        let next_id = max_id.checked_add(1).ok_or_else(|| SessionError::Corrupt {
            line: line_no,
            message: "numeric entry ID exhausts the u64 ID space".to_owned(),
        })?;

        if recover_tail && valid_end < content_bytes.len() {
            // Torn final line: truncate it away so the next append starts on
            // a fresh line rather than merging into the torn bytes (which
            // would corrupt the record for every later reopen).
            file.set_len(valid_end as u64)?;
        }

        if recover_tail && valid_end > 0 && !content[..valid_end].ends_with('\n') {
            // The final record parsed but lost its newline in an interrupted
            // write; complete the line so the next append cannot merge into it.
            let repaired_len = u64::try_from(valid_end)
                .ok()
                .and_then(|length| length.checked_add(1))
                .ok_or_else(|| {
                    SessionError::Limit("repaired session file length overflow".to_owned())
                })?;
            if repaired_len > max_file_bytes {
                return Err(SessionError::Limit(format!(
                    "repair would grow session to {repaired_len} bytes (limit {max_file_bytes})"
                )));
            }
            file.write_all(b"\n")?;
        }
        let persisted_len = file.metadata()?.len();
        FileExt::unlock(&file)?;
        Ok(Self {
            path,
            file,
            persisted_len,
            persisted_records,
            entries,
            next_id,
            index,
            head,
            context_cache: RefCell::new(None),
            total_cost_microdollars,
            total_cost_picodollars_remainder,
            checkpoints,
            usage_records,
        })
    }

    /// The path of the underlying JSONL file.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Returns a stable, provider-safe cache-affinity key for this session.
    ///
    /// The key is derived from the full session path, so two sessions with the
    /// same filename in different workspaces cannot share a provider cache.
    /// Reopening the same file preserves the key across process restarts.
    pub fn cache_key(&self) -> String {
        const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
        let mut hash = FNV_OFFSET;
        for byte in self.path.to_string_lossy().as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        format!("ygg-{hash:016x}")
    }

    /// Append bytes only if this handle still reflects the complete file.
    ///
    /// The length check and write happen under one OS advisory lock. This is
    /// deliberately per-write rather than a lifetime lock: read-only session
    /// listing can still open active sessions, while a second writer fails
    /// before it can reuse stale entry IDs.
    fn persist(&mut self, bytes: &[u8]) -> Result<(), SessionError> {
        self.file.lock_exclusive()?;
        let result = (|| {
            if self.file.metadata()?.len() != self.persisted_len {
                return Err(SessionError::ConcurrentModification);
            }
            let byte_count = u64::try_from(bytes.len()).map_err(|_| {
                SessionError::Limit("record write length does not fit u64".to_owned())
            })?;
            let new_len = self
                .persisted_len
                .checked_add(byte_count)
                .ok_or_else(|| SessionError::Limit("session file length overflow".to_owned()))?;
            if new_len > MAX_SESSION_FILE_BYTES {
                return Err(SessionError::Limit(format!(
                    "write would grow session to {new_len} bytes (limit {MAX_SESSION_FILE_BYTES})"
                )));
            }
            let added_records = bytes.iter().filter(|byte| **byte == b'\n').count();
            let new_records = self
                .persisted_records
                .checked_add(added_records)
                .ok_or_else(|| SessionError::Limit("session record count overflow".to_owned()))?;
            if new_records > MAX_SESSION_RECORDS {
                return Err(SessionError::Limit(format!(
                    "write would grow session past {MAX_SESSION_RECORDS} records"
                )));
            }
            self.file.write_all(bytes)?;
            self.file.sync_data()?;
            self.persisted_len = new_len;
            self.persisted_records = new_records;
            Ok(())
        })();
        let unlock_result = FileExt::unlock(&self.file);
        result?;
        unlock_result?;
        Ok(())
    }

    /// Appends an entry (parented on the current head) and records the new
    /// head. Writes two JSONL records — the entry, then a head record — in a
    /// single synced write to the append-only file.
    pub fn append(&mut self, value: EntryValue) -> Result<EntryId, SessionError> {
        self.append_with_metadata(value, None)
    }

    /// Appends an entry with stable semantic presentation metadata.
    ///
    /// Metadata is intentionally kept outside [`EntryValue`] so model-visible
    /// conversation messages remain provider-independent and legacy readers can
    /// continue to ignore presentation details.
    pub fn append_with_metadata(
        &mut self,
        value: EntryValue,
        metadata: Option<EntryMetadata>,
    ) -> Result<EntryId, SessionError> {
        let id = EntryId(format!("{:03}", self.next_id));
        let next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| SessionError::Limit("session entry ID space is exhausted".to_owned()))?;
        let metadata = metadata.and_then(EntryMetadata::sanitized);
        let entry = Entry {
            id: id.clone(),
            parent: self.head.clone(),
            metadata,
            value,
        };
        let mut buf = Vec::with_capacity(256);
        write_json_line(&mut buf, &SessionRecordRef::Entry(&entry))?;
        write_json_line(
            &mut buf,
            &SessionRecordRef::Head {
                id: &id,
                total_cost_microdollars: &self.total_cost_microdollars,
                total_cost_picodollars_remainder: &self.total_cost_picodollars_remainder,
            },
        )?;
        // `persist` performs the unbuffered, synced write while excluding
        // stale concurrent writers.
        self.persist(&buf)?;

        self.index.insert(id.clone(), self.entries.len());
        self.entries.push(entry);
        self.head = Some(id.clone());
        self.next_id = next_id;

        let cache = self.context_cache.get_mut();
        match &self.entries.last().expect("just appended").value {
            EntryValue::Message(message) => {
                if let Some(messages) = cache {
                    append_context_message(messages, message);
                }
            }
            EntryValue::Config { .. } | EntryValue::PromptTemplateSelected { .. } => {}
            EntryValue::Compaction { .. }
            | EntryValue::SkillActivated { .. }
            | EntryValue::SkillResourceRead { .. }
            | EntryValue::SkillDeactivated { .. } => *cache = None,
        }
        Ok(id)
    }

    /// Changes the head to an existing entry and appends a head record (same
    /// persistence semantics as [`Session::append`]). Future appends fork a
    /// new branch from this point.
    pub fn checkout(&mut self, id: EntryId) -> Result<(), SessionError> {
        if !self.index.contains_key(&id) {
            return Err(SessionError::UnknownEntry(id));
        }
        let mut buf = Vec::with_capacity(64);
        write_json_line(
            &mut buf,
            &SessionRecordRef::Head {
                id: &id,
                total_cost_microdollars: &self.total_cost_microdollars,
                total_cost_picodollars_remainder: &self.total_cost_picodollars_remainder,
            },
        )?;
        self.persist(&buf)?;
        self.head = Some(id);
        *self.context_cache.get_mut() = None;
        Ok(())
    }

    /// Persist a restore point for a completed prompt without changing the
    /// current head or model-visible context.
    ///
    /// The prompt must be a user-message ancestor of the current head. The
    /// returned record can later be restored with [`Self::restore_checkpoint`].
    pub fn checkpoint(&mut self, prompt: EntryId) -> Result<Checkpoint, SessionError> {
        self.checkpoint_with_telemetry(prompt, None, None)
    }

    /// Persist a completed-prompt restore point together with exact aggregate
    /// usage and current-run cost for UI/status rehydration.
    ///
    /// `run_cost_microdollars` is `Some(0)` for explicitly zero-priced models
    /// and `None` when pricing was unavailable.
    pub fn checkpoint_with_telemetry(
        &mut self,
        prompt: EntryId,
        usage: Option<Usage>,
        run_cost_microdollars: Option<u64>,
    ) -> Result<Checkpoint, SessionError> {
        let head = self.head.clone().ok_or(SessionError::EmptySession)?;
        let prompt_is_user = self
            .entry(&prompt)
            .is_some_and(|entry| matches!(&entry.value, EntryValue::Message(Message::User(_))));
        if !prompt_is_user {
            return Err(SessionError::UnknownEntry(prompt));
        }
        if !self.is_ancestor_of_head(&prompt) {
            return Err(SessionError::NotAncestor(prompt));
        }

        let checkpoint = Checkpoint {
            prompt,
            head,
            usage,
            run_cost_microdollars,
        };
        let mut buffer = Vec::with_capacity(192);
        write_json_line(
            &mut buffer,
            &SessionRecordRef::Checkpoint {
                prompt: &checkpoint.prompt,
                head: &checkpoint.head,
                usage: &checkpoint.usage,
                run_cost_microdollars: &checkpoint.run_cost_microdollars,
            },
        )?;
        self.persist(&buffer)?;
        self.checkpoints.push(checkpoint.clone());
        Ok(checkpoint)
    }

    /// Durable completed-prompt restore points in append order, across all
    /// preserved branches.
    pub fn checkpoints(&self) -> &[Checkpoint] {
        &self.checkpoints
    }

    /// Provider usage records in append order, across all preserved branches.
    ///
    /// Assistant-turn records point at their exact durable assistant entry,
    /// unlike checkpoint usage which is aggregated for a whole user prompt.
    pub fn usage_records(&self) -> &[UsageRecord] {
        &self.usage_records
    }

    /// Newest provider usage record for an assistant turn on the active
    /// branch. Unlike checkpoint usage, this is one request rather than the
    /// sum of every autonomous tool turn in a submitted prompt.
    pub fn latest_active_assistant_usage(&self) -> Option<&UsageRecord> {
        let mut active = std::collections::HashSet::<&str>::new();
        let mut cursor = self.head.as_ref();
        while let Some(id) = cursor {
            active.insert(id.0.as_str());
            cursor = self.entry(id).and_then(|entry| entry.parent.as_ref());
        }
        self.usage_records.iter().rev().find(|record| {
            matches!(
                &record.kind,
                UsageRecordKind::AssistantTurn { assistant }
                    if active.contains(assistant.0.as_str())
            )
        })
    }

    /// Persist usage for one completed assistant turn.
    pub fn record_assistant_usage(
        &mut self,
        assistant: EntryId,
        endpoint: EndpointId,
        model: ModelId,
        usage: Usage,
        cost: Option<Cost>,
    ) -> Result<(), SessionError> {
        let valid_assistant = self.entry(&assistant).is_some_and(|entry| {
            matches!(&entry.value, EntryValue::Message(Message::Assistant(_)))
        });
        if !valid_assistant {
            return Err(SessionError::UnknownEntry(assistant));
        }
        self.record_usage(UsageRecord {
            kind: UsageRecordKind::AssistantTurn { assistant },
            usage,
            endpoint: Some(endpoint),
            model: Some(model),
            completed_at_unix_ms: Some(now_unix_millis()),
            cost,
            cost_microdollars: cost.map(|cost| cost.total),
            session_cost_microdollars: None,
            session_cost_picodollars_remainder: None,
        })
    }

    /// Persist usage for a context-compaction provider call.
    pub fn record_compaction_usage(
        &mut self,
        endpoint: EndpointId,
        model: ModelId,
        usage: Usage,
        cost: Option<Cost>,
    ) -> Result<(), SessionError> {
        self.record_usage(UsageRecord {
            kind: UsageRecordKind::Compaction,
            usage,
            endpoint: Some(endpoint),
            model: Some(model),
            completed_at_unix_ms: Some(now_unix_millis()),
            cost,
            cost_microdollars: cost.map(|cost| cost.total),
            session_cost_microdollars: None,
            session_cost_picodollars_remainder: None,
        })
    }

    /// Persist usage for an isolated terminal-gate provider call.
    pub fn record_terminal_gate_usage(
        &mut self,
        endpoint: EndpointId,
        model: ModelId,
        usage: Usage,
        cost: Option<Cost>,
        returned: Option<bool>,
    ) -> Result<(), SessionError> {
        self.record_usage(UsageRecord {
            kind: UsageRecordKind::TerminalGate { returned },
            usage,
            endpoint: Some(endpoint),
            model: Some(model),
            completed_at_unix_ms: Some(now_unix_millis()),
            cost,
            cost_microdollars: cost.map(|cost| cost.total),
            session_cost_microdollars: None,
            session_cost_picodollars_remainder: None,
        })
    }

    fn record_usage(&mut self, mut record: UsageRecord) -> Result<(), SessionError> {
        let request_remainder = record
            .cost
            .map(|cost| cost.total_picodollars_remainder)
            .unwrap_or_default();
        let remainder_sum = u64::from(self.total_cost_picodollars_remainder)
            .saturating_add(u64::from(request_remainder));
        let carry = remainder_sum / u64::from(PICODOLLARS_PER_MICRODOLLAR);
        let new_total = self
            .total_cost_microdollars
            .saturating_add(record.cost_microdollars.unwrap_or_default())
            .saturating_add(carry);
        let new_remainder = (remainder_sum % u64::from(PICODOLLARS_PER_MICRODOLLAR)) as u32;
        record.session_cost_microdollars = Some(new_total);
        record.session_cost_picodollars_remainder = Some(new_remainder);
        let mut buffer = Vec::with_capacity(224);
        write_json_line(&mut buffer, &SessionRecordRef::Usage { record: &record })?;
        self.persist(&buffer)?;
        self.total_cost_microdollars = new_total;
        self.total_cost_picodollars_remainder = new_remainder;
        self.usage_records.push(record);
        Ok(())
    }

    /// Newest completed-prompt checkpoint on the active branch.
    pub fn latest_active_checkpoint(&self) -> Option<&Checkpoint> {
        if self.checkpoints.is_empty() {
            return None;
        }
        let mut active_entry_ids = std::collections::HashSet::<&str>::new();
        let mut cursor = self.head.as_ref();
        while let Some(id) = cursor {
            active_entry_ids.insert(id.0.as_str());
            cursor = self.entry(id).and_then(|entry| entry.parent.as_ref());
        }
        self.checkpoints
            .iter()
            .rev()
            .find(|checkpoint| active_entry_ids.contains(checkpoint.head.0.as_str()))
    }

    /// Restore the newest checkpoint written for `prompt` and append the
    /// corresponding durable head update. Future appends branch from it.
    pub fn restore_checkpoint(&mut self, prompt: &EntryId) -> Result<(), SessionError> {
        let checkpoint = self
            .checkpoints
            .iter()
            .rev()
            .find(|checkpoint| &checkpoint.prompt == prompt)
            .cloned()
            .ok_or_else(|| SessionError::UnknownCheckpoint(prompt.clone()))?;
        self.checkout(checkpoint.head)
    }

    /// Returns the whole-microdollar portion of cumulative session cost.
    pub fn total_cost_microdollars(&self) -> u64 {
        self.total_cost_microdollars
    }

    /// Returns the cumulative picodollar remainder below one microdollar.
    pub fn total_cost_picodollars_remainder(&self) -> u32 {
        self.total_cost_picodollars_remainder
    }

    /// Increments the cumulative session cost by `additional` microdollars
    /// and persists a new head record. Local/custom models that have no
    /// pricing should pass 0 so the tally stays unchanged.
    pub fn add_cost(&mut self, additional: u64) -> Result<(), SessionError> {
        if additional == 0 {
            return Ok(());
        }
        let new_total = self.total_cost_microdollars.saturating_add(additional);
        let mut buf = Vec::with_capacity(64);
        write_json_line(
            &mut buf,
            &SessionRecordRef::Head {
                id: self.head.as_ref().expect("head exists after first append"),
                total_cost_microdollars: &new_total,
                total_cost_picodollars_remainder: &self.total_cost_picodollars_remainder,
            },
        )?;
        self.persist(&buf)?;
        self.total_cost_microdollars = new_total;
        Ok(())
    }

    /// Appends a manual compaction entry. `summary` is caller-provided text
    /// (this crate never generates summaries itself); `first_kept` must be an
    /// ancestor of — or equal to — the current head and marks the oldest
    /// entry kept in full fidelity by [`Session::context`].
    pub fn compact(
        &mut self,
        summary: impl Into<String>,
        first_kept: EntryId,
    ) -> Result<EntryId, SessionError> {
        self.compact_with_details(
            summary,
            first_kept,
            crate::compaction::CompactionDetails::default(),
        )
    }

    /// Appends a compaction checkpoint with cumulative Pi-compatible file
    /// operation details used by later iterative handoffs.
    pub fn compact_with_details(
        &mut self,
        summary: impl Into<String>,
        first_kept: EntryId,
        details: crate::compaction::CompactionDetails,
    ) -> Result<EntryId, SessionError> {
        if !self.is_ancestor_of_head(&first_kept) {
            return Err(SessionError::NotAncestor(first_kept));
        }
        let parent_id = self
            .entry(&first_kept)
            .ok_or_else(|| SessionError::UnknownEntry(first_kept.clone()))?
            .parent
            .clone();

        let (active_skills, skill_resources) = if let Some(p_id) = parent_id {
            let state = self.resolve_active_skills(&p_id)?;
            (state.active_skills, state.skill_resources)
        } else {
            (Vec::new(), Vec::new())
        };

        self.append(EntryValue::Compaction {
            summary: summary.into(),
            first_kept,
            active_skills,
            skill_resources,
            details,
        })
    }

    /// Returns the current head entry ID (`None` for an empty session).
    pub fn head(&self) -> Option<EntryId> {
        self.head.clone()
    }

    /// Borrows the current head entry ID without allocating.
    pub fn head_ref(&self) -> Option<&EntryId> {
        self.head.as_ref()
    }

    /// Returns all entries in insertion order, across all branches.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Returns the entry with the given ID.
    pub fn entry(&self, id: &EntryId) -> Option<&Entry> {
        self.index.get(id).map(|&i| &self.entries[i])
    }

    /// Reconstructs the model-visible context from the current head.
    ///
    /// Walks the parent chain from the head, stopping at the nearest
    /// compaction's `first_kept` boundary, and returns messages in
    /// chronological order. Compaction summaries are injected in front as
    /// synthetic user messages (`ygg-ai` has no system role inside
    /// [`Message`]; the request-level system prompt belongs to the agent).
    /// Config entries are skipped. Consecutive tool-result messages are
    /// coalesced into a single user message, matching the provider-required
    /// shape. Once materialized, the result is incrementally updated for
    /// ordinary appends and reused until checkout or compaction changes the
    /// active branch semantics.
    fn reconstruct_context(&self) -> Result<Vec<Message>, SessionError> {
        let mut newest_first: Vec<Message> = Vec::new();
        let mut summary: Option<String> = None;
        let mut boundary: Option<EntryId> = None;

        let mut cursor = self.head.as_ref();
        while let Some(id) = cursor {
            let entry = self
                .entry(id)
                .ok_or_else(|| SessionError::UnknownEntry(id.clone()))?;
            match &entry.value {
                EntryValue::Message(m) => newest_first.push(m.clone()),
                EntryValue::Config { .. } | EntryValue::PromptTemplateSelected { .. } => {}
                EntryValue::Compaction {
                    summary: compaction_summary,
                    first_kept,
                    ..
                } => {
                    // A compaction summary represents everything it replaces,
                    // including any older summary in that range. Therefore only
                    // the marker nearest the head is model-visible; injecting
                    // older summaries again duplicates overlapping history.
                    if boundary.is_none() {
                        summary = Some(compaction_summary.clone());
                        boundary = Some(first_kept.clone());
                    }
                }
                EntryValue::SkillActivated { .. }
                | EntryValue::SkillResourceRead { .. }
                | EntryValue::SkillDeactivated { .. } => {}
            }
            if boundary.as_ref() == Some(id) {
                break;
            }
            cursor = entry.parent.as_ref();
        }

        let mut messages: Vec<Message> = summary
            .into_iter()
            .map(|summary| {
                Message::User(UserMessage {
                    content: vec![UserPart::Text(format!(
                        "[summary of earlier conversation]\n{summary}"
                    ))],
                })
            })
            .collect();
        messages.extend(newest_first.into_iter().rev());
        Ok(coalesce_tool_results(messages))
    }

    /// Borrows the cached model-visible context without deep-cloning message
    /// text, tool output, or media. The first call reconstructs the active
    /// branch; ordinary appends update that cache incrementally.
    pub fn context_ref(&self) -> Result<Ref<'_, [Message]>, SessionError> {
        if self.context_cache.borrow().is_none() {
            let messages = self.reconstruct_context()?;
            *self.context_cache.borrow_mut() = Some(messages);
        }
        Ok(Ref::map(self.context_cache.borrow(), |cache| {
            cache
                .as_deref()
                .expect("context cache initialized immediately above")
        }))
    }

    /// Returns an owned model-visible context snapshot.
    ///
    /// Call [`Self::context_ref`] for estimates and inspection that do not
    /// require ownership; it avoids copying the complete conversation.
    pub fn context(&self) -> Result<Vec<Message>, SessionError> {
        Ok(self.context_ref()?.to_vec())
    }

    /// Reconstructs the model-visible messages represented strictly before an
    /// active-branch boundary. This is used by autonomous context recovery to
    /// summarize exactly what a compaction record will replace.
    pub fn context_before(&self, first_kept: &EntryId) -> Result<Vec<Message>, SessionError> {
        let entry = self
            .entry(first_kept)
            .ok_or_else(|| SessionError::UnknownEntry(first_kept.clone()))?;
        let mut reverse = Vec::new();
        let mut cursor = entry.parent.as_ref();
        while let Some(id) = cursor {
            let entry = self
                .entry(id)
                .ok_or_else(|| SessionError::UnknownEntry(id.clone()))?;
            reverse.push(entry);
            cursor = entry.parent.as_ref();
        }
        reverse.reverse();

        let mut messages = Vec::new();
        for entry in reverse {
            match &entry.value {
                EntryValue::Message(message) => messages.push(message.clone()),
                EntryValue::Compaction { summary, .. } => {
                    messages.clear();
                    messages.push(Message::User(UserMessage {
                        content: vec![UserPart::Text(format!(
                            "[summary of earlier conversation]\n{summary}"
                        ))],
                    }));
                }
                EntryValue::Config { .. }
                | EntryValue::PromptTemplateSelected { .. }
                | EntryValue::SkillActivated { .. }
                | EntryValue::SkillResourceRead { .. }
                | EntryValue::SkillDeactivated { .. } => {}
            }
        }
        Ok(coalesce_tool_results(messages))
    }

    /// True when `id` is the head or one of its persistent-tree ancestors.
    ///
    /// Compaction markers deliberately do not sever parent-link ancestry: they
    /// change model-visible context reconstruction, not which branch an entry
    /// belongs to. This predicate protects `compact()` from abandoned-branch
    /// references; it is not a context-visibility query.
    fn is_ancestor_of_head(&self, id: &EntryId) -> bool {
        let mut cursor = self.head.as_ref();
        while let Some(current) = cursor {
            if current == id {
                return true;
            }
            cursor = self.entry(current).and_then(|entry| entry.parent.as_ref());
        }
        false
    }

    /// Active skills resolved for a given leaf entry along its branch ancestry.
    pub fn resolve_active_skills(
        &self,
        leaf_id: &EntryId,
    ) -> Result<ActiveSkillState, SessionError> {
        let mut cursor = Some(leaf_id);
        let mut deactivated = std::collections::HashSet::new();
        let mut active_skills: Vec<SkillActivatedSnapshot> = Vec::new();
        let mut skill_resources: Vec<SkillResourceSnapshot> = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();
        let mut boundary: Option<&EntryId> = None;
        let mut saw_compaction = false;

        while let Some(id) = cursor {
            if boundary == Some(id) {
                break;
            }
            let entry = self
                .entry(id)
                .ok_or_else(|| SessionError::UnknownEntry(id.clone()))?;
            match &entry.value {
                EntryValue::SkillDeactivated {
                    activation_id,
                    skill_id,
                } => {
                    deactivated.insert(activation_id.clone());
                    // Deactivation resolves the skill ID, not merely one
                    // historical activation. Otherwise walking farther back
                    // resurrects the activation that a reload superseded.
                    seen_ids.insert(skill_id.clone());
                }
                EntryValue::SkillActivated {
                    descriptor,
                    instructions_hash,
                    instructions,
                } => {
                    let act_id = id.clone();
                    if !deactivated.contains(&act_id) && seen_ids.insert(descriptor.id.clone()) {
                        active_skills.push(SkillActivatedSnapshot {
                            activation_id: act_id,
                            descriptor: descriptor.clone(),
                            instructions_hash: instructions_hash.clone(),
                            instructions: instructions.clone(),
                        });
                    }
                }
                EntryValue::SkillResourceRead {
                    activation_id,
                    skill_id,
                    resource_path,
                    start_line,
                    line_count,
                    content_hash,
                    content,
                } => {
                    skill_resources.push(SkillResourceSnapshot {
                        activation_id: activation_id.clone(),
                        skill_id: skill_id.clone(),
                        resource_path: resource_path.clone(),
                        start_line: *start_line,
                        line_count: *line_count,
                        content_hash: content_hash.clone(),
                        content: content.clone(),
                    });
                }
                EntryValue::Compaction {
                    active_skills: comp_skills,
                    skill_resources: comp_res,
                    first_kept,
                    ..
                } if !saw_compaction => {
                    // The nearest compaction snapshot replaces all older
                    // snapshots in its range, just like its model-visible
                    // summary. Kept-range events are still traversed normally.
                    saw_compaction = true;
                    // The ancestry walk is newest-to-oldest, while cached
                    // skills are stored oldest-to-newest. Push this boundary
                    // in reverse so the final reversal restores chronology.
                    for skill in comp_skills.iter().rev() {
                        if !deactivated.contains(&skill.activation_id)
                            && seen_ids.insert(skill.descriptor.id.clone())
                        {
                            active_skills.push(skill.clone());
                        }
                    }
                    for res in comp_res {
                        skill_resources.push(res.clone());
                    }
                    boundary = self
                        .entry(first_kept)
                        .and_then(|entry| entry.parent.as_ref());
                }
                _ => {}
            }
            cursor = entry.parent.as_ref();
        }

        let active_activation_ids: std::collections::HashSet<crate::skills::SkillActivationId> =
            active_skills
                .iter()
                .map(|s| s.activation_id.clone())
                .collect();

        skill_resources.retain(|r| active_activation_ids.contains(&r.activation_id));
        active_skills.reverse();

        Ok(ActiveSkillState {
            active_skills,
            skill_resources,
        })
    }
}

/// Active skills resolved for a given leaf entry along its branch ancestry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ActiveSkillState {
    /// Ordered snapshots of active skills.
    pub active_skills: Vec<SkillActivatedSnapshot>,
    /// Snapshots of lazy resource reads active at the compaction boundary.
    pub skill_resources: Vec<SkillResourceSnapshot>,
}

fn is_tool_results(m: &UserMessage) -> bool {
    !m.content.is_empty()
        && m.content
            .iter()
            .all(|p| matches!(p, UserPart::ToolResult(_)))
}

/// Appends one newly persisted message to an already materialized context.
fn append_context_message(messages: &mut Vec<Message>, message: &Message) {
    if let Message::User(current) = message {
        if is_tool_results(current) {
            if let Some(Message::User(previous)) = messages.last_mut() {
                if is_tool_results(previous) {
                    previous.content.extend(current.content.iter().cloned());
                    return;
                }
            }
        }
    }
    messages.push(message.clone());
}

/// Merges consecutive user messages that consist solely of tool results into
/// one user message, the shape providers require for a tool-result turn.
/// Individual tool results stay individual *entries* on disk; coalescing
/// happens only during context reconstruction.
fn coalesce_tool_results(messages: Vec<Message>) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(messages.len());
    for message in messages {
        if let Message::User(current) = &message {
            if is_tool_results(current) {
                if let Some(Message::User(previous)) = out.last_mut() {
                    if is_tool_results(previous) {
                        previous.content.extend(current.content.clone());
                        continue;
                    }
                }
            }
        }
        out.push(message);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ygg_ai::{AssistantMessage, AssistantPart, ModelId, Protocol, ToolCallId, ToolResult};

    fn user(text: &str) -> EntryValue {
        EntryValue::Message(Message::User(UserMessage {
            content: vec![UserPart::Text(text.to_string())],
        }))
    }

    fn assistant(text: &str) -> EntryValue {
        EntryValue::Message(Message::Assistant(AssistantMessage {
            content: vec![AssistantPart::Text(text.to_string())],
            model: ModelId("m".to_string()),
            protocol: Protocol::AnthropicMessages,
        }))
    }

    fn tool_result(call_id: &str, text: &str) -> EntryValue {
        EntryValue::Message(Message::User(UserMessage {
            content: vec![UserPart::ToolResult(ToolResult {
                tool_call_id: ToolCallId(call_id.to_string()),
                content: vec![ygg_ai::ToolResultPart::Text(text.to_string())],
                is_error: false,
            })],
        }))
    }

    fn text_of(m: &Message) -> String {
        match m {
            Message::User(u) => u
                .content
                .iter()
                .map(|p| match p {
                    UserPart::Text(t) => t.clone(),
                    UserPart::ToolResult(r) => format!("result:{}", r.tool_call_id.0),
                    UserPart::Media(_) => "media".to_string(),
                })
                .collect::<Vec<_>>()
                .join("|"),
            Message::Assistant(a) => a
                .content
                .iter()
                .map(|p| match p {
                    AssistantPart::Text(t) => t.clone(),
                    _ => "other".to_string(),
                })
                .collect::<Vec<_>>()
                .join("|"),
        }
    }

    fn temp_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("session.jsonl")
    }

    fn skill_descriptor(id: &str) -> crate::skills::SkillDescriptor {
        crate::skills::SkillDescriptor {
            id: id.to_string(),
            name: id.to_string(),
            description: String::new(),
            version: None,
            source: crate::skills::SkillSource::BuiltIn,
            trust: crate::skills::SkillTrust::BuiltIn,
            required_tools: Vec::new(),
            tags: Vec::new(),
        }
    }

    #[test]
    fn prompt_metadata_persists_safe_identity_and_exact_normalized_color() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        let mut session = Session::create(&path).unwrap();
        let valid = session
            .append_with_metadata(
                user("valid"),
                Some(EntryMetadata {
                    prompt_model: Some(ModelId("custom/model-a".into())),
                    prompt_model_source: Some("  deepseek  ".into()),
                    prompt_color: Some("  #22AACC  ".into()),
                    display_text: Some("visible\ndraft".into()),
                }),
            )
            .unwrap();
        let invalid = session
            .append_with_metadata(
                user("invalid"),
                Some(EntryMetadata {
                    prompt_model: Some(ModelId("model\u{1b}[31m".into())),
                    prompt_model_source: Some("#2243e6".into()),
                    prompt_color: Some("rgb(1,2,3)\u{1b}".into()),
                    display_text: Some("bad\u{1b}".into()),
                }),
            )
            .unwrap();
        drop(session);

        let session = Session::open(&path).unwrap();
        assert_eq!(
            session.entry(&valid).unwrap().metadata,
            Some(EntryMetadata {
                prompt_model: Some(ModelId("custom/model-a".into())),
                prompt_model_source: Some("deepseek".into()),
                prompt_color: Some("#22aacc".into()),
                display_text: Some("visible\ndraft".into()),
            })
        );
        assert_eq!(session.entry(&invalid).unwrap().metadata, None);
        let persisted = std::fs::read_to_string(path).unwrap();
        assert!(!persisted.contains("#2243e6"));
        assert!(persisted.contains("#22aacc"));
        assert!(!persisted.contains("[31m"));
    }

    #[test]
    fn prompt_colors_are_immutable_across_checkout_branch_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        let mut session = Session::create(&path).unwrap();
        let first = session
            .append_with_metadata(
                user("first model"),
                Some(EntryMetadata {
                    prompt_model: Some(ModelId("model-a".into())),
                    prompt_color: Some("#123456".into()),
                    ..EntryMetadata::default()
                }),
            )
            .unwrap();
        let abandoned = session.append(assistant("old branch")).unwrap();
        session.checkout(first.clone()).unwrap();
        let second = session
            .append_with_metadata(
                user("second model"),
                Some(EntryMetadata {
                    prompt_model: Some(ModelId("model-b".into())),
                    prompt_color: Some("#abcdef".into()),
                    ..EntryMetadata::default()
                }),
            )
            .unwrap();
        assert_ne!(session.head(), Some(abandoned));
        drop(session);

        let session = Session::open(path).unwrap();
        assert_eq!(
            session
                .entry(&first)
                .and_then(|entry| entry.metadata.as_ref())
                .and_then(|metadata| metadata.prompt_color.as_deref()),
            Some("#123456")
        );
        assert_eq!(
            session
                .entry(&second)
                .and_then(|entry| entry.metadata.as_ref())
                .and_then(|metadata| metadata.prompt_color.as_deref()),
            Some("#abcdef")
        );
    }

    #[test]
    fn create_append_reopen_and_reconstruct() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);

        let mut s = Session::create(&path).unwrap();
        let e1 = s.append(user("hello")).unwrap();
        let e2 = s.append(assistant("hi there")).unwrap();
        assert_eq!(s.head(), Some(e2.clone()));
        assert_eq!(s.entries()[1].parent, Some(e1.clone()));
        drop(s);

        let reopened = Session::open(&path).unwrap();
        assert_eq!(reopened.head(), Some(e2));
        let ctx = reopened.context().unwrap();
        assert_eq!(ctx.len(), 2);
        assert_eq!(text_of(&ctx[0]), "hello");
        assert_eq!(text_of(&ctx[1]), "hi there");
    }

    #[test]
    fn cache_key_is_stable_and_path_scoped() {
        let first_dir = tempfile::tempdir().unwrap();
        let second_dir = tempfile::tempdir().unwrap();
        let first_path = first_dir.path().join("session.jsonl");
        let second_path = second_dir.path().join("session.jsonl");
        let first = Session::create(&first_path).unwrap();
        let first_key = first.cache_key();
        assert_eq!(first_key, first.cache_key());
        drop(first);
        let reopened = Session::open(&first_path).unwrap();
        assert_eq!(first_key, reopened.cache_key());
        let other = Session::create(&second_path).unwrap();
        assert_ne!(first_key, other.cache_key());
    }

    #[test]
    fn create_refuses_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        std::fs::write(&path, "").unwrap();
        assert!(matches!(Session::create(&path), Err(SessionError::Io(_))));
    }

    #[cfg(unix)]
    #[test]
    fn newly_created_session_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        let _session = Session::create(&path).unwrap();
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn stale_handle_cannot_append_a_duplicate_entry_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        let original = Session::create(&path).unwrap();
        let mut stale = Session::open(&path).unwrap();
        let mut current = original;

        assert_eq!(
            current.append(user("first")).unwrap(),
            EntryId("001".into())
        );
        assert!(matches!(
            stale.append(user("stale")),
            Err(SessionError::ConcurrentModification)
        ));

        drop(stale);
        drop(current);
        let reopened = Session::open(path).unwrap();
        assert_eq!(reopened.entries().len(), 1);
        assert_eq!(reopened.head(), Some(EntryId("001".into())));
    }

    #[test]
    fn append_preflights_the_file_limit_without_partial_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        let mut session = Session::create(&path).unwrap();
        let near_limit = MAX_SESSION_FILE_BYTES - 1;
        session.file.set_len(near_limit).unwrap();
        session.persisted_len = near_limit;
        let before_next_id = session.next_id;
        let before_records = session.persisted_records;

        let error = session.append(user("must not be written")).unwrap_err();

        assert!(matches!(error, SessionError::Limit(_)), "{error}");
        assert_eq!(session.file.metadata().unwrap().len(), near_limit);
        assert_eq!(session.persisted_len, near_limit);
        assert_eq!(session.persisted_records, before_records);
        assert_eq!(session.next_id, before_next_id);
        assert!(session.entries.is_empty());
        assert!(session.index.is_empty());
        assert!(session.head.is_none());
    }

    #[test]
    fn append_preflights_the_record_limit_without_partial_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        let mut session = Session::create(&path).unwrap();
        session.persisted_records = MAX_SESSION_RECORDS - 1;
        let before_next_id = session.next_id;

        let error = session
            .append(user("two records would exceed the limit"))
            .unwrap_err();

        assert!(matches!(error, SessionError::Limit(_)), "{error}");
        assert_eq!(session.file.metadata().unwrap().len(), 0);
        assert_eq!(session.persisted_len, 0);
        assert_eq!(session.persisted_records, MAX_SESSION_RECORDS - 1);
        assert_eq!(session.next_id, before_next_id);
        assert!(session.entries.is_empty());
        assert!(session.index.is_empty());
        assert!(session.head.is_none());
    }

    #[test]
    fn missing_newline_repair_never_grows_past_the_file_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        let mut session = Session::create(&path).unwrap();
        session.append(user("valid unterminated record")).unwrap();
        drop(session);

        let mut unterminated = std::fs::read(&path).unwrap();
        assert_eq!(unterminated.pop(), Some(b'\n'));
        std::fs::write(&path, &unterminated).unwrap();
        let exact_limit = u64::try_from(unterminated.len()).unwrap();

        let error =
            Session::open_impl_with_limits(path.clone(), true, exact_limit, MAX_SESSION_RECORDS)
                .unwrap_err();
        assert!(matches!(error, SessionError::Limit(_)), "{error}");
        assert!(error.to_string().contains("repair would grow session"));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            unterminated,
            "a rejected repair must leave the source bytes untouched"
        );

        let repaired = Session::open_impl_with_limits(
            path.clone(),
            true,
            exact_limit + 1,
            MAX_SESSION_RECORDS,
        )
        .unwrap();
        drop(repaired);
        let repaired = std::fs::read(path).unwrap();
        assert_eq!(repaired.len() as u64, exact_limit + 1);
        assert_eq!(repaired.last(), Some(&b'\n'));
    }

    #[test]
    fn maximum_numeric_entry_id_is_corruption_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        let mut session = Session::create(&path).unwrap();
        session.append(user("ordinary")).unwrap();
        drop(session);
        let original = std::fs::read_to_string(&path).unwrap();
        let mut corrupt = original
            .replace("\"001\"", &format!("\"{}\"", u64::MAX))
            .into_bytes();
        assert_eq!(corrupt.pop(), Some(b'\n'));
        std::fs::write(&path, &corrupt).unwrap();

        let opened = std::panic::catch_unwind(|| Session::open(&path));
        assert!(opened.is_ok(), "opening a corrupt ID must never unwind");
        let error = opened.unwrap().unwrap_err();
        assert!(matches!(error, SessionError::Corrupt { .. }), "{error}");
        assert!(error.to_string().contains("exhausts the u64 ID space"));
        assert_eq!(
            std::fs::read(path).unwrap(),
            corrupt,
            "semantic corruption must be rejected before tail repair mutates bytes"
        );
    }

    #[test]
    fn durable_head_survives_reopen_after_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);

        let mut s = Session::create(&path).unwrap();
        let e1 = s.append(user("one")).unwrap();
        let _e2 = s.append(assistant("two")).unwrap();
        s.checkout(e1.clone()).unwrap();
        drop(s);

        let reopened = Session::open(&path).unwrap();
        assert_eq!(reopened.head(), Some(e1));
        let ctx = reopened.context().unwrap();
        assert_eq!(ctx.len(), 1);
        assert_eq!(text_of(&ctx[0]), "one");
    }

    #[test]
    fn checkout_ancestor_and_continue_forms_branch_preserving_old_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);

        let mut s = Session::create(&path).unwrap();
        let e1 = s.append(user("root")).unwrap();
        let e2 = s.append(assistant("branch-a")).unwrap();
        s.checkout(e1.clone()).unwrap();
        let e3 = s.append(assistant("branch-b")).unwrap();

        // The new entry forks from the ancestor, the old branch is intact.
        assert_eq!(s.entry(&e3).unwrap().parent, Some(e1.clone()));
        assert_eq!(s.entry(&e2).unwrap().parent, Some(e1));
        assert_eq!(s.entries().len(), 3);

        let ctx = s.context().unwrap();
        assert_eq!(ctx.len(), 2);
        assert_eq!(text_of(&ctx[1]), "branch-b");

        // Reopen: both branches still present, head on the new branch.
        drop(s);
        let reopened = Session::open(&path).unwrap();
        assert_eq!(reopened.entries().len(), 3);
        assert_eq!(reopened.head(), Some(e3));
    }

    #[test]
    fn completed_prompt_checkpoint_round_trips_and_restores_a_branch() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        let mut session = Session::create(&path).unwrap();
        let prompt = session.append(user("make a change")).unwrap();
        let completed = session.append(assistant("done")).unwrap();

        let checkpoint = session.checkpoint(prompt.clone()).unwrap();
        assert_eq!(checkpoint.head, completed);
        assert_eq!(session.head(), Some(completed.clone()));
        session.append(user("later branch")).unwrap();
        drop(session);

        let mut reopened = Session::open(&path).unwrap();
        assert_eq!(reopened.checkpoints(), &[checkpoint]);
        reopened.restore_checkpoint(&prompt).unwrap();
        assert_eq!(reopened.head(), Some(completed.clone()));
        let branch = reopened.append(user("new branch")).unwrap();
        assert_eq!(reopened.entry(&branch).unwrap().parent, Some(completed));
        assert_eq!(
            text_of(reopened.context().unwrap().last().unwrap()),
            "new branch"
        );
    }

    #[test]
    fn checkpoint_telemetry_round_trips_and_follows_the_active_branch() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        let mut session = Session::create(&path).unwrap();
        let first_prompt = session.append(user("first")).unwrap();
        session.append(assistant("first answer")).unwrap();
        let first_usage = Usage {
            input_tokens: 120,
            output_tokens: 30,
            total_tokens: 150,
            ..Usage::default()
        };
        let first = session
            .checkpoint_with_telemetry(first_prompt, Some(first_usage), Some(8_600))
            .unwrap();

        let second_prompt = session.append(user("second")).unwrap();
        session.append(assistant("second answer")).unwrap();
        session
            .checkpoint_with_telemetry(second_prompt, Some(Usage::default()), Some(0))
            .unwrap();
        session.checkout(first.head.clone()).unwrap();
        drop(session);

        let reopened = Session::open(path).unwrap();
        let active = reopened.latest_active_checkpoint().unwrap();
        assert_eq!(active, &first);
        assert_eq!(active.usage, Some(first_usage));
        assert_eq!(active.run_cost_microdollars, Some(8_600));
    }

    #[test]
    fn latest_active_assistant_usage_is_per_request_and_branch_aware() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::create(temp_path(&dir)).unwrap();
        let root = session.append(user("root")).unwrap();
        let abandoned = session.append(assistant("abandoned")).unwrap();
        session
            .record_assistant_usage(
                abandoned,
                EndpointId("provider".into()),
                ModelId("m".into()),
                Usage {
                    total_tokens: 900,
                    ..Usage::default()
                },
                None,
            )
            .unwrap();
        session.checkout(root).unwrap();
        session.append(user("active")).unwrap();
        let active = session.append(assistant("active answer")).unwrap();
        session
            .record_assistant_usage(
                active,
                EndpointId("provider".into()),
                ModelId("m".into()),
                Usage {
                    total_tokens: 100,
                    ..Usage::default()
                },
                None,
            )
            .unwrap();
        session
            .record_compaction_usage(
                EndpointId("provider".into()),
                ModelId("m".into()),
                Usage {
                    total_tokens: 500,
                    ..Usage::default()
                },
                None,
            )
            .unwrap();

        assert_eq!(
            session
                .latest_active_assistant_usage()
                .unwrap()
                .usage
                .total_tokens,
            100
        );
    }

    #[test]
    fn per_operation_usage_round_trips_for_turns_and_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        let mut session = Session::create(&path).unwrap();
        session.append(user("first")).unwrap();
        let assistant = session.append(assistant("answer")).unwrap();
        let turn_usage = Usage {
            input_tokens: 50,
            cache_read_tokens: 100,
            output_tokens: 20,
            total_tokens: 170,
            ..Usage::default()
        };
        let turn_cost = Cost {
            total: 42,
            total_picodollars_remainder: 600_000,
            ..Cost::default()
        };
        session
            .record_assistant_usage(
                assistant.clone(),
                EndpointId("provider".to_string()),
                ModelId("m".to_string()),
                turn_usage,
                Some(turn_cost),
            )
            .unwrap();
        let compaction_usage = Usage {
            input_tokens: 75,
            output_tokens: 10,
            total_tokens: 85,
            ..Usage::default()
        };
        session
            .record_compaction_usage(
                EndpointId("provider".to_string()),
                ModelId("m".to_string()),
                compaction_usage,
                Some(Cost {
                    total_picodollars_remainder: 600_000,
                    ..Cost::default()
                }),
            )
            .unwrap();
        let expected = session.usage_records().to_vec();
        assert_eq!(expected[0].cost, Some(turn_cost));
        assert_eq!(expected[0].cost_microdollars, Some(42));
        assert_eq!(expected[0].session_cost_microdollars, Some(42));
        assert_eq!(
            expected[0].session_cost_picodollars_remainder,
            Some(600_000)
        );
        assert_eq!(expected[1].session_cost_microdollars, Some(43));
        assert_eq!(
            expected[1].session_cost_picodollars_remainder,
            Some(200_000)
        );
        assert!(expected[0].completed_at_unix_ms.is_some());
        assert_eq!(session.total_cost_microdollars(), 43);
        assert_eq!(session.total_cost_picodollars_remainder(), 200_000);
        drop(session);

        let reopened = Session::open(path).unwrap();
        assert_eq!(reopened.usage_records(), expected);
        assert_eq!(reopened.total_cost_microdollars(), 43);
        assert_eq!(reopened.total_cost_picodollars_remainder(), 200_000);
    }

    #[test]
    fn checkpoint_rejects_non_user_and_non_ancestor_entries() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::create(temp_path(&dir)).unwrap();
        let root = session.append(user("root")).unwrap();
        let old_prompt = session.append(user("old branch")).unwrap();
        let assistant_entry = session.append(assistant("done")).unwrap();
        assert!(matches!(
            session.checkpoint(assistant_entry),
            Err(SessionError::UnknownEntry(_))
        ));
        session.checkout(root).unwrap();
        session.append(user("new branch")).unwrap();
        assert!(matches!(
            session.checkpoint(old_prompt),
            Err(SessionError::NotAncestor(_))
        ));
    }

    #[test]
    fn replay_rejects_a_checkpoint_whose_prompt_is_on_another_branch() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        let mut session = Session::create(&path).unwrap();
        let root = session.append(user("root")).unwrap();
        let abandoned_prompt = session.append(user("abandoned prompt")).unwrap();
        session.append(assistant("abandoned answer")).unwrap();
        session.checkout(root).unwrap();
        session.append(user("active prompt")).unwrap();
        let active_head = session.append(assistant("active answer")).unwrap();
        drop(session);

        let mut bytes = Vec::new();
        write_json_line(
            &mut bytes,
            &SessionRecord::Checkpoint {
                prompt: abandoned_prompt,
                head: active_head,
                usage: None,
                run_cost_microdollars: None,
            },
        )
        .unwrap();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&bytes)
            .unwrap();

        let error = Session::open(&path).unwrap_err();
        assert!(
            matches!(error, SessionError::Corrupt { line: 12, .. }),
            "{error}"
        );
        assert!(error.to_string().contains("not an ancestor"), "{error}");
    }

    #[test]
    fn replay_validates_many_checkpoints_with_one_linear_ancestry_index() {
        const ENTRY_COUNT: u64 = 4_096;
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        let mut bytes = Vec::new();
        let mut parent = None;
        let root = EntryId("001".into());
        for number in 1..=ENTRY_COUNT {
            let id = EntryId(format!("{number:03}"));
            let entry = Entry {
                id: id.clone(),
                parent: parent.clone(),
                metadata: None,
                value: if number == 1 {
                    user("checkpoint root")
                } else {
                    EntryValue::Config {
                        model: None,
                        reasoning: None,
                        reasoning_mode: None,
                    }
                },
            };
            write_json_line(&mut bytes, &SessionRecordRef::Entry(&entry)).unwrap();
            parent = Some(id);
        }
        let head = parent.unwrap();
        let total_cost = 0u64;
        let remainder = 0u32;
        write_json_line(
            &mut bytes,
            &SessionRecordRef::Head {
                id: &head,
                total_cost_microdollars: &total_cost,
                total_cost_picodollars_remainder: &remainder,
            },
        )
        .unwrap();
        let usage = None;
        let run_cost = None;
        for _ in 0..ENTRY_COUNT {
            write_json_line(
                &mut bytes,
                &SessionRecordRef::Checkpoint {
                    prompt: &root,
                    head: &head,
                    usage: &usage,
                    run_cost_microdollars: &run_cost,
                },
            )
            .unwrap();
        }
        std::fs::write(&path, bytes).unwrap();

        let session = Session::open_read_only(path).unwrap();
        assert_eq!(session.entries().len(), ENTRY_COUNT as usize);
        assert_eq!(session.checkpoints().len(), ENTRY_COUNT as usize);
    }

    #[test]
    fn checkout_of_unknown_entry_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::create(temp_path(&dir)).unwrap();
        s.append(user("x")).unwrap();
        let err = s.checkout(EntryId("999".to_string())).unwrap_err();
        assert!(matches!(err, SessionError::UnknownEntry(_)));
    }

    #[test]
    fn malformed_parent_reference_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        let entry = r#"{"type":"entry","id":"001","parent":"000","value":{"type":"config","model":null,"reasoning":null}}"#;
        std::fs::write(&path, format!("{entry}\n{entry}\n")).unwrap();
        let err = Session::open(&path).unwrap_err();
        assert!(
            matches!(err, SessionError::Corrupt { line: 1, .. }),
            "{err}"
        );
    }

    #[test]
    fn incomplete_trailing_record_is_recovered() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);

        let mut s = Session::create(&path).unwrap();
        let e1 = s.append(user("kept")).unwrap();
        drop(s);

        // Simulate a torn write: a partial JSON record with no newline.
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(br#"{"type":"entry","id":"002","paren"#)
            .unwrap();
        drop(f);

        let mut reopened = Session::open(&path).unwrap();
        assert_eq!(reopened.entries().len(), 1);
        assert_eq!(reopened.head(), Some(e1));
        // The session remains appendable after recovery.
        let e2 = reopened.append(assistant("next")).unwrap();
        assert_eq!(reopened.head(), Some(e2.clone()));
        drop(reopened);

        // Regression: recovery must truncate the torn bytes, so the
        // post-recovery append starts a fresh line — a second reopen must not
        // see a merged/corrupt record.
        let reopened_again = Session::open(&path).unwrap();
        assert_eq!(reopened_again.entries().len(), 2);
        assert_eq!(reopened_again.head(), Some(e2));
        assert!(!std::fs::read_to_string(&path).unwrap().contains("paren\""));
    }

    #[test]
    fn invalid_utf8_in_an_unterminated_final_record_is_recovered() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);

        let mut session = Session::create(&path).unwrap();
        let durable_head = session.append(user("kept")).unwrap();
        drop(session);
        let durable_len = std::fs::metadata(&path).unwrap().len();

        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"text\":\"").unwrap();
        file.write_all(&[0xf0, 0x9f]).unwrap();
        drop(file);
        let torn_bytes = std::fs::read(&path).unwrap();

        let read_only = Session::open_read_only(&path).unwrap();
        assert_eq!(read_only.head(), Some(durable_head.clone()));
        drop(read_only);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            torn_bytes,
            "read-only inspection must not repair the tail"
        );

        let recovered = Session::open(&path).unwrap();
        assert_eq!(recovered.head(), Some(durable_head));
        drop(recovered);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), durable_len);
    }

    #[test]
    fn invalid_utf8_in_a_newline_terminated_record_is_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);

        let mut session = Session::create(&path).unwrap();
        session.append(user("kept")).unwrap();
        drop(session);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&[0xff, b'\n']).unwrap();
        drop(file);
        let original = std::fs::read(&path).unwrap();

        let error = Session::open(&path).unwrap_err();
        assert!(
            matches!(error, SessionError::Corrupt { line: 3, .. }),
            "{error}"
        );
        assert!(error.to_string().contains("invalid UTF-8"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn malformed_newline_terminated_final_record_is_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);

        let mut session = Session::create(&path).unwrap();
        session.append(user("kept")).unwrap();
        drop(session);

        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"type\":\"entry\"\n").unwrap();
        drop(file);
        let original = std::fs::read(&path).unwrap();

        let read_only_error = Session::open_read_only(&path).unwrap_err();
        assert!(
            matches!(read_only_error, SessionError::Corrupt { line: 3, .. }),
            "{read_only_error}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);

        let recovery_error = Session::open(&path).unwrap_err();
        assert!(
            matches!(recovery_error, SessionError::Corrupt { line: 3, .. }),
            "{recovery_error}"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            original,
            "completed corrupt records must never be truncated as torn tails"
        );
    }

    #[test]
    fn valid_final_record_without_trailing_newline_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);

        let mut s = Session::create(&path).unwrap();
        s.append(user("one")).unwrap();
        let e2 = s.append(assistant("two")).unwrap();
        drop(s);

        // Simulate losing only the final newline of an otherwise complete
        // write: the record is valid and must be kept, not discarded.
        let content = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, content.strip_suffix('\n').unwrap()).unwrap();

        let mut reopened = Session::open(&path).unwrap();
        assert_eq!(reopened.entries().len(), 2);
        assert_eq!(reopened.head(), Some(e2));

        // And the completed newline keeps subsequent appends line-separated.
        let e3 = reopened.append(user("three")).unwrap();
        drop(reopened);
        let reopened_again = Session::open(&path).unwrap();
        assert_eq!(reopened_again.entries().len(), 3);
        assert_eq!(reopened_again.head(), Some(e3));
    }

    #[test]
    fn corruption_before_the_trailing_record_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);

        let mut s = Session::create(&path).unwrap();
        s.append(user("a")).unwrap();
        s.append(assistant("b")).unwrap();
        drop(s);

        let content = std::fs::read_to_string(&path).unwrap();
        let mut lines: Vec<String> = content.lines().map(String::from).collect();
        // Corrupt a completed (non-final) record.
        lines[1] = lines[1][..lines[1].len() / 2].to_string();
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();

        let err = Session::open(&path).unwrap_err();
        assert!(
            matches!(err, SessionError::Corrupt { line: 2, .. }),
            "{err}"
        );
    }

    #[test]
    fn config_entries_persist_but_are_not_context() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);

        let mut s = Session::create(&path).unwrap();
        s.append(user("hi")).unwrap();
        s.append(EntryValue::Config {
            model: Some("claude".to_string()),
            reasoning: Some("high".to_string()),
            reasoning_mode: None,
        })
        .unwrap();
        s.append(assistant("hello")).unwrap();
        drop(s);

        let reopened = Session::open(&path).unwrap();
        assert!(matches!(
            reopened.entries()[1].value,
            EntryValue::Config { .. }
        ));
        let ctx = reopened.context().unwrap();
        assert_eq!(ctx.len(), 2, "config entries are not model-visible");
    }

    #[test]
    fn active_skills_keep_chronological_order_across_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::create(temp_path(&dir)).unwrap();
        for id in ["a", "b"] {
            session
                .append(EntryValue::SkillActivated {
                    descriptor: skill_descriptor(id),
                    instructions_hash: format!("{id}-hash"),
                    instructions: format!("{id}-instructions"),
                })
                .unwrap();
        }
        // Compaction caches the state before this entry: [a, b].
        let first_kept = session.append(user("keep this")).unwrap();
        session.compact("summary", first_kept).unwrap();
        session
            .append(EntryValue::SkillActivated {
                descriptor: skill_descriptor("c"),
                instructions_hash: "c-hash".to_string(),
                instructions: "c-instructions".to_string(),
            })
            .unwrap();

        let state = session
            .resolve_active_skills(&session.head().unwrap())
            .unwrap();
        let ids: Vec<_> = state
            .active_skills
            .iter()
            .map(|skill| skill.descriptor.id.as_str())
            .collect();
        assert_eq!(ids, ["a", "b", "c"]);
    }

    #[test]
    fn deactivating_latest_skill_activation_does_not_resurrect_an_older_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::create(temp_path(&dir)).unwrap();
        session
            .append(EntryValue::SkillActivated {
                descriptor: skill_descriptor("audit"),
                instructions_hash: "old-hash".to_string(),
                instructions: "old instructions".to_string(),
            })
            .unwrap();
        let latest = session
            .append(EntryValue::SkillActivated {
                descriptor: skill_descriptor("audit"),
                instructions_hash: "new-hash".to_string(),
                instructions: "new instructions".to_string(),
            })
            .unwrap();
        session
            .append(EntryValue::SkillDeactivated {
                activation_id: latest,
                skill_id: "audit".to_string(),
            })
            .unwrap();

        let state = session
            .resolve_active_skills(&session.head().unwrap())
            .unwrap();
        assert!(state.active_skills.is_empty());
    }

    #[test]
    fn repeated_compaction_uses_only_the_nearest_skill_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::create(temp_path(&dir)).unwrap();
        let activation_id = session
            .append(EntryValue::SkillActivated {
                descriptor: skill_descriptor("audit"),
                instructions_hash: "instructions-hash".into(),
                instructions: "audit instructions".into(),
            })
            .unwrap();
        session
            .append(EntryValue::SkillResourceRead {
                activation_id: activation_id.clone(),
                skill_id: "audit".into(),
                resource_path: "reference.txt".into(),
                start_line: None,
                line_count: None,
                content_hash: "resource-hash".into(),
                content: "resource content".into(),
            })
            .unwrap();
        session.append(user("old user")).unwrap();
        let old = session.append(assistant("old assistant")).unwrap();
        session.append(user("recent user")).unwrap();
        let recent = session.append(assistant("recent assistant")).unwrap();

        // Append both markers after the same completed history, matching
        // repeated provider rejection before another assistant can be added.
        session.compact("first summary", old).unwrap();
        session.compact("replacement summary", recent).unwrap();

        let state = session
            .resolve_active_skills(&session.head().unwrap())
            .unwrap();
        assert_eq!(state.active_skills.len(), 1);
        assert_eq!(state.skill_resources.len(), 1);
        assert_eq!(state.skill_resources[0].resource_path, "reference.txt");
    }

    #[test]
    fn compaction_reconstruction_matches_design_example() {
        // Entries: E1, E2, E3, C(first_kept=E2), E5, E6 — context must be
        // [summary, E2, E3, E5, E6].
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::create(temp_path(&dir)).unwrap();
        let _e1 = s.append(user("E1")).unwrap();
        let e2 = s.append(assistant("E2")).unwrap();
        let _e3 = s.append(user("E3")).unwrap();
        let _c = s.compact("what came before", e2).unwrap();
        let _e5 = s.append(assistant("E5")).unwrap();
        let _e6 = s.append(user("E6")).unwrap();

        let ctx = s.context().unwrap();
        let texts: Vec<String> = ctx.iter().map(text_of).collect();
        assert_eq!(
            texts,
            vec![
                "[summary of earlier conversation]\nwhat came before".to_string(),
                "E2".to_string(),
                "E3".to_string(),
                "E5".to_string(),
                "E6".to_string(),
            ]
        );
    }

    #[test]
    fn successive_compactions_expose_only_the_newest_summary() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::create(temp_path(&dir)).unwrap();
        session.append(user("old user")).unwrap();
        session.append(assistant("old assistant")).unwrap();
        session.append(user("middle user")).unwrap();
        let middle = session.append(assistant("middle assistant")).unwrap();
        session.append(user("recent user")).unwrap();
        let recent = session.append(assistant("recent assistant")).unwrap();

        session
            .compact("first overlapping summary", middle)
            .unwrap();
        session
            .compact("replacement summary including prior history", recent)
            .unwrap();

        let texts: Vec<String> = session.context().unwrap().iter().map(text_of).collect();
        assert_eq!(
            texts,
            [
                "[summary of earlier conversation]\nreplacement summary including prior history",
                "recent assistant",
            ]
        );
        assert!(texts.iter().all(|text| !text.contains("first overlapping")));
    }

    #[test]
    fn compact_rejects_non_ancestor_first_kept() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::create(temp_path(&dir)).unwrap();
        let e1 = s.append(user("root")).unwrap();
        let e2 = s.append(assistant("side")).unwrap();
        s.checkout(e1).unwrap();
        let _e3 = s.append(assistant("main")).unwrap();
        // e2 is on the abandoned branch, not an ancestor of the head.
        let err = s.compact("s", e2).unwrap_err();
        assert!(matches!(err, SessionError::NotAncestor(_)));
    }

    #[test]
    fn compaction_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        let mut s = Session::create(&path).unwrap();
        let e1 = s.append(user("old")).unwrap();
        let e2 = s.append(user("kept")).unwrap();
        assert_eq!(e1.0, "001");
        s.compact("summary text", e2).unwrap();
        drop(s);

        let reopened = Session::open(&path).unwrap();
        let ctx = reopened.context().unwrap();
        let texts: Vec<String> = ctx.iter().map(text_of).collect();
        assert_eq!(
            texts,
            vec![
                "[summary of earlier conversation]\nsummary text".to_string(),
                "kept".to_string(),
            ]
        );
    }

    #[test]
    fn tool_results_persist_individually_and_coalesce_in_context() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::create(temp_path(&dir)).unwrap();
        s.append(user("do things")).unwrap();
        s.append(assistant("calling tools")).unwrap();
        s.append(tool_result("call_1", "one")).unwrap();
        s.append(tool_result("call_2", "two")).unwrap();

        // Individual persistence: two separate entries on disk.
        assert_eq!(s.entries().len(), 4);

        // Coalesced reconstruction: one user message with both results.
        let ctx = s.context().unwrap();
        assert_eq!(ctx.len(), 3);
        match &ctx[2] {
            Message::User(u) => {
                assert_eq!(u.content.len(), 2);
                assert!(u
                    .content
                    .iter()
                    .all(|p| matches!(p, UserPart::ToolResult(_))));
            }
            _ => panic!("expected coalesced user message"),
        }
    }

    #[test]
    fn plain_user_text_does_not_coalesce_with_tool_results() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::create(temp_path(&dir)).unwrap();
        s.append(assistant("calling tool")).unwrap();
        s.append(tool_result("call_1", "one")).unwrap();
        s.append(user("interjection")).unwrap();
        let ctx = s.context().unwrap();
        assert_eq!(ctx.len(), 3);
    }
}
