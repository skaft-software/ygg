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
//! dying. It is *not* fsync-level durability — the crate does not call
//! `sync_data`, so an OS crash or power loss can lose the newest records;
//! recovery then simply resumes from the last record that reached disk.
//!
//! After an unclean exit the file may end in a torn final line;
//! [`Session::open`] drops (and truncates away) an unparseable *final* line
//! and resumes from the last recorded head, while corruption in any earlier
//! (completed) record is rejected. Because there is an unavoidable window
//! between a tool's external side effect and the write of its result entry,
//! tool execution is **at-least-once** across an unclean crash: a tool whose
//! result was not persisted may run again when the conversation is retried.
//! This crate never claims exactly-once execution.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use ygg_ai::{Message, UserMessage, UserPart};

/// Identifier of a session entry. Unique within one session file.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct EntryId(pub String);

/// A parent-linked session entry. `parent: None` marks a root entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    /// This entry's ID.
    pub id: EntryId,
    /// The entry this one follows; `None` for a conversation root.
    pub parent: Option<EntryId>,
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
    },
    /// A configuration marker (not part of model-visible context).
    Config {
        /// Model selection recorded at this point, if any.
        model: Option<String>,
        /// Reasoning setting recorded at this point, if any.
        reasoning: Option<String>,
    },
}

/// One line of the session JSONL file.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionRecord {
    /// An appended entry.
    Entry(Entry),
    /// A durable head update: the current head entry ID.
    Head {
        /// The entry the head now points at.
        id: EntryId,
    },
}

