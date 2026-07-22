#![allow(missing_docs)]

//! Chronological, cursor-free fallback for dumb, redirected, unknown, and
//! explicitly plain terminals. Explicit `--print` remains the raw response-only
//! scripting surface; this mode preserves the interactive product's execution
//! structure without ANSI cursor control.

use std::collections::HashMap;
use std::io::{BufRead, IsTerminal, Read, Write};
use std::time::Duration;

use tokio::time::MissedTickBehavior;
use ygg_agent::{AgentEvent, OutputChannel};
use ygg_ai::ToolCallId;

use crate::app::bootstrap::{build_app, resolve_launch_print, Bootstrap};
use crate::modes::{timestamp, RunEnded};
use crate::presentation::{
    format_duration, is_hidden_tool_detail, summarize_tool_with_workspace, tool_failure_reason,
    tool_result_is_failure, RunOutcome, RunPhase, RunTracker,
};
use crate::resources::compose_instructions;
use crate::tui::theme::YggTheme;

fn safe_text(raw: &str) -> String {
    let mut safe = String::with_capacity(raw.len());
    let mut characters = raw.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\r' if characters.peek() == Some(&'\n') => {
                characters.next();
                safe.push('\n');
            }
            '\n' | '\t' => safe.push(character),
            value if value.is_control() => safe.push('·'),
            value => safe.push(value),
        }
    }
    safe
}

fn style_log(theme: &YggTheme, line: &str) -> String {
    let line = safe_text(line);
    if line.starts_with("[working]") {
        theme.fg("model_accent", &line)
    } else if line.starts_with("[failed]") || line.contains(" failed") {
        theme.fg("error", &line)
    } else if line.starts_with("[needs input]") || line.starts_with("[completed with warnings]") {
        theme.fg("warning", &line)
    } else {
        line
    }
}

fn write_log(
    output: &mut impl Write,
    response_open: &mut bool,
    theme: &YggTheme,
    line: &str,
) -> std::io::Result<()> {
    if *response_open {
        writeln!(output)?;
        *response_open = false;
    }
    writeln!(output, "{}", style_log(theme, line))
}

fn write_prompt(output: &mut impl Write, theme: &YggTheme, prompt: &str) -> std::io::Result<()> {
    for (index, line) in prompt.lines().enumerate() {
        let marker = if index == 0 {
            theme.fg("model_accent", ">")
        } else {
            " ".into()
        };
        writeln!(output, "{marker} {}", safe_text(line))?;
    }
    if prompt.is_empty() {
        writeln!(output, "{}", theme.fg("model_accent", ">"))?;
    }
    Ok(())
}

fn outcome_text(outcome: &RunOutcome) -> String {
    match outcome {
        RunOutcome::Completed { elapsed, summary } => {
            let mut parts = vec![format!("[completed] {}", format_duration(*elapsed))];
            if summary.files_changed > 0 {
                parts.push(format!(
                    "{} file{} changed",
                    summary.files_changed,
                    if summary.files_changed == 1 { "" } else { "s" }
                ));
            }
            parts.join(" - ")
        }
        RunOutcome::CompletedWithWarnings {
            elapsed, warnings, ..
        } => format!(
            "[completed with warnings] {} warning{} - {}",
            warnings,
            if *warnings == 1 { "" } else { "s" },
            format_duration(*elapsed)
        ),
        RunOutcome::Failed { elapsed, reason } => {
            format!("[failed] {reason} - {}", format_duration(*elapsed))
        }
        RunOutcome::Interrupted { elapsed } => {
            format!("[interrupted] {}", format_duration(*elapsed))
        }
        RunOutcome::NeedsInput { prompt } => format!("[needs input] {prompt}"),
        RunOutcome::Cancelled { elapsed } => {
            format!("[cancelled] {}", format_duration(*elapsed))
        }
    }
}

