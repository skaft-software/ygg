#![allow(missing_docs)]

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::{Stream, StreamExt};
use tokio::time::{Interval, MissedTickBehavior};
#[cfg(unix)]
use ygg_agent::extension_process::ProcessGroupGuard;
use ygg_agent::{
    analyze_session_cache_stats, AgentError, AgentEvent, EntryId, Run, RunControl, Session,
};
use ygg_ai::{ModelId, ReasoningConfig, ToolCallId};

use crate::app::bootstrap::{
    build_app, estimate_text_tokens, rebuild_app, resolve_launch_interactive, Bootstrap,
};
use crate::app::{
    apply_reconfig, level_from_reasoning, reasoning_label, supported_levels, thinking_to_reasoning,
    App, Reconfig,
};
use crate::commands::{self, Command};
use crate::compaction::{
    attempt_compaction, context_window, estimate_next_request_tokens, CompactionOutcome,
};
use crate::config::ThinkingLevel;
use crate::modes::RunEnded;
use crate::prompts::{render_and_record, RenderedPrompt};
use crate::resources::{compose_instructions, validate_skill_requirements};
use crate::session_tree::render_session_tree;
use crate::tui::composer::ComposedInput;
use crate::tui::keymap::{self, InputAction};
use crate::tui::pickers::{
    confirmation_picker, extension_confirmation_picker, optional_model_picker, session_picker,
    theme_picker, thinking_picker, tool_input_picker,
};
use crate::tui::theme::{
    available_themes, background_from_terminal_rgb, load_named_theme_for_background, load_theme,
    load_theme_for_background, TerminalBackground,
};
use crate::tui::view::InteractiveShell;

/// Ordered controls sent to the frozen Agent during an active run.
#[derive(Debug)]
enum ControlIntent {
    Steer(ygg_agent::UserInput),
}

type ControlFuture = Pin<Box<dyn Future<Output = Result<(), AgentError>>>>;

struct InteractiveExtensionConfirmations<'a> {
    shell: &'a mut InteractiveShell,
    input: &'a mut EventStream,
}

impl crate::extensions::ExtensionConfirmationHandler for InteractiveExtensionConfirmations<'_> {
    fn wait_for_cancel<'a>(&'a mut self) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + 'a>> {
        Box::pin(async move {
            loop {
                let event = tokio::select! {
                    biased;
                    _ = crate::tui::terminal::wait_for_shutdown_signal() => return Ok(()),
                    event = self.input.next() => event,
                };
                match event {
                    Some(Ok(Event::Key(key)))
                        if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                            && key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        return Ok(());
                    }
                    Some(Ok(Event::Resize(columns, rows))) => {
                        self.shell.set_size(columns, rows);
                        self.shell.render();
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => return Err(error.into()),
                    None => return Ok(()),
                }
            }
        })
    }

    fn confirm<'a>(
        &'a mut self,
        extension: &'a str,
        request: &'a ygg_agent::extension_process::ConfirmationRequest,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<bool>> + 'a>> {
        Box::pin(async move {
            tokio::select! {
                biased;
                _ = crate::tui::terminal::wait_for_shutdown_signal() => {
                    anyhow::bail!("shutdown requested while awaiting extension confirmation")
                }
                result = extension_confirmation_picker(
                    self.shell,
                    self.input,
                    extension,
                    request,
                ) => result,
            }
        })
    }
}

/// Reconfiguration work requested while the Agent is active. It is applied
/// only after `Run` is dropped at the next idle boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PendingIdleAction {
    Login(Option<String>),
    Logout(Option<String>),
    ChangeModel(ModelId),
    CycleModel,
    ChangeThinking(ReasoningConfig),
    ChangeThinkingLevel(ThinkingLevel),
    CycleThinking,
    PickModel,
    PickThinking,
    NewSession,
    ResumeSession(Option<String>),
    Compact,
    AutoCompact(Option<commands::AutoCompactSetting>),
    ShowContext,
    ReloadResources,
    ShowTree,
    CheckoutEntry(String),
    Skills(commands::SkillsSubcommand),
}

/// Push an idle action while preserving ordering barriers. Adjacent model or
/// thinking changes collapse to the latest request; sessions and compaction do
/// not collapse or disappear.
pub fn push_pending_action(queue: &mut VecDeque<PendingIdleAction>, action: PendingIdleAction) {
    let same_kind = matches!(
        (&queue.back(), &action),
        (
            Some(PendingIdleAction::ChangeModel(_)),
            PendingIdleAction::ChangeModel(_)
        ) | (
            Some(PendingIdleAction::ChangeThinking(_)),
            PendingIdleAction::ChangeThinking(_)
        ) | (
            Some(PendingIdleAction::ChangeThinking(_)),
            PendingIdleAction::ChangeThinkingLevel(_)
        ) | (
            Some(PendingIdleAction::ChangeThinkingLevel(_)),
            PendingIdleAction::ChangeThinking(_)
        ) | (
            Some(PendingIdleAction::ChangeThinkingLevel(_)),
            PendingIdleAction::ChangeThinkingLevel(_)
        ) | (
            Some(
                PendingIdleAction::ChangeThinking(_)
                    | PendingIdleAction::ChangeThinkingLevel(_)
                    | PendingIdleAction::CycleThinking
            ),
            PendingIdleAction::CycleThinking
        )
    );
    if same_kind {
        let _ = queue.pop_back();
    }
    queue.push_back(action);
}

#[derive(Debug)]
enum Idle {
    Submit(ComposedInput),
    Command(String),
    CycleThinking,
    Quit,
}

async fn wait_for_prompt<S>(
    shell: &mut InteractiveShell,
    input: &mut S,
    scroll_tick: &mut Interval,
    extension_tick: &mut Interval,
    executable_extensions: &mut crate::extensions::ExecutableExtensions,
) -> anyhow::Result<Idle>
where
    S: Stream<Item = std::io::Result<Event>> + Unpin,
{
    let mut scroll_dirty = false;
    loop {
        tokio::select! {
            biased;
            _ = crate::tui::terminal::wait_for_shutdown_signal() => {
                return Ok(Idle::Quit);
            }
            maybe = input.next() => {
                let event = match maybe {
                    Some(Ok(event)) => event,
                    Some(Err(error)) => return Err(error.into()),
                    None => return Ok(Idle::Quit),
                };
                // Panels are driven by picker functions that own the event
                // stream. If a panel leaks here (shouldn't happen), Esc closes it.
                if shell.has_panel() {
                    match &event {
                        Event::Mouse(_) => continue,
                        Event::Resize(columns, rows) => {
                            shell.set_size(*columns, *rows);
                            shell.render();
                            continue;
                        }
                        Event::Key(key)
                            if key.kind == KeyEventKind::Press && key.code == KeyCode::Esc =>
                        {
                            shell.close_panel();
                            shell.render();
                            continue;
                        }
                        _ => continue,
                    }
                }
                if shell.has_overlay() {
                    match event {
                        Event::Mouse(_) => continue,
                        Event::Resize(columns, rows) => {
                            shell.set_size(columns, rows);
                            shell.render();
                            continue;
                        }
                        _ => {
                            shell.close_overlay();
                            shell.clear_error();
                            shell.render();
                            continue;
                        }
                    }
                }
                let pending = if shell.pending_is_empty() {
                    String::new()
                } else {
                    shell.pending()
                };
                match keymap::translate_with_popup(
                    Some(event),
                    false,
                    &pending,
                    shell.slash_popup_open(),
                ) {
                    InputAction::SlashMenu(action) => {
                        shell.slash_menu(action);
                        shell.render();
                    }
                    InputAction::CompleteSlashCommand => {
                        shell.complete_slash_command();
                        shell.render();
                    }
                    InputAction::CompleteMention => {
                        shell.complete_mention();
                        shell.render();
                    }
                    InputAction::Edit(action) => {
                        shell.apply_edit(action);
                        shell.render();
                    }
                    InputAction::Resize(columns, rows) => {
                        shell.set_size(columns, rows);
                        shell.render();
                    }
                    InputAction::Scroll(direction) => {
                        shell.scroll(direction);
                        shell.render();
                    }
                    InputAction::ScrollLines(direction) => {
                        shell.scroll_lines(direction);
                        scroll_dirty = true;
                    }
                    InputAction::JumpToTail => {
                        shell.jump_to_tail();
                        shell.render();
                    }
                    InputAction::SelectAllTranscript => {
                        shell.select_all_transcript();
                        shell.render();
                    }
                    InputAction::CopyTranscriptSelection => {
                        if shell.copy_selected_plain_text().is_some() {
                            shell.notice("copied to clipboard");
                        }
                        shell.render();
                    }
                    InputAction::TranscriptPointer(gesture) => {
                        match gesture {
                            crate::tui::keymap::PointerGesture::Begin { row, col, extend } => {
                                shell.begin_transcript_selection(row, col, extend);
                            }
                            crate::tui::keymap::PointerGesture::Extend { row, col } => {
                                shell.extend_transcript_selection(row, col);
                            }
                            crate::tui::keymap::PointerGesture::End { row, col } => {
                                shell.end_transcript_selection(row, col);
                            }
                        }
                        shell.render();
                    }
                    InputAction::ShowCompactionSummary => {
                        shell.show_compaction_summary();
                        shell.render();
                    }
                    InputAction::ExpandFocusedTool => {
                        shell.expand_focused_tool();
                        shell.render();
                    }
                    InputAction::CycleThinking => return Ok(Idle::CycleThinking),
                    InputAction::Close => {
                        shell.clear_error();
                        shell.render();
                    }
                    InputAction::Submit(_) => return Ok(Idle::Submit(shell.drain_composed())),
                    InputAction::Command(_) => return Ok(Idle::Command(shell.drain_editor())),
                    InputAction::Closed => return Ok(Idle::Quit),
                    InputAction::Ignore
                    | InputAction::Abort
                    | InputAction::Steer(_) => {}
                }
            }
            // Mouse/trackpad events arrive in bursts. Apply every delta to
            // state, but draw at most once per frame so a large transcript
            // cannot leave a backlog that appears as post-scroll inertia.
            _ = scroll_tick.tick(), if scroll_dirty => {
                shell.render();
                scroll_dirty = false;
            },
            _ = extension_tick.tick() => {
                if apply_extension_background(shell, executable_extensions) {
                    shell.render();
                }
            }
        }
    }
}

fn queue_command(command: Command, queue: &mut VecDeque<PendingIdleAction>) -> anyhow::Result<()> {
    let action = match command {
        Command::Login(provider) => PendingIdleAction::Login(provider),
        Command::Logout(provider) => PendingIdleAction::Logout(provider),
        Command::Model(Some(id)) => PendingIdleAction::ChangeModel(ModelId(id)),
        Command::Model(None) => PendingIdleAction::PickModel,
        Command::CycleModel => PendingIdleAction::CycleModel,
        Command::Thinking(Some(level)) => match ThinkingLevel::parse(&level)? {
            ThinkingLevel::Off => PendingIdleAction::ChangeThinking(ReasoningConfig::Off),
            level => PendingIdleAction::ChangeThinkingLevel(level),
        },
        Command::Thinking(None) => PendingIdleAction::PickThinking,
        Command::New => PendingIdleAction::NewSession,
        Command::Resume(id) => PendingIdleAction::ResumeSession(id),
        Command::Compact => PendingIdleAction::Compact,
        Command::AutoCompact(setting) => PendingIdleAction::AutoCompact(setting),
        Command::Context => PendingIdleAction::ShowContext,
        Command::Reload => PendingIdleAction::ReloadResources,
        Command::Tree => PendingIdleAction::ShowTree,
        Command::Checkout(id) => PendingIdleAction::CheckoutEntry(id),
        Command::Skills(sub) => PendingIdleAction::Skills(sub),
        other => anyhow::bail!("{other:?} cannot be queued as an idle action"),
    };
    push_pending_action(queue, action);
    Ok(())
}

async fn await_with_ctrl_c<F, S>(future: F, input: &mut S) -> Option<F::Output>
where
    F: std::future::Future,
    S: Stream<Item = std::io::Result<Event>> + Unpin,
{
    let mut future = Box::pin(future);
    let mut input_open = true;
    loop {
        tokio::select! {
            biased;
            event = input.next(), if input_open => match event {
                Some(Ok(Event::Key(key)))
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                        && key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    return None;
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => input_open = false,
            },
            output = &mut future => return Some(output),
        }
    }
}

const LIFECYCLE_SHUTDOWN_GRACE: Duration = Duration::from_millis(1400);
const RAW_CTRL_C_SIGNAL: i32 = 2;

fn is_ctrl_c(key: &crossterm::event::KeyEvent) -> bool {
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        && key.code == KeyCode::Char('c')
        && key.modifiers.contains(KeyModifiers::CONTROL)
}