/// Borrowed serialization view used by append/checkout. Keeping the entry
/// borrowed avoids cloning large message payloads solely to write JSONL.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SessionRecordRef<'a> {
    Entry(&'a Entry),
    Head { id: &'a EntryId },
}

fn write_json_line<T: Serialize>(buf: &mut Vec<u8>, record: &T) -> Result<(), SessionError> {
    serde_json::to_writer(&mut *buf, record).map_err(|e| SessionError::Serde(e.to_string()))?;
    buf.push(b'\n');
    Ok(())
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
    /// An operation referenced an entry ID that does not exist.
    #[error("unknown session entry: {0:?}")]
    UnknownEntry(EntryId),
    /// A compaction's `first_kept` is not an ancestor of the current head.
    #[error("entry {0:?} is not an ancestor of the current head")]
    NotAncestor(EntryId),
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
    entries: Vec<Entry>,
    index: HashMap<EntryId, usize>,
    head: Option<EntryId>,
    /// Cached model-visible context. Message appends update it in place;
    /// checkout and compaction invalidate it because they can change the
    /// active branch or summary boundary.
    context_cache: RefCell<Option<Vec<Message>>>,
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
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)?;
        Ok(Self {
            path,
            file,
            entries: Vec::new(),
            index: HashMap::new(),
            head: None,
            context_cache: RefCell::new(None),
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
        let path = path.into();
        let content = std::fs::read_to_string(&path)?;

        let mut entries: Vec<Entry> = Vec::new();
        let mut index: HashMap<EntryId, usize> = HashMap::new();
        let mut head: Option<EntryId> = None;

        // Byte offset of the end of the last accepted record, so a torn tail
        // can be truncated away below.
        let mut valid_end: usize = 0;
        let segments: Vec<&str> = content.split_inclusive('\n').collect();
        for (i, segment) in segments.iter().enumerate() {
            let line_no = i + 1;
            let is_last = i + 1 == segments.len();
            let line = segment.strip_suffix('\n').unwrap_or(segment);
            let record: SessionRecord = match serde_json::from_str(line) {
                Ok(r) => r,
                // Only the final line may be torn by an interrupted write;
                // anything earlier was completed by a successful append.
                Err(_) if is_last => break,
                Err(e) => {
                    return Err(SessionError::Corrupt {
                        line: line_no,
                        message: e.to_string(),
                    })
                }
            };
            valid_end += segment.len();
            match record {
                SessionRecord::Entry(entry) => {
                    if index.contains_key(&entry.id) {
                        return Err(SessionError::Corrupt {
                            line: line_no,
                            message: format!("duplicate entry id {:?}", entry.id.0),
                        });
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
                    entries.push(entry);
                }
                SessionRecord::Head { id } => {
                    if !index.contains_key(&id) {
                        return Err(SessionError::Corrupt {
                            line: line_no,
                            message: format!("head references unknown entry {:?}", id.0),
                        });
                    }
                    head = Some(id);
                }
            }
        }

        if valid_end < content.len() {
            // Torn final line: truncate it away so the next append starts on
            // a fresh line rather than merging into the torn bytes (which
            // would corrupt the record for every later reopen).
            let truncate = OpenOptions::new().write(true).open(&path)?;
            truncate.set_len(valid_end as u64)?;
        }

        let mut file = OpenOptions::new().append(true).open(&path)?;
        if valid_end > 0 && !content[..valid_end].ends_with('\n') {
            // The final record parsed but lost its newline in an interrupted
            // write; complete the line so the next append cannot merge into it.
            file.write_all(b"\n")?;
        }
        Ok(Self {
            path,
            file,
            entries,
            index,
            head,
            context_cache: RefCell::new(None),
        })
    }

    /// The path of the underlying JSONL file.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Appends an entry (parented on the current head) and records the new
    /// head. Writes two JSONL records — the entry, then a head record — in a
    /// single write to the append-only file. The write is process-crash safe
    /// once this returns, but is not fsynced (see the module docs on crash
    /// semantics).
    pub fn append(&mut self, value: EntryValue) -> Result<EntryId, SessionError> {
        let id = EntryId(format!("{:03}", self.entries.len() + 1));
        let entry = Entry {
            id: id.clone(),
            parent: self.head.clone(),
            value,
        };
        let mut buf = Vec::with_capacity(256);
        write_json_line(&mut buf, &SessionRecordRef::Entry(&entry))?;
        write_json_line(&mut buf, &SessionRecordRef::Head { id: &id })?;
        // `File` writes are unbuffered; `write_all` is the persistence point.
        // No fsync — see the module docs on crash semantics.
        self.file.write_all(&buf)?;

        self.index.insert(id.clone(), self.entries.len());
        self.entries.push(entry);
        self.head = Some(id.clone());

        let cache = self.context_cache.get_mut();
        match &self.entries.last().expect("just appended").value {
            EntryValue::Message(message) => {
                if let Some(messages) = cache {
                    append_context_message(messages, message);
                }
            }
            EntryValue::Config { .. } => {}
            EntryValue::Compaction { .. } => *cache = None,
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
        write_json_line(&mut buf, &SessionRecordRef::Head { id: &id })?;
        self.file.write_all(&buf)?;
        self.head = Some(id);
        *self.context_cache.get_mut() = None;
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
        if !self.is_ancestor_of_head(&first_kept) {
            return Err(SessionError::NotAncestor(first_kept));
        }
        self.append(EntryValue::Compaction {
            summary: summary.into(),
            first_kept,
        })
    }

    /// Returns the current head entry ID (`None` for an empty session).
    pub fn head(&self) -> Option<EntryId> {
        self.head.clone()
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
    /// shape.
    pub fn context(&self) -> Result<Vec<Message>, SessionError> {
        if let Some(cached) = self.context_cache.borrow().as_ref() {
            return Ok(cached.clone());
        }

        let mut newest_first: Vec<Message> = Vec::new();
        let mut summaries_newest_first: Vec<String> = Vec::new();
        let mut boundary: Option<EntryId> = None;

        let mut cursor = self.head.clone();
        while let Some(id) = cursor {
            let entry = self
                .entry(&id)
                .ok_or_else(|| SessionError::UnknownEntry(id.clone()))?;
            match &entry.value {
                EntryValue::Message(m) => newest_first.push(m.clone()),
                EntryValue::Config { .. } => {}
                EntryValue::Compaction {
                    summary,
                    first_kept,
                } => {
                    summaries_newest_first.push(summary.clone());
                    // The compaction nearest to the head governs the stop
                    // boundary; an older compaction inside the kept range only
                    // contributes its summary.
                    if boundary.is_none() {
                        boundary = Some(first_kept.clone());
                    }
                }
            }
            if boundary.as_ref() == Some(&id) {
                break;
            }
            cursor = entry.parent.clone();
        }

        let mut messages: Vec<Message> = summaries_newest_first
            .into_iter()
            .rev()
            .map(|summary| {
                Message::User(UserMessage {
                    content: vec![UserPart::Text(format!(
                        "[summary of earlier conversation]\n{summary}"
                    ))],
                })
            })
            .collect();
        messages.extend(newest_first.into_iter().rev());
        let messages = coalesce_tool_results(messages);
        *self.context_cache.borrow_mut() = Some(messages.clone());
        Ok(messages)
    }

    /// True when `id` is the head or one of its ancestors.
    fn is_ancestor_of_head(&self, id: &EntryId) -> bool {
        let mut cursor = self.head.clone();
        while let Some(current) = cursor {
            if &current == id {
                return true;
            }
            cursor = self.entry(&current).and_then(|e| e.parent.clone());
        }
        false
    }
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
    fn create_refuses_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_path(&dir);
        std::fs::write(&path, "").unwrap();
        assert!(matches!(Session::create(&path), Err(SessionError::Io(_))));
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