async fn run_prompt(
    app: &mut crate::app::App,
    prompt: String,
    output: &mut impl Write,
    theme: &YggTheme,
    tracker: &mut RunTracker,
) -> anyhow::Result<()> {
    let prompt = match crate::prompts::render_configured(app, &prompt)? {
        Some(rendered) => {
            if app.config.debug_prompt {
                writeln!(output, "{}", crate::prompts::debug_expansion(&rendered))?;
            }
            rendered.text
        }
        None => prompt,
    };
    write_prompt(output, theme, &prompt)?;
    let run_id = tracker
        .begin(&app.model.endpoint.id.0)
        .expect("fresh tracker cannot have an active run");

    if let Some(limit) = app.config.max_cost_microdollars {
        if app.agent.session().total_cost_microdollars() >= limit {
            let outcome = tracker
                .fail(
                    run_id,
                    format!(
                        "Session cost limit of {} reached.",
                        crate::commands::format_microdollars_cents(limit)
                    ),
                )
                .expect("active run");
            writeln!(output, "{}", style_log(theme, &outcome_text(&outcome)))?;
            output.flush()?;
            return Ok(());
        }
    }

    // Capacity checks and compaction happen inside the cancellable Agent run.
    if let Some(limit) = app.config.max_cost_microdollars {
        if app.agent.session().total_cost_microdollars() >= limit {
            let outcome = tracker
                .fail(
                    run_id,
                    format!(
                        "Session cost limit of {} reached.",
                        crate::commands::format_microdollars_cents(limit)
                    ),
                )
                .expect("active run");
            writeln!(output, "{}", style_log(theme, &outcome_text(&outcome)))?;
            output.flush()?;
            return Ok(());
        }
    }

    app.executable_extensions.refresh_host_state(
        app.agent.session(),
        &app.model,
        &app.reasoning,
        &app.sessions,
    );
    let composition = app
        .executable_extensions
        .compose_prompt(&app.system, prompt.clone())
        .await?;
    let pending_context_count = composition.pending_context_count;
    for notification in composition.notifications {
        writeln!(
            output,
            "{}",
            style_log(theme, &format!("[extension] {notification}"))
        )?;
    }
    app.agent.set_system_prompt(composition.system);
    app.agent.set_prompt_display_text(Some(prompt));
    let mut run = match app.agent.prompt(composition.prompt).await {
        Ok(run) => run,
        Err(error) => {
            // Pending extension context remains uncommitted. A later TTY
            // prompt recomposes from `app.system` before touching the Agent.
            let outcome = tracker.fail(run_id, error.to_string()).expect("active run");
            writeln!(output, "{}", style_log(theme, &outcome_text(&outcome)))?;
            output.flush()?;
            return Ok(());
        }
    };
    app.executable_extensions
        .commit_prompt_context(pending_context_count);
    tracker.awaiting_provider(run_id);
    let control = run.control();
    let mut last_run_cost = 0u64;
    writeln!(
        output,
        "{}",
        style_log(
            theme,
            &format!("[working] Waiting for {}", app.model.endpoint.id.0)
        )
    )?;
    output.flush()?;

    let mut tools: HashMap<ToolCallId, (String, serde_json::Value)> = HashMap::new();
    let mut response_open = false;
    let mut last_phase = tracker.current().map(|run| run.phase().clone());
    let mut heartbeat = tokio::time::interval(Duration::from_secs(5));
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);
    heartbeat.tick().await;
    let mut response_text = String::new();
    let mut shutdown_requested = false;
    let mut finished = None;

    loop {
        tokio::select! {
            biased;
            _ = crate::tui::terminal::wait_for_shutdown_signal() => {
                control.abort();
                ygg_agent::extension_process::terminate_exec_process_groups(
                    Duration::from_millis(400),
                )
                .await;
                shutdown_requested = true;
                break;
            }
            event = run.next() => {
                let Some(event) = event else {
                    let outcome = tracker
                        .fail(run_id, "run stream ended without a final outcome")
                        .expect("active run");
                    write_log(output, &mut response_open, theme, &outcome_text(&outcome))?;
                    break;
                };
                let update = tracker.apply_event(run_id, &event);
                match &event {
                    AgentEvent::OutputDelta { channel: OutputChannel::Text, text } => {
                        write!(output, "{}", safe_text(text))?;
                        response_open = !text.ends_with('\n');
                    }
                    AgentEvent::OutputDelta { channel: OutputChannel::Reasoning, .. } => {
                        if !matches!(last_phase, Some(RunPhase::Thinking)) {
                            write_log(output, &mut response_open, theme, "[working] Thinking")?;
                        }
                    }
                    AgentEvent::ProviderRetry { attempt, max_attempts, .. } => {
                        write_log(
                            output,
                            &mut response_open,
                            theme,
                            &format!(
                                "[retry] provider stream interrupted; discarding partial response and retrying ({attempt}/{max_attempts})"
                            ),
                        )?;
                    }
                    AgentEvent::ToolStarted { id, name, args } => {
                        let display = summarize_tool_with_workspace(
                            name,
                            args,
                            Some(&app.config.workspace),
                        );
                        write_log(
                            output,
                            &mut response_open,
                            theme,
                            &format!("[working] {}", display.active),
                        )?;
                        tools.insert(id.clone(), (name.clone(), args.clone()));
                    }
                    AgentEvent::ToolProgress { .. } => {}
                    AgentEvent::ToolFinished { id, result } => {
                        let (name, args) = tools
                            .remove(id)
                            .unwrap_or_else(|| ("tool".into(), serde_json::Value::Null));
                        let display = summarize_tool_with_workspace(
                            &name,
                            &args,
                            Some(&app.config.workspace),
                        );
                        if tool_result_is_failure(&name, result) {
                            let reason = tool_failure_reason(&name, result)
                                .unwrap_or_else(|| "tool failed".into());
                            write_log(
                                output,
                                &mut response_open,
                                theme,
                                &format!("[{}] {} - {reason}", display.plain_tag, display.failure),
                            )?;
                            if let Err(error) = result {
                                for line in error
                                    .message
                                    .lines()
                                    .skip(1)
                                    .filter(|line| !is_hidden_tool_detail(line))
                                    .take(12)
                                {
                                    write_log(
                                        output,
                                        &mut response_open,
                                        theme,
                                        &format!("  {}", safe_text(line)),
                                    )?;
                                }
                            }
                        } else {
                            write_log(
                                output,
                                &mut response_open,
                                theme,
                                &format!("[{}] {}", display.plain_tag, display.success),
                            )?;
                        }
                    }
                    AgentEvent::TurnFinished {
                        message,
                        session_cost_microdollars,
                        run_cost_microdollars,
                        ..
                    } => {
                        response_text.clear();
                        response_text.push_str(&crate::extensions::assistant_text(message));
                        let turn_cost = run_cost_microdollars.saturating_sub(last_run_cost);
                        if app
                            .config
                            .cost_warning_microdollars
                            .is_some_and(|threshold| turn_cost >= threshold)
                        {
                            write_log(
                                output,
                                &mut response_open,
                                theme,
                                &format!(
                                    "[warning] turn cost {} reached the {} threshold",
                                    crate::commands::format_microdollars(turn_cost),
                                    crate::commands::format_microdollars_cents(
                                        app.config.cost_warning_microdollars.unwrap_or_default()
                                    )
                                ),
                            )?;
                        }
                        last_run_cost = *run_cost_microdollars;
                        if let (Some(limit), Some(total)) =
                            (app.config.max_cost_microdollars, *session_cost_microdollars)
                        {
                            if total >= limit {
                                write_log(
                                    output,
                                    &mut response_open,
                                    theme,
                                    &format!(
                                        "[failed] Session cost limit of {} reached.",
                                        crate::commands::format_microdollars_cents(limit)
                                    ),
                                )?;
                                control.abort();
                            }
                        }
                    }
                    AgentEvent::SteeringDelivered { .. } => {}
                    AgentEvent::RunFinished { reason, .. } => {
                        finished = Some(match reason {
                            ygg_agent::FinishReason::Completed => RunEnded::Completed,
                            ygg_agent::FinishReason::Aborted => RunEnded::Aborted,
                            ygg_agent::FinishReason::MaxTurns => RunEnded::MaxTurns,
                            ygg_agent::FinishReason::Failed(error) => {
                                RunEnded::Failed(error.to_string())
                            }
                        });
                        if let Some(outcome) = update.outcome {
                            write_log(output, &mut response_open, theme, &outcome_text(&outcome))?;
                        }
                        break;
                    }
                }
                last_phase = tracker.current().map(|current| current.phase().clone());
                output.flush()?;
            }
            _ = heartbeat.tick(), if tracker.is_active() => {
                if let Some(current) = tracker.current() {
                    let label = match current.phase() {
                        RunPhase::Preparing { summary } => summary.clone(),
                        RunPhase::AwaitingProvider { provider } => format!("waiting for {provider}"),
                        RunPhase::Thinking => "thinking".into(),
                        RunPhase::StreamingResponse => "writing response".into(),
                        RunPhase::PreparingToolCall => "preparing tool call".into(),
                        RunPhase::RunningTool { summary } => summary.clone(),
                        RunPhase::AwaitingApproval { prompt } => format!("approval required - {prompt}"),
                        RunPhase::Finished(_) => continue,
                    };
                    write_log(
                        output,
                        &mut response_open,
                        theme,
                        &format!("[working] {label} - {}", format_duration(current.phase_elapsed_at(std::time::Instant::now()))),
                    )?;
                    output.flush()?;
                }
            }
        }
    }
    drop(run);
    app.agent.set_system_prompt(app.system.clone());
    if shutdown_requested {
        let _ = tokio::time::timeout(
            Duration::from_millis(1400),
            app.executable_extensions.shutdown(),
        )
        .await;
        ygg_agent::extension_process::force_kill_registered_process_groups();
        output.flush()?;
        return Ok(());
    }
    if matches!(finished, Some(RunEnded::Completed)) {
        for notification in app
            .executable_extensions
            .after_response(&response_text)
            .await
        {
            write_log(
                output,
                &mut response_open,
                theme,
                &format!("[extension] {notification}"),
            )?;
        }
    }
    writeln!(output)?;
    output.flush()?;
    Ok(())
}