/// Keep raw-terminal input, resize handling, rendering, and termination
/// signals live while a bounded lifecycle operation runs elsewhere. Ordinary
/// typing is intentionally ignored at this boundary; Ctrl-C becomes the same
/// coordinated SIGINT shutdown used by the signal thread.
async fn await_lifecycle<F, T>(
    shell: &mut InteractiveShell,
    input: &mut EventStream,
    label: &str,
    operation: F,
) -> anyhow::Result<T>
where
    F: Future<Output = anyhow::Result<T>>,
{
    let mut operation = Box::pin(operation);
    let mut render_tick = tokio::time::interval(Duration::from_millis(50));
    render_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut input_open = true;
    shell.set_run_label(label);
    shell.render();

    loop {
        tokio::select! {
            biased;
            signal = crate::tui::terminal::wait_for_shutdown_signal() => {
                shell.set_run_label("shutting down…");
                shell.render();
                let _ = tokio::time::timeout(LIFECYCLE_SHUTDOWN_GRACE, &mut operation).await;
                anyhow::bail!("shutdown signal {signal} received during {label}");
            }
            result = &mut operation => {
                shell.set_run_label("idle");
                return result;
            }
            event = input.next(), if input_open => match event {
                Some(Ok(Event::Key(key))) if is_ctrl_c(&key) => {
                    crate::tui::terminal::request_coordinated_shutdown(RAW_CTRL_C_SIGNAL)?;
                    shell.set_run_label("shutting down…");
                    shell.render();
                    let _ = tokio::time::timeout(LIFECYCLE_SHUTDOWN_GRACE, &mut operation).await;
                    anyhow::bail!("Ctrl-C cancelled {label}");
                }
                Some(Ok(Event::Resize(columns, rows))) => {
                    shell.set_size(columns, rows);
                    shell.render();
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    // A blocking lifecycle worker cannot be aborted safely: it
                    // may own the only App and dropping its JoinHandle merely
                    // detaches it. Treat terminal input failure like loss of the
                    // controlling TTY, announce coordinated shutdown, and give
                    // the owned operation the same bounded settlement window as
                    // an explicit signal before returning the original error.
                    let shutdown =
                        crate::tui::terminal::request_coordinated_shutdown(RAW_CTRL_C_SIGNAL);
                    shell.set_run_label("shutting down…");
                    shell.render();
                    let _ = tokio::time::timeout(LIFECYCLE_SHUTDOWN_GRACE, &mut operation).await;
                    return match shutdown {
                        Ok(()) => Err(error.into()),
                        Err(shutdown_error) => Err(anyhow::anyhow!(
                            "terminal input failed: {error}; coordinated shutdown also failed: {shutdown_error}"
                        )),
                    };
                }
                None => input_open = false,
            },
            _ = render_tick.tick() => shell.render(),
        }
    }
}

pub(crate) async fn run_blocking_lifecycle<T, W>(
    shell: &mut InteractiveShell,
    input: &mut EventStream,
    label: &str,
    work: W,
) -> anyhow::Result<T>
where
    T: Send + 'static,
    W: FnOnce() -> anyhow::Result<T> + Send + 'static,
{
    let task = tokio::task::spawn_blocking(work);
    await_lifecycle(shell, input, label, async move {
        task.await
            .map_err(|error| anyhow::anyhow!("{label} worker failed: {error}"))?
    })
    .await
}

fn validate_provider(provider: Option<&str>) -> anyhow::Result<&str> {
    match provider.unwrap_or("codex") {
        "codex" | "openai-codex" | "openai" => Ok("codex"),
        "custom" | "openai-custom" => Ok("custom"),
        other => anyhow::bail!("unknown provider {other:?}; supported: codex, custom"),
    }
}

/// Run device-code login outside raw/alternate-screen mode, then make the new
/// models available immediately without restarting the current Agent.
async fn login_codex(app: &mut App, shell: &mut InteractiveShell) -> anyhow::Result<()> {
    shell.set_run_label("signing in to ChatGPT…");
    shell.render();
    shell.suspend();
    let store = crate::auth::codex::CredentialStore::new(crate::auth::codex::default_path());
    let login_result = crate::auth::codex::login(&store, false).await;
    // Restoring the terminal is mandatory even when OAuth fails.
    shell.resume()?;
    shell.set_run_label("idle");

    if let Err(error) = login_result {
        shell.error(format!("ChatGPT login failed: {error:#}"));
        shell.render();
        return Ok(());
    }

    let catalog = match crate::app::bootstrap::model_catalog() {
        Ok(catalog) => catalog,
        Err(error) => {
            shell.error(format!(
                "ChatGPT login succeeded, but reloading models failed: {error:#}"
            ));
            shell.render();
            return Ok(());
        }
    };
    if !catalog
        .models()
        .any(|model| model.endpoint.0 == crate::auth::codex::ENDPOINT_ID)
    {
        shell.error("ChatGPT login completed, but no Codex models could be registered".into());
        shell.render();
        return Ok(());
    }
    app.catalog = catalog;
    shell.clear_error();
    shell.notice("signed in to ChatGPT; use /model to select a Codex model");
    shell.render();
    Ok(())
}

/// Remove the Ygg-owned credential and catalog entries together. If the active
/// model is a Codex model, choose its replacement before deleting anything so
/// cancellation leaves both the session and credentials untouched.
async fn logout_codex(
    mut app: App,
    shell: &mut InteractiveShell,
    input: &mut EventStream,
) -> anyhow::Result<App> {
    let catalog = crate::app::bootstrap::model_catalog_without_codex()?;
    let replacement = if app.model.endpoint.id.0 == crate::auth::codex::ENDPOINT_ID {
        shell.notice("select a replacement model before signing out");
        let Some(model) = optional_model_picker(shell, input, &catalog).await? else {
            shell.notice("logout cancelled");
            return Ok(app);
        };
        Some(model)
    } else {
        None
    };

    // Transition while authentication and the old catalog are still intact.
    // If rebuilding the Agent fails, the user remains signed in rather than
    // being stranded on a model whose credential was already deleted.
    if let Some(model) = replacement {
        app = transition(app, shell, input, Reconfig::Model(model)).await?;
    }

    let store = crate::auth::codex::CredentialStore::new(crate::auth::codex::default_path());
    if let Err(error) = store.delete() {
        shell.error(format!("ChatGPT logout failed: {error:#}"));
        return Ok(app);
    }
    app.catalog = catalog;
    shell.clear_error();
    shell.notice("signed out of ChatGPT");
    shell.render();
    Ok(app)
}

/// Save a default custom endpoint credential and reload the catalog.
fn login_custom(shell: &mut InteractiveShell) -> anyhow::Result<()> {
    use crate::auth::custom::{self, CustomCredential};
    let store = custom::CredentialStore::new(custom::default_path());
    let path = custom::default_path();

    if store.load()?.is_some() {
        shell.notice(format!(
            "custom endpoint already configured at {}; use /logout custom first to replace it",
            path.display()
        ));
        return Ok(());
    }

    // Save a default credential that the user can edit.
    let cred = CustomCredential {
        base_url: "http://localhost:1234/v1/".into(),
        api_key: String::new(),
        api_name: "local-model".into(),
        headers: Vec::new(),
        models: Vec::new(),
        auto_discover: true,
    };
    store.save(&cred)?;
    shell.notice(format!(
        "custom endpoint template saved to {}\n\
         edit it with your endpoint details, then /reload to register the model",
        path.display()
    ));
    Ok(())
}

/// Remove custom endpoint credentials and rebuild the catalog.
async fn logout_custom(
    mut app: App,
    shell: &mut InteractiveShell,
    input: &mut EventStream,
) -> anyhow::Result<App> {
    use crate::auth::custom;

    let store = custom::CredentialStore::new(custom::default_path());
    if store.load()?.is_none() {
        shell.notice("no custom endpoint configured");
        return Ok(app);
    }

    // Pick a replacement model if the active model is the custom one.
    let needs_replacement = app.model.endpoint.id.0 == custom::ENDPOINT_ID;
    if needs_replacement {
        let catalog = crate::app::bootstrap::model_catalog()?;
        // Temporarily remove custom from consideration.
        shell.notice("select a replacement model before signing out");
        let Some(model) = optional_model_picker(shell, input, &catalog).await? else {
            shell.notice("logout cancelled");
            return Ok(app);
        };
        app = transition(app, shell, input, Reconfig::Model(model)).await?;
    }

    store.delete()?;
    // Rebuild catalog without the custom endpoint.
    let catalog = crate::app::bootstrap::model_catalog()?;
    // model_catalog() will no longer find the credential, so the custom model
    // won't be registered. But if we just deleted, it might still show. Force a
    // fresh rebuild by calling base_model_catalog + codex registration directly
    // is complex; for now just reload.
    app.catalog = catalog;
    shell.clear_error();
    shell.notice("custom endpoint removed");
    shell.render();
    Ok(app)
}

async fn apply_theme(
    shell: &mut InteractiveShell,
    input: &mut EventStream,
    name: &str,
) -> anyhow::Result<()> {
    let config = shell
        .theme_config()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("theme configuration is unavailable"))?;
    let theme_name = name.to_owned();
    let background = shell.theme().background();
    let theme = run_blocking_lifecycle(shell, input, "loading theme…", move || {
        load_named_theme_for_background(&theme_name, &config, background)
    })
    .await?;
    shell.set_theme(theme);
    shell.notice(format!("theme changed to {name}"));
    Ok(())
}

async fn theme_choices(
    shell: &mut InteractiveShell,
    input: &mut EventStream,
) -> anyhow::Result<Vec<String>> {
    let config = shell
        .theme_config()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("theme configuration is unavailable"))?;
    run_blocking_lifecycle(shell, input, "discovering themes…", move || {
        Ok(available_themes(&config))
    })
    .await
}

async fn show_theme_choices(
    shell: &mut InteractiveShell,
    input: &mut EventStream,
) -> anyhow::Result<()> {
    let names = theme_choices(shell, input).await?;
    if names.is_empty() {
        shell.notice("no themes found under .ygg/themes or ~/.ygg/themes");
    } else {
        shell.show_overlay_text(format!(
            "Available themes:\n{}\n\nUse /theme <name> to switch.",
            names.join("\n")
        ));
    }
    Ok(())
}

async fn reload_active_theme(
    shell: &mut InteractiveShell,
    input: &mut EventStream,
) -> anyhow::Result<()> {
    let active_theme = shell.theme();
    let theme = run_blocking_lifecycle(shell, input, "reloading theme…", move || {
        active_theme.reload()
    })
    .await?;
    shell.set_theme(theme);
    shell.notice("theme reloaded");
    Ok(())
}

fn handle_active_command(
    shell: &mut InteractiveShell,
    command: Command,
    queue: &mut VecDeque<PendingIdleAction>,
    quit_requested: &mut bool,
) {
    match command {
        Command::Status => {
            let mut status = shell.status_detail();
            if !queue.is_empty() {
                status.push_str(&format!("\nQueued idle actions: {}", queue.len()));
            }
            shell.show_status_text_with_telemetry(status);
        }
        Command::Cost | Command::Cache => {
            shell.notice("cost and cache reports are available at the next idle boundary")
        }
        Command::Update => shell.notice("update checks are available at the next idle boundary"),
        Command::Theme(_) => shell.notice("theme commands are available at the next idle boundary"),
        Command::Tool(_id) => {
            shell.notice("tool details follow transcript verbosity; use Ctrl+O or /verbose")
        }
        Command::Verbose(value) => {
            let enabled = value.unwrap_or(!shell.verbose_tools());
            shell.set_verbose_tools(enabled);
            shell.notice(format!(
                "verbose transcript {}",
                if enabled { "enabled" } else { "disabled" }
            ));
        }
        Command::Extensions(_) => {
            shell.notice("extension inspection and reload are available at the next idle boundary")
        }
        Command::Name(_) | Command::Sessions | Command::Export(_) => {
            shell.notice("session management commands are available at the next idle boundary")
        }
        Command::Quit => *quit_requested = true,
        Command::Unknown(text) => shell.error(format!("unknown command: {text}")),
        command => match queue_command(command, queue) {
            Ok(()) => shell.notice("command queued for the next idle boundary"),
            Err(error) => shell.error(error.to_string()),
        },
    }
    shell.render();
}

