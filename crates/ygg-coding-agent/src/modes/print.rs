#![allow(missing_docs)]

use std::io::{IsTerminal, Write};

use ygg_agent::{AgentEvent, OutputChannel};

use crate::app::bootstrap::{build_app, resolve_launch_print, Bootstrap};
use crate::modes::{timestamp, RunEnded};
use crate::resources::compose_instructions;

/// Convert an explicit terminal run result to process success or an actionable
/// nonzero error. A started run must always yield `RunFinished`.
pub fn classify_finish(finished: Option<RunEnded>) -> anyhow::Result<()> {
    match finished {
        Some(RunEnded::Completed) => Ok(()),
        Some(RunEnded::MaxTurns) => {
            anyhow::bail!("run hit max turns before completing")
        }
        Some(RunEnded::Aborted) => anyhow::bail!("run aborted before completing"),
        Some(RunEnded::Failed(error)) => anyhow::bail!("run failed: {error}"),
        None => anyhow::bail!("run stream ended without RunFinished (invariant violation)"),
    }
}

fn terminal_outcome(event: &AgentEvent) -> Option<RunEnded> {
    let AgentEvent::RunFinished { reason, .. } = event else {
        return None;
    };
    Some(match reason {
        ygg_agent::FinishReason::Completed => RunEnded::Completed,
        ygg_agent::FinishReason::Aborted => RunEnded::Aborted,
        ygg_agent::FinishReason::MaxTurns => RunEnded::MaxTurns,
        ygg_agent::FinishReason::Failed(error) => RunEnded::Failed(error.to_string()),
    })
}

fn terminal_safe_output(text: &str, terminal: bool) -> std::borrow::Cow<'_, str> {
    if terminal {
        sexy_tui_rs::sanitize_text(text, sexy_tui_rs::SanitizeOptions::default())
    } else {
        std::borrow::Cow::Borrowed(text)
    }
}

