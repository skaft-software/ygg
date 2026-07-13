#![allow(missing_docs)]

use std::cell::Cell;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use crossterm::event::{Event, EventStream};
use futures_util::{Stream, StreamExt};
use tokio::time::Interval;
use ygg_agent::{AgentError, AgentEvent, Run, RunControl};
use ygg_ai::{ModelId, ReasoningConfig};

use crate::app::bootstrap::{build_app, resolve_launch_interactive, Bootstrap};
use crate::app::{apply_reconfig, supported_levels, thinking_to_reasoning, App, Reconfig};
use crate::commands::{self, Command};
use crate::compaction::{
    attempt_compaction, ensure_capacity_before_prompt, estimate_next_request_tokens,
    hard_input_budget, CapacityDecision, CompactionOutcome,
};
use crate::config::ThinkingLevel;
use crate::modes::RunEnded;
use crate::resources::compose_instructions;
use crate::tui::keymap::{self, InputAction};
use crate::tui::pickers::{model_picker, session_picker, theme_picker, thinking_picker};
use crate::tui::theme::{available_themes, load_named_theme, load_theme};
use crate::tui::view::InteractiveShell;

/// Ordered controls sent to the frozen Agent during an active run.
#[derive(Debug)]
enum ControlIntent {
    Steer(String),
    FollowUp(String),
}

type ControlFuture = Pin<Box<dyn Future<Output = Result<(), AgentError>>>>;

/// Reconfiguration work requested while the Agent is active. It is applied
/// only after `Run` is dropped at the next idle boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PendingIdleAction {
    ChangeModel(ModelId),
    ChangeThinking(ReasoningConfig),
    ChangeThinkingLevel(ThinkingLevel),
    PickModel,
    PickThinking,
    NewSession,
    ResumeSession(Option<String>),
    Compact,
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
        )
    );
    if same_kind {
        let _ = queue.pop_back();
    }
    queue.push_back(action);
}

#[derive(Debug)]
enum Idle {
    Submit(String),
    Command(String),
    Quit,
}

async fn wait_for_prompt<S>(
    shell: &mut InteractiveShell,
    input: &mut S,
    ticker: &mut Interval,
) -> anyhow::Result<Idle>
where
    S: Stream<Item = std::io::Result<Event>> + Unpin,
{
    loop {
        tokio::select! {
            maybe = input.next() => {
                let event = match maybe {
                    Some(Ok(event)) => event,
                    Some(Err(error)) => return Err(error.into()),
                    None => return Ok(Idle::Quit),
                };
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
                match keymap::translate(Some(event), false, &pending) {
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
                        shell.render();
                    }
                    InputAction::Close => {
                        shell.clear_error();
                        shell.render();
                    }
                    InputAction::Submit(_) => return Ok(Idle::Submit(shell.drain_editor())),
                    InputAction::Command(_) => return Ok(Idle::Command(shell.drain_editor())),
                    InputAction::Closed => return Ok(Idle::Quit),
                    InputAction::Ignore
                    | InputAction::Abort
                    | InputAction::Steer(_)
                    | InputAction::FollowUp(_) => {}
                }
            }
            _ = ticker.tick() => shell.render(),
        }
    }
}

fn queue_command(command: Command, queue: &mut VecDeque<PendingIdleAction>) -> anyhow::Result<()> {
    let action = match command {
        Command::Model(Some(id)) => PendingIdleAction::ChangeModel(ModelId(id)),
        Command::Model(None) => PendingIdleAction::PickModel,
        Command::Thinking(Some(level)) => match ThinkingLevel::parse(&level)? {
            ThinkingLevel::Off => PendingIdleAction::ChangeThinking(ReasoningConfig::Off),
            level => PendingIdleAction::ChangeThinkingLevel(level),
        },
        Command::Thinking(None) => PendingIdleAction::PickThinking,
        Command::New => PendingIdleAction::NewSession,
        Command::Resume(id) => PendingIdleAction::ResumeSession(id),
        Command::Compact => PendingIdleAction::Compact,
        other => anyhow::bail!("{other:?} cannot be queued as an idle action"),
    };
    push_pending_action(queue, action);
    Ok(())
}