/// Drive one active frozen-Agent run. Control sends are queued locally, and
/// input polling pauses while a bounded send waits so a full control channel
/// can never starve the run stream that drains it.
#[allow(clippy::too_many_arguments)]
pub async fn drive_active_run<S>(
    run: &mut Run<'_>,
    control: &RunControl,
    shell: &mut InteractiveShell,
    input: &mut S,
    scroll_tick: &mut Interval,
    pending_actions: &mut VecDeque<PendingIdleAction>,
    quit_requested: &mut bool,
    max_cost_microdollars: Option<u64>,
    cost_warning_microdollars: Option<u64>,
    executable_extensions: &mut crate::extensions::ExecutableExtensions,
) -> anyhow::Result<RunEnded>
where
    S: Stream<Item = std::io::Result<Event>> + Unpin,
{
    let run_id = shell
        .current_run_id()
        .ok_or_else(|| anyhow::anyhow!("cannot drive a run without presentation state"))?;
    let mut intents = VecDeque::<ControlIntent>::new();
    let mut in_flight: Option<ControlFuture> = None;
    let mut aborting = false;
    let mut input_open = true;
    let mut scroll_dirty = false;
    let mut last_run_cost = 0u64;
    let mut extension_tick = tokio::time::interval(Duration::from_millis(50));
    extension_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut extension_tool_calls =
        std::collections::HashMap::<ToolCallId, (String, serde_json::Value)>::new();

    loop {
        if !aborting && in_flight.is_none() {
            if let Some(intent) = intents.pop_front() {
                let control = control.clone();
                in_flight = Some(Box::pin(async move {
                    match intent {
                        ControlIntent::Steer(text) => control.steer(text).await,
                    }
                }));
            }
        }

        tokio::select! {
            biased;
            _ = crate::tui::terminal::wait_for_shutdown_signal() => {
                control.abort();
                *quit_requested = true;
                shell.restore_queued_steering();
                shell.set_run_preparing(run_id, "shutting down");
                shell.render();
                ygg_agent::extension_process::terminate_exec_process_groups(
                    Duration::from_millis(400),
                )
                .await;
                return Ok(RunEnded::Aborted);
            }
            result = futures_util::future::OptionFuture::from(in_flight.as_mut().map(|f| f.as_mut())), if in_flight.is_some() => {
                // A run may have ended before a pending control was delivered.
                // That error is harmless; no detached send survives this loop.
                let _ = result;
                in_flight = None;
            }
            _ = scroll_tick.tick(), if scroll_dirty => {
                shell.render();
                scroll_dirty = false;
            }
            _ = extension_tick.tick() => {
                if apply_extension_background(shell, executable_extensions) {
                    shell.render();
                }
            }
            maybe = input.next(), if input_open => {
                let event = match maybe {
                    Some(Ok(event)) => event,
                    Some(Err(error)) => {
                        control.abort();
                        shell.fail_run(run_id, format!("terminal input failed: {error}"));
                        return Err(error.into());
                    }
                    None => {
                        // A fused/closed stream is immediately ready forever.
                        // Disable this select branch after the first EOF so it
                        // cannot starve the Agent's terminal RunFinished event.
                        input_open = false;
                        control.abort();
                        aborting = true;
                        intents.clear();
                        in_flight = None;
                        shell.set_run_preparing(run_id, "cancelling");
                        shell.render();
                        *quit_requested = true;
                        continue;
                    }
                };
                // Panels are driven by picker functions that own the event
                // stream. If a panel leaks here (shouldn't happen), Esc closes it.
                if shell.has_panel() {
                    match &event {
                        Event::Mouse(_) => continue,
                        Event::Resize(columns, rows) => {
                            shell.set_size(*columns, *rows);
                            shell.render();
                            continue;
                        }
                        Event::Key(key)
                            if key.kind == KeyEventKind::Press && key.code == KeyCode::Esc =>
                        {
                            shell.close_panel();
                            shell.render();
                            continue;
                        }
                        _ => continue,
                    }
                }
                if shell.has_overlay() {
                    match event {
                        Event::Mouse(_) => continue,
                        Event::Resize(columns, rows) => {
                            shell.set_size(columns, rows);
                            shell.render();
                            continue;
                        }
                        _ => {
                            shell.close_overlay();
                            shell.clear_error();
                            shell.render();
                            continue;
                        }
                    }
                }
                let pending = if shell.pending_is_empty() {
                    String::new()
                } else {
                    shell.pending()
                };
                match keymap::translate_with_popup(
                    Some(event),
                    true,
                    &pending,
                    shell.slash_popup_open(),
                ) {
                    InputAction::SlashMenu(action) => {
                        shell.slash_menu(action);
                        shell.render();
                    }
                    InputAction::CompleteMention => {
                        shell.complete_mention();
                        shell.render();
                    }
                    InputAction::Abort => {
                        control.abort();
                        // A steer send can be waiting for acknowledgement or
                        // still be only a local intent. Stop dispatching both,
                        // then let SteeringDelivered/RunFinished settle which
                        // entries became durable before restoring the rest.
                        aborting = true;
                        intents.clear();
                        in_flight = None;
                        shell.set_run_preparing(run_id, "cancelling");
                        shell.render();
                    }
                    InputAction::Steer(_) => {
                        if !aborting {
                            let composed = shell.drain_composed();
                            if !composed.is_empty() {
                                shell.queue_steering(&composed);
                                intents.push_back(ControlIntent::Steer(composed.into_user_input()));
                            }
                        }
                        shell.render();
                    }

                    InputAction::Command(_) => {
                        let command = commands::parse(&shell.drain_editor());
                        let was_quit = matches!(command, Command::Quit);
                        handle_active_command(
                            shell,
                            command,
                            pending_actions,
                            quit_requested,
                        );
                        if was_quit {
                            control.abort();
                            aborting = true;
                            intents.clear();
                            in_flight = None;
                            shell.set_run_preparing(run_id, "cancelling");
                            shell.render();
                        }
                    }
                    InputAction::CompleteSlashCommand => {
                        shell.complete_slash_command();
                        shell.render();
                    }
                    InputAction::Edit(action) => {
                        shell.apply_edit(action);
                        shell.render();
                    }
                    InputAction::Resize(columns, rows) => {
                        shell.set_size(columns, rows);
                        shell.render();
                    }
                    InputAction::Scroll(direction) => {
                        shell.scroll(direction);
                        shell.render();
                    }
                    InputAction::ScrollLines(direction) => {
                        shell.scroll_lines(direction);
                        scroll_dirty = true;
                    }
                    InputAction::JumpToTail => {
                        shell.jump_to_tail();
                        shell.render();
                    }
                    InputAction::SelectAllTranscript => {
                        shell.select_all_transcript();
                        shell.render();
                    }
                    InputAction::CopyTranscriptSelection => {
                        if shell.copy_selected_plain_text().is_some() {
                            shell.notice("copied to clipboard");
                        }
                        shell.render();
                    }
                    InputAction::TranscriptPointer(gesture) => {
                        match gesture {
                            crate::tui::keymap::PointerGesture::Begin { row, col, extend } => {
                                shell.begin_transcript_selection(row, col, extend);
                            }
                            crate::tui::keymap::PointerGesture::Extend { row, col } => {
                                shell.extend_transcript_selection(row, col);
                            }
                            crate::tui::keymap::PointerGesture::End { row, col } => {
                                shell.end_transcript_selection(row, col);
                            }
                        }
                        shell.render();
                    }
                    InputAction::ShowCompactionSummary => {
                        shell.show_compaction_summary();
                        shell.render();
                    }
                    InputAction::ExpandFocusedTool => {
                        shell.expand_focused_tool();
                        shell.render();
                    }
                    InputAction::CycleThinking => {
                        push_pending_action(pending_actions, PendingIdleAction::CycleThinking);
                        shell.notice("thinking change queued for the next idle boundary");
                        shell.render();
                    }
                    InputAction::Close => {
                        shell.clear_error();
                        shell.render();
                    }
                    InputAction::Closed => {
                        input_open = false;
                        control.abort();
                        aborting = true;
                        intents.clear();
                        in_flight = None;
                        shell.set_run_preparing(run_id, "cancelling");
                        shell.render();
                        *quit_requested = true;
                    }
                    InputAction::Ignore | InputAction::Submit(_) => {}
                }
            }
            event = run.next() => match event {
                Some(event) => {
                    if let AgentEvent::ToolStarted { id, name, args } = &event {
                        extension_tool_calls.insert(id.clone(), (name.clone(), args.clone()));
                    }
                    if let AgentEvent::ToolProgress {
                        progress: ygg_agent::ToolProgress::Confirmation(request),
                        ..
                    } = &event
                    {
                        let confirmation = tokio::select! {
                            biased;
                            _ = crate::tui::terminal::wait_for_shutdown_signal() => {
                                request.respond(false);
                                control.abort();
                                *quit_requested = true;
                                shell.restore_queued_steering();
                                shell.set_run_preparing(run_id, "shutting down");
                                shell.render();
                                ygg_agent::extension_process::terminate_exec_process_groups(
                                    Duration::from_millis(400),
                                )
                                .await;
                                return Ok(RunEnded::Aborted);
                            }
                            result = confirmation_picker(shell, input, request) => result,
                        };
                        let confirmed = match confirmation {
                            Ok(confirmed) => confirmed,
                            Err(error) => {
                                request.respond(false);
                                return Err(error);
                            }
                        };
                        request.respond(confirmed);
                        shell.notice(if confirmed {
                            "extension action confirmed"
                        } else {
                            "extension action denied"
                        });
                    }
                    if let AgentEvent::ToolProgress {
                        progress: ygg_agent::ToolProgress::Input(request),
                        ..
                    } = &event
                    {
                        let answered = tool_input_picker(shell, input, request).await?;
                        if !answered {
                            shell.notice("interactive command input cancelled");
                        }
                    }
                    if let AgentEvent::ProviderRetry {
                        attempt,
                        max_attempts,
                        error,
                        ..
                    } = &event
                    {
                        shell.notice(format!(
                            "{error} Retrying ({attempt}/{max_attempts})…"
                        ));
                    }
                    shell.on_run_event(run_id, &event);
                    if let AgentEvent::ToolFinished { id, result } = &event {
                        if let Some((name, arguments)) = extension_tool_calls.remove(id) {
                            let (output, is_error) = match result {
                                Ok(output) => (Some(output.text.clone()), false),
                                Err(error) => (Some(error.message.clone()), true),
                            };
                            executable_extensions.request_tool_render(
                                id.clone(),
                                &name,
                                arguments,
                                output,
                                is_error,
                            );
                            for message in executable_extensions.drain_events() {
                                shell.notice(message);
                            }
                        }
                    }
                    if let AgentEvent::TurnFinished {
                        session_cost_microdollars,
                        run_cost_microdollars,
                        ..
                    } = &event
                    {
                        let turn_cost = run_cost_microdollars.saturating_sub(last_run_cost);
                        if cost_warning_microdollars.is_some_and(|threshold| turn_cost >= threshold)
                        {
                            shell.notice(format!(
                                "turn cost warning: {} reached the {} threshold",
                                crate::commands::format_microdollars(turn_cost),
                                crate::commands::format_microdollars_cents(
                                    cost_warning_microdollars.unwrap_or_default()
                                )
                            ));
                        }
                        last_run_cost = *run_cost_microdollars;
                        if let (Some(limit), Some(total)) =
                            (max_cost_microdollars, *session_cost_microdollars)
                        {
                            if total >= limit {
                                shell.error(format!(
                                    "Session cost limit of {} reached.",
                                    crate::commands::format_microdollars_cents(limit)
                                ));
                                control.abort();
                                aborting = true;
                                intents.clear();
                                in_flight = None;
                            }
                        }
                    }
                    let run_finished = matches!(&event, AgentEvent::RunFinished { .. });
                    if run_finished {
                        // The renderer is asynchronous and coalesces requests.
                        // Restore any steer that lost the final delivery race
                        // before requesting the terminal frame, so idle chrome,
                        // the terminal outcome, and the editor are one atomic
                        // presentation state.
                        shell.restore_queued_steering();
                    }
                    shell.render();
                    if let AgentEvent::RunFinished { reason, .. } = event {
                        let ended = RunEnded::from(reason);
                        return Ok(ended);
                    }
                }
                None => {
                    shell.restore_queued_steering();
                    shell.fail_run(run_id, "run stream ended without a final outcome");
                    shell.render();
                    return Ok(RunEnded::Failed(
                        "run stream ended without RunFinished".into(),
                    ));
                }
            },
        }
    }
}

fn cost_limit_message(app: &App) -> Option<String> {
    let limit = app.config.max_cost_microdollars?;
    (app.agent.session().total_cost_microdollars() >= limit).then(|| {
        format!(
            "Session cost limit of {} reached.",
            crate::commands::format_microdollars_cents(limit)
        )
    })
}

fn prepare_prompt(shell: &mut InteractiveShell) {
    // Errors describe the previous interaction. Once a new prompt is accepted
    // they are stale and must not remain pinned below the active run.
    shell.clear_error();
}

fn status_context_estimate(app: &App) -> u64 {
    // Context is a property of the next request, not cumulative session spend.
    // This borrows Session's cached model-visible messages, so compaction and
    // checkout are reflected immediately without cloning the transcript.
    estimate_next_request_tokens(app, &[])
}

fn update_status(shell: &mut InteractiveShell, app: &App) {
    let context_estimate = status_context_estimate(app);
    let cache_stats = analyze_session_cache_stats(app.agent.session());
    shell.set_identity(
        &app.model.endpoint.id.0,
        &app.model.spec.id.0,
        &crate::app::reasoning_label(&app.reasoning),
    );
    // Registry/configured metadata overrides the conservative canonical-ID
    // fallback installed by `set_identity` and is cached until model switch.
    shell.set_model_theme(&app.model);
    shell.set_status_detail(commands::status_text_with_metrics(
        app,
        None,
        context_estimate,
        &cache_stats,
    ));
    shell.set_input_modalities(app.model.spec.capabilities.input_modalities);
    shell.set_workspace(app.config.workspace.clone());
    shell.set_prompt_templates(app.prompts.descriptors());
    shell.set_extension_commands(Arc::from(app.executable_extensions.command_suggestions()));
    shell.set_context_estimate(context_estimate, context_window(&app.model));
    shell.set_session_telemetry(
        app.agent.session(),
        cache_stats.latest_raw_hit_rate_basis_points(),
    );
}

fn request_extension_ui(shell: &mut InteractiveShell, app: &mut App) {
    app.executable_extensions.refresh_host_state(
        app.agent.session(),
        &app.model,
        &app.reasoning,
        &app.sessions,
    );
    app.executable_extensions.request_status_refresh();
    for message in app.executable_extensions.drain_events() {
        shell.notice(message);
    }
}

fn apply_extension_background(
    shell: &mut InteractiveShell,
    executable_extensions: &mut crate::extensions::ExecutableExtensions,
) -> bool {
    let updates = executable_extensions.drain_background_updates();
    let mut changed = false;
    if let Some(ui) = updates.ui {
        shell.set_extension_header(ui.header);
        shell.set_extension_status(ui.status);
        shell.set_extension_footer(ui.footer);
        changed = true;
    }
    for update in updates.rendered_tools {
        shell.apply_extension_tool_renderer(&update.id, &update.segments);
        changed = true;
    }
    for message in executable_extensions.drain_events() {
        shell.notice(message);
        changed = true;
    }
    changed
}

fn report_compaction(shell: &mut InteractiveShell, outcome: &CompactionOutcome, session: &Session) {
    match outcome {
        CompactionOutcome::Compacted { elided } => {
            let usage = session
                .usage_records()
                .iter()
                .rev()
                .find(|record| matches!(record.kind, ygg_agent::UsageRecordKind::Compaction));
            let detail = usage.map_or_else(
                || format!("{elided} earlier messages summarized"),
                |record| {
                    let cost = record
                        .cost_microdollars
                        .map(commands::format_microdollars)
                        .unwrap_or_else(|| "cost unavailable".to_owned());
                    let prompt_tokens = record
                        .usage
                        .input_tokens
                        .saturating_add(record.usage.cache_read_tokens)
                        .saturating_add(record.usage.cache_write_tokens);
                    format!("{prompt_tokens} input tokens summarized · {cost} compaction cost")
                },
            );
            let summary = session
                .head()
                .and_then(|head| session.entry(&head))
                .and_then(|entry| match &entry.value {
                    ygg_agent::EntryValue::Compaction { summary, .. } => Some(summary.clone()),
                    _ => None,
                });
            if let Some(summary) = summary {
                shell.compaction_marker(format!("Context compacted · {detail}"), summary);
            } else {
                shell.error("compaction completed without a durable summary marker".to_owned());
            }
        }
        CompactionOutcome::Skipped { reason } => {
            shell.notice(format!("compaction skipped: {reason}"))
        }
    }
}

