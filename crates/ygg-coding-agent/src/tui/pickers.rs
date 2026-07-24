#![allow(missing_docs)]

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ygg_agent::extension_process::ConfirmationRequest;
use ygg_agent::tool::{ToolConfirmation, ToolInputRequest};
use ygg_ai::{ModelCatalog, ModelId};

use crate::config::ThinkingLevel;
use crate::presentation::{format_token_rate_value, ModelDisplayMetadata};
use crate::session_store::{SessionMeta, SessionStore};
use crate::tui::view::{InteractiveShell, Panel, PanelAction, PanelResult};

const MAX_SECRET_INPUT_BYTES: usize = 4096;

#[derive(Default)]
struct SecretInputBuffer(Vec<u8>);

impl SecretInputBuffer {
    fn push(&mut self, character: char) {
        let mut encoded = [0; 4];
        let bytes = character.encode_utf8(&mut encoded).as_bytes();
        if self.0.len().saturating_add(bytes.len()) <= MAX_SECRET_INPUT_BYTES {
            self.0.extend_from_slice(bytes);
        }
        encoded.fill(0);
    }

    fn extend_paste(&mut self, pasted: &str) {
        let pasted = pasted.trim_end_matches(['\r', '\n']);
        let remaining = MAX_SECRET_INPUT_BYTES.saturating_sub(self.0.len());
        let mut end = pasted.len().min(remaining);
        while end > 0 && !pasted.is_char_boundary(end) {
            end -= 1;
        }
        self.0.extend_from_slice(&pasted.as_bytes()[..end]);
    }

    fn backspace(&mut self) {
        let Some((start, _)) = std::str::from_utf8(&self.0)
            .ok()
            .and_then(|text| text.char_indices().last())
        else {
            return;
        };
        self.0[start..].fill(0);
        self.0.truncate(start);
    }

    fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.0)
    }
}

impl Drop for SecretInputBuffer {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

/// Give one running tool exclusive ownership of terminal input. The answer is
/// sent directly to its reply channel and never enters the ordinary editor.
pub async fn tool_input_picker<S>(
    shell: &mut InteractiveShell,
    input: &mut S,
    request: &ToolInputRequest,
) -> anyhow::Result<bool>
where
    S: futures_util::Stream<Item = std::io::Result<Event>> + Unpin,
{
    shell.set_tool_input_prompt(Some(request.prompt.clone()));
    shell.render();
    let mut secret = SecretInputBuffer::default();
    loop {
        let next = tokio::select! {
            biased;
            _ = crate::tui::terminal::wait_for_shutdown_signal() => None,
            next = input.next() => next,
        };
        let event = match next {
            Some(Ok(event)) => event,
            Some(Err(error)) => {
                request.cancel();
                shell.set_tool_input_prompt(None);
                shell.render();
                return Err(error.into());
            }
            None => {
                request.cancel();
                shell.set_tool_input_prompt(None);
                shell.render();
                return Ok(false);
            }
        };
        match event {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                match key.code {
                    KeyCode::Enter => {
                        request.respond(secret.take());
                        shell.set_tool_input_prompt(None);
                        shell.render();
                        return Ok(true);
                    }
                    KeyCode::Esc => {
                        request.cancel();
                        shell.set_tool_input_prompt(None);
                        shell.render();
                        return Ok(false);
                    }
                    KeyCode::Backspace => secret.backspace(),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        request.cancel();
                        shell.set_tool_input_prompt(None);
                        shell.render();
                        return Ok(false);
                    }
                    KeyCode::Char(character)
                        if !key.modifiers.intersects(
                            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                        ) =>
                    {
                        secret.push(character)
                    }
                    _ => {}
                }
            }
            Event::Paste(pasted) => secret.extend_paste(&pasted),
            Event::Resize(columns, rows) => shell.set_size(columns, rows),
            _ => {}
        }
        // Re-rendering is safe: only the fixed prompt and cursor are visible;
        // secret bytes never influence frame contents.
        shell.render();
    }
}