fn apply_theme(shell: &mut InteractiveShell, name: &str) -> anyhow::Result<()> {
    let config = shell
        .theme_config()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("theme configuration is unavailable"))?;
    let theme = load_named_theme(name, &config)?;
    shell.set_theme(theme);
    shell.notice(format!("theme changed to {name}"));
    Ok(())
}

fn active_theme_choices(shell: &mut InteractiveShell) {
    match shell.theme_config() {
        Some(config) => {
            let names = available_themes(config);
            if names.is_empty() {
                shell.notice("no themes found under .ygg/themes or ~/.ygg/themes");
            } else {
                shell.show_overlay_text(format!(
                    "Available themes:\n{}\n\nUse /theme <name> while a run is active.",
                    names.join("\n")
                ));
            }
        }
        None => shell.error("theme configuration is unavailable".into()),
    }
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
            shell.show_overlay_text(status);
        }
        Command::Help => shell.show_overlay_text(commands::help_text()),
        Command::Theme(Some(name)) => {
            if let Err(error) = apply_theme(shell, &name) {
                shell.error(error.to_string());
            }
        }
        Command::Theme(None) => active_theme_choices(shell),
        Command::Quit => *quit_requested = true,
        Command::Unknown(text) => shell.error(format!("unknown command: {text}")),
        command => match queue_command(command, queue) {
            Ok(()) => shell.notice("command queued for the next idle boundary"),
            Err(error) => shell.error(error.to_string()),
        },
    }
    shell.render();
}

/// Drive one active frozen-Agent run. The input arm only queues sends, so a
/// full control channel can never stop the run stream from making progress.
pub async fn drive_active_run<S>(
    run: &mut Run<'_>,
    control: &RunControl,
    shell: &mut InteractiveShell,
    input: &mut S,
    ticker: &mut Interval,
    pending_actions: &mut VecDeque<PendingIdleAction>,
    quit_requested: &mut bool,
) -> anyhow::Result<RunEnded>
where
    S: Stream<Item = std::io::Result<Event>> + Unpin,
{
    let mut intents = VecDeque::<ControlIntent>::new();
    let mut in_flight: Option<ControlFuture> = None;

    loop {
        if in_flight.is_none() {
            if let Some(intent) = intents.pop_front() {
                let control = control.clone();
                in_flight = Some(Box::pin(async move {
                    match intent {
                        ControlIntent::Steer(text) => control.steer(text).await,
                        ControlIntent::FollowUp(text) => control.follow_up(text).await,
                    }
                }));
            }
        }

        tokio::select! {
            biased;
            result = async { in_flight.as_mut().expect("guarded by select condition").await }, if in_flight.is_some() => {
                // A run may have ended before a pending control was delivered.
                // That error is harmless; no detached send survives this loop.
                let _ = result;
                in_flight = None;
            }
            maybe = input.next() => {
                let event = match maybe {
                    Some(Ok(event)) => event,
                    Some(Err(error)) => return Err(error.into()),
                    None => {
                        control.abort();
                        *quit_requested = true;
                        continue;
                    }
                };
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
                match keymap::translate(Some(event), true, &pending) {
                    InputAction::Abort => {
                        control.abort();
                        shell.set_run_label("aborting…");
                        shell.render();
                    }
                    InputAction::Steer(_) => {
                        let text = shell.drain_editor();
                        if !text.is_empty() {
                            shell.on_prompt_submitted(&text);
                            intents.push_back(ControlIntent::Steer(text));
                        }
                        shell.render();
                    }
                    InputAction::FollowUp(_) => {
                        let text = shell.drain_editor();
                        if !text.is_empty() {
                            shell.on_prompt_submitted(&text);
                            intents.push_back(ControlIntent::FollowUp(text));
                        }
                        shell.render();
                    }
                    InputAction::Command(_) => {
                        let command = commands::parse(&shell.drain_editor());
                        let was_quit = matches!(command, Command::Quit);
                        handle_active_command(shell, command, pending_actions, quit_requested);
                        if was_quit {
                            control.abort();
                            shell.set_run_label("quitting…");
                            shell.render();
                        }
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
                        shell.render();
                    }
                    InputAction::Close => {
                        shell.clear_error();
                        shell.render();
                    }
                    InputAction::Closed => {
                        control.abort();
                        *quit_requested = true;
                    }
                    InputAction::Ignore | InputAction::Submit(_) => {}
                }
            }
            event = run.next() => match event {
                Some(AgentEvent::RunFinished { reason, .. }) => {
                    let ended = RunEnded::from(reason);
                    shell.set_run_label(&format!("run: {ended:?}"));
                    shell.render();
                    return Ok(ended);
                }
                Some(AgentEvent::TurnFinished { usage, .. }) => {
                    shell.on_turn_finished(&usage);
                    shell.set_run_label("turn complete");
                    shell.render();
                }
                Some(event) => {
                    shell.on_agent_event(&event);
                    shell.render();
                }
                None => return Ok(RunEnded::Aborted),
            },
            _ = ticker.tick() => shell.render(),
        }
    }
}