fn configure_auto_compaction(
    app: &mut App,
    shell: &mut InteractiveShell,
    setting: Option<commands::AutoCompactSetting>,
) -> anyhow::Result<()> {
    match setting {
        Some(commands::AutoCompactSetting::Enabled(enabled)) => {
            app.config.compaction.enabled = enabled;
        }
        Some(commands::AutoCompactSetting::ThresholdPercent(percent)) => {
            app.config.compaction.threshold_fraction = f64::from(percent) / 100.0;
        }
        None => {}
    }
    app.agent.set_compaction_policy(
        app.config.compaction.enabled,
        app.config.compaction.threshold_fraction,
        app.config.compaction.keep_recent_turns,
    )?;
    shell.notice(format!(
        "auto-compaction {} at {:.0}% · keep {} recent turns · this process",
        if app.config.compaction.enabled {
            "on"
        } else {
            "off"
        },
        app.config.compaction.threshold_fraction * 100.0,
        app.config.compaction.keep_recent_turns,
    ));
    Ok(())
}

async fn reload_resources(
    app: App,
    shell: &mut InteractiveShell,
    input: &mut EventStream,
) -> anyhow::Result<App> {
    let background = shell.theme().background();
    let (app, theme) = run_blocking_lifecycle(shell, input, "reloading resources…", move || {
        let mut app = app;
        app.system = compose_instructions(&app.config)?;
        app.system_tokens = estimate_text_tokens(&app.system);
        let app = rebuild_app(app, None, None, None)?;
        let theme = load_theme_for_background(&app.config, background);
        Ok((app, theme))
    })
    .await?;
    shell.set_theme(theme);
    shell.set_theme_config(app.config.clone());
    shell.hydrate(app.agent.session())?;
    update_status(shell, &app);
    Ok(app)
}

fn next_model_id(app: &App) -> anyhow::Result<ModelId> {
    next_model_id_in_catalog(&app.catalog, &app.model.spec.id)
}

fn next_thinking_level(app: &App) -> anyhow::Result<ThinkingLevel> {
    let levels = supported_levels(&app.model);
    let current = level_from_reasoning(&app.reasoning, &app.model)?;
    let index = levels
        .iter()
        .position(|level| *level == current)
        .unwrap_or(0);
    levels
        .get((index + 1) % levels.len())
        .copied()
        .ok_or_else(|| anyhow::anyhow!("no thinking levels are available"))
}

fn next_model_id_in_catalog(
    catalog: &ygg_ai::ModelCatalog,
    current_model: &ModelId,
) -> anyhow::Result<ModelId> {
    let mut models = catalog
        .models()
        .map(|model| model.id.clone())
        .collect::<Vec<_>>();
    models.sort_by(|left, right| left.0.cmp(&right.0));
    let current = models
        .iter()
        .position(|model| model == current_model)
        .ok_or_else(|| anyhow::anyhow!("active model is not present in the catalog"))?;
    models
        .get((current + 1) % models.len())
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no models are available"))
}

fn session_tree_text(session: &Session) -> String {
    render_session_tree(session)
}

fn restore_session_head(path: &std::path::Path, head: EntryId) -> anyhow::Result<()> {
    let mut session = Session::open(path)?;
    session.checkout(head)?;
    Ok(())
}

async fn checkout_entry(
    mut app: App,
    shell: &mut InteractiveShell,
    input: &mut EventStream,
    id: String,
) -> anyhow::Result<App> {
    let display_id = id.clone();
    let (app, path, previous_head) =
        run_blocking_lifecycle(shell, input, "checking out session…", move || {
        let path = app.agent.session().path().to_owned();
        let previous_head = app
            .agent
            .session()
            .head()
            .ok_or_else(|| anyhow::anyhow!("cannot checkout from an empty session"))?;
        app.agent.session_mut().checkout(EntryId(id.clone()))?;
        match rebuild_app(
            app,
            None,
            None,
            Some(crate::app::bootstrap::SessionSelection::OpenExisting(
                path.clone(),
            )),
        ) {
            Ok(app) => Ok((app, path, previous_head)),
            Err(error) => {
                if let Err(rollback) = restore_session_head(&path, previous_head) {
                    anyhow::bail!(
                        "checkout failed: {error}; restoring the previous head also failed: {rollback}"
                    );
                }
                Err(error)
            }
        }
        })
        .await?;
    if let Err(error) = shell.hydrate(app.agent.session()) {
        if let Err(rollback) = restore_session_head(&path, previous_head) {
            anyhow::bail!(
                "checkout hydration failed: {error}; restoring the previous head also failed: {rollback}"
            );
        }
        return Err(error);
    }
    update_status(shell, &app);
    shell.notice(format!(
        "checked out entry {display_id}; future messages will create a branch"
    ));
    Ok(app)
}

async fn transition(
    app: App,
    shell: &mut InteractiveShell,
    input: &mut EventStream,
    reconfig: Reconfig,
) -> anyhow::Result<App> {
    let app = run_blocking_lifecycle(shell, input, "reconfiguring…", move || {
        apply_reconfig(app, reconfig)
    })
    .await?;
    shell.hydrate(app.agent.session())?;
    update_status(shell, &app);
    Ok(app)
}

async fn pick_session_path(
    shell: &mut InteractiveShell,
    input: &mut EventStream,
    store: &crate::session_store::SessionStore,
) -> anyhow::Result<Option<std::path::PathBuf>> {
    let store = store.clone();
    let session_dir = store.dir().to_owned();
    let sessions = run_blocking_lifecycle(shell, input, "discovering sessions…", move || {
        Ok(store.list())
    })
    .await?;
    session_picker(shell, input, &sessions, &session_dir).await
}

async fn apply_pending_actions(
    mut app: App,
    shell: &mut InteractiveShell,
    input: &mut EventStream,
    pending_actions: &mut VecDeque<PendingIdleAction>,
) -> anyhow::Result<App> {
    while let Some(action) = pending_actions.pop_front() {
        match action {
            PendingIdleAction::Login(provider) => match validate_provider(provider.as_deref()) {
                Ok("codex") => login_codex(&mut app, shell).await?,
                Ok("custom") => login_custom(shell)?,
                Ok(_) => unreachable!(),
                Err(e) => shell.error(e.to_string()),
            },
            PendingIdleAction::Logout(provider) => match validate_provider(provider.as_deref()) {
                Ok("codex") => {
                    app = logout_codex(app, shell, input).await?;
                }
                Ok("custom") => {
                    app = logout_custom(app, shell, input).await?;
                }
                Ok(_) => unreachable!(),
                Err(e) => shell.error(e.to_string()),
            },
            PendingIdleAction::ChangeModel(id) => {
                app = transition(app, shell, input, Reconfig::Model(id)).await?;
                shell.notice("queued model change applied");
            }
            PendingIdleAction::CycleModel => {
                let id = next_model_id(&app)?;
                app = transition(app, shell, input, Reconfig::Model(id)).await?;
                shell.notice("queued model cycle applied");
            }
            PendingIdleAction::ChangeThinking(reasoning) => {
                if let Err(e) = crate::cli::persist_reasoning(&reasoning_label(&reasoning)) {
                    shell.error(format!("failed to save thinking preference: {e}"));
                }
                app = transition(app, shell, input, Reconfig::Thinking(reasoning)).await?;
                shell.notice("queued thinking change applied");
            }
            PendingIdleAction::ChangeThinkingLevel(level) => {
                if let Err(e) = crate::cli::persist_reasoning(level.label()) {
                    shell.error(format!("failed to save thinking preference: {e}"));
                }
                let reasoning = thinking_to_reasoning(level, &app.model)?;
                app = transition(app, shell, input, Reconfig::Thinking(reasoning)).await?;
                shell.notice("queued thinking change applied");
            }
            PendingIdleAction::CycleThinking => {
                let level = next_thinking_level(&app)?;
                if let Err(e) = crate::cli::persist_reasoning(level.label()) {
                    shell.error(format!("failed to save thinking preference: {e}"));
                }
                let reasoning = thinking_to_reasoning(level, &app.model)?;
                app = transition(app, shell, input, Reconfig::Thinking(reasoning)).await?;
                shell.notice(format!("thinking changed to {}", level.label()));
            }
            PendingIdleAction::NewSession => {
                app = transition(app, shell, input, Reconfig::NewSession).await?;
                shell.notice("queued new session created");
            }
            PendingIdleAction::ResumeSession(Some(id)) => {
                let path = app.sessions.path_by_id(&id)?;
                app = transition(app, shell, input, Reconfig::Resume(path)).await?;
                shell.notice("queued session resumed");
            }
            PendingIdleAction::ResumeSession(None) => {
                if let Some(path) = pick_session_path(shell, input, &app.sessions).await? {
                    app = transition(app, shell, input, Reconfig::Resume(path)).await?;
                    shell.notice("queued session resumed");
                }
            }
            PendingIdleAction::Compact => {
                shell.set_run_label("compacting…");
                shell.render();
                let outcome = attempt_compaction(&mut app).await?;
                report_compaction(shell, &outcome, app.agent.session());
                update_status(shell, &app);
                shell.set_run_label("idle");
            }
            PendingIdleAction::AutoCompact(setting) => {
                configure_auto_compaction(&mut app, shell, setting)?;
                update_status(shell, &app);
            }
            PendingIdleAction::ShowContext => {
                shell.show_context_report(crate::tui::context::ContextReport::capture(&app, &[]));
            }
            PendingIdleAction::ReloadResources => {
                app = reload_resources(app, shell, input).await?;
                shell.notice("instructions, themes, prompts, skills, and extensions reloaded");
            }
            PendingIdleAction::ShowTree => {
                shell.show_overlay_text(session_tree_text(app.agent.session()));
            }
            PendingIdleAction::CheckoutEntry(id) => {
                app = checkout_entry(app, shell, input, id).await?;
            }
            PendingIdleAction::PickModel => {
                if let Some(model) = optional_model_picker(shell, input, &app.catalog).await? {
                    app = transition(app, shell, input, Reconfig::Model(model)).await?;
                    shell.notice("queued model change applied");
                }
            }
            PendingIdleAction::PickThinking => {
                if let Some(level) =
                    thinking_picker(shell, input, &supported_levels(&app.model)).await?
                {
                    let reasoning = thinking_to_reasoning(level, &app.model)?;
                    app = transition(app, shell, input, Reconfig::Thinking(reasoning)).await?;
                    shell.notice("queued thinking change applied");
                }
            }
            PendingIdleAction::Skills(sub) => {
                if sub == commands::SkillsSubcommand::Reload {
                    app = reload_resources(app, shell, input).await?;
                    shell.notice("queued skills and prompt templates reload applied");
                } else {
                    execute_skills_command(&mut app, shell, sub).await?;
                }
            }
        }
        request_extension_ui(shell, &mut app);
        shell.render();
    }
    Ok(app)
}

