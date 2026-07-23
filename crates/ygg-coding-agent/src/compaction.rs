#![allow(missing_docs)]

use std::io::{self, Write};

use ygg_agent::{
    build_handoff_message, finish_handoff, prepare_handoff, EntryId, EntryValue,
    HandoffPreparation, InputPart, Session, SUMMARIZATION_SYSTEM_PROMPT,
};
use ygg_ai::{
    AssistantPart, Media, Message, OutputFormat, OutputModalities, ReasoningConfig, Request,
    ToolChoice, ToolResultPart, UserPart,
};

use crate::app::App;

const FRAMING_OVERHEAD_TOKENS: u64 = 32;
const PER_MESSAGE_OVERHEAD_TOKENS: u64 = 8;
/// Flat per-item media reserves. Providers bill images by tiles/resolution and
/// audio by duration, neither of which is knowable from byte length, and
/// serialized payload bytes wildly overestimate (a 5 MB image is not millions
/// of tokens). Coarse constants keep the gate predictable; the provider still
/// enforces the true limit.
const IMAGE_TOKENS: u64 = 1_600;
const AUDIO_TOKENS: u64 = 8_000;

/// Result of a best-effort compaction attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompactionOutcome {
    Compacted { elided: usize },
    Skipped { reason: String },
}

/// Conservative character-based estimate retained alongside App's precomputed
/// system and tool-schema reserves.
pub fn estimate_text_tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(4)
}

/// Flat reserve for one media item regardless of source (inline, URL, or
/// provider reference): the provider charges for the content either way.
pub fn estimate_media_tokens(media: &Media) -> u64 {
    match media {
        Media::Image(_) => IMAGE_TOKENS,
        Media::Audio(_) => AUDIO_TOKENS,
    }
}

#[derive(Default)]
struct CountingWriter(u64);

impl Write for CountingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len() as u64);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn json_len_tokens<T: serde::Serialize>(value: &T) -> u64 {
    let mut bytes = CountingWriter::default();
    if serde_json::to_writer(&mut bytes, value).is_err() {
        return 0;
    }
    bytes.0.div_ceil(4)
}

fn user_part_tokens(part: &UserPart) -> u64 {
    match part {
        UserPart::Media(media) => estimate_media_tokens(media),
        UserPart::ToolResult(result) => result
            .content
            .iter()
            .map(|part| match part {
                ToolResultPart::Media(media) => estimate_media_tokens(media),
                other => json_len_tokens(other),
            })
            .fold(0u64, u64::saturating_add),
        other => json_len_tokens(other),
    }
}

fn assistant_part_tokens(part: &AssistantPart) -> u64 {
    match part {
        AssistantPart::Media(media) => estimate_media_tokens(media),
        other => json_len_tokens(other),
    }
}

fn message_tokens(message: &Message) -> u64 {
    match message {
        Message::User(user) => user.content.iter().map(user_part_tokens).sum(),
        Message::Assistant(assistant) => assistant.content.iter().map(assistant_part_tokens).sum(),
    }
}

/// Estimate serialized context messages including a small per-message framing
/// reserve for provider role/tool wrappers. Media parts count as flat
/// per-item reserves; their payload bytes are never serialized.
pub fn estimate_messages_tokens(messages: &[Message]) -> u64 {
    messages.iter().fold(0u64, |total, message| {
        total
            .saturating_add(message_tokens(message))
            .saturating_add(PER_MESSAGE_OVERHEAD_TOKENS)
    })
}

/// Estimate the pending prompt: text by length, media by flat reserve, using
/// the same heuristic as the context estimator so both directions agree.
pub fn estimate_pending_tokens(pending: &[InputPart]) -> u64 {
    pending.iter().fold(0u64, |total, part| {
        total.saturating_add(match part {
            InputPart::Text(text) => estimate_text_tokens(text),
            InputPart::Media(media) => estimate_media_tokens(media),
        })
    })
}