fn update_status(shell: &mut InteractiveShell, app: &App) {
    shell.set_status(&format!(
        "{} · {} · {}",
        app.model.spec.id.0,
        crate::app::reasoning_label(&app.reasoning),
        app.config.workspace.display()
    ));
    shell.set_status_detail(commands::status_text(app, None));
    shell.set_context_estimate(
        estimate_next_request_tokens(app, ""),
        hard_input_budget(&app.model),
    );
}

fn report_compaction(shell: &mut InteractiveShell, outcome: &CompactionOutcome) {
    match outcome {
        CompactionOutcome::NotNeeded => {}
        CompactionOutcome::Compacted { elided } => {
            shell.compaction_marker(format!("summarized {elided} earlier messages"));
        }
        CompactionOutcome::Skipped { reason } => {
            shell.notice(format!("compaction skipped: {reason}"))
        }
    }
}

fn transition(app: App, shell: &mut InteractiveShell, reconfig: Reconfig) -> anyhow::Result<App> {
    let app = apply_reconfig(app, reconfig)?;
    shell.hydrate(app.agent.session())?;
    update_status(shell, &app);
    Ok(app)
}

async fn apply_pending_actions(
    mut app: App,
    shell: &mut InteractiveShell,
    input: &mut EventStream,
    pending_actions: &mut VecDeque<PendingIdleAction>,
) -> anyhow::Result<App> {
    while let Some(action) = pending_actions.pop_front() {
        match action {
            PendingIdleAction::ChangeModel(id) => {
                app = transition(app, shell, Reconfig::Model(id))?;
                shell.notice("queued model change applied");
            }
            PendingIdleAction::ChangeThinking(reasoning) => {
                app = transition(app, shell, Reconfig::Thinking(reasoning))?;
                shell.notice("queued thinking change applied");
            }
            PendingIdleAction::ChangeThinkingLevel(level) => {
                let reasoning = thinking_to_reasoning(level, &app.model)?;
                app = transition(app, shell, Reconfig::Thinking(reasoning))?;
                shell.notice("queued thinking change applied");
            }
            PendingIdleAction::NewSession => {
                app = transition(app, shell, Reconfig::NewSession)?;
                shell.notice("queued new session created");
            }
            PendingIdleAction::ResumeSession(Some(id)) => {
                let path = app.sessions.by_id(&id)?.path;
                app = transition(app, shell, Reconfig::Resume(path))?;
                shell.notice("queued session resumed");
            }
            PendingIdleAction::ResumeSession(None) => {
                if let Some(path) = session_picker(shell, input, &app.sessions).await? {
                    app = transition(app, shell, Reconfig::Resume(path))?;
                    shell.notice("queued session resumed");
                }
            }
            PendingIdleAction::Compact => {
                shell.set_run_label("compacting…");
                shell.render();
                let outcome = attempt_compaction(&mut app).await?;
                report_compaction(shell, &outcome);
                update_status(shell, &app);
            }
            PendingIdleAction::PickModel => {
                let model = model_picker(shell, input, &app.catalog).await?;
                app = transition(app, shell, Reconfig::Model(model))?;
                shell.notice("queued model change applied");
            }
            PendingIdleAction::PickThinking => {
                let level = thinking_picker(shell, input, &supported_levels(&app.model)).await?;
                let reasoning = thinking_to_reasoning(level, &app.model)?;
                app = transition(app, shell, Reconfig::Thinking(reasoning))?;
                shell.notice("queued thinking change applied");
            }
        }
        shell.render();
    }
    Ok(app)
}