async fn execute_skills_command(
    app: &mut App,
    shell: &mut InteractiveShell,
    sub: commands::SkillsSubcommand,
) -> anyhow::Result<()> {
    match sub {
        commands::SkillsSubcommand::List => {
            let mut text = String::from("Discovered skills:\n");
            let descriptors = app.skills.descriptors();
            if descriptors.is_empty() {
                text.push_str("  (none found)");
            } else {
                for desc in descriptors.iter() {
                    text.push_str(&format!(
                        "  - {} (v{}) [trust: {:?}]\n    {}\n",
                        desc.id,
                        desc.version.as_deref().unwrap_or("1.0"),
                        desc.trust,
                        desc.description
                    ));
                }
            }
            let diagnostics = app.skills.diagnostics();
            if !diagnostics.is_empty() {
                const SHOWN_DIAGNOSTICS: usize = 20;
                text.push_str("\nDiagnostics:\n");
                for diagnostic in diagnostics.iter().take(SHOWN_DIAGNOSTICS) {
                    text.push_str(&format!(
                        "  - {}\n    {}\n",
                        diagnostic.path.display(),
                        diagnostic.message
                    ));
                }
                if diagnostics.len() > SHOWN_DIAGNOSTICS {
                    text.push_str(&format!(
                        "  ... and {} more; narrow the configured skill directories\n",
                        diagnostics.len() - SHOWN_DIAGNOSTICS
                    ));
                }
            }
            shell.show_overlay_text(text);
        }
        commands::SkillsSubcommand::Show(id) => {
            let descriptors = app.skills.descriptors();
            if let Some(desc) = descriptors.iter().find(|d| d.id == id) {
                let text = format!(
                    "Skill: {}\nName: {}\nVersion: {}\nTrust Level: {:?}\nRequired Tools: {:?}\nTags: {:?}\n\nDescription:\n{}",
                    desc.id,
                    desc.name,
                    desc.version.as_deref().unwrap_or("1.0"),
                    desc.trust,
                    desc.required_tools,
                    desc.tags,
                    desc.description
                );
                shell.show_overlay_text(text);
            } else {
                shell.error(format!("Skill '{}' not found", id));
            }
        }
        commands::SkillsSubcommand::Active => {
            let mut text = String::from("Active skills:\n");
            if let Some(head_id) = app.agent.session().head() {
                match app.agent.session().resolve_active_skills(&head_id) {
                    Ok(state) => {
                        if state.active_skills.is_empty() {
                            text.push_str("  (none active)");
                        } else {
                            for skill in state.active_skills {
                                text.push_str(&format!(
                                    "  - {} (activation: {})\n",
                                    skill.descriptor.id, skill.activation_id.0
                                ));
                            }
                        }
                    }
                    Err(e) => {
                        text.push_str(&format!("  (failed to resolve: {e})"));
                    }
                }
            } else {
                text.push_str("  (empty session)");
            }
            shell.show_overlay_text(text);
        }
        commands::SkillsSubcommand::Search(query) => {
            let results = app.skills.find(&ygg_agent::skills::SkillQuery {
                text: query.clone(),
            });
            let mut text = format!("Skills matching {query:?}:\n");
            if results.is_empty() {
                text.push_str("  (none found)");
            } else {
                for result in results {
                    text.push_str(&format!(
                        "  - {} · {}\n    {}\n",
                        result.descriptor.id, result.descriptor.name, result.descriptor.description
                    ));
                }
            }
            shell.show_overlay_text(text);
        }
        commands::SkillsSubcommand::Load(id) => match app.skills.load(&id) {
            Ok(loaded) => {
                let registered_tools = app.agent.registered_tool_names();
                if let Err(error) =
                    validate_skill_requirements(&loaded.descriptor, &registered_tools)
                {
                    shell.error(format!("Failed to load skill '{id}': {error}"));
                    return Ok(());
                }
                let event = ygg_agent::session::EntryValue::SkillActivated {
                    descriptor: loaded.descriptor.clone(),
                    instructions_hash: loaded.content_hash.clone(),
                    instructions: loaded.instructions.clone(),
                };
                match app.agent.session_mut().append(event) {
                    Ok(act_id) => {
                        shell.notice(format!(
                            "Skill '{}' loaded successfully (activation: {})",
                            id, act_id.0
                        ));
                    }
                    Err(e) => {
                        shell.error(format!("Failed to record skill activation in session: {e}"));
                    }
                }
            }
            Err(e) => {
                shell.error(format!("Failed to load skill '{}': {}", id, e));
            }
        },
        commands::SkillsSubcommand::Reload => {
            shell.error("skill reload must run at an idle resource boundary".into());
        }
        commands::SkillsSubcommand::Off(id) => {
            let mut act_id_opt = None;
            if let Some(head_id) = app.agent.session().head() {
                if let Ok(state) = app.agent.session().resolve_active_skills(&head_id) {
                    if let Some(skill) = state.active_skills.iter().find(|s| s.descriptor.id == id)
                    {
                        act_id_opt = Some(skill.activation_id.clone());
                    }
                }
            }
            if let Some(act_id) = act_id_opt {
                let event = ygg_agent::session::EntryValue::SkillDeactivated {
                    activation_id: act_id.clone(),
                    skill_id: id.clone(),
                };
                match app.agent.session_mut().append(event) {
                    Ok(_) => {
                        shell.notice(format!(
                            "Skill '{}' deactivated (unloaded activation: {})",
                            id, act_id.0
                        ));
                    }
                    Err(e) => {
                        shell.error(format!("Failed to record skill deactivation: {e}"));
                    }
                }
            } else {
                shell.error(format!(
                    "Skill '{}' is not currently active on this branch",
                    id
                ));
            }
        }
    }
    Ok(())
}

enum IdleCommandOutcome {
    Continue(Box<App>),
    Submit { app: Box<App>, prompt: String },
    Quit(Box<App>),
}

fn prompt_templates_text(app: &App) -> String {
    let descriptors = app.prompts.descriptors();
    let mut text = String::from("Prompt templates:\n");
    if descriptors.is_empty() {
        text.push_str("  (none found under ~/.ygg/prompts, .ygg/prompts, or explicit paths)");
    } else {
        for descriptor in descriptors.iter() {
            let hint = descriptor
                .argument_hint
                .as_deref()
                .map(|hint| format!(" {hint}"))
                .unwrap_or_default();
            text.push_str(&format!(
                "  /{}{hint}\n    {} · {:?}\n",
                descriptor.name, descriptor.description, descriptor.trust
            ));
        }
    }
    let diagnostics = app.prompts.diagnostics();
    if !diagnostics.is_empty() {
        text.push_str("\nDiagnostics:\n");
        for diagnostic in diagnostics.iter() {
            text.push_str(&format!(
                "  - {}: {}\n",
                diagnostic.path.display(),
                diagnostic.message
            ));
        }
    }
    text
}

fn split_prompt_invocation(invocation: &str) -> Option<(&str, &str)> {
    let invocation = invocation.trim().trim_start_matches('/');
    let end = invocation
        .find(char::is_whitespace)
        .unwrap_or(invocation.len());
    let name = &invocation[..end];
    (!name.is_empty()).then(|| (name, invocation[end..].trim_start()))
}

fn expand_prompt_invocation(
    app: &mut App,
    invocation: &str,
    require_match: bool,
    selection: Option<&str>,
) -> anyhow::Result<Option<RenderedPrompt>> {
    let Some((name, arguments)) = split_prompt_invocation(invocation) else {
        return Ok(None);
    };
    if require_match && !app.prompts.contains(name) {
        return Ok(None);
    }
    let prompts = app.prompts.clone();
    let workspace = app.config.workspace.clone();
    render_and_record(
        &prompts,
        app.agent.session_mut(),
        &workspace,
        name,
        arguments,
        selection,
    )
    .map(Some)
    .map_err(Into::into)
}

async fn run_idle_command(
    mut app: App,
    shell: &mut InteractiveShell,
    input: &mut EventStream,
    command: Command,
) -> anyhow::Result<IdleCommandOutcome> {
    match command {
        Command::Status => {
            shell.show_status_text_with_telemetry(commands::status_text(&app, None));
        }
        Command::Context => {
            shell.show_context_report(crate::tui::context::ContextReport::capture(&app, &[]));
        }
        Command::Cost => {
            shell.show_overlay_text(commands::cost_text(app.agent.session(), &app.model))
        }
        Command::Cache => shell.show_overlay_text(commands::cache_text(app.agent.session())),
        Command::Update => {
            match await_lifecycle(shell, input, "checking for updates…", async {
                crate::update::check().await
            })
            .await
            {
                Ok(status) => shell.show_overlay_text(status.to_string()),
                Err(error) => shell.error(format!("update check failed: {error}")),
            }
        }
        Command::Name(name) => {
            let id = app
                .agent
                .session()
                .path()
                .file_stem()
                .and_then(|value| value.to_str())
                .ok_or_else(|| anyhow::anyhow!("current session has no valid id"))?
                .to_owned();
            match name {
                Some(name) => {
                    let metadata = app.sessions.rename(&id, &name)?;
                    shell.notice(format!(
                        "session named {}",
                        metadata.name.as_deref().unwrap_or("(unnamed)")
                    ));
                    request_extension_ui(shell, &mut app);
                }
                None => {
                    let metadata = app.sessions.load_metadata(&id)?;
                    shell.notice(format!(
                        "session name: {}",
                        metadata
                            .name
                            .as_deref()
                            .unwrap_or("(derived from first prompt)")
                    ));
                }
            }
        }
        Command::Sessions => {
            let store = app.sessions.clone();
            let sessions =
                run_blocking_lifecycle(shell, input, "discovering sessions…", move || {
                    Ok(store.list())
                })
                .await?;
            let text = if sessions.is_empty() {
                "No sessions in this workspace.".to_owned()
            } else {
                let mut lines = vec!["Sessions".to_owned()];
                lines.extend(sessions.into_iter().map(|session| {
                    let name = session.name.as_deref().unwrap_or(&session.title);
                    let tags = if session.tags.is_empty() {
                        String::new()
                    } else {
                        format!(" · {}", session.tags.join(", "))
                    };
                    format!("- {} · {name}{tags}", session.id)
                }));
                lines.join("\n")
            };
            shell.show_overlay_text(text);
        }
        Command::Export(output) => {
            let id = app
                .agent
                .session()
                .path()
                .file_stem()
                .and_then(|value| value.to_str())
                .ok_or_else(|| anyhow::anyhow!("current session has no valid id"))?;
            let report = crate::session_commands::export_portable(
                &app.sessions,
                id,
                output.map(std::path::PathBuf::from),
                &app.config.invocation_cwd,
                false,
                false,
            )?;
            shell.show_overlay_text(format!(
                "Exported {}\nRedacted {} potentially sensitive values{}",
                report.destination.display(),
                report.redaction_count,
                if report.ignored_torn_tail {
                    "\nIgnored an interrupted final append; use `ygg sessions repair`."
                } else {
                    ""
                }
            ));
        }
        Command::Prompt(None) => shell.show_overlay_text(prompt_templates_text(&app)),
        Command::Prompt(Some(invocation)) => {
            let selection = shell.selected_plain_text();
            match expand_prompt_invocation(&mut app, &invocation, false, selection.as_deref()) {
                Ok(Some(rendered)) => {
                    if app.config.debug_prompt {
                        shell.show_overlay_text(crate::prompts::debug_expansion(&rendered));
                    }
                    return Ok(IdleCommandOutcome::Submit {
                        app: Box::new(app),
                        prompt: rendered.text,
                    });
                }
                Ok(None) => shell.error("usage: /prompt <name> [arguments]".into()),
                Err(error) => shell.error(error.to_string()),
            }
        }
        Command::Extensions(commands::ExtensionsSubcommand::List) => {
            request_extension_ui(shell, &mut app);
            shell.show_overlay_text(app.executable_extensions.inspect_text());
        }
        Command::Extensions(commands::ExtensionsSubcommand::Reload) => {
            let messages = await_lifecycle(shell, input, "reloading extensions…", async {
                Ok(app.executable_extensions.reload().await)
            })
            .await?;
            if messages.is_empty() {
                shell.notice("no running executable extensions to reload");
            } else {
                shell.show_overlay_text(messages.join("\n"));
            }
            request_extension_ui(shell, &mut app);
        }
        Command::Quit => return Ok(IdleCommandOutcome::Quit(Box::new(app))),
        Command::Login(provider) => match validate_provider(provider.as_deref()) {
            Ok("codex") => login_codex(&mut app, shell).await?,
            Ok("custom") => login_custom(shell)?,
            Ok(_) => unreachable!(),
            Err(e) => shell.error(e.to_string()),
        },
        Command::Logout(provider) => match validate_provider(provider.as_deref()) {
            Ok("codex") => {
                app = logout_codex(app, shell, input).await?;
            }
            Ok("custom") => {
                app = logout_custom(app, shell, input).await?;
            }
            Ok(_) => unreachable!(),
            Err(e) => shell.error(e.to_string()),
        },
        Command::New => {
            app = transition(app, shell, input, Reconfig::NewSession).await?;
            shell.notice("created a new session");
        }
        Command::Resume(Some(id)) => {
            let path = app.sessions.path_by_id(&id)?;
            app = transition(app, shell, input, Reconfig::Resume(path)).await?;
            shell.notice("resumed session");
        }
        Command::Resume(None) => {
            if let Some(path) = pick_session_path(shell, input, &app.sessions).await? {
                app = transition(app, shell, input, Reconfig::Resume(path)).await?;
                shell.notice("resumed session");
            }
        }
        Command::Model(Some(id)) => {
            app = transition(app, shell, input, Reconfig::Model(ModelId(id))).await?;
            shell.notice(format!(
                "model changed · {}",
                commands::model_selection_text(&app.model)
            ));
        }
        Command::CycleModel => {
            let id = next_model_id(&app)?;
            app = transition(app, shell, input, Reconfig::Model(id)).await?;
            shell.notice(format!(
                "model changed · {}",
                commands::model_selection_text(&app.model)
            ));
        }
        Command::Thinking(Some(level)) => {
            let level = ThinkingLevel::parse(&level)?;
            let reasoning = thinking_to_reasoning(level, &app.model)?;
            if let Err(e) = crate::cli::persist_reasoning(level.label()) {
                shell.error(format!("failed to save thinking preference: {e}"));
            }
            app = transition(app, shell, input, Reconfig::Thinking(reasoning)).await?;
            shell.notice("thinking changed");
        }
        Command::Model(None) => {
            if let Some(model) = optional_model_picker(shell, input, &app.catalog).await? {
                app = transition(app, shell, input, Reconfig::Model(model)).await?;
                shell.notice(format!(
                    "model changed · {}",
                    commands::model_selection_text(&app.model)
                ));
            }
        }
        Command::Thinking(None) => {
            if let Some(level) =
                thinking_picker(shell, input, &supported_levels(&app.model)).await?
            {
                let reasoning = thinking_to_reasoning(level, &app.model)?;
                app = transition(app, shell, input, Reconfig::Thinking(reasoning)).await?;
                shell.notice("thinking changed");
            }
        }
        Command::Theme(Some(name)) if name == "list" => {
            if let Err(error) = show_theme_choices(shell, input).await {
                shell.error(error.to_string());
            }
        }
        Command::Theme(Some(name)) if name == "reload" => {
            if let Err(error) = reload_active_theme(shell, input).await {
                shell.error(format!("unable to reload theme: {error}"));
            }
        }
        Command::Theme(Some(name)) => {
            if let Err(error) = apply_theme(shell, input, &name).await {
                shell.error(error.to_string());
            }
        }
        Command::Theme(None) => match theme_choices(shell, input).await {
            Ok(names) => {
                if let Some(name) = theme_picker(shell, input, &names).await? {
                    if let Err(error) = apply_theme(shell, input, &name).await {
                        shell.error(error.to_string());
                    }
                }
            }
            Err(error) => {
                shell.error(error.to_string());
            }
        },
        Command::Tool(_id) => {
            shell.notice("tool details follow transcript verbosity; use Ctrl+O or /verbose")
        }
        Command::Verbose(value) => {
            let enabled = value.unwrap_or(!shell.verbose_tools());
            shell.set_verbose_tools(enabled);
            shell.notice(format!(
                "verbose transcript {}",
                if enabled { "enabled" } else { "disabled" }
            ));
        }
        Command::Compact => {
            if let Some(message) = cost_limit_message(&app) {
                shell.error(message);
            } else {
                shell.set_run_label("compacting…");
                shell.render();
                let original_keep = app.config.compaction.keep_recent_turns;
                app.config.compaction.keep_recent_turns = 1;
                let result = await_with_ctrl_c(attempt_compaction(&mut app), input).await;
                app.config.compaction.keep_recent_turns = original_keep;
                match result {
                    Some(Ok(outcome)) => {
                        report_compaction(shell, &outcome, app.agent.session());
                    }
                    Some(Err(error)) => shell.error(format!("compaction failed: {error}")),
                    None => shell.notice("compaction cancelled"),
                }
                if let Some(message) = cost_limit_message(&app) {
                    shell.error(message);
                }
                update_status(shell, &app);
                shell.set_run_label("idle");
            }
        }
        Command::AutoCompact(setting) => {
            configure_auto_compaction(&mut app, shell, setting)?;
            update_status(shell, &app);
        }
        Command::Reload => {
            app = reload_resources(app, shell, input).await?;
            request_extension_ui(shell, &mut app);
            shell.notice("instructions, themes, prompts, skills, and extensions reloaded");
        }
        Command::Tree => shell.show_overlay_text(session_tree_text(app.agent.session())),
        Command::Checkout(id) => {
            app = checkout_entry(app, shell, input, id).await?;
        }
        Command::Skills(commands::SkillsSubcommand::Reload) => {
            app = reload_resources(app, shell, input).await?;
            request_extension_ui(shell, &mut app);
            shell.notice("skills and prompt templates reloaded");
        }
        Command::Skills(sub) => {
            execute_skills_command(&mut app, shell, sub).await?;
        }
        Command::Unknown(text) => {
            let (extension_name, extension_arguments) = split_prompt_invocation(&text)
                .map(|(name, arguments)| {
                    (
                        name.to_owned(),
                        arguments
                            .split_whitespace()
                            .map(str::to_owned)
                            .collect::<Vec<_>>(),
                    )
                })
                .unwrap_or_default();
            let result = {
                let mut confirmations = InteractiveExtensionConfirmations { shell, input };
                app.executable_extensions
                    .execute_command_with_confirmation(
                        &extension_name,
                        extension_arguments,
                        &mut confirmations,
                    )
                    .await
            };
            match result {
                Ok(Some(output)) => {
                    if output.trim().is_empty() {
                        shell.notice(format!("/{extension_name} completed"));
                    } else {
                        shell.show_overlay_text(output);
                    }
                }
                Ok(None) => {
                    let selection = shell.selected_plain_text();
                    match expand_prompt_invocation(&mut app, &text, true, selection.as_deref()) {
                        Ok(Some(rendered)) => {
                            if app.config.debug_prompt {
                                shell.show_overlay_text(crate::prompts::debug_expansion(&rendered));
                            }
                            return Ok(IdleCommandOutcome::Submit {
                                app: Box::new(app),
                                prompt: rendered.text,
                            });
                        }
                        Ok(None) => shell.error(format!("unknown command: {text}")),
                        Err(error) => shell.error(error.to_string()),
                    }
                }
                Err(error) => shell.error(format!("extension command failed: {error}")),
            }
        }
    }
    shell.render();
    Ok(IdleCommandOutcome::Continue(Box::new(app)))
}