/// Run the chronological fallback. A positional prompt is one-shot. Without
/// one, piped stdin becomes one prompt; a TTY reads one line at each `> `.
pub async fn run_plain(boot: Bootstrap, initial_prompt: Option<String>) -> anyhow::Result<()> {
    let launch = resolve_launch_print(&boot, &timestamp())?;
    let system = compose_instructions(&boot.config)?;
    let mut theme = crate::tui::theme::load_theme(&boot.config);
    let mut app = build_app(boot, launch, system)?;
    crate::tui::theme::apply_model_lab(&mut theme, crate::tui::theme::model_lab(&app.model));
    let mut tracker = RunTracker::default();
    let mut output = std::io::stdout().lock();
    writeln!(
        output,
        "YGG - {}/{}{}",
        safe_text(&app.model.endpoint.id.0),
        theme.fg("model_accent", &safe_text(&app.model.spec.id.0)),
        match crate::app::reasoning_label(&app.reasoning).as_str() {
            "off" => String::new(),
            level => format!(" - {level}"),
        }
    )?;
    writeln!(
        output,
        "Workspace - {}\n",
        safe_text(&app.config.workspace.display().to_string())
    )?;

    if let Some(prompt) = initial_prompt.filter(|prompt| !prompt.trim().is_empty()) {
        return run_prompt(&mut app, prompt, &mut output, &theme, &mut tracker).await;
    }

    if app.config.prompt_template.is_some() && std::io::stdin().is_terminal() {
        return run_prompt(&mut app, String::new(), &mut output, &theme, &mut tracker).await;
    }

    if !std::io::stdin().is_terminal() {
        let mut prompt = String::new();
        std::io::stdin().read_to_string(&mut prompt)?;
        if prompt.trim().is_empty() {
            anyhow::bail!("plain mode needs a positional prompt or text on stdin");
        }
        return run_prompt(&mut app, prompt, &mut output, &theme, &mut tracker).await;
    }

    // A blocking terminal read must not prevent coordinated signal cleanup.
    // Keep the OS read on a dedicated thread and let this async owner select
    // between the next line and the level-triggered shutdown notification.
    let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<std::io::Result<Option<String>>>(1);
    std::thread::Builder::new()
        .name("ygg-plain-input".into())
        .spawn(move || {
            let stdin = std::io::stdin();
            let mut input = stdin.lock();
            loop {
                let mut line = String::new();
                let item = match input.read_line(&mut line) {
                    Ok(0) => Ok(None),
                    Ok(_) => Ok(Some(line)),
                    Err(error) => Err(error),
                };
                let finished = !matches!(&item, Ok(Some(_)));
                if input_tx.blocking_send(item).is_err() || finished {
                    break;
                }
            }
        })?;
    loop {
        write!(output, "{} ", theme.fg("model_accent", ">"))?;
        output.flush()?;
        let next = tokio::select! {
            biased;
            _ = crate::tui::terminal::wait_for_shutdown_signal() => {
                ygg_agent::extension_process::terminate_exec_process_groups(
                    Duration::from_millis(400),
                )
                .await;
                let _ = tokio::time::timeout(
                    Duration::from_millis(1400),
                    app.executable_extensions.shutdown(),
                )
                .await;
                ygg_agent::extension_process::force_kill_registered_process_groups();
                output.flush()?;
                return Ok(());
            }
            next = input_rx.recv() => next,
        };
        let Some(next) = next else {
            break;
        };
        let Some(prompt) = next? else {
            break;
        };
        let prompt = prompt.trim_end_matches(['\r', '\n']).to_owned();
        if prompt.is_empty() {
            continue;
        }
        run_prompt(&mut app, prompt, &mut output, &theme, &mut tracker).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::RunSummary;

    #[test]
    fn plain_outcomes_are_ascii_and_explicit() {
        let completed = outcome_text(&RunOutcome::Completed {
            elapsed: Duration::from_millis(1200),
            summary: RunSummary {
                files_changed: 2,
                tool_calls: 3,
                warnings: 0,
            },
        });
        assert_eq!(completed, "[completed] 1.2s - 2 files changed");
        assert!(completed.is_ascii());

        let failed = outcome_text(&RunOutcome::Failed {
            elapsed: Duration::from_secs(2),
            reason: "command exited 1".into(),
        });
        assert_eq!(failed, "[failed] command exited 1 - 2.0s");
    }

    #[test]
    fn plain_text_neutralizes_terminal_controls() {
        assert_eq!(safe_text("a\x1b[31m\x07"), "a·[31m·");
        assert_eq!(safe_text("a\r\nb\rc"), "a\nb·c");
    }
}
