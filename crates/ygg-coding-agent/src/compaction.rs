#![allow(missing_docs)]

use ygg_agent::{EntryId, EntryValue, Session};
use ygg_ai::{
    AssistantPart, Message, OutputFormat, OutputModalities, ReasoningConfig, Request, ToolChoice,
    UserMessage, UserPart,
};

use crate::app::App;
use crate::config::CompactionPolicy;

const FRAMING_OVERHEAD_TOKENS: u64 = 32;
const PER_MESSAGE_OVERHEAD_TOKENS: u64 = 8;

/// Result of a best-effort compaction attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompactionOutcome {
    NotNeeded,
    Compacted { elided: usize },
    Skipped { reason: String },
}

/// Decision returned by the capacity gate before a prompt can start a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CapacityDecision {
    Proceed(CompactionOutcome),
    Exceeded { estimate: u64, budget: u64 },
}

/// Conservative character-based estimate retained alongside App's precomputed
/// system and tool-schema reserves.
pub fn estimate_text_tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(4)
}

fn message_byte_len(message: &Message) -> usize {
    serde_json::to_vec(message)
        .map(|serialized| serialized.len())
        .unwrap_or_default()
}

/// Estimate serialized context messages including a small per-message framing
/// reserve for provider role/tool wrappers.
pub fn estimate_messages_tokens(messages: &[Message]) -> u64 {
    messages.iter().fold(0u64, |total, message| {
        total
            .saturating_add((message_byte_len(message) as u64).div_ceil(4))
            .saturating_add(PER_MESSAGE_OVERHEAD_TOKENS)
    })
}

/// Estimate the complete next request rather than calibrating from cumulative
/// provider usage, which would misrepresent a multi-turn run.
pub fn estimate_next_request_tokens(app: &App, pending_prompt: &str) -> u64 {
    let context = app
        .agent
        .session()
        .context()
        .map(|messages| estimate_messages_tokens(&messages))
        .unwrap_or_default();
    context
        .saturating_add(app.system_tokens)
        .saturating_add(app.tool_schema_tokens)
        .saturating_add(estimate_text_tokens(pending_prompt))
        .saturating_add(FRAMING_OVERHEAD_TOKENS)
}

/// Reserve output capacity from the model's advertised context window.
pub fn hard_input_budget(model: &ygg_ai::Model) -> u64 {
    model
        .spec
        .limits
        .context_window
        .saturating_sub(model.spec.limits.max_output_tokens)
}

/// Proactive compaction threshold below the hard request limit.
pub fn soft_threshold(model: &ygg_ai::Model, policy: &CompactionPolicy) -> u64 {
    let fraction = policy.threshold_fraction.clamp(0.0, 1.0);
    (fraction * hard_input_budget(model) as f64) as u64
}

fn active_branch_ids(session: &Session) -> Vec<EntryId> {
    let mut reverse = Vec::new();
    let mut cursor = session.head();
    while let Some(id) = cursor {
        let Some(entry) = session.entry(&id) else {
            break;
        };
        reverse.push(id);
        cursor = entry.parent.clone();
    }
    reverse.reverse();
    reverse
}

fn is_assistant(entry: &ygg_agent::Entry) -> bool {
    matches!(&entry.value, EntryValue::Message(Message::Assistant(_)))
}

fn parent_is_user_text(session: &Session, entry: &ygg_agent::Entry) -> bool {
    let Some(parent) = &entry.parent else {
        return false;
    };
    let Some(parent) = session.entry(parent) else {
        return false;
    };
    matches!(
        &parent.value,
        EntryValue::Message(Message::User(user))
            if !user.content.is_empty()
                && user.content.iter().all(|part| matches!(part, UserPart::Text(_)))
    )
}