#[derive(Default)]
struct BoundedShellOutput {
    head: Vec<u8>,
    tail: Vec<u8>,
    total_bytes: usize,
    budget: usize,
}

impl BoundedShellOutput {
    fn new(budget: usize) -> Self {
        Self {
            budget,
            ..Self::default()
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.total_bytes = self.total_bytes.saturating_add(bytes.len());
        let head_capacity = self.budget / 2;
        let tail_capacity = self.budget.saturating_sub(head_capacity);
        let mut remaining = bytes;
        if self.head.len() < head_capacity {
            let keep = remaining.len().min(head_capacity - self.head.len());
            self.head.extend_from_slice(&remaining[..keep]);
            remaining = &remaining[keep..];
        }
        if remaining.is_empty() || tail_capacity == 0 {
            return;
        }
        if remaining.len() >= tail_capacity {
            self.tail.clear();
            self.tail
                .extend_from_slice(&remaining[remaining.len() - tail_capacity..]);
            return;
        }
        let overflow = self
            .tail
            .len()
            .saturating_add(remaining.len())
            .saturating_sub(tail_capacity);
        if overflow > 0 {
            self.tail.drain(..overflow);
        }
        self.tail.extend_from_slice(remaining);
    }

    fn render(&self, stream: &str) -> String {
        if self.total_bytes <= self.budget {
            let mut complete = Vec::with_capacity(self.total_bytes);
            complete.extend_from_slice(&self.head);
            complete.extend_from_slice(&self.tail);
            return String::from_utf8_lossy(&complete).into_owned();
        }
        let omitted = self
            .total_bytes
            .saturating_sub(self.head.len())
            .saturating_sub(self.tail.len());
        format!(
            "{}\n[… {stream} truncated; {omitted} bytes omitted …]\n{}",
            String::from_utf8_lossy(&self.head),
            String::from_utf8_lossy(&self.tail)
        )
    }
}

async fn drain_shell_pipe<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut Option<R>,
    capture: &std::sync::Arc<std::sync::Mutex<BoundedShellOutput>>,
    updates: &tokio::sync::mpsc::UnboundedSender<()>,
) {
    use tokio::io::AsyncReadExt as _;

    let Some(reader) = reader.as_mut() else {
        return;
    };
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(read) => {
                capture
                    .lock()
                    .expect("shell output mutex poisoned")
                    .push(&buffer[..read]);
                let _ = updates.send(());
            }
        }
    }
}

fn rendered_shell_captures(
    stdout: &std::sync::Arc<std::sync::Mutex<BoundedShellOutput>>,
    stderr: &std::sync::Arc<std::sync::Mutex<BoundedShellOutput>>,
) -> String {
    let out = stdout
        .lock()
        .expect("shell stdout mutex poisoned")
        .render("stdout");
    let err = stderr
        .lock()
        .expect("shell stderr mutex poisoned")
        .render("stderr");
    let mut combined = String::new();
    if !out.is_empty() {
        combined.push_str(out.trim_end());
    }
    if !err.is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(err.trim_end());
    }
    combined
}

async fn shutdown_for_exit(app: &mut App) {
    if crate::tui::terminal::received_shutdown_signal().is_some() {
        ygg_agent::extension_process::terminate_exec_process_groups(Duration::from_millis(400))
            .await;
        let _ = tokio::time::timeout(
            Duration::from_millis(1400),
            app.executable_extensions.shutdown(),
        )
        .await;
        ygg_agent::extension_process::force_kill_registered_process_groups();
    } else {
        app.executable_extensions.shutdown().await;
    }
}

fn explicit_terminal_background_override() -> bool {
    std::env::var("YGG_COLOR_SCHEME")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "dark" | "light" | "unknown" | "universal"
            )
        })
        .unwrap_or(false)
}

fn apply_detected_terminal_background(
    shell: &mut InteractiveShell,
    config: &crate::config::Config,
) {
    if explicit_terminal_background_override()
        || shell.theme().background() != TerminalBackground::Unknown
    {
        return;
    }
    let Some((red, green, blue)) =
        crate::tui::terminal::query_terminal_background_color(Duration::from_millis(120))
    else {
        return;
    };
    let background = background_from_terminal_rgb(red, green, blue);
    shell.set_theme(load_theme_for_background(config, background));
}

