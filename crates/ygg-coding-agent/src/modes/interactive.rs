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

use crate::app::bootstrap::{build_app, Bootstrap, LaunchSelection, SessionSelection};
use crate::app::App;
use crate::commands::{self, Command};
use crate::config::{parse_reasoning, ResumeSelector};
use crate::modes::{timestamp, RunEnded};
use crate::tui::keymap::{self, InputAction};
use crate::tui::theme::load_theme;
use crate::tui::view::InteractiveShell;

const BASE_SYSTEM: &str = "You are ygg, a careful coding agent. Work directly in the workspace, explain important changes concisely, and use tools when they improve accuracy.";

/// Ordered controls sent to the frozen Agent during an active run.
#[derive(Debug)]
enum ControlIntent {
    Steer(String),
    FollowUp(String),
}

/// Reconfiguration work requested while the Agent is active. It is applied
/// only after `Run` is dropped at the next idle boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PendingIdleAction {
    ChangeModel(ModelId),
    ChangeThinking(ReasoningConfig),
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

fn launch_for_interactive(boot: &Bootstrap, stamp: &str) -> anyhow::Result<LaunchSelection> {
    let model = boot.config.model.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "no model configured: pass --model <id>; interactive model selection arrives after startup"
        )
    })?;
    let session = match &boot.config.resume {
        ResumeSelector::New => SessionSelection::CreateNew(boot.sessions.new_path(stamp)),
        ResumeSelector::Continue => SessionSelection::OpenExisting(boot.sessions.latest()?.path),
        ResumeSelector::Resume(Some(id)) => {
            SessionSelection::OpenExisting(boot.sessions.by_id(id)?.path)
        }
        ResumeSelector::Resume(None) => {
            anyhow::bail!("--resume without an id requires the session picker")
        }
    };
    Ok(LaunchSelection { model, session })
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
                    shell.close_overlay();
                    shell.clear_error();
                    shell.render();
                    continue;
                }
                let pending = shell.pending();
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
        Command::Thinking(Some(level)) => {
            PendingIdleAction::ChangeThinking(parse_reasoning(&level)?)
        }
        Command::Thinking(None) => PendingIdleAction::PickThinking,
        Command::New => PendingIdleAction::NewSession,
        Command::Resume(id) => PendingIdleAction::ResumeSession(id),
        Command::Compact => PendingIdleAction::Compact,
        other => anyhow::bail!("{other:?} cannot be queued as an idle action"),
    };
    push_pending_action(queue, action);
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
            shell.show_overlay_text(
                "Run active. Model and session status remains visible in the status bar.".into(),
            );
        }
        Command::Help => shell.show_overlay_text(commands::help_text()),
        Command::Theme(name) => match name {
            Some(name) => shell.notice(format!(
                "theme change requested: {name} (applied when theme support is configured)"
            )),
            None => shell.notice("theme picker requested (available after theme discovery)"),
        },
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
    let mut in_flight: Option<Pin<Box<dyn Future<Output = Result<(), AgentError>>>>> = None;

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
                    shell.close_overlay();
                    shell.clear_error();
                    shell.render();
                    continue;
                }
                let pending = shell.pending();
                match keymap::translate(Some(event), true, &pending) {
                    InputAction::Abort => {
                        control.abort();
                        shell.set_run_label("aborting…");
                        shell.render();
                    }
                    InputAction::Steer(_) => {
                        let text = shell.drain_editor();
                        if !text.is_empty() {
                            intents.push_back(ControlIntent::Steer(text));
                        }
                        shell.render();
                    }
                    InputAction::FollowUp(_) => {
                        let text = shell.drain_editor();
                        if !text.is_empty() {
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

async fn apply_pending_actions_stub(
    shell: &mut InteractiveShell,
    pending_actions: &mut VecDeque<PendingIdleAction>,
) {
    while let Some(action) = pending_actions.pop_front() {
        shell.notice(format!("pending action reached idle boundary: {action:?}"));
    }
    shell.render();
}

async fn run_idle_command(
    app: &mut App,
    shell: &mut InteractiveShell,
    command: Command,
) -> anyhow::Result<bool> {
    match command {
        Command::Status => {
            shell.show_overlay_text(format!(
                "model: {}\nthinking: {:?}\nworkspace: {}\nsession: {}",
                app.model.spec.id.0,
                app.reasoning,
                app.config.workspace.display(),
                app.agent.session().path().display(),
            ));
        }
        Command::Help => shell.show_overlay_text(commands::help_text()),
        Command::Quit => return Ok(true),
        Command::Unknown(text) => shell.error(format!("unknown command: {text}")),
        command => shell.notice(format!(
            "{command:?} is available once runtime reconfiguration is initialized"
        )),
    }
    shell.render();
    Ok(false)
}

/// Run the interactive frontend with explicit idle and active borrow phases.
pub async fn run_interactive(boot: Bootstrap) -> anyhow::Result<()> {
    let initial_prompt = boot.config.initial_prompt.clone();
    let theme = load_theme(&boot.config);
    let size = Rc::new(Cell::new(crossterm::terminal::size().unwrap_or((80, 24))));
    let mut shell = InteractiveShell::enter(theme, size)?;
    let mut input = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(80));

    let launch = launch_for_interactive(&boot, &timestamp())?;
    let mut app = build_app(boot, launch, BASE_SYSTEM.to_owned())?;
    shell.hydrate(app.agent.session())?;
    shell.set_status(&format!(
        "{} · {:?} · {}",
        app.model.spec.id.0,
        app.reasoning,
        app.config.workspace.display()
    ));
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
            Idle::Command(input) => {
                if run_idle_command(&mut app, &mut shell, commands::parse(&input)).await? {
                    break;
                }
            }
            Idle::Submit(prompt) => {
                let mut run = app.agent.prompt(prompt).await?;
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
                shell.set_run_label(&format!("run: {ended:?}"));
                apply_pending_actions_stub(&mut shell, &mut pending_actions).await;
                if quit_requested {
                    break;
                }
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
            Some(PendingIdleAction::ChangeThinking(_))
        ));
        assert_eq!(
            queue.pop_front(),
            Some(PendingIdleAction::ResumeSession(Some("id".into())))
        );
    }
}