/// Select the assistant entry beginning the oldest of the requested recent
/// turns. Starting at an assistant preserves summary/user/assistant context
/// alternation and keeps any following tool results with their tool call.
pub fn choose_first_kept(session: &Session, keep_recent_turns: usize) -> Option<EntryId> {
    let starts = active_branch_ids(session)
        .into_iter()
        .filter(|id| {
            session
                .entry(id)
                .is_some_and(|entry| is_assistant(entry) && parent_is_user_text(session, entry))
        })
        .collect::<Vec<_>>();
    let keep_recent_turns = keep_recent_turns.max(1);
    if starts.len() <= keep_recent_turns {
        return None;
    }
    let first_kept = starts[starts.len() - keep_recent_turns].clone();
    if session.head().as_ref() == Some(&first_kept) {
        None
    } else {
        Some(first_kept)
    }
}

fn synthetic_summary(summary: String) -> Message {
    Message::User(UserMessage {
        content: vec![UserPart::Text(format!(
            "[summary of earlier conversation]\n{summary}"
        ))],
    })
}

fn is_tool_results(message: &Message) -> bool {
    matches!(
        message,
        Message::User(user)
            if !user.content.is_empty()
                && user.content.iter().all(|part| matches!(part, UserPart::ToolResult(_)))
    )
}

fn coalesce_tool_results(messages: Vec<Message>) -> Vec<Message> {
    let mut result = Vec::with_capacity(messages.len());
    for message in messages {
        if is_tool_results(&message) {
            if let Some(Message::User(previous)) = result.last_mut() {
                if previous
                    .content
                    .iter()
                    .all(|part| matches!(part, UserPart::ToolResult(_)))
                {
                    if let Message::User(current) = message {
                        previous.content.extend(current.content);
                        continue;
                    }
                }
            }
        }
        result.push(message);
    }
    result
}

/// Reconstruct model-visible messages strictly before `first_kept` on the
/// current active branch. Older compaction records fold to their summaries.
pub fn messages_before(session: &Session, first_kept: &EntryId) -> anyhow::Result<Vec<Message>> {
    let entry = session
        .entry(first_kept)
        .ok_or_else(|| anyhow::anyhow!("unknown first kept entry {}", first_kept.0))?;
    let mut reverse = Vec::new();
    let mut cursor = entry.parent.clone();
    while let Some(id) = cursor {
        let entry = session
            .entry(&id)
            .ok_or_else(|| anyhow::anyhow!("dangling session entry {}", id.0))?;
        reverse.push(entry);
        cursor = entry.parent.clone();
    }
    reverse.reverse();

    let mut messages = Vec::new();
    for entry in reverse {
        match &entry.value {
            EntryValue::Message(message) => messages.push(message.clone()),
            EntryValue::Compaction { summary, .. } => {
                // The marker replaces everything represented before it.
                messages.clear();
                messages.push(synthetic_summary(summary.clone()));
            }
            EntryValue::Config { .. } => {}
        }
    }
    Ok(coalesce_tool_results(messages))
}