/// Run the interactive frontend with explicit idle and active borrow phases.
pub async fn run_interactive(boot: Bootstrap) -> anyhow::Result<()> {
    let initial_prompt = boot.config.initial_prompt.clone();
    let theme = load_theme(&boot.config);
    let size = Arc::new(Mutex::new(crossterm::terminal::size().unwrap_or((80, 24))));
    let mut shell =
        InteractiveShell::enter_with_mouse(theme, size, boot.config.mouse.application_owned())?;
    shell.set_theme_config(boot.config.clone());
    apply_detected_terminal_background(&mut shell, &boot.config);
    let mut input = EventStream::new();
    // The shell owns a dedicated renderer thread, but sexy-tui still renders
    // synchronously when that thread receives a request. This clock only
    // coalesces high-rate wheel input on the input loop.
    let mut scroll_tick = tokio::time::interval(Duration::from_millis(16));
    scroll_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut extension_tick = tokio::time::interval(Duration::from_millis(50));
    extension_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let launch = resolve_launch_interactive(&boot, &mut shell, &mut input).await?;
    let mut app = run_blocking_lifecycle(
        &mut shell,
        &mut input,
        "starting extensions…",
        move || {
            let system = compose_instructions(&boot.config)?;
            build_app(boot, launch, system)
        },
    )
    .await?;
    let mut startup_prompt = initial_prompt;
    if let Some(name) = app.config.prompt_template.clone() {
        let arguments = startup_prompt.take().unwrap_or_default();
        let rendered =
            expand_prompt_invocation(&mut app, &format!("{name} {arguments}"), false, None)?
                .ok_or_else(|| anyhow::anyhow!("prompt template name is missing"))?;
        if app.config.debug_prompt {
            shell.show_overlay_text(crate::prompts::debug_expansion(&rendered));
        }
        startup_prompt = Some(rendered.text);
    }
    shell.hydrate(app.agent.session())?;
    update_status(&mut shell, &app);
    request_extension_ui(&mut shell, &mut app);
    shell.render();

    let mut pending_actions = VecDeque::new();
    'interactive: loop {
        let idle = match startup_prompt.take() {
            Some(prompt) if !prompt.is_empty() => Idle::Submit(ComposedInput::from_text(prompt)),
            _ => {
                wait_for_prompt(
                    &mut shell,
                    &mut input,
                    &mut scroll_tick,
                    &mut extension_tick,
                    &mut app.executable_extensions,
                )
                .await?
            }
        };
        match idle {
            Idle::Quit => {
                shutdown_for_exit(&mut app).await;
                break;
            }
            Idle::CycleThinking => {
                let level = next_thinking_level(&app)?;
                if let Err(error) = crate::cli::persist_reasoning(level.label()) {
                    shell.error(format!("failed to save thinking preference: {error}"));
                }
                let reasoning = thinking_to_reasoning(level, &app.model)?;
                app =
                    transition(app, &mut shell, &mut input, Reconfig::Thinking(reasoning)).await?;
                shell.notice(format!("thinking changed to {}", level.label()));
                shell.render();
            }
            Idle::Command(command_input) => {
                match run_idle_command(app, &mut shell, &mut input, commands::parse(&command_input))
                    .await?
                {
                    IdleCommandOutcome::Continue(next) => app = *next,
                    IdleCommandOutcome::Submit { app: next, prompt } => {
                        app = *next;
                        startup_prompt = Some(prompt);
                    }
                    IdleCommandOutcome::Quit(next) => {
                        app = *next;
                        shutdown_for_exit(&mut app).await;
                        break;
                    }
                }
            }
            Idle::Submit(mut composed) => {
                // Shell escapes have the same authority as the model `exec`
                // tool and executable extensions. Never let this local UX
                // bypass the product-wide process gate.
                if let Some(command) = composed
                    .display_text
                    .trim()
                    .strip_prefix('!')
                    .map(|s| s.trim().to_owned())
                {
                    if !app.config.sandbox.process_execution_allowed() {
                        shell.error(
                            "shell commands are disabled by --no-process/--no-shell".to_owned(),
                        );
                        shell.render();
                        continue;
                    }
                    if command.is_empty() {
                        shell.notice("usage: !<shell command>");
                        shell.render();
                        continue;
                    }
                    shell.on_local_command_submitted(&format!("!{command}"));
                    let shell_id = shell.append_shell_in_progress(command.clone());
                    shell.render();

                    let workspace = app.config.workspace.clone();
                    let cmd = command.clone();

                    // Spawn the child process with piped output.
                    let mut process = tokio::process::Command::new("sh");
                    process
                        .arg("-c")
                        .arg(&cmd)
                        .current_dir(&workspace)
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::piped())
                        .stdin(std::process::Stdio::null())
                        .kill_on_drop(true);
                    #[cfg(unix)]
                    process.process_group(0);
                    let mut child = match process.spawn() {
                        Ok(child) => child,
                        Err(error) => {
                            shell.finalize_shell(
                                &shell_id,
                                format!("failed to spawn: {error}"),
                                -1,
                            );
                            shell.render();
                            continue;
                        }
                    };
                    #[cfg(unix)]
                    let group_guard = ProcessGroupGuard::exec(child.id());

                    // Animate a braille spinner while the process runs.
                    const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
                    let mut spinner_tick = tokio::time::interval(Duration::from_millis(80));
                    spinner_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
                    let mut frame: usize = 0;
                    let mut stdout_pipe = child.stdout.take();
                    let mut stderr_pipe = child.stderr.take();
                    let output_budget = app.config.sandbox.max_output_bytes;
                    let stdout_budget = output_budget / 2;
                    let stderr_budget = output_budget.saturating_sub(stdout_budget);
                    let stdout = std::sync::Arc::new(std::sync::Mutex::new(
                        BoundedShellOutput::new(stdout_budget),
                    ));
                    let stderr = std::sync::Arc::new(std::sync::Mutex::new(
                        BoundedShellOutput::new(stderr_budget),
                    ));
                    let (output_tx, mut output_rx) = tokio::sync::mpsc::unbounded_channel();
                    let command_timeout = Duration::from_secs(app.config.sandbox.exec_timeout_secs);
                    #[cfg(unix)]
                    let command_started = tokio::time::Instant::now();
                    let stdout_capture = stdout.clone();
                    let stderr_capture = stderr.clone();
                    let stdout_updates = output_tx.clone();
                    let stderr_updates = output_tx;
                    let work = async {
                        let (_, _, status) = tokio::join!(
                            drain_shell_pipe(&mut stdout_pipe, &stdout_capture, &stdout_updates,),
                            drain_shell_pipe(&mut stderr_pipe, &stderr_capture, &stderr_updates,),
                            child.wait(),
                        );
                        status
                    };
                    let mut work = Box::pin(work);
                    let deadline = tokio::time::sleep(command_timeout);
                    tokio::pin!(deadline);
                    let mut input_open = true;
                    let mut interrupted = false;
                    let mut timed_out = false;
                    let mut shutting_down = false;

                    let exit = loop {
                        tokio::select! {
                            biased;
                            _ = crate::tui::terminal::wait_for_shutdown_signal() => {
                                interrupted = true;
                                shutting_down = true;
                                break Err(std::io::Error::new(
                                    std::io::ErrorKind::Interrupted,
                                    "command stopped during shutdown",
                                ));
                            }
                            status = &mut work => {
                                break status;
                            }
                            event = input.next(), if input_open => match event {
                                Some(Ok(Event::Key(key)))
                                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                                        && key.code == KeyCode::Char('c')
                                        && key.modifiers == KeyModifiers::CONTROL =>
                                {
                                    interrupted = true;
                                    break Err(std::io::Error::new(
                                        std::io::ErrorKind::Interrupted,
                                        "command cancelled",
                                    ));
                                }
                                Some(Ok(Event::Key(key)))
                                    if key.kind == KeyEventKind::Press
                                        && key.code == KeyCode::Char('o')
                                        && key.modifiers == KeyModifiers::CONTROL =>
                                {
                                    shell.expand_focused_tool();
                                    shell.render();
                                }
                                Some(Ok(Event::Resize(columns, rows))) => {
                                    shell.set_size(columns, rows);
                                    shell.render();
                                }
                                Some(Ok(_)) => {}
                                Some(Err(_)) | None => input_open = false,
                            },
                            _ = &mut deadline => {
                                timed_out = true;
                                break Err(std::io::Error::new(
                                    std::io::ErrorKind::TimedOut,
                                    "command timed out",
                                ));
                            }
                            update = output_rx.recv() => {
                                if update.is_some() {
                                    // Collapse a burst into one bounded tail update. This keeps
                                    // the latest process lines visible without repainting once
                                    // per read syscall.
                                    while output_rx.try_recv().is_ok() {}
                                    shell.update_shell_output(
                                        &shell_id,
                                        rendered_shell_captures(&stdout, &stderr),
                                    );
                                    shell.render();
                                }
                            }
                            _ = spinner_tick.tick() => {
                                shell.update_shell_spinner(&shell_id, SPINNER[frame]);
                                frame = (frame + 1) % SPINNER.len();
                                shell.render();
                            }
                        }
                    };

                    if shutting_down {
                        #[cfg(unix)]
                        {
                            let process_cleanup =
                                ygg_agent::extension_process::terminate_exec_process_groups(
                                    Duration::from_millis(400),
                                );
                            let _ = tokio::time::timeout(Duration::from_millis(500), async {
                                tokio::join!(&mut work, process_cleanup)
                            })
                            .await;
                            group_guard.terminate_now();
                        }
                        drop(work);
                        #[cfg(not(unix))]
                        {
                            let _ = child.kill().await;
                            let _ = child.wait().await;
                        }
                        let mut combined = rendered_shell_captures(&stdout, &stderr);
                        if !combined.is_empty() {
                            combined.push('\n');
                        }
                        combined.push_str("command stopped during shutdown");
                        shell.finalize_shell(&shell_id, combined, -1);
                        shell.render();
                        shutdown_for_exit(&mut app).await;
                        break 'interactive;
                    }

                    if interrupted || timed_out {
                        #[cfg(unix)]
                        {
                            group_guard.terminate_now();
                            // Retain output already in the pipes when ordinary
                            // descendants close promptly, but an escaped child
                            // must not defeat the execution deadline forever.
                            let _ =
                                tokio::time::timeout(Duration::from_millis(500), &mut work).await;
                        }
                    } else {
                        #[cfg(unix)]
                        group_guard.supervise_exec_descendants(
                            command_timeout.saturating_sub(command_started.elapsed()),
                            Default::default(),
                        );
                    }
                    // Releasing the concurrent wait/drain future closes any
                    // descriptors retained by an escaped descendant.
                    drop(work);
                    #[cfg(not(unix))]
                    if interrupted || timed_out {
                        let _ = child.kill().await;
                    }

                    let exit_code = match exit {
                        Ok(status) => status.code().unwrap_or(-1),
                        Err(error) => {
                            let mut combined = rendered_shell_captures(&stdout, &stderr);
                            if !combined.is_empty() {
                                combined.push('\n');
                            }
                            if interrupted {
                                combined.push_str("command cancelled");
                            } else if timed_out {
                                combined.push_str(&format!(
                                    "command exceeded the {}s execution limit",
                                    app.config.sandbox.exec_timeout_secs
                                ));
                            } else {
                                combined.push_str(&format!("process error: {error}"));
                            }
                            shell.finalize_shell(&shell_id, combined, -1);
                            shell.render();
                            continue;
                        }
                    };

                    let combined = rendered_shell_captures(&stdout, &stderr);
                    shell.finalize_shell(&shell_id, combined, exit_code);
                    shell.render();
                    continue;
                }

                if let Some(message) = cost_limit_message(&app) {
                    shell.error(message);
                    shell.render();
                    continue;
                }
                app.executable_extensions.refresh_host_state(
                    app.agent.session(),
                    &app.model,
                    &app.reasoning,
                    &app.sessions,
                );
                let composition = tokio::select! {
                    biased;
                    _ = crate::tui::terminal::wait_for_shutdown_signal() => {
                        shell.restore_composed(composed);
                        shutdown_for_exit(&mut app).await;
                        break 'interactive;
                    }
                    result = await_with_ctrl_c(
                        app.executable_extensions.compose_prompt(
                            &app.system,
                            composed.transcript_text.clone(),
                        ),
                        &mut input,
                    ) => result,
                };
                let Some(composition) = composition else {
                    shell.restore_composed(composed);
                    shell.notice("extension prompt composition cancelled");
                    shell.render();
                    continue;
                };
                let composition = match composition {
                    Ok(composition) => composition,
                    Err(error) => {
                        shell.restore_composed(composed);
                        shell.error(format!("extension prompt composition failed: {error}"));
                        shell.render();
                        continue;
                    }
                };
                let pending_context_count = composition.pending_context_count;
                for notification in composition.notifications {
                    shell.notice(notification);
                }
                app.agent.set_system_prompt(composition.system);
                let retry_composed = composed.clone();
                composed.replace_model_text(composition.prompt);
                // Keep extension context in the replayable model message, but
                // persist the exact user-facing draft separately for title and
                // transcript reconstruction.
                app.agent
                    .set_prompt_display_text(Some(composed.transcript_text.clone()));
                // Capacity checks and autonomous compaction live inside the
                // cancellable Agent run. Frontends must not start an
                // unabortable provider request before RunControl exists.
                shell.set_context_estimate(
                    estimate_next_request_tokens(&app, &composed.parts),
                    context_window(&app.model),
                );

                let mut run = match app.agent.prompt(composed.into_user_input()).await {
                    Ok(run) => run,
                    Err(error) => {
                        // No context commit occurred. The restored draft's next
                        // attempt recomposes from `app.system` and overwrites
                        // this transient composed Agent system before append.
                        shell.restore_composed(retry_composed);
                        shell.error(format!("prompt was not saved: {error}"));
                        shell.render();
                        continue;
                    }
                };
                app.executable_extensions
                    .commit_prompt_context(pending_context_count);
                prepare_prompt(&mut shell);
                let display = retry_composed.transcript_text;
                shell.on_prompt_submitted(&display);
                let run_id = shell.begin_run(&app.model.endpoint.id.0);
                shell.mark_prompt_persisted();
                shell.set_awaiting_provider(run_id);
                shell.render();
                let control = run.control();
                let mut quit_requested = false;
                let ended = drive_active_run(
                    &mut run,
                    &control,
                    &mut shell,
                    &mut input,
                    &mut scroll_tick,
                    &mut pending_actions,
                    &mut quit_requested,
                    app.config.max_cost_microdollars,
                    app.config.cost_warning_microdollars,
                    &mut app.executable_extensions,
                )
                .await?;
                drop(run);
                app.agent.set_system_prompt(app.system.clone());
                if crate::tui::terminal::received_shutdown_signal().is_some() {
                    shutdown_for_exit(&mut app).await;
                    break 'interactive;
                }
                // Hooks observe only the assistant output from this successful
                // run. Looking up the latest persisted assistant after a
                // failed/aborted run would resend a previous turn.
                if matches!(ended, RunEnded::Completed) {
                    let response = crate::extensions::latest_assistant_text(app.agent.session());
                    let notifications = tokio::select! {
                        biased;
                        _ = crate::tui::terminal::wait_for_shutdown_signal() => {
                            shutdown_for_exit(&mut app).await;
                            break 'interactive;
                        }
                        result = await_with_ctrl_c(
                            app.executable_extensions.after_response(&response),
                            &mut input,
                        ) => result,
                    };
                    if let Some(notifications) = notifications {
                        for notification in notifications {
                            shell.notice(notification);
                        }
                    } else {
                        shell.notice("extension after_response hooks cancelled");
                    }
                }
                // The run's tools may have created files; refresh mention
                // completion lazily on the next `@`.
                shell.invalidate_file_index();
                update_status(&mut shell, &app);
                request_extension_ui(&mut shell, &mut app);
                // `drive_active_run` settles the semantic outcome, while these
                // idle-boundary refreshes settle the final composer/footer.
                // Always publish that complete frame even when no queued idle
                // action follows to trigger another render.
                shell.render();
                if quit_requested {
                    shutdown_for_exit(&mut app).await;
                    break;
                }
                app = apply_pending_actions(app, &mut shell, &mut input, &mut pending_actions)
                    .await?;
            }
        }
    }
    shell.leave();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ygg_agent::EntryValue;

    #[tokio::test]
    async fn cancellable_wait_returns_none_on_ctrl_c() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use tokio_stream::wrappers::ReceiverStream;

        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        sender
            .send(Ok(Event::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
            ))))
            .await
            .unwrap();
        drop(sender);
        let mut input = ReceiverStream::new(receiver);

        let result = await_with_ctrl_c(std::future::pending::<()>(), &mut input).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn cancellable_wait_finishes_after_input_stream_closes() {
        let mut input = tokio_stream::empty::<std::io::Result<Event>>();
        assert_eq!(await_with_ctrl_c(async { 42 }, &mut input).await, Some(42));
    }

    #[test]
    fn bounded_shell_output_keeps_head_and_tail_within_budget() {
        let mut output = BoundedShellOutput::new(10);
        output.push(b"0123");
        output.push(b"456789");
        output.push(b"abcdef");

        assert_eq!(output.head, b"01234");
        assert_eq!(output.tail, b"bcdef");
        assert_eq!(output.total_bytes, 16);
        let rendered = output.render("stdout");
        assert!(rendered.starts_with("01234\n"), "{rendered:?}");
        assert!(rendered.contains("stdout truncated; 6 bytes omitted"));
        assert!(rendered.ends_with("\nbcdef"), "{rendered:?}");
    }

    #[test]
    fn bounded_shell_output_does_not_claim_untruncated_tail_was_omitted() {
        let mut output = BoundedShellOutput::new(10);
        output.push("012345é".as_bytes());

        assert_eq!(output.total_bytes, 8);
        assert_eq!(output.render("stdout"), "012345é");
    }

    #[tokio::test]
    async fn shell_pipes_are_drained_concurrently_with_process_exit() {
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg("yes o | head -c 1048576 & yes e | head -c 1048576 >&2 & wait")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();
        let stdout = std::sync::Arc::new(std::sync::Mutex::new(BoundedShellOutput::new(1024)));
        let stderr = std::sync::Arc::new(std::sync::Mutex::new(BoundedShellOutput::new(1024)));
        let (updates, mut update_rx) = tokio::sync::mpsc::unbounded_channel();

        let status = tokio::time::timeout(Duration::from_secs(5), async {
            let (_, _, status) = tokio::join!(
                drain_shell_pipe(&mut stdout_pipe, &stdout, &updates),
                drain_shell_pipe(&mut stderr_pipe, &stderr, &updates),
                child.wait(),
            );
            status
        })
        .await
        .expect("full stdout and stderr pipes must not deadlock")
        .unwrap();

        assert!(status.success());
        let stdout = stdout.lock().unwrap();
        let stderr = stderr.lock().unwrap();
        assert_eq!(stdout.total_bytes, 1_048_576);
        assert_eq!(stderr.total_bytes, 1_048_576);
        assert_eq!(stdout.head.len() + stdout.tail.len(), 1024);
        assert_eq!(stderr.head.len() + stderr.tail.len(), 1024);
        assert!(
            update_rx.try_recv().is_ok(),
            "pipe reads must wake live rendering"
        );
    }

    #[test]
    fn model_cycle_is_sorted_and_wraps() {
        let catalog = ygg_ai::ModelCatalog::builtin().unwrap();
        let mut ids = catalog
            .models()
            .map(|model| model.id.clone())
            .collect::<Vec<_>>();
        ids.sort_by(|left, right| left.0.cmp(&right.0));
        assert!(ids.len() > 1);
        for (index, current) in ids.iter().enumerate() {
            assert_eq!(
                next_model_id_in_catalog(&catalog, current).unwrap(),
                ids[(index + 1) % ids.len()]
            );
        }
    }

    #[test]
    fn session_tree_marks_the_durable_head_and_parent_links() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("tree.jsonl")).unwrap();
        let root = session
            .append(EntryValue::Config {
                model: Some("model".to_string()),
                reasoning: Some("off".to_string()),
            })
            .unwrap();
        let child = session
            .append(EntryValue::Config {
                model: None,
                reasoning: Some("high".to_string()),
            })
            .unwrap();
        session.checkout(root.clone()).unwrap();

        let tree = session_tree_text(&session);
        assert!(tree.contains(&format!("└─* {}  config", root.0)), "{tree}");
        assert!(tree.contains(&format!("└─  {}  config", child.0)), "{tree}");
    }

    #[test]
    fn failed_checkout_can_restore_the_previous_durable_head() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rollback.jsonl");
        let mut session = Session::create(&path).unwrap();
        let previous = session
            .append(EntryValue::Config {
                model: Some("model".to_string()),
                reasoning: Some("off".to_string()),
            })
            .unwrap();
        let target = session
            .append(EntryValue::Config {
                model: Some("missing-model".to_string()),
                reasoning: None,
            })
            .unwrap();
        session.checkout(target).unwrap();
        drop(session);

        restore_session_head(&path, previous.clone()).unwrap();
        assert_eq!(Session::open(path).unwrap().head(), Some(previous));
    }

    #[test]
    fn adjacent_reconfigurations_coalesce_but_boundaries_survive() {
        let mut queue = VecDeque::new();
        push_pending_action(
            &mut queue,
            PendingIdleAction::ChangeModel(ModelId("a".into())),
        );
        push_pending_action(
            &mut queue,
            PendingIdleAction::ChangeModel(ModelId("b".into())),
        );
        push_pending_action(&mut queue, PendingIdleAction::NewSession);
        push_pending_action(
            &mut queue,
            PendingIdleAction::ChangeModel(ModelId("c".into())),
        );
        assert_eq!(
            queue,
            VecDeque::from([
                PendingIdleAction::ChangeModel(ModelId("b".into())),
                PendingIdleAction::NewSession,
                PendingIdleAction::ChangeModel(ModelId("c".into())),
            ])
        );
    }

    #[test]
    fn command_queue_parses_reconfiguration_values() {
        let mut queue = VecDeque::new();
        queue_command(Command::Login(None), &mut queue).unwrap();
        queue_command(Command::Thinking(Some("high".into())), &mut queue).unwrap();
        queue_command(Command::Resume(Some("id".into())), &mut queue).unwrap();
        assert_eq!(queue.pop_front(), Some(PendingIdleAction::Login(None)));
        assert!(matches!(
            queue.pop_front(),
            Some(PendingIdleAction::ChangeThinkingLevel(ThinkingLevel::High))
        ));
        assert_eq!(
            queue.pop_front(),
            Some(PendingIdleAction::ResumeSession(Some("id".into())))
        );
    }

    #[test]
    fn active_theme_commands_wait_for_the_idle_boundary() {
        for command in [
            Command::Theme(None),
            Command::Theme(Some("list".into())),
            Command::Theme(Some("reload".into())),
            Command::Theme(Some("default".into())),
        ] {
            let mut shell = InteractiveShell::test_shell();
            let mut queue = VecDeque::new();
            let mut quit_requested = false;
            handle_active_command(&mut shell, command, &mut queue, &mut quit_requested);

            assert!(shell
                .debug_snapshot()
                .contains("theme commands are available at the next idle boundary"));
            assert!(queue.is_empty());
            assert!(!quit_requested);
        }
    }

    #[test]
    fn active_cost_and_cache_reports_wait_for_the_idle_boundary() {
        for command in [Command::Cost, Command::Cache] {
            let mut shell = InteractiveShell::test_shell();
            let mut queue = VecDeque::new();
            let mut quit_requested = false;
            handle_active_command(&mut shell, command, &mut queue, &mut quit_requested);

            assert!(shell
                .debug_snapshot()
                .contains("cost and cache reports are available at the next idle boundary"));
            assert!(queue.is_empty());
            assert!(!quit_requested);
        }
    }

    #[test]
    fn starting_a_new_prompt_clears_the_previous_error() {
        let mut shell = InteractiveShell::test_shell();
        shell.error("old failure".to_string());
        assert_eq!(shell.debug_error().as_deref(), Some("old failure"));

        prepare_prompt(&mut shell);

        assert_eq!(shell.debug_error(), None);
    }

    fn text_turn() -> String {
        concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"done\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        )
        .to_owned()
    }

    fn scripted_model(uri: &str) -> ygg_ai::Model {
        use std::sync::Arc;
        use std::time::Duration;
        use ygg_ai::{
            Auth, Capabilities, Endpoint, EndpointId, Modality, ModalitySet, ModelLimits,
            ModelSpec, Protocol,
        };

        ygg_ai::Model {
            spec: Arc::new(ModelSpec {
                id: ModelId("scripted".into()),
                endpoint: EndpointId("test".into()),
                api_name: "scripted".into(),
                display_name: None,
                protocol: Protocol::AnthropicMessages,
                capabilities: Capabilities {
                    input_modalities: ModalitySet::none().with(Modality::Image),
                    output_modalities: ModalitySet::none(),
                    tools: true,
                    parallel_tool_calls: false,
                    reasoning: None,
                    structured_output: false,
                },
                limits: ModelLimits {
                    context_window: 16_000,
                    max_output_tokens: 1024,
                },
                pricing: None,
                cache: ygg_ai::CacheCompatibility::default(),
            }),
            endpoint: Arc::new(Endpoint {
                id: EndpointId("test".into()),
                base_url: url::Url::parse(&format!("{uri}/v1/")).unwrap(),
                auth: Auth::None,
                default_headers: http::HeaderMap::new(),
                transport: ygg_ai::EndpointTransport::Http,
                timeout: Duration::from_secs(5),
            }),
        }
    }

    async fn scripted_agent_with_delay(
        response_delay: Duration,
    ) -> (wiremock::MockServer, tempfile::TempDir, ygg_agent::Agent) {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        use ygg_agent::{Agent, AgentConfig, CoreTools, ExtensionHost, SandboxConfig, Session};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_delay(response_delay)
                    .set_body_string(text_turn()),
            )
            .mount(&server)
            .await;

        let workspace = tempfile::tempdir().unwrap();
        let session_path = workspace.path().join("session.jsonl");
        let mut extensions = ExtensionHost::new();
        extensions.load(&CoreTools);
        let mut sandbox = SandboxConfig::new(workspace.path());
        sandbox.allow_edit = true;
        sandbox.allow_process = true;
        let agent = Agent::new(AgentConfig {
            client: ygg_ai::AiClient::new(),
            model: scripted_model(&server.uri()),
            session: Session::create(&session_path).unwrap(),
            system: "test".into(),
            sandbox,
            extensions,
            max_turns: Some(4),
            reasoning: ReasoningConfig::Off,
            cache_retention: ygg_ai::CacheRetention::default(),
            session_id: None,
        })
        .unwrap();
        (server, workspace, agent)
    }

    async fn scripted_agent() -> (wiremock::MockServer, tempfile::TempDir, ygg_agent::Agent) {
        scripted_agent_with_delay(Duration::ZERO).await
    }

    struct EndsThenPanics(bool);

    impl Stream for EndsThenPanics {
        type Item = std::io::Result<Event>;

        fn poll_next(
            mut self: Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            assert!(!self.0, "a closed input stream was polled more than once");
            self.0 = true;
            std::task::Poll::Ready(None)
        }
    }

    #[tokio::test]
    async fn closed_input_is_disabled_while_the_aborted_run_settles() {
        let (_server, _workspace, mut agent) = scripted_agent().await;
        let mut shell = InteractiveShell::test_shell();
        let run_id = shell.begin_run("test");
        let mut run = agent.prompt("initial").await.unwrap();
        shell.set_awaiting_provider(run_id);
        let control = run.control();
        let mut input = EndsThenPanics(false);
        let mut ticker = tokio::time::interval(Duration::from_millis(1));
        let mut pending = VecDeque::new();
        let mut quit = false;
        let mut executable_extensions = crate::extensions::ExecutableExtensions::default();

        let ended = drive_active_run(
            &mut run,
            &control,
            &mut shell,
            &mut input,
            &mut ticker,
            &mut pending,
            &mut quit,
            None,
            None,
            &mut executable_extensions,
        )
        .await
        .unwrap();
        drop(run);

        assert_eq!(ended, RunEnded::Aborted);
        assert!(quit);
        assert!(shell.debug_snapshot().contains("Interrupted"));
    }

    #[tokio::test]
    async fn scripted_active_loop_queues_controls_and_never_forwards_active_model_command() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use tokio_stream::wrappers::ReceiverStream;

        let (_server, workspace, mut agent) = scripted_agent().await;
        let image = workspace.path().join("shot.png");
        std::fs::write(&image, b"png").unwrap();

        let mut shell = InteractiveShell::test_shell();
        shell.set_input_modalities(ygg_ai::ModalitySet::none().with(ygg_ai::Modality::Image));
        for character in "steer first".chars() {
            shell.apply_edit(crate::tui::keymap::EditAction::Char(character));
        }
        shell.apply_edit(crate::tui::keymap::EditAction::Paste(
            image.display().to_string(),
        ));
        let (sender, receiver) = tokio::sync::mpsc::channel(32);
        sender
            .send(Ok(Event::Key(KeyEvent::new(
                KeyCode::Char('s'),
                KeyModifiers::CONTROL,
            ))))
            .await
            .unwrap();
        for character in "/model gpt-4o-mini".chars() {
            sender
                .send(Ok(Event::Key(KeyEvent::new(
                    KeyCode::Char(character),
                    KeyModifiers::NONE,
                ))))
                .await
                .unwrap();
        }
        sender
            .send(Ok(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            ))))
            .await
            .unwrap();
        // Keep the sender alive so the receiver remains pending rather than
        // signalling an input close that would abort the real run.
        let _sender = sender;
        let mut input = ReceiverStream::new(receiver);
        let mut ticker = tokio::time::interval(Duration::from_millis(1));
        let mut pending = VecDeque::new();
        let mut quit = false;
        let mut executable_extensions = crate::extensions::ExecutableExtensions::default();
        let run_id = shell.begin_run("test");
        let mut run = agent.prompt("initial").await.unwrap();
        shell.set_awaiting_provider(run_id);
        let control = run.control();
        let ended = drive_active_run(
            &mut run,
            &control,
            &mut shell,
            &mut input,
            &mut ticker,
            &mut pending,
            &mut quit,
            None,
            None,
            &mut executable_extensions,
        )
        .await
        .unwrap();
        drop(run);

        assert_eq!(ended, RunEnded::Completed);
        assert!(!quit);
        assert_eq!(
            pending.pop_front(),
            Some(PendingIdleAction::ChangeModel(ModelId(
                "gpt-4o-mini".into()
            )))
        );
        let context = agent.session().context().unwrap();
        let user_text = context
            .iter()
            .filter_map(|message| match message {
                ygg_ai::Message::User(user) => user.content.iter().find_map(|part| match part {
                    ygg_ai::UserPart::Text(text) => Some(text.as_str()),
                    _ => None,
                }),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(user_text.contains(&"steer first"));
        assert!(!user_text.iter().any(|text| text.contains("/model")));
        assert!(context.iter().any(|message| matches!(
            message,
            ygg_ai::Message::User(user)
                if user
                    .content
                    .iter()
                    .any(|part| matches!(part, ygg_ai::UserPart::Media(_)))
        )));
    }

    #[tokio::test]
    async fn abort_restores_all_undelivered_steering_after_the_final_event() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use tokio_stream::wrappers::ReceiverStream;

        let (_server, _workspace, mut agent) =
            scripted_agent_with_delay(Duration::from_secs(2)).await;
        let mut shell = InteractiveShell::test_shell();
        for character in "steer first".chars() {
            shell.apply_edit(crate::tui::keymap::EditAction::Char(character));
        }
        let (sender, receiver) = tokio::sync::mpsc::channel(32);
        sender
            .send(Ok(Event::Key(KeyEvent::new(
                KeyCode::Char('s'),
                KeyModifiers::CONTROL,
            ))))
            .await
            .unwrap();
        for character in "steer second".chars() {
            sender
                .send(Ok(Event::Key(KeyEvent::new(
                    KeyCode::Char(character),
                    KeyModifiers::NONE,
                ))))
                .await
                .unwrap();
        }
        sender
            .send(Ok(Event::Key(KeyEvent::new(
                KeyCode::Char('s'),
                KeyModifiers::CONTROL,
            ))))
            .await
            .unwrap();
        sender
            .send(Ok(Event::Key(KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            ))))
            .await
            .unwrap();
        let _sender = sender;

        let mut input = ReceiverStream::new(receiver);
        let mut ticker = tokio::time::interval(Duration::from_millis(1));
        let mut pending = VecDeque::new();
        let mut quit = false;
        let mut executable_extensions = crate::extensions::ExecutableExtensions::default();
        let run_id = shell.begin_run("test");
        let mut run = agent.prompt("initial").await.unwrap();
        shell.set_awaiting_provider(run_id);
        let control = run.control();
        let ended = drive_active_run(
            &mut run,
            &control,
            &mut shell,
            &mut input,
            &mut ticker,
            &mut pending,
            &mut quit,
            None,
            None,
            &mut executable_extensions,
        )
        .await
        .unwrap();
        drop(run);

        assert_eq!(ended, RunEnded::Aborted);
        assert_eq!(shell.pending(), "steer first\n\nsteer second");
        assert!(shell.debug_snapshot().contains("Interrupted"));
        let context = agent.session().context().unwrap();
        assert!(!context.iter().any(|message| matches!(
            message,
            ygg_ai::Message::User(user)
                if user.content.iter().any(|part| matches!(
                    part,
                    ygg_ai::UserPart::Text(text) if text.starts_with("steer ")
                ))
        )));
    }
}