/// Stream a persistent Agent session to standard output without constructing a
/// terminal UI.
pub async fn run_print(boot: Bootstrap, prompt: String) -> anyhow::Result<()> {
    let launch = resolve_launch_print(&boot, &timestamp())?;
    let system = compose_instructions(&boot.config)?;
    let mut app = build_app(boot, launch, system)?;
    let prompt = match crate::prompts::render_configured(&mut app, &prompt)? {
        Some(rendered) => {
            if app.config.debug_prompt {
                eprintln!("{}", crate::prompts::debug_expansion(&rendered));
            }
            rendered.text
        }
        None => prompt,
    };

    // The Agent owns cancellable capacity checks and compaction. Check spend
    // before creating any billable subrequest.
    if let Some(limit) = app.config.max_cost_microdollars {
        if app.agent.session().total_cost_microdollars() >= limit {
            anyhow::bail!(
                "Session cost limit of {} reached.",
                crate::commands::format_microdollars_cents(limit)
            );
        }
    }
    let show_reasoning = app.config.show_reasoning_in_print;
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
        eprintln!("extension: {notification}");
    }
    app.agent.set_system_prompt(composition.system);
    app.agent.set_prompt_display_text(Some(prompt));
    let mut run = match app.agent.prompt(composition.prompt).await {
        Ok(run) => run,
        Err(error) => return Err(error.into()),
    };
    app.executable_extensions
        .commit_prompt_context(pending_context_count);
    let control = run.control();
    let stdout_is_terminal = std::io::stdout().is_terminal();
    let mut output = std::io::stdout().lock();
    let mut pending_output = String::new();
    let mut finished = None;
    let mut limit_reached = false;
    let mut last_run_cost = 0u64;
    let mut response_text = String::new();
    let mut shutdown_requested = false;

    loop {
        let event = tokio::select! {
            biased;
            _ = crate::tui::terminal::wait_for_shutdown_signal() => {
                control.abort();
                ygg_agent::extension_process::terminate_exec_process_groups(
                    std::time::Duration::from_millis(400),
                )
                .await;
                shutdown_requested = true;
                break;
            }
            event = run.next() => event,
        };
        let Some(event) = event else {
            break;
        };
        if let Some(outcome) = terminal_outcome(&event) {
            finished = Some(outcome);
        }
        match event {
            AgentEvent::OutputDelta {
                channel: OutputChannel::Text,
                text,
            } => pending_output.push_str(&text),
            AgentEvent::OutputDelta {
                channel: OutputChannel::Reasoning,
                text,
            } if show_reasoning => pending_output.push_str(&text),
            // stdout cannot retract bytes. Keep each provider attempt buffered
            // until `TurnFinished`, then a transient reconnect can discard its
            // provisional output without corrupting print-mode results.
            AgentEvent::ProviderRetry { .. } => pending_output.clear(),
            AgentEvent::TurnFinished {
                message,
                session_cost_microdollars,
                run_cost_microdollars,
                ..
            } => {
                response_text.clear();
                response_text.push_str(&crate::extensions::assistant_text(&message));
                write!(
                    output,
                    "{}",
                    terminal_safe_output(&pending_output, stdout_is_terminal)
                )?;
                output.flush()?;
                pending_output.clear();
                let turn_cost = run_cost_microdollars.saturating_sub(last_run_cost);
                if app
                    .config
                    .cost_warning_microdollars
                    .is_some_and(|threshold| turn_cost >= threshold)
                {
                    eprintln!(
                        "turn cost warning: {} reached the {} threshold",
                        crate::commands::format_microdollars(turn_cost),
                        crate::commands::format_microdollars_cents(
                            app.config.cost_warning_microdollars.unwrap_or_default()
                        )
                    );
                }
                last_run_cost = run_cost_microdollars;
                if let (Some(limit), Some(total)) =
                    (app.config.max_cost_microdollars, session_cost_microdollars)
                {
                    if total >= limit {
                        limit_reached = true;
                        control.abort();
                    }
                }
            }
            AgentEvent::RunFinished { .. } => {}
            _ => {}
        }
    }
    drop(run);
    app.agent.set_system_prompt(app.system.clone());
    if shutdown_requested {
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(1400),
            app.executable_extensions.shutdown(),
        )
        .await;
        ygg_agent::extension_process::force_kill_registered_process_groups();
        output.flush()?;
        return Ok(());
    }
    if matches!(finished, Some(RunEnded::Completed)) && !limit_reached {
        for notification in app
            .executable_extensions
            .after_response(&response_text)
            .await
        {
            eprintln!("extension: {notification}");
        }
    }
    output.flush()?;
    let result = if limit_reached {
        Err(anyhow::anyhow!(
            "Session cost limit of {} reached.",
            crate::commands::format_microdollars_cents(
                app.config.max_cost_microdollars.unwrap_or_default()
            )
        ))
    } else {
        classify_finish(finished)
    };
    // A tool error is model-visible and may be recovered by a later turn; the
    // final run outcome, not an intermediate attempt, determines exit status.
    app.executable_extensions.shutdown().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tty_output_neutralizes_terminal_control_sequences_but_pipes_remain_exact() {
        let hostile = "answer\x1b]52;c;secret\x07";
        assert_eq!(terminal_safe_output(hostile, false), hostile);
        let safe = terminal_safe_output(hostile, true);
        assert!(!safe.contains('\x1b'));
        assert!(!safe.contains('\x07'));
    }

    #[test]
    fn classify_finish_has_explicit_success_and_failures() {
        assert!(classify_finish(Some(RunEnded::Completed)).is_ok());
        assert!(classify_finish(Some(RunEnded::MaxTurns)).is_err());
        assert!(classify_finish(Some(RunEnded::Aborted)).is_err());
        assert!(classify_finish(Some(RunEnded::Failed("nope".into()))).is_err());
        assert!(classify_finish(None).is_err());
    }

    #[test]
    fn intermediate_tool_error_does_not_override_the_final_run_outcome() {
        let events = [
            AgentEvent::ToolFinished {
                id: ygg_ai::ToolCallId("failed-tool".into()),
                result: Err(ygg_agent::ToolError::new("recoverable tool failure")),
            },
            AgentEvent::RunFinished {
                head: ygg_agent::EntryId("004".into()),
                reason: ygg_agent::FinishReason::Completed,
            },
        ];
        let mut finished = None;
        for event in &events {
            if let Some(outcome) = terminal_outcome(event) {
                finished = Some(outcome);
            }
        }

        assert_eq!(finished, Some(RunEnded::Completed));
        assert!(classify_finish(finished).is_ok());
    }
}
