//! Pi-compatible structured handoffs for context compaction.
//!
//! The summarizer sees a serialized transcript, never a live conversation to
//! continue. The model produces the structured Markdown checkpoint; Ygg tracks
//! file operations deterministically and appends the XML file lists host-side.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use ygg_ai::{AssistantPart, Media, Message, ToolResult, ToolResultPart, UserMessage, UserPart};

use crate::session::{Entry, EntryId, EntryValue, Session, SessionError};

/// System instruction shared by every compaction/handoff summarizer call.
pub const SUMMARIZATION_SYSTEM_PROMPT: &str = r#"You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.

Do NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary."#;

/// Default amount of recent model-visible conversation retained verbatim.
pub const DEFAULT_KEEP_RECENT_TOKENS: u64 = 20_000;
/// Maximum output budget for the main structured checkpoint summary.
pub const SUMMARY_OUTPUT_TOKENS: u64 = 13_107; // floor(0.8 * Pi's 16,384-token reserve)
/// Maximum output budget for a split-turn prefix summary.
pub const TURN_PREFIX_OUTPUT_TOKENS: u64 = 8_192; // floor(0.5 * Pi's 16,384-token reserve)

const SUMMARIZATION_PROMPT: &str = r#"The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.

Use this EXACT format:

## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by user]
- [Or "(none)" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or "(none)" if not applicable]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

const UPDATE_SUMMARIZATION_PROMPT: &str = r#"The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.

Update the existing structured summary with new information. RULES:
- PRESERVE all existing information from the previous summary
- ADD new progress, decisions, and context from the new messages
- UPDATE the Progress section: move items from "In Progress" to "Done" when completed
- UPDATE "Next Steps" based on what was accomplished
- PRESERVE exact file paths, function names, and error messages
- If something is no longer relevant, you may remove it

Use this EXACT format:

## Goal
[Preserve existing goals, add new ones if the task expanded]

## Constraints & Preferences
- [Preserve existing, add new ones discovered]

## Progress
### Done
- [x] [Include previously done items AND newly completed items]

### In Progress
- [ ] [Current work - update based on progress]

### Blocked
- [Current blockers - remove if resolved]

## Key Decisions
- **[Decision]**: [Brief rationale] (preserve all previous, add new)

## Next Steps
1. [Update based on current state]

## Critical Context
- [Preserve important context, add new if needed]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

/// Preamble Pi places before summaries of abandoned conversation branches.
pub const BRANCH_SUMMARY_PREAMBLE: &str = r#"The user explored a different conversation branch before returning here.
Summary of that exploration:

"#;

const TURN_PREFIX_SUMMARIZATION_PROMPT: &str = r#"This is the PREFIX of a turn that was too large to keep. The SUFFIX (recent work) is retained.

Summarize the prefix to provide context for the retained suffix:

## Original Request
[What did the user ask for in this turn?]

## Early Progress
- [Key decisions and work done in the prefix]

## Context for Suffix
- [Information needed to understand the kept recent work]

Be concise. Focus on what's needed to understand the kept suffix."#;

const BRANCH_SUMMARY_PROMPT: &str = r#"Create a structured summary of this conversation branch for context when returning later.

Use this EXACT format:

## Goal
[What was the user trying to accomplish in this branch?]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned]
- [Or "(none)" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Work that was started but not finished]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [What should happen next to continue this work]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

const TOOL_RESULT_MAX_CHARS: usize = 2_000;

/// Cumulative file-operation details persisted with a compaction checkpoint.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionDetails {
    /// Files read but never modified in the represented history.
    pub read_files: Vec<String>,
    /// Files written or edited in the represented history.
    pub modified_files: Vec<String>,
}

/// Owned input for one structured handoff request.
#[derive(Clone, Debug)]
pub struct HandoffPreparation {
    /// Older messages being folded into the checkpoint.
    pub messages: Vec<Message>,
    /// Prefix of a turn whose recent suffix begins at the kept boundary.
    pub turn_prefix_messages: Vec<Message>,
    /// Previous structured checkpoint, when this updates an earlier compaction.
    pub previous_summary: Option<String>,
    /// Cumulative deterministic file lists for the resulting checkpoint.
    pub details: CompactionDetails,
}