/// Estimate the complete next request rather than calibrating from cumulative
/// provider usage, which would misrepresent a multi-turn run.
pub fn estimate_next_request_tokens(app: &App, pending: &[InputPart]) -> u64 {
    let context = app
        .agent
        .session()
        .context_ref()
        .map(|messages| estimate_messages_tokens(&messages))
        .unwrap_or_default();
    let active_skill_tokens = app
        .agent
        .session()
        .head_ref()
        .and_then(|head| app.agent.session().resolve_active_skills(head).ok())
        .map(|state| {
            state
                .active_skills
                .iter()
                .map(|skill| estimate_text_tokens(&skill.instructions))
                .sum::<u64>()
        })
        .unwrap_or_default();
    context
        .saturating_add(app.system_tokens)
        .saturating_add(active_skill_tokens)
        .saturating_add(app.tool_schema_tokens)
        .saturating_add(estimate_pending_tokens(pending))
        .saturating_add(if pending.is_empty() {
            0
        } else {
            PER_MESSAGE_OVERHEAD_TOKENS
        })
        .saturating_add(FRAMING_OVERHEAD_TOKENS)
}

/// Full model context window used as the denominator in presentation. Output
/// reserve is a request-capacity concern, not already-consumed context; using
/// an input budget here can falsely display overflow while the provider is
/// still within its advertised window.
pub fn context_window(model: &ygg_ai::Model) -> u64 {
    model.spec.limits.context_window
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

/// Full-fidelity entries retained by the compaction nearest the active head.
/// Older ancestry remains durable for branching but must not be selected as a
/// fresh compaction boundary.
fn model_visible_branch_ids(session: &Session) -> Vec<EntryId> {
    let ids = active_branch_ids(session);
    let first_kept = ids.iter().rev().find_map(|id| {
        let entry = session.entry(id)?;
        match &entry.value {
            EntryValue::Compaction { first_kept, .. } => Some(first_kept),
            _ => None,
        }
    });
    let start = first_kept
        .and_then(|first_kept| ids.iter().position(|id| id == first_kept))
        .unwrap_or_default();
    ids.into_iter().skip(start).collect()
}

fn is_assistant(entry: &ygg_agent::Entry) -> bool {
    matches!(&entry.value, EntryValue::Message(Message::Assistant(_)))
}

fn previous_message_is_user(session: &Session, entry: &ygg_agent::Entry) -> bool {
    let mut cursor = entry.parent.as_ref();
    while let Some(id) = cursor {
        let Some(previous) = session.entry(id) else {
            return false;
        };
        match &previous.value {
            EntryValue::Message(Message::User(user)) => return !user.content.is_empty(),
            EntryValue::Message(Message::Assistant(_)) => return false,
            EntryValue::Compaction { .. }
            | EntryValue::Config { .. }
            | EntryValue::PromptTemplateSelected { .. }
            | EntryValue::SkillActivated { .. }
            | EntryValue::SkillResourceRead { .. }
            | EntryValue::SkillDeactivated { .. } => cursor = previous.parent.as_ref(),
        }
    }
    false
}

/// Select the assistant entry beginning the oldest of the requested recent
/// turns. Starting at an assistant preserves summary/user/assistant context
/// alternation and keeps any following tool results with their tool call.
pub fn choose_first_kept(session: &Session, keep_recent_turns: usize) -> Option<EntryId> {
    let starts = model_visible_branch_ids(session)
        .into_iter()
        .filter(|id| {
            session.entry(id).is_some_and(|entry| {
                is_assistant(entry) && previous_message_is_user(session, entry)
            })
        })
        .collect::<Vec<_>>();
    let keep_recent_turns = keep_recent_turns.max(1);
    if starts.len() <= keep_recent_turns {
        return None;
    }
    Some(starts[starts.len() - keep_recent_turns].clone())
}

/// Call a tool-free compaction subagent, persist its billable telemetry, and
/// return its text.
async fn compaction_call(
    client: &ygg_ai::AiClient,
    model: &ygg_ai::Model,
    session: &mut Session,
    cache_retention: ygg_ai::CacheRetention,
    system: &str,
    messages: Vec<Message>,
    output_tokens: u64,
) -> anyhow::Result<String> {
    let request = Request {
        system: Some(system.into()),
        messages,
        tools: vec![],
        tool_choice: ToolChoice::None,
        max_output_tokens: Some(model.spec.limits.max_output_tokens.clamp(1, output_tokens)),
        temperature: None,
        stop: vec![],
        reasoning: ReasoningConfig::Off,
        output_format: OutputFormat::Text,
        output_modalities: OutputModalities::Text,
        compatibility: ygg_ai::CompatibilityMode::Strict,
        cache_retention,
        session_id: Some(session.cache_key()),
    };
    let response = client.complete(model, request).await?;
    let cost = response.cost;
    // A failed/empty compaction response is still paid work. Persist it before
    // checking the stop reason so session totals survive every outcome.
    session.record_compaction_usage(
        model.endpoint.id.clone(),
        model.spec.id.clone(),
        response.usage,
        cost,
    )?;
    if !matches!(
        response.stop_reason,
        ygg_ai::StopReason::EndTurn | ygg_ai::StopReason::StopSequence
    ) {
        anyhow::bail!(
            "compaction subagent did not finish normally: {:?}",
            response.stop_reason
        );
    }
    let text = response
        .message
        .content
        .iter()
        .filter_map(|part| match part {
            AssistantPart::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    if text.trim().is_empty() {
        anyhow::bail!("compaction subagent returned no text")
    }
    Ok(text)
}

/// Produce one Pi-compatible structured handoff without replaying the original
/// history through additional question/answer calls.
pub async fn summarize(
    client: &ygg_ai::AiClient,
    model: &ygg_ai::Model,
    session: &mut Session,
    cache_retention: ygg_ai::CacheRetention,
    preparation: &HandoffPreparation,
) -> anyhow::Result<String> {
    compaction_call(
        client,
        model,
        session,
        cache_retention,
        SUMMARIZATION_SYSTEM_PROMPT,
        vec![build_handoff_message(preparation)],
        4096,
    )
    .await
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
    let preparation = match prepare_handoff(app.agent.session(), &first_kept) {
        Ok(preparation) if !preparation.messages.is_empty() => preparation,
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
    // Clone immutable request dependencies before borrowing the session for
    // durable compaction telemetry.
    let client = app.client.clone();
    // Bootstrap resolves an explicit override and stores it on the agent. The
    // active route remains the safe default for credentials and cache affinity.
    let model = app
        .agent
        .compaction_model()
        .cloned()
        .unwrap_or_else(|| app.model.clone());
    let cache_retention = app.config.cache_retention;
    let summary_messages = vec![build_handoff_message(&preparation)];
    let estimated_input = estimate_text_tokens(SUMMARIZATION_SYSTEM_PROMPT)
        .saturating_add(estimate_messages_tokens(&summary_messages))
        .saturating_add(FRAMING_OVERHEAD_TOKENS);
    let summary_output_tokens = model.spec.limits.max_output_tokens.clamp(1, 4096);
    let input_budget = model
        .spec
        .limits
        .context_window
        .saturating_sub(summary_output_tokens);
    if estimated_input > input_budget {
        return Ok(CompactionOutcome::Skipped {
            reason: format!(
                "compaction input exceeds summary model capacity ({estimated_input} > {input_budget} tokens)"
            ),
        });
    }
    if let Err(error) =
        app.agent
            .ensure_request_cost_capacity(&model, estimated_input, summary_output_tokens)
    {
        return Ok(CompactionOutcome::Skipped {
            reason: error.to_string(),
        });
    }
    let summary = match summarize(
        &client,
        &model,
        app.agent.session_mut(),
        cache_retention,
        &preparation,
    )
    .await
    {
        Ok(summary) => finish_handoff(summary, &preparation.details),
        Err(error) => {
            return Ok(CompactionOutcome::Skipped {
                reason: error.to_string(),
            })
        }
    };
    match app
        .agent
        .session_mut()
        .compact_with_details(summary, first_kept, preparation.details)
    {
        Ok(_) => Ok(CompactionOutcome::Compacted {
            elided: preparation.messages.len(),
        }),
        Err(error) => Ok(CompactionOutcome::Skipped {
            reason: error.to_string(),
        }),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ygg_ai::{
        AssistantMessage, Media, ModelId, Protocol, ToolCall, ToolCallId, ToolResult,
        ToolResultPart, UserMessage,
    };

    use crate::app::bootstrap::{bootstrap, build_app, LaunchSelection, SessionSelection};
    use crate::config::{CompactionPolicy, Config, Mode, ResumeSelector, SandboxPolicy};

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
    fn repeated_compaction_ignores_history_behind_the_latest_boundary() {
        let (_directory, mut session) = turns(5);
        let first_kept = choose_first_kept(&session, 2).unwrap();
        session.compact("first summary", first_kept).unwrap();

        // Exactly the requested two full-fidelity turns remain. The durable
        // pre-summary ancestry must not make another compaction appear safe.
        assert_eq!(choose_first_kept(&session, 2), None);

        session.append(user("new user 1")).unwrap();
        session.append(assistant("new assistant 1")).unwrap();
        session.append(user("new user 2")).unwrap();
        session.append(assistant("new assistant 2")).unwrap();
        let next = choose_first_kept(&session, 2).expect("new turns permit compaction");
        let preparation = prepare_handoff(&session, &next).unwrap();
        assert_eq!(
            preparation.previous_summary.as_deref(),
            Some("first summary")
        );
        assert!(preparation.messages.iter().any(|message| {
            matches!(message, Message::User(user) if user.content.iter().any(|part| matches!(part, UserPart::Text(text) if text == "user 4")))
        }));
        assert!(!preparation.messages.iter().any(|message| {
            matches!(message, Message::User(user) if user.content.iter().any(|part| matches!(part, UserPart::Text(text) if text == "user 2")))
        }));
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

    pub(crate) fn app_for_estimate() -> (tempfile::TempDir, App) {
        let directory = tempfile::tempdir().unwrap();
        let config = Config {
            workspace: directory.path().to_owned(),
            invocation_cwd: directory.path().to_owned(),
            model: Some(ModelId("gpt-4o-mini".into())),
            model_explicit: false,
            reasoning: ReasoningConfig::Off,
            reasoning_explicit: false,
            cache_retention: ygg_ai::CacheRetention::Short,
            sandbox: SandboxPolicy::default(),
            theme: None,
            theme_paths: vec![],
            color: crate::config::ColorMode::Auto,
            mouse: crate::config::MouseMode::Auto,
            plain: false,
            session_dir: directory.path().join("sessions"),
            compaction: CompactionPolicy::default(),
            max_cost_microdollars: None,
            cost_warning_microdollars: None,
            show_turn_cost: false,
            max_turns: Some(40),
            show_reasoning_in_print: false,
            initial_prompt: None,
            prompt_template: None,
            debug_prompt: false,
            prompt_paths: vec![],
            mode: Mode::Interactive,
            resume: ResumeSelector::New,
            skill_paths: vec![],
            extension_paths: vec![],
            enabled_extensions: vec![],
            trusted_extensions: vec![],
            invocation_trusted_extensions: vec![],
            tools: crate::config::ToolPolicy::default(),
            context_files: true,
            offline: true,
            workspace_trusted: true,
        };
        let boot = bootstrap(config).unwrap();
        let app = build_app(
            boot,
            LaunchSelection {
                model: ModelId("gpt-4o-mini".into()),
                session: SessionSelection::CreateNew(directory.path().join("session.jsonl")),
                reasoning: ReasoningConfig::Off,
            },
            "system".into(),
        )
        .unwrap();
        (directory, app)
    }

    fn user_media(text: &str) -> EntryValue {
        EntryValue::Message(Message::User(UserMessage {
            content: vec![
                UserPart::Text(text.into()),
                UserPart::Media(Media::image_url(
                    "https://example.com/test.png".parse().unwrap(),
                    Some(mime::IMAGE_PNG),
                )),
            ],
        }))
    }

    #[test]
    fn multimodal_user_prompts_count_as_real_turns_for_compaction() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("session.jsonl")).unwrap();
        for index in 0..5 {
            session
                .append(user_media(&format!("look at this screenshot {index}")))
                .unwrap();
            session
                .append(assistant(&format!("I see it {index}")))
                .unwrap();
        }
        let first_kept = choose_first_kept(&session, 2).unwrap();
        session.compact("summary", first_kept).unwrap();
        let context = session.context().unwrap();
        assert!(matches!(context.first(), Some(Message::User(_))));
        assert_eq!(context.len(), 4);
    }

    #[test]
    fn tool_result_user_messages_can_start_completed_episodes() {
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
            session.append(assistant(&format!("done {index}"))).unwrap();
        }
        let starts = choose_first_kept(&session, 1);
        assert!(starts.is_some());
    }

    #[test]
    fn inline_media_estimates_flat_per_item_not_by_payload_size() {
        // A 1 MB inline image must contribute a flat per-image reserve, not
        // its serialized byte length (~350k "tokens"), which wedged sessions
        // once an image entered the kept-recent window.
        let with_image = Message::User(UserMessage {
            content: vec![
                UserPart::Text("look".into()),
                UserPart::Media(Media::image_bytes(
                    bytes::Bytes::from(vec![0u8; 1024 * 1024]),
                    mime::IMAGE_PNG,
                )),
            ],
        });
        let text_only = Message::User(UserMessage {
            content: vec![UserPart::Text("look".into())],
        });
        let media_cost = estimate_messages_tokens(std::slice::from_ref(&with_image))
            - estimate_messages_tokens(std::slice::from_ref(&text_only));
        assert!(
            media_cost >= 500,
            "an image must reserve a real token cost, got {media_cost}"
        );
        assert!(
            media_cost <= 20_000,
            "an image reserve must not scale with payload bytes, got {media_cost}"
        );
    }

    #[test]
    fn budgets_and_estimates_are_monotonic() {
        let catalog = ygg_ai::ModelCatalog::builtin().unwrap();
        let model = catalog.resolve(&ModelId("gpt-4o-mini".into())).unwrap();
        assert_eq!(context_window(&model), model.spec.limits.context_window);
        assert!(estimate_text_tokens("longer prompt") > estimate_text_tokens("x"));
        let (_directory, app) = app_for_estimate();
        assert!(
            estimate_next_request_tokens(
                &app,
                &[InputPart::Text("a longer pending prompt".into())]
            ) > estimate_next_request_tokens(&app, &[InputPart::Text("x".into())])
        );
    }

    #[test]
    fn compaction_recalculates_the_model_visible_request_instead_of_preserving_a_total() {
        let (_directory, mut app) = app_for_estimate();
        for index in 0..4 {
            app.agent
                .session_mut()
                .append(user(&format!("old prompt {index} {}", "x".repeat(4_000))))
                .unwrap();
            app.agent
                .session_mut()
                .append(assistant(&format!(
                    "old answer {index} {}",
                    "y".repeat(4_000)
                )))
                .unwrap();
        }
        let before = estimate_next_request_tokens(&app, &[]);
        let first_kept = choose_first_kept(app.agent.session(), 1).unwrap();
        app.agent
            .session_mut()
            .compact("short grounded summary", first_kept)
            .unwrap();
        let after = estimate_next_request_tokens(&app, &[]);
        assert!(after < before / 2, "before={before}, after={after}");
    }

    #[test]
    fn pending_media_is_reserved_in_the_next_request_estimate() {
        let (_directory, app) = app_for_estimate();
        let text_only = [InputPart::Text("describe this".into())];
        let with_image = [
            InputPart::Text("describe this".into()),
            InputPart::Media(Media::image_bytes(
                bytes::Bytes::from(vec![0u8; 4096]),
                mime::IMAGE_PNG,
            )),
        ];
        let reserve = estimate_next_request_tokens(&app, &with_image)
            - estimate_next_request_tokens(&app, &text_only);
        assert!(
            reserve >= 500,
            "pending media must reserve tokens in the gate, got {reserve}"
        );
    }
}