/// Call the one permitted direct AiClient completion: a stateless summary with
/// tools and reasoning disabled.
pub async fn summarize(
    client: &ygg_ai::AiClient,
    model: &ygg_ai::Model,
    messages: &[Message],
) -> anyhow::Result<String> {
    let request = Request {
        system: Some(
            "Summarize the prior coding conversation for another agent. Preserve completed work, decisions, constraints, unresolved tasks, file paths, and tool findings. Be concise and factual."
                .into(),
        ),
        messages: messages.to_vec(),
        tools: vec![],
        tool_choice: ToolChoice::None,
        max_output_tokens: Some(model.spec.limits.max_output_tokens.min(4096)),
        temperature: None,
        stop: vec![],
        reasoning: ReasoningConfig::Off,
        output_format: OutputFormat::Text,
        output_modalities: OutputModalities::Text,
        compatibility: ygg_ai::CompatibilityMode::Strict,
        cache_retention: ygg_ai::CacheRetention::default(),
        session_id: None,
    };
    let response = client.complete(model, request).await?;
    let summary = response
        .message
        .content
        .iter()
        .filter_map(|part| match part {
            AssistantPart::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    if summary.trim().is_empty() {
        anyhow::bail!("summarizer returned no text")
    }
    Ok(summary)
}

/// Attempt one nonfatal semantic-boundary compaction.
pub async fn attempt_compaction(app: &mut App) -> anyhow::Result<CompactionOutcome> {
    let first_kept =
        match choose_first_kept(app.agent.session(), app.config.compaction.keep_recent_turns) {
            Some(entry) => entry,
            None => {
                return Ok(CompactionOutcome::Skipped {
                    reason: "no safe turn boundary to compact".into(),
                })
            }
        };
    let messages = match messages_before(app.agent.session(), &first_kept) {
        Ok(messages) if !messages.is_empty() => messages,
        Ok(_) => {
            return Ok(CompactionOutcome::Skipped {
                reason: "no prior messages to summarize".into(),
            })
        }
        Err(error) => {
            return Ok(CompactionOutcome::Skipped {
                reason: error.to_string(),
            })
        }
    };
    let summary = match summarize(&app.client, &app.model, &messages).await {
        Ok(summary) => summary,
        Err(error) => {
            return Ok(CompactionOutcome::Skipped {
                reason: error.to_string(),
            })
        }
    };
    match app.agent.session_mut().compact(summary, first_kept) {
        Ok(_) => Ok(CompactionOutcome::Compacted {
            elided: messages.len(),
        }),
        Err(error) => Ok(CompactionOutcome::Skipped {
            reason: error.to_string(),
        }),
    }
}

/// Gate every new prompt against soft proactive compaction and the hard input
/// budget. Callers must never prompt after `Exceeded`.
pub async fn ensure_capacity_before_prompt(
    app: &mut App,
    pending_prompt: &str,
) -> anyhow::Result<CapacityDecision> {
    let hard = hard_input_budget(&app.model);
    let soft = soft_threshold(&app.model, &app.config.compaction);
    let estimate = estimate_next_request_tokens(app, pending_prompt);
    if estimate <= soft {
        return Ok(CapacityDecision::Proceed(CompactionOutcome::NotNeeded));
    }

    let outcome = attempt_compaction(app).await?;
    let recomputed = estimate_next_request_tokens(app, pending_prompt);
    if recomputed <= hard {
        Ok(CapacityDecision::Proceed(outcome))
    } else {
        Ok(CapacityDecision::Exceeded {
            estimate: recomputed,
            budget: hard,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ygg_ai::{
        AssistantMessage, ModelId, Protocol, ToolCall, ToolCallId, ToolResult, ToolResultPart,
    };

    use crate::app::bootstrap::{bootstrap, build_app, LaunchSelection, SessionSelection};
    use crate::config::{Config, Mode, ResumeSelector, SandboxPolicy};

    fn user(text: &str) -> EntryValue {
        EntryValue::Message(Message::User(UserMessage {
            content: vec![UserPart::Text(text.into())],
        }))
    }

    fn assistant(text: &str) -> EntryValue {
        EntryValue::Message(Message::Assistant(AssistantMessage {
            content: vec![AssistantPart::Text(text.into())],
            model: ModelId("m".into()),
            protocol: Protocol::OpenAiChat,
        }))
    }

    fn turns(count: usize) -> (tempfile::TempDir, Session) {
        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("session.jsonl")).unwrap();
        for index in 0..count {
            session.append(user(&format!("user {index}"))).unwrap();
            session
                .append(assistant(&format!("assistant {index}")))
                .unwrap();
        }
        (directory, session)
    }

    #[test]
    fn boundary_is_assistant_after_completed_user_turn_and_compacts_valid_context() {
        let (_directory, mut session) = turns(5);
        let first_kept = choose_first_kept(&session, 2).unwrap();
        let entry = session.entry(&first_kept).unwrap();
        assert!(matches!(
            entry.value,
            EntryValue::Message(Message::Assistant(_))
        ));
        let parent = session.entry(entry.parent.as_ref().unwrap()).unwrap();
        assert!(matches!(
            parent.value,
            EntryValue::Message(Message::User(_))
        ));

        session.compact("summary", first_kept).unwrap();
        let context = session.context().unwrap();
        assert!(matches!(context.first(), Some(Message::User(_))));
        assert!(matches!(context.get(1), Some(Message::Assistant(_))));
        for pair in context.windows(2).skip(1) {
            assert!(matches!(
                pair,
                [Message::Assistant(_), Message::User(_)]
                    | [Message::User(_), Message::Assistant(_)]
            ));
        }
    }

    #[test]
    fn single_turn_has_no_safe_compaction_boundary() {
        let (_directory, session) = turns(1);
        assert_eq!(choose_first_kept(&session, 4), None);
    }

    #[test]
    fn boundary_uses_only_the_active_branch() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("session.jsonl")).unwrap();
        let root = session.append(user("root")).unwrap();
        let abandoned = session.append(assistant("abandoned")).unwrap();
        session.checkout(root).unwrap();
        for index in 0..4 {
            session.append(user(&format!("active {index}"))).unwrap();
            session
                .append(assistant(&format!("answer {index}")))
                .unwrap();
        }
        let selected = choose_first_kept(&session, 2).unwrap();
        assert_ne!(selected, abandoned);
        assert!(active_branch_ids(&session).contains(&selected));
    }

    #[test]
    fn tool_result_context_is_not_orphaned_after_compaction() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("session.jsonl")).unwrap();
        for index in 0..4 {
            session.append(user(&format!("turn {index}"))).unwrap();
            session
                .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                    content: vec![AssistantPart::ToolCall(ToolCall {
                        id: ToolCallId(format!("call-{index}")),
                        name: "read".into(),
                        arguments_json: "{}".into(),
                    })],
                    model: ModelId("m".into()),
                    protocol: Protocol::OpenAiChat,
                })))
                .unwrap();
            session
                .append(EntryValue::Message(Message::User(UserMessage {
                    content: vec![UserPart::ToolResult(ToolResult {
                        tool_call_id: ToolCallId(format!("call-{index}")),
                        content: vec![ToolResultPart::Text("ok".into())],
                        is_error: false,
                    })],
                })))
                .unwrap();
            session.append(assistant("finished")).unwrap();
        }
        let first_kept = choose_first_kept(&session, 2).unwrap();
        session.compact("summary", first_kept).unwrap();
        let context = session.context().unwrap();
        for (index, message) in context.iter().enumerate() {
            if let Message::User(user) = message {
                if user
                    .content
                    .iter()
                    .any(|part| matches!(part, UserPart::ToolResult(_)))
                {
                    assert!(matches!(
                        context.get(index.saturating_sub(1)),
                        Some(Message::Assistant(_))
                    ));
                }
            }
        }
    }

    fn app_for_estimate() -> (tempfile::TempDir, App) {
        let directory = tempfile::tempdir().unwrap();
        let config = Config {
            workspace: directory.path().to_owned(),
            invocation_cwd: directory.path().to_owned(),
            model: Some(ModelId("gpt-4o-mini".into())),
            reasoning: ReasoningConfig::Off,
            sandbox: SandboxPolicy::default(),
            theme: None,
            session_dir: directory.path().join("sessions"),
            compaction: CompactionPolicy::default(),
            max_turns: 40,
            show_reasoning_in_print: false,
            initial_prompt: None,
            mode: Mode::Interactive,
            resume: ResumeSelector::New,
        };
        let boot = bootstrap(config).unwrap();
        let app = build_app(
            boot,
            LaunchSelection {
                model: ModelId("gpt-4o-mini".into()),
                session: SessionSelection::CreateNew(directory.path().join("session.jsonl")),
            },
            "system".into(),
        )
        .unwrap();
        (directory, app)
    }

    #[test]
    fn budgets_and_estimates_are_monotonic() {
        let catalog = ygg_ai::ModelCatalog::builtin().unwrap();
        let model = catalog.resolve(&ModelId("gpt-4o-mini".into())).unwrap();
        let policy = CompactionPolicy::default();
        assert!(hard_input_budget(&model) < model.spec.limits.context_window);
        assert!(soft_threshold(&model, &policy) <= hard_input_budget(&model));
        assert!(estimate_text_tokens("longer prompt") > estimate_text_tokens("x"));
        let (_directory, app) = app_for_estimate();
        assert!(
            estimate_next_request_tokens(&app, "a longer pending prompt")
                > estimate_next_request_tokens(&app, "x")
        );
    }
}