/// Owned source material for a Pi-compatible abandoned-branch summary.
#[derive(Clone, Debug)]
pub struct BranchHandoffPreparation {
    /// Chronological branch messages selected for summarization.
    pub messages: Vec<Message>,
    /// Cumulative deterministic file lists for the branch.
    pub details: CompactionDetails,
}

#[derive(Default)]
struct FileOperations {
    read: BTreeSet<String>,
    written: BTreeSet<String>,
    edited: BTreeSet<String>,
}

impl FileOperations {
    fn from_details(details: &CompactionDetails) -> Self {
        Self {
            read: details.read_files.iter().cloned().collect(),
            written: BTreeSet::new(),
            edited: details.modified_files.iter().cloned().collect(),
        }
    }

    fn finish(self) -> CompactionDetails {
        let modified = self
            .edited
            .union(&self.written)
            .cloned()
            .collect::<BTreeSet<_>>();
        let read_files = self.read.difference(&modified).cloned().collect();
        let modified_files = modified.into_iter().collect();
        CompactionDetails {
            read_files,
            modified_files,
        }
    }
}

fn extract_file_operations(message: &Message, operations: &mut FileOperations) {
    let Message::Assistant(assistant) = message else {
        return;
    };
    for part in &assistant.content {
        let AssistantPart::ToolCall(call) = part else {
            continue;
        };
        // These calls were rejected before dispatch, so their paths must not
        // be represented as completed workspace operations in a handoff.
        if call.argument_error.is_some() {
            continue;
        }
        let Ok(arguments) = call.arguments_value() else {
            continue;
        };
        let Some(path) = arguments.get("path").and_then(serde_json::Value::as_str) else {
            continue;
        };
        match call.name.as_str() {
            "read" => {
                operations.read.insert(path.to_owned());
            }
            "write" => {
                operations.written.insert(path.to_owned());
            }
            "edit" => {
                operations.edited.insert(path.to_owned());
            }
            _ => {}
        }
    }
}

fn active_branch_entries(session: &Session) -> Result<Vec<&Entry>, SessionError> {
    let mut reverse = Vec::new();
    let mut cursor = session.head();
    while let Some(id) = cursor {
        let entry = session
            .entry(&id)
            .ok_or_else(|| SessionError::UnknownEntry(id.clone()))?;
        reverse.push(entry);
        cursor = entry.parent.clone();
    }
    reverse.reverse();
    Ok(reverse)
}

/// Prepare the exact history increment and cumulative file details represented
/// by a compaction whose full-fidelity tail begins at `first_kept`.
///
/// Repeated compactions follow Pi's iterative rule: the previous summary is
/// supplied separately, while messages from its kept boundary up to the new
/// boundary are summarized again so no once-retained work disappears.
pub fn prepare_handoff(
    session: &Session,
    first_kept: &EntryId,
) -> Result<HandoffPreparation, SessionError> {
    let branch = active_branch_entries(session)?;
    let first_kept_index = branch
        .iter()
        .position(|entry| &entry.id == first_kept)
        .ok_or_else(|| SessionError::UnknownEntry(first_kept.clone()))?;

    let previous = branch.iter().rev().find_map(|entry| match &entry.value {
        EntryValue::Compaction {
            summary,
            first_kept,
            details,
            ..
        } => Some((summary.clone(), first_kept.clone(), details.clone())),
        _ => None,
    });

    let (previous_summary, boundary_start, mut operations) = match previous {
        Some((summary, previous_first_kept, details)) => {
            let start = branch
                .iter()
                .position(|entry| entry.id == previous_first_kept)
                .ok_or(SessionError::UnknownEntry(previous_first_kept))?;
            (
                Some(summary),
                start.min(first_kept_index),
                FileOperations::from_details(&details),
            )
        }
        None => (None, 0, FileOperations::default()),
    };

    // An assistant cut point splits the turn that started at the nearest
    // preceding user message. This applies both to a first assistant reply
    // and to a later assistant suffix after tool results; Pi does not limit
    // split detection to tool-result boundaries.
    let split_turn_start = if first_kept_index > boundary_start
        && matches!(
            &branch[first_kept_index].value,
            EntryValue::Message(Message::Assistant(_))
        ) {
        (boundary_start..first_kept_index)
            .rev()
            .find(|index| match &branch[*index].value {
                EntryValue::Message(message) => is_turn_start_user(message),
                _ => false,
            })
    } else {
        None
    };

    let history_end = split_turn_start.unwrap_or(first_kept_index);
    let mut messages = Vec::new();
    for entry in &branch[boundary_start..history_end] {
        if let EntryValue::Message(message) = &entry.value {
            extract_file_operations(message, &mut operations);
            messages.push(message.clone());
        }
    }

    let mut turn_prefix_messages = Vec::new();
    if let Some(turn_start) = split_turn_start {
        for entry in &branch[turn_start..first_kept_index] {
            if let EntryValue::Message(message) = &entry.value {
                extract_file_operations(message, &mut operations);
                turn_prefix_messages.push(message.clone());
            }
        }
    }

    Ok(HandoffPreparation {
        messages,
        turn_prefix_messages,
        previous_summary,
        details: operations.finish(),
    })
}

