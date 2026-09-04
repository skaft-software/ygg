#![allow(missing_docs)]

use std::io::{IsTerminal, Write};

use ygg_agent::{AgentEvent, OutputChannel};

use crate::app::bootstrap::{build_app, resolve_launch_print, Bootstrap};
use crate::modes::{timestamp, HostRunOutcome};
use crate::resources::{compose_instructions, expand_skill_command};

/// Convert an explicit terminal run result to process success or an actionable
/// nonzero error. A started run must always yield `RunFinished`.
pub fn classify_finish(outcome: HostRunOutcome) -> anyhow::Result<()> {
    match outcome {
        HostRunOutcome::Completed | HostRunOutcome::Shutdown => Ok(()),
        HostRunOutcome::MaxTurns => {
            anyhow::bail!("run hit max turns before completing")
        }
        HostRunOutcome::Aborted => anyhow::bail!("run aborted before completing"),
        HostRunOutcome::Failed(error) => {
            let error = sexy_tui_rs::sanitize_line(&error, true);
            anyhow::bail!("run failed: {error}")
        }
        HostRunOutcome::StreamLost => {
            anyhow::bail!("run stream ended without RunFinished (invariant violation)")
        }
    }
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
                crate::output::stderr_multiline(crate::prompts::debug_expansion(&rendered));
            }
            rendered.text
        }
        None => prompt,
    };
    let display_prompt = prompt.clone();
    let prompt = match expand_skill_command(
        app.skills.as_ref(),
        &prompt,
        &app.agent.registered_tool_names(),
    ) {
        Ok(Some(expanded)) => expanded,
        Ok(None) => prompt,
        Err(error) => {
            eprintln!("warning: failed to expand /skill: command: {error}");
            prompt
        }
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
        crate::output::stderr!("extension: {notification}");
    }
    app.agent.set_system_prompt(composition.system);
    app.agent.set_prompt_display_text(Some(display_prompt));
    let mut run = match app.agent.prompt(composition.prompt).await {
        Ok(run) => run,
        Err(error) => anyhow::bail!(
            "{}",
            ygg_agent::public_error_diagnostic(
                &error,
                &app.model.endpoint.id.0,
                &app.model.spec.id.0,
            )
        ),
    };
    let extension_turn = app.executable_extensions.begin_turn().await;
    app.executable_extensions
        .commit_prompt_context(pending_context_count);
    let control = run.control();
    let stdout_is_terminal = std::io::stdout().is_terminal();
    let mut output = std::io::stdout().lock();
    let mut pending_output = String::new();
    let mut limit_reached = false;
    let mut last_run_cost = 0u64;
    let mut response_text = String::new();
    let outcome = loop {
        let event = tokio::select! {
            biased;
            _ = crate::tui::terminal::wait_for_shutdown_signal() => {
                control.abort();
                ygg_agent::extension_process::terminate_bash_process_groups(
                    std::time::Duration::from_millis(400),
                )
                .await;
                break HostRunOutcome::shutdown();
            }
            event = run.next() => event,
        };
        let Some(event) = event else {
            break HostRunOutcome::stream_lost();
        };
        if let Some(outcome) =
            HostRunOutcome::from_event(&event, &app.model.endpoint.id.0, &app.model.spec.id.0)
        {
            break outcome;
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
            AgentEvent::ProviderLifecycle { lifecycle } => {
                // `--print` stdout is response-only. Surface opt-in endpoint
                // telemetry only as a separate stderr diagnostic.
                crate::output::stderr!(
                    "provider lifecycle: {}",
                    crate::presentation::provider_lifecycle_label(
                        &app.model.endpoint.id.0,
                        &lifecycle
                    )
                );
            }
            // stdout cannot retract bytes. Keep each provider attempt buffered
            // until `TurnFinished`, then a transient reconnect can discard its
            // provisional output without corrupting print-mode results.
            AgentEvent::ProviderRetry { .. } => pending_output.clear(),
            AgentEvent::CandidateRejected {
                run_cost_microdollars,
                ..
            } => {
                pending_output.clear();
                last_run_cost = run_cost_microdollars;
            }
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
                    crate::output::stderr!(
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
            _ => {}
        }
    };
    drop(run);
    app.executable_extensions
        .settle_turn(extension_turn, &outcome)
        .await;
    app.agent.set_system_prompt(app.system.clone());
    if outcome.allows_after_response() && !limit_reached {
        for notification in app
            .executable_extensions
            .after_response(&response_text)
            .await
        {
            crate::output::stderr!("extension: {notification}");
        }
    }
    let presentation = app.executable_extensions.presentation_text();
    if !presentation.is_empty() {
        crate::output::stderr!("extension state:\n{presentation}");
    }
    if outcome.shutdown_requested() {
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(1400),
            app.executable_extensions.shutdown(),
        )
        .await;
        ygg_agent::extension_process::force_kill_registered_process_groups();
        output.flush()?;
        return Ok(());
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
        classify_finish(outcome)
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
        assert!(classify_finish(HostRunOutcome::Completed).is_ok());
        assert!(classify_finish(HostRunOutcome::MaxTurns).is_err());
        assert!(classify_finish(HostRunOutcome::Aborted).is_err());
        assert!(classify_finish(HostRunOutcome::Failed("nope".into())).is_err());
        assert!(classify_finish(HostRunOutcome::StreamLost).is_err());
        assert!(classify_finish(HostRunOutcome::Shutdown).is_ok());
    }

    #[test]
    fn classify_finish_neutralizes_control_sequences_in_errors() {
        let error = classify_finish(HostRunOutcome::Failed(
            "provider\x1b]52;c;YXR0YWNr\x07\nforged".into(),
        ))
        .unwrap_err()
        .to_string();
        assert!(!error.contains('\x1b'), "{error:?}");
        assert!(!error.contains('\x07'), "{error:?}");
        assert!(!error.contains('\n'), "{error:?}");
        assert!(error.contains("^["), "{error:?}");
        assert!(error.contains("<BEL>"), "{error:?}");
    }

    #[test]
    fn intermediate_tool_error_does_not_override_the_final_run_outcome() {
        let events = [
            AgentEvent::ToolFinished {
                id: ygg_ai::ToolCallId("failed-tool".into()),
                result: Err(ygg_agent::ToolError::new("recoverable tool failure")),
                duration: std::time::Duration::from_millis(10),
            },
            AgentEvent::RunFinished {
                head: ygg_agent::EntryId("004".into()),
                reason: ygg_agent::FinishReason::Completed,
            },
        ];
        let mut finished = None;
        for event in &events {
            if let Some(outcome) = HostRunOutcome::from_event(event, "test-provider", "test-model")
            {
                finished = Some(outcome);
            }
        }

        assert_eq!(finished, Some(HostRunOutcome::Completed));
        assert!(classify_finish(finished.expect("terminal outcome")).is_ok());
    }
}