/// Drive a panel-based selection list. Owns the event loop while the panel is open.
async fn pick_list<S>(
    shell: &mut InteractiveShell,
    input: &mut S,
    title: &str,
    items: Vec<String>,
    descriptions: Vec<Option<String>>,
    action: PanelAction,
) -> anyhow::Result<Option<usize>>
where
    S: futures_util::Stream<Item = std::io::Result<Event>> + Unpin,
{
    if items.is_empty() {
        shell.error("nothing is available to select".into());
        shell.render();
        return Ok(None);
    }

    shell.open_panel(Panel::SelectList {
        title: title.into(),
        items,
        descriptions,
        selected: 0,
        filter: String::new(),
        action,
    });
    shell.render();

    loop {
        let next = tokio::select! {
            biased;
            _ = crate::tui::terminal::wait_for_shutdown_signal() => {
                shell.close_panel();
                return Ok(None);
            }
            next = input.next() => next,
        };
        let event = match next {
            Some(Ok(event)) => event,
            Some(Err(error)) => {
                shell.close_panel();
                return Err(error.into());
            }
            None => {
                shell.close_panel();
                return Ok(None);
            }
        };
        // Mouse events pass through to the shell for transcript scrolling.
        if matches!(event, Event::Mouse(_)) {
            continue;
        }
        if let Some((result, _action)) = shell.panel_input(&event) {
            shell.render();
            return Ok(match result {
                PanelResult::Confirm(index) => Some(index),
                PanelResult::Cancel => None,
            });
        }
        // Panel consumed the event; render updated state.
        shell.render();
    }
}

/// Convert persistent session metadata to select-list items.
#[allow(dead_code)]
pub fn session_items(store: &SessionStore) -> Vec<String> {
    store
        .list()
        .into_iter()
        .map(|session| session.title)
        .collect()
}

/// Ask the user to select a stored session from a precomputed snapshot.
/// Callers discover and summarize sessions off the raw-terminal input task.
pub async fn session_picker(
    shell: &mut InteractiveShell,
    input: &mut EventStream,
    sessions: &[SessionMeta],
    session_dir: &std::path::Path,
) -> anyhow::Result<Option<std::path::PathBuf>> {
    if sessions.is_empty() {
        shell.error(format!("no sessions in {}", session_dir.display()));
        shell.render();
        return Ok(None);
    }
    let items: Vec<String> = sessions.iter().map(|s| s.title.clone()).collect();
    let descs: Vec<Option<String>> = sessions
        .iter()
        .map(|s| Some(format!("{}", s.path.display())))
        .collect();
    let Some(index) = pick_list(
        shell,
        input,
        "Select session",
        items,
        descs,
        PanelAction::SelectSession(vec![]), // dummy — blocking path ignores this
    )
    .await?
    else {
        return Ok(None);
    };
    Ok(Some(sessions[index].path.clone()))
}

/// Ask the user to select an installed theme name.
pub async fn theme_picker(
    shell: &mut InteractiveShell,
    input: &mut EventStream,
    names: &[String],
) -> anyhow::Result<Option<String>> {
    let items: Vec<String> = names.to_vec();
    let action_names = names.to_vec();
    let Some(index) = pick_list(
        shell,
        input,
        "Select theme",
        items,
        vec![None; names.len()],
        PanelAction::SelectTheme(action_names),
    )
    .await?
    else {
        return Ok(None);
    };
    Ok(Some(names[index].clone()))
}

/// Ask the user to select standard or Pro execution before choosing effort.
pub async fn reasoning_mode_picker(
    shell: &mut InteractiveShell,
    input: &mut EventStream,
    current: ygg_ai::ReasoningMode,
) -> anyhow::Result<Option<ygg_ai::ReasoningMode>> {
    let other = match current {
        ygg_ai::ReasoningMode::Standard => ygg_ai::ReasoningMode::Pro,
        ygg_ai::ReasoningMode::Pro => ygg_ai::ReasoningMode::Standard,
    };
    let modes = vec![current, other];
    let items = modes
        .iter()
        .map(|mode| match mode {
            ygg_ai::ReasoningMode::Standard => "standard".to_owned(),
            ygg_ai::ReasoningMode::Pro => "pro".to_owned(),
        })
        .collect();
    let Some(index) = pick_list(
        shell,
        input,
        "Select reasoning mode",
        items,
        modes
            .iter()
            .map(|mode| match mode {
                ygg_ai::ReasoningMode::Standard => Some("normal model execution".to_owned()),
                ygg_ai::ReasoningMode::Pro => {
                    Some("more model work; higher latency and token usage".to_owned())
                }
            })
            .collect(),
        PanelAction::SelectReasoningMode(modes.clone()),
    )
    .await?
    else {
        return Ok(None);
    };
    Ok(Some(modes[index]))
}