fn media_summary(media: &Media) -> String {
    match media {
        Media::Image(_) => "[image]".to_owned(),
        Media::Audio(audio) => audio
            .transcript
            .as_ref()
            .filter(|transcript| !transcript.trim().is_empty())
            .map(|transcript| format!("[audio transcript]: {transcript}"))
            .unwrap_or_else(|| "[audio]".to_owned()),
    }
}

fn truncate_for_summary(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_owned();
    }
    let prefix = text.chars().take(max_chars).collect::<String>();
    format!(
        "{prefix}\n\n[... {} more characters truncated]",
        count - max_chars
    )
}

fn tool_result_text(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .map(|part| match part {
            ToolResultPart::Text(text) => text.clone(),
            ToolResultPart::Media(media) => media_summary(media),
        })
        .collect::<Vec<_>>()
        .join("")
}

fn tool_call_text(call: &ygg_ai::ToolCall) -> String {
    let arguments = call.arguments_value().ok();
    let rendered = arguments
        .as_ref()
        .and_then(serde_json::Value::as_object)
        .map(|arguments| {
            arguments
                .iter()
                .map(|(name, value)| {
                    let value = serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned());
                    format!("{name}={value}")
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_else(|| format!("arguments={}", call.arguments_json));
    format!("{}({rendered})", call.name)
}

/// Serialize messages as labelled transcript text so the summary model treats
/// them as source material rather than a conversation to continue.
///
/// Tool results are capped at 2,000 characters, matching Pi's compaction
/// playbook. Exact paths, tool arguments, reasoning text, and visible replies
/// remain available to the summarizer.
pub fn serialize_conversation(messages: &[Message]) -> String {
    let mut sections = Vec::new();
    for message in messages {
        match message {
            Message::User(user) => {
                let mut user_content = Vec::new();
                for part in &user.content {
                    match part {
                        UserPart::Text(text) => user_content.push(text.clone()),
                        UserPart::Media(media) => user_content.push(media_summary(media)),
                        UserPart::ToolResult(result) => {
                            let text = tool_result_text(result);
                            if !text.is_empty() {
                                sections.push(format!(
                                    "[Tool result]: {}",
                                    truncate_for_summary(&text, TOOL_RESULT_MAX_CHARS)
                                ));
                            }
                        }
                    }
                }
                if !user_content.is_empty() {
                    sections.push(format!("[User]: {}", user_content.join("")));
                }
            }
            Message::Assistant(assistant) => {
                let mut thinking = Vec::new();
                let mut text = Vec::new();
                let mut calls = Vec::new();
                for part in &assistant.content {
                    match part {
                        AssistantPart::Text(value) => text.push(value.clone()),
                        AssistantPart::Reasoning(reasoning) => {
                            if let Some(value) = &reasoning.text {
                                thinking.push(value.clone());
                            }
                        }
                        AssistantPart::ToolCall(call) => calls.push(tool_call_text(call)),
                        AssistantPart::Media(media) => text.push(media_summary(media)),
                    }
                }
                if !thinking.is_empty() {
                    sections.push(format!("[Assistant thinking]: {}", thinking.join("\n")));
                }
                if !text.is_empty() {
                    sections.push(format!("[Assistant]: {}", text.join("")));
                }
                if !calls.is_empty() {
                    sections.push(format!("[Assistant tool calls]: {}", calls.join("; ")));
                }
            }
        }
    }
    sections.join("\n\n")
}

fn is_turn_start_user(message: &Message) -> bool {
    matches!(
        message,
        Message::User(user)
            if !user.content.is_empty()
                && user
                    .content
                    .iter()
                    .any(|part| !matches!(part, UserPart::ToolResult(_)))
    )
}

fn model_visible_branch_entries(session: &Session) -> Result<Vec<&Entry>, SessionError> {
    let branch = active_branch_entries(session)?;
    let first_kept = branch.iter().rev().find_map(|entry| match &entry.value {
        EntryValue::Compaction { first_kept, .. } => Some(first_kept),
        _ => None,
    });
    let start = first_kept
        .and_then(|first_kept| branch.iter().position(|entry| &entry.id == first_kept))
        .unwrap_or_default();
    Ok(branch.into_iter().skip(start).collect())
}

/// Select the first retained entry using Pi's token-budgeted backward walk.
///
/// Candidates are user turn starts and assistant messages, while tool-result
/// messages are never selected so the retained context stays protocol-valid.
/// The fallback keeps from the oldest visible candidate when the requested
/// token budget exceeds the visible history.
pub fn choose_first_kept_by_tokens<F>(
    session: &Session,
    keep_recent_tokens: u64,
    estimate_message_tokens: F,
) -> Result<Option<EntryId>, SessionError>
where
    F: Fn(&Message) -> u64,
{
    let entries = model_visible_branch_entries(session)?;
    let mut candidates = Vec::new();
    // Pi treats user messages and assistant messages as valid cut points, but
    // never cuts at a tool result because it must remain paired with its call.
    for (index, entry) in entries.iter().enumerate() {
        let is_cut_point = match &entry.value {
            EntryValue::Message(message) => {
                matches!(message, Message::Assistant(_)) || is_turn_start_user(message)
            }
            _ => false,
        };
        if is_cut_point {
            candidates.push((index, entry.id.clone()));
        }
    }
    let budget = keep_recent_tokens.max(1);
    let mut accumulated = 0u64;
    for index in (0..entries.len()).rev() {
        if let EntryValue::Message(message) = &entries[index].value {
            accumulated = accumulated.saturating_add(estimate_message_tokens(message));
        }
        if accumulated >= budget {
            if let Some((_, id)) = candidates.iter().find(|(candidate, _)| *candidate >= index) {
                return Ok(Some(id.clone()));
            }
            break;
        }
    }

    // Match Pi's fallback when the visible history is smaller than the
    // requested tail: keep from its oldest valid cut point. The caller still
    // skips the attempt when that leaves no history to summarize.
    Ok(candidates.first().map(|(_, id)| id.clone()))
}

/// Build the single user message sent to the tool-free summary model.
pub fn build_handoff_message(preparation: &HandoffPreparation) -> Message {
    let conversation = serialize_conversation(&preparation.messages);
    let mut prompt = format!("<conversation>\n{conversation}\n</conversation>\n\n");
    if let Some(previous) = &preparation.previous_summary {
        prompt.push_str("<previous-summary>\n");
        prompt.push_str(previous);
        prompt.push_str("\n</previous-summary>\n\n");
        prompt.push_str(UPDATE_SUMMARIZATION_PROMPT);
    } else {
        prompt.push_str(SUMMARIZATION_PROMPT);
    }
    Message::User(UserMessage {
        content: vec![UserPart::Text(prompt)],
    })
}

/// Build the dedicated Pi-style summary request for a split turn prefix.
pub fn build_turn_prefix_handoff_message(messages: &[Message]) -> Message {
    let conversation = serialize_conversation(messages);
    Message::User(UserMessage {
        content: vec![UserPart::Text(format!(
            "<conversation>\n{conversation}\n</conversation>\n\n{TURN_PREFIX_SUMMARIZATION_PROMPT}"
        ))],
    })
}

/// Prepare branch-summary source messages and cumulative file details.
///
/// `inherited_details` represents nested Pi-generated branch summaries. Tool
/// result messages are omitted from the branch transcript, as in Pi; assistant
/// tool calls remain available for deterministic file tracking.
pub fn prepare_branch_handoff(
    messages: Vec<Message>,
    inherited_details: &CompactionDetails,
) -> BranchHandoffPreparation {
    let mut operations = FileOperations::from_details(inherited_details);
    for message in &messages {
        extract_file_operations(message, &mut operations);
    }
    let messages = messages
        .into_iter()
        .filter(|message| {
            !matches!(
                message,
                Message::User(user)
                    if !user.content.is_empty()
                        && user
                            .content
                            .iter()
                            .all(|part| matches!(part, UserPart::ToolResult(_)))
            )
        })
        .collect();
    BranchHandoffPreparation {
        messages,
        details: operations.finish(),
    }
}

/// Build Pi's exact structured abandoned-branch summary request message.
pub fn build_branch_handoff_message(preparation: &BranchHandoffPreparation) -> Message {
    let conversation = serialize_conversation(&preparation.messages);
    Message::User(UserMessage {
        content: vec![UserPart::Text(format!(
            "<conversation>\n{conversation}\n</conversation>\n\n{BRANCH_SUMMARY_PROMPT}"
        ))],
    })
}

/// Prefix a branch summary and append host-derived XML file lists exactly as Pi
/// does before handing the abandoned work to the selected branch.
pub fn finish_branch_handoff(summary: String, details: &CompactionDetails) -> String {
    format!(
        "{BRANCH_SUMMARY_PREAMBLE}{summary}{}",
        format_file_operations(details)
    )
}

/// Format deterministic cumulative file lists using Pi's XML handoff tags.
pub fn format_file_operations(details: &CompactionDetails) -> String {
    let mut sections = Vec::new();
    if !details.read_files.is_empty() {
        sections.push(format!(
            "<read-files>\n{}\n</read-files>",
            details.read_files.join("\n")
        ));
    }
    if !details.modified_files.is_empty() {
        sections.push(format!(
            "<modified-files>\n{}\n</modified-files>",
            details.modified_files.join("\n")
        ));
    }
    if sections.is_empty() {
        String::new()
    } else {
        format!("\n\n{}", sections.join("\n\n"))
    }
}

/// Append host-derived XML file lists to a model-produced structured summary.
pub fn finish_handoff(summary: String, details: &CompactionDetails) -> String {
    format!("{summary}{}", format_file_operations(details))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ygg_ai::{
        AssistantMessage, AudioFormat, AudioMedia, AudioPayload, ModelId, Protocol, ToolCall,
        ToolCallArgumentError, ToolCallId, ToolResult, ToolResultPart,
    };

    fn assistant(parts: Vec<AssistantPart>) -> Message {
        Message::Assistant(AssistantMessage {
            content: parts,
            model: ModelId("test".into()),
            protocol: Protocol::OpenAiChat,
        })
    }

    fn file_call(
        id: &str,
        name: &str,
        path: &str,
        argument_error: Option<ToolCallArgumentError>,
    ) -> AssistantPart {
        AssistantPart::ToolCall(ToolCall {
            id: ToolCallId(id.into()),
            name: name.into(),
            arguments_json: serde_json::json!({"path": path}).to_string(),
            argument_error,
        })
    }

    fn file_operation_messages() -> Vec<Message> {
        let tool_result = |id: &str, is_error| {
            let text = if is_error {
                "schema rejected"
            } else {
                "executed"
            };
            UserPart::ToolResult(ToolResult {
                tool_call_id: ToolCallId(id.into()),
                content: vec![ToolResultPart::Text(text.into())],
                is_error,
                added_tool_names: None,
            })
        };
        vec![
            Message::User(UserMessage {
                content: vec![UserPart::Text("update files".into())],
            }),
            assistant(vec![
                file_call("read", "read", "src/read.rs", None),
                file_call("write", "write", "src/write.rs", None),
                file_call("edit", "edit", "src/edit.rs", None),
                file_call(
                    "rejected-read",
                    "read",
                    "src/rejected-read.rs",
                    Some(ToolCallArgumentError::SchemaMismatch),
                ),
                file_call(
                    "rejected-write",
                    "write",
                    "src/rejected-write.rs",
                    Some(ToolCallArgumentError::SchemaMismatch),
                ),
                file_call(
                    "rejected-edit",
                    "edit",
                    "src/rejected-edit.rs",
                    Some(ToolCallArgumentError::SchemaMismatch),
                ),
            ]),
            Message::User(UserMessage {
                content: vec![
                    tool_result("read", false),
                    tool_result("write", false),
                    tool_result("edit", false),
                    tool_result("rejected-read", true),
                    tool_result("rejected-write", true),
                    tool_result("rejected-edit", true),
                ],
            }),
        ]
    }

    fn assert_file_operation_details(details: &CompactionDetails) {
        assert_eq!(details.read_files, vec!["src/read.rs"]);
        assert_eq!(details.modified_files, vec!["src/edit.rs", "src/write.rs"]);
    }

    #[test]
    fn token_budget_selects_a_user_boundary_without_counting_turns() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("session.jsonl")).unwrap();
        let mut user_ids = Vec::new();
        for index in 0..3 {
            user_ids.push(
                session
                    .append(EntryValue::Message(Message::User(UserMessage {
                        content: vec![UserPart::Text(format!("request {index}"))],
                    })))
                    .unwrap(),
            );
            session
                .append(EntryValue::Message(assistant(vec![AssistantPart::Text(
                    format!("answer {index}"),
                )])))
                .unwrap();
        }

        let selected = choose_first_kept_by_tokens(&session, 20, |_| 10)
            .unwrap()
            .expect("three turns provide a retained boundary");
        assert_eq!(selected, user_ids[2]);
    }

    #[test]
    fn turn_prefix_handoff_uses_the_dedicated_split_turn_contract() {
        let Message::User(user) =
            build_turn_prefix_handoff_message(&[Message::User(UserMessage {
                content: vec![UserPart::Text("original request".into())],
            })])
        else {
            panic!("handoff is a user message");
        };
        let UserPart::Text(prompt) = &user.content[0] else {
            panic!("handoff is text");
        };
        assert!(prompt.contains("## Original Request"));
        assert!(prompt.contains("## Early Progress"));
        assert!(prompt.contains("## Context for Suffix"));
        assert!(prompt.contains("original request"));
    }

    #[test]
    fn handoff_prompt_uses_the_exact_structured_checkpoint_contract() {
        let message = build_handoff_message(&HandoffPreparation {
            messages: vec![Message::User(UserMessage {
                content: vec![UserPart::Text("fix it".into())],
            })],
            turn_prefix_messages: Vec::new(),
            previous_summary: None,
            details: CompactionDetails::default(),
        });
        let Message::User(user) = message else {
            panic!("handoff is a user message");
        };
        let UserPart::Text(prompt) = &user.content[0] else {
            panic!("handoff is text");
        };
        assert!(prompt.starts_with("<conversation>\n[User]: fix it\n</conversation>"));
        for heading in [
            "## Goal",
            "## Constraints & Preferences",
            "## Progress",
            "### Done",
            "### In Progress",
            "### Blocked",
            "## Key Decisions",
            "## Next Steps",
            "## Critical Context",
        ] {
            assert!(prompt.contains(heading), "missing {heading}: {prompt}");
        }
    }

    #[test]
    fn repeated_handoff_uses_previous_summary_update_contract() {
        let message = build_handoff_message(&HandoffPreparation {
            messages: Vec::new(),
            turn_prefix_messages: Vec::new(),
            previous_summary: Some("## Goal\nkeep this".into()),
            details: CompactionDetails::default(),
        });
        let Message::User(user) = message else {
            panic!("handoff is a user message");
        };
        let UserPart::Text(prompt) = &user.content[0] else {
            panic!("handoff is text");
        };
        assert!(prompt.contains("<previous-summary>\n## Goal\nkeep this\n</previous-summary>"));
        assert!(prompt.contains("PRESERVE all existing information"));
    }

    #[test]
    fn branch_handoff_uses_pi_prompt_preamble_and_host_file_lists() {
        let preparation = prepare_branch_handoff(
            vec![
                assistant(vec![AssistantPart::ToolCall(ToolCall {
                    id: ToolCallId("edit".into()),
                    name: "edit".into(),
                    arguments_json: r#"{"path":"src/lib.rs"}"#.into(),
                    argument_error: None,
                })]),
                Message::User(UserMessage {
                    content: vec![UserPart::ToolResult(ToolResult {
                        tool_call_id: ToolCallId("edit".into()),
                        content: vec![ToolResultPart::Text("done".into())],
                        is_error: false,
                        added_tool_names: None,
                    })],
                }),
            ],
            &CompactionDetails::default(),
        );
        assert_eq!(preparation.messages.len(), 1, "tool result is omitted");
        assert_eq!(preparation.details.modified_files, vec!["src/lib.rs"]);

        let Message::User(user) = build_branch_handoff_message(&preparation) else {
            panic!("branch handoff is a user message");
        };
        let UserPart::Text(prompt) = &user.content[0] else {
            panic!("branch handoff is text");
        };
        assert!(prompt.contains(
            "Create a structured summary of this conversation branch for context when returning later."
        ));
        assert!(prompt.contains("## Goal"));
        assert!(prompt.contains("## Next Steps"));
        assert!(!prompt.contains("## Critical Context"));

        assert_eq!(
            finish_branch_handoff("## Goal\nbranch work".into(), &preparation.details),
            "The user explored a different conversation branch before returning here.\nSummary of that exploration:\n\n## Goal\nbranch work\n\n<modified-files>\nsrc/lib.rs\n</modified-files>"
        );
    }

    #[test]
    fn normal_handoff_omits_schema_rejected_file_operations() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("session.jsonl")).unwrap();
        for message in file_operation_messages() {
            session.append(EntryValue::Message(message)).unwrap();
        }
        let first_kept = session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Text("continue".into())],
            })))
            .unwrap();

        let preparation = prepare_handoff(&session, &first_kept).unwrap();

        assert_file_operation_details(&preparation.details);
    }

    #[test]
    fn branch_handoff_omits_schema_rejected_file_operations() {
        let preparation =
            prepare_branch_handoff(file_operation_messages(), &CompactionDetails::default());

        assert_eq!(preparation.messages.len(), 2, "tool results are omitted");
        assert_file_operation_details(&preparation.details);
    }

    #[test]
    fn serialization_labels_calls_and_truncates_tool_results() {
        let messages = vec![
            assistant(vec![AssistantPart::ToolCall(ToolCall {
                id: ToolCallId("call".into()),
                name: "read".into(),
                arguments_json: r#"{"path":"src/lib.rs"}"#.into(),
                argument_error: None,
            })]),
            Message::User(UserMessage {
                content: vec![UserPart::ToolResult(ToolResult {
                    tool_call_id: ToolCallId("call".into()),
                    content: vec![ToolResultPart::Text("x".repeat(2_100))],
                    is_error: false,
                    added_tool_names: None,
                })],
            }),
        ];
        let serialized = serialize_conversation(&messages);
        assert!(serialized.contains("[Assistant tool calls]: read(path=\"src/lib.rs\")"));
        assert!(serialized.contains("[Tool result]:"));
        assert!(serialized.contains("[... 100 more characters truncated]"));
    }

    #[test]
    fn serialization_preserves_audio_transcripts_for_every_message_role() {
        let audio = |transcript: &str| {
            Media::Audio(AudioMedia {
                payload: AudioPayload::Inline(bytes::Bytes::from_static(b"audio")),
                format: AudioFormat::Wav,
                transcript: Some(transcript.into()),
            })
        };
        let messages = vec![
            Message::User(UserMessage {
                content: vec![
                    UserPart::Media(audio("user recording")),
                    UserPart::ToolResult(ToolResult {
                        tool_call_id: ToolCallId("call".into()),
                        content: vec![ToolResultPart::Media(audio("tool recording"))],
                        is_error: false,
                        added_tool_names: None,
                    }),
                ],
            }),
            assistant(vec![AssistantPart::Media(audio("assistant recording"))]),
        ];

        let serialized = serialize_conversation(&messages);
        assert!(serialized.contains("[User]: [audio transcript]: user recording"));
        assert!(serialized.contains("[Tool result]: [audio transcript]: tool recording"));
        assert!(serialized.contains("[Assistant]: [audio transcript]: assistant recording"));
    }

    #[test]
    fn file_lists_are_sorted_cumulative_and_modified_wins_over_read() {
        let mut operations = FileOperations::from_details(&CompactionDetails {
            read_files: vec!["z.rs".into(), "same.rs".into()],
            modified_files: vec!["old.rs".into()],
        });
        extract_file_operations(
            &assistant(vec![
                AssistantPart::ToolCall(ToolCall {
                    id: ToolCallId("read".into()),
                    name: "read".into(),
                    arguments_json: r#"{"path":"a.rs"}"#.into(),
                    argument_error: None,
                }),
                AssistantPart::ToolCall(ToolCall {
                    id: ToolCallId("edit".into()),
                    name: "edit".into(),
                    arguments_json: r#"{"path":"same.rs","old":"a","new":"b"}"#.into(),
                    argument_error: None,
                }),
            ]),
            &mut operations,
        );
        let details = operations.finish();
        assert_eq!(details.read_files, vec!["a.rs", "z.rs"]);
        assert_eq!(details.modified_files, vec!["old.rs", "same.rs"]);
        assert_eq!(
            format_file_operations(&details),
            "\n\n<read-files>\na.rs\nz.rs\n</read-files>\n\n<modified-files>\nold.rs\nsame.rs\n</modified-files>"
        );
    }

    #[test]
    fn iterative_preparation_merges_previous_details_and_new_messages() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("session.jsonl")).unwrap();
        session
            .append(EntryValue::Message(assistant(vec![AssistantPart::Text(
                "earlier".into(),
            )])))
            .unwrap();
        let previous_first_kept = session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Text("previous retained turn".into())],
            })))
            .unwrap();
        session
            .compact_with_details(
                "## Goal\nprevious checkpoint",
                previous_first_kept,
                CompactionDetails {
                    read_files: vec!["same.rs".into(), "z.rs".into()],
                    modified_files: vec!["old.rs".into()],
                },
            )
            .unwrap();
        session
            .append(EntryValue::Message(assistant(vec![
                AssistantPart::ToolCall(ToolCall {
                    id: ToolCallId("read".into()),
                    name: "read".into(),
                    arguments_json: r#"{"path":"a.rs"}"#.into(),
                    argument_error: None,
                }),
                AssistantPart::ToolCall(ToolCall {
                    id: ToolCallId("edit".into()),
                    name: "edit".into(),
                    arguments_json: r#"{"path":"same.rs"}"#.into(),
                    argument_error: None,
                }),
                AssistantPart::ToolCall(ToolCall {
                    id: ToolCallId("write".into()),
                    name: "write".into(),
                    arguments_json: r#"{"path":"b.rs"}"#.into(),
                    argument_error: None,
                }),
            ])))
            .unwrap();
        let next_first_kept = session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Text("next retained turn".into())],
            })))
            .unwrap();

        let preparation = prepare_handoff(&session, &next_first_kept).unwrap();
        assert_eq!(
            preparation.previous_summary.as_deref(),
            Some("## Goal\nprevious checkpoint")
        );
        assert_eq!(preparation.messages.len(), 2);
        assert_eq!(preparation.details.read_files, vec!["a.rs", "z.rs"]);
        assert_eq!(
            preparation.details.modified_files,
            vec!["b.rs", "old.rs", "same.rs"]
        );
    }

    #[test]
    fn compaction_details_use_pi_field_names_and_default_for_legacy_entries() {
        let value = EntryValue::Compaction {
            summary: "summary".into(),
            first_kept: EntryId("1".into()),
            active_skills: Vec::new(),
            skill_resources: Vec::new(),
            details: CompactionDetails {
                read_files: vec!["read.rs".into()],
                modified_files: vec!["changed.rs".into()],
            },
        };
        let mut json = serde_json::to_value(value).unwrap();
        assert_eq!(json["details"]["readFiles"][0], "read.rs");
        assert_eq!(json["details"]["modifiedFiles"][0], "changed.rs");

        json.as_object_mut().unwrap().remove("details");
        let legacy: EntryValue = serde_json::from_value(json).unwrap();
        let EntryValue::Compaction { details, .. } = legacy else {
            panic!("expected compaction");
        };
        assert_eq!(details, CompactionDetails::default());
    }
}