enum IdleCommandOutcome {
    Continue(Box<App>),
    Quit,
}

async fn run_idle_command(
    mut app: App,
    shell: &mut InteractiveShell,
    input: &mut EventStream,
    command: Command,
) -> anyhow::Result<IdleCommandOutcome> {
    match command {
        Command::Status => shell.show_overlay_text(commands::status_text(&app, None)),
        Command::Help => shell.show_overlay_text(commands::help_text()),
        Command::Quit => return Ok(IdleCommandOutcome::Quit),
        Command::New => {
            app = transition(app, shell, Reconfig::NewSession)?;
            shell.notice("created a new session");
        }
        Command::Resume(Some(id)) => {
            let path = app.sessions.by_id(&id)?.path;
            app = transition(app, shell, Reconfig::Resume(path))?;
            shell.notice("resumed session");
        }
        Command::Resume(None) => {
            if let Some(path) = session_picker(shell, input, &app.sessions).await? {
                app = transition(app, shell, Reconfig::Resume(path))?;
                shell.notice("resumed session");
            }
        }
        Command::Model(Some(id)) => {
            app = transition(app, shell, Reconfig::Model(ModelId(id)))?;
            shell.notice("model changed");
        }
        Command::Thinking(Some(level)) => {
            let level = ThinkingLevel::parse(&level)?;
            let reasoning = thinking_to_reasoning(level, &app.model)?;
            app = transition(app, shell, Reconfig::Thinking(reasoning))?;
            shell.notice("thinking changed");
        }
        Command::Model(None) => {
            let model = model_picker(shell, input, &app.catalog).await?;
            app = transition(app, shell, Reconfig::Model(model))?;
            shell.notice("model changed");
        }
        Command::Thinking(None) => {
            let level = thinking_picker(shell, input, &supported_levels(&app.model)).await?;
            let reasoning = thinking_to_reasoning(level, &app.model)?;
            app = transition(app, shell, Reconfig::Thinking(reasoning))?;
            shell.notice("thinking changed");
        }
        Command::Theme(Some(name)) => {
            if let Err(error) = apply_theme(shell, &name) {
                shell.error(error.to_string());
            }
        }
        Command::Theme(None) => {
            let config = shell
                .theme_config()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("theme configuration is unavailable"))?;
            let names = available_themes(&config);
            let name = theme_picker(shell, input, &names).await?;
            if let Err(error) = apply_theme(shell, &name) {
                shell.error(error.to_string());
            }
        }
        Command::Compact => {
            shell.set_run_label("compacting…");
            shell.render();
            let outcome = attempt_compaction(&mut app).await?;
            report_compaction(shell, &outcome);
            update_status(shell, &app);
        }
        Command::Unknown(text) => shell.error(format!("unknown command: {text}")),
    }
    shell.render();
    Ok(IdleCommandOutcome::Continue(Box::new(app)))
}