/// Ask the user to select a capability-supported thinking level.
pub async fn thinking_picker(
    shell: &mut InteractiveShell,
    input: &mut EventStream,
    levels: &[ThinkingLevel],
) -> anyhow::Result<Option<ThinkingLevel>> {
    let items: Vec<String> = levels.iter().map(|l| l.label().into()).collect();
    let action_levels = levels.to_vec();
    let Some(index) = pick_list(
        shell,
        input,
        "Select thinking level",
        items,
        vec![None; levels.len()],
        PanelAction::SelectThinking(action_levels),
    )
    .await?
    else {
        return Ok(None);
    };
    let selected = levels[index];
    if let Err(e) = crate::cli::persist_reasoning(selected.label()) {
        shell.error(format!("failed to save thinking preference: {e}"));
    }
    Ok(Some(selected))
}

/// Ask the user to approve a typed extension/tool request. Escape and input
/// closure are denials; approval is never inferred from a missing frontend.
pub async fn confirmation_picker<S>(
    shell: &mut InteractiveShell,
    input: &mut S,
    request: &ToolConfirmation,
) -> anyhow::Result<bool>
where
    S: futures_util::Stream<Item = std::io::Result<Event>> + Unpin,
{
    confirmation_prompt_picker(
        shell,
        input,
        &request.prompt,
        request.detail.as_deref(),
        request.destructive,
        request.default,
    )
    .await
}

pub async fn extension_confirmation_picker<S>(
    shell: &mut InteractiveShell,
    input: &mut S,
    extension: &str,
    request: &ConfirmationRequest,
) -> anyhow::Result<bool>
where
    S: futures_util::Stream<Item = std::io::Result<Event>> + Unpin,
{
    let prompt = format!("{extension}: {}", request.prompt);
    confirmation_prompt_picker(
        shell,
        input,
        &prompt,
        request.detail.as_deref(),
        request.destructive,
        request.default,
    )
    .await
}

async fn confirmation_prompt_picker<S>(
    shell: &mut InteractiveShell,
    input: &mut S,
    prompt: &str,
    detail: Option<&str>,
    destructive: bool,
    default: bool,
) -> anyhow::Result<bool>
where
    S: futures_util::Stream<Item = std::io::Result<Event>> + Unpin,
{
    let (items, decisions) = if default {
        (vec!["Allow".to_owned(), "Deny".to_owned()], [true, false])
    } else {
        (vec!["Deny".to_owned(), "Allow".to_owned()], [false, true])
    };
    let consequence = detail.map(str::to_owned).or_else(|| {
        destructive.then(|| "The extension marked this action as destructive.".to_owned())
    });
    let descriptions = vec![consequence.clone(), consequence];
    let title = if destructive {
        format!("Confirm destructive action · {prompt}")
    } else {
        format!("Confirm extension action · {prompt}")
    };
    let selected = pick_list(
        shell,
        input,
        &title,
        items,
        descriptions,
        PanelAction::ExtensionConfirmation,
    )
    .await?;
    Ok(selected.map(|index| decisions[index]).unwrap_or(false))
}

/// Build a concise human-facing label from the same cached metadata boundary
/// used by the footer. Canonical and wire-level IDs remain available in
/// `/status`; picker descriptions carry provider disambiguation.
fn model_label(model: &ygg_ai::ModelSpec) -> String {
    ModelDisplayMetadata::resolve(model).name
}

fn model_description(model: &ygg_ai::ModelSpec) -> String {
    let context = match model.limits.context_window {
        value if value >= 1_000_000 => format!("{}M", value / 1_000_000),
        value if value >= 1_000 => format!("{}k", value / 1_000),
        value => value.to_string(),
    };
    let pricing = model
        .pricing
        .as_ref()
        .map(|pricing| {
            format!(
                "{}/{} per M · cache-read {} per M",
                format_token_rate_value(pricing.input),
                format_token_rate_value(pricing.output),
                format_token_rate_value(pricing.cache_read),
            )
        })
        .unwrap_or_else(|| "pricing unavailable ($?)".to_owned());
    let mut details = vec![format!("{pricing} · {context} context")];
    if model.capabilities.tools {
        details.push("tools".into());
    }
    if model
        .capabilities
        .input_modalities
        .contains(ygg_ai::Modality::Image)
    {
        details.push("vision".into());
    }
    format!("{} · {}", model.endpoint.0, details.join(" · "))
}

