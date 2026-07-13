#![allow(missing_docs)]

use ygg_agent::{EntryValue, Session};
use ygg_ai::{AssistantPart, Message, ToolCallId, ToolResultPart, UserPart};

/// One displayable item reconstructed from a session's active branch.
#[derive(Clone, Debug, PartialEq)]
pub enum TranscriptItem {
    User(String),
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
    },
    CompactionMarker {
        summary_preview: String,
    },
}

fn preview(text: &str) -> String {
    const LIMIT: usize = 160;
    let mut end = text.len().min(LIMIT);
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }
    let mut value = text[..end].replace('\n', " ");
    if end < text.len() {
        value.push('…');
    }
    value
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

fn push_message(items: &mut Vec<TranscriptItem>, message: &Message) {
    match message {
        Message::User(user) => {
            let mut text = String::new();
            for part in &user.content {
                match part {
                    UserPart::Text(value) => text.push_str(value),
                    UserPart::ToolResult(result) => items.push(TranscriptItem::ToolResult {
                        id: result.tool_call_id.clone(),
                        text: tool_result_text(&result.content),
                        is_error: result.is_error,
                    }),
                    UserPart::Media(_) => {}
                }
            }
            if !text.is_empty() {
                items.push(TranscriptItem::User(text));
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
                    AssistantPart::Media(_) => {}
                }
            }
        }
    }
}

/// Rebuild the display transcript by walking head-to-root on the active branch
/// and reversing it into chronological order. Abandoned branches never appear.
pub fn hydrate_transcript(session: &Session) -> anyhow::Result<Vec<TranscriptItem>> {
    let mut entries = Vec::new();
    let mut cursor = session.head();
    while let Some(id) = cursor {
        let entry = session
            .entry(&id)
            .ok_or_else(|| anyhow::anyhow!("dangling session entry {}", id.0))?;
        entries.push(entry);
        cursor = entry.parent.clone();
    }
    entries.reverse();

    let mut items = Vec::new();
    for entry in entries {
        match &entry.value {
            EntryValue::Message(message) => push_message(&mut items, message),
            EntryValue::Compaction { summary, .. } => {
                items.push(TranscriptItem::CompactionMarker {
                    summary_preview: preview(summary),
                });
            }
            EntryValue::Config { .. } => {}
        }
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ygg_agent::{EntryValue, Session};
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
                TranscriptItem::User("active question".into()),
                TranscriptItem::Assistant("active answer".into()),
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
                })],
            })))
            .unwrap();

        let items = hydrate_transcript(&session).unwrap();
        assert!(matches!(items[1], TranscriptItem::ToolCall { ref name, .. } if name == "read"));
        assert!(
            matches!(items[2], TranscriptItem::ToolResult { ref text, .. } if text == "contents")
        );
    }
}
