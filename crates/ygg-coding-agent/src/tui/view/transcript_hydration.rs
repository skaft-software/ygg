//! Durable transcript-item projection into the interactive shell model.

use std::time::Duration;

use super::{
    bounded_append, sanitize_for_terminal, AssistantBlock, CompactionBlock, ShellState, ToolPanel,
    TranscriptBlock,
};
use crate::hydrate::TranscriptItem;
use crate::presentation::{
    summarize_tool, summarize_tool_with_workspace, tool_failure_reason, tool_result_is_failure,
};

fn apply_hydrated_tool_result(panel: &mut ToolPanel, text: &str, is_error: bool) {
    panel.finished = true;
    let replayed = Ok(ygg_agent::ToolOutput::new(text.to_owned()));
    panel.is_error = is_error || tool_result_is_failure(&panel.name, &replayed);
    if !panel.is_error {
        panel.display.mark_media_read_from_result(text);
    }
    panel.failure_reason = if is_error {
        tool_failure_reason(
            &panel.name,
            &Err(ygg_agent::ToolError::new(text.to_owned())),
        )
    } else {
        tool_failure_reason(&panel.name, &replayed)
    };
    bounded_append(&mut panel.output, text);
}

pub(super) fn append_hydrated_items(
    state: &mut ShellState,
    items: impl IntoIterator<Item = TranscriptItem>,
) {
    for item in items {
        match item {
            TranscriptItem::User {
                text,
                model_lab,
                prompt_color,
            } => {
                state.push_block(TranscriptBlock::User {
                    text,
                    model_lab,
                    prompt_color,
                    persisted: true,
                });
            }
            TranscriptItem::Assistant(text) => state.push_block(TranscriptBlock::Assistant(
                Box::new(AssistantBlock::finalized(text)),
            )),
            TranscriptItem::Reasoning(text) => state.push_block(TranscriptBlock::Reasoning(
                Box::new(AssistantBlock::finalized_reasoning(text)),
            )),
            TranscriptItem::ToolCall { id, name, args } => {
                let index = state.transcript.len();
                let display =
                    summarize_tool_with_workspace(&name, &args, state.workspace.as_deref());
                let model_lab = state.model_lab;
                state.push_block(TranscriptBlock::Tool(Box::new(ToolPanel::new(
                    id.clone(),
                    name,
                    args.to_string(),
                    display,
                    String::new(),
                    false,
                    false,
                    None,
                    model_lab,
                ))));
                state.tool_panels.insert(id, index);
            }
            TranscriptItem::ToolResult {
                id,
                text,
                is_error,
                duration_ms,
            } => {
                // Malformed provider output can reuse one call ID within the
                // same assistant turn. The durable protocol cannot identify
                // which duplicate a result belongs to, so conservatively close
                // every still-open matching card. Leaving an older duplicate
                // active would revive a spinner for work that cannot still be
                // running after process restart.
                let pending = state
                    .transcript
                    .iter()
                    .enumerate()
                    .filter_map(|(index, block)| match block {
                        TranscriptBlock::Tool(panel) if panel.id == id && !panel.finished => {
                            Some(index)
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                if !pending.is_empty() {
                    for index in pending {
                        if let Some(TranscriptBlock::Tool(panel)) = state.transcript.get_mut(index)
                        {
                            apply_hydrated_tool_result(panel, &text, is_error);
                            panel.duration =
                                duration_ms.map(|millis| Duration::from_millis(millis));
                        }
                    }
                } else if let Some(panel) = state.tool_output_mut(&id) {
                    apply_hydrated_tool_result(panel, &text, is_error);
                    panel.duration = duration_ms.map(|millis| Duration::from_millis(millis));
                } else {
                    let index = state.transcript.len();
                    let model_lab = state.model_lab;
                    state.push_block(TranscriptBlock::Tool(Box::new(ToolPanel::new(
                        id.clone(),
                        "tool result".into(),
                        String::new(),
                        summarize_tool("tool result", &serde_json::Value::Null),
                        sanitize_for_terminal(&text),
                        true,
                        is_error,
                        is_error.then(|| {
                            tool_failure_reason(
                                "tool result",
                                &Err(ygg_agent::ToolError::new(text.clone())),
                            )
                            .unwrap_or_else(|| "tool failed".into())
                        }),
                        model_lab,
                    ))));
                    state.tool_panels.insert(id, index);
                }
            }
            TranscriptItem::CompactionMarker { summary } => {
                state.push_block(TranscriptBlock::Compaction(Box::new(CompactionBlock {
                    label: "Context compacted".into(),
                    summary,
                    expanded: false,
                })));
            }
            TranscriptItem::NativeCompactionMarker => {
                state.push_block(TranscriptBlock::Notice(
                    "Context compacted natively · opaque Responses state retained".into(),
                ));
            }
        }
    }
}
