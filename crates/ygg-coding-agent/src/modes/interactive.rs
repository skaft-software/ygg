#![allow(missing_docs)]

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::{Stream, StreamExt};
use tokio::time::{Instant, Interval, MissedTickBehavior};
#[cfg(unix)]
use ygg_agent::extension_process::ProcessGroupGuard;
use ygg_agent::{
    analyze_session_cache_stats, AgentCompactionMode, AgentError, AgentEvent, EntryId,
    GoalDecision, GoalStatus, GoalTurnSource, Run, RunControl, Session,
};
use ygg_ai::{ModelId, ReasoningConfig, ReasoningMode, ToolCallId};

use crate::app::bootstrap::{
    build_app, estimate_text_tokens, rebuild_app, resolve_launch_interactive, Bootstrap,
};
use crate::app::{
    apply_reconfig, level_from_reasoning, reasoning_label, supported_levels_with_subagents,
    thinking_to_reasoning_with_subagents, App, Reconfig,
};
use crate::commands::{self, Command};
use crate::compaction::{
    attempt_compaction, context_window, estimate_next_request_tokens, CompactionOutcome,
};
use crate::config::{CompactionMode, ThinkingLevel};
use crate::modes::{HostRunOutcome, RUN_STREAM_LOST_MESSAGE};
use crate::presentation::RunId;
use crate::prompts::{render_and_record, RenderedPrompt};
use crate::resources::{compose_instructions, expand_skill_command};
use crate::session_tree::render_session_tree;
use crate::tui::composer::ComposedInput;
use crate::tui::keymap::{self, InputAction};
use crate::tui::pickers::{
    confirmation_picker, extension_confirmation_picker, extension_input_picker, extension_picker,
    message_picker, optional_model_picker, read_only_document, read_only_document_live_styled,
    session_picker, subagent_picker, thinking_picker, tool_input_picker, SubagentPickerSnapshot,
};
use crate::tui::theme::YggTheme;
use crate::tui::theme::{
    background_from_terminal_rgb, load_theme, load_theme_for_background, TerminalBackground,
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
                    Some(Ok(Event::Key(key))) if keymap::is_close_key(&key) => {
                        self.shell.request_close();
                        return Ok(());
                    }
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

    fn input<'a>(
        &'a mut self,
        _extension: &'a str,
        request: &'a ygg_agent::extension_process::ExtensionInputRequest,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Option<String>>> + 'a>> {
        Box::pin(async move {
            tokio::select! {
                biased;
                _ = crate::tui::terminal::wait_for_shutdown_signal() => {
                    anyhow::bail!("shutdown requested while awaiting extension input")
                }
                result = extension_input_picker(self.shell, self.input, request) => result,
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
    ChangeThinking(ReasoningConfig),
    ChangeThinkingLevel(ThinkingLevel),
    CycleThinking,
    PickModel,
    PickThinking,
    NewSession,
    ResumeSession(Option<String>),
    Fork,
    Clone,
    Compact,
    AutoCompact(Option<commands::AutoCompactSetting>),
    ShowContext,
    ReloadResources,
    ShowTree,
    CheckoutEntry(String),
    Skills(commands::SkillsSubcommand),
    Goal(commands::GoalCommand),
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
    GoalContinuation,
    CycleThinking,
    Quit,
}

async fn wait_for_prompt<S>(
    shell: &mut InteractiveShell,
    input: &mut S,
    scroll_tick: &mut Interval,
    extension_tick: &mut Interval,
    executable_extensions: &mut crate::extensions::ExecutableExtensions,
    goal_deadline: Option<Instant>,
) -> anyhow::Result<Idle>
where
    S: Stream<Item = std::io::Result<Event>> + Unpin,
{
    let mut scroll_dirty = false;
    loop {
        if shell.close_requested() {
            return Ok(Idle::Quit);
        }
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
                if matches!(&event, Event::Key(key) if keymap::is_close_key(key)) {
                    shell.request_close();
                    return Ok(Idle::Quit);
                }
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
                    InputAction::CompletePath => {
                        shell.complete_path();
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
                    InputAction::ToggleDisclosure => {
                        shell.toggle_disclosure();
                        shell.render();
                    }
                    InputAction::CycleThinking => return Ok(Idle::CycleThinking),
                    InputAction::ClearEditor => {
                        shell.clear_editor();
                        shell.render();
                    }
                    InputAction::Close => {
                        shell.clear_error();
                        shell.render();
                    }
                    InputAction::Submit(_) => return Ok(Idle::Submit(shell.drain_composed())),
                    InputAction::Command(_) => return Ok(Idle::Command(shell.drain_editor())),
                    InputAction::Closed => return Ok(Idle::Quit),
                    InputAction::Ignore | InputAction::Abort | InputAction::Steer(_) => {}
                }
            }
            // Mouse/trackpad events arrive in bursts. Apply every delta to
            // state, but draw at most once per frame so a large transcript
            // cannot leave a backlog that appears as post-scroll inertia.
            _ = scroll_tick.tick(), if scroll_dirty => {
                shell.render();
                scroll_dirty = false;
            },
            _ = async {
                match goal_deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            } => return Ok(Idle::GoalContinuation),
            _ = extension_tick.tick() => {
                if apply_extension_background(shell, executable_extensions) {
                    shell.render();
                }
            }
        }
    }
}

fn goal_status_label(status: GoalStatus) -> &'static str {
    match status {
        GoalStatus::Active => "Active",
        GoalStatus::Paused => "Paused",
        GoalStatus::Complete => "Complete",
        GoalStatus::Blocked => "Blocked",
        GoalStatus::BudgetLimited => "Budget limited",
        _ => "Unknown",
    }
}

fn goal_status_text(app: &App) -> anyhow::Result<String> {
    let goal = app.goal_store.get(&app.goal_session_id)?;
    let Some(goal) = goal else {
        return Ok("No goal is configured for this session.".to_owned());
    };
    let remaining = goal
        .turn_budget
        .map(|budget| {
            format!(
                " · {} turn{} remaining",
                budget.saturating_sub(goal.turns_used),
                if budget.saturating_sub(goal.turns_used) == 1 {
                    ""
                } else {
                    "s"
                }
            )
        })
        .unwrap_or_default();
    Ok(format!(
        "{} goal: {}{}",
        goal_status_label(goal.status),
        goal.objective,
        remaining
    ))
}

fn arm_goal_deadline(app: &App) -> anyhow::Result<Option<Instant>> {
    match app
        .goal_driver
        .turn_settled(GoalTurnSource::User, "", false)?
    {
        GoalDecision::Wait { delay, .. } => Ok(Some(Instant::now() + delay)),
        _ => Ok(None),
    }
}

fn recovered_goal_deadline(app: &App) -> anyhow::Result<Option<Instant>> {
    if app
        .goal_store
        .get(&app.goal_session_id)?
        .is_some_and(|goal| goal.status == GoalStatus::Active)
    {
        arm_goal_deadline(app)
    } else {
        Ok(None)
    }
}

fn apply_goal_command(
    app: &App,
    shell: &mut InteractiveShell,
    command: commands::GoalCommand,
    goal_deadline: &mut Option<Instant>,
) -> anyhow::Result<()> {
    use ygg_agent::GoalAction as DurableGoalAction;

    match command {
        commands::GoalCommand::Help => shell.show_overlay_text(
            "Goal commands\n\n/goal <objective>\n/goal status\n/goal pause\n/goal resume\n/goal clear"
                .to_owned(),
        ),
        commands::GoalCommand::Status => match goal_status_text(app) {
            Ok(status) => shell.show_overlay_text(status),
            Err(error) => shell.error(format!("unable to read goal: {error}")),
        },
        commands::GoalCommand::Set(objective) => match app
            .goal_store
            .set(&app.goal_session_id, &objective, None)
        {
            Ok(goal) => {
                app.goal_driver.user_spoke();
                *goal_deadline = arm_goal_deadline(app)?;
                shell.notice(format!(
                    "goal set · {} goal: {}",
                    goal_status_label(goal.status), goal.objective
                ));
            }
            Err(error) => shell.error(format!("unable to set goal: {error}")),
        },
        commands::GoalCommand::Pause => {
            match app
                .goal_store
                .apply(&app.goal_session_id, DurableGoalAction::Pause)
            {
                Ok(Some(goal)) => {
                    *goal_deadline = None;
                    app.goal_driver.user_spoke();
                    shell.notice(format!("goal paused · {}", goal.objective));
                }
                Ok(None) => shell.error("no goal is configured for this session".to_owned()),
                Err(error) => shell.error(format!("unable to pause goal: {error}")),
            }
        }
        commands::GoalCommand::Resume => {
            match app
                .goal_store
                .apply(&app.goal_session_id, DurableGoalAction::Resume)
            {
                Ok(Some(goal)) => {
                    app.goal_driver.user_spoke();
                    *goal_deadline = arm_goal_deadline(app)?;
                    shell.notice(format!("goal resumed · {}", goal.objective));
                }
                Ok(None) => shell.error("no goal is configured for this session".to_owned()),
                Err(error) => shell.error(format!("unable to resume goal: {error}")),
            }
        }
        commands::GoalCommand::Clear => {
            match app
                .goal_store
                .apply(&app.goal_session_id, DurableGoalAction::Clear)
            {
                Ok(None) => {
                    *goal_deadline = None;
                    app.goal_driver.user_spoke();
                    shell.notice("goal cleared");
                }
                Ok(Some(_)) => unreachable!("clearing a goal returns no state"),
                Err(error) => shell.error(format!("unable to clear goal: {error}")),
            }
        }
    }
    Ok(())
}
fn settle_goal(
    app: &App,
    shell: &mut InteractiveShell,
    source: GoalTurnSource,
    response: &str,
    made_tool_call: bool,
    completed: bool,
) -> Option<GoalDecision> {
    if !completed {
        let _ = app.goal_driver.session_error();
        return None;
    }
    match app
        .goal_driver
        .turn_settled(source, response, made_tool_call)
    {
        Ok(decision) => Some(decision),
        Err(error) => {
            let _ = app.goal_driver.session_error();
            shell.error(format!("unable to update goal state: {error}"));
            None
        }
    }
}

fn queue_command(command: Command, queue: &mut VecDeque<PendingIdleAction>) -> anyhow::Result<()> {
    let action = match command {
        Command::Login(provider) => PendingIdleAction::Login(provider),
        Command::Logout(provider) => PendingIdleAction::Logout(provider),
        Command::Model(Some(id)) => PendingIdleAction::ChangeModel(ModelId(id)),
        Command::Model(None) => PendingIdleAction::PickModel,
        Command::Thinking(Some(level)) => match ThinkingLevel::parse(&level)? {
            ThinkingLevel::Off => PendingIdleAction::ChangeThinking(ReasoningConfig::Off),
            level => PendingIdleAction::ChangeThinkingLevel(level),
        },
        Command::Thinking(None) => PendingIdleAction::PickThinking,
        Command::New => PendingIdleAction::NewSession,
        Command::Resume(id) => PendingIdleAction::ResumeSession(id),
        Command::Fork => PendingIdleAction::Fork,
        Command::Clone => PendingIdleAction::Clone,
        Command::Compact => PendingIdleAction::Compact,
        Command::AutoCompact(setting) => PendingIdleAction::AutoCompact(setting),
        Command::Context => PendingIdleAction::ShowContext,
        Command::Reload => PendingIdleAction::ReloadResources,
        Command::Tree => PendingIdleAction::ShowTree,
        Command::Checkout(id) => PendingIdleAction::CheckoutEntry(id),
        Command::Skills(sub) => PendingIdleAction::Skills(sub),
        Command::Goal(goal) => PendingIdleAction::Goal(goal),
        other => anyhow::bail!("{other:?} cannot be queued as an idle action"),
    };
    push_pending_action(queue, action);
    Ok(())
}

async fn await_with_ctrl_c<F, S>(
    future: F,
    shell: &mut InteractiveShell,
    input: &mut S,
) -> Option<F::Output>
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
                Some(Ok(Event::Key(key))) if keymap::is_close_key(&key) => {
                    shell.request_close();
                    return None;
                }
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
/// typing is intentionally ignored at this boundary. Ctrl-C becomes the same
/// coordinated SIGINT shutdown used by the signal thread; Ctrl-D records a
/// close request and lets the owned operation settle before its caller exits.
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
                Some(Ok(Event::Key(key))) if keymap::is_close_key(&key) => {
                    shell.request_close();
                    shell.set_run_label("closing…");
                    shell.render();
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

/// Run device-code login outside raw/alternate-screen mode and return the
/// refreshed catalog. The caller decides whether it can install the catalog
/// into a live Agent or must ask the user to restart from a model-less shell.
async fn login_codex_catalog(
    shell: &mut InteractiveShell,
) -> anyhow::Result<Option<ygg_ai::ModelCatalog>> {
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
        return Ok(None);
    }

    let catalog = match crate::app::bootstrap::model_catalog() {
        Ok(catalog) => catalog,
        Err(error) => {
            shell.error(format!(
                "ChatGPT login succeeded, but reloading models failed: {error:#}"
            ));
            shell.render();
            return Ok(None);
        }
    };
    if !catalog
        .models()
        .any(|model| model.endpoint.0 == crate::auth::codex::ENDPOINT_ID)
    {
        shell.error("ChatGPT login completed, but no Codex models could be registered".into());
        shell.render();
        return Ok(None);
    }
    Ok(Some(catalog))
}

/// Run device-code login outside raw/alternate-screen mode, then make the new
/// models available immediately without restarting the current Agent.
async fn login_codex(app: &mut App, shell: &mut InteractiveShell) -> anyhow::Result<()> {
    if let Some(catalog) = login_codex_catalog(shell).await? {
        app.catalog = catalog;
        shell.clear_error();
        shell.notice("signed in to ChatGPT; use /model to select a Codex model");
        shell.render();
    }
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
    if let Err(error) = store.delete_async().await {
        shell.error(format!("ChatGPT logout failed: {error:#}"));
        return Ok(app);
    }
    app.catalog = catalog;
    shell.clear_error();
    shell.notice("signed out of ChatGPT");
    shell.render();
    Ok(app)
}

/// Save a default custom provider registry and reload the catalog.
fn login_custom(shell: &mut InteractiveShell) -> anyhow::Result<()> {
    use crate::auth::custom::{
        self, CustomAuthConfig, CustomCredential, CustomProvider, CustomRegistry,
    };
    let store = custom::CredentialStore::new(custom::default_path());
    let path = custom::default_path();

    if store.load_registry()?.is_some() {
        shell.notice(format!(
            "custom provider registry already configured at {}; use /logout custom first to replace it",
            path.display()
        ));
        return Ok(());
    }

    let provider = CustomProvider {
        label: "Local endpoint".into(),
        credential: CustomCredential {
            base_url: "http://localhost:1234/v1/".into(),
            api_key: String::new(),
            api_name: "local-model".into(),
            headers: Vec::new(),
            models: Vec::new(),
            auto_discover: true,
        },
        auth: Some(CustomAuthConfig::None),
        api_key_env: None,
        cache: None,
        startup_timeout_secs: None,
    };
    store.save_registry(&CustomRegistry::single("local", provider))?;
    shell.notice(format!(
        "custom provider registry template saved to {}\n\
         edit it with your provider details, then /reload to register the models",
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
    if store.load_registry()?.is_none() {
        shell.notice("no custom provider registry configured");
        return Ok(app);
    }

    // Pick a replacement model if the active model belongs to any custom
    // provider in the unified registry.
    let needs_replacement = custom::is_endpoint_id(&app.model.endpoint.id.0);
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
        Command::Help(_) => shell.notice("help is available at the next idle boundary"),
        Command::Name(_) | Command::Export(_) => {
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

#[allow(clippy::too_many_arguments)]
fn request_active_close(
    control: &RunControl,
    shell: &mut InteractiveShell,
    run_id: RunId,
    input_open: &mut bool,
    aborting: &mut bool,
    intents: &mut VecDeque<ControlIntent>,
    in_flight: &mut Option<ControlFuture>,
    quit_requested: &mut bool,
) {
    shell.request_close();
    *input_open = false;
    control.abort();
    *aborting = true;
    intents.clear();
    *in_flight = None;
    *quit_requested = true;
    shell.set_run_preparing(run_id, "cancelling");
    shell.render();
}

fn confirmation_action(tool_name: Option<&str>) -> &str {
    match tool_name {
        Some(name) if matches!(name, "bash" | "edit" | "write") => name,
        Some(_) => "extension",
        None => "tool",
    }
}

fn confirmation_notice(tool_name: Option<&str>, confirmed: bool) -> String {
    format!(
        "{} action {}",
        confirmation_action(tool_name),
        if confirmed { "approved" } else { "denied" }
    )
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
    made_tool_call: &mut bool,
) -> anyhow::Result<HostRunOutcome>
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
    let mut tool_calls =
        std::collections::HashMap::<ToolCallId, (String, serde_json::Value)>::new();

    loop {
        if shell.close_requested() && !*quit_requested {
            request_active_close(
                control,
                shell,
                run_id,
                &mut input_open,
                &mut aborting,
                &mut intents,
                &mut in_flight,
                quit_requested,
            );
        }
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
                ygg_agent::extension_process::terminate_bash_process_groups(
                    Duration::from_millis(400),
                )
                .await;
                return Ok(HostRunOutcome::shutdown());
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
                if matches!(&event, Event::Key(key) if keymap::is_close_key(key)) {
                    request_active_close(
                        control,
                        shell,
                        run_id,
                        &mut input_open,
                        &mut aborting,
                        &mut intents,
                        &mut in_flight,
                        quit_requested,
                    );
                    continue;
                }
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
                    InputAction::CompletePath => {
                        shell.complete_path();
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
                    InputAction::ClearEditor => {
                        shell.clear_editor();
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
                        // The live subagents view only reads extension
                        // presentation state, so it is safe to open while the
                        // run keeps going. Run events buffer until the panel
                        // closes and are applied immediately afterwards.
                        if matches!(&command, Command::Unknown(text)
                            if is_live_subagents_command(text, executable_extensions))
                        {
                            match active_subagents_view(
                                shell,
                                input,
                                executable_extensions,
                                |principal: &str, reference: &str| {
                                    run.open_delegated_session_reference(principal, reference)
                                },
                            )
                            .await
                            {
                                Ok(()) => {
                                    shell.render();
                                    continue;
                                }
                                Err(error) => {
                                    shell.error(format!("extension command failed: {error}"));
                                    shell.render();
                                    continue;
                                }
                            }
                        }
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
                    InputAction::ToggleDisclosure => {
                        shell.toggle_disclosure();
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
                        request_active_close(
                            control,
                            shell,
                            run_id,
                            &mut input_open,
                            &mut aborting,
                            &mut intents,
                            &mut in_flight,
                            quit_requested,
                        );
                    }
                    InputAction::Ignore | InputAction::Submit(_) => {}
                }
            }
            event = run.next() => match event {
                Some(event) => {
                    if let AgentEvent::ToolStarted { id, name, args } = &event {
                        *made_tool_call = true;
                        tool_calls.insert(id.clone(), (name.clone(), args.clone()));
                    }
                    if let AgentEvent::ToolProgress {
                        id,
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
                                ygg_agent::extension_process::terminate_bash_process_groups(
                                    Duration::from_millis(400),
                                )
                                .await;
                                return Ok(HostRunOutcome::shutdown());
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
                        if shell.close_requested() {
                            request_active_close(
                                control,
                                shell,
                                run_id,
                                &mut input_open,
                                &mut aborting,
                                &mut intents,
                                &mut in_flight,
                                quit_requested,
                            );
                        }
                        let tool_name = tool_calls.get(id).map(|(name, _)| name.as_str());
                        let notice = confirmation_notice(tool_name, confirmed);
                        if confirmed {
                            shell.notice_success(notice);
                        } else {
                            shell.notice_error(notice);
                        }
                    }
                    if let AgentEvent::ToolProgress {
                        progress: ygg_agent::ToolProgress::Input(request),
                        ..
                    } = &event
                    {
                        let answered = tool_input_picker(shell, input, request).await?;
                        if shell.close_requested() {
                            request_active_close(
                                control,
                                shell,
                                run_id,
                                &mut input_open,
                                &mut aborting,
                                &mut intents,
                                &mut in_flight,
                                quit_requested,
                            );
                        }
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
                    if let AgentEvent::ToolFinished { id, result, .. } = &event {
                        if let Some((name, arguments)) = tool_calls.remove(id) {
                            let (output, is_error) = match result {
                                Ok(output) => (Some(output.text.clone()), output.is_error()),
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
                        let (endpoint, model) = shell
                            .current_run_route()
                            .unwrap_or_else(|| ("unknown".to_owned(), "unknown".to_owned()));
                        return Ok(HostRunOutcome::from_finish_reason(
                            &reason,
                            &endpoint,
                            &model,
                        ));
                    }
                }
                None => {
                    shell.restore_queued_steering();
                    shell.fail_run(run_id, RUN_STREAM_LOST_MESSAGE);
                    shell.render();
                    return Ok(HostRunOutcome::stream_lost());
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
    let endpoint_label = app
        .catalog
        .endpoint_label(&app.model.endpoint.id)
        .unwrap_or(&app.model.endpoint.id.0);
    shell.set_identity(
        endpoint_label,
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
    shell.set_skill_commands(Arc::from(
        app.skills
            .descriptors()
            .iter()
            .map(|skill| (format!("skill:{}", skill.id), skill.description.clone()))
            .collect::<Vec<_>>(),
    ));
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
    for update in updates.rendered_tools {
        shell.apply_extension_tool_renderer(&update.id, &update.segments);
        changed = true;
    }
    for message in executable_extensions.drain_events() {
        shell.notice(message);
        changed = true;
    }
    // Extension contributions can arrive (or change) after the initial
    // handshake; keep the composer's slash-command list in step so commands
    // like /subagents are enterable as soon as their owning process is ready.
    shell.set_extension_commands(Arc::from(executable_extensions.command_suggestions()));
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
        CompactionOutcome::NativeCompacted => {
            let usage = session
                .usage_records()
                .iter()
                .rev()
                .find(|record| matches!(record.kind, ygg_agent::UsageRecordKind::Compaction));
            let detail = usage.map_or_else(
                || "opaque Responses state retained".to_owned(),
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
                    format!("{prompt_tokens} input tokens compacted · {cost} compaction cost")
                },
            );
            shell.native_compaction_marker(format!("Context compacted natively · {detail}"));
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
    let mut candidate_mode = app.config.compaction.mode;
    let mut candidate_threshold = app.config.compaction.threshold_fraction;
    match setting {
        Some(commands::AutoCompactSetting::Mode(mode)) => {
            if mode == CompactionMode::NativeResponses
                && app.model.spec.protocol != ygg_ai::Protocol::OpenAiResponses
            {
                shell.error(format!(
                    "native Responses compaction is unavailable for {:?}; select an OpenAI Responses model or use local compaction",
                    app.model.spec.protocol
                ));
                return Ok(());
            }
            if mode == CompactionMode::NativeResponses
                && app
                    .config
                    .compaction
                    .compact_model
                    .as_ref()
                    .is_some_and(|model| model != &app.model.spec.id)
            {
                shell.error(
                    "native Responses compaction requires compaction.compact_model to match the active model"
                        .to_owned(),
                );
                return Ok(());
            }
            candidate_mode = mode;
        }
        Some(commands::AutoCompactSetting::ThresholdPercent(percent)) => {
            candidate_threshold = f64::from(percent) / 100.0;
        }
        None => {}
    }
    let agent_mode = match candidate_mode {
        CompactionMode::Disabled => AgentCompactionMode::Disabled,
        CompactionMode::Local => AgentCompactionMode::Local,
        CompactionMode::NativeResponses => AgentCompactionMode::NativeResponses,
    };
    if let Err(error) = app.agent.set_compaction_token_mode(
        agent_mode,
        candidate_threshold,
        app.config.compaction.keep_recent_tokens,
    ) {
        shell.error(format!("auto-compaction was not changed: {error}"));
        return Ok(());
    }
    // Publish the candidate only after the Agent accepts it. In particular, a
    // legacy Responses session cannot leave configuration claiming `native`
    // while the Agent safely remains in its previous mode.
    app.config.compaction.mode = candidate_mode;
    app.config.compaction.threshold_fraction = candidate_threshold;
    shell.notice(format!(
        "auto-compaction {} at {:.0}% · keep ~{} recent tokens · this process",
        candidate_mode.label(),
        candidate_threshold * 100.0,
        app.config.compaction.keep_recent_tokens,
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
        let app = rebuild_app(app, None, None, None, None)?;
        let theme = load_theme_for_background(&app.config, background);
        Ok((app, theme))
    })
    .await?;
    shell.set_theme(theme);
    shell.set_runtime_config(app.config.clone());
    shell.hydrate(app.agent.session())?;
    update_status(shell, &app);
    Ok(app)
}

#[derive(Clone, Debug)]
struct InstalledExtensionChoice {
    name: String,
    label: String,
    description: String,
    enabled: bool,
    toggleable: bool,
}

fn installed_extension_choices(app: &App) -> anyhow::Result<Vec<InstalledExtensionChoice>> {
    let root = crate::extension_package::extensions_root()?;
    let installed = crate::extension_bundle::list_installed(&root)?;
    let summaries = app.executable_extensions.summaries();
    let explicitly_required = app
        .config
        .tools
        .explicit_names()
        .into_iter()
        .flatten()
        .collect::<std::collections::BTreeSet<_>>();
    let current_tools = app
        .agent
        .registered_tool_names()
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    Ok(installed
        .into_iter()
        .map(|bundle| {
            let summary = summaries.iter().find(|summary| summary.name == bundle.id);
            let enabled = app
                .config
                .enabled_extensions
                .iter()
                .any(|name| name == &bundle.id);
            let global_source = summary.is_some_and(|summary| {
                matches!(
                    summary.source,
                    ygg_agent::extension_process::ExtensionSource::Global
                )
            });
            let unavailable_disable = summary.is_none() && enabled;
            let one_shot_trust_enable = !enabled
                && app
                    .config
                    .invocation_trusted_extensions
                    .iter()
                    .any(|name| name == &bundle.id);
            let alternate_source_trust_enable = !enabled
                && app.config.trusted_extensions.iter().any(|grant| {
                    grant
                        .split_once('@')
                        .is_some_and(|(name, _)| name == bundle.id.as_str())
                });
            let required_tools = summary
                .map(|summary| {
                    summary
                        .tools
                        .iter()
                        .filter(|tool| explicitly_required.contains(tool.as_str()))
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let required_by_explicit_tools = enabled && !required_tools.is_empty();
            let colliding_tools = if enabled {
                Vec::new()
            } else {
                summary
                    .map(|summary| {
                        summary
                            .tools
                            .iter()
                            .filter(|tool| current_tools.contains(tool.as_str()))
                            .cloned()
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            };
            let activation_authoritative = !app.config.extension_activation_overridden;
            let toggleable = (global_source || unavailable_disable)
                && activation_authoritative
                && !one_shot_trust_enable
                && !alternate_source_trust_enable
                && !required_by_explicit_tools
                && colliding_tools.is_empty();
            let marker = if enabled { "[x]" } else { "[ ]" };
            let description = match summary {
                Some(_) if !activation_authoritative => format!(
                    "installed {} · activation controlled by project, environment, or CLI; menu is read-only",
                    bundle.version
                ),
                Some(_) if one_shot_trust_enable => format!(
                    "installed {} · one-shot name trust can change source during rebuild; enable on a clean next launch",
                    bundle.version
                ),
                Some(_) if alternate_source_trust_enable => format!(
                    "installed {} · another manifest source has an exact trust grant; enable on a clean next launch",
                    bundle.version
                ),
                Some(_) if required_by_explicit_tools => format!(
                    "installed {} · required by explicit tool(s) {}; disable after removing that allowlist",
                    bundle.version,
                    required_tools.join(", ")
                ),
                Some(_) if !colliding_tools.is_empty() => format!(
                    "installed {} · tool name collision with {}; enable after resolving the active provider",
                    bundle.version,
                    colliding_tools.join(", ")
                ),
                Some(summary) if toggleable => format!(
                    "{} · {} · {} · API {}",
                    if summary.running {
                        "running"
                    } else {
                        "stopped"
                    },
                    if summary.trusted {
                        "trusted"
                    } else {
                        "untrusted"
                    },
                    bundle.version,
                    summary.api_version,
                ),
                Some(summary) => format!(
                    "installed {} · shadowed by {:?} source; toggle unavailable",
                    bundle.version, summary.source
                ),
                None if unavailable_disable && activation_authoritative => format!(
                    "installed {} · enabled but unavailable in discovery; Enter disables safely",
                    bundle.version
                ),
                None if unavailable_disable => format!(
                    "installed {} · enabled but unavailable; activation override makes the menu read-only",
                    bundle.version
                ),
                None => format!(
                    "installed {} · unavailable in current discovery; cannot enable (see /extensions status)",
                    bundle.version
                ),
            };
            InstalledExtensionChoice {
                name: bundle.id.clone(),
                label: format!("{marker} {}", bundle.id),
                description,
                enabled,
                toggleable,
            }
        })
        .collect())
}

const WEB_SEARCH_EXTENSION_NAME: &str = "ygg-web-search";
const WEB_SEARCH_COMMAND_NAME: &str = "web-search";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WebSearchMenuAction {
    Configured,
    Disable,
    Back,
}

fn web_search_menu_entries(allow_disable: bool) -> (Vec<String>, Vec<Option<String>>) {
    let mut items = vec![
        "Brave Search (recommended)".to_owned(),
        "SearXNG".to_owned(),
    ];
    let mut descriptions = vec![
        Some(
            "Hosted Brave Search API · setup asks for an API key and provides the signup link"
                .to_owned(),
        ),
        Some("Use an existing self-hosted or public SearXNG JSON endpoint".to_owned()),
    ];
    if allow_disable {
        items.push("Disable ygg-web-search".to_owned());
        descriptions.push(Some("Stop the extension and remove its tools".to_owned()));
    }
    (items, descriptions)
}

async fn web_search_management_menu(
    app: &mut App,
    shell: &mut InteractiveShell,
    input: &mut EventStream,
    allow_disable: bool,
) -> anyhow::Result<WebSearchMenuAction> {
    let (items, descriptions) = web_search_menu_entries(allow_disable);
    let Some(index) = extension_picker(
        shell,
        input,
        "Web search provider · Enter selects · Esc returns",
        items,
        descriptions,
        0,
    )
    .await?
    else {
        return Ok(WebSearchMenuAction::Back);
    };
    if allow_disable && index == 2 {
        return Ok(WebSearchMenuAction::Disable);
    }
    let provider = if index == 0 { "brave" } else { "searxng" };
    let output = {
        let mut interaction = InteractiveExtensionConfirmations { shell, input };
        app.executable_extensions
            .execute_command_with_confirmation(
                WEB_SEARCH_COMMAND_NAME,
                vec!["setup".to_owned(), provider.to_owned()],
                &mut interaction,
            )
            .await
    };
    match output {
        Ok(Some(output)) => {
            if !output.trim().is_empty() {
                shell.notice(output);
            }
            shell.clear_error();
            request_extension_ui(shell, app);
        }
        Ok(None) => shell.error(
            "ygg-web-search is running but its setup command is unavailable; see /extensions status"
                .to_owned(),
        ),
        Err(error) => shell.error(format!("web search provider was not changed: {error}")),
    }
    Ok(WebSearchMenuAction::Configured)
}

async fn extension_management_menu(
    mut app: App,
    shell: &mut InteractiveShell,
    input: &mut EventStream,
) -> anyhow::Result<App> {
    let mut selected = 0usize;
    loop {
        let choices = match installed_extension_choices(&app) {
            Ok(choices) => choices,
            Err(error) => {
                shell.error(format!(
                    "extension menu could not inspect installed bundles: {error}"
                ));
                return Ok(app);
            }
        };
        if choices.is_empty() {
            read_only_document(
                shell,
                input,
                "Extensions",
                "No executable extension bundles are installed.\n\nInstall one with `ygg extension install <name>`.".into(),
            )
            .await?;
            return Ok(app);
        }
        let items = choices.iter().map(|choice| choice.label.clone()).collect();
        let descriptions = choices
            .iter()
            .map(|choice| Some(choice.description.clone()))
            .collect();
        let Some(index) = extension_picker(
            shell,
            input,
            "Extensions · Enter manages/toggles · Esc closes",
            items,
            descriptions,
            selected,
        )
        .await?
        else {
            return Ok(app);
        };
        selected = index;
        let choice = &choices[index];
        if choice.enabled
            && choice.name == WEB_SEARCH_EXTENSION_NAME
            && app
                .executable_extensions
                .command_owner(WEB_SEARCH_COMMAND_NAME)
                .as_deref()
                == Some(WEB_SEARCH_EXTENSION_NAME)
        {
            match web_search_management_menu(&mut app, shell, input, choice.toggleable).await? {
                WebSearchMenuAction::Disable => {}
                WebSearchMenuAction::Configured | WebSearchMenuAction::Back => continue,
            }
        }
        if !choice.toggleable {
            shell.error(format!(
                "{} cannot be toggled from this menu: {}",
                choice.name, choice.description
            ));
            continue;
        }

        let authoritative = match crate::cli::extension_activation_menu_authoritative(&app.config) {
            Ok(authoritative) => authoritative,
            Err(error) => {
                shell.error(format!(
                    "{} was not changed: could not revalidate activation precedence: {error}",
                    choice.name
                ));
                continue;
            }
        };
        if !authoritative {
            shell.error(format!(
                "{} was not changed: project, environment, or CLI activation now makes the user config read-only",
                choice.name
            ));
            continue;
        }

        let enabled = !choice.enabled;
        let persisted = match crate::cli::persist_extension_enabled(&choice.name, enabled) {
            Ok(persisted) => persisted,
            Err(error) => {
                shell.error(format!(
                    "{} was not changed: could not update user configuration: {error}",
                    choice.name
                ));
                continue;
            }
        };
        app.config.enabled_extensions = persisted;
        app = match reload_resources(app, shell, input).await {
            Ok(app) => app,
            Err(error) => {
                let rollback = crate::cli::persist_extension_enabled(&choice.name, choice.enabled);
                return match rollback {
                    Ok(_) => Err(error.context(format!(
                        "{} runtime rebuild failed; the user-config activation change was rolled back",
                        choice.name
                    ))),
                    Err(rollback_error) => Err(error.context(format!(
                        "{} runtime rebuild failed and user-config rollback also failed: {rollback_error}",
                        choice.name
                    ))),
                };
            }
        };
        request_extension_ui(shell, &mut app);
        let summary = app
            .executable_extensions
            .summaries()
            .into_iter()
            .find(|summary| summary.name == choice.name);
        let detail = if enabled && summary.as_ref().is_some_and(|summary| !summary.trusted) {
            "; trust remains a separate explicit decision"
        } else {
            ""
        };
        shell.notice(format!(
            "{} {}{detail}",
            choice.name,
            if enabled { "enabled" } else { "disabled" }
        ));
        shell.clear_error();
        if enabled
            && choice.name == WEB_SEARCH_EXTENSION_NAME
            && summary
                .as_ref()
                .is_some_and(|summary| summary.running && summary.trusted)
            && app
                .executable_extensions
                .command_owner(WEB_SEARCH_COMMAND_NAME)
                .as_deref()
                == Some(WEB_SEARCH_EXTENSION_NAME)
        {
            let _ = web_search_management_menu(&mut app, shell, input, false).await?;
        }
    }
}

fn next_thinking_level(app: &App) -> anyhow::Result<ThinkingLevel> {
    let levels = supported_levels_with_subagents(&app.model, app.subagents_available());
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

async fn thinking_configuration_picker(
    app: &App,
    shell: &mut InteractiveShell,
    input: &mut EventStream,
) -> anyhow::Result<Option<(ReasoningMode, ThinkingLevel)>> {
    let levels = supported_levels_with_subagents(&app.model, app.subagents_available());
    let Some(level) = thinking_picker(shell, input, &levels).await? else {
        return Ok(None);
    };
    Ok(Some((ReasoningMode::Standard, level)))
}

fn delegated_session_text(
    session: &Session,
    theme: &YggTheme,
    width: u16,
) -> anyhow::Result<String> {
    use crate::tui::view::{
        assistant_markdown_document_lines, sanitize_for_terminal, user_prompt_document_lines,
    };
    use ygg_ai::{AssistantMessage, Message, UserPart};

    const MAX_MESSAGES: usize = 64;
    const MAX_BLOCK_BYTES: usize = 16 * 1024;
    const MAX_TOTAL_BYTES: usize = 128 * 1024;

    fn bounded(value: &str, limit: usize) -> String {
        let mut end = value.len().min(limit);
        while end > 0 && !value.is_char_boundary(end) {
            end -= 1;
        }
        value[..end].to_owned()
    }

    // One sanitized line of a compact argument preview for a tool call row.
    fn tool_argument_preview(call: &ygg_ai::ToolCall) -> String {
        let raw = sanitize_for_terminal(&call.arguments_json);
        let flat = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        bounded(&flat, 96)
    }

    let rich_renderer = theme.rich_renderer();
    let dot = if theme.unicode() { "•" } else { "*" };
    let context = session.context()?;

    // Styled header chrome; the panel title carries the worker label.
    let header = format!(
        "{} {}\n{}",
        theme.settled_event_dot("neutral", dot),
        theme.bold(&theme.fg("foreground", "Delegated worker transcript")),
        theme.dim("read-only · mutation remains owner-bound to agent_sessions"),
    );
    let omitted_marker = format!("\n\n{}", theme.dim("[older transcript entries omitted]"));
    let recent_start = context.len().saturating_sub(MAX_MESSAGES);
    let mut blocks = Vec::new();
    for message in context.iter().skip(recent_start) {
        let mut rows: Vec<String> = Vec::new();
        let mut block_bytes = 0usize;
        let push_row = |rows: &mut Vec<String>, block_bytes: &mut usize, row: String| {
            if *block_bytes >= MAX_BLOCK_BYTES {
                return;
            }
            *block_bytes += row.len() + 1;
            rows.push(row);
        };
        match message {
            Message::User(user) => {
                let texts = user.content.iter().filter_map(|part| match part {
                    UserPart::Text(text) => Some(text.as_str()),
                    _ => None,
                });
                for (index, text) in texts.enumerate() {
                    if block_bytes >= MAX_BLOCK_BYTES {
                        break;
                    }
                    if index == 0 {
                        let label = theme.bold(&theme.fg("foreground", "Parent request"));
                        push_row(
                            &mut rows,
                            &mut block_bytes,
                            format!(
                                "\n{} {} {}",
                                theme.settled_event_dot("success", dot),
                                label,
                                theme.dim("· from parent")
                            ),
                        );
                    } else {
                        push_row(&mut rows, &mut block_bytes, String::new());
                    }
                    // Rendered identically to the main transcript's user block.
                    for line in user_prompt_document_lines(text, &rich_renderer, theme, width) {
                        if block_bytes >= MAX_BLOCK_BYTES {
                            break;
                        }
                        push_row(&mut rows, &mut block_bytes, line);
                    }
                }
            }
            Message::Assistant(AssistantMessage { content, .. }) => {
                for part in content {
                    if block_bytes >= MAX_BLOCK_BYTES {
                        break;
                    }
                    match part {
                        ygg_ai::AssistantPart::Text(text) => {
                            if !rows.is_empty() {
                                push_row(&mut rows, &mut block_bytes, String::new());
                            }
                            // Rendered identically to the main transcript's
                            // settled assistant markdown block.
                            for line in assistant_markdown_document_lines(
                                text,
                                &rich_renderer,
                                theme,
                                width,
                            ) {
                                if block_bytes >= MAX_BLOCK_BYTES {
                                    break;
                                }
                                push_row(&mut rows, &mut block_bytes, line);
                            }
                        }
                        ygg_ai::AssistantPart::ToolCall(call) => {
                            let label = crate::tui::view::tool_display_label(&call.name);
                            let label = theme.bold(&theme.fg("foreground", &label));
                            let preview = tool_argument_preview(call);
                            let detail = if preview.is_empty() {
                                String::new()
                            } else {
                                theme.dim(&format!(" {preview}"))
                            };
                            push_row(
                                &mut rows,
                                &mut block_bytes,
                                format!(
                                    "\n{} {}{}",
                                    theme.settled_event_dot("neutral", dot),
                                    label,
                                    detail
                                ),
                            );
                        }
                        ygg_ai::AssistantPart::Reasoning(_) | ygg_ai::AssistantPart::Media(_) => {}
                    }
                }
            }
        }
        if !rows.is_empty() {
            blocks.push(rows.join("\n"));
        }
    }

    // Fill from the newest complete block backwards so the live tail and final
    // worker result can never be displaced by older verbose output. Reserve the
    // omission marker before admitting each older block.
    let mut selected_start = blocks.len();
    let mut selected_bytes = 0usize;
    for index in (0..blocks.len()).rev() {
        let older_would_be_omitted = recent_start > 0 || index > 0;
        let marker_bytes = usize::from(older_would_be_omitted) * omitted_marker.len();
        if header
            .len()
            .saturating_add(selected_bytes)
            .saturating_add(blocks[index].len())
            .saturating_add(marker_bytes)
            > MAX_TOTAL_BYTES
        {
            break;
        }
        selected_start = index;
        selected_bytes += blocks[index].len();
    }

    let omitted = recent_start > 0 || selected_start > 0;
    let mut output = String::with_capacity(
        header.len() + selected_bytes + usize::from(omitted) * omitted_marker.len(),
    );
    output.push_str(&header);
    for block in &blocks[selected_start..] {
        output.push_str(block);
    }
    if omitted {
        output.insert_str(header.len(), &omitted_marker);
    }
    debug_assert!(output.len() <= MAX_TOTAL_BYTES + 4096);
    Ok(output)
}

#[derive(Clone, Debug)]
struct SubagentViewEntry {
    node_id: String,
    label: String,
    description: String,
    session_reference: Option<String>,
    fallback_detail: String,
}

fn subagent_view_entries_from_presentation(
    view: crate::extensions::ExtensionPresentationView,
) -> Option<(String, Vec<SubagentViewEntry>)> {
    let title = view
        .snapshot
        .status
        .as_ref()
        .map(|status| status.label.clone())
        .unwrap_or_else(|| "Subagents".into());
    let collection = view.snapshot.collection?;
    let detail = collection.detail;
    let entries = collection
        .nodes
        .into_iter()
        .map(|node| {
            let state = format!("{:?}", node.state).to_lowercase();
            let description = node.secondary.clone().unwrap_or_else(|| state.clone());
            let session_reference = node
                .references
                .iter()
                .find(|reference| {
                    reference.kind == ygg_agent::ExtensionPresentationReferenceKind::Session
                })
                .map(|reference| reference.id.clone());
            let fallback_detail = detail
                .as_ref()
                .filter(|detail| detail.node_id.as_deref() == Some(node.id.as_str()))
                .map(|detail| detail.body.clone())
                .unwrap_or_else(|| {
                    format!(
                        "{}\n\nState: {state}\nTranscript is not available yet.",
                        node.label
                    )
                });
            SubagentViewEntry {
                node_id: node.id,
                label: node.label,
                description,
                session_reference,
                fallback_detail,
            }
        })
        .collect();
    Some((title, entries))
}

fn subagent_view_entries(
    extensions: &crate::extensions::ExecutableExtensions,
) -> Option<(String, Vec<SubagentViewEntry>)> {
    let view = extensions
        .presentation_views()
        .into_iter()
        .find(|view| view.extension == "ygg-subagents")?;
    subagent_view_entries_from_presentation(view)
}

fn subagent_picker_snapshot(
    title: &str,
    entries: &[SubagentViewEntry],
    notices: Vec<String>,
) -> SubagentPickerSnapshot {
    SubagentPickerSnapshot {
        title: format!("{title} · Enter views transcript · Esc closes"),
        items: entries.iter().map(|entry| entry.label.clone()).collect(),
        descriptions: entries
            .iter()
            .map(|entry| Some(entry.description.clone()))
            .collect(),
        node_ids: entries.iter().map(|entry| entry.node_id.clone()).collect(),
        notices,
    }
}

struct SubagentRefreshContext<'a> {
    extensions: &'a mut crate::extensions::ExecutableExtensions,
    last_error: Option<String>,
}

fn refresh_subagent_snapshot<'a, 'extensions>(
    context: &'a mut SubagentRefreshContext<'extensions>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = SubagentPickerSnapshot> + 'a>> {
    Box::pin(async move {
        let refresh_result = tokio::time::timeout(
            Duration::from_millis(750),
            context
                .extensions
                .execute_command_without_confirmation("subagents", vec!["status".into()]),
        )
        .await;
        let refresh_error = match refresh_result {
            Err(_) => Some(
                "subagent view live refresh timed out; showing the last accepted state".to_owned(),
            ),
            Ok(Ok(Some(output))) if output.contains("failed closed") => Some(
                "subagent view live refresh failed closed; showing the last accepted state"
                    .to_owned(),
            ),
            Ok(Ok(Some(_))) => None,
            Ok(Ok(None)) => Some(
                "subagent view live refresh is unavailable; showing the last accepted state"
                    .to_owned(),
            ),
            Ok(Err(error)) => Some(format!(
                "subagent view live refresh failed; showing the last accepted state: {error}"
            )),
        };
        let mut notices = context.extensions.drain_events();
        if refresh_error != context.last_error {
            if let Some(error) = refresh_error.as_ref() {
                notices.push(error.clone());
            }
            context.last_error = refresh_error;
        }
        match subagent_view_entries(context.extensions) {
            Some((title, entries)) => subagent_picker_snapshot(&title, &entries, notices),
            None => SubagentPickerSnapshot {
                title: "Subagents · waiting for current state · Esc closes".into(),
                items: Vec::new(),
                descriptions: Vec::new(),
                node_ids: Vec::new(),
                notices,
            },
        }
    })
}

/// Whether an unknown-command text is the bare `/subagents` live view owned
/// by the ygg-subagents extension. Only that view is safe to open mid-run:
/// it reads extension presentation state and never touches the running
/// agent session.
fn is_live_subagents_command(
    text: &str,
    extensions: &crate::extensions::ExecutableExtensions,
) -> bool {
    let Some(name) = text.strip_prefix('/') else {
        return false;
    };
    name.trim() == "subagents"
        && extensions.command_owner("subagents").as_deref() == Some("ygg-subagents")
}

/// Live `/subagents` view while a run is active. Mirrors the idle
/// `subagents_view` picker: selection opens the same theme-styled read-only
/// worker transcript through the active run's delegation binding, so every
/// subagent with a durable child session is viewable mid-run too.
async fn active_subagents_view<S, F>(
    shell: &mut InteractiveShell,
    input: &mut S,
    extensions: &mut crate::extensions::ExecutableExtensions,
    open_delegated: F,
) -> anyhow::Result<()>
where
    S: futures_util::Stream<Item = std::io::Result<Event>> + Unpin,
    F: Fn(&str, &str) -> Result<Option<Session>, ygg_agent::AgentError>,
{
    loop {
        if subagent_view_entries(extensions).is_none_or(|(_, entries)| entries.is_empty()) {
            for notice in extensions.drain_events() {
                shell.notice(notice);
            }
            shell.notice("No subagents for this session.");
            return Ok(());
        }
        let notices = extensions.drain_events();
        let (title, entries) =
            subagent_view_entries(extensions).expect("entries checked non-empty above");
        let initial = subagent_picker_snapshot(&title, &entries, notices);
        let selected_id = {
            let mut refresh = SubagentRefreshContext {
                extensions,
                last_error: None,
            };
            subagent_picker(
                shell,
                input,
                initial,
                0,
                &mut refresh,
                refresh_subagent_snapshot,
            )
            .await?
        };
        let Some(selected_id) = selected_id else {
            return Ok(());
        };
        for notice in extensions.drain_events() {
            shell.notice(notice);
        }
        // Open the same theme-styled transcript as the idle view, resolved
        // through the active run's delegation binding. The read-only session
        // file never touches the running agent state.
        if let Some((_, entries)) = subagent_view_entries(extensions) {
            if let Some(entry) = entries.iter().find(|entry| entry.node_id == selected_id) {
                let reference = entry.session_reference.clone();
                let principal = reference.as_deref().and_then(|reference| {
                    extensions.presentation_session_reference_principal(reference)
                });
                let node_id = entry.node_id.clone();
                let fallback_detail = entry.fallback_detail.clone();
                let label = entry.label.clone();
                let theme = shell.theme();
                let width = shell.width();
                let open_text = |fallback: &str| -> String {
                    match (principal.as_deref(), reference.as_deref()) {
                        (Some(principal), Some(reference)) => {
                            match open_delegated(principal, reference) {
                                Ok(Some(session)) => {
                                    delegated_session_text(&session, &theme, width)
                                        .unwrap_or_else(|error| {
                                            format!(
                                                "{fallback}\n\nFailed to render the delegated transcript: {error}"
                                            )
                                        })
                                }
                                Ok(None) => format!(
                                    "{fallback}\n\nThe delegated transcript is not available yet."
                                ),
                                Err(error) => format!(
                                    "{fallback}\n\nFailed to open the delegated transcript: {error}"
                                ),
                            }
                        }
                        _ => fallback.to_owned(),
                    }
                };
                let initial_text = open_text(&fallback_detail);
                let refresh = || {
                    let current_fallback = subagent_view_entries(extensions)
                        .and_then(|(_, entries)| {
                            entries
                                .into_iter()
                                .find(|candidate| candidate.node_id == node_id)
                        })
                        .map(|candidate| candidate.fallback_detail)
                        .unwrap_or_else(|| fallback_detail.clone());
                    let result: anyhow::Result<Option<String>> =
                        Ok(Some(open_text(&current_fallback)));
                    std::future::ready(result)
                };
                read_only_document_live_styled(
                    shell,
                    input,
                    format!("{label} · read-only transcript"),
                    initial_text,
                    refresh,
                )
                .await?;
            } else {
                shell.notice("subagent state changed; select it again to view");
            }
        }
    }
}

async fn subagents_view(
    app: &mut App,
    shell: &mut InteractiveShell,
    input: &mut EventStream,
    command_output: String,
) -> anyhow::Result<()> {
    let unavailable = (!command_output.trim().is_empty()).then_some(command_output);
    if unavailable.as_deref().is_some_and(|output| {
        output.contains("failed closed")
            || output.contains(" Warning]")
            || output.contains(" Error]")
            || output.contains("confirmation denied")
            || output.contains("input cancelled")
            || output.contains("extension events because the consumer lagged")
            || output
                .lines()
                .any(|line| line.starts_with("warning:") || line.starts_with("error:"))
    }) {
        shell.notice(
            "subagent command reported extension diagnostics; showing the last accepted state (see /extensions status)",
        );
    }
    let mut selected_node_id = None::<String>;
    loop {
        let notices = app.executable_extensions.drain_events();
        let Some((title, entries)) = subagent_view_entries(&app.executable_extensions) else {
            for notice in notices {
                shell.notice(notice);
            }
            read_only_document(
                shell,
                input,
                "Subagents",
                unavailable
                    .as_deref()
                    .unwrap_or("No subagents for this session.")
                    .to_owned(),
            )
            .await?;
            return Ok(());
        };
        if entries.is_empty() {
            for notice in notices {
                shell.notice(notice);
            }
            read_only_document(
                shell,
                input,
                "Subagents",
                unavailable
                    .as_deref()
                    .unwrap_or("No subagents for this session.")
                    .to_owned(),
            )
            .await?;
            return Ok(());
        }
        let initial_selected = selected_node_id
            .as_ref()
            .and_then(|id| entries.iter().position(|entry| &entry.node_id == id))
            .unwrap_or(0);
        let initial = subagent_picker_snapshot(&title, &entries, notices);
        let selected_id = {
            let mut refresh = SubagentRefreshContext {
                extensions: &mut app.executable_extensions,
                last_error: None,
            };
            subagent_picker(
                shell,
                input,
                initial,
                initial_selected,
                &mut refresh,
                refresh_subagent_snapshot,
            )
            .await?
        };
        let Some(selected_id) = selected_id else {
            return Ok(());
        };
        selected_node_id = Some(selected_id.clone());

        // Revalidate the stable node and typed reference against the newest
        // accepted presentation revision immediately before opening it.
        for notice in app.executable_extensions.drain_events() {
            shell.notice(notice);
        }
        let Some((_, current_entries)) = subagent_view_entries(&app.executable_extensions) else {
            shell.error("subagent state changed before the transcript could open".into());
            continue;
        };
        let Some(entry) = current_entries
            .iter()
            .find(|entry| entry.node_id == selected_id)
        else {
            shell.error("the selected subagent is no longer available".into());
            continue;
        };
        let reference = entry.session_reference.clone();
        let principal = reference.as_deref().and_then(|reference| {
            app.executable_extensions
                .presentation_session_reference_principal(reference)
        });
        let node_id = entry.node_id.clone();
        let fallback_detail = entry.fallback_detail.clone();
        let theme = shell.theme();
        let width = shell.width();
        let initial_text = if let (Some(principal), Some(reference)) =
            (principal.as_deref(), reference.as_deref())
        {
            match app
                .agent
                .open_delegated_session_reference(principal, reference)
            {
                Ok(Some(session)) => delegated_session_text(&session, &shell.theme(), width)?,
                Ok(None) => format!(
                    "{}\n\nThe delegated transcript is no longer available for this parent session.",
                    fallback_detail
                ),
                Err(error) => format!(
                    "{}\n\nFailed to open the delegated transcript: {error}",
                    fallback_detail
                ),
            }
        } else {
            fallback_detail.clone()
        };
        let refresh = || {
            let current_fallback = subagent_view_entries(&app.executable_extensions)
                .and_then(|(_, entries)| {
                    entries
                        .into_iter()
                        .find(|candidate| candidate.node_id == node_id)
                })
                .map(|candidate| candidate.fallback_detail)
                .unwrap_or_else(|| fallback_detail.clone());
            let result: anyhow::Result<Option<String>> = if let (Some(principal), Some(reference)) =
                (principal.as_deref(), reference.as_deref())
            {
                match app
                    .agent
                    .open_delegated_session_reference(principal, reference)
                {
                    Ok(Some(session)) => {
                        delegated_session_text(&session, &theme, width).map(Some)
                    }
                    Ok(None) => Ok(Some(format!(
                        "{}\n\nThe delegated transcript is no longer available for this parent session.",
                        current_fallback
                    ))),
                    Err(error) => Ok(Some(format!(
                        "{}\n\nFailed to open the delegated transcript: {error}",
                        current_fallback
                    ))),
                }
            } else {
                Ok(Some(current_fallback))
            };
            std::future::ready(result)
        };
        read_only_document_live_styled(
            shell,
            input,
            format!("{} · read-only transcript", entry.label),
            initial_text,
            refresh,
        )
        .await?;
        if shell.close_requested() {
            return Ok(());
        }
    }
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
    current_session_path: Option<&std::path::Path>,
) -> anyhow::Result<Option<std::path::PathBuf>> {
    let store_for_listing = store.clone();
    let sessions = run_blocking_lifecycle(shell, input, "discovering sessions…", move || {
        Ok(store_for_listing.list())
    })
    .await?;
    session_picker(shell, input, &sessions, store, current_session_path).await
}

fn active_fork_messages(session: &Session) -> Vec<crate::tui::view::ForkMessage> {
    let mut newest_first = Vec::new();
    let mut cursor = session.head_ref();
    while let Some(id) = cursor {
        let Some(entry) = session.entry(id) else {
            break;
        };
        newest_first.push(entry);
        cursor = entry.parent.as_ref();
    }
    newest_first.reverse();

    let mut messages = newest_first
        .into_iter()
        .filter_map(|entry| {
            let ygg_agent::EntryValue::Message(ygg_ai::Message::User(user)) = &entry.value else {
                return None;
            };
            let text = user
                .content
                .iter()
                .filter_map(|part| match part {
                    ygg_ai::UserPart::Text(text) => Some(text.as_str()),
                    ygg_ai::UserPart::Media(_) | ygg_ai::UserPart::ToolResult(_) => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            (!text.trim().is_empty()).then(|| crate::tui::view::ForkMessage {
                entry_id: entry.id.0.clone(),
                text,
                whole_conversation: false,
            })
        })
        .collect::<Vec<_>>();

    if !messages.is_empty() {
        if let Some(head) = session.head_ref() {
            messages.push(crate::tui::view::ForkMessage {
                entry_id: head.0.clone(),
                text: String::new(),
                whole_conversation: true,
            });
        }
    }
    messages
}

fn fork_active_session(
    sessions: &crate::session_store::SessionStore,
    source_path: &std::path::Path,
    destination: std::path::PathBuf,
    checkpoint: Option<&EntryId>,
) -> anyhow::Result<std::path::PathBuf> {
    let source = Session::open_read_only(source_path).with_context(|| {
        format!(
            "could not open current session for forking: {}",
            source_path.display()
        )
    })?;
    let source_id = source
        .path()
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| anyhow::anyhow!("current session has no valid id"))?;
    let forked = source.fork_to(destination.clone(), checkpoint)?;
    drop(forked);
    if let Some(checkpoint) = checkpoint {
        let destination_id = destination
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| anyhow::anyhow!("forked session has no valid id"))?;
        if let Err(error) =
            sessions.set_fork_provenance(destination_id, source_id, checkpoint.0.as_str())
        {
            let _ = std::fs::remove_file(&destination);
            return Err(error);
        }
    }
    Ok(destination)
}

async fn fork_active_session_lifecycle(
    app: &App,
    checkpoint: EntryId,
    shell: &mut InteractiveShell,
    input: &mut EventStream,
) -> anyhow::Result<std::path::PathBuf> {
    let sessions = app.sessions.clone();
    let source_path = app.agent.session().path().to_owned();
    let destination = sessions.new_path(&crate::modes::timestamp());
    run_blocking_lifecycle(shell, input, "forking session…", move || {
        fork_active_session(&sessions, &source_path, destination, Some(&checkpoint))
    })
    .await
}

async fn fork_session(
    mut app: App,
    shell: &mut InteractiveShell,
    input: &mut EventStream,
) -> anyhow::Result<App> {
    let messages = active_fork_messages(app.agent.session());
    if messages.is_empty() {
        shell.notice("No messages to fork from");
        return Ok(app);
    }
    let Some((entry_id, text)) = message_picker(shell, input, messages).await? else {
        return Ok(app);
    };
    let checkpoint = EntryId(entry_id);
    let destination = fork_active_session_lifecycle(&app, checkpoint, shell, input).await?;
    app = transition(app, shell, input, Reconfig::Resume(destination)).await?;
    shell.prefill_editor(text);
    shell.notice("Forked to new session");
    Ok(app)
}

async fn clone_session(
    mut app: App,
    shell: &mut InteractiveShell,
    input: &mut EventStream,
) -> anyhow::Result<App> {
    let Some(head) = app.agent.session().head() else {
        shell.notice("Nothing to clone yet");
        return Ok(app);
    };
    let destination = fork_active_session_lifecycle(&app, head, shell, input).await?;
    app = transition(app, shell, input, Reconfig::Resume(destination)).await?;
    shell.clear_editor();
    shell.notice("Cloned to new session");
    Ok(app)
}

async fn apply_pending_actions(
    mut app: App,
    shell: &mut InteractiveShell,
    input: &mut EventStream,
    pending_actions: &mut VecDeque<PendingIdleAction>,
    goal_deadline: &mut Option<Instant>,
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
            PendingIdleAction::ChangeThinking(reasoning) => {
                if let Err(e) = crate::cli::persist_reasoning(&reasoning_label(&reasoning)) {
                    shell.error(format!("failed to save thinking preference: {e}"));
                }
                app = transition(app, shell, input, Reconfig::Thinking(reasoning)).await?;
                shell.notice("queued thinking change applied");
            }
            PendingIdleAction::ChangeThinkingLevel(level) => {
                let reasoning = thinking_to_reasoning_with_subagents(
                    level,
                    &app.model,
                    app.subagents_available(),
                )?;
                if let Err(e) = crate::cli::persist_reasoning(&reasoning_label(&reasoning)) {
                    shell.error(format!("failed to save thinking preference: {e}"));
                }
                app = transition(app, shell, input, Reconfig::Thinking(reasoning)).await?;
                shell.notice("queued thinking change applied");
            }
            PendingIdleAction::CycleThinking => {
                let level = next_thinking_level(&app)?;
                let reasoning = thinking_to_reasoning_with_subagents(
                    level,
                    &app.model,
                    app.subagents_available(),
                )?;
                if let Err(e) = crate::cli::persist_reasoning(&reasoning_label(&reasoning)) {
                    shell.error(format!("failed to save thinking preference: {e}"));
                }
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
                if let Some(path) = pick_session_path(
                    shell,
                    input,
                    &app.sessions,
                    Some(app.agent.session().path()),
                )
                .await?
                {
                    app = transition(app, shell, input, Reconfig::Resume(path)).await?;
                    shell.notice("queued session resumed");
                }
            }
            PendingIdleAction::Fork => {
                app = fork_session(app, shell, input).await?;
            }
            PendingIdleAction::Clone => {
                app = clone_session(app, shell, input).await?;
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
                shell.notice("instructions, prompts, skills, and extensions reloaded");
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
                if let Some((mode, level)) =
                    thinking_configuration_picker(&app, shell, input).await?
                {
                    if let Err(error) = crate::cli::persist_reasoning_mode(mode) {
                        shell.error(format!("failed to save reasoning mode preference: {error}"));
                    }
                    let reasoning = thinking_to_reasoning_with_subagents(
                        level,
                        &app.model,
                        app.subagents_available(),
                    )?;
                    app = transition(
                        app,
                        shell,
                        input,
                        Reconfig::ThinkingMode { mode, reasoning },
                    )
                    .await?;
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
            PendingIdleAction::Goal(command) => {
                apply_goal_command(&app, shell, command, goal_deadline)?;
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
            Ok(_) => {
                shell.restore_composed(ComposedInput::from_text(format!("/skill:{id}")));
                shell.notice(format!("skill invocation /skill:{id} is ready to submit"));
            }
            Err(error) => shell.error(format!("Failed to invoke skill '{id}': {error}")),
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
    goal_deadline: &mut Option<Instant>,
) -> anyhow::Result<IdleCommandOutcome> {
    match command {
        Command::Help(topic) => {
            shell.show_overlay_text(commands::help_text(&app.config.workspace, topic.as_deref()));
        }
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
                Ok(status) => shell.show_overlay_text(match status {
                    crate::update::UpdateStatus::Available { .. } => {
                        format!("{}\n\nRun `ygg update` to install.", status)
                    }
                    status => status.to_string(),
                }),
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
        Command::Extensions(commands::ExtensionsSubcommand::Menu) => {
            app = extension_management_menu(app, shell, input).await?;
        }
        Command::Extensions(commands::ExtensionsSubcommand::Status) => {
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
        Command::Extensions(commands::ExtensionsSubcommand::Inspect { reference }) => {
            let _ = app.executable_extensions.drain_events();
            let principal = app
                .executable_extensions
                .presentation_session_reference_principal(&reference);
            if let Some(principal) = principal {
                match app
                    .agent
                    .open_delegated_session_reference(&principal, &reference)
                {
                    Ok(Some(session)) => {
                        match delegated_session_text(&session, &shell.theme(), shell.width()) {
                            Ok(text) => shell.show_overlay_text(text),
                            Err(error) => {
                                shell.error(format!("failed to inspect delegated session: {error}"))
                            }
                        }
                    }
                    Ok(None) => shell
                        .error("delegated session reference is unavailable for this parent".into()),
                    Err(error) => {
                        shell.error(format!("failed to inspect delegated session: {error}"))
                    }
                }
            } else {
                shell.error(
                    "delegated session reference is unavailable, stale, or owned by another extension"
                        .into(),
                );
            }
        }
        Command::Extensions(commands::ExtensionsSubcommand::Action { extension, action }) => {
            let result = {
                let mut confirmations = InteractiveExtensionConfirmations { shell, input };
                app.executable_extensions
                    .execute_presentation_action_with_confirmation(
                        &extension,
                        &action,
                        &mut confirmations,
                    )
                    .await
            };
            match result {
                Ok(output) if output.trim().is_empty() => {
                    shell.notice(format!("{extension} action {action} completed"));
                }
                Ok(output) => shell.show_overlay_text(output),
                Err(error) => shell.error(format!("extension action failed: {error}")),
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
            if let Some(path) = pick_session_path(
                shell,
                input,
                &app.sessions,
                Some(app.agent.session().path()),
            )
            .await?
            {
                app = transition(app, shell, input, Reconfig::Resume(path)).await?;
                shell.notice("resumed session");
            }
        }
        Command::Fork => {
            app = fork_session(app, shell, input).await?;
        }
        Command::Clone => {
            app = clone_session(app, shell, input).await?;
        }
        Command::Model(Some(id)) => {
            app = transition(app, shell, input, Reconfig::Model(ModelId(id))).await?;
            shell.notice(format!(
                "model changed · {}",
                commands::model_selection_text(&app.model)
            ));
        }
        Command::Thinking(Some(level)) => {
            let level = ThinkingLevel::parse(&level)?;
            let reasoning =
                thinking_to_reasoning_with_subagents(level, &app.model, app.subagents_available())?;
            if let Err(e) = crate::cli::persist_reasoning(&reasoning_label(&reasoning)) {
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
            if let Some((mode, level)) = thinking_configuration_picker(&app, shell, input).await? {
                if let Err(error) = crate::cli::persist_reasoning_mode(mode) {
                    shell.error(format!("failed to save reasoning mode preference: {error}"));
                }
                let reasoning = thinking_to_reasoning_with_subagents(
                    level,
                    &app.model,
                    app.subagents_available(),
                )?;
                app = transition(
                    app,
                    shell,
                    input,
                    Reconfig::ThinkingMode { mode, reasoning },
                )
                .await?;
                shell.notice("thinking changed");
            }
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
                let original_keep = app.config.compaction.keep_recent_tokens;
                app.config.compaction.keep_recent_tokens = 1;
                let result = await_with_ctrl_c(attempt_compaction(&mut app), shell, input).await;
                app.config.compaction.keep_recent_tokens = original_keep;
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
            shell.notice("instructions, prompts, skills, and extensions reloaded");
        }
        Command::Tree => shell.show_overlay_text(session_tree_text(app.agent.session())),
        Command::Checkout(id) => {
            app = checkout_entry(app, shell, input, id).await?;
        }
        Command::Skills(commands::SkillsSubcommand::Load(id)) => {
            if let Err(error) = app.skills.load(&id) {
                shell.error(format!("Failed to invoke skill '{id}': {error}"));
            } else {
                return Ok(IdleCommandOutcome::Submit {
                    app: Box::new(app),
                    prompt: format!("/skill:{id}"),
                });
            }
        }
        Command::Skills(commands::SkillsSubcommand::Reload) => {
            app = reload_resources(app, shell, input).await?;
            request_extension_ui(shell, &mut app);
            shell.notice("skills and prompt templates reloaded");
        }
        Command::Skills(sub) => {
            execute_skills_command(&mut app, shell, sub).await?;
        }
        Command::Goal(goal) => {
            apply_goal_command(&app, shell, goal, goal_deadline)?;
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
            let presentation_owner = app.executable_extensions.command_owner(&extension_name);
            let open_subagents = extension_name == "subagents"
                && extension_arguments.is_empty()
                && presentation_owner.as_deref() == Some("ygg-subagents");
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
                Ok(Some(output)) if open_subagents => {
                    subagents_view(&mut app, shell, input, output).await?;
                }
                Ok(Some(output)) => {
                    let presentation = presentation_owner
                        .as_deref()
                        .and_then(|owner| app.executable_extensions.presentation_text_for(owner));
                    let mut visible_blocks = Vec::new();
                    if !output.trim().is_empty() {
                        visible_blocks.push(output);
                    }
                    visible_blocks.extend(presentation);
                    let visible = visible_blocks.join("\n\n");
                    if visible.trim().is_empty() {
                        shell.notice(format!("/{extension_name} completed"));
                    } else {
                        shell.show_extension_output(&extension_name, visible);
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
                        Ok(None) => {
                            // A slash command that no running extension
                            // contributes may still belong to an extension
                            // that is starting, degraded, or parked. Say so
                            // instead of a bare unknown.
                            let not_ready: Vec<String> = app
                                .executable_extensions
                                .summaries()
                                .into_iter()
                                .filter(|summary| {
                                    summary.enabled
                                        && (!summary.running
                                            || summary.health.as_ref().is_some_and(|health| {
                                                health.state
                                                    != ygg_agent::ExtensionHealthState::Ready
                                            }))
                                })
                                .map(|summary| summary.name)
                                .collect();
                            if text.starts_with('/') && !not_ready.is_empty() {
                                shell.error(format!(
                                    "unknown command: {text} (extensions not ready: {} — see /extensions status)",
                                    not_ready.join(", ")
                                ));
                            } else {
                                shell.error(format!("unknown command: {text}"));
                            }
                        }
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
    tail: VecDeque<u8>,
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
                .extend(remaining[remaining.len() - tail_capacity..].iter().copied());
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
        self.tail.extend(remaining.iter().copied());
    }

    fn render(&self, stream: &str) -> String {
        if self.total_bytes <= self.budget {
            let mut complete = Vec::with_capacity(self.total_bytes);
            complete.extend_from_slice(&self.head);
            complete.extend(self.tail.iter().copied());
            return String::from_utf8_lossy(&complete).into_owned();
        }
        let omitted = self
            .total_bytes
            .saturating_sub(self.head.len())
            .saturating_sub(self.tail.len());
        let tail = self.tail.iter().copied().collect::<Vec<_>>();
        format!(
            "{}\n[… {stream} truncated; {omitted} bytes omitted …]\n{}",
            String::from_utf8_lossy(&self.head),
            String::from_utf8_lossy(&tail)
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
        ygg_agent::extension_process::terminate_bash_process_groups(Duration::from_millis(400))
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

fn startup_launch_outcome<T>(
    shell: &InteractiveShell,
    result: anyhow::Result<T>,
) -> anyhow::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(_) if shell.close_requested() => Ok(None),
        Err(error) => Err(error),
    }
}

async fn run_interactive_without_model(
    boot: Bootstrap,
    launch: crate::app::bootstrap::LaunchSelection,
    shell: &mut InteractiveShell,
    input: &mut EventStream,
) -> anyhow::Result<()> {
    let mut boot = boot;
    let workspace = boot.config.workspace.clone();
    let mut prepared = boot.take_prepared_session();
    let selection = launch.session;
    let session = run_blocking_lifecycle(shell, input, "opening session…", move || {
        crate::app::bootstrap::open_launch_session(&mut prepared, selection)
    })
    .await?;

    shell.set_identity("", "", "");
    shell.set_status_detail("no configured model · read-only session".to_owned());
    shell.set_workspace(workspace.clone());
    shell.set_input_modalities(ygg_ai::ModalitySet::none());
    shell.set_session_telemetry(&session, None);
    shell.hydrate(&session)?;
    shell.notice(
        "No configured model. Use /login, /model, or /reload to configure one; prompts are disabled until then.",
    );
    shell.render();

    let mut scroll_tick = tokio::time::interval(Duration::from_millis(16));
    scroll_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut extension_tick = tokio::time::interval(Duration::from_millis(50));
    extension_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut extensions = crate::extensions::ExecutableExtensions::default();

    loop {
        match wait_for_prompt(
            shell,
            input,
            &mut scroll_tick,
            &mut extension_tick,
            &mut extensions,
            None,
        )
        .await?
        {
            Idle::Quit => return Ok(()),
            Idle::CycleThinking => {
                shell.notice("thinking is unavailable until a model is configured");
                shell.render();
            }
            Idle::GoalContinuation => unreachable!("model-less mode has no goal deadline"),
            Idle::Submit(_) => {
                shell.error(
                    "no configured model; set an API key and restart before submitting prompts"
                        .to_owned(),
                );
                shell.render();
            }
            Idle::Command(raw) => match commands::parse(&raw) {
                Command::Quit => return Ok(()),
                Command::Help(topic) => {
                    shell.show_overlay_text(commands::help_text(&workspace, topic.as_deref()));
                    shell.render();
                }
                Command::Status => {
                    shell.show_overlay_text(
                        "No model is configured. The session can be read, but prompts are disabled."
                            .to_owned(),
                    );
                    shell.render();
                }
                Command::Login(provider) => match validate_provider(provider.as_deref()) {
                    Ok("codex") => {
                        if let Some(catalog) = login_codex_catalog(shell).await? {
                            boot.catalog = catalog;
                            shell.clear_error();
                            shell.notice(
                                "signed in to ChatGPT; use /model to select a model, then restart Ygg to chat",
                            );
                            shell.render();
                        }
                    }
                    Ok("custom") => {
                        login_custom(shell)?;
                        shell.render();
                    }
                    Ok(_) => unreachable!(),
                    Err(error) => {
                        shell.error(error.to_string());
                        shell.render();
                    }
                },
                Command::Model(model) => {
                    if boot.catalog.models().next().is_none() {
                        shell.notice(
                            "no configured models are available; use /login or edit the custom provider, then /reload",
                        );
                    } else {
                        let selected = match model {
                            Some(id) => {
                                let id = ModelId(id);
                                if boot.catalog.resolve(&id).is_err() {
                                    shell.error(format!("model {} is not available", id.0));
                                    None
                                } else {
                                    if let Err(error) = crate::cli::persist_model(&id.0) {
                                        shell.error(format!(
                                            "failed to save model preference: {error}"
                                        ));
                                    }
                                    Some(id)
                                }
                            }
                            None => optional_model_picker(shell, input, &boot.catalog).await?,
                        };
                        if let Some(model) = selected {
                            shell.notice(format!(
                                "model {} selected; restart Ygg to start chatting",
                                model.0
                            ));
                        }
                    }
                    shell.render();
                }
                Command::Reload => {
                    let catalog = run_blocking_lifecycle(
                        shell,
                        input,
                        "reloading models…",
                        crate::app::bootstrap::model_catalog,
                    )
                    .await?;
                    let has_models = catalog.models().next().is_some();
                    boot.catalog = catalog;
                    if has_models {
                        shell.notice(
                            "models reloaded; use /model to select one, then restart Ygg to chat",
                        );
                    } else {
                        shell.notice("model reload completed, but no configured models were found");
                    }
                    shell.render();
                }
                _ => {
                    shell.notice("this command is unavailable until a model is configured");
                    shell.render();
                }
            },
        }
    }
}

fn schedule_responses_prewarm(app: &App) {
    let Ok(Some((client, model, request))) = app.agent.responses_prewarm_request() else {
        return;
    };
    tokio::spawn(async move {
        let _ = client.prewarm_responses(&model, request).await;
    });
}

/// Run the interactive frontend with explicit idle and active borrow phases.
pub async fn run_interactive(boot: Bootstrap) -> anyhow::Result<()> {
    let initial_prompt = boot.config.initial_prompt.clone();
    let theme = load_theme(&boot.config);
    let size = Arc::new(Mutex::new(crossterm::terminal::size().unwrap_or((80, 24))));
    let mut shell =
        InteractiveShell::enter_with_mouse(theme, size, boot.config.mouse.application_owned())?;
    shell.set_runtime_config(boot.config.clone());
    apply_detected_terminal_background(&mut shell, &boot.config);
    let mut input = EventStream::new();
    // The shell owns a dedicated renderer thread, but sexy-tui still renders
    // synchronously when that thread receives a request. This clock only
    // coalesces high-rate wheel input on the input loop.
    let mut scroll_tick = tokio::time::interval(Duration::from_millis(16));
    scroll_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut extension_tick = tokio::time::interval(Duration::from_millis(50));
    extension_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let launch_result = resolve_launch_interactive(&boot, &mut shell, &mut input).await;
    let Some(launch) = startup_launch_outcome(&shell, launch_result)? else {
        shell.leave();
        return Ok(());
    };
    if boot.is_modeless() {
        let result = run_interactive_without_model(boot, launch, &mut shell, &mut input).await;
        shell.leave();
        return result;
    }
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
    schedule_responses_prewarm(&app);

    let mut pending_actions = VecDeque::new();
    let mut goal_deadline = recovered_goal_deadline(&app)?;
    let mut next_prompt_source = GoalTurnSource::User;
    'interactive: loop {
        if shell.close_requested() {
            shutdown_for_exit(&mut app).await;
            break;
        }
        let idle = match startup_prompt.take() {
            Some(prompt) if !prompt.is_empty() => Idle::Submit(ComposedInput::from_text(prompt)),
            _ => {
                wait_for_prompt(
                    &mut shell,
                    &mut input,
                    &mut scroll_tick,
                    &mut extension_tick,
                    &mut app.executable_extensions,
                    goal_deadline,
                )
                .await?
            }
        };
        match idle {
            Idle::Quit => {
                shutdown_for_exit(&mut app).await;
                break;
            }
            Idle::GoalContinuation => {
                goal_deadline = None;
                match app.goal_driver.fire_continuation() {
                    Ok(Some(continuation)) => {
                        next_prompt_source = GoalTurnSource::Continuation;
                        startup_prompt = Some(continuation.prompt);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        let _ = app.goal_driver.session_error();
                        shell.error(format!("goal continuation unavailable: {error}"));
                        shell.render();
                    }
                }
            }
            Idle::CycleThinking => {
                let level = next_thinking_level(&app)?;
                let reasoning = thinking_to_reasoning_with_subagents(
                    level,
                    &app.model,
                    app.subagents_available(),
                )?;
                if let Err(error) = crate::cli::persist_reasoning(&reasoning_label(&reasoning)) {
                    shell.error(format!("failed to save thinking preference: {error}"));
                }
                app =
                    transition(app, &mut shell, &mut input, Reconfig::Thinking(reasoning)).await?;
                schedule_responses_prewarm(&app);
                shell.notice(format!("thinking changed to {}", level.label()));
                shell.render();
            }
            Idle::Command(command_input) => {
                if command_input.trim_start().starts_with("/skill:") {
                    startup_prompt = Some(command_input);
                    continue;
                }
                match run_idle_command(
                    app,
                    &mut shell,
                    &mut input,
                    commands::parse(&command_input),
                    &mut goal_deadline,
                )
                .await?
                {
                    IdleCommandOutcome::Continue(next) => {
                        app = *next;
                        schedule_responses_prewarm(&app);
                    }
                    IdleCommandOutcome::Submit { app: next, prompt } => {
                        app = *next;
                        schedule_responses_prewarm(&app);
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
                let prompt_source = next_prompt_source;
                next_prompt_source = GoalTurnSource::User;
                if prompt_source == GoalTurnSource::User {
                    goal_deadline = None;
                    app.goal_driver.user_spoke();
                }
                // Shell escapes have the same authority as the model `bash`
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
                    let group_guard = ProcessGroupGuard::bash(child.id());

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
                    let command_timeout = Duration::from_secs(app.config.sandbox.bash_timeout_secs);
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
                                Some(Ok(Event::Key(key))) if keymap::is_close_key(&key) => {
                                    shell.request_close();
                                    interrupted = true;
                                    shutting_down = true;
                                    break Err(std::io::Error::new(
                                        std::io::ErrorKind::Interrupted,
                                        "command stopped during close",
                                    ));
                                }
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
                                    shell.toggle_disclosure();
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
                        }
                    };

                    if shutting_down {
                        #[cfg(unix)]
                        {
                            let process_cleanup =
                                ygg_agent::extension_process::terminate_bash_process_groups(
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
                        group_guard.supervise_bash_descendants(
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
                                    app.config.sandbox.bash_timeout_secs
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
                let model_prompt = match expand_skill_command(
                    app.skills.as_ref(),
                    &composed.transcript_text,
                    &app.agent.registered_tool_names(),
                ) {
                    Ok(Some(expanded)) => expanded,
                    Ok(None) => composed.transcript_text.clone(),
                    Err(error) => {
                        shell.restore_composed(composed);
                        shell.error(format!("skill invocation failed: {error}"));
                        shell.render();
                        continue;
                    }
                };
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
                            model_prompt,
                        ),
                        &mut shell,
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
                        let error = ygg_agent::public_error_diagnostic(
                            &error,
                            &app.model.endpoint.id.0,
                            &app.model.spec.id.0,
                        );
                        shell.error(format!("prompt was not saved: {error}"));
                        shell.render();
                        continue;
                    }
                };
                let extension_turn = app.executable_extensions.begin_turn().await;
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
                let mut made_tool_call = false;
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
                    &mut made_tool_call,
                )
                .await?;
                drop(run);
                app.executable_extensions
                    .settle_turn(extension_turn, &ended)
                    .await;
                app.agent.set_system_prompt(app.system.clone());
                if crate::tui::terminal::received_shutdown_signal().is_some() {
                    shutdown_for_exit(&mut app).await;
                    break 'interactive;
                }
                let goal_decision = if ended.allows_after_response() {
                    let response = crate::extensions::latest_assistant_text(app.agent.session());
                    let notifications = tokio::select! {
                        biased;
                        _ = crate::tui::terminal::wait_for_shutdown_signal() => {
                            shutdown_for_exit(&mut app).await;
                            break 'interactive;
                        }
                        result = await_with_ctrl_c(
                            app.executable_extensions.after_response(&response),
                            &mut shell,
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
                    settle_goal(
                        &app,
                        &mut shell,
                        prompt_source,
                        &response,
                        made_tool_call,
                        true,
                    )
                } else {
                    settle_goal(&app, &mut shell, prompt_source, "", made_tool_call, false)
                };
                goal_deadline = match goal_decision {
                    Some(GoalDecision::Wait { delay, .. }) => Some(Instant::now() + delay),
                    Some(GoalDecision::Complete) => {
                        shell.notice("goal completed");
                        None
                    }
                    Some(GoalDecision::Blocked) => {
                        shell.notice("goal blocked");
                        None
                    }
                    Some(GoalDecision::BudgetLimited) => {
                        shell.notice("goal continuation budget exhausted");
                        None
                    }
                    Some(GoalDecision::Suppressed) => None,
                    Some(GoalDecision::Paused) | Some(GoalDecision::Inactive) | None => None,
                };
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
                if shell.close_requested() {
                    shutdown_for_exit(&mut app).await;
                    break 'interactive;
                }
                if quit_requested {
                    shutdown_for_exit(&mut app).await;
                    break;
                }
                app = apply_pending_actions(
                    app,
                    &mut shell,
                    &mut input,
                    &mut pending_actions,
                    &mut goal_deadline,
                )
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

    fn test_theme() -> crate::tui::theme::YggTheme {
        crate::tui::theme::test_theme()
    }

    #[test]
    fn confirmation_notices_identify_core_tools_and_extensions() {
        assert_eq!(
            confirmation_notice(Some("write"), true),
            "write action approved"
        );
        assert_eq!(
            confirmation_notice(Some("bash"), false),
            "bash action denied"
        );
        assert_eq!(
            confirmation_notice(Some("custom_tool"), true),
            "extension action approved"
        );
        assert_eq!(confirmation_notice(None, false), "tool action denied");
    }

    #[test]
    fn fork_message_projection_uses_the_active_branch_and_adds_a_head_row() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.jsonl");
        let mut session = Session::create(&path).unwrap();
        session
            .append(EntryValue::Message(ygg_ai::Message::User(
                ygg_ai::UserMessage {
                    content: vec![ygg_ai::UserPart::Text("first prompt".into())],
                },
            )))
            .unwrap();
        session
            .append(EntryValue::Message(ygg_ai::Message::Assistant(
                ygg_ai::AssistantMessage {
                    content: vec![ygg_ai::AssistantPart::Text("answer".into())],
                    model: ygg_ai::ModelId("test".into()),
                    protocol: ygg_ai::Protocol::OpenAiChat,
                },
            )))
            .unwrap();
        session
            .append(EntryValue::Message(ygg_ai::Message::User(
                ygg_ai::UserMessage {
                    content: vec![ygg_ai::UserPart::Text("second prompt".into())],
                },
            )))
            .unwrap();

        let messages = active_fork_messages(&session);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].text, "first prompt");
        assert_eq!(messages[1].text, "second prompt");
        assert!(messages[2].whole_conversation);
        assert_eq!(messages[2].entry_id, session.head().unwrap().0);
    }

    #[test]
    fn web_search_menu_recommends_brave_and_keeps_searxng_and_disable() {
        let (items, descriptions) = web_search_menu_entries(true);
        assert_eq!(
            items,
            [
                "Brave Search (recommended)",
                "SearXNG",
                "Disable ygg-web-search"
            ]
        );
        assert!(descriptions[0]
            .as_deref()
            .is_some_and(|description| description.contains("API key")));

        let (items, _) = web_search_menu_entries(false);
        assert_eq!(items, ["Brave Search (recommended)", "SearXNG"]);
    }

    #[test]
    fn delegated_session_overlay_is_bounded_path_free_and_read_only_labeled() {
        let directory = tempfile::tempdir().unwrap();
        let private_path = directory.path().join("private-child.jsonl");
        let mut session = Session::create(&private_path).unwrap();
        session
            .append(EntryValue::Message(ygg_ai::Message::User(
                ygg_ai::UserMessage {
                    content: vec![ygg_ai::UserPart::Text("Inspect the worker result.".into())],
                },
            )))
            .unwrap();
        let text = delegated_session_text(&session, &test_theme(), 80).unwrap();
        // The styled header must still read correctly after ANSI stripping.
        let plain = crate::tui::view::sanitize_for_terminal(&text);
        assert!(plain.contains("Delegated worker transcript"));
        assert!(plain.contains("read-only · mutation remains owner-bound"));
        assert!(text.contains("Inspect the worker result."));
        assert!(!text.contains(private_path.to_str().unwrap()));
        assert!(text.len() <= 128 * 1024);
    }

    #[test]
    fn delegated_session_overlay_keeps_newest_output_within_the_exact_byte_cap() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("child.jsonl")).unwrap();
        for index in 0..20 {
            let marker = if index == 19 {
                "NEWEST-FINAL-WORKER-RESULT"
            } else {
                "older-worker-output"
            };
            session
                .append(EntryValue::Message(ygg_ai::Message::Assistant(
                    ygg_ai::AssistantMessage {
                        content: vec![ygg_ai::AssistantPart::Text(format!(
                            "block-{index:02}-{marker}-{}",
                            "x".repeat(16 * 1024)
                        ))],
                        model: ygg_ai::ModelId("worker-test".into()),
                        protocol: ygg_ai::Protocol::OpenAiChat,
                    },
                )))
                .unwrap();
        }

        let text = delegated_session_text(&session, &test_theme(), 80).unwrap();
        assert!(text.contains("NEWEST-FINAL-WORKER-RESULT"), "{text}");
        assert!(!text.contains("block-00-older-worker-output"));
        assert!(text.contains("[older transcript entries omitted]"));
        assert!(text.len() <= 128 * 1024, "{}", text.len());
    }

    #[test]
    fn delegated_session_renders_markdown_like_the_main_transcript() {
        // The worker transcript must flow through the exact same rich
        // markdown renderer as the main conversation: headings, bold, and
        // inline code keep their theme styling instead of being flattened to
        // raw text.
        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("child.jsonl")).unwrap();
        let markdown = "# Heading One\n\nplain **bold** and `code` tail";
        session
            .append(EntryValue::Message(ygg_ai::Message::Assistant(
                ygg_ai::AssistantMessage {
                    content: vec![ygg_ai::AssistantPart::Text(markdown.into())],
                    model: ygg_ai::ModelId("worker-test".into()),
                    protocol: ygg_ai::Protocol::OpenAiChat,
                },
            )))
            .unwrap();
        let text = delegated_session_text(&session, &test_theme(), 80).unwrap();

        let theme = test_theme();
        let renderer = theme.rich_renderer();
        let expected =
            crate::tui::view::assistant_markdown_document_lines(markdown, &renderer, &theme, 80)
                .join("\n");
        assert!(text.contains(&expected), "styled block not found in {text}");
    }

    #[test]
    fn subagent_presentation_becomes_navigable_rows_with_opaque_session_references() {
        let mut snapshot: ygg_agent::ExtensionPresentationSnapshot =
            serde_json::from_str(include_str!("../../fixtures/extension-presentation.json"))
                .unwrap();
        let collection = snapshot.collection.as_mut().unwrap();
        collection.nodes[0].references = collection.detail.as_ref().unwrap().references.clone();
        let (title, entries) =
            subagent_view_entries_from_presentation(crate::extensions::ExtensionPresentationView {
                extension: "ygg-subagents".into(),
                generation: 1,
                extension_instance_id: "instance".into(),
                resource_owner: Some("owner".into()),
                snapshot,
            })
            .unwrap();

        assert_eq!(title, "1 worker");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].label, "test-review");
        assert!(entries[0].description.contains("running"));
        assert_eq!(
            entries[0].session_reference.as_deref(),
            Some("session-worker-1")
        );
        assert!(entries[0].fallback_detail.contains("bounded child session"));
        assert!(!entries[0].fallback_detail.contains(".jsonl"));
    }

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

        let mut shell = InteractiveShell::test_shell();
        let result = await_with_ctrl_c(std::future::pending::<()>(), &mut shell, &mut input).await;
        assert!(result.is_none());
        assert!(!shell.close_requested());
    }

    #[tokio::test]
    async fn cancellable_wait_finishes_after_input_stream_closes() {
        let mut input = tokio_stream::empty::<std::io::Result<Event>>();
        let mut shell = InteractiveShell::test_shell();
        assert_eq!(
            await_with_ctrl_c(async { 42 }, &mut shell, &mut input).await,
            Some(42)
        );
    }

    #[tokio::test]
    async fn cancellable_wait_propagates_ctrl_d_as_a_close_request() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use tokio_stream::wrappers::ReceiverStream;

        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        sender
            .send(Ok(Event::Key(KeyEvent::new(
                KeyCode::Char('d'),
                KeyModifiers::CONTROL,
            ))))
            .await
            .unwrap();
        drop(sender);
        let mut input = ReceiverStream::new(receiver);
        let mut shell = InteractiveShell::test_shell();

        let result = await_with_ctrl_c(std::future::pending::<()>(), &mut shell, &mut input).await;

        assert!(result.is_none());
        assert!(shell.close_requested());
    }

    #[test]
    fn startup_picker_close_is_a_graceful_exit_but_other_errors_survive() {
        let mut shell = InteractiveShell::test_shell();
        shell.request_close();
        assert_eq!(
            startup_launch_outcome::<u8>(&shell, Err(anyhow::anyhow!("selection cancelled")))
                .unwrap(),
            None
        );

        let shell = InteractiveShell::test_shell();
        let error =
            startup_launch_outcome::<u8>(&shell, Err(anyhow::anyhow!("selection cancelled")))
                .unwrap_err();
        assert_eq!(error.to_string(), "selection cancelled");
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
    fn session_tree_marks_the_durable_head_and_parent_links() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("tree.jsonl")).unwrap();
        let root = session
            .append(EntryValue::Config {
                model: Some("model".to_string()),
                reasoning: Some("off".to_string()),
                reasoning_mode: None,
            })
            .unwrap();
        let child = session
            .append(EntryValue::Config {
                model: None,
                reasoning: Some("high".to_string()),
                reasoning_mode: None,
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
                reasoning_mode: None,
            })
            .unwrap();
        let target = session
            .append(EntryValue::Config {
                model: Some("missing-model".to_string()),
                reasoning: None,
                reasoning_mode: None,
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
                    responses_lite: false,
                    agent_delegation: None,
                    structured_output: false,
                    deferred_tool_loading: false,
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
        use ygg_agent::{
            Agent, AgentConfig, CoreTools, EffectBroker, ExtensionHost, SandboxConfig, Session,
        };

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
            effect_broker: EffectBroker::default(),
            extensions,
            max_turns: Some(4),
            reasoning: ReasoningConfig::Off,
            reasoning_mode: ygg_ai::ReasoningMode::Standard,
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
            &mut false,
        )
        .await
        .unwrap();
        drop(run);

        assert_eq!(ended, HostRunOutcome::Aborted);
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
            &mut false,
        )
        .await
        .unwrap();
        drop(run);

        assert_eq!(ended, HostRunOutcome::Completed);
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
            &mut false,
        )
        .await
        .unwrap();
        drop(run);

        assert_eq!(ended, HostRunOutcome::Aborted);
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

    #[tokio::test]
    async fn active_run_subagents_without_extension_owner_stays_an_unknown_command() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use tokio_stream::wrappers::ReceiverStream;

        let (_server, _workspace, mut agent) = scripted_agent().await;
        let mut shell = InteractiveShell::test_shell();
        let (sender, receiver) = tokio::sync::mpsc::channel(32);
        for character in "/subagents".chars() {
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
            &mut false,
        )
        .await
        .unwrap();
        drop(run);

        assert_eq!(ended, HostRunOutcome::Completed);
        assert_eq!(
            shell.debug_error().as_deref(),
            Some("unknown command: /subagents"),
            "without a ygg-subagents owner the command must not open the live view"
        );
    }
}