/// Ask the user to select one model, preserving cancellation for workflows
/// such as `/logout` that must not mutate credentials until a replacement model
/// has been chosen.
pub async fn optional_model_picker(
    shell: &mut InteractiveShell,
    input: &mut EventStream,
    catalog: &ModelCatalog,
) -> anyhow::Result<Option<ModelId>> {
    let mut models = catalog.models().collect::<Vec<_>>();
    models.sort_by(|left, right| left.id.0.cmp(&right.id.0));
    let model_ids: Vec<ModelId> = models.iter().map(|m| m.id.clone()).collect();
    let items: Vec<String> = models.iter().map(|m| model_label(m)).collect();
    let descs: Vec<Option<String>> = models
        .iter()
        .map(|model| Some(model_description(model)))
        .collect();

    let Some(index) = pick_list(
        shell,
        input,
        "Select model",
        items,
        descs,
        PanelAction::SelectModel(model_ids),
    )
    .await?
    else {
        return Ok(None);
    };
    let selected_id = models[index].id.0.clone();
    if let Err(e) = crate::cli::persist_model(&selected_id) {
        shell.error(format!("failed to save model preference: {e}"));
    }
    Ok(Some(ModelId(selected_id)))
}

/// Ask the user to select one model from the active catalog.
pub async fn model_picker(
    shell: &mut InteractiveShell,
    input: &mut EventStream,
    catalog: &ModelCatalog,
) -> anyhow::Result<ModelId> {
    optional_model_picker(shell, input, catalog)
        .await?
        .ok_or_else(|| anyhow::anyhow!("model selection cancelled"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_label_uses_friendly_metadata_without_wire_id_noise() {
        let spec = ygg_ai::ModelSpec {
            id: ModelId("my-custom".into()),
            endpoint: ygg_ai::EndpointId("local".into()),
            api_name: "llama-3.1-8b-instruct".into(),
            display_name: Some("Llama 3.1 8B".into()),
            protocol: ygg_ai::Protocol::OpenAiChat,
            capabilities: ygg_ai::Capabilities {
                input_modalities: ygg_ai::ModalitySet::none(),
                output_modalities: ygg_ai::ModalitySet::none(),
                tools: true,
                parallel_tool_calls: false,
                reasoning: None,
                structured_output: false,
            },
            limits: ygg_ai::ModelLimits {
                context_window: 131072,
                max_output_tokens: 8192,
            },
            pricing: None,
            cache: ygg_ai::CacheCompatibility::default(),
        };
        assert_eq!(model_label(&spec), "Llama 3.1 8B");
        assert!(model_description(&spec).contains("$?"));

        let mut priced = spec.clone();
        priced.pricing = Some(ygg_ai::Pricing {
            input: ygg_ai::TokenRate(1_000_000),
            output: ygg_ai::TokenRate(6_000_000),
            cache_read: ygg_ai::TokenRate(100_000),
            cache_write_5m: ygg_ai::TokenRate(1_250_000),
            cache_write_1h: None,
            reasoning: None,
            tiers: Vec::new(),
        });
        let description = model_description(&priced);
        assert!(description.contains("$1.00/$6.00"), "{description}");
        assert!(description.contains("cache-read $0.1"), "{description}");
    }

    #[test]
    fn custom_model_label_removes_provider_repository_and_quantization_noise() {
        let spec = ygg_ai::ModelSpec {
            id: ModelId("custom/Intel/Qwen3.6-27B-int4-AutoRound".into()),
            endpoint: ygg_ai::EndpointId("custom-openai".into()),
            api_name: "Intel/Qwen3.6-27B-int4-AutoRound".into(),
            display_name: None,
            protocol: ygg_ai::Protocol::OpenAiChat,
            capabilities: ygg_ai::Capabilities {
                input_modalities: ygg_ai::ModalitySet::none(),
                output_modalities: ygg_ai::ModalitySet::none(),
                tools: true,
                parallel_tool_calls: true,
                reasoning: None,
                structured_output: true,
            },
            limits: ygg_ai::ModelLimits {
                context_window: 128000,
                max_output_tokens: 16384,
            },
            pricing: None,
            cache: ygg_ai::CacheCompatibility::default(),
        };
        assert_eq!(model_label(&spec), "Qwen3.6 27B");
    }

    #[test]
    fn session_items_map_ids_and_titles() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path(), workspace.path());
        std::fs::create_dir_all(store.dir()).unwrap();
        let mut session = ygg_agent::Session::create(store.dir().join("one.jsonl")).unwrap();
        session
            .append(ygg_agent::EntryValue::Message(ygg_ai::Message::User(
                ygg_ai::UserMessage {
                    content: vec![ygg_ai::UserPart::Text("mapped title".into())],
                },
            )))
            .unwrap();
        let items = session_items(&store);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0], "mapped title");
    }
}