/// Run the interactive frontend with explicit idle and active borrow phases.
pub async fn run_interactive(boot: Bootstrap) -> anyhow::Result<()> {
    let initial_prompt = boot.config.initial_prompt.clone();
    let theme = load_theme(&boot.config);
    let size = Rc::new(Cell::new(crossterm::terminal::size().unwrap_or((80, 24))));
    let mut shell = InteractiveShell::enter(theme, size)?;
    shell.set_theme_config(boot.config.clone());
    let mut input = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(80));

    let launch = resolve_launch_interactive(&boot, &mut shell, &mut input).await?;
    let system = compose_instructions(&boot.config)?;
    let mut app = build_app(boot, launch, system)?;
    shell.hydrate(app.agent.session())?;
    update_status(&mut shell, &app);
    shell.render();

    let mut pending_actions = VecDeque::new();
    let mut startup_prompt = initial_prompt;
    loop {
        let idle = match startup_prompt.take() {
            Some(prompt) if !prompt.is_empty() => Idle::Submit(prompt),
            _ => wait_for_prompt(&mut shell, &mut input, &mut ticker).await?,
        };
        match idle {
            Idle::Quit => break,
            Idle::Command(command_input) => {
                match run_idle_command(app, &mut shell, &mut input, commands::parse(&command_input))
                    .await?
                {
                    IdleCommandOutcome::Continue(next) => app = *next,
                    IdleCommandOutcome::Quit => break,
                }
            }
            Idle::Submit(prompt) => {
                shell.set_run_label("checking context…");
                shell.render();
                match ensure_capacity_before_prompt(&mut app, &prompt).await? {
                    CapacityDecision::Proceed(outcome) => {
                        report_compaction(&mut shell, &outcome);
                        shell.set_context_estimate(
                            estimate_next_request_tokens(&app, &prompt),
                            hard_input_budget(&app.model),
                        );
                    }
                    CapacityDecision::Exceeded { estimate, budget } => {
                        shell.error(format!(
                            "prompt too large: ~{estimate} tokens exceeds the {budget}-token budget even after compaction — shorten it or start a new session"
                        ));
                        shell.set_run_label("idle");
                        shell.render();
                        continue;
                    }
                }
                let mut run = app.agent.prompt(prompt.clone()).await?;
                shell.on_prompt_submitted(&prompt);
                shell.render();
                let control = run.control();
                let mut quit_requested = false;
                let ended = drive_active_run(
                    &mut run,
                    &control,
                    &mut shell,
                    &mut input,
                    &mut ticker,
                    &mut pending_actions,
                    &mut quit_requested,
                )
                .await?;
                drop(run);
                update_status(&mut shell, &app);
                shell.set_run_label(&format!("run: {ended:?}"));
                if quit_requested {
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
        queue_command(Command::Thinking(Some("high".into())), &mut queue).unwrap();
        queue_command(Command::Resume(Some("id".into())), &mut queue).unwrap();
        assert!(matches!(
            queue.pop_front(),
            Some(PendingIdleAction::ChangeThinkingLevel(ThinkingLevel::High))
        ));
        assert_eq!(
            queue.pop_front(),
            Some(PendingIdleAction::ResumeSession(Some("id".into())))
        );
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
            Auth, Capabilities, Endpoint, EndpointId, ModalitySet, ModelLimits, ModelSpec, Protocol,
        };

        ygg_ai::Model {
            spec: Arc::new(ModelSpec {
                id: ModelId("scripted".into()),
                endpoint: EndpointId("test".into()),
                api_name: "scripted".into(),
                protocol: Protocol::AnthropicMessages,
                capabilities: Capabilities {
                    input_modalities: ModalitySet::none(),
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
            }),
            endpoint: Arc::new(Endpoint {
                id: EndpointId("test".into()),
                base_url: url::Url::parse(&format!("{uri}/v1/")).unwrap(),
                auth: Auth::None,
                default_headers: http::HeaderMap::new(),
                timeout: Duration::from_secs(5),
            }),
        }
    }

    #[tokio::test]
    async fn scripted_active_loop_queues_controls_and_never_forwards_active_model_command() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use tokio_stream::wrappers::ReceiverStream;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        use ygg_agent::{Agent, AgentConfig, CoreTools, ExtensionHost, SandboxConfig, Session};
        use ygg_ai::AiClient;

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
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
        let mut agent = Agent::new(AgentConfig {
            client: AiClient::new(),
            model: scripted_model(&server.uri()),
            session: Session::create(&session_path).unwrap(),
            system: "test".into(),
            sandbox,
            extensions,
            max_turns: 4,
            reasoning: ReasoningConfig::Off,
        })
        .unwrap();

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
        let mut run = agent.prompt("initial").await.unwrap();
        let control = run.control();
        let ended = drive_active_run(
            &mut run,
            &control,
            &mut shell,
            &mut input,
            &mut ticker,
            &mut pending,
            &mut quit,
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
    }
}
